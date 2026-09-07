# Dependency Majors Migration

EdgeZero moved several dependencies across major versions at once. An
application generated before that change keeps its own pins, and those
pins have to move together with EdgeZero's — the adapters hand
provider-owned types (`fastly::Request`, `spin_sdk::http::Request`)
straight through, and typed config derives `validator::Validate`.

A crate resolved at two majors produces two unrelated types with the
same name, so the mismatch surfaces as a trait or type error pointing
at your own code:

```
error[E0277]: the trait bound `MyAppConfig: validator::traits::Validate`
is not satisfied
```

Nothing in that message names the real cause, which is why this page
exists.

## What to change

Update these in your application's workspace `Cargo.toml` at the same
time as the EdgeZero dependency:

| Crate        | Old    | New    | Applies to                  |
| ------------ | ------ | ------ | --------------------------- |
| `fastly`     | `0.12` | `0.13` | Fastly adapter              |
| `log-fastly` | `0.12` | `0.13` | Fastly adapter              |
| `spin-sdk`   | `6`    | `7`    | Spin adapter                |
| `validator`  | `0.20` | `0.21` | Any typed app config        |
| `rusqlite`   | `0.32` | `0.40` | Only if you use it directly |

Then refresh the lockfile:

```bash
cargo update
cargo test --workspace --all-targets
```

## Runtime requirement: Spin 4.1

`spin-sdk` 7 imports `wasi:http/types@0.3.0`, which Spin 4.0.x does not
provide. This one does not fail at build time — the component compiles,
its tests pass under `wasmtime`, and only `spin up` reports:

```
Error: component imports instance `wasi:http/types@0.3.0`, but a
matching implementation was not found in the linker
```

Install Spin 4.1 or newer ([install](https://spinframework.dev/install))
before upgrading the SDK. `cargo check` cannot detect this.

## Toolchain

The workspace builds on Rust 1.98.1. If your application pins a
toolchain in `.tool-versions`, move it up with the dependencies; the
`edition = "2024"` crates in EdgeZero need 1.85 or newer regardless.

## Verifying

The mismatch this page describes is a compile error, so a clean build
is the check:

```bash
cargo build --workspace
cargo test --workspace --all-targets
```

For the Spin runtime requirement, build is not enough — run the app:

```bash
spin --version          # expect 4.1.0 or newer
spin up --from crates/<your-app>-adapter-spin
```
