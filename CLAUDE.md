# OpenWorkers Runtime V8

V8-based JavaScript runtime.

## Stack

```
openworkers-core     ← Defines traits (Worker, Task, Response...)
openworkers-runtime-v8  ← Implements traits (this crate)
openworkers-runner   ← Uses runtime to execute workers
```

The runner selects one runtime by cargo feature: V8, JSC, QuickJS, Boa or WASM.

## Development setup

`openworkers-core` must be in sibling folder (`../openworkers-core`).

The engine is `openworkers-v8` (crates.io), renamed to `v8` in Cargo.toml:
upstream's tag plus the openworkers/rusty-v8 patches. Its prebuilt static
library and binding download automatically from that repo's GitHub release
for every variant we build (pointer compression, sandbox, aarch64-linux
included), so no environment variables are needed.

`serde_v8` and `glue_v8` are our forks, so their crates.io names are patched
to the sibling checkouts:

```toml
[patch.crates-io]
openworkers-serde-v8 = { path = "../serde-v8" }
openworkers-glue-v8 = { path = "../glue-v8" }
```

Cargo honours `[patch]` only in the root manifest, so a crate that depends on
this one has to repeat it.

### Offline builds

`RUSTY_V8_ARCHIVE` and `RUSTY_V8_SRC_BINDING_PATH` can point at local copies
of the release assets to skip the download. Both must match the `v8` version
in `Cargo.toml`, or the bindings will not describe the library they are
linked against.

## After editing

1. `cargo fmt`
2. `cargo run --bin snapshot` (if `src/runtime/*.rs` changed)
3. Run tests
4. Run benchmarks (after refactoring)

## Versioning

Major versions follow `openworkers-core` (e.g., core 0.11.x → runtime 0.11.x).

After bumping version in `Cargo.toml`, run `cargo check` to update `Cargo.lock` before committing.

## Architecture

See [docs/architecture.md](docs/architecture.md)
