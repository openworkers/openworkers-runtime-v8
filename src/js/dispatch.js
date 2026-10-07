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

globalThis.addEventListener = function(type, handler) {
    if (type === 'fetch') {
        globalThis.__fetchHandler = handler;
        globalThis.__triggerFetch = function(request) {
            // Collect promises passed to waitUntil
            const waitUntilPromises = [];
            let responsePromise = null;

            const event = {
                request: request,
                waitUntil: function(promise) {
                    waitUntilPromises.push(Promise.resolve(promise));
                },
                respondWith: function(responseOrPromise) {
                    // Handle both direct Response and Promise<Response>
                    if (responseOrPromise && typeof responseOrPromise.then === 'function') {
                        responsePromise = responseOrPromise
                            .then(response => __streamResponseBody(response))
                            .then(response => {
                                globalThis.__lastResponse = response;
                            })
                            .catch(error => {
                                console.error('[respondWith] Promise rejected:', error);
                                globalThis.__lastResponse = new Response(
                                    'Promise rejected: ' + (error.message || error),
                                    { status: 500 }
                                );
                            });
                    } else {
                        responsePromise = __streamResponseBody(responseOrPromise)
                            .then(response => {
                                globalThis.__lastResponse = response;
                            });
                    }
                }
            };

            // Run async to track completion
            (async () => {
                try {
                    // Call handler and capture return value
                    const result = handler(event);

                    // If handler returns a Response or Promise<Response>, use it
                    // ONLY if respondWith() was not already called (respondWith has priority)
                    // (Service Worker / Cloudflare Workers compatibility)
                    if (!responsePromise && result instanceof Response) {
                        responsePromise = __streamResponseBody(result)
                            .then(response => {
                                globalThis.__lastResponse = response;
                            });
                    } else if (!responsePromise && result && typeof result.then === 'function') {
                        // Handler returned a Promise - could be Promise<Response>
                        responsePromise = result
                            .then(response => {
                                if (response instanceof Response) {
                                    return __streamResponseBody(response)
                                        .then(processed => {
                                            globalThis.__lastResponse = processed;
                                        });
                                }
                            })
                            .catch(error => {
                                console.error('[addEventListener] Handler promise rejected:', error);
                                globalThis.__lastResponse = new Response(
                                    'Handler promise rejected: ' + (error.message || error),
                                    { status: 500 }
                                );
                            });
                    }

                    // Wait for response to be set first
                    if (responsePromise) {
                        await responsePromise;
                    }

                    // Then wait for all waitUntil promises to complete
                    if (waitUntilPromises.length > 0) {
                        await Promise.all(waitUntilPromises);
                    }
                } catch (error) {
                    console.error('[addEventListener] Error in fetch handler:', error);
                    // Only set a 500 response if no response was already produced.
                    // A rejected waitUntil promise must NOT overwrite a valid response.
                    if (!globalThis.__lastResponse) {
                        globalThis.__lastResponse = new Response('Handler exception: ' + (error.message || error), { status: 500 });
                    }
                } finally {
                    globalThis.__requestComplete = true;
                }
            })();
        };
    } else if (type === 'scheduled') {
        globalThis.__scheduledHandler = async function(event) {
            // Collect promises passed to waitUntil
            const waitUntilPromises = [];
            globalThis.__taskResult = { success: true };

            event.type = 'scheduled';
            // The runner never retries a scheduled event, so there is nothing to turn off.
            event.noRetry = function() {};
            event.waitUntil = function(promise) {
                waitUntilPromises.push(Promise.resolve(promise));
            };

            try {
                await handler(event);

                // Wait for all waitUntil promises to complete
                if (waitUntilPromises.length > 0) {
                    await Promise.all(waitUntilPromises);
                }
            } catch (error) {
                console.error('[scheduled] Handler error:', error);
                globalThis.__taskResult = {
                    success: false,
                    error: error.message || String(error)
                };
            } finally {
                globalThis.__requestComplete = true;
            }
        };
    } else if (type === 'task') {
        globalThis.__taskHandler = async function(event) {
            // Collect promises passed to waitUntil
            const waitUntilPromises = [];

            // Default result (success with no data)
            globalThis.__taskResult = { success: true };

            event.waitUntil = function(promise) {
                waitUntilPromises.push(Promise.resolve(promise));
            };

            event.respondWith = function(result) {
                if (result && typeof result === 'object') {
                    globalThis.__taskResult = {
                        success: result.success !== false,
                        data: result.data,
                        error: result.error
                    };
                } else {
                    globalThis.__taskResult = { success: true, data: result };
                }
            };

            try {
                const result = await handler(event);

                // If handler returns a value and respondWith wasn't called, use it
                if (result !== undefined && globalThis.__taskResult.data === undefined) {
                    if (result && typeof result === 'object' && 'success' in result) {
                        globalThis.__taskResult = {
                            success: result.success !== false,
                            data: result.data,
                            error: result.error
                        };
                    } else {
                        globalThis.__taskResult = { success: true, data: result };
                    }
                }

                // Wait for all waitUntil promises to complete
                if (waitUntilPromises.length > 0) {
                    await Promise.all(waitUntilPromises);
                }
            } catch (error) {
                console.error('[task] Handler error:', error);
                globalThis.__taskResult = {
                    success: false,
                    error: error.message || String(error)
                };
            } finally {
                globalThis.__requestComplete = true;
            }
        };
    }
};
