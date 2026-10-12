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
    const write = (op) => {
        let done;

        try {
            done = call({ kind: 'write', puts: [], deletes: [], deleteAll: false, ...op });
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
        getAlarm() {
            return Promise.reject(new Error('Durable Object alarms are not supported yet'));
        },
        setAlarm() {
            return Promise.reject(new Error('Durable Object alarms are not supported yet'));
        },
        deleteAlarm() {
            return Promise.reject(new Error('Durable Object alarms are not supported yet'));
        },
        get sql() {
            throw new Error('Durable Object SQL storage is not supported');
        },
    });

    const objectId = Object.freeze({
        toString: () => id,
        equals: (other) => other !== null && typeof other === 'object' && String(other) === id,
    });

    const state = Object.freeze({
        id: objectId,
        storage,
        // The object lives on after the response, so there is nothing to extend.
        waitUntil() {},
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
    };
})
