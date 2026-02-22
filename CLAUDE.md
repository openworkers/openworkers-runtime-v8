# OpenWorkers Runtime V8

V8-based JavaScript runtime.

## Stack

```
openworkers-core     ← Defines traits (Worker, Task, Response...)
openworkers-runtime-v8  ← Implements traits (this crate)
openworkers-runner   ← Uses runtime to execute workers
```

Multiple runtimes exist (V8, QuickJS, JSC). Runner picks one.

## Development setup

`openworkers-core` must be in sibling folder (`../openworkers-core`).

### Local rusty-v8 development

When modifying the V8 engine fork (`rusty-v8`), switch to a local path dependency:

```toml
# Cargo.toml — replace the crates.io version:
v8 = { package = "openworkers-v8", path = "../rusty-v8" }

# Add patch so serde_v8/glue_v8 also resolve to the local fork:
[patch.crates-io]
openworkers-v8 = { path = "../rusty-v8" }
```

The `[patch.crates-io]` is required because `serde_v8` and `glue_v8` depend on
`openworkers-v8` via crates.io. Without it, Cargo resolves two different versions
and V8 types become incompatible.

Once the new rusty-v8 version is published on crates.io, revert to:

```toml
v8 = { package = "openworkers-v8", version = "145.0.2" }
# Remove [patch.crates-io] section
```

### Prebuilt V8 binaries

If V8 itself hasn't been modified (only Rust bindings changed), you can skip the
full V8 build (~10h CI) by pointing to prebuilt artifacts:

```bash
export RUSTY_V8_ARCHIVE=/tmp/rusty-v8-prebuilt/librusty_v8_ptrcomp_release_aarch64-apple-darwin.a
export RUSTY_V8_SRC_BINDING_PATH=/tmp/rusty-v8-prebuilt/src_binding_ptrcomp_release_aarch64-apple-darwin.rs
```

These are downloaded from the CI artifacts of a previous rusty-v8 release.
With these set, `cargo check` / `cargo build` skips the C++ compilation entirely.

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
