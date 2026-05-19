# Extensible `edgezero-cli` Library (sub-project #1)

**Date:** 2026-05-19
**Status:** Approved design, pending implementation plan
**Roadmap position:** Sub-project 1 of 7 in the CLI extensions effort
(extensible lib + `app-demo-cli` skeleton → app-config schema → `config
validate` → `auth` → `provision` → `config push` → `app-demo` integration
polish). This spec covers sub-project #1 only. `app-demo` is updated
incrementally across all seven sub-projects, not backloaded to the end.

## Goal

Let downstream projects build their own CLI binary that:

- Reuses any subset of edgezero's built-in commands (today: `build`, `deploy`,
  `dev`, `new`, `serve`).
- Adds their own subcommands.
- Owns the binary name, `about` text, and top-level help.

The default `edgezero` binary keeps working unchanged for users who do not
build their own CLI.

Ship `app-demo-cli` in the same sub-project as the canonical downstream
consumer. It uses every built-in verbatim today (no custom subcommands
yet) and becomes the staging ground each later sub-project extends.

## Non-goals

- No runtime command registry (`inventory` / `linkme`-style).
- No cargo-style external subcommand discovery on PATH.
- No re-exposing internal modules (`adapter`, `generator`, `scaffold`,
  `dev_server`) — only high-level `run_*` entry points and per-command
  `*Args` structs.
- No renaming or hiding individual built-ins via a library API — opt-out
  happens by omission in the downstream `Subcommand` enum.
- No new commands (`auth`, `provision`, `config`). Those are sub-projects
  3–6 and will add their own `*Args` + `run_*` pairs once this substrate
  ships.

## Approach

Use clap-derive composition. `edgezero-cli` becomes lib + bin in one crate:

- New `crates/edgezero-cli/src/lib.rs` — public API surface.
- Existing `crates/edgezero-cli/src/main.rs` — rewritten as a thin wrapper
  that depends only on the public API.

The library exposes one `*Args` struct per built-in command plus one
`run_*` function per command. Downstream projects compose their own
`#[derive(Subcommand)]` enum that mixes edgezero variants with their own,
and write a small `main` that dispatches each variant.

## Public API surface

```rust
// crates/edgezero-cli/src/lib.rs (feature = "cli")

pub use args::{BuildArgs, DeployArgs, NewArgs, ServeArgs};

pub fn init_cli_logger();

pub fn run_build(args: &BuildArgs) -> Result<(), String>;
pub fn run_deploy(args: &DeployArgs) -> Result<(), String>;
pub fn run_new(args: &NewArgs) -> Result<(), String>;
pub fn run_serve(args: &ServeArgs) -> Result<(), String>;

#[cfg(feature = "edgezero-adapter-axum")]
pub fn run_dev() -> !;
```

Everything else (`adapter`, `generator`, `scaffold`, `dev_server` modules;
`load_manifest_optional`, `ensure_adapter_defined`, `store_bindings_message`)
stays private to the crate.

### Pattern for adding future built-ins (informational)

When sub-projects 3–6 add their commands, each one follows the same
two-symbol pattern:

```rust
pub use args::AuthArgs;
pub fn run_auth(args: &AuthArgs) -> Result<(), String>;
```

This pattern is established here; later specs will not need to re-justify
the shape.

## Downstream usage (canonical example)

```rust
// myapp-cli/src/main.rs
use clap::{Parser, Subcommand};
use edgezero_cli::{BuildArgs, DeployArgs, ServeArgs};

#[derive(Parser)]
#[command(name = "myapp", about = "MyApp edge CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Build(BuildArgs),
    Deploy(DeployArgs),
    Serve(ServeArgs),
    // Opt out of `new` and `dev`: simply not listed.
    Migrate(MigrateArgs), // downstream's own
    Seed,
}

fn main() {
    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Cmd::Build(a) => edgezero_cli::run_build(&a),
        Cmd::Deploy(a) => edgezero_cli::run_deploy(&a),
        Cmd::Serve(a) => edgezero_cli::run_serve(&a),
        Cmd::Migrate(a) => run_migrate(a),
        Cmd::Seed => run_seed(),
    };
    if let Err(err) = result {
        log::error!("[myapp] {err}");
        std::process::exit(1);
    }
}
```

Opt-in is "add the variant"; opt-out is "don't". No machinery beyond clap.

