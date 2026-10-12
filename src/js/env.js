// Installs globalThis.env, read-only: the worker's variables, then one object
// per binding, which wins over a variable of the same name. The host calls
// this function with both as JSON data.
(function installEnv(vars, bindings) {
    // A request through a binding's fetch, with its body encoded as the
    // global fetch encodes one.
    const bindingFetch = async (nativeFn, name, input, options) => {
        const { url, method, headers, body } = __normalizeFetchInput(input, options);
        const bytes = await __encodeFetchBody(body, headers);

        return new Promise((resolve, reject) => {
            nativeFn(name, { url, method, headers, body: bytes }, (meta) => {
                resolve(__responseFromMeta(meta));
            }, reject);
        });
    };

    // D1 statements, keyed by the frozen object the guest holds, so a batch
    // reads the SQL and the values the guest cannot change.
    const d1Statements = new WeakMap();

    const d1Value = (value) => {
        if (value === null || typeof value === 'number' || typeof value === 'string'
            || typeof value === 'boolean' || value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
            return value;
        }

        throw new Error(`D1_TYPE_ERROR: Type '${typeof value}' not supported for value '${String(value)}'`);
    };

    const d1Row = (columns, values) => {
        const row = {};

        columns.forEach((column, index) => {
            row[column] = values[index];
        });

        return row;
    };

    const d1Result = ({ columns, rows, meta }) => ({
        success: true,
        meta: {
            ...meta,
            timings: { sql_duration_ms: meta.duration },
            served_by_primary: true,
            total_attempts: 1,
        },
        results: rows.map((values) => d1Row(columns, values)),
    });

    const d1Database = (name) => {
        const call = (op) => new Promise((resolve, reject) => __nativeBindingD1(name, op, resolve, reject));
        const query = (statement, rows) => call({ kind: 'query', statement, rows });

        const prepared = (sql, params) => {
            const statement = { sql, params };

            const stmt = Object.freeze({
                bind: (...values) => prepared(sql, values.map(d1Value)),
                all: () => query(statement, 'all').then(d1Result),
                run: () => query(statement, 'all').then(d1Result),
                raw: (options) => query(statement, 'all')
                    .then(({ columns, rows }) => options?.columnNames ? [columns, ...rows] : rows),
                first: (column) => query(statement, 'first').then(({ columns, rows }) => {
                    if (rows.length === 0) {
                        return null;
                    }

                    if (column === undefined) {
                        return d1Row(columns, rows[0]);
                    }

                    if (!columns.includes(column)) {
                        throw new Error(`D1_ERROR: Column not found: ${column}`);
                    }

                    return d1Row(columns, rows[0])[column];
                }),
            });

            d1Statements.set(stmt, { name, statement });

            return stmt;
        };

        return Object.freeze({
            prepare: (sql) => {
                if (typeof sql !== 'string') {
                    throw new TypeError('D1: prepare() needs a SQL string');
                }

                return prepared(sql, []);
            },
            batch: async (list) => {
                if (!Array.isArray(list)) {
                    throw new TypeError('D1: batch() needs an array of statements');
                }

                const statements = list.map((stmt) => {
                    const entry = d1Statements.get(stmt);

                    if (entry === undefined || entry.name !== name) {
                        throw new TypeError('D1: batch() takes statements prepared on this binding only');
                    }

                    return entry.statement;
                });

                if (statements.length === 0) {
                    return [];
                }

                const { results } = await call({ kind: 'batch', statements });

                return results.map(d1Result);
            },
            exec: (sql) => {
                if (typeof sql !== 'string') {
                    return Promise.reject(new TypeError('D1: exec() needs a SQL string'));
                }

                return call({ kind: 'exec', sql }).then(({ count, duration }) => ({ count, duration }));
            },
            withSession: () => {
                throw new Error('D1_ERROR: withSession() is not supported');
            },
            dump: () => Promise.reject(new Error('D1_ERROR: dump() is not supported')),
        });
    };

    // Durable Object ids, keyed by the frozen object the guest holds, so get()
    // reads the namespace and the id the guest cannot change.
    const objectIds = new WeakMap();

    const objectId = (binding, id, name) => {
        const value = Object.freeze({
            name,
            toString: () => id,
            equals: (other) => other !== null && typeof other === 'object' && String(other) === id,
        });

        objectIds.set(value, { binding, id });

        return value;
    };

    const noJurisdiction = (options) => {
        if (options?.jurisdiction !== undefined) {
            throw new TypeError('Durable Objects: jurisdictions are not supported');
        }
    };

    const durableObjectNamespace = (binding) => {
        const idFromName = (name) => {
            if (typeof name !== 'string') {
                throw new TypeError('Durable Objects: idFromName() needs a string');
            }

            return objectId(binding, __nativeDurableObjectId(binding, 'name', name), name);
        };

        const get = (id) => {
            const entry = objectIds.get(id);

            if (entry === undefined || entry.binding !== binding) {
                throw new TypeError('Durable Objects: get() takes an id of this namespace');
            }

            const send = (name, request, resolve, reject) =>
                __nativeBindingDurableObject(name, entry.id, request, resolve, reject);

            return Object.freeze({
                id,
                name: id.name,
                fetch: (input, options) => bindingFetch(send, binding, input, options),
            });
        };

        return Object.freeze({
            idFromName,
            idFromString: (text) => {
                if (typeof text !== 'string') {
                    throw new TypeError('Durable Objects: idFromString() needs a string');
                }

                return objectId(binding, __nativeDurableObjectId(binding, 'text', text));
            },
            newUniqueId: (options) => {
                noJurisdiction(options);

                return objectId(binding, __nativeDurableObjectId(binding, 'unique'));
            },
            get: (id) => get(id),
            getByName: (name) => get(idFromName(name)),
            jurisdiction: () => {
                throw new TypeError('Durable Objects: jurisdictions are not supported');
            },
        });
    };

    const make = {
        assets: (name) => ({
            fetch: (input, options) => bindingFetch(__nativeBindingFetch, name, input, options),
        }),

        storage: (name) => ({
            get: (key) => __bindingCall(__nativeBindingStorage, name, 'get', { key })
                .then(r => r.body ? new TextDecoder().decode(r.body) : null),
            put: (key, value) => {
                const body = typeof value === 'string' ? new TextEncoder().encode(value) : value;
                return __bindingCall(__nativeBindingStorage, name, 'put', { key, body }).then(() => {});
            },
            head: (key) => __bindingCall(__nativeBindingStorage, name, 'head', { key })
                .then(r => ({ size: r.size, etag: r.etag })),
            list: (options) => __bindingCall(__nativeBindingStorage, name, 'list', { prefix: options?.prefix, limit: options?.limit })
                .then(r => ({ keys: r.keys, truncated: r.truncated })),
            delete: (key) => __bindingCall(__nativeBindingStorage, name, 'delete', { key }).then(() => {}),
            fetch(input, options) {
                const { url } = __normalizeFetchInput(input, options);
                const key = new URL(url, 'http://localhost').pathname;
                return __bindingCall(__nativeBindingStorage, name, 'fetch', { key })
                    .then(r => __responseFromMeta(r));
            },
        }),

        kv: (name) => ({
            get: (key) => __bindingCall(__nativeBindingKv, name, 'get', { key }).then(r => r.value),
            put: (key, value, options) => {
                const params = { key, value };
                if (options?.expiresIn) params.expiresIn = options.expiresIn;
                return __bindingCall(__nativeBindingKv, name, 'put', params).then(() => {});
            },
            delete: (key) => __bindingCall(__nativeBindingKv, name, 'delete', { key }).then(() => {}),
            list: (options) => {
                const params = {};
                if (options?.prefix) params.prefix = options.prefix;
                if (options?.limit) params.limit = options.limit;
                return __bindingCall(__nativeBindingKv, name, 'list', params).then(r => r.keys);
            },
        }),

        database: (name) => ({
            query: (sql, params) => __bindingCall(__nativeBindingDatabase, name, 'query', { sql, params: params || [] })
                .then(r => r.rows),
        }),

        d1: (name) => d1Database(name),

        worker: (name) => ({
            fetch: (input, options) => bindingFetch(__nativeBindingWorker, name, input, options),
        }),

        durable_object: (name) => durableObjectNamespace(name),

        // No native handler yet, so the binding exists only to say so
        images: () => ({
            input() {
                throw new Error('images binding is not supported by this runtime');
            },
        }),
    };

    const env = Object.assign({}, vars);

    for (const { name, type } of bindings) {
        env[name] = make[type](name);
    }

    Object.defineProperty(globalThis, 'env', {
        value: Object.freeze(env),
        writable: false,
        enumerable: true,
        configurable: false,
    });
})
