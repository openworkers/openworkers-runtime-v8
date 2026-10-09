// How a response body reaches the host on V8: the engine part of the shared
// dispatch (openworkers-wintertc's DISPATCH). The script evaluates to a
// function of the dispatch options; install_dispatch passes what it answers
// to the dispatch. TextEncoder and setTimeout are read before the guest
// script runs, so a guest that replaces them changes nothing here.

(function streamEngine(options) {
    'use strict';

    const TextEncoder = globalThis.TextEncoder;
    const setTimeout = globalThis.setTimeout;

    const errorMessage = (error) => (error && error.message) || String(error);

    // Strict: a chunk is a Uint8Array, as the Fetch spec has it. Lax, the
    // default: also a string or another view, which the OpenWorkers docs
    // taught.
    const strict = options?.strict === true;

    function toBytes(value, encoder) {
        if (value instanceof Uint8Array) {
            return value;
        }

        if (strict) {
            throw new TypeError('a response body chunk must be a Uint8Array');
        }

        if (typeof value === 'string') {
            return encoder.encode(value);
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

    return { streamBody, disconnect };
})
