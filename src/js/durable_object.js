// A Durable Object: the instance of a class of the worker, and the dispatch
// its events go through.
//
// The script evaluates to a function that the host calls once, after the
// worker script, with the `{ fetch, task }` object of the wintertc dispatch,
// the class and the id of the object. The call runs the constructor and
// answers what the host keeps in place of the dispatch:
//
//   fetch(request)  the handle of the wintertc dispatch for the request
//   blocked()       whether a blockConcurrencyWhile callback runs: the host
//                   delivers no event until it ends
//   failure()       the error of a blockConcurrencyWhile callback that
//                   failed, after which the object must be dropped

(function installObject(dispatch, Class, id) {
    'use strict';

    let blocked = 0;
    let failure = null;

    const objectId = Object.freeze({
        toString: () => id,
        equals: (other) => other !== null && typeof other === 'object' && String(other) === id,
    });

    const state = Object.freeze({
        id: objectId,
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
                    failure ??= (error && error.message) || String(error);
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
    };
})
