# `provision --local`: write per-adapter local emulator state

## Motivation

`edgezero provision --adapter <name>` today creates **platform**
resources -- KV namespaces in Cloudflare, Config Stores in Fastly,
edits to `spin.toml` for Spin. Cloudflare and Fastly shell out to
their respective cloud CLIs.

Operators iterating locally (`wrangler dev`, `viceroy serve`,
`spin up`, the Axum dev server) need the **local emulator**
equivalent: the per-adapter on-disk files those tools read for the
same set of resources, populated with default-but-overridable
values, with **no** cloud calls.

This spec adds `--local` to `provision`. It writes adapter-specific
local environment files in a deterministic, idempotent way. It MUST
NOT touch the cloud. A subsequent `config push --local` only updates
the config blob, leaving everything provision wrote intact.

## CLI

```
<app-cli> provision --adapter <name> --local [--dry-run]
```

- `--local` switches the entire flow from cloud-SDK shell-outs to
  local-file writes. The two modes do not compose: a single
  invocation is either cloud or local, never both. Without
  `--local`, provision continues the existing cloud-CLI shellouts.
- `--dry-run` reports what would be written without touching the
  filesystem.
- Operates on the adapter's manifest paths (`wrangler.toml`,
  `fastly.toml`, `spin.toml`, `runtime-config.toml`) -- all
  gitignored per a later section. If a manifest is missing,
  `provision --local`'s CLI-owned bootstrap synthesises a
  minimal valid file from `edgezero.toml` primitives via
  `toml_edit::DocumentMut` (NOT from the scaffold `.hbs`
  templates -- those carry generator-only placeholders that
  aren't reconstructible from `edgezero.toml` alone; the
  `.hbs` files remain in use only by `edgezero new`'s
  scaffold path). The synthesiser's exact per-adapter output
  is specified in the "Primitive synthesiser output" section.
- Never creates files outside the adapter crate's directory or
  the gitignored local-state directories (`.wrangler/`, `.spin/`,
  `.edgezero/`, `.dev.vars`, `.env`). This is enforced by the
  path-containment rule below, not just by convention.

### Path containment (MUST)

Today's manifest schema accepts `[adapters.<name>.adapter].crate`
and `.manifest` as non-empty strings only
(`crates/edgezero-core/src/manifest.rs:379`, `:397`); the
top-level validator at `manifest.rs:751` does not reject
absolute paths or `..` traversal. Adapters then join those
values directly against `manifest_root` -- e.g.
`crates/edgezero-adapter-cloudflare/src/cli.rs:204`,
`crates/edgezero-adapter-fastly/src/cli.rs:220`,
`crates/edgezero-adapter-spin/src/cli.rs:200`. A malicious or
typo'd `manifest = "../outside/spin.toml"` or
`manifest = "/etc/wrangler.toml"` would let a `provision --local`
run write OUTSIDE the project tree, breaking the "never
creates files outside" promise AND breaking the dry-run
tempdir's worktree-isolation guarantee (the tempdir is only
safe if every write actually lands under it).

The implementing PR adds a `pub(crate)` containment helper
in a shared module both provision and push can import
(`crates/edgezero-cli/src/path_safety.rs` is the canonical
location; provision and config push both `use` it). The
helper runs at the TOP of every local-writing CLI entry
point, BEFORE any manifest-path / crate-path use:

```rust
pub(crate) fn assert_provision_paths_contained(
    project_root: &Path,
    adapter_manifest_path: Option<&str>,
    adapter_crate_path: Option<&str>,
) -> Result<(), String> {
    // Normalise the ROOT first. Callers commonly pass
    // `args.manifest.parent()` (e.g. provision.rs:78,
    // config.rs:841), which can be relative ("." for a
    // bare `--manifest edgezero.toml`, or
    // "examples/app-demo" for an explicit relative path).
    // If we compared a `.`-stripped joined path against
    // a literal "." root, `starts_with` would silently
    // reject every valid path. Normalising both sides to
    // the same shape closes that gap.
    let root = lexical_normalize(project_root);
    // When `root` normalises to "." (caller passed "" or "." --
    // a bare `--manifest edgezero.toml` or an explicit
    // cwd-relative path), the joined-vs-root `starts_with`
    // check is structurally broken: `lexical_normalize` strips
    // the leading `./` from the join, leaving e.g.
    // `crates/cf/wrangler.toml` -- which does NOT start with
    // ".". Skip Step 1's containment check in that case; the
    // absolute + `..` rejection below already guarantees the
    // candidate sits under cwd, and Step 2 (manifest-inside-
    // crate) compares two paths that BOTH go through the same
    // normalisation so the leading-dot strip cancels out
    // there.
    let do_step1_starts_with = root != Path::new(".");

    // Step 1: each declared path is project-relative + no `..`
    // + (when root is concretely-rooted) resolves under the
    // project root.
    for (label, raw) in [
        ("[adapters.<name>.adapter].manifest", adapter_manifest_path),
        ("[adapters.<name>.adapter].crate",    adapter_crate_path),
    ] {
        let Some(raw) = raw else { continue };
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            return Err(format!(
                "{label} must be a project-relative path; got absolute `{raw}`",
            ));
        }
        if candidate.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!(
                "{label} must not contain `..` traversal; got `{raw}`",
            ));
        }
        if do_step1_starts_with {
            // Lexical normalisation (no fs::canonicalize: the
            // path may not exist yet on first-run bootstrap, and
            // canonicalize would resolve project_root symlinks
            // operators may rely on).
            let normalized = lexical_normalize(&root.join(candidate));
            if !normalized.starts_with(&root) {
                return Err(format!(
                    "{label} resolves outside project root `{}`: `{}`",
                    root.display(),
                    normalized.display(),
                ));
            }
        }
    }
    // Step 2: when both `.crate` and `.manifest` are set, the
    // manifest path MUST resolve inside the adapter crate dir.
    // Without this, `crate = "crates/cf"` plus
    // `manifest = "tmp/wrangler.toml"` would pass step 1 but
    // write to a path OUTSIDE the adapter crate -- breaking
    // the "never creates files outside the adapter crate"
    // promise this section opens with.
    //
    // `crate = None` means the operator omitted the crate
    // declaration (legal for adapters whose layout doesn't
    // need it, e.g. Axum); we skip this stronger check
    // entirely and fall back to step 1's project-root
    // containment.
    if let (Some(crate_raw), Some(manifest_raw)) =
        (adapter_crate_path, adapter_manifest_path)
    {
        let crate_resolved = lexical_normalize(&root.join(Path::new(crate_raw)));
        let manifest_resolved = lexical_normalize(&root.join(Path::new(manifest_raw)));
        if !manifest_resolved.starts_with(&crate_resolved) {
            return Err(format!(
                "[adapters.<name>.adapter].manifest `{manifest_raw}` must \
                 resolve inside [adapters.<name>.adapter].crate `{crate_raw}`; \
                 resolved manifest path `{}` is not under crate path `{}`",
                manifest_resolved.display(),
                crate_resolved.display(),
            ));
        }
    }
    Ok(())
}
```

Implementing-PR note: callers SHOULD pass the absolute
`ManifestLoader` root when they have it (e.g. via a new
`pub(crate) fn manifest_root_abs() -> &Path` accessor on
`ManifestLoader`), but the helper MUST still normalise
defensively so a relative root passed in from
`args.manifest.parent()` (`""` → `.`, or
`"examples/app-demo"`) compares correctly against the
normalised join.

The containment guarantee is scoped to **paths the manifest
declares** -- `[adapters.<name>.adapter].manifest` and
`[adapters.<name>.adapter].crate`. The helper rejects bad
paths BEFORE dispatch, so the adapter never sees them. The
dry-run tempdir staging step (above) gives an additional
layer of defence by rerooting those manifest-declared paths
under the staging copy, so an adapter that joins
`manifest_root` correctly stays inside the tempdir even when
the operator hand-wrote a path the upfront check missed.

**Call sites that MUST invoke the helper.** Every CLI entry
point that resolves `[adapters.<name>.adapter].manifest` or
`[adapters.<name>.adapter].crate` and lets the adapter write
local files through it is wired up in the same PR:

1. `run_provision` / `run_provision_typed` -- top of the
   function, before any other manifest-path use.
2. `run_config_push_typed` -- the local writeback path at
   `crates/edgezero-cli/src/config.rs:841` resolves
   `manifest_path` and passes it into adapters
   (Fastly's `[local_server.config_stores.*]` writer,
   Axum's JSON-map writer, Spin's SQLite writer all
   ultimately resolve manifest-relative paths). Push
   only writes local files when `--local` is set, so
   the helper call sits inside the `args.local` arm.
3. (Existing remote-only entry points -- cloud
   provision shell-outs, `run_config_diff` read-only --
   stay unchecked. They don't write through
   manifest-declared paths.)

Push and provision import the shared helper from
`path_safety.rs` so a future check addition lands in both
call sites atomically. Without this, an operator could
push-bypass the check (`config push --local --adapter <x>`
with `manifest = "/etc/wrangler.toml"`) even though the
identical provision invocation would have rejected the
path -- the bug-shaped asymmetry the previous review
caught.

The spec does NOT promise a filesystem sandbox. Adapter
code that ignores `manifest_root` and calls
`fs::write("/tmp/escape.txt", ...)` directly is not
something tempdir staging or any post-run walk can prevent
or undo -- the write has already landed by the time the
walk could see it. Defending against that would require an
OS-level sandbox (seccomp / Landlock / similar), which is
out of scope for v1. The adapter trait remains a trust
boundary: in-tree adapters are reviewed in this repo, and
third-party adapters that violate the path-honouring
contract are an operator-level concern.

**Required tests** (in `crates/edgezero-cli/src/provision.rs`'s
`#[cfg(test)] mod tests`):

1. `provision_local_rejects_parent_traversal_in_adapter_manifest`
   -- fixture sets `manifest = "../outside/spin.toml"`. Assert
   `provision --local` returns an error containing
   `"must not contain `..` traversal"` AND assert the project
   parent dir (`<project_root>/../outside/`) is unchanged
   afterwards (mtime check on a sentinel).
2. `provision_local_rejects_absolute_adapter_manifest` -- fixture
   sets `manifest = "/tmp/some.toml"`. Assert error contains
   `"must be a project-relative path"` AND `/tmp/some.toml` is
   absent afterwards (or unchanged if a pre-created sentinel
   exists).
3. `provision_local_rejects_parent_traversal_in_adapter_crate`
   -- same shape against `[adapters.<name>.adapter].crate`.
4. `provision_local_dry_run_adapter_writes_stay_under_tempdir`
   -- run `provision --local --dry-run` against the real
   in-tree app-demo fixture (every adapter exercised), then
   walk the tempdir's allow-list of provision-owned outputs
   (per the dry-run diff scope above) and assert every
   would-write path the driver recorded resolves under the
   tempdir root. The test exercises the happy-path adapters
   honouring `manifest_root`; it does NOT attempt to defend
   against a malicious adapter writing outside it.
5. `config_push_local_rejects_parent_traversal_in_adapter_manifest`
   -- mirror of test 1, but invokes `config push --local`
   instead of `provision --local`. Asserts the SAME error
   string (so the shared helper is provably the source).
   Locks the push call site against future regression.
6. `config_push_local_rejects_absolute_adapter_manifest`
   -- mirror of test 2 against `config push --local`.
7. `provision_local_accepts_relative_manifest_root_default`
   -- run `provision --local` with `--manifest edgezero.toml`
   (cwd-relative; `args.manifest.parent()` returns `""` →
   `Path::new(".")`). Assert success against a valid
   `[adapters.<name>.adapter].manifest`. Locks the relative-
   root normalisation; without it, the helper would silently
   reject every valid path because joined `./crates/<name>/foo.toml`
   does not `starts_with("")`.
8. `provision_local_accepts_relative_manifest_root_nested`
   -- run `provision --local` with
   `--manifest examples/app-demo/edgezero.toml` (multi-segment
   relative root). Assert success against a valid adapter
   manifest. Covers the second common relative-root shape.

`lexical_normalize` is a small internal helper (no new dep) that
resolves `.` and validates against `..` (since the upfront check
already rejected `..`, normalisation is purely
`.`-component-stripping plus `Component::CurDir` handling).

### CLI / trait surface

`--local` requires three coordinated additions:

1. **`ProvisionArgs.local: bool`** in `crates/edgezero-cli/src/args.rs`
   (clap `#[arg(long)]`). Default `false` (cloud mode unchanged).
2. **`pub enum ProvisionMode { Cloud, Local }`** in
   `crates/edgezero-adapter/src/registry.rs`, threaded into
   `Adapter::provision(...)` as a new parameter (cleaner than a
   parallel `provision_local` trait method: keeps one
   provisioning entry per adapter, with the per-mode branch
   inside). Existing impls match `ProvisionMode::Cloud` for the
   current behaviour; the local arm is new.
