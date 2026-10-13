// A Durable Object: the instance of a class of the worker, and the dispatch
// its events go through.
//
// The script evaluates to a function that the host calls once, after the
// worker script, with the `{ fetch, task }` object of the wintertc dispatch,
// the name the worker exports the class under, and the id of the object.
// openworkers-transform puts the named exports in globalThis.__exports. The
// call runs the constructor and answers what the host keeps in place of the
// dispatch:
//
//   fetch(request)  the handle of the wintertc dispatch for the request
//   blocked()       whether a blockConcurrencyWhile callback runs: the host
//                   delivers no event until it ends
//   failure()       the error of a blockConcurrencyWhile callback or of a
//                   storage write that failed, after which the object must
//                   be dropped
//   writing()       whether a storage write is under way: the host holds
//                   the responses back until it is committed (output gate)
//   alarm(retries)  runs the alarm() handler, and answers an object whose
//                   `done` turns true when it ends, with `error` if it threw
//   socket(id, kind, data, reason)
//                   delivers a message, close or error of a socket of the
//                   object, and answers an object whose `done` turns true
//                   when its handler ends
//   pinned()        whether a socket that is not hibernatable is open: its
//                   listeners live in this instance, so it must stay
//
// The runner holds the WebSockets of the object. A socket accepted with
// state.acceptWebSocket() is hibernatable: its events go to the handlers of
// the class, and a later instance finds it again. A socket accepted with
// accept() sends its events to its listeners and keeps this instance.

