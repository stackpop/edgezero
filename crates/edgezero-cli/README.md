# edgezero-cli

Command-line tooling for the EdgeZero framework. The CLI handles scaffolding, local development, and (soon) build/deploy flows across edge adapters.

## Feature Flags & Optional Demo Dependency

The crate exposes two cargo features:

| Feature        | Description                                              | Enabled by default |
|----------------|----------------------------------------------------------|--------------------|
| `cli`          | Builds the command-line interface (`edgezero` binary).    | ✅                 |
| `demo-example` | Pulls in `examples/app-demo/app-demo-core` so `edgezero demo` can boot the bundled example app. Contributor-only; enable when working on the in-repo example. | ❌ |

When you just need the CLI functionality (e.g. packaging for distribution), build without the demo feature:

```bash
cargo build -p edgezero-cli --no-default-features --features cli
```

For contributors working on the bundled example, enable the extra feature:

```bash
cargo run -p edgezero-cli --features "cli,demo-example" -- demo
```

## Commands

_(summaries only; see `edgezero --help` for details)_

- `edgezero new <name>` – Scaffold a new EdgeZero project (templates still evolving).
- `edgezero serve --adapter <name>` – Run the current project locally on the named adapter.
- `edgezero demo` – Run the bundled `app-demo` example locally (contributor-only; requires `--features demo-example`).
- `edgezero build --adapter fastly` – Compile the Fastly crate to `wasm32-wasip1` and drop the artifact in `pkg/`.
- `edgezero deploy --adapter fastly` – Invoke the Fastly CLI (`fastly compute deploy`) from the detected Fastly crate.
- `edgezero serve --adapter fastly` – Run `fastly compute serve` in the Fastly crate directory for local testing (requires Fastly CLI).

## Developing the CLI

- Keep the demo dependency optional so downstream consumers aren't forced to ship example code.
- Update both the CLI templates and `examples/app-demo` when changing scaffolded project structure.
- Run `cargo test -p edgezero-cli` and `cargo fmt` before opening a PR.