## Source layout changes

1. **`crates/edgezero-cli/src/args.rs`** — promote each `Command` variant's
   inline fields into a standalone `#[derive(clap::Args)]` struct. `NewArgs`
   already exists. The internal `Command` enum (used only by the default
   `edgezero` binary) becomes:

   ```rust
   #[derive(Subcommand, Debug)]
   pub enum Command {
       Build(BuildArgs),
       Deploy(DeployArgs),
       Dev,
       New(NewArgs),
       Serve(ServeArgs),
   }
   ```

   The four new public structs each carry exactly the fields the variant
   currently inlines (`adapter`, `adapter_args`, etc.). No new fields.

2. **`crates/edgezero-cli/src/lib.rs` (new)** — declares the private
   `adapter`, `generator`, `scaffold`, and (feature-gated) `dev_server`
   modules. Moves `init_cli_logger`, `load_manifest_optional`,
   `ensure_adapter_defined`, `store_bindings_message`, `log_store_bindings`,
   and the five handlers (renamed `handle_*` → `run_*`) into this file.

3. **`crates/edgezero-cli/src/main.rs`** — shrinks to roughly:

   ```rust
   use clap::Parser as _;
   use edgezero_cli::{run_build, run_deploy, run_new, run_serve};

   fn main() {
       edgezero_cli::init_cli_logger();
       let args = edgezero_cli::Args::parse();
       let result = match args.cmd {
           edgezero_cli::Command::Build(a)  => run_build(&a),
           edgezero_cli::Command::Deploy(a) => run_deploy(&a),
           edgezero_cli::Command::New(a)    => run_new(&a),
           edgezero_cli::Command::Serve(a)  => run_serve(&a),
           edgezero_cli::Command::Dev       => edgezero_cli::run_dev(),
       };
       if let Err(err) = result {
           log::error!("[edgezero] {err}");
           std::process::exit(1);
       }
   }
   ```

   `Args` and `Command` are re-exported from `lib.rs` only so the default
   binary can build against the public API.

4. **Existing tests** — move from `main.rs` to `lib.rs` (they test what are
   now public functions). Assertions are unchanged.

5. **`examples/app-demo/crates/app-demo-cli` (new crate)** — added as a
   member of the `examples/app-demo` workspace. That workspace is
   excluded from the root `Cargo.toml` workspace and stays that way.
   Layout:

   ```
   examples/app-demo/crates/app-demo-cli/
     Cargo.toml
     src/main.rs
   ```

   Wiring:

   - Add `"crates/app-demo-cli"` to `members` in
     `examples/app-demo/Cargo.toml`.
   - Add `edgezero-cli = { path = "../../../../crates/edgezero-cli" }` to
     the example workspace's `[workspace.dependencies]` (mirroring the
     existing pattern for `edgezero-core` at line 23 of that file).
   - The new crate's `Cargo.toml` declares:
     - `name = "app-demo-cli"` (package and default binary name match;
       no `[[bin]]` section needed)
     - `edgezero-cli = { workspace = true }` with the default feature set
     - `clap = { version = "4", features = ["derive"] }`
     - `log = { workspace = true }`
   - `publish = false`, `[lints] workspace = true` to match siblings.

   `src/main.rs` implements the canonical downstream pattern from the
   "Downstream usage" section above, with **all five built-ins included
   verbatim and no custom subcommands yet**:

   ```rust
   use clap::{Parser, Subcommand};
   use edgezero_cli::{BuildArgs, DeployArgs, NewArgs, ServeArgs};

   #[derive(Parser)]
   #[command(name = "app-demo-cli", about = "app-demo edge CLI")]
   struct Args { #[command(subcommand)] cmd: Cmd }

   #[derive(Subcommand)]
   enum Cmd {
       Build(BuildArgs),
       Deploy(DeployArgs),
       Dev,
       New(NewArgs),
       Serve(ServeArgs),
   }

   fn main() {
       edgezero_cli::init_cli_logger();
       let result = match Args::parse().cmd {
           Cmd::Build(a)  => edgezero_cli::run_build(&a),
           Cmd::Deploy(a) => edgezero_cli::run_deploy(&a),
           Cmd::Dev       => edgezero_cli::run_dev(),
           Cmd::New(a)    => edgezero_cli::run_new(&a),
           Cmd::Serve(a)  => edgezero_cli::run_serve(&a),
       };
       if let Err(err) = result {
           log::error!("[app-demo-cli] {err}");
           std::process::exit(1);
       }
   }
   ```

   No changes to existing `app-demo` crates, `edgezero.toml`, or routes.
   `app-demo-cli` is purely additive: it gives the example workspace a
   binary that exercises the new lib end-to-end. Later sub-projects add
   custom variants (`Auth`, `Provision`, `Config`) to this same `Cmd`
   enum and the matching `app-demo.toml` plumbing.

