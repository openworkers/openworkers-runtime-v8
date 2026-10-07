// Track active response streams - worker stays alive until all streams are closed
globalThis.__activeResponseStreams = 0;

// Signal client disconnect to abort response streams
// Called from Rust when consumer disconnects
globalThis.__signalClientDisconnect = function() {
    const body = globalThis.__lastResponse?.body;

    if (!body) {
        return;
    }

    const ctrl = body._controller;

    if (ctrl?._abortController && !ctrl.signal.aborted) {
        ctrl._abortController.abort('Client disconnected');
    }

    // Aborting only tells a guest that watches the signal. Cancelling
    // ends the read the response pump is waiting on, which is what
    // releases the worker for the next request.
    if (typeof body.cancel === 'function') {
        body.cancel('Client disconnected');
    }
};

// Stream response body to Rust (only for true streaming responses)
async function __streamResponseBody(response) {
    if (!response.body) {
        return response;
    }

    // If it's a native stream (fetch forward), just mark it
    // Native streams are managed by Rust, no need to track here
    if (response.body._nativeStreamId !== undefined) {
        response._responseStreamId = response.body._nativeStreamId;
        globalThis.__lastResponseStreamId = response.body._nativeStreamId;
        return response;
    }

    // Check if this is a buffered response (created from string/Uint8Array/ArrayBuffer)
    // These have _isBuffered = true, set by the Response constructor
    // For these, skip streaming and let Rust use _getRawBody() instead
    if (response._isBuffered) {
        // Buffered response - data already available, no need to stream
        return response;
    }

    // True streaming response - create output stream and pipe
    const streamId = __responseStreamCreate();
    response._responseStreamId = streamId;

    // Store the stream ID globally so exec() can detect cancellation
    globalThis.__lastResponseStreamId = streamId;

    // Connect the controller to the stream ID so enqueue() can detect disconnect
    // The controller is on the ReadableStream, which is response.body
    if (response.body._controller) {
        response.body._controller._responseStreamId = streamId;
    }

    // Increment active stream counter BEFORE starting async read
    globalThis.__activeResponseStreams++;

    // Helper to convert value to Uint8Array (handles strings and typed arrays)
    const encoder = new TextEncoder();
    function toUint8Array(value) {
        if (typeof value === 'string') {
            return encoder.encode(value);
        }

        if (value instanceof Uint8Array) {
            return value;
        }

        if (ArrayBuffer.isView(value)) {
            return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        }

        // Fallback: convert to string then encode
        return encoder.encode(String(value));
    }

    // Read and forward asynchronously
    (async () => {
        let reader = null;
        let cancelled = false;
        let failed = false;

        try {
            reader = response.body.getReader();

            while (true) {
                const { value, done } = await reader.read();

                if (done) break;

                if (value && value.length > 0) {
                    const chunk = toUint8Array(value);

                    // Try to write, with backpressure handling
                    while (!__responseStreamWrite(streamId, chunk)) {
                        // Check if stream was closed (client disconnected)
                        if (__responseStreamIsClosed(streamId)) {
                            console.log('[streamResponseBody] Client disconnected, cancelling stream');
                            cancelled = true;
                            break;
                        }

                        // Buffer full, wait and retry
                        await new Promise(resolve => setTimeout(resolve, 1));
                    }

                    if (cancelled) break;
                }
            }
        } catch (error) {
            // The body stopped short. Ending the stream here would hand
            // the host a truncated response that looks complete, so the
            // failure goes on the channel instead.
            failed = true;
            __responseStreamError(streamId, String(error && error.message ? error.message : error));
        } finally {
            // Cancel the reader to trigger the source's cancel() callback
            if (reader && cancelled) {
                try {
                    await reader.cancel('Client disconnected');
                } catch (e) {
                    // Ignore cancel errors
                }
            }

            if (!failed) {
                __responseStreamEnd(streamId);
            }

            // Decrement counter when stream is fully consumed
            globalThis.__activeResponseStreams--;
        }
    })();

    return response;
}

// One dispatch for every event. The handler is looked up when the event
// arrives, so a handler the script declares through `export default` wins
// over one it registers through addEventListener, whatever the order.
const __listeners = Object.create(null);

globalThis.addEventListener = function(type, handler) {
    __listeners[type] = handler;
};

// The handler `export default` declares under `name`, called as a method of
// the module object.
function __moduleHandler(name) {
    const module = globalThis.default;

    if (module === null || typeof module !== 'object' || typeof module[name] !== 'function') {
        return null;
    }

    return (...args) => module[name](...args);
}

function __errorMessage(error) {
    return (error && error.message) || String(error);
}

// The promises an event passes to waitUntil, awaited after its answer.
function __lifetime() {
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

// The response a fetch listener gives: the one it passes to respondWith, or
// else a Response it returns, directly or through a promise. respondWith
// may run at any time, from a timer or a callback included, so a listener
// that returns without either still has time to answer. A second
// respondWith throws and leaves the first response.
function __listenerResponse(listener, request, life) {
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

globalThis.__triggerFetch = function(request) {
    const life = __lifetime();
    const module = __moduleHandler('fetch');
    const listener = __listeners.fetch;

    (async () => {
        try {
            let response;

            if (module) {
                const ctx = { waitUntil: life.waitUntil, passThroughOnException() {} };
                response = await module(request, globalThis.env, ctx);
            } else if (listener) {
                response = await __listenerResponse(listener, request, life);
            } else {
                response = new Response('Worker does not implement fetch handler', { status: 501 });
            }

            if (!(response instanceof Response)) {
                throw new TypeError(
                    response === undefined
                        ? 'the fetch handler did not respond'
                        : 'the fetch handler did not answer with a Response'
                );
            }

            globalThis.__lastResponse = await __streamResponseBody(response);
        } catch (error) {
            console.error('[fetch] Handler error:', error);
            globalThis.__lastResponse = new Response('Handler exception: ' + __errorMessage(error), { status: 500 });
        }

        try {
            await life.settled();
        } catch (error) {
            // The response is already out; a background failure cannot change it.
            console.error('[fetch] waitUntil rejected:', error);
        } finally {
            globalThis.__requestComplete = true;
        }
    })();
};

// A task result from what a task handler answers: an object with a
// `success` field is the result, anything else is its data.
function __taskEnvelope(value) {
    if (value !== null && typeof value === 'object' && 'success' in value) {
        return { success: value.success !== false, data: value.data, error: value.error };
    }

    return { success: true, data: value };
}

// Runs the handler for a task and answers its result. A `task` handler gets
// every task; without one, a `scheduled` handler gets them as cron events and
// its return value is not a result.
async function __runTask(event, life) {
    const moduleTask = __moduleHandler('task');
    const task = moduleTask ?? __listeners.task;

    if (task) {
        let responded = null;

        event.waitUntil = life.waitUntil;
        event.respondWith = (value) => {
            responded = __taskEnvelope(value);
        };

        const returned = moduleTask
            ? await moduleTask(event, globalThis.env, { waitUntil: life.waitUntil })
            : await task(event);

        return responded ?? __taskEnvelope(returned);
    }

    const moduleScheduled = __moduleHandler('scheduled');
    const scheduled = moduleScheduled ?? __listeners.scheduled;

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

globalThis.__taskHandler = async function(event) {
    const life = __lifetime();

    try {
        globalThis.__taskResult = await __runTask(event, life);
        await life.settled();
    } catch (error) {
        console.error('[task] Handler error:', error);
        globalThis.__taskResult = { success: false, error: __errorMessage(error) };
    } finally {
        globalThis.__requestComplete = true;
    }
};
