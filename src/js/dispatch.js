// The dispatch between the host and the guest's handlers.
//
// The script evaluates to { fetch, task }, which the host keeps; nothing but
// addEventListener goes on globalThis. Each call answers a handle for one
// event, whose promises tell the host how far the event got, and they
// always fulfil:
//
//   answer    the Response for a fetch, the task result for a task
//   done      the answer and every waitUntil promise are settled
//   streamed  the response body is out (at once for a buffered body)
//
// and disconnect() tells a streaming body that the client hung up.
//
// The handler is looked up when the event arrives, so a handler the script
// declares through `export default` wins over one it registers through
// addEventListener, whatever the order. The globals below are read before
// the guest script runs, so a guest that replaces them changes nothing here.

(() => {
    'use strict';

    const Response = globalThis.Response;
    const TextEncoder = globalThis.TextEncoder;
    const setTimeout = globalThis.setTimeout;

    const listeners = Object.create(null);

    globalThis.addEventListener = function(type, handler) {
        listeners[type] = handler;
    };

    const errorMessage = (error) => (error && error.message) || String(error);

    // The handler `export default` declares under `name`, called as a method
    // of the module object.
    function moduleHandler(name) {
        const module = globalThis.default;

        if (module === null || typeof module !== 'object' || typeof module[name] !== 'function') {
            return null;
        }

        return (...args) => module[name](...args);
    }

    // The promises an event passes to waitUntil, awaited after its answer.
    function lifetime() {
        const pending = [];

        return {
            waitUntil(promise) {
                pending.push(Promise.resolve(promise));
            },
            settled() {
                return Promise.all(pending);
            },
        };
    }

    function toBytes(value, encoder) {
        if (typeof value === 'string') {
            return encoder.encode(value);
        }

        if (value instanceof Uint8Array) {
            return value;
        }

        if (ArrayBuffer.isView(value)) {
            return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        }

        return encoder.encode(String(value));
    }

    // Sends a response body to the host and calls `ended` once it is out. A
    // buffered body is read by the host, and a native body (a fetch passed on)
    // is streamed by it, so for these `ended` runs at once.
    function streamBody(response, ended) {
        const body = response.body;

        if (!body) {
            ended();
            return;
        }

        if (body._nativeStreamId !== undefined) {
            response._responseStreamId = body._nativeStreamId;
            ended();
            return;
        }

        if (response._isBuffered) {
            ended();
            return;
        }

        const streamId = __responseStreamCreate();
        response._responseStreamId = streamId;

        // enqueue() reads it to see a client that hung up
        if (body._controller) {
            body._controller._responseStreamId = streamId;
        }

        const encoder = new TextEncoder();

        (async () => {
            let reader = null;
            let cancelled = false;
            let failed = false;

            try {
                reader = body.getReader();

                for (;;) {
                    const { value, done } = await reader.read();

                    if (done) {
                        break;
                    }

                    if (!value || value.length === 0) {
                        continue;
                    }

                    const chunk = toBytes(value, encoder);

                    // A full channel takes nothing: wait and try again
                    while (!__responseStreamWrite(streamId, chunk)) {
                        if (__responseStreamIsClosed(streamId)) {
                            cancelled = true;
                            break;
                        }

                        await new Promise((resolve) => setTimeout(resolve, 1));
                    }

                    if (cancelled) {
                        break;
                    }
                }
            } catch (error) {
                // Ending the stream here would hand the host a truncated body
                // that looks complete, so the failure goes on the channel.
                failed = true;
                __responseStreamError(streamId, errorMessage(error));
            } finally {
                if (reader && cancelled) {
                    // Runs the source's cancel() callback
                    try {
                        await reader.cancel('Client disconnected');
                    } catch {}
                }

                if (!failed) {
                    __responseStreamEnd(streamId);
                }

                ended();
            }
        })();
    }

    // Tells a streaming body that the client hung up. Aborting tells a guest
    // that watches the signal; cancelling ends the read the body pump waits
    // on, which is what releases the worker.
    function disconnect(response) {
        const body = response?.body;

        if (!body) {
            return;
        }

        const controller = body._controller;

        if (controller?._abortController && !controller.signal.aborted) {
            controller._abortController.abort('Client disconnected');
        }

        if (typeof body.cancel === 'function') {
            body.cancel('Client disconnected');
        }
    }

    // The response a fetch listener gives: the one it passes to respondWith,
    // or else a Response it returns, directly or through a promise.
    // respondWith may run at any time, from a timer or a callback included,
    // so a listener that returns without either still has time to answer. A
    // second respondWith throws and leaves the first response.
    function listenerResponse(listener, request, life) {
        let answered = false;
        let answer;
        const response = new Promise((resolve) => {
            answer = (value) => {
                answered = true;
                resolve(value);
            };
        });

        const event = {
            request,
            waitUntil: life.waitUntil,
            respondWith(value) {
                if (answered) {
                    throw new TypeError('respondWith was already called');
                }

                answer(value);
            },
        };

        let returned;

        try {
            returned = listener(event);
        } catch (error) {
            returned = Promise.reject(error);
        }

        Promise.resolve(returned).then(
            (value) => {
                if (!answered && value instanceof Response) {
                    answer(value);
                }
            },
            (error) => {
                if (answered) {
                    console.error('[fetch] Handler error after respondWith:', error);
                } else {
                    answer(Promise.reject(error));
                }
            }
        );

        return response;
    }

    async function handlerResponse(request, life) {
        const module = moduleHandler('fetch');

        if (module) {
            const ctx = { waitUntil: life.waitUntil, passThroughOnException() {} };

            return module(request, globalThis.env, ctx);
        }

        if (listeners.fetch) {
            return listenerResponse(listeners.fetch, request, life);
        }

        return new Response('Worker does not implement fetch handler', { status: 501 });
    }

    function fetch(request) {
        const life = lifetime();
        let ended;
        const streamed = new Promise((resolve) => {
            ended = resolve;
        });

        const answer = (async () => {
            let response;

            try {
                response = await handlerResponse(request, life);

                if (!(response instanceof Response)) {
                    throw new TypeError(
                        response === undefined
                            ? 'the fetch handler did not respond'
                            : 'the fetch handler did not answer with a Response'
                    );
                }
            } catch (error) {
                console.error('[fetch] Handler error:', error);
                response = new Response('Handler exception: ' + errorMessage(error), { status: 500 });
            }

            streamBody(response, ended);

            return response;
        })();

        const done = answer.then(async () => {
            try {
                await life.settled();
            } catch (error) {
                // The response is out; a background failure cannot change it.
                console.error('[fetch] waitUntil rejected:', error);
            }
        });

        let response = null;
        answer.then((value) => {
            response = value;
        });

        return { answer, done, streamed, disconnect: () => disconnect(response) };
    }

    // A task result from what a task handler answers: an object with a
    // `success` field is the result, anything else is its data.
    function taskEnvelope(value) {
        if (value !== null && typeof value === 'object' && 'success' in value) {
            return { success: value.success !== false, data: value.data, error: value.error };
        }

        return { success: true, data: value };
    }

    // Runs the handler for a task and answers its result. A `task` handler
    // gets every task; without one, a `scheduled` handler gets them as cron
    // events and its return value is not a result.
    async function runTask(event, life) {
        const moduleTask = moduleHandler('task');
        const task = moduleTask ?? listeners.task;

        if (task) {
            let responded = null;

            event.waitUntil = life.waitUntil;
            event.respondWith = (value) => {
                responded = taskEnvelope(value);
            };

            const returned = moduleTask
                ? await moduleTask(event, globalThis.env, { waitUntil: life.waitUntil })
                : await task(event);

            return responded ?? taskEnvelope(returned);
        }

        const moduleScheduled = moduleHandler('scheduled');
        const scheduled = moduleScheduled ?? listeners.scheduled;

        if (scheduled) {
            event.type = 'scheduled';
            // The runner never retries a scheduled event, so there is nothing to turn off.
            event.noRetry = function() {};

            if (moduleScheduled) {
                await moduleScheduled(event, globalThis.env, { waitUntil: life.waitUntil });
            } else {
                event.waitUntil = life.waitUntil;
                await scheduled(event);
            }

            return { success: true };
        }

        throw new Error(
            event.scheduledTime === undefined
                ? 'Worker does not implement task handler'
                : 'Worker does not implement scheduled handler'
        );
    }

    function task(event) {
        const life = lifetime();

        const done = (async () => {
            try {
                const result = await runTask(event, life);
                await life.settled();

                return result;
            } catch (error) {
                console.error('[task] Handler error:', error);

                return { success: false, error: errorMessage(error) };
            }
        })();

        return { answer: done, done, streamed: Promise.resolve(), disconnect() {} };
    }

    return { fetch, task };
})()
