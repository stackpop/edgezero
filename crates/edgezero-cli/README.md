# edgezero-cli

Command-line tooling for the EdgeZero framework, shipped as **both a binary
and a library**. The binary (`edgezero`) provides the project-agnostic
scaffolding / build / deploy / serve flow across all four edge adapters. The
library exposes the same flow as `(<Cmd>Args, run_<cmd>)` pairs so a
downstream project can build its own CLI binary that reuses any subset of the
built-ins and adds its own subcommands.

See [docs/guide/cli-reference.md](../../docs/guide/cli-reference.md) for the
full user-facing reference; this README covers the crate surface and
contributor concerns.

## Feature Flags

| Feature                       | Description                                                                                                                                                   | Enabled by default |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------ |
| `cli`                         | Builds the command-line interface (`edgezero` binary).                                                                                                        | ✅                 |
| `edgezero-adapter-axum`       | Links the Axum adapter into the binary and registers its blueprint with the scaffolder.                                                                       | ✅                 |
| `edgezero-adapter-fastly`     | Links the Fastly adapter and registers its blueprint.                                                                                                         | ✅                 |
| `edgezero-adapter-cloudflare` | Links the Cloudflare adapter and registers its blueprint.                                                                                                     | ✅                 |
| `edgezero-adapter-spin`       | Links the Spin adapter and registers its blueprint.                                                                                                           | ✅                 |
| `demo-example`                | Pulls in `examples/app-demo/app-demo-core` so `edgezero demo` can boot the bundled example app. Contributor-only; enable when working on the in-repo example. | ❌                 |

Distributable build (default — includes all four adapters):

```bash
cargo build -p edgezero-cli --release
```

Slim build with only a specific adapter (e.g. axum-only):

```bash
cargo build -p edgezero-cli --release --no-default-features \
    --features "cli edgezero-adapter-axum"
```

Note: `--no-default-features --features cli` alone drops every
`edgezero-adapter-*` dep, so the resulting binary has **no** built-in
adapter helpers and `edgezero new` will scaffold nothing. Enable each
adapter feature you want to ship explicitly.

Contributor build with the bundled example:

```bash
cargo run -p edgezero-cli --features "cli,demo-example" -- demo
```

## Built-in commands

`edgezero` ships with these subcommands (see `edgezero --help` for full
flag detail):

- `edgezero new <name>` — scaffold a new EdgeZero workspace (core crate +
  per-adapter crates + a downstream `*-cli` crate).
- `edgezero build --adapter <name>` — build for `fastly`, `cloudflare`,
  `spin`, or `axum`. Runs the `[adapters.<name>.commands].build` shell
  command from `edgezero.toml` when present; otherwise falls back to the
  built-in adapter helper. Extra args after `--` are forwarded.
- `edgezero deploy --adapter <name>` — deploy via `fastly compute deploy` /
  `wrangler deploy` / `spin deploy`. (`axum` is not deployable through this
  command — use standard container/binary deployment.)
- `edgezero serve --adapter <name>` — run the local provider server:
  `fastly compute serve` / `wrangler dev` / `spin up` / native Axum.
- `edgezero demo` — run the bundled `app-demo` example locally on the
  native Axum adapter. **Contributor-only** — requires `--features
  demo-example`.

## Library API

Each command is exposed as a `(<Cmd>Args, run_<cmd>)` pair so downstream
binaries can wire any subset of the built-ins behind their own `clap` enum
and add additional subcommands. See
[Building Your Own CLI](../../docs/guide/cli-reference.md#building-your-own-cli)
for the canonical pattern; the generated `crates/<name>-cli` from `edgezero
new` is a working starting point.

```rust
use edgezero_cli::{run_build, run_deploy, run_new, run_serve};
use edgezero_cli::args::{BuildArgs, DeployArgs, NewArgs, ServeArgs};
// …compose into your own clap subcommand enum.
```

Argument structs derive `clap::Args` + `Default` and are
`#[non_exhaustive]`, so callers tolerate new fields in a future minor
release. Build them programmatically by starting from `Default::default()`
and assigning the fields you need — struct-literal expressions are not
allowed across crates for `#[non_exhaustive]` types, so the
`StructName { … ..Default::default() }` shorthand does not compile from
downstream code:

```rust
let mut args = BuildArgs::default();
args.adapter = "fastly".into();
edgezero_cli::run_build(&args)?;
```

## Adapter discovery

Built-in adapters register themselves at link time via the
`edgezero-adapter` registry. The set is determined by the workspace's
optional `edgezero-adapter-*` dependencies and their cargo features (see
[`build.rs`](build.rs)); the default-feature build includes all four
adapters (`fastly`, `cloudflare`, `spin`, `axum`).

## Developing the CLI

- Keep the `demo-example` dependency optional so downstream consumers
  aren't forced to ship example code.
- When changing scaffolded structure, update both the CLI templates under
  `src/templates/` and `examples/app-demo` to stay in sync.
- Run `cargo test -p edgezero-cli` and `cargo fmt --all -- --check` before
  opening a PR. The opt-in `cargo test -p edgezero-cli --test
  generated_project_builds -- --ignored` exercises the full scaffold path
  end-to-end (creates a temp workspace from the templates and compiles it).