3. **First-run bootstrap re-order in `run_provision`**: today
   the orchestrator at `crates/edgezero-cli/src/provision.rs:83`
   validates the adapter manifest BEFORE calling
   `adapter.provision`. In local mode, the manifest may not
   exist on a clean clone -- validation would fail before
   regeneration. The fix: when `args.local && !manifest_exists`,
   the CLI synthesises a MINIMAL valid manifest from primitives
   FIRST (`edgezero.toml`'s `[app].name` + the adapter's
   declared `[stores.*]` ids), then re-runs validation, then
   dispatches to `adapter.provision` with
   `ProvisionMode::Local` which fills in the per-store
   bindings.

   **`--dry-run` interaction (MUST)**: `--dry-run` MUST NOT
   modify the project worktree. The existing validation and
   adapter-provision APIs are path-based:
   `validate_adapter_manifest` at
   `crates/edgezero-adapter/src/registry.rs:451` takes a
   manifest path, Spin reads `spin.toml` from disk at
   `crates/edgezero-adapter-spin/src/cli.rs:473`, and Spin
   provision resolves the component from a file path at
   `crates/edgezero-adapter-spin/src/cli.rs:207`.
   Rather than refactor every adapter to accept in-memory
   documents, dry-run uses a **scratch tempdir staging
   area**:

   1. Create a `tempfile::TempDir` (in the OS temp root,
      never inside the project worktree).
   2. Materialise inputs the path-based APIs need, with a
      hard split between read-only and mutable paths:
      - **Read-only** (the adapter loaders only read these
        on the dry-run path): the project's
        `edgezero.toml` may be symlinked, since no
        provision code path writes to it during adapter
        dispatch (CLI-layer writeback to
        `[adapters.<name>.deployed]` is short-circuited
        in dry-run -- next paragraph).
      - **Mutable** (anything the adapter's `provision` /
        `provision_typed` may write to): MUST be a real
        copy, never a symlink. This covers BOTH of:
        (a) the adapter crate directory the loader walks
        (Spin's `crates/<adapter-crate>` ancestor lookup
        at `crates/edgezero-adapter-spin/src/cli.rs:207`,
        Cloudflare's `wrangler.toml` write at
        `crates/edgezero-adapter-cloudflare/src/cli.rs:832`,
        Fastly's `fastly.toml` writes); AND
        (b) the project-root `.edgezero/` directory --
        Axum writes `.edgezero/.env` here, which lives
        OUTSIDE any adapter crate. If `.edgezero/`
        exists in the project root, the staging copy
        mirrors it; if absent, the driver creates an
        empty `.edgezero/` in the tempdir so the
        adapter's create-if-missing path lands there
        instead of in the worktree.

        The CLI uses a small internal recursive copy
        helper built on `std::fs::read_dir` +
        `std::fs::copy` (no new workspace dependency --
        see the "No new workspace dependencies"
        subsection below) so an adapter that opens any
        path under the copied roots for write hits the
        staging copy, never the real worktree. The
        helper preserves regular files and re-creates
        directories; symbolic links and special files
        inside the staged dirs are out of scope (none
        exist in the in-tree fixture or scaffold output
        today).

      Implementation note: the dry-run driver does NOT
      attempt to predict which exact files an adapter
      will write -- the safe default is "every path
      under the copied adapter crate dir AND under
      `.edgezero/` is mutable". Read-only inputs outside
      those dirs (currently just `edgezero.toml`) get
      the symlink optimisation.

      For first-run bootstrap, ALSO write the synthesised
      baseline manifest into the copied tempdir at the
      adapter's expected relative path BEFORE adapter
      dispatch.
   3. Re-point the `manifest_root` argument that flows
      into `adapter.provision` (and `provision_typed`) at
      the tempdir. Dispatch the adapter call with
      **`dry_run = false`** -- the dry-run driver
      deliberately lies to the adapter about dry-run
      state so that adapters take their real-write
      branches (current dry-run branches at
      `crates/edgezero-adapter-cloudflare/src/cli.rs:263`
      and `crates/edgezero-adapter-spin/src/cli.rs:223`
      early-return without mutating files, which would
      leave the tempdir empty of the very content
      operators expect to preview). The tempdir is the
      safety net: every adapter write hits the copied
      staging tree, which never escapes back to the
      worktree (per the read-only/mutable split above).
      Each adapter runs end-to-end against the tempdir
      copy, emitting status lines that name the
      tempdir-relative paths the writes hit.
   4. After the run, rewrite status lines and diff:
      - Status lines: the driver rewrites every tempdir
        path back to its project-relative equivalent
        AND prefixes the verb with "would " (e.g. the
        adapter's `"wrote crates/<adapter>/fastly.toml"`
        becomes `"would write crates/<adapter>/fastly.toml"`).
        Operators see project paths in dry-run language,
        never tempdir paths.
      - File diff: for every provision-owned output the
        tempdir copy now contains -- NOT just the adapter
        manifest -- diff the tempdir version against the
        project version (using the same
        `similar::TextDiff` machinery `config diff` uses)
        and include the diff in the dry-run report. The
        complete set of paths the driver walks: per-adapter
        manifests (`wrangler.toml`, `fastly.toml`,
        `spin.toml`, `runtime-config.toml`), per-adapter
        env / vars files (`.edgezero/.env`, `.dev.vars`,
        Spin-side `.env`), and any other file the relevant
        adapter row in "Per-adapter local state" enumerates.
        The driver enumerates files via a stable allow-list
        derived from the per-adapter local-state tables,
        NOT a generic tempdir walk -- this keeps the diff
        scope tied to the spec and prevents surprise
        diffs of accidentally-touched paths from masking
        bugs. Files the adapter created from scratch in
        the tempdir (no project counterpart) appear in
        the diff as a full-file addition; files the
        operator has but provision didn't touch are
        omitted (zero diff).
   5. Drop the tempdir. Project worktree is byte-identical
      before and after.

   **Mode × dry-run dispatch matrix (MUST).** The
   `dry_run` value passed to the adapter trait call is
   scoped strictly to `--local` mode. Cloud dry-run must
   keep the existing pass-through behaviour or the
   Cloudflare / Fastly cloud CLIs will shell out for
   real.

   | `--local`? | `--dry-run`? | Tempdir staging? | `dry_run` passed to adapter | Why                                                                                                                                                                |
   | ---------- | ------------ | ---------------- | --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
   | No         | No           | No               | `false`                     | Today's default cloud-mode path.                                                                                                                                   |
   | No         | Yes          | No               | **`true`**                  | Cloud dry-run goes straight through to each adapter's existing dry-run branch (Cloudflare `cli.rs:263`, Fastly's `setup` block, Spin's lookup-only flow). NO shell-outs. |
   | Yes        | No           | No               | `false`                     | Local real-write path; adapters mutate the real worktree directly.                                                                                                 |
   | Yes        | Yes          | **Yes**          | **`false`**                 | The tempdir IS the safety net. Adapters take their real-write branch against the copied tree; the driver rewrites status + diffs the result.                       |

   The first three rows match current behaviour exactly;
   only the bottom row is new. The tempdir-and-lie
   pattern is the ONLY case where `args.dry_run = true`
   reaches the adapter as `dry_run = false`. The driver
   computes the dispatch value as a single function of
   `(args.local, args.dry_run)`, NOT by transforming the
   operator's flag along the way.

   `[adapters.<name>.deployed]` writeback in dry-run mode
   short-circuits at the CLI layer (no tempdir copy
   needed): the merger builds the would-be
   `toml_edit::DocumentMut` and prints the diff without
   writing.

   This contract is enforced by a dedicated test in
   `crates/edgezero-cli/src/provision.rs` that runs
   `provision --local --dry-run` against a clean
   fixture directory, asserts the process exit is 0 and
   the report names every would-write, then asserts
   `git status` (or equivalent worktree-state check) is
   clean. A second test pre-creates a sentinel file
   inside each adapter crate directory under the fixture,
   runs `provision --local --dry-run`, and asserts the
   sentinel's mtime is unchanged -- catches accidental
   regressions where an adapter write follows a symlink
   back into the project tree.

   The bootstrap synthesiser does NOT re-render the
   per-adapter manifest from `.hbs` templates -- those
   carry generator-only placeholders (`{{proj_spin}}`,
   `{{target_dir_spin}}`, `{{proj_spin_underscored}}`)
   that aren't reconstructible from `edgezero.toml` alone.
   Instead the bootstrap builds the manifest from
   primitives via `toml_edit::DocumentMut`, writing only
   the structural keys the adapter's `provision` step is
   about to populate (e.g. for Spin: `spin_manifest_version = 2`,
   `[application].name = "<from edgezero.toml [app].name>"`,
   a `[[trigger.http]]` block referencing the resolved
   component id, and a single `[component.<component_id>]`
   block where `<component_id>` is
   `[adapters.spin.adapter].component` from `edgezero.toml`
   when set, falling back to `[app].name` -- per "Primitive
   synthesiser output" → "Spin (`spin.toml`)" below).
   Operator-authored extensions (custom `[scripts]`, deploy
   commands, alternate component shapes) stay the
   operator's job: they hand-edit the generated minimal
   file, and the merge mechanics preserve those edits on
   re-run. The bootstrap is intentionally minimal so
   re-runs don't churn it. See "Shareable vs. local-only
   customizations (v1)" below for the cross-team sharing
   contract.

   The scaffold `.hbs` templates remain the source-of-truth
   for `edgezero new`'s richer first-time generation; only
   `provision --local`'s bootstrap path uses the primitive
   synthesiser.

### Bundled binary vs. generated CLI

`provision --local` runs in two flavours:

| Entry point                                    | What it can write                                                                                                                                          |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Bundled `edgezero provision --adapter <name> --local` | Manifest-and-store-binding work only: synthesise the per-adapter manifest from `edgezero.toml` primitives via `toml_edit::DocumentMut` if absent (see "Primitive synthesiser output"), merge `[stores.*]` from `edgezero.toml` into per-store binding blocks, seed the `edgezero_runtime_env` Fastly Config Store. No `#[secret]` field walking -- the bundled binary has no typed `C`. Output names the missing secret-placeholder step as a follow-up for the operator to run from their `<app-cli>`. |
| Generated `<app-cli> provision --adapter <name> --local` | Everything the bundled binary does, plus adapter-specific per-secret placeholders sourced from `C::SECRET_FIELDS` (filtered to skip `SecretKind::StoreRef`). The destination varies per adapter and is NOT uniformly an env file -- see "Per-adapter `provision_typed` output" below for the per-adapter contract. |

The split mirrors the spec 3.2.1 stub-pointer model for
`config push` / `config diff`: anything that requires typed
`C` is the downstream CLI's job, anything that only needs the
manifest is the bundled binary's job. This requires two
coordinated changes in `edgezero-cli`:

1. **Add `run_provision_typed<C>` to the public surface.**
   Today `crates/edgezero-cli/src/lib.rs:55` exports
   `run_provision` only. The implementing PR adds a sibling
   `pub fn run_provision_typed<C: DeserializeOwned + Validate + AppConfigMeta>(args: &ProvisionArgs) -> Result<(), String>`
   (signature matches the existing `run_config_push_typed`
   re-export pattern at the same site) so generated CLIs
   can call it from their `main.rs`.
2. **Update the scaffold template.** The current call at
   `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs:98`
   is `Cmd::Provision(args) => edgezero_cli::run_provision(&args),`.
   The implementing PR rewrites that single line to
   `Cmd::Provision(args) => edgezero_cli::run_provision_typed::<{{NameUpperCamel}}Config>(&args),`.
   This exactly mirrors the existing push / validate sites
   in the same template
   (`crates/edgezero-cli/src/templates/cli/src/main.rs.hbs:91`
   and `:94`), so the Handlebars context and `&args`
   reference convention are already in scope. The bundled
   `edgezero` binary's own `main.rs` keeps calling
   `run_provision`.

This template change affects ONLY the generated CLI's
`main.rs`; per-adapter manifests (`wrangler.toml`,
`fastly.toml`, `spin.toml`, `runtime-config.toml`) are
NOT touched by `.hbs` rendering on this path -- the
primitive synthesiser owns those (see above).

Implementation contract:

- `run_provision(args)` -- existing path. Calls the adapter's
  `provision(stores, ...)` with stores derived from
  `edgezero.toml` only. Writes manifest scaffold + store
  bindings + `edgezero_runtime_env`. No secrets.
- `run_provision_typed<C: DeserializeOwned + Validate + AppConfigMeta>(args)`
  -- new path. The bounds match the existing typed-push entry
  `run_config_push_typed<C: DeserializeOwned + Validate + AppConfigMeta>`
  at `crates/edgezero-cli/src/config.rs:194` so the generated
  CLI dispatches both via the same `<NameUpperCamel>Config`
  parameter.

  **Cloud-mode short-circuit (MUST)**: `run_provision_typed`
  inspects `args.local` FIRST. If `!args.local`, it
  delegates straight to `run_provision(args)` and returns
  -- no `<app>.toml` lookup, no `deserialize_app_config_with_options`
  call, no `provision_typed` dispatch. This guarantees the
  cloud path cannot fail (or, worse, half-succeed after
  remote side effects) on a missing or malformed
  `<app>.toml`. Typed validation in cloud mode is
  intentionally out of scope for v1 -- adding it would
  require moving the validation in front of the cloud
  side-effecting code in `run_provision`, which is a
  separate change.

  The rest of this bullet describes ONLY the
  `args.local == true` path.

  **Args resolution**: `ProvisionArgs` gains only `--local`
  (no `--app-config` flag). The current helper at
  `crates/edgezero-cli/src/config.rs:1164` is
  `resolve_app_config_path(args: &ConfigValidateArgs, manifest_path: &Path, app_name: &str) -> PathBuf`
  -- coupled to `ConfigValidateArgs` for the `--app-config`
  override. The implementing PR refactors it into a
  config-args-agnostic helper:

  ```rust
  // crates/edgezero-cli/src/config.rs (moved to a shared
  // module the validate/push/provision flows all import)
  pub(crate) fn resolve_app_config_path(
      explicit: Option<&Path>,
      manifest_path: &Path,
      app_name: &str,
  ) -> PathBuf
  ```

  Push and validate keep their existing behaviour by
  passing `args.app_config.as_deref()`; provision passes
  `None`. The app name comes from `[app].name` in
  `edgezero.toml` (already resolved at this point in the
  call chain via the manifest loader the typed flows use,
  e.g. `crates/edgezero-cli/src/config.rs:1131`), and the
  helper locates `<app_name>.toml` next to the manifest.

  **v1 limitation -- non-conventional `<app>.toml`
  locations are unsupported for `provision --local`.**
  No `--app-config` override is exposed on `provision`
  in v1, and there is no way to thread a non-conventional
  path through the typed-secret walk. If the operator
  doesn't have `<app_name>.toml` adjacent to
  `edgezero.toml`, typed provision fails with a clear
  error pointing at the expected path. The follow-up
  to add `--app-config` to provision is small (passes
  through to the shared helper) but is out of scope
  for v1 because no production project hits this case
  today and the workaround is "symlink the file into
  the conventional location for the duration of the
  provision call."

  **Validation**: provision follows the SAME typed-load
  sequence the push path uses (`config.rs:282-285`
  reference) -- shape + non-secret rules + the secret
  preflight + adapter-specific typed checks. The push
  path calls TWO helpers in sequence, NOT one:
  `typed_secret_checks(&typed, &ctx.validation)` at
  `crates/edgezero-cli/src/config.rs:284` covers
  empty-key-name rejection, declared-store-id presence,
  store-ref field shape, and within-secrets uniqueness;
  `run_adapter_typed_checks::<C>(&ctx.validation)` at
  `crates/edgezero-cli/src/config.rs:285` then walks the
  adapter chain and calls each adapter's
  `validate_typed_secrets` (Spin's variable-name +
  collision rules at
  `crates/edgezero-adapter-spin/src/cli.rs:514` are the
  reference implementation -- the runtime would silently
  open the wrong store / fail at first request if
  provision skipped this).

  Provision MUST call both checks via ONE shared helper.
  Today's private surface
  (`crates/edgezero-cli/src/config.rs:77`'s
  `ValidationContext` is private with no `new`,
  `typed_secret_checks` at `:1295` and
  `run_adapter_typed_checks` at `:1339` are both
  private, and both inner checks read
  `ValidationContext.raw_config: toml::Value` at
  `config.rs:95` -- the same raw root table the push
  path loads at `config.rs:274` with
  `env_overlay = !args.no_env`) means `provision.rs`
  cannot reach the helpers directly without duplicating
  internals or silently reloading the raw config with the
  wrong overlay flag.

  The implementing PR MUST introduce a single concrete
  shared entry point. An earlier rev of this spec
  proposed a standalone `TypedPreflightInputs<'_, C>`
  struct that the helper would unpack into a fresh
  `ValidationContext`, but that design depends on
  `Manifest::clone()` which is not derived on
  `edgezero_core::manifest::Manifest` (verified
  `crates/edgezero-core/src/manifest.rs:86`) -- and
  reconstructing `ManifestLoader` from a borrowed
  `Manifest` is not zero-copy either. The compilable
  design promotes `ValidationContext` (the type only;
  fields stay private) to `pub(crate)` and the helper
  takes it by shared reference:

  ```rust
  // crates/edgezero-cli/src/config.rs
  pub(crate) fn run_typed_preflight<C>(
      typed: &C,
      ctx: &ValidationContext,
  ) -> Result<(), String>
  where
      C: AppConfigMeta,
  {
      typed_secret_checks(typed, ctx)?;
      run_adapter_typed_checks::<C>(ctx)?;
      Ok(())
  }
  ```

  Provision constructs its own `ValidationContext` via a
  new `pub(crate) fn load_validation_context_with_options(
  manifest_path, app_config_override, strict, env_overlay)`
  helper (a refactor of the existing
  `load_validation_context(args: &ConfigValidateArgs)`
  at `config.rs:1128` -- the args-shaped wrapper stays,
  delegating to the new primitive-args sibling). Push,
  validate, and diff continue to use the existing
  args-shaped wrapper; provision passes
  `env_overlay = false` directly.

  Push's overlay setting (`env_overlay = !args.no_env`,
  per `config.rs:274`) and provision's overlay setting
  (`env_overlay = false` -- provision must see
  operator-typed values, not env-overlay redirects) are
  preserved by each caller picking the right helper
  arm; no implicit overlay decision in
  `run_typed_preflight` itself.

  The implementing PR rewrites **all four** existing
  caller sites in the same commit so the helper has a
  single source of truth from day one:
  - validate at `crates/edgezero-cli/src/config.rs:216-217`
  - push at `crates/edgezero-cli/src/config.rs:284-285`
  - diff at `crates/edgezero-cli/src/config.rs:415-416`
  - provision (the new call site this spec adds)

  Each call site is rewritten to call
  `run_typed_preflight(&typed, &ctx)` against the
  surrounding `ValidationContext` (or `ctx.validation`
  for push). The two-line `typed_secret_checks` +
  `run_adapter_typed_checks` pair at each old site is
  deleted. `typed_secret_checks` and
  `run_adapter_typed_checks` stay PRIVATE; only
  `ValidationContext` (type), `run_typed_preflight`,
  `load_validation_context_with_options`, and the
  `build_typed_secret_entries` helper extracted to let
  provision build the `TypedSecretEntry` slice without
  duplicating the secret walk become `pub(crate)`.
  `pub(crate)` keeps every name crate-internal -- no
  external API widening. Routing every caller through one
  helper prevents the typed-validation drift the previous
  reviewer round caught on the diff site, where a future
  preflight rule added to push alone would silently skip
  diff.

  Skipping the adapter half of the preflight is the bug
  the reviewer caught earlier -- it would leave Spin's
  variable-name validation out of the local provision
  path. Routing every caller through one helper makes
  the bug structurally unreachable.

  ```rust
  let mut opts = AppConfigLoadOptions::default();
  opts.env_overlay = false;  // capture operator-typed values
  // 1. Deserialize into the typed C and validate everything
  //    except secret fields. Fails loud if the shape doesn't
  //    match or a non-secret validator rejects -- prevents
  //    provision silently skipping placeholders when the
  //    file is malformed.
  let cfg: C = app_config::deserialize_app_config_with_options::<C>(
      &app_config_path, &app_name, &opts,
  )?;
  app_config::validate_excluding_secrets(&cfg)
      .map_err(|e| format!("app config validation failed: {e}"))?;
  // 2. Re-load as raw TOML so we can look up each secret
  //    field's runtime VALUE (the key NAME the operator
  //    typed). Generic C has no runtime field reflection, so
  //    raw-TOML lookup is the only way -- existing
  //    typed_secret_checks at config.rs:1339 takes the same
  //    approach.
  let raw_table = app_config::load_app_config_raw_with_options(
      &app_config_path, &app_name, &opts,
  )?;
  // 3. Run the shared typed preflight. This is the
  //    pub(crate) helper introduced in this PR; the push
  //    path at config.rs:282-285 is rewritten in the
  //    same PR to call it. The helper takes the
  //    already-loaded `raw_table` (no implicit reload,
  //    no implicit overlay mode); provision's
  //    `env_overlay = false` choice flows through via
  //    the `raw_table` we just loaded above with that
  //    setting. Internally runs typed_secret_checks +
  //    run_adapter_typed_checks (which stay private).
  //    Routing through one entry point makes "forgot
  //    the adapter half" structurally unreachable.
  let ctx = config::load_validation_context_with_options(
      &manifest_path,
      None,        // no --app-config override on provision in v1
      false,       // strict = false
      false,       // env_overlay = false: capture operator-typed values
  )?;
  config::run_typed_preflight(&cfg, &ctx)?;
  ```

  `AppConfigLoadOptions` is `#[non_exhaustive]`, so the
  positional `..Default::default()` struct-update form does
  NOT compile outside `edgezero-core`. Always construct via
  `default()` + field assignment as above.

  Provision performs ALL THREE steps before assembling its
  `TypedSecretEntry` slice and dispatching to
  `provision_typed`. This guarantees the secret-store
  writes provision emits (Fastly `[[local_server.secret_stores.*]]`,
  Spin `[variables]` declarations, `.dev.vars` /
  `.edgezero/.env` placeholders) never carry a key NAME
  the runtime would later reject -- the operator gets the
  error at provision time, not at first request.
- Walks `C::SECRET_FIELDS` (the COMPILE-TIME metadata is
  available; it's just the runtime field VALUES that aren't)
  and SKIPS entries whose `kind == SecretKind::StoreRef`:
  those fields hold a store-id selector, not a secret-store
  key (the existing `typed_secret_checks` path at
  `crates/edgezero-cli/src/config.rs:1308` is the reference
  for this filter).
- For each non-StoreRef entry, looks up the field's VALUE in
  `raw_table[field.name]` -- this is the key NAME the
  operator typed, e.g. `api_token = "demo_api_token"`
  yields `key_value = "demo_api_token"`. Builds a
  `TypedSecretEntry<'_>` (Phase B's existing struct
  carrying `store_id`, `field_name`, `key_value`). The
  `store_id` is resolved per `SecretKind`:
  - `KeyInDefault` → the default `[stores.secrets].ids` entry.
  - `KeyInNamedStore { store_ref_field }` → the VALUE of the
    sibling raw-TOML key named `store_ref_field`, read as
    `raw_table[store_ref_field].as_str()`. Both the
    sibling-field name (`store_ref_field`) and its lookup
    against the raw TOML mirror exactly how
    `typed_secret_checks` resolves the same field today.
- Hands the resulting secret list to a new adapter trait
  method `provision_typed`. The slice element is
  `TypedSecretEntry<'_>` -- the existing
  `#[non_exhaustive]` struct introduced for typed
  validation in Phase B at
  `crates/edgezero-adapter/src/registry.rs:178` (already
  carries `store_id`, `field_name`, `key_value`). It
  already lives in `edgezero-adapter`, so no new neutral
  type is needed and the dep-free boundary is preserved
  by construction. The CLI builds the slice from the
  `raw_table` walk above using the existing inherent
  constructor `TypedSecretEntry::new(...)`.

  The exact trait signature deliberately OMITS the
  `stores: &ProvisionStores<'_>` parameter that
  `Adapter::provision` at
  `crates/edgezero-adapter/src/registry.rs:282` carries.
  This is intentional: base `provision` owns ALL store-
  binding emission (`[[kv_namespaces]]`,
  `[local_server.*_stores.*]`, `[component.*.key_value_stores]`,
  etc., per the per-store propagation table above), and
  the orchestrator guarantees `provision_typed` runs
  AFTER `provision` on the same `manifest_root`. By the
  time `provision_typed` sees the tree, every store
  binding the typed-secret writes need to coexist with
  is already present, so passing `stores` again would
  invite split-brain implementations where typed
  provision re-derives the same bindings from
  `edgezero.toml`. The signature mirrors the SUBSET of
  `Adapter::provision`'s parameters typed provision
  actually needs, plus the secret slice and mode:

  ```rust
  // crates/edgezero-adapter/src/registry.rs
  fn provision_typed<'entry>(
      &self,
      manifest_root: &Path,
      adapter_manifest_path: Option<&str>,
      component_selector: Option<&str>,
      typed_secrets: &[TypedSecretEntry<'entry>],
      mode: ProvisionMode,
      dry_run: bool,
  ) -> Result<ProvisionOutcome, String> {
      // Default: no-op (returns empty status + None deployed).
      // Each adapter overrides for the local mode; cloud
      // mode is currently a no-op (cloud secret-store
      // creation is operator scope per spec 3.3).
      Ok(ProvisionOutcome::default())
  }
  ```

  The default impl keeps existing adapters compilable while
  the per-adapter overrides land. `provision_typed` writes
  adapter-specific outputs (per-adapter table below); the
  destination is NOT uniformly "the env file" -- Fastly
  writes secret-store array entries in `fastly.toml`, Spin
  writes both `[variables]` declarations and `.env`
  placeholders.

Both paths support `--local` and `--dry-run`. The two-step
sequence (`run_provision` then `provision_typed`) is one
top-level invocation. If `provision_typed` fails after
`run_provision` succeeded, the partially-written manifest
and `[adapters.<name>.deployed]` block stay on disk;
re-running `provision --local` is idempotent (merge
mechanics above) and resumes from the partial state. The
spec does NOT promise transactional rollback -- adding it
would require a new atomic-write helper and operator-visible
backup files, which is follow-up scope.

**Shared staging for typed dry-run (MUST).** The
non-dry-run path can run `run_provision` and then
`provision_typed` as two independent calls -- each writes
to the real worktree, and the second call sees the
results of the first via the merge mechanics. But on the
**`--local --dry-run`** path, the two calls MUST share a
single tempdir staging tree. If `run_provision_typed` were
to stage twice (one tempdir per call), the second stage
would not see the baseline manifest the first stage
synthesised on a clean clone, and the typed-secret merger
would either fail validation or skip the merge entirely.

The implementing PR factors the staging machinery into a
shared internal helper:

```rust
// crates/edgezero-cli/src/provision.rs
fn run_with_staging<F, R>(
    args: &ProvisionArgs,
    project_root: &Path,
    body: F,
) -> Result<R, String>
where
    F: FnOnce(&Path /* tempdir-relative manifest_root */)
        -> Result<R, String>,
{ /* ... */ }
```

`run_provision_typed` calls `run_with_staging` ONCE and
invokes both `adapter.provision` and `adapter.provision_typed`
inside the closure, against the same tempdir-relative
`manifest_root`. After the closure returns, the driver
emits one combined diff and one combined status block
(base-provision lines + typed-provision lines, in that
order, both rewritten to "would write" language).
Dry-run rollback is automatic via `TempDir` drop on scope
exit, regardless of whether either inner call errored.

#### Per-adapter `provision_typed` output

For each `TypedSecretEntry { store_id, field_name, key_value }`
that comes out of the typed walk:

| Adapter    | Where the per-secret placeholder lands                                                                                                                                                                                                                                                                                                                                                                                                          |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Axum       | `.edgezero/.env` line: `<key_value>=` (empty placeholder; operator fills). One line per entry.                                                                                                                                                                                                                                                                                                                                                  |
| Cloudflare | `.dev.vars` line: `<key_value>=""` (quoted empty placeholder, wrangler convention).                                                                                                                                                                                                                                                                                                                                                             |
| Fastly     | `fastly.toml` `[[local_server.secret_stores.<store_id>]]` array entry: `key = "<key_value>"`, `env = "<KEY_VALUE_UPPER>"`. The array NAME is the entry's `store_id` (NOT hardcoded `default`) so `#[secret(store_ref = "field")]` routes to the correct `[stores.secrets]` id. Append IFF the array entry with the same `key` is absent.                                                                                                       |
| Spin       | Two writes per entry, both using `spin_var = key_value.to_ascii_lowercase()` (canonicalised exactly as `crates/edgezero-adapter-spin/src/cli.rs:535` does, matching the runtime lookup at `crates/edgezero-adapter-spin/src/secret_store.rs:51`). (1) `spin.toml` -- `[variables].<spin_var> = { default = "", secret = true }` plus `[component.<component_id>.variables].<spin_var> = "{{ <spin_var> }}"`. (2) `.env` next to `spin.toml` -- a `SPIN_VARIABLE_<spin_var.to_ascii_uppercase()>=` line for the operator to populate. Both writes are merge-only (append IFF absent). Spin's flat variable namespace means `store_id` is informational only -- there's no per-store routing. Mixed-case operator-typed key values (`api_token` vs `API_TOKEN` vs `Api_Token`) all canonicalise to the same `api_token` spin var; the lowercased-collision check that catches this lives in Spin's `validate_typed_secrets` at `crates/edgezero-adapter-spin/src/cli.rs:514`, which provision reaches via the `run_adapter_typed_checks` call documented in the "Validation" section above (NOT via `typed_secret_checks`, which only covers cross-adapter-agnostic rules). Both helpers must run; provision's two-helper preflight pattern (also documented in "Validation") catches the collision before the merge step. |

Existing values are preserved (the merge mechanics above
apply): an operator who pre-filled a placeholder with a real
value sees that value survive a re-run.

Exit codes match the existing `provision`: 0 on success, non-zero
on any write/parse error. Output is one human-readable line per
file touched (or "would touch" under `--dry-run`).

### Merge mechanics

Every file `provision --local` writes can already exist when the
command runs (re-provision, operator edits since last run). The
spec contract is **preserve operator-set values; only add what's
missing**:

| File shape                                          | Merge mechanism                                                                                                                                                                                                                                  |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| TOML (`fastly.toml`, `spin.toml`, `wrangler.toml`, `runtime-config.toml`) | `toml_edit::DocumentMut` round-trip. New tables / array-of-tables append only when absent (`setup_block_present`-style check). Existing tables keep their child keys verbatim, including operator comments and formatting. |
| Line-oriented (`.env`, `.dev.vars`)               | Line-wise dedup, key-normalised. Parse each line by stripping at most one leading `#` and optional adjacent whitespace, then parsing `<key>=<value>`; the resulting `<key>` is the dedup key. Provision skips appending its line when either an uncommented OR a commented form of the same `<key>` is already present. Concretely: if the file already contains `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging` (operator uncommented and edited), a second provision run MUST NOT re-add the `# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config` commented placeholder it originally emitted. Existing lines (including comments operator-edited and operator-set values) are preserved byte-for-byte. (`.gitignore` is NOT in this set -- it is generator-owned; see "Scaffold generator".) Contract test: "Re-provision after operator uncomments override does not re-add commented placeholder" -- write provision output, uncomment + edit the override line, re-run provision, assert exactly one line per normalised key. |
| JSON map (`.edgezero/local-config-<id>.json`)       | Not provision's responsibility -- that file is written only by `config push --local`. Provision ensures the parent directory exists.                                                                                                            |

A schema-version header lands at the top of each provision-written
file as a single-line comment, e.g. `# edgezero-provision: v1`.
Future spec revs that change placeholder shape can detect the
header and migrate; v1 ships without explicit migration logic
(the file shape is simple enough that operators can hand-edit if
v2 changes it). The header itself is treated as a normal line
during dedup -- if absent, it gets prepended.

## Per-store binding propagation

A clean `git clone` has no **generated adapter manifest**
(`wrangler.toml`, `fastly.toml`, `spin.toml`,
`runtime-config.toml` -- the four under the synthesiser /
gitignore model; Axum's `axum.toml` stays tracked, see the
Axum subsection under "Primitive synthesiser output").
Provision's CLI-owned bootstrap synthesises a minimal baseline
for each of those four via `toml_edit::DocumentMut` (NOT from
the scaffold `.hbs` templates -- see CLI section). The
synthesised baseline carries enough adapter shape to validate
and boot, but NOT the operator-declared list of `[stores.kv]`
/ `[stores.config]` ids. Those ids only exist in
`edgezero.toml` and the adapter's `provision` step is what
merges per-store bindings into the generated manifest on top
of the bootstrap.

### Logical ID vs platform-resolved name (v1 contract)

`provision --local` preserves the existing logical-id →
platform-name resolution baked into cloud provision today
(`crates/edgezero-cli/src/provision.rs:89` resolves each
logical `[stores.<kind>].ids` entry through the
`EDGEZERO__STORES__<KIND>__<ID>__NAME` env overlay and
writes the **platform name** into the per-adapter manifest).
The two flavours of identifier are:

- **Logical ID** -- the entry the operator typed under
  `[stores.<kind>].ids` in `edgezero.toml`. Stable across
  environments. Used for:
  - operator-facing status lines (so re-runs say what the
    operator declared even when the env overlay redirected
    the create);
  - the key under `[adapters.<name>.deployed]` sub-tables
    (e.g. `kv_namespaces.<logical_id> = "<cloud_id>"`) --
    this is the durable per-id record;
  - typed-secret `store_id` resolution in `TypedSecretEntry`
    (matches today's `typed_secret_checks` filter at
    `crates/edgezero-cli/src/config.rs:1308`).
- **Platform-resolved name** -- the result of the env-overlay
  lookup; falls back to the logical id when no override is
  set. Used for the actual TOML cells the merger writes:
  Cloudflare `[[kv_namespaces]].binding`, Fastly
  `[local_server.kv_stores.<name>]` / `[local_server.config_stores.<name>]`
  table names, Spin `[component.<name>.key_value_stores]`
  array entries and `[key_value_store.<name>]` block names in
  `runtime-config.toml`.

In the propagation table below, every `<id>` in a rendered
TOML cell is the **platform-resolved name** (so the merged
manifest matches what `cloud provision` would write today and
what the runtime resolves via the same env overlay). Every
`<id>` shown as a key under `[adapters.<name>.deployed].*` or
in `TypedSecretEntry.store_id` discussion is the **logical
id**. The current `reject_merged_id_collisions` check at
`provision.rs:106` already catches collisions across logical
ids and across env-resolved platform labels -- that check
runs on the `--local` path unchanged.

`provision --local` reads `edgezero.toml`'s `[stores.kv]`,
`[stores.config]`, and (when typed via `<app-cli>`)
`[stores.secrets]` declarations and MERGES one binding per id
into each adapter's local manifest:

| Adapter    | Binding the merge appends per `stores.<kind>.ids` entry                                                                                                                                                                                                                 |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cloudflare | `[[kv_namespaces]] binding = "<id>", id = "<resolved_id>"` per `stores.kv` AND per `stores.config` id -- Cloudflare's config store IS KV-backed, so config ids also need a `[[kv_namespaces]]` binding. **`<resolved_id>` precedence (this list is the SOLE authority; the general "preserve operator values" rule in Merge mechanics yields to it for the managed keys `id` and `preview_id`):** (1) tracked `[adapters.cloudflare.deployed].kv_namespaces.<logical_id>` in `edgezero.toml` if present -- cloud authority wins, so a teammate's pushed cloud id overrides any stale local hand-edit; (2) otherwise, the `id` value currently on the matched existing local `[[kv_namespaces]]` entry, if any -- preserves the operator's hand-edit when no cloud authority exists; (3) otherwise, the literal placeholder `"<placeholder-namespace-id-{logical_id}>"` so `wrangler dev --local` still boots and operators can spot the unfilled slot. `preview_id` follows the v1 rule under "Cloudflare preview_id (v1)" below. Matches the existing `stores.kv.iter().chain(stores.config.iter())` pattern in `crates/edgezero-adapter-cloudflare/src/cli.rs:207`. **Per-binding merge mechanics (matches existing `id`-only update at `crates/edgezero-adapter-cloudflare/src/cli.rs:821`):** locate the `[[kv_namespaces]]` array entry whose `binding == "<id>"`; if found, UPSERT only the managed keys (`id`, and -- when v1 has a value for it -- `preview_id`) per the precedence above. Sibling operator-authored keys on the same entry (custom `usage_model`, adjacent comments) AND entries with a different `binding` value are untouched. If not found, append a new entry with `binding`, `id`, and (when present) `preview_id` only. Never delete an entry. |
| Fastly     | Per `stores.kv` id, a `[[local_server.kv_stores.<id>]]` ARRAY-of-tables entry with a placeholder `key = "__init__"`, `data = ""` row -- Viceroy rejects empty `[local_server.kv_stores.<id>]` normal-tables (the existing app-demo fixture uses the array-of-tables shape). Per `stores.config` id, a `[local_server.config_stores.<id>]` normal table with `format = "inline-toml"` PLUS an empty `[local_server.config_stores.<id>.contents]` sub-table (NOT `contents = ""` -- the existing Fastly push writer at `crates/edgezero-adapter-fastly/src/cli.rs:986` calls `contents_entry.as_table_mut()` and refuses to edit in place when the value isn't a table). Separate from the `[local_server.config_stores.edgezero_runtime_env]` block that provision also writes -- same shape.                  |
| Spin       | Add `<id>` to the component's `key_value_stores` array in `spin.toml`, AND a `[key_value_store.<id>]` block in `runtime-config.toml` (`type = "spin"`, default `path = ".spin/sqlite_key_value.db"`). `spin.toml` is regenerated when absent (via the primitive-synthesiser, not the scaffold `.hbs`), otherwise the array merge appends only the missing ids. **Typed `[variables]` declarations for `#[secret]` fields are NOT written here** -- they belong in `provision_typed` (the generated `<app-cli>` path), per the table below. The bundled binary has no typed `C` and so cannot derive the secret-variable names. |
| Axum       | No manifest equivalent (Axum's stores are file-backed; ids surface as `local-config-<id>.json` filenames).                                                                                                                                                              |

The merge is additive: bindings whose `binding` value
matches a declared `[stores.<kind>].ids` entry get their
**managed keys** (per the per-adapter row above) refreshed
via the precedence the row defines; sibling keys on the same
entry are preserved. Bindings whose `binding` value does NOT
match any declared store id are not touched at all (operator
authored, provision has no business with them). Bindings for
ids removed from `edgezero.toml` are NOT pruned -- pruning
would race with operator hand-edits and the runtime tolerates
extra bindings. The implementing PR may add a
`provision --local --prune` flag as follow-up.

This supersedes the looser "existing bindings are left alone"
wording previous spec revisions used. The Cloudflare row's
explicit precedence list is the authority for managed-key
values; the general Merge mechanics table still governs
sibling-key and adjacent-comment preservation.

### Cloudflare `preview_id` (v1)

Current cloud `provision --adapter cloudflare` extracts ONLY
the primary namespace id from `wrangler kv namespace create`
output at `crates/edgezero-adapter-cloudflare/src/cli.rs:535`.
It does not call `wrangler kv namespace create --preview` and
does not capture a separate preview id. v1 keeps that
behaviour and the spec MUST NOT promise cloud writeback of
preview ids.

Local provision's `preview_id` emission rule, in priority
order:

1. If `[adapters.cloudflare.deployed].preview_kv_namespaces.<logical_id>`
   is set in `edgezero.toml` (operator hand-filled after
   running `wrangler kv namespace create --preview` out of
   band), use that value. The key is the **logical id** --
   the same logical id used by the primary `kv_namespaces`
   map. A separate `preview_kv_namespaces` map (rather than
   a `<id>_preview` sibling-key convention on
   `kv_namespaces`) avoids any collision risk with a legal
   store id like `sessions_preview` -- store-id validation
   at `crates/edgezero-core/src/manifest.rs:883` permits
   underscores, so the sibling convention would conflict
   with operator naming freedom.
2. Otherwise, OMIT `preview_id` from the merged
   `[[kv_namespaces]]` entry entirely -- do NOT synthesise a
   placeholder. Existing per-binding merge code at
   `crates/edgezero-adapter-cloudflare/src/cli.rs:821` already
   writes only `id`; this matches that contract.
   `wrangler dev --local` does not require `preview_id` to
   boot, so omission is safe for the local-emulator flow.
3. If the operator's existing `wrangler.toml` already has a
   `preview_id = "..."` line on the matched entry, the
   sibling-preservation rule (Cloudflare row in the
   propagation table) keeps it intact.

Operators who need a separate cloud preview namespace today:
run `wrangler kv namespace create <binding> --preview`
manually, then write the returned id into
`[adapters.cloudflare.deployed].preview_kv_namespaces.<logical_id>`
in `edgezero.toml`. Teammates' `provision --local` then picks
it up via rule (1). A v2 follow-up may add `--preview`
capture to cloud `provision --adapter cloudflare` so this
becomes automatic; spelled out in "Out of scope".

### Where durable identifiers live

Some operator-authored values must persist across teammates'
clones because they're tied to remote infrastructure:
Cloudflare KV namespace ids returned by
`wrangler kv namespace create` (during `edgezero provision
--adapter cloudflare`), and Fastly's `service_id` returned by
`fastly compute deploy` (during `edgezero deploy --adapter
fastly`, NOT during `provision`).

These do NOT live in the gitignored adapter manifests
(`wrangler.toml`, `fastly.toml`, `spin.toml`,
`runtime-config.toml`) -- that would defeat the
share-across-clones property. They live
in `edgezero.toml`'s tracked `[adapters.<name>]` block, in new
`[adapters.<name>.deployed]` sub-tables:

```toml
[adapters.cloudflare.deployed]
# Populated by `edgezero provision --adapter cloudflare`
# (cloud mode). Tracked so teammates' `provision --local`
# bootstraps wrangler.toml with the real namespace ids.
# Primary namespace ids:
kv_namespaces.sessions = "abc123def456..."
kv_namespaces.cache = "789ghi012jkl..."
# Optional preview-namespace ids, kept in a SEPARATE map
# so a legal store id like `sessions_preview` can never
# collide with a sibling-suffix convention. v1 leaves
# this map empty unless the operator hand-fills it (see
# "Cloudflare preview_id (v1)").
preview_kv_namespaces.sessions = "abc123def456_preview..."
preview_kv_namespaces.cache = "789ghi012jkl_preview..."

[adapters.fastly.deployed]
service_id = "SU1Z0isxPaozGVKXdv0eY"
```

#### Manifest schema change

Today `ManifestAdapter` at `crates/edgezero-core/src/manifest.rs:357`
captures unknown sub-tables as "legacy" and the validator at
`manifest.rs:780` rejects them. The current manifest types
derive `Deserialize + Validate` (no `Serialize`); writeback
lives in the CLI via `toml_edit::DocumentMut`
(`crates/edgezero-cli/src/provision.rs`). That split stays --
the implementing PR does NOT add `Serialize` derives to
`ManifestAdapter` or related types. The implementing PR:

1. Adds a typed `ManifestAdapterDeployed` struct alongside
   `ManifestAdapter`:

   ```rust
   #[derive(Debug, Default, Deserialize, Validate)]
   #[serde(deny_unknown_fields)]
   pub struct ManifestAdapterDeployed {
       /// Primary namespace ids, keyed by logical
       /// `[stores.kv]` / `[stores.config]` id.
       #[serde(default)]
       pub kv_namespaces: BTreeMap<String, String>,
       /// Preview-namespace ids, keyed by the SAME
       /// logical id. Separate map so a legal store id
       /// like `sessions_preview` cannot collide with a
       /// sibling-suffix convention. Optional; v1 leaves
       /// it empty unless the operator hand-fills it.
       #[serde(default)]
       pub preview_kv_namespaces: BTreeMap<String, String>,
       #[serde(default)]
       pub service_id: Option<String>,
   }
   ```

   `#[serde(deny_unknown_fields)]` catches typos at load
   time. Per-adapter applicability is enforced at the
   Manifest level (next bullet) -- the validator below
   needs the adapter NAME, which the nested struct
   doesn't see, so it cannot live on
   `ManifestAdapterDeployed` itself.

2. Adds a `pub deployed: Option<ManifestAdapterDeployed>`
   field on `ManifestAdapter` and removes `deployed` from
   the "legacy / unknown" reject list. All other unknown
   sub-tables continue to be rejected. Adds a
   Manifest-level schema validator
   `validate_manifest_deployed_adapter_match(manifest: &Manifest)`
   wired into the existing top-level Manifest validator
   slot at `crates/edgezero-core/src/manifest.rs:87`
   (which already hosts
   `validate_manifest_adapter_keys_case_unique`):

   ```rust
   #[derive(Debug, Deserialize, Validate)]
   #[validate(schema(
       function = "validate_manifest_adapter_keys_case_unique"
   ))]
   #[validate(schema(
       function = "validate_manifest_deployed_adapter_match"
   ))]
   pub struct Manifest { /* unchanged fields */ }

   fn validate_manifest_deployed_adapter_match(
       manifest: &Manifest,
   ) -> Result<(), validator::ValidationError> {
       for (name, adapter) in &manifest.adapters {
           let Some(deployed) = adapter.deployed.as_ref()
           else { continue };
           if deployed.service_id.is_some()
               && !name.eq_ignore_ascii_case("fastly")
           {
               return Err(validator::ValidationError::new(
                   "deployed_service_id_only_on_fastly",
               ));
           }
           let cloudflare_only_map_set = !deployed
               .kv_namespaces
               .is_empty()
               || !deployed.preview_kv_namespaces.is_empty();
           if cloudflare_only_map_set
               && !name.eq_ignore_ascii_case("cloudflare")
           {
               return Err(validator::ValidationError::new(
                   "deployed_kv_namespaces_only_on_cloudflare",
               ));
           }
       }
       Ok(())
   }
   ```

   The `eq_ignore_ascii_case` calls match the existing
   adapter-name lookup convention at
   `crates/edgezero-core/src/manifest.rs:132` (operator
   spelling is preserved; identity comparisons are
   case-insensitive). The `adapter_deployed_block_rejects_cross_adapter_keys`
   contract test below covers both `[adapters.Fastly]`
   and `[adapters.FASTLY]` to lock the case-insensitive
   contract.

   These rejections fire at manifest-load time, so a
   typo'd `[adapters.cloudflare.deployed].service_id = "..."`
   surfaces immediately rather than silently no-op'ing
   downstream.

3. Extends the manifest contract tests with two new tests
   in the existing `manifest::tests` module:
   - `adapter_deployed_block_parses_and_validates` --
     parse + Validate for each adapter's happy-path shape.
   - `adapter_deployed_block_rejects_cross_adapter_keys`
     -- asserts `service_id` under Cloudflare and
     `kv_namespaces` under Fastly each fail Validate
     with the expected error code.

   Writeback is tested in
   `crates/edgezero-cli/src/provision.rs`'s tests via a
   `toml_edit::DocumentMut` round-trip
   (parse → mutate `[adapters.<name>.deployed]` keys →
   serialise → re-parse and assert), since the CLI owns
   the writer.

#### Writeback ownership

Today `Adapter::provision(stores, manifest_root, ...)` doesn't
receive the manifest FILE path -- just the directory root --
and returns only `Vec<String>` status lines. Writing
`[adapters.<name>.deployed]` into `edgezero.toml` is therefore
a CLI-owned step, not the adapter's:

1. Extend the adapter trait with a new return type.
   `crates/edgezero-adapter` is deliberately dep-free of
   `edgezero-core` (see the load-bearing comment at
   `crates/edgezero-adapter/src/registry.rs:218`); adding a
   reverse dep to pull in `ManifestAdapterDeployed` would
   break that invariant. Instead, define a NEUTRAL type in
   `edgezero-adapter`:

   ```rust
   // crates/edgezero-adapter/src/registry.rs
   /// Adapter-emitted deployed identifiers. Kept neutral
   /// (just a string-keyed map) so edgezero-adapter stays
   /// dep-free of edgezero-core. The CLI maps this into
   /// the core ManifestAdapterDeployed shape when writing
   /// edgezero.toml.
   #[derive(Debug, Default, Clone)]
   pub struct AdapterDeployedState {
       pub fields: BTreeMap<String, String>,
       pub sub_tables: BTreeMap<String, BTreeMap<String, String>>,
   }
   ```

   `provision(...)` returns
   `Result<ProvisionOutcome, String>` where
   `ProvisionOutcome { status_lines: Vec<String>, deployed: Option<AdapterDeployedState> }`.
   Existing impls return `deployed: None` until they're updated.

   The two-level shape (`fields` + `sub_tables`) covers both
   adapters' needs: Fastly's `service_id` is a top-level
   string; Cloudflare's `kv_namespaces.<id>` is a nested
   map. The CLI maps each adapter's `AdapterDeployedState`
   into the appropriate strongly-typed
   `ManifestAdapterDeployed` variant in `edgezero-core`.
2. `run_provision` (the CLI orchestrator at
   `crates/edgezero-cli/src/provision.rs`) takes the returned
   `AdapterDeployedState`, maps it to the typed
   `ManifestAdapterDeployed`, and merges it into
   `edgezero.toml`'s `[adapters.<name>.deployed]` table via
   `toml_edit::DocumentMut`. Per-key merge: new keys append,
   existing keys are replaced with the latest cloud-returned
   value (cloud authority over local stale state). The merge
   preserves operator-authored comments and adjacent keys.
3. `--dry-run` reports the would-be `edgezero.toml` diff
   without writing.

**Cloud `provision`** (existing path) populates `deployed`
from the cloud CLI's stdout (e.g. `wrangler kv namespace
create`'s returned id). Local provision returns
`deployed: None` -- it never has authoritative remote
identifiers.

**Cloud `deploy`** (separate command, NOT provision) is what
populates Fastly's `service_id`. v1 takes the lower-cost
path here:

- The `Adapter::deploy` trait is NOT changed in v1.
  `run_deploy` at `crates/edgezero-cli/src/lib.rs:160`
  continues to call `adapter::execute(adapter, Action::Deploy,
  ...)` at `crates/edgezero-cli/src/adapter.rs:97`, which
  prefers the manifest-defined `[adapters.<name>.commands].deploy`
  shell command when present (which app-demo and the scaffold
  template both do). The shell command's `fastly compute
  deploy` writes `service_id` into the local `fastly.toml`
  in place -- the CLI never sees it.
- After a successful first deploy, operators run a documented
  one-time manual step: read `service_id` from the local
  `fastly.toml` and write it into
  `edgezero.toml`'s `[adapters.fastly.deployed].service_id`,
  then commit. Teammates' clones pick up the value, and
  their `provision --local` bootstraps `fastly.toml` with
  the real id pinned at the top.
- Local provision reads `[adapters.fastly.deployed].service_id`
  when synthesising or merging `fastly.toml`: if set, the
  emitted file pins it as a top-level
  `service_id = "..."`; if absent, the field is omitted and
  the operator runs deploy first.

A future v2 can refactor `Adapter::deploy` to return a
`DeployOutcome` matching `ProvisionOutcome`'s shape and
route through a dedicated dispatch that doesn't hand the
shell command full control -- but the refactor touches every
adapter's deploy path AND the shell-vs-trait dispatch
priority at `adapter.rs:97`, which is invasive enough to
warrant its own spec. See "Out of scope" for the
`sync-deployed` follow-up that would replace the manual
copy step.

Operators who don't deploy to cloud at all leave
`[adapters.<name>.deployed]` empty; local provision falls back
to deterministic placeholders for all bound stores. For
Fastly, the `service_id` top-level key is OMITTED
entirely from the synthesised `fastly.toml` -- viceroy
does not require it for `viceroy serve`. Operators
running `fastly compute deploy` for the first time
trigger the local CLI's interactive create-or-pick prompt,
which writes the new `service_id` into the local
`fastly.toml`; the documented one-time manual copy step
then lands it in `[adapters.fastly.deployed].service_id`.
The placeholder fall-back covers the rest
(`wrangler dev --local`, `viceroy serve`, `spin up`).

### Shareable vs. local-only customizations (v1)

v1 splits adapter-manifest content into three tiers by how
it's shared across teammates:

| Tier                                                                                                                                                                                                                                                                                              | Source-of-truth (tracked)                                  | Reaches teammates how                                                                                                                                                |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Structural primitives (`<app_name>`, store ids, build script paths)                                                                                                                                                                                                                                | `edgezero.toml`'s `[app]` + `[stores.*]`                  | Re-derived by `provision --local` on every clone.                                                                                                                    |
| Build / deploy / serve commands                                                                                                                                                                                                                                                                   | `edgezero.toml`'s `[adapters.<name>.commands]` (existing) | **v1: NOT re-derived.** The `synthesise_baseline_manifest` trait receives only `manifest_root`, adapter manifest path, component selector, `app_name`, and `deployed` -- the existing `[adapters.<name>.commands]` block doesn't flow through. Each adapter's synthesiser hard-codes its default build script (e.g. Fastly's `cargo build --profile release --target wasm32-wasip1`). Operators with custom commands edit the generated manifest by hand after `provision --local`; re-runs preserve those edits via merge mechanics. Extending the synthesiser to consume `[adapters.<name>.commands]` is an out-of-scope follow-up. |
| Durable deploy-time ids (Cloudflare namespace ids, Fastly `service_id`)                                                                                                                                                                                                                            | `edgezero.toml`'s `[adapters.<name>.deployed]` (this spec) | Re-derived by `provision --local` (per propagation rules above).                                                                                                     |
| **Everything else** -- custom `[scripts]` not expressible in `[adapters.<name>.commands]`, alternate `[component.*]` shapes, additional `[setup.*]` declarations, log endpoints, `[local_server]` overrides, etc.                                                                                  | **Not shareable in v1.**                                  | Local-only: operator hand-edits the gitignored adapter manifest; re-runs of `provision --local` preserve those edits via merge mechanics. Teammates re-apply by hand. |

The v1 contract is intentionally minimal: anything not
expressible in the three tracked tiers above stays a
local-only edit. Cross-team sharing of richer customizations
(e.g. a `[adapters.<name>.manifest_extensions]` block in
`edgezero.toml` whose contents merge into the synthesised
output, or a tracked `crates/<adapter-crate>/adapter.toml`
overlay) is an out-of-scope follow-up. The migration guide
notes this limitation; downstream projects that need
richer sharing today work around it by checking in a
small post-`provision --local` shell helper alongside
their `edgezero.toml`.

This sub-section supersedes the earlier wording that
suggested operators "fork the scaffold `.hbs` template"
for cross-team sharing. The scaffold templates ship with
`edgezero new` and are not project-local artefacts in
downstream codebases -- they live inside the EdgeZero
binary, so forking them is not a realistic sharing mechanism.

## Primitive synthesiser output

When a manifest is absent at the start of `provision --local`,
the CLI bootstrap writes the following minimal-valid baseline
via `toml_edit::DocumentMut` before the adapter's
`provision` step layers store bindings on top. `<app_name>`
comes from `edgezero.toml`'s `[app].name`. Per-adapter
overrides past this baseline are operator scope (hand-edit
the synthesised file; the merge mechanics preserve those
edits on re-run). See "Shareable vs. local-only
customizations (v1)" above for the cross-team sharing
contract.

### Cloudflare (`wrangler.toml`)

```toml
# edgezero-provision: v1
name = "<app_name>"
main = "build/worker/shim.mjs"
compatibility_date = "2024-01-01"
```

`build/worker/shim.mjs` matches Wrangler's standard Rust
worker entrypoint convention. Operators using a different
build output edit the synthesised file once; re-runs preserve
the change.

### Fastly (`fastly.toml`)

```toml
# edgezero-provision: v1
manifest_version = 3
name = "<app_name>"
language = "rust"

[scripts]
build = "cargo build --profile release --target wasm32-wasip1"

[local_server]
```

`service_id` is intentionally omitted; if
`[adapters.fastly.deployed].service_id` is present in
`edgezero.toml`, the bootstrap reads it and emits it as a
top-level `service_id = "..."` line. Cloud `deploy`
populates it on first run.

### Spin (`spin.toml`)

```toml
# edgezero-provision: v1
spin_manifest_version = 2

[application]
name = "<app_name>"
version = "0.1.0"

[[trigger.http]]
route = "/..."
component = "<component_id>"

[component.<component_id>]
source = "<target_wasm_path>"
key_value_stores = []
```

**`<component_id>` resolution.** The bootstrap reads
`[adapters.spin.adapter].component` from `edgezero.toml`
(the same selector Spin's runtime validates against actual
component ids at
`crates/edgezero-adapter-spin/src/cli.rs:942`). Precedence:
(1) `[adapters.spin.adapter].component` when set, verbatim;
(2) otherwise `<app_name>` (the project's `[app].name`) as
the default, matching the app-demo fixture and the scaffold
template's first-time output. Whatever the bootstrap writes
into the trigger's `component = "..."` value MUST equal the
`[component.<id>]` block name it emits in the same pass --
Spin's loader otherwise rejects the manifest. Operators who
later add a `component = "..."` value to `edgezero.toml`
out of phase with their already-synthesised `spin.toml`
re-run `provision --local` to refresh.

`<target_wasm_path>` is computed as the conventional
workspace-relative wasm artefact path
`"../../target/wasm32-wasip2/release/<component_id_underscored>.wasm"`
(matches the existing app-demo fixture; the wasm filename
matches the component id, not the app name). Operators
whose workspace layout differs edit the synthesised file
once.

### Spin (`runtime-config.toml`)

```toml
# edgezero-provision: v1
```

Empty body. The adapter's `provision` step adds one
`[key_value_store.<id>]` block per declared
`stores.kv`/`stores.config` id (per the propagation table
above), keyed by the platform name.

### Axum

Axum's per-adapter manifest (`axum.toml`) is synthesised by
`provision --local` on missing, gitignored, and re-generated on
fresh clone -- matching the model for the other three adapters.
Amendment 2026-07: earlier drafts of this spec kept
`axum.toml` tracked because it has no deploy-time identifiers
to weave in, but the asymmetry led to two concrete problems --
dry-run showed no diff for a manifest the operator's tree
did have to produce, and fresh clones missing `axum.toml`
failed at `serve` before any provision step ran. Aligning
`axum.toml` with the other three closes both.

- The primitive synthesiser has an Axum entry emitted by
  `Adapter::synthesise_baseline_manifest` on
  `AxumCliAdapter`. Default content:
  `crate = "<app>-adapter-axum"`, `crate_dir = "."`,
  `host = "127.0.0.1"`, `port = 8787`, prefixed with the
  `# edgezero-provision: v1` header.
- The "Adapter manifests are gitignored" section below
  covers all four: `fastly.toml`, `wrangler.toml`,
  `spin.toml`, `runtime-config.toml`, AND `axum.toml`. Per
  clone, they're regenerated by `provision --local`.
- The CLI bootstrap step ensures `.edgezero/` exists AND
  writes the axum baseline if `axum.toml` is missing.
  `write_baseline_to_disk` skips existing files so operator
  edits (custom host / port / crate_dir) survive re-runs
  byte-identical.
- `provision --local --dry-run` lists `axum.toml` in its
  diff allow-list, so operators can preview the exact
  content that would land on first synthesis.

The adapter merge path is a no-op on the file: Axum has no
per-machine identifiers to weave in on re-provision, so
`AxumCliAdapter::provision` does not touch the manifest.
Operator edits therefore survive byte-identical -- the same
guarantee the other three adapters' merge paths provide for
their own edits.

## Per-adapter local state

Each row's "Already gitignored?" column reflects the project state
AFTER this spec lands (the scaffold `.gitignore` discussed below).

### Axum

| File                              | What `provision --local` writes                                                                                                                                                                                                                                   | Already gitignored? |
| --------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------- |
| `.edgezero/.env`                  | **Base `provision --local`** writes one `EDGEZERO__STORES__<KIND>__<ID>__NAME` line per `StoresMetadata` id (KV / CONFIG / SECRETS), set to the **platform-resolved name** (same value the adapter manifest's binding uses; see "Env-file `__NAME` values match adapter bindings" below), plus one `EDGEZERO__STORES__CONFIG__<ID>__KEY` line per CONFIG id only -- `__KEY` is config-only at runtime (Cloudflare's `env_config_from_worker` at `crates/edgezero-adapter-cloudflare/src/lib.rs:55` derives `__KEY` only for CONFIG; KV/SECRETS have no per-id key override). **Generated `<app-cli> provision --local` (`provision_typed`)** appends one `<key_value>=` line per `#[secret]` field in the typed `C` (key NAME → empty string placeholder for the operator to fill in). Base writes store env labels; typed appends secret placeholders -- bundled `edgezero` has no typed `C`, so it cannot walk `#[secret]` fields. See the "Per-adapter `provision_typed` output" table above for the typed path. | yes (`.edgezero/`)  |
| `.edgezero/local-config-<id>.json` | Not created by provision (`config push --local` owns this file). Provision only ensures `.edgezero/` exists.                                                                                                                                                  | yes                 |

Axum reads secrets via `EnvSecretStore` from process env. The
implementing PR extends `crates/edgezero-cli/src/lib.rs`'s
`run_serve` (the CLI orchestrator at `lib.rs:179`) with an
**adapter-scoped** env-file load: at most ONE env file is
loaded per invocation, selected by the resolved adapter
name. Each `<key>=<value>` line is set in the current
process env BEFORE dispatching to either the
manifest-defined serve command OR the adapter registry's
`serve`. `run_serve` is shared across adapters, so loading
both files unconditionally would let Axum's
`.edgezero/.env` preempt Spin's `EDGEZERO__STORES__*__NAME`
lines via the "existing env wins" rule below, silently
opening the wrong store labels on every Spin serve.

Dispatch table (MUST):

| `args.adapter` | File loaded                                              |
| -------------- | -------------------------------------------------------- |
| `axum`         | `<manifest_root>/.edgezero/.env`                         |
| `spin`         | `<spin_crate_dir>/.env`                                  |
| `cloudflare`   | none (operator's `wrangler dev` reads `.dev.vars` itself) |
| `fastly`       | none (Viceroy reads `[local_server.config_stores.*]` itself) |
| any other name | none                                                     |

`<spin_crate_dir>` resolves via the same path the adapter
dispatch uses (the parent of `[adapters.spin.adapter].manifest`,
defaulting to the in-tree per-adapter crate when unset).
The Spin file load is required because the default serve
command at `crates/edgezero-adapter-spin/src/cli.rs:54`
is `spin up --from {crate_dir} --runtime-config-file
{crate_dir}/runtime-config.toml`, which `run_serve`
executes from the workspace root via the manifest command
path at `crates/edgezero-cli/src/lib.rs:182`. Spin's env
provider reads `.env` from the spawning process's
current working directory; that's the workspace root,
NOT `{crate_dir}`. So the operator's
`<spin_crate_dir>/.env` would never reach Spin without
the CLI loading it explicitly first.

Contract test (MUST): create a fixture with conflicting
values -- `<manifest_root>/.edgezero/.env` containing
`EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=axum_only`,
`<spin_crate_dir>/.env` containing
`EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=spin_only` --
run `edgezero serve --adapter spin`, intercept the env
vars passed to the spawned `spin up` child via a fake
`spin` shim on `PATH`, and assert the child saw
`spin_only` (NOT `axum_only`). Mirror test for
`--adapter axum` asserting it saw `axum_only`.

The CLI layer is chosen (not
`crates/edgezero-adapter-axum/src/cli.rs`'s `serve`)
because `edgezero serve` honours manifest-defined serve
commands at `crates/edgezero-cli/src/adapter.rs:97`
BEFORE the adapter dispatch -- generated and app-demo
manifests define serve commands, so an adapter-layer
`.env` load would be bypassed. Loading in the CLI
ensures both paths see the env vars.

Operator-set env vars on the shell take precedence (the
`.env` load is best-effort: existing
`std::env::var(<key>).is_ok()` skips the line). Without this,
the runtime never sees the placeholders provision wrote. The
smoke harness drops its inline `demo_api_token=...` env shim
once this lands.

Lines in `.edgezero/.env` look like:

```sh
# edgezero-provision: v1
# Override the binding's default key:
# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging

# Secret values -- key NAMES come from the AppConfig's #[secret]
# fields' VALUES at rest in the blob (Model A). Fill in real
# values before running `cargo run`:
demo_api_token=
```

### Cloudflare

| File              | What `provision --local` writes                                                                                                                                                                                                                                                                      | Already gitignored?                                              |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| `.dev.vars`       | Same base/typed split as Axum's `.env`. **Base `provision --local`** writes `EDGEZERO__STORES__<KIND>__<ID>__NAME` lines for KV / CONFIG / SECRETS (values are the **platform-resolved name**, matching the `binding` of the corresponding `[[kv_namespaces]]` entry; see "Env-file `__NAME` values match adapter bindings" below) plus `EDGEZERO__STORES__CONFIG__<ID>__KEY` lines for CONFIG ids only. **Generated `<app-cli> provision --local` (`provision_typed`)** appends one `<key_value>=""` line per `#[secret]` field (quoted-empty wrangler convention). `wrangler dev` surfaces these to `env.var(...)`. See the "Per-adapter `provision_typed` output" table above for the typed path. | yes -- included in the scaffold-generated `.gitignore` (below)  |
| `.wrangler/state/`| Not created or pre-seeded. `wrangler kv` populates this on first push.                                                                                                                                                                                                                              | yes                                                              |
| `wrangler.toml`   | Synthesised from primitives when absent (first-run bootstrap; see "Primitive synthesiser output" below). When present, additively merged: missing `[[kv_namespaces]]` entries for any `stores.kv` or `stores.config` id are appended (per the propagation table above). Operator-set bindings, durable namespace ids from `[adapters.cloudflare.deployed]`, and unrelated keys are preserved verbatim.                                  | yes -- per the gitignore section below                           |

Line-wise dedup applies to `.dev.vars` (see Merge mechanics).

### Fastly

| File           | What `provision --local` writes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| -------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fastly.toml`  | Append IFF the named block is absent (never overwrite existing blocks): `[local_server.config_stores.<id>]` (`format = "inline-toml"` + empty `[local_server.config_stores.<id>.contents]` sub-table -- NEVER `contents = ""`; see propagation table above) per `stores.config` id; the `[[local_server.kv_stores.<id>]]` array-of-tables placeholders described in the propagation table above; `[local_server.config_stores.edgezero_runtime_env]` with `contents` containing `EDGEZERO__STORES__<KIND>__<ID>__NAME = "<platform_name>"` for KV / CONFIG / SECRETS ids (the value is the **platform-resolved name**, matching the binding the per-store propagation table emits; see "Env-file `__NAME` values match adapter bindings" below) and a commented-out `# EDGEZERO__STORES__CONFIG__<ID>__KEY = "<logical_id>_staging"` example line for CONFIG ids only. The `__KEY` value is a **config-blob key inside the store** (per `EnvConfig::store_key` at `crates/edgezero-core/src/env_config.rs:113`, which falls back to the logical id), NOT a store label -- the commented example shows the per-environment override pattern (e.g. `app_config_staging` instead of the default `app_config`) per spec 12.7. **Secret-store entries are NOT written here** -- the bundled `run_provision` has no typed `C`, so it can't know the secret KEY NAMES. `run_provision_typed` (the generated `<app-cli>` path) writes the per-secret `[[local_server.secret_stores.<store_id>]]` entries using the resolved `TypedSecretEntry.store_id` (NOT a hardcoded `default`), per the Per-adapter `provision_typed` output table above. Synthesised from primitives when absent (first-run bootstrap; see "Primitive synthesiser output" below). |

The `[setup.config_stores.<id>]` / `[setup.secret_stores.<id>]`
blocks that cloud-mode provision writes are NOT touched in
`--local` mode (those drive `fastly compute deploy`, not viceroy).

### Spin

| File                                                  | What `provision --local` writes                                                                                                                                                                                                                                                                              |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `runtime-config.toml` (next to `spin.toml`)           | A `[key_value_store.<platform>]` block per declared config/kv id with `type = "spin"` and a default `path = ".spin/sqlite_key_value.db"`. Created when absent; existing blocks preserved. **No** `[variables_provider]` block -- Spin's runtime-config does not have a variables-provider concept (verified against the official `spinframework.dev/v3/dynamic-configuration` page); variables resolve from `[variables]` defaults in `spin.toml` plus `SPIN_VARIABLE_*` process env. |
| `.env` (next to `spin.toml`)                          | Two groups of lines (line-wise dedup applies to all): (1) `EDGEZERO__STORES__<KIND>__<LOGICAL_ID>__NAME=<platform_name>` for every KV / CONFIG / SECRETS store id (same shape as Axum / Cloudflare emit; required because Spin's runtime resolves labels through the env overlay at `crates/edgezero-adapter-spin/src/request.rs:258`, `:301`); (2) `# EDGEZERO__STORES__CONFIG__<LOGICAL_ID>__KEY=<logical_id>` for CONFIG ids only, commented out by default (operator uncomments and edits per-environment override per spec 12.7). **The `SPIN_VARIABLE_*` lines are NOT written by base `provision --local`** -- they require typed-`C` reflection over `#[secret]` fields, which only the generated `<app-cli>`'s `run_provision_typed` path has. See the "Spin" row of the "Per-adapter `provision_typed` output" table above for the typed-CLI's `SPIN_VARIABLE_<SPIN_VAR_UPPER>=` emission. The base/typed split mirrors the Cloudflare and Fastly split: base writes adapter-level bindings + env labels; typed writes per-secret placeholders. Consumption: `edgezero serve` loads this file into the spawning process env BEFORE exec'ing the Spin serve command (`crates/edgezero-cli/src/lib.rs:182`); see the adapter-scoped `run_serve` env-file table described after the Axum row above. Operators invoking `spin up` directly from a third-party shell still need to source the file by hand. The correct pattern is path-explicit so it works from any cwd: either `set -a; source <spin_crate_dir>/.env; set +a; spin up --from <spin_crate_dir> ...` (sources by full path; cwd irrelevant), or `cd <spin_crate_dir> && set -a; source .env; set +a; spin up ...` (cd first, then sources from current dir). The bare-`source .env` form is only correct when cwd already IS the Spin crate dir, which is rarely true from operator shells. |
| `spin.toml`                                           | Synthesised from primitives when absent (first-run bootstrap; see "Primitive synthesiser output" below). When present, the component's `key_value_stores` array gets the missing store ids appended. Everything else operator-owned is preserved by `toml_edit::DocumentMut`. **`[variables]` declarations and `[component.<name>.variables]` mappings are NOT written by base `provision --local`** -- they require typed-`C` reflection over `#[secret]` fields and live in `provision_typed` (the generated `<app-cli>` path). See the "Spin" row of the "Per-adapter `provision_typed` output" table above. |
| `.spin/sqlite_key_value.db`                           | Not created; `spin up` initialises it on first read.                                                                                                                                                                                                                                                       |

### Env-file `__NAME` values match adapter bindings

The `EDGEZERO__STORES__<KIND>__<LOGICAL_ID>__NAME` lines
that provision writes into `.edgezero/.env` (Axum),
`.dev.vars` (Cloudflare), and the
`[local_server.config_stores.edgezero_runtime_env].contents`
table (Fastly) MUST be the **platform-resolved name** -- the
same value the per-store propagation table emits as the
`binding` (Cloudflare) or `[local_server.kv_stores.<name>]`
/ `[local_server.config_stores.<name>]` / `[component.<name>.key_value_stores]`
table name (Fastly / Spin). When no env overlay is set in
the operator's shell when `provision --local` runs, the
platform-resolved name equals the logical id (env-overlay
fallback) and the two are indistinguishable.

The desync risk this rule closes: if the operator set
`EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config` in
their shell at provision time, the adapter manifest's
binding resolves to `prod_config` (today's behaviour at
`crates/edgezero-cli/src/provision.rs:89`) but a v1 spec
that wrote the **logical** id `app_config` into the local
env file would leave the runtime looking for binding
`app_config`, which the manifest doesn't expose -- the
runtime then falls back to its baked-in default and the
operator sees a silent mismatch. Writing the resolved name
into both sides keeps the runtime resolution and the
manifest binding pointing at the same logical-thing.

Spin writes the same per-id `__NAME` lines as the other
adapters. The Spin runtime calls `EnvConfig::from_env()`
at `crates/edgezero-adapter-spin/src/lib.rs:116` and
resolves both KV and CONFIG store labels via
`env.store_name(kind, id)` at
`crates/edgezero-adapter-spin/src/request.rs:258` (KV)
and `crates/edgezero-adapter-spin/src/request.rs:301`
(CONFIG). The `[key_value_store.<name>]` table in
`runtime-config.toml` pins the runtime label for a given
PLATFORM name, but Spin's app code reaches that table by
asking the env overlay first ("logical id `app_config` →
which platform store label?"). If provision wrote
`[key_value_store.prod_config]` into `runtime-config.toml`
but skipped `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config`
in Spin's `.env`, the runtime would fall back to the
logical id `app_config` and open a store named `app_config`
that no `runtime-config.toml` block declares. The
Spin row of the local-state table below adds these lines
to the Spin `.env` write list.

## Interaction with `config push --local`

Spec invariant: **`config push --local` only mutates config-blob
storage**. Stated as **table / key ownership**, not file
ownership -- Fastly's local push writes back to the same
`fastly.toml` provision wrote to, but only into the
`[local_server.config_stores.<id>.contents]` table. Per-key
upserts within that table preserve sibling keys; everything
outside it is untouched.

| Adapter    | What `push --local` writes (the ONLY tables / keys it owns)                          | What it MUST NOT touch                                                                                                                                |
| ---------- | ------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| Axum       | `.edgezero/local-config-<id>.json` JSON-map keys                                     | `.edgezero/.env` (any line)                                                                                                                           |
| Cloudflare | `.wrangler/state/` KV rows (via `wrangler kv`)                                       | `.dev.vars`, any `wrangler.toml` table                                                                                                                |
| Fastly     | `fastly.toml` `[local_server.config_stores.<id>.contents]` keys (per-key upsert)     | `[local_server.kv_stores.*]`, `[local_server.secret_stores.*]`, `[local_server.config_stores.edgezero_runtime_env]`, `[setup.*]`, top-level fields    |
| Spin       | `.spin/sqlite_key_value.db` `spin_key_value` table rows                              | `runtime-config.toml`, `.env`, any `spin.toml` table including `[variables]`                                                                          |

Adapter writers MUST preserve unrelated tables and keys. The
Fastly local writer already does this via per-key upsert (see
`write_fastly_local_config_store` in
`crates/edgezero-adapter-fastly/src/cli.rs`). The Axum local
writer upserts into the JSON map. Cloudflare and Spin shell out
to their emulator-native writers which only touch the KV table.

### Sibling-key coexistence (per spec 12.7)

`config push --local --key app_config_staging` MUST leave the
default `app_config` blob intact. This holds across all four
adapters today (Phase E review fixes).

### Re-provision is idempotent

`provision --local` run twice in a row produces no diff. If the
operator hand-edits a value provision wrote (e.g. fills in a real
secret in `.dev.vars`), the second provision MUST NOT clobber it.
The merge mechanics above pin the contract.

## Adapter manifests are gitignored

All four per-adapter manifests -- **Cloudflare** (`wrangler.toml`),
**Fastly** (`fastly.toml`), **Spin** (`spin.toml` +
`runtime-config.toml`), AND **Axum** (`axum.toml`) -- are
treated as **generated local state**, not source-of-truth
artefacts. `provision --local` regenerates each on fresh clone
or after a delete. Operator edits are preserved via the merge
paths described above:

```
# add to .gitignore (root) -- four entries, NO axum.toml
fastly.toml
spin.toml
wrangler.toml
runtime-config.toml
```

Rationale:

- Both `provision --local` (this spec) and `config push --local`
  (existing) mutate these files. A version-controlled file that
  the dev loop rewrites is a constant source of merge conflicts
  and noisy diffs.
- The canonical inputs that drive each adapter's manifest are
  the tracked `edgezero.toml` primitives plus the cloud
  identifiers persisted in `[adapters.<name>.deployed]` (see
  "Where durable identifiers live"). `provision --local`
  synthesises the per-adapter manifest from those primitives
  via `toml_edit::DocumentMut` so a clean `git clone` plus
  `provision --local` is always reproducible. The richer
  scaffold `.hbs` templates under
  `crates/edgezero-cli/src/templates/` and
  `crates/edgezero-adapter-*/src/templates/` are used only
  by `edgezero new`'s first-time generation -- they carry
  generator-only placeholders (`{{proj_spin}}` etc.) that
  the steady-state dev loop can't reconstruct.
- Operator-authored values (custom `[scripts]`, deploy commands,
  service ids) live alongside generated content; the merge
  mechanics above preserve unknown / pre-existing keys when
  re-writing.

The `runtime-config.toml` pattern is unscoped (matches any file
with that name anywhere in the tree). No fixture, template, or
test artefact may share the name; the implementing PR enforces
this against the TRACKED file list (not docs/templates that
merely mention the name in prose):

```sh
git ls-files | rg '(^|/)runtime-config\.toml$' && exit 1 || true
```

### Migration for the in-tree `examples/app-demo/`

- The currently-tracked manifests are removed from version control:
  - `examples/app-demo/crates/app-demo-adapter-fastly/fastly.toml`
  - `examples/app-demo/crates/app-demo-adapter-cloudflare/wrangler.toml`
  - `examples/app-demo/crates/app-demo-adapter-spin/spin.toml`
  - any in-tree `runtime-config.toml` next to the Spin manifest
- The implementing commit runs `git rm` on each of these files
  and adds the four patterns to the root `.gitignore` in the same
  commit so the worktree is clean immediately after.
- The richer per-adapter scaffold remains in the adapter
  crates' `templates/` directories for `edgezero new`'s use.
  For the steady-state dev loop, `provision --local`
  synthesises the concrete files at the same paths from
  `edgezero.toml` primitives via `toml_edit::DocumentMut`
  (see "Primitive synthesiser output") -- not from those
  `.hbs` templates.
- Smoke scripts that previously `backup_in_tree`'d these files
  drop that machinery -- the files are no longer tracked, so
  worktree cleanliness checks ignore them anyway. Smoke calls
  `provision --local` (or the test-only helper that mirrors it)
  at the top of the run to materialise the files before the first
  push / boot.

The `app-demo.toml` (operator-authored typed config),
`edgezero.toml` (workspace manifest), and the per-crate
`Cargo.toml` files remain tracked -- those are source-of-truth,
not generated.

### Migration for downstream projects

Operators with existing projects that already track
`fastly.toml` / `wrangler.toml` / `spin.toml` / `runtime-config.toml`
follow a one-time runbook:

```sh
# Add the four manifest patterns + .dev.vars to .gitignore.
# `.dev.vars` is Cloudflare's per-secret placeholder file
# (written by `<app-cli> provision --adapter cloudflare --local`);
# operator values must never be committed.
cat >> .gitignore <<'EOF'
fastly.toml
spin.toml
wrangler.toml
runtime-config.toml
.dev.vars
EOF

# Stop tracking the manifest files at every depth (per-adapter
# crates nest them under crates/<adapter>/). --cached preserves
# the local copies so the dev loop keeps working until the
# operator runs `provision --local` next. The portable guarded
# loop below works on macOS / BSD `xargs` (which lacks the
# GNU `-r` "no-run-if-empty" flag) as well as GNU systems.
# `.dev.vars` is left untouched -- operators rarely have it
# tracked, but the same loop will pick it up if they do.
tracked=$(git ls-files | rg '(^|/)(fastly|spin|wrangler|runtime-config)\.toml$|(^|/)\.dev\.vars$' || true)
if [ -n "$tracked" ]; then
  printf '%s\n' "$tracked" | xargs git rm --cached
fi

# Persist deploy-time identifiers (Cloudflare namespace ids,
# Fastly service id) into [adapters.<name>.deployed] in
# edgezero.toml so teammates' `provision --local` regenerates
# manifests with the real ids. See "Where durable identifiers
# live" above.

# Commit. Teammates re-running `edgezero provision --adapter <name> --local`
# regenerate the files locally.
git commit -m "Gitignore Cloudflare/Fastly/Spin manifests; regenerate via provision --local"
```

The migration guide documents this runbook in the per-adapter
sections (see Documentation impact below).

## Scaffold generator (`edgezero new`)

The `edgezero new` generator MUST emit a `.gitignore` at the new
project's root that pre-lists every file generated or rewritten
by the dev loop, so a fresh `git init` after scaffolding doesn't
accidentally commit machine-generated state:

```
# Generated by edgezero scaffolding; safe to extend.

# Build artefacts
target/
pkg/
bin/
!**/src/bin/
!**/src/bin/**

# Per-adapter manifests -- regenerated by `edgezero provision --local`
fastly.toml
spin.toml
wrangler.toml
runtime-config.toml

# Per-adapter local emulator state
.edgezero/
.wrangler/
.spin/
.dev.vars
.env
```

The generator's responsibilities:

1. Write the `.gitignore` above into `<project>/.gitignore`
   (verbatim if no `.gitignore` exists; line-wise dedup if it
   does -- never clobber an operator-extended `.gitignore`).
2. Run **untyped** `provision --local` (the bundled
   `edgezero` binary's `run_provision`, NOT
   `run_provision_typed`) **once per selected adapter**.
   `ProvisionArgs.adapter` at
   `crates/edgezero-cli/src/args.rs:167` is a single
   required `String`, so a literal one-shot call would
   leave every other selected adapter without local
   manifests. The generator MUST iterate the set of
   adapters the operator selected during scaffolding
   (the existing `adapter_artifacts.adapter_ids`
   collection inside `generate_new`) and invoke
   `run_provision` for each in turn:

   ```rust
   for adapter_id in &adapter_artifacts.adapter_ids {
       let mut args = ProvisionArgs::default();
       args.adapter = adapter_id.clone();
       args.local = true;
       args.dry_run = false;
       args.manifest = project_root.join("edgezero.toml");
       edgezero_cli::run_provision(&args).map_err(|err| {
           format!(
               "scaffold provision failed for adapter \
                `{adapter_id}`: {err}"
           )
       })?;
   }
   ```

   If ANY adapter's provision call errors, the scaffold
   fails as a whole (the partial worktree is left for
   the operator to inspect; the generator does NOT
   roll back files it wrote in earlier steps -- that's
   consistent with the existing scaffold's failure
   posture).

   The bundled binary has no typed `C` and cannot walk
   `C::SECRET_FIELDS` (per "Bundled binary vs. generated
   CLI" above). The untyped path here is correct because
   the scaffold ships with every `#[secret]` field
   commented out in `crates/<proj_core>/src/config.rs`
   and the matching `[stores.secrets]` block commented
   out in `edgezero.toml` (see the scaffold's
   `README.md.hbs`). So at scaffold time there are no
   declared secret fields to walk, and the untyped
   provision run produces a complete-for-the-default-app
   local state across every selected adapter. The first
   time the operator un-comments a `#[secret]` field,
   they re-run provision through their generated
   `<project>-cli`
   (`cargo run -p <project>-cli -- provision --adapter <name> --local`),
   which dispatches to `run_provision_typed` and emits
   the per-secret placeholders.
3. Print the same per-adapter "next steps" hints
   `provision --local` would print on a re-run.

Lives in `crates/edgezero-cli/src/generator.rs`'s `generate_new`
flow. The new `.hbs` template at
`crates/edgezero-cli/src/templates/root/gitignore.hbs` matches
the existing scaffold convention: dotfile templates ship
WITHOUT the leading dot in the source path (sibling examples:
`tool-versions.hbs` → `.tool-versions`,
`clippy.toml.hbs` → `clippy.toml`). The output filename is set
EXPLICITLY by a `(template_name, output_path)` tuple in the
scaffold's emit list at `crates/edgezero-cli/src/generator.rs:694`
(e.g. `("root_gitignore", ".gitignore")`) -- there is no
automatic `.hbs`-strip-then-prepend-dot transformation; each
template's output path is named on a separate line. The
implementing PR extends that emit list to include `.gitignore`.

**`.gitignore` is generator-owned, not provision-owned.**
`provision --local` MUST NOT create or modify `.gitignore`
under any circumstance -- the CLI contract earlier in this
spec ("Never creates files outside the adapter crate's
directory or the gitignored local-state directories")
already forbids it. Downstream projects whose scaffolding
predates this spec follow the one-time runbook in the
"Migration for downstream projects" section above
(operator runs `cat >> .gitignore <<'EOF' ... EOF` manually
plus `git rm --cached`). This keeps the provision contract
simple ("only writes inside per-adapter manifest + local
emulator state paths") and avoids the dual-ownership
ambiguity where a re-run of `provision --local` could
silently re-add a line an operator deliberately removed.

## Documentation impact

`grep -rln -E 'fastly\.toml|spin\.toml|wrangler\.toml|runtime-config\.toml' docs/`
returns these files; every one needs a touch to reflect the
gitignored-manifests model:

| File                                       | Update                                                                                                                                                              |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs/guide/getting-started.md`            | First-run flow ends with `provision --adapter <name> --local` (scaffold runs it automatically). Note that Cloudflare / Fastly / Spin manifests are gitignored (Axum's `axum.toml` stays tracked).                     |
| `docs/guide/cli-walkthrough.md`            | Per-adapter walkthroughs: "Generate the manifest" via `provision --local`, not hand-edit.                                                                           |
| `docs/guide/cli-reference.md`              | New `provision --local` row in the CLI table.                                                                                                                       |
| `docs/guide/configuration.md`              | "Where does each setting live?": Cloudflare / Fastly / Spin manifests are operator-modifiable but gitignored; Axum's `axum.toml` stays tracked; durable settings go in `edgezero.toml` (tracked).                    |
| `docs/guide/manifest-store-migration.md`   | Reword tracked-path references to "the local copy of".                                                                                                              |
| `docs/guide/blob-app-config-migration.md`  | Per-adapter mechanics sections: reword to "your local `<manifest>`" plus a note that it's not committed.                                                            |
| `docs/guide/adapters/cloudflare.md`        | Setup section: `wrangler.toml` generated by scaffold + `provision --local`; do not commit.                                                                          |
| `docs/guide/adapters/fastly.md`            | Same for `fastly.toml`.                                                                                                                                             |
| `docs/guide/adapters/spin.md`              | Same for `spin.toml` + `runtime-config.toml`. Note that `[variables]` lives in the generated file -- operator edits are local until they hand-share to teammates. |
| `docs/guide/kv.md`                         | KV setup snippets: replace hand-edits with `provision --local` re-run plus a re-run-safe extension snippet.                                                         |

`docs/.vitepress/dist/` is the built site -- regenerated by
`npm run build` in `docs/`, so no manual edits. The implementing
PR rebuilds it once.

Also update the top-level `README.md` (if it references these
files) and any blog/announcement copy.

## CI impact

Before the implementing PR lands, run:

```sh
grep -rn -E 'fastly\.toml|spin\.toml|wrangler\.toml|runtime-config\.toml' \
  examples/app-demo/crates/*/tests/ \
  examples/app-demo/crates/*/src/
cargo test --workspace --all-targets --no-run
```

The grep catches `fs::read_to_string` / `include_str!` references
in test or production code; the `--no-run` build catches
compile-time references (e.g. `include_bytes!`, build-script
reads).

If either surfaces hits, those call sites read a tracked manifest
in place and would break after `git rm`. Two fixes:

1. **Preferred**: rewrite the call site to build a tempdir fixture
   (matches the pattern
   `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs`
   already uses for spin / cloudflare).
2. **Fallback**: prepend a `provision --local` step in
   `.github/workflows/test.yml` before the app-demo `cargo test`
   line so the materialised manifest exists at test time.

If both checks come back empty, no CI change is required -- the
manifests are build-time data for the adapter binary, not
test-time data for the adapter crate.

### Smoke scripts impact

All four smoke scripts assume the per-adapter local files
(`fastly.toml`, `wrangler.toml`, `spin.toml`,
`runtime-config.toml`, `.dev.vars`, `.edgezero/.env`,
Spin-side `.env`) exist before they boot any emulator or
push config:

- `scripts/smoke_test_config_key_override.sh`
- `scripts/smoke_test_config.sh` (boots Spin and reads
  `runtime-config.toml`; see `smoke_test_config.sh:97`)
- `scripts/smoke_test_kv.sh` (same Spin dependency; see
  `smoke_test_kv.sh:68`)
- `scripts/smoke_test_secrets.sh` (boots Spin / Fastly
  with secret stores; see `smoke_test_secrets.sh:111`)

Once the manifests are gitignored, a fresh clone has none
of these files until provision runs. The implementing PR
adds a **shared smoke warm-up helper** sourced by every
smoke script:

```sh
# scripts/lib/smoke_warmup.sh (new)
#
# Smoke scripts in this repo bootstrap with
# `ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"`
# (see scripts/smoke_test_config.sh:19 for the canonical
# pattern). The helper consumes that variable verbatim --
# callers do NOT need to set REPO_ROOT first.
#
# DEMO_DIR points at the app-demo workspace -- it's
# excluded from the root Cargo.toml (`Cargo.toml:1` lists
# explicit members and does not include
# `examples/app-demo`), so cargo commands MUST run from
# inside it. app-demo-cli has no adapter features
# (`examples/app-demo/crates/app-demo-cli/Cargo.toml`
# has no [features] section) -- adapter selection
# happens via the CLI arg, not via cargo features.
: "${ROOT_DIR:?ROOT_DIR must be set by the caller (existing smoke bootstrap)}"
DEMO_DIR="$ROOT_DIR/examples/app-demo"

# Map the smoke-script CLI alias to the canonical adapter
# name `provision --adapter` accepts. Existing smoke
# scripts accept `cloudflare|cf` (see
# smoke_test_config.sh:72, smoke_test_kv.sh:52);
# `provision --adapter cf` would not match the manifest
# adapter key and fail with a noisy error, so canonicalise
# here. Add new aliases below as new scripts add them.
smoke_canonical_adapter() {
  case "$1" in
    cf|cloudflare) echo "cloudflare" ;;
    *)             echo "$1" ;;
  esac
}

smoke_warmup_provision_local() {
  local adapter
  adapter="$(smoke_canonical_adapter "$1")"
  (
    cd "$DEMO_DIR"
    cargo run --quiet -p app-demo-cli -- \
      provision --adapter "$adapter" --local
  )
}
```

Each smoke script sources
`scripts/lib/smoke_warmup.sh` AFTER its own
`ROOT_DIR=...` bootstrap line and calls
`smoke_warmup_provision_local "$adapter"` (where
`$adapter` is the raw operator-supplied alias the script
already parses) for every adapter it boots, BEFORE any
`config push --local`, emulator boot, or assertion. The
helper uses the generated `app-demo-cli` (not bundled
`edgezero`) so typed secret placeholders land too --
app-demo declares `#[secret]` fields, so the typed path
matters. Runtime artifact builds (wasm modules etc.)
stay in the existing per-adapter build path each smoke
script already invokes; this helper only owns
local-state materialisation. Existing `backup_in_tree`
calls for the gitignored files become no-ops and the
implementing PR drops them in the same commit.

## Per-adapter test contract

Add to each adapter's CLI test file
(`crates/edgezero-adapter-<name>/src/cli.rs`'s
`#[cfg(test)] mod tests`) a per-adapter `provision_local_*`
suite:

1. **First-run writes the expected files** with the documented
   placeholder contents. Assert exact file contents for the
   placeholder-only baseline (no operator edits).
2. **Re-provision is a no-op** when nothing changed. Assert
   byte-for-byte equality before and after the second invocation.
3. **Push after provision leaves provision artifacts intact.**
   Each adapter needs its own concrete test:
   - **Axum**: write `demo_api_token=real-secret-value` into
     `.edgezero/.env`, run `config push --local`, re-read
     `.edgezero/.env`, assert it still contains
     `demo_api_token=real-secret-value` byte-for-byte.
   - **Cloudflare**: write
     `demo_api_token="real-secret-value"` into `.dev.vars`,
     run `config push --local`, re-read `.dev.vars`, assert
     unchanged.
   - **Fastly**: hand-edit a
     `[[local_server.secret_stores.<store_id>]]` entry
     (where `<store_id>` is the operator's
     `[stores.secrets].ids` selection -- typically `default`,
     but parameterised in tests covering
     `#[secret(store_ref = "field")]`) with a real `env`
     mapping, run `config push --local`, re-parse
     `fastly.toml` via `toml_edit::DocumentMut`, assert the
     secret-store array still contains the entry untouched
     (key, env, position).
   - **Spin**: write
     `SPIN_VARIABLE_DEMO_API_TOKEN=real-secret-value` into
     the Spin-side `.env`, run `config push --local`, re-read
     `.env`, assert unchanged.
4. **Spin env-label / runtime-config alignment** (Spin-only,
   load-bearing because Spin resolves store labels through
   the env overlay at
   `crates/edgezero-adapter-spin/src/request.rs:258`
   (KV) and `crates/edgezero-adapter-spin/src/request.rs:301`
   (CONFIG); any drift between the `.env` lines and the
   `runtime-config.toml` / `spin.toml` labels silently opens
   the wrong store at request time):
   - **Writes the expected env lines.** After
     `provision --local`, Spin's `.env` (next to
     `spin.toml`) contains
     `EDGEZERO__STORES__CONFIG__<logical_id>__NAME=<platform_name>`
     for every `[stores.config].ids` entry,
     `EDGEZERO__STORES__KV__<logical_id>__NAME=<platform_name>`
     for every `[stores.kv].ids` entry, and
     `EDGEZERO__STORES__SECRETS__<logical_id>__NAME=<platform_name>`
     for every `[stores.secrets].ids` entry. Where the
     env overlay was unset at provision time, the
     platform name equals the logical id (env-overlay
     fallback).
   - **Labels line up.** Every `<platform_name>` written
     into Spin's `.env` matches a `[key_value_store.<platform_name>]`
     block in `runtime-config.toml` (for KV / CONFIG
     stores) AND appears verbatim in the
     `[component.<component_id>.key_value_stores]` array
     in `spin.toml`. The test parses all three files via
     `toml_edit::DocumentMut`, builds the three sets, and
     asserts set equality. A drift bug (e.g. provision
     writes `__NAME=prod_config` to `.env` but
     `runtime-config.toml` still has
     `[key_value_store.app_config]`) fails this
     assertion with a clear diff.
   - **Env overlay round-trips.** With
     `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config`
     set in the test's process env at provision time,
     re-run the alignment assertion against the resolved
     name `prod_config` (NOT the logical id `app_config`).
     Validates that the provision-time resolution path
     and the runtime-time resolution path read the same
     overlay key.
   - **Re-provision preserves operator-set env lines.**
     Write `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging`
     (uncommenting and editing the override line provision
     left commented), run `provision --local` a second
     time, re-read `.env`, assert the operator line is
     intact byte-for-byte.
5. **Zero cloud calls.** The existing PATH-prepend fake-CLI
   infrastructure (e.g. `fake_wrangler_returning`,
   `fake_fastly_returning`) needs a panicking variant:
   `fake_<cli>_panicking()` writes a shim script that exits
   non-zero with stderr `"FAKE-CLI INVOKED: provision --local
   must not shell out"`. The test prepends this fake to `PATH`
   via `PathPrepend`, runs `provision --local`, asserts
   success. If the implementation shells out, the fake panics
   and the test fails with a clear message.

Cross-adapter smoke (`scripts/smoke_test_config_key_override.sh`):

- After `provision --local`, the runtime can boot and serve
  `/config/typed` end-to-end without any cloud calls.
- The runtime override flow
  (`EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=...`) works
  through the per-adapter env file provision wrote. The smoke
  invokes `edgezero serve` (NOT a bare `spin up`), so the
  `run_serve` adapter-scoped env-file load described in the Axum
  section applies -- the smoke does NOT manually source
  any `.env`. Operators or smokes that bypass
  `edgezero serve` (e.g. invoking `spin up` directly,
  `wrangler dev` from a third-party shell, etc.) must
  source the relevant provision-written `.env` / `.dev.vars`
  themselves; the per-adapter local-state rows above
  document the precise sourcing pattern.

## What this spec does NOT change

- Cloud `provision` (without `--local`) keeps its cloud-side
  resource-creation behaviour. The CLI orchestrator changes
  to record the returned identifiers into
  `[adapters.<name>.deployed]` in `edgezero.toml`, but the
  adapter-level cloud calls (wrangler / fastly / spin
  shell-outs) are unchanged. Same for `edgezero deploy`'s
  cloud-side behaviour.
- The blob envelope format, SHA contract, secret walk, and
  runtime extractor are unchanged.
- The `config push` / `config diff` CLI surface is unchanged.
- No new workspace-level dependencies. Two existing
  workspace deps gain a CLI promotion:
  - `toml_edit` -- already a workspace dep
    (`Cargo.toml` `[workspace.dependencies]`); the
    implementing PR adds
    `toml_edit = { workspace = true }` to
    `crates/edgezero-cli/Cargo.toml`'s `[dependencies]`
    (today only the adapter crates pull it in).
  - `tempfile` -- already a workspace dep AND already
    listed under
    `crates/edgezero-cli/Cargo.toml`'s
    `[dev-dependencies]` at line 48. The implementing
    PR PROMOTES it to `[dependencies]` because the
    dry-run staging helper described above
    (`tempfile::TempDir`) runs in production, not just
    tests. Dev-dep only would make the staging code
    fail to compile in the released binary. The
    workspace dep entry already exists, so this is a
    single-line edit to the CLI crate manifest, NOT a
    new workspace dep.

  No NEW crates land in the workspace.

## Out of scope (future work)

- Generating real cryptographic secret values. Operators fill
  in the placeholders. A `provision --local --gen-secrets`
  flag could ship in a follow-up.
- Pulling Axum's `axum.toml` into the generated /
  gitignored model. v1 keeps `axum.toml` tracked and
  scaffold-owned (see the Axum subsection under "Primitive
  synthesiser output") because the file carries no
  deploy-time identifiers or platform-resolved store
  labels -- nothing the other three adapters' manifests
  carry that justifies the gitignore + synthesiser
  symmetry. A v2 that adds deploy-time fields to
  `axum.toml` (none exist today) revisits the call.
- OS-level filesystem sandboxing for adapter dispatch.
  v1's path containment (rejection of absolute and `..`
  paths plus the dry-run tempdir staging) protects
  manifest-declared paths but does NOT defend against an
  adapter's `provision` calling `fs::write` against a
  hardcoded absolute path. Defending against that would
  require seccomp / Landlock / similar OS sandboxing,
  which is a much larger change. v1 treats the adapter
  trait as a trust boundary; in-tree adapters are
  reviewed in this repo, and third-party adapters that
  ignore `manifest_root` are an operator-level concern.
- Cross-adapter env file format unification. Each adapter keeps
  its idiomatic local-state shape (`.dev.vars` for CF, `.env`
  for Spin, etc.) so the files stay greppable for operators
  familiar with each platform.
- Migration of existing local state from non-blob shape. The
  blob-app-config cutover already documented the operator
  runbook; `provision --local` is additive.
- A `# edgezero-provision: v1` header revvable migration story
  for v2 placeholder format changes. v1 ships with the header
  in place; no v2 migration logic yet.
- A standalone `edgezero sync-deployed --adapter <name>`
  command for the bypassed-deploy case (operator uses a
  manifest shell command instead of the adapter's `deploy`
  method, so deploy-time writeback to
  `[adapters.<name>.deployed]` doesn't fire). v1 documents
  the manual fix-up runbook in the migration guide; a
  dedicated command would be its own clap args + dispatch
  + tests + docs.
- Automatic Cloudflare preview-namespace capture during
  cloud `provision --adapter cloudflare`. v1 only extracts
  the primary namespace id from `wrangler kv namespace
  create`; operators wanting a separate preview namespace
  run `wrangler kv namespace create <binding> --preview`
  manually and write the result into
  `[adapters.cloudflare.deployed].preview_kv_namespaces.<logical_id>`.
  A v2 follow-up adds a second `wrangler` call inside
  `create_kv_namespace` at
  `crates/edgezero-adapter-cloudflare/src/cli.rs:535` and
  the corresponding `AdapterDeployedState` sub_tables entry
  so the round-trip is automatic.
- `--app-config <path>` override on `provision --local`.
  v1 only supports the conventional `<app_name>.toml`
  next to `edgezero.toml`. The follow-up is small (plumb
  the optional path through `ProvisionArgs` to the
  shared helper) but is intentionally deferred to keep
  v1's CLI surface minimal; operators with
  non-conventional layouts symlink the file into the
  conventional location for the duration of the
  provision call.