## Cargo manifest changes

- `crates/edgezero-cli/Cargo.toml`: the crate already builds an implicit
  binary from `src/main.rs`; adding `src/lib.rs` makes it lib + bin
  automatically. No explicit `[lib]` or `[[bin]]` section needed.
- The `cli` feature continues to gate clap. All public API lives under
  `#[cfg(feature = "cli")]`. `cli` remains in the `default` feature set so
  normal consumers are unaffected.
- Adapter feature gates carry over: `run_dev` requires
  `edgezero-adapter-axum`. `run_build`, `run_deploy`, and `run_serve`
  dispatch by adapter name at runtime and surface a clear error if the
  named adapter's feature is disabled (current behavior preserved).

## Tests

- **Move existing tests:** every `#[test]` currently in `main.rs` moves to
  `lib.rs`. No behavior change.
- **New integration test:** `crates/edgezero-cli/tests/lib_consumer.rs`.
  Imports `edgezero_cli` as an external consumer would, constructs
  `BuildArgs` programmatically, and invokes `run_build` against a temp-dir
  manifest (mirroring the existing `handle_build_executes_manifest_command`
  test). This proves the public API actually compiles from outside the
  crate root and produces the same result.
- **`app-demo-cli` build smoke test:** the example workspace must
  successfully compile the new binary. The implementation plan will
  identify the existing CI step or script that validates the
  `examples/app-demo` workspace and extend it to run `cargo build -p
  app-demo-cli` (or `cargo build --workspace` from inside the example
  workspace, if that's what's already in use). A minimal `--help`
  invocation test in
  `examples/app-demo/crates/app-demo-cli/tests/help.rs` confirms the
  binary parses its CLI without panicking.
- All four CI gates (`fmt`, `clippy -D warnings`, `cargo test`, feature
  `cargo check`) must pass. The wasm32 spin gate is unaffected by this
  change (no adapter crate touched).

## Documentation

- New page at `docs/cli/extending.md` (linked from the docs sidebar) showing
  the canonical downstream example, the list of public `*Args` / `run_*`
  symbols, and which Cargo features to enable.
- `CLAUDE.md` workspace-layout section gets one sentence noting
  `edgezero-cli` is lib + bin.

## Risks and trade-offs

- **API stability:** promoting the four arg structs to public surface means
  future field additions become semver-affecting. Mitigation: every
  `*Args` struct gets `#[non_exhaustive]` so we can add fields without a
  breaking change. New constructors are not needed — clap derive is the
  intended construction path.
- **Test relocation churn:** moving ~10 tests from `main.rs` to `lib.rs` is
  mechanical but touches a familiar file. Reviewers will see a large diff
  with no behavior change; PR description must call this out.
- **Adapter-feature coupling:** `run_dev` being gated on
  `edgezero-adapter-axum` means a downstream that disables that feature
  loses access to the symbol entirely. This is the same constraint the
  current `edgezero dev` command has; we're not making it worse, just
  exposing it through the type system.

## What this spec does NOT cover

- The new commands (`auth`, `provision`, `config validate`, `config push`)
  — each gets its own spec. Those specs will add new public `*Args` /
  `run_*` symbols to `edgezero-cli` and new variants to `app-demo-cli`'s
  `Cmd` enum, without modifying the substrate established here.
- The new `app-demo.toml` schema and loader — separate spec.
- Any change to existing `app-demo` crates (`app-demo-core`,
  `app-demo-adapter-*`), to `edgezero.toml`, or to routes.

When sub-project #1 ships:

- The default `edgezero` binary still works exactly as before.
- An unrelated downstream project can already build its own CLI against
  `edgezero-cli` as a library.
- `app-demo-cli` exists as the canonical consumer and is wired into the
  example workspace.

Sub-projects 2–7 extend this substrate; they do not modify it.
