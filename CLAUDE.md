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