(function installObject(dispatch, className, id) {
    'use strict';

    const Class = globalThis.__exports?.[className];

    if (typeof Class !== 'function') {
        throw new TypeError(`the worker exports no class '${className}'`);
    }

    let blocked = 0;
    let failure = null;
    let writes = 0;

    const message = (error) => (error && error.message) || String(error);

    // The host runs the storage ops of the object one at a time, in the
    // order they are made. A value that cannot be cloned throws here.
    const call = (op) => {
        let settle;
        const done = new Promise((resolve, reject) => {
            settle = { resolve, reject };
        });

        __nativeObjectStorage(op, settle.resolve, settle.reject);

        return done;
    };

    // A write that cannot start rejects without harm to the object; one that
    // fails in the host drops the object, as its memory no longer matches.
    const tracked = (op) => {
        let done;

        try {
            done = call(op);
        } catch (error) {
            return Promise.reject(error);
        }

        writes += 1;
        done.then(
            () => {
                writes -= 1;
            },
            (error) => {
                writes -= 1;
                failure ??= `a storage write failed: ${message(error)}`;
            }
        );

        return done;
    };

    const write = (op) => tracked({ kind: 'write', puts: [], deletes: [], deleteAll: false, ...op });

    const isKeys = (value) => Array.isArray(value);

    const keyOf = (key) => {
        if (typeof key !== 'string') {
            throw new TypeError('Durable Object storage: a key is a string');
        }

        return key;
    };

    const storage = Object.freeze({
        async get(keys) {
            if (!isKeys(keys)) {
                const [value] = await call({ kind: 'get', keys: [keyOf(keys)] });

                return value;
            }

            const values = await call({ kind: 'get', keys: keys.map(keyOf) });
            const found = new Map();

            keys.forEach((key, index) => {
                if (values[index] !== undefined) {
                    found.set(key, values[index]);
                }
            });

            return found;
        },
        put(keyOrEntries, value) {
            const puts = typeof keyOrEntries === 'string'
                ? [[keyOrEntries, value]]
                : Object.entries(keyOrEntries ?? {});

            return write({ puts }).then(() => undefined);
        },
        delete(keys) {
            if (!isKeys(keys)) {
                return write({ deletes: [keyOf(keys)] }).then((deleted) => deleted > 0);
            }

            return write({ deletes: keys.map(keyOf) });
        },
        async list(options = {}) {
            const entries = await call({
                kind: 'list',
                start: options.start,
                startAfter: options.startAfter,
                end: options.end,
                prefix: options.prefix,
                reverse: options.reverse === true,
                limit: options.limit,
            });

            return new Map(entries);
        },
        deleteAll() {
            return write({ deleteAll: true }).then(() => undefined);
        },
        // Its writes are kept here and go out in one atomic write when the
        // closure ends; rollback() or a throw drops them
        async transaction(closure) {
            const puts = new Map();
            const deletes = new Set();
            let rolledBack = false;

            const txn = Object.freeze({
                async get(keys) {
                    const local = (key) => (puts.has(key) ? structuredClone(puts.get(key)) : undefined);

                    if (!isKeys(keys)) {
                        if (puts.has(keys) || deletes.has(keys)) {
                            return local(keys);
                        }

                        return storage.get(keys);
                    }

                    const stored = await storage.get(keys.filter((key) => !puts.has(key) && !deletes.has(key)));
                    const found = new Map();

                    for (const key of keys) {
                        const value = puts.has(key) ? local(key) : stored.get(key);

                        if (value !== undefined) {
                            found.set(key, value);
                        }
                    }

                    return found;
                },
                async put(keyOrEntries, value) {
                    const entries = typeof keyOrEntries === 'string'
                        ? [[keyOrEntries, value]]
                        : Object.entries(keyOrEntries ?? {});

                    for (const [key, entry] of entries) {
                        puts.set(keyOf(key), structuredClone(entry));
                        deletes.delete(key);
                    }
                },
                async delete(keys) {
                    const list = isKeys(keys) ? keys.map(keyOf) : [keyOf(keys)];
                    const stored = await txn.get(list);

                    for (const key of list) {
                        puts.delete(key);
                        deletes.add(key);
                    }

                    return isKeys(keys) ? stored.size : stored.size > 0;
                },
                list() {
                    return Promise.reject(new Error('Durable Object storage: list() is not supported in a transaction'));
                },
                getAlarm() {
                    return storage.getAlarm();
                },
                setAlarm() {
                    return Promise.reject(new Error('Durable Object storage: setAlarm() is not supported in a transaction'));
                },
                deleteAlarm() {
                    return Promise.reject(new Error('Durable Object storage: deleteAlarm() is not supported in a transaction'));
                },
                rollback() {
                    rolledBack = true;
                },
            });

            const result = await closure(txn);

            if (!rolledBack && (puts.size > 0 || deletes.size > 0)) {
                await write({ puts: [...puts], deletes: [...deletes] });
            }

            return result;
        },
        // Resolves once every write before it is committed
        sync() {
            return call({ kind: 'get', keys: [] }).then(() => undefined);
        },
        // Milliseconds since the Unix epoch, or null
        getAlarm() {
            return call({ kind: 'getAlarm' });
        },
        setAlarm(scheduledTime) {
            if (typeof Class.prototype.alarm !== 'function') {
                return Promise.reject(new TypeError('the Durable Object class has no alarm() handler'));
            }

            const time = scheduledTime instanceof Date ? scheduledTime.getTime() : scheduledTime;

            if (typeof time !== 'number' || !Number.isFinite(time)) {
                return Promise.reject(new TypeError('setAlarm() takes a Date or a time in milliseconds'));
            }

            return tracked({ kind: 'setAlarm', at: Math.trunc(time) }).then(() => undefined);
        },
        deleteAlarm() {
            return tracked({ kind: 'setAlarm', at: null }).then(() => undefined);
        },
        get sql() {
            throw new Error('Durable Object SQL storage is not supported');
        },
    });

    const OPEN = 1;
    const CLOSED = 3;
    const MAX_TAGS = 10;
    const MAX_TAG_LENGTH = 256;

    // The sockets of the object, by id; a socket leaves when it closes
    const sockets = new Map();

    const closeChecks = (code, reason) => {
        if (code !== undefined && code !== 1000 && (code < 3000 || code > 4999)) {
            throw WebSocket.__err('InvalidAccessError', "Failed to execute 'close' on 'WebSocket': The code must be 1000 or in the range 3000-4999.");
        }

        if (reason !== undefined && new TextEncoder().encode(String(reason)).length > 123) {
            throw WebSocket.__err('SyntaxError', "Failed to execute 'close' on 'WebSocket': The close reason must not exceed 123 bytes.");
        }
    };

    // A WebSocket whose frames go through the runner. `mode` is null until
    // it is accepted, then 'handlers' (hibernatable) or 'listeners'.
    const socketMethods = Object.create(WebSocket.prototype, {
        accept: {
            value() {
                if (this._mode !== null) {
                    throw new TypeError('WebSocket: the socket is accepted already');
                }

                this._mode = 'listeners';
                sockets.set(this._wsId, this);
                __nativeObjectSocketAccept(this._wsId, [], false);
            },
        },
        send: {
            value(data) {
                if (this._mode === null) {
                    throw WebSocket.__err('InvalidStateError', 'WebSocket: accept the socket before sending on it');
                }

                if (this.readyState !== OPEN) {
                    return;
                }

                __nativeObjectSocketSend(this._wsId, data);
            },
        },
        close: {
            value(code, reason) {
                closeChecks(code, reason);

                if (this.readyState !== OPEN) {
                    return;
                }

                this.readyState = CLOSED;
                sockets.delete(this._wsId);
                __nativeObjectSocketClose(this._wsId, code ?? 1000, reason ?? '');
            },
        },
        serializeAttachment: {
            value(value) {
                __nativeObjectSocketAttachment(this._wsId, value);
                this._attachment = value === undefined ? null : structuredClone(value);
            },
        },
        deserializeAttachment: {
            value() {
                return structuredClone(this._attachment);
            },
        },
    });

    const objectSocket = (id, mode, tags, attachment) => {
        const ws = Object.create(socketMethods);

        ws._wsId = id;
        ws._mode = mode;
        ws._tags = tags;
        ws._attachment = attachment;
        ws._listeners = { message: [], close: [], error: [], open: [] };
        ws.url = null;
        ws.binaryType = 'arraybuffer';
        ws.bufferedAmount = 0;
        ws.extensions = '';
        ws.protocol = '';
        ws.readyState = OPEN;

        return ws;
    };

    for (const { id, tags, attachment } of __nativeObjectSockets()) {
        sockets.set(id, objectSocket(id, 'handlers', tags, attachment));
    }

    // A pair made here gives a socket of the object
    globalThis.WebSocketPair = class WebSocketPair {
        constructor() {
            const id = __nativeObjectSocketId();

            this[0] = Object.freeze({ _pairId: id });
            this[1] = objectSocket(id, null, [], null);
        }
    };

    const ownSocket = (ws) => Object.getPrototypeOf(ws ?? {}) === socketMethods;

    const hibernatable = (ws) => ownSocket(ws) && ws._mode === 'handlers';

    const acceptWebSocket = (ws, tags = []) => {
        if (!ownSocket(ws)) {
            throw new TypeError('acceptWebSocket() takes the server end of a WebSocketPair made in this object');
        }

        if (ws._mode !== null) {
            throw new TypeError('WebSocket: the socket is accepted already');
        }

        if (!Array.isArray(tags) || tags.length > MAX_TAGS) {
            throw new TypeError(`acceptWebSocket() takes at most ${MAX_TAGS} tags`);
        }

        for (const tag of tags) {
            if (typeof tag !== 'string' || tag.length > MAX_TAG_LENGTH) {
                throw new TypeError(`a tag is a string of at most ${MAX_TAG_LENGTH} characters`);
            }
        }

        ws._mode = 'handlers';
        ws._tags = [...new Set(tags)];
        sockets.set(ws._wsId, ws);
        __nativeObjectSocketAccept(ws._wsId, ws._tags, true);
    };

    const unsupported = (name) => () => {
        throw new Error(`Durable Objects: ${name}() is not supported`);
    };

    const objectId = Object.freeze({
        toString: () => id,
        equals: (other) => other !== null && typeof other === 'object' && String(other) === id,
    });

    const state = Object.freeze({
        id: objectId,
        storage,
        // The object lives on after the response, so there is nothing to extend.
        waitUntil() {},
        acceptWebSocket,
        getWebSockets(tag) {
            return [...sockets.values()].filter(
                (ws) => ws._mode === 'handlers' && (tag === undefined || ws._tags.includes(tag))
            );
        },
        getTags(ws) {
            if (!hibernatable(ws)) {
                throw new TypeError('getTags() takes a socket accepted with acceptWebSocket()');
            }

            return [...ws._tags];
        },
        setWebSocketAutoResponse: unsupported('setWebSocketAutoResponse'),
        getWebSocketAutoResponse: unsupported('getWebSocketAutoResponse'),
        getWebSocketAutoResponseTimestamp: unsupported('getWebSocketAutoResponseTimestamp'),
        setHibernatableWebSocketEventTimeout: unsupported('setHibernatableWebSocketEventTimeout'),
        getHibernatableWebSocketEventTimeout: unsupported('getHibernatableWebSocketEventTimeout'),
        blockConcurrencyWhile(callback) {
            blocked += 1;

            const run = (async () => callback())();

            run.then(
                () => {
                    blocked -= 1;
                },
                (error) => {
                    failure ??= message(error);
                    blocked -= 1;
                }
            );

            return run;
        },
    });

    const instance = new Class(state, globalThis.env);

    return {
        fetch: (request) => dispatch.fetch(request, instance),
        blocked: () => blocked > 0,
        failure: () => failure,
        writing: () => writes > 0,
        alarm: (retryCount) => {
            const run = { done: false, error: null };

            Promise.resolve()
                .then(() => {
                    if (typeof instance.alarm !== 'function') {
                        throw new TypeError('the Durable Object class has no alarm() handler');
                    }

                    return instance.alarm({ retryCount, isRetry: retryCount > 0 });
                })
                .then(
                    () => {
                        run.done = true;
                    },
                    (error) => {
                        run.error = message(error);
                        run.done = true;
                    }
                );

            return run;
        },
        socket: (id, kind, data, reason) => {
            const run = { done: false };
            const ws = sockets.get(id);

            Promise.resolve()
                .then(() => {
                    // A socket this instance closed has no more events
                    if (ws === undefined) {
                        return;
                    }

                    if (kind !== 'message') {
                        ws.readyState = CLOSED;
                        sockets.delete(id);
                    }

                    if (ws._mode === 'listeners') {
                        if (kind === 'message') {
                            ws._dispatch('message', { type: 'message', data });
                        } else if (kind === 'close') {
                            ws._dispatch('close', { type: 'close', code: data, reason, wasClean: data !== 1006 });
                        } else {
                            ws._dispatch('error', { type: 'error', message: data });
                        }

                        return;
                    }

                    if (kind === 'message') {
                        return instance.webSocketMessage?.(ws, data);
                    }

                    if (kind === 'close') {
                        return instance.webSocketClose?.(ws, data, reason, data !== 1006);
                    }

                    return instance.webSocketError?.(ws, new Error(data));
                })
                .then(
                    () => {
                        run.done = true;
                    },
                    (error) => {
                        console.error(`Durable Object ${className}: the ${kind} handler of a WebSocket threw:`, error);
                        run.done = true;
                    }
                );

            return run;
        },
        pinned: () => [...sockets.values()].some((ws) => ws._mode === 'listeners'),
    };
})
