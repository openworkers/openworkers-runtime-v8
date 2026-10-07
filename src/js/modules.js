// Check if ES Modules style is used: export default { fetch }
if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.fetch === 'function') {
    const moduleHandler = globalThis.default;

    // Override __triggerFetch for ES Modules style
    globalThis.__triggerFetch = function(request) {
        // Collect promises passed to waitUntil
        const waitUntilPromises = [];

        const ctx = {
            waitUntil: (promise) => {
                waitUntilPromises.push(Promise.resolve(promise));
            },
            passThroughOnException: () => {}
        };

        // Run async and track completion separately from response
        (async () => {
            try {
                // ES Modules style: fetch(request, env, ctx) returns Response directly
                const response = await moduleHandler.fetch(request, globalThis.env, ctx);

                // Process response body for streaming
                const processed = await __streamResponseBody(response);
                globalThis.__lastResponse = processed;

                // Wait for all waitUntil promises to complete (after response is set)
                if (waitUntilPromises.length > 0) {
                    await Promise.all(waitUntilPromises);
                }
            } catch (error) {
                console.error('[ES Modules] Error in fetch handler:', error);
                if (!globalThis.__lastResponse) {
                    globalThis.__lastResponse = new Response(
                        'Handler exception: ' + (error.message || error),
                        { status: 500 }
                    );
                }
            } finally {
                globalThis.__requestComplete = true;
            }
        })();
    };
}

// If export default exists but no fetch, and no addEventListener handler, create a handler that returns 501
if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.fetch !== 'function' && typeof globalThis.__triggerFetch !== 'function') {
    globalThis.__triggerFetch = function(request) {
        globalThis.__lastResponse = new Response('Worker does not implement fetch handler', { status: 501 });
        globalThis.__requestComplete = true;
    };
}

// Same for scheduled events
if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.scheduled === 'function') {
    const moduleScheduled = globalThis.default.scheduled;

    // Wrap to pass env and ctx
    globalThis.__scheduledHandler = async function(event) {
        // Collect promises passed to waitUntil
        const waitUntilPromises = [];
        globalThis.__taskResult = { success: true };

        const ctx = {
            waitUntil: (promise) => {
                waitUntilPromises.push(Promise.resolve(promise));
            }
        };

        event.type = 'scheduled';
        // The runner never retries a scheduled event, so there is nothing to turn off.
        event.noRetry = function() {};

        try {
            await moduleScheduled(event, globalThis.env, ctx);

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
}

// If export default exists but no scheduled, and no addEventListener handler, fail the task
if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.scheduled !== 'function' && typeof globalThis.__scheduledHandler !== 'function') {
    globalThis.__scheduledHandler = async function(event) {
        globalThis.__taskResult = {
            success: false,
            error: 'Worker does not implement scheduled handler'
        };
        globalThis.__requestComplete = true;
    };
}

// Same for task events (ES modules style)
if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.task === 'function') {
    const moduleTask = globalThis.default.task;

    // Wrap to pass env and ctx, and handle return value
    globalThis.__taskHandler = async function(event) {
        const waitUntilPromises = [];
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

        const ctx = {
            waitUntil: event.waitUntil
        };

        try {
            const result = await moduleTask(event, globalThis.env, ctx);

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

            // Wait for all waitUntil promises
            if (waitUntilPromises.length > 0) {
                await Promise.all(waitUntilPromises);
            }
        } catch (e) {
            globalThis.__taskResult = {
                success: false,
                error: e.message || String(e)
            };
        } finally {
            globalThis.__requestComplete = true;
        }
    };
}

// If export default exists but no task, and no addEventListener handler, create a fallback
if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.task !== 'function' && typeof globalThis.__taskHandler !== 'function') {
    // Fall back to scheduled handler if it exists (backward compat)
    if (typeof globalThis.__scheduledHandler === 'function') {
        globalThis.__taskHandler = globalThis.__scheduledHandler;
    }
}

// Final fallback: if __triggerFetch is still not defined (no addEventListener, no valid export default),
// create a 501 handler. This handles cases like:
// - globalThis.default = null
// - globalThis.default = 42
// - globalThis.default = "string"
// - No handler defined at all
if (typeof globalThis.__triggerFetch !== 'function') {
    globalThis.__triggerFetch = function(request) {
        globalThis.__lastResponse = new Response('Worker does not implement fetch handler', { status: 501 });
        globalThis.__requestComplete = true;
    };
}
