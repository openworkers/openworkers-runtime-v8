use v8;

/// Setup ReadableStream API (WHATWG Streams spec)
pub fn setup_readable_stream(scope: &mut v8::PinScope) {
    let code = r#"
        // ReadableStream implementation (simplified WHATWG spec)
        globalThis.ReadableStream = class ReadableStream {
            constructor(underlyingSource = {}) {
                this._underlyingSource = underlyingSource;
                this._controller = null;
                this._reader = null;
                this._state = 'readable'; // 'readable', 'closed', 'errored'
                this._storedError = null;

                // Create controller
                const controller = new ReadableStreamDefaultController(this);
                this._controller = controller;

                // Start the stream
                if (underlyingSource.start) {
                    const startPromise = Promise.resolve(underlyingSource.start(controller));
                    startPromise.catch(e => {
                        controller.error(e);
                    });
                }
            }

            getReader() {
                if (this._reader) {
                    throw new TypeError('ReadableStream is locked to a reader');
                }
                const reader = new ReadableStreamDefaultReader(this);
                this._reader = reader;
                return reader;
            }

            cancel(reason) {
                if (this._state === 'closed') {
                    return Promise.resolve();
                }
                if (this._state === 'errored') {
                    return Promise.reject(this._storedError);
                }

                this._state = 'closed';

                // Clear reader
                if (this._reader) {
                    this._reader._closePending();
                    this._reader = null;
                }

                // Call underlying source cancel
                if (this._underlyingSource.cancel) {
                    return Promise.resolve(this._underlyingSource.cancel(reason));
                }

                return Promise.resolve();
            }

            get locked() {
                return this._reader !== null;
            }
        };

        // ReadableStreamDefaultController
        globalThis.ReadableStreamDefaultController = class ReadableStreamDefaultController {
            constructor(stream) {
                this._stream = stream;
                this._queue = [];
                this._closeRequested = false;
            }

            enqueue(chunk) {
                if (this._closeRequested) {
                    throw new TypeError('Cannot enqueue after close');
                }
                if (this._stream._state !== 'readable') {
                    throw new TypeError('Stream is not in readable state');
                }

                this._queue.push({ type: 'chunk', value: chunk });
                this._processQueue();
            }

            close() {
                if (this._closeRequested) {
                    throw new TypeError('Stream is already closing');
                }
                if (this._stream._state !== 'readable') {
                    throw new TypeError('Stream is not in readable state');
                }

                this._closeRequested = true;
                this._queue.push({ type: 'close' });
                this._processQueue();
            }

            error(error) {
                if (this._stream._state !== 'readable') {
                    return;
                }

                this._stream._state = 'errored';
                this._stream._storedError = error;

                // Reject all pending reads
                if (this._stream._reader) {
                    this._stream._reader._errorPending(error);
                }

                this._queue = [];
            }

            _processQueue() {
                if (this._stream._reader) {
                    this._stream._reader._processQueue();
                }
            }

            get desiredSize() {
                if (this._stream._state === 'errored') {
                    return null;
                }
                if (this._stream._state === 'closed') {
                    return 0;
                }
                // Simple high water mark
                return Math.max(0, 1 - this._queue.length);
            }
        };

        // ReadableStreamDefaultReader
        globalThis.ReadableStreamDefaultReader = class ReadableStreamDefaultReader {
            constructor(stream) {
                if (stream._reader) {
                    throw new TypeError('Stream is already locked');
                }

                this._stream = stream;
                this._readRequests = [];
                this._closedPromise = null;
                this._closedPromiseResolve = null;
                this._closedPromiseReject = null;

                // Create closed promise
                this._closedPromise = new Promise((resolve, reject) => {
                    this._closedPromiseResolve = resolve;
                    this._closedPromiseReject = reject;
                });
            }

            read() {
                if (!this._stream) {
                    return Promise.reject(new TypeError('Reader is released'));
                }

                if (this._stream._state === 'errored') {
                    return Promise.reject(this._stream._storedError);
                }

                const controller = this._stream._controller;

                // Check if we have data in queue
                if (controller._queue.length > 0) {
                    const item = controller._queue.shift();

                    if (item.type === 'close') {
                        this._stream._state = 'closed';
                        this._closePending();
                        return Promise.resolve({ done: true, value: undefined });
                    }

                    return Promise.resolve({ done: false, value: item.value });
                }

                // Stream is closed
                if (this._stream._state === 'closed') {
                    return Promise.resolve({ done: true, value: undefined });
                }

                // If underlying source has pull(), call it to get more data
                const underlyingSource = this._stream._underlyingSource;
                if (underlyingSource && underlyingSource.pull) {
                    // Create pending read request first
                    return new Promise((resolve, reject) => {
                        this._readRequests.push({ resolve, reject });

                        // Call pull to request more data
                        // pull() should enqueue data or close/error the stream
                        const pullPromise = underlyingSource.pull(controller);
                        if (pullPromise && typeof pullPromise.then === 'function') {
                            pullPromise.catch(e => {
                                controller.error(e);
                            });
                        }
                    });
                }

                // No pull(), create pending read request
                return new Promise((resolve, reject) => {
                    this._readRequests.push({ resolve, reject });
                });
            }

            _processQueue() {
                const controller = this._stream._controller;

                while (this._readRequests.length > 0 && controller._queue.length > 0) {
                    const request = this._readRequests.shift();
                    const item = controller._queue.shift();

                    if (item.type === 'close') {
                        this._stream._state = 'closed';
                        request.resolve({ done: true, value: undefined });
                        this._closePending();
                        break;
                    } else {
                        request.resolve({ done: false, value: item.value });
                    }
                }

                // If stream is closed and no more data, resolve pending reads
                if (this._stream._state === 'closed' && this._readRequests.length > 0) {
                    while (this._readRequests.length > 0) {
                        const request = this._readRequests.shift();
                        request.resolve({ done: true, value: undefined });
                    }
                }
            }

            _closePending() {
                if (this._closedPromiseResolve) {
                    this._closedPromiseResolve();
                    this._closedPromiseResolve = null;
                }
            }

            _errorPending(error) {
                // Reject all pending reads
                while (this._readRequests.length > 0) {
                    const request = this._readRequests.shift();
                    request.reject(error);
                }

                if (this._closedPromiseReject) {
                    this._closedPromiseReject(error);
                    this._closedPromiseReject = null;
                }
            }

            releaseLock() {
                if (!this._stream) {
                    return;
                }

                if (this._readRequests.length > 0) {
                    throw new TypeError('Cannot release lock while read requests are pending');
                }

                this._stream._reader = null;
                this._stream = null;
            }

            cancel(reason) {
                if (!this._stream) {
                    return Promise.reject(new TypeError('Reader is released'));
                }

                const cancelPromise = this._stream.cancel(reason);
                this.releaseLock();
                return cancelPromise;
            }

            get closed() {
                return this._closedPromise;
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
