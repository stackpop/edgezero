# anyedge-cli

Command-line tooling for the AnyEdge framework. The CLI handles scaffolding, local development, and (soon) build/deploy flows across edge adapters.

## Feature Flags & Optional Demo Dependency

The crate exposes two cargo features:

| Feature        | Description                                              | Enabled by default |
|----------------|----------------------------------------------------------|--------------------|
| `cli`          | Builds the command-line interface (`anyedge` binary).    | ✅                 |
| `dev-example`  | Pulls in `examples/app-demo/app-demo-core` so `anyedge dev` can boot the bundled demo app. Enable only when you want the sample router available. | ❌ |

When you just need the CLI functionality (e.g. packaging for distribution), build without the demo feature:

```bash
cargo build -p anyedge-cli --no-default-features --features cli
```

For contributors working on the demo, enable the extra feature:

```bash
cargo run -p anyedge-cli --features "cli,dev-example" -- dev
```

## Commands

_(summaries only; see `anyedge --help` for details)_

- `anyedge new <name>` – Scaffold a new AnyEdge project (templates still evolving).
- `anyedge dev` – Serve the current project locally (add `--features dev-example` to run the bundled demo).
- `anyedge build --adapter fastly` – Compile the Fastly crate to `wasm32-wasip1` and drop the artifact in `pkg/`.
- `anyedge deploy --adapter fastly` – Invoke the Fastly CLI (`fastly compute deploy`) from the detected Fastly crate.
- `anyedge serve --adapter fastly` – Run `fastly compute serve` in the Fastly crate directory for local testing (requires Fastly CLI).

## Developing the CLI

- Keep the demo dependency optional so downstream consumers aren’t forced to ship example code.
- Update both the CLI templates and `examples/app-demo` when changing scaffolded project structure.
- Run `cargo test -p anyedge-cli` and `cargo fmt` before opening a PR.
