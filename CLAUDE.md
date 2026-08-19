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

The engine is upstream `v8` (crates.io). `serde_v8` and `glue_v8` are our forks,
so their crates.io names are patched to the sibling checkouts:

```toml
[patch.crates-io]
openworkers-serde-v8 = { path = "../serde-v8" }
openworkers-glue-v8 = { path = "../glue-v8" }
```

Cargo honours `[patch]` only in the root manifest, so a crate that depends on
this one has to repeat it.

### Prebuilt V8 binaries

Point at a local archive to skip the C++ build:

```bash
export RUSTY_V8_ARCHIVE=~/rusty-v8-prebuilt/librusty_v8_ptrcomp_release_aarch64-apple-darwin.a
export RUSTY_V8_SRC_BINDING_PATH=~/rusty-v8-prebuilt/src_binding_ptrcomp_release_aarch64-apple-darwin.rs
```

Both must match the `v8` version in `Cargo.toml`, or the bindings will not
describe the library they are linked against.

Upstream publishes pointer-compression builds for aarch64/x86_64 macOS and
x86_64 linux, and no sandbox build at all. `openworkers/rusty-v8` fills those
gaps, so CI reads both:

```bash
export RUSTY_V8_MIRROR=https://github.com/openworkers/rusty-v8/releases/download
export RUSTY_V8_MIRROR_FALLBACK=1
```

The fallback flag is what keeps everything else coming from upstream.

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
