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

        worker: (name) => ({
            fetch: (input, options) => bindingFetch(__nativeBindingWorker, name, input, options),
        }),

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
