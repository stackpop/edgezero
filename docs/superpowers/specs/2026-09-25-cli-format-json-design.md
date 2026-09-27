# Machine-readable CLI output (`--format json`) — Design

- **Issue:** [#383](https://github.com/stackpop/edgezero/issues/383)
- **Status:** Approved; implemented (revision 2 records what implementation changed, see §13)
- **Date:** 2026-09-25
- **Delivery:** a single implementation PR, opened only after this spec is approved

## 1. Summary

Nine CLI commands gain a `--format <text|json>` flag. `text`, the default, keeps
today's output byte for byte. `json` writes exactly one versioned JSON envelope
to stdout and sends everything else to stderr. To make this possible, adapters
return typed outcomes to the CLI instead of printing their results as log
lines. The CLI then owns both renderings.

**Commands in scope:** `active-version`, `auth status`, `build`, `config gc`,
`config validate`, `deploy`, `healthcheck`, `provision`, `rollback`.

## 2. Goals and non-goals

### Goals

1. Scripts can gate on structured results without parsing log lines.
2. Every existing invocation behaves exactly as before: same bytes, same
   streams, same exit codes.
3. Under `--format json`, no byte on stdout is anything other than the
   envelope. This holds by construction and is enforced by lint and tests,
   not by convention.
4. The JSON shape is a documented, versioned contract with an explicit
   compatibility policy.

### Non-goals

- `--format` on `new`, `serve`, `demo`, `auth login` / `auth logout`, or
  `config push`.
- Any change to `config diff` or to its existing JSON envelope (§6.6).
- Streaming or progress events (NDJSON). One command produces one document.
- Machine-readable error codes. v1 errors carry a message only, and codes can
  be added later without breaking consumers (§6.5).
- Published JSON Schema files, or a `schemars` dependency.
- JSON for command-line usage errors, which clap reports before `--format` is
  known (§6.4).
- Migrating the `.github/actions/*` scripts from `key=value` parsing to JSON.
  They keep working unchanged, and moving them is optional follow-up work.
- An environment variable that selects the format.

## 3. Current state

These are the findings this design responds to. Line numbers are as of
`563fb65`.

| Area | Today |
| --- | --- |
| Result data | The values a script wants (`version=`, `healthy=`, `status-code=`, `rolled-back-to=`) exist only as `log::info!` lines inside the Fastly adapter. `Adapter::execute` returns `Result<(), String>`. `provision` and `gc_config_entries` return prose `Vec<String>` (`edgezero-adapter/src/registry.rs:291`, `:322`, `:398`). |
| Logger | `CliLogger` sends `Info` to **stdout** via `println!`, and `Warn`/`Error` to stderr (`edgezero-cli/src/lib.rs:96-118`). This is the only production `print_stdout` site outside `config diff`. |
| Child processes | `run_native_cli` (`edgezero-adapter/src/cli_support.rs:82`), the Fastly `run_fastly_status`, each adapter's `cargo` runner, and the CLI's `run_shell` all inherit stdout. `run_shell_tee` copies child stdout to our stdout (`edgezero-cli/src/adapter.rs:344-422`). |
| Text contract | CI actions parse `^version=[0-9]+$`, `healthy=` and `pushed-key=` from stdout (`.github/actions/*/scripts/*.sh`). |
| Error exit | The bundled binary logs `[edgezero] <err>` to stderr and exits 1. The downstream template does the same with its own prefix and exits 2. Both call the library's `run_*(&Args) -> Result<(), String>`. |
| Lints | The workspace denies clippy `restriction`, so `print_stdout` and `print_stderr` are already forbidden outside `#[expect]` sites. |
| Issue drift | #383 says `config gc` / `config validate` print through `eprintln!`. They actually use `log::info!`, which goes to stdout. `config validate` stops at the first error, so it has no list of findings to emit. |

## 4. Decisions on the issue's open questions

| # | Question | Decision | Section |
| --- | --- | --- | --- |
| 1 | Enum sharing | A new shared `OutputFormat { Text, Json }`. `DiffFormat` is left alone. | §5.1 |
| 2 | Name of the non-JSON value | `text`, used consistently across the CLI. `config diff` keeps `unified` as its only exception. | §5.1 |
| 3 | Stream discipline | Under `json`, stdout holds one envelope. Logs and child output go to stderr. Enforced by a scoped routing guard, a spawn lint and an end-to-end test. | §6.2, §7.4 |
| 4 | Error representation | Failures still emit the envelope, with `ok: false` and `error.message`. Exit codes are unchanged. | §6.3, §6.4 |
| 5 | Envelope stability | `schema_version: 1` is shared by every command. Additive changes don't bump it; breaking changes do. | §6.5 |
| 6 | Stub commands | No change. They exit 2 with empty stdout and the pointer on stderr. | §5.3 |

## 5. Command-line interface

### 5.1 The flag

```rust
/// Output format for commands with a machine-readable mode.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputFormat {
    /// Human-readable output (the pre-existing behaviour).
    #[default]
    Text,
    /// A single JSON envelope on stdout (see the cli-reference docs).
    Json,
}
```

Each in-scope args struct gets:

```rust
/// Output format: `text` (default) or `json`.
#[arg(long, value_enum, default_value_t = OutputFormat::Text)]
pub format: OutputFormat,
```

The structs are `ActiveVersionArgs`, `BuildArgs`, `ConfigGcArgs`,
`ConfigValidateArgs`, `DeployArgs`, `HealthcheckArgs`, `ProvisionArgs`,
`RollbackArgs`, and the `AuthSub::Status` variant only.

`DiffFormat` is **not** generalized or reused. Its `structured` and `unified`
values only make sense for a diff, and changing it would change how
`config diff` is spelled. A separate enum means no command advertises a value
it cannot honour.

### 5.2 Parsing constraints

- `--format=json` and `--format json` are both accepted, as clap handles either.
- `build` collects passthrough arguments with `trailing_var_arg`, so
  `--format` has to come before the first passthrough token. After that point
  it is forwarded to the adapter. This is documented and covered by a parse
  test.
- `deploy` passes arguments through only after `--`, so `--format` can go
  anywhere before `--`.

### 5.3 Bundled-binary stubs

`config push` and `config diff` in the bundled `edgezero` binary stay as they
are:

- They absorb every token, including `--format json`.
- They print the pointer text to stderr, write nothing to stdout, and exit 2.

They point elsewhere rather than being in-scope commands, and emitting JSON
would mean interpreting tokens they deliberately never parse. The
documentation tells scripts to read "exit 2 with empty stdout" as "not
available in this binary".

## 6. Output contract

### 6.1 The envelope

Every in-scope command run with `--format json` writes exactly this shape,
whether it succeeds or fails:

```json
{
  "schema_version": 1,
  "command": "healthcheck",
  "ok": true,
  "result": { "...": "command-specific, see §8" },
  "error": null
}
```

| Key | Type | Rule |
| --- | --- | --- |
| `schema_version` | integer | `1` for this spec. See §6.5. |
| `command` | string | The command path as the user types it: `"active-version"`, `"auth status"`, `"build"`, `"config gc"`, `"config validate"`, `"deploy"`, `"healthcheck"`, `"provision"`, `"rollback"`. |
| `ok` | boolean | `true` exactly when the process will exit 0. |
| `result` | object or `null` | Never `null` when `ok` is `true`. When `ok` is `false` it holds the **partial result** if the command produced one (an unhealthy healthcheck, a partially failed gc, an unauthenticated `auth status`, `active-version --require-active` with nothing active), and is `null` otherwise. |
| `error` | object or `null` | `null` exactly when `ok` is `true`. Otherwise `{ "message": string }`. |

**Field conventions**, applied to the envelope and every result:

- Keys are `snake_case`.
- All five envelope keys and every documented result key are always present.
  An unknown value is `null`, never an omitted key. An empty collection is
  `[]`, never `null`.
- Integers are JSON numbers. Durations are integer seconds, and their keys end
  in `_secs`.
- Filesystem paths are strings, lossily converted from the OS encoding.
- String-valued enums are lowercase `snake_case`.
- The order of keys is not part of the contract.

### 6.2 Streams

| | `--format text` (default) | `--format json` |
| --- | --- | --- |
| stdout | Unchanged: info logs, `key=value` lines, and inherited child stdout | **Exactly one envelope**: pretty-printed (`serde_json::to_string_pretty`, matching `config diff`), ending with `\n`, written once when the command finishes |
| stderr | Unchanged: warnings and errors | Everything `text` mode prints (every log level, including the `key=value` lines), all child-process stdout and stderr, and the binary's final `[edgezero] <err>` line |

Rules:

- Nothing is silenced. Each command prints its human-readable output through
  the logger in both formats; under JSON it moves to stderr, so CI logs keep
  showing progress and the text output needs no second renderer.
- The envelope is written only by `edgezero_cli::output`, and only once per
  run.
- An empty stdout with a non-zero exit means no envelope was produced. That
  happens on usage errors, stub commands and panics, and consumers must treat
  it as failure.

### 6.3 Where the envelope is written

The envelope is written **inside the library's `run_*` functions**, not in
`main`. Their public signatures stay `pub fn run_x(&XArgs) -> Result<(), String>`.

- On failure, a JSON-mode `run_*` writes the failure envelope to stdout and
  then returns `Err(message)` as it does today.
- Each binary's `main` then logs the error to stderr and exits with its usual
  code.

This way, downstream CLIs generated from the template before this change get
`--format json`, including failure envelopes, without regenerating `main.rs`.

### 6.4 Exit codes

Exit codes do not change.

| Situation | Exit | stdout under `json` |
| --- | --- | --- |
| Success | 0 | envelope, `ok: true` |
| Command failure | 1 (bundled binary) / 2 (template-generated CLIs) | envelope, `ok: false` |
| Usage error (reported by clap) | 2 | empty. clap parses before the format is known, and its message goes to stderr as today |
| Bundled stub (`config push` / `config diff`) | 2 | empty |

Consumers should branch on `ok`, not on the specific non-zero code, because
failure codes differ between binaries.

### 6.5 Versioning and compatibility

- One `schema_version` covers the whole JSON surface of the CLI (the envelope
  and every result), so there is one number to pin. It starts at `1`.
- **Compatible changes (no bump):** adding a key to `result` or `error`; adding
  `error.code`; turning a nullable field into one that is always non-null;
  adding a value to a string enum.
- **Breaking changes (bump):** removing or renaming a key; changing a key's
  type or meaning; making a non-null field nullable; changing the envelope's
  meaning (`ok` semantics, when `result` is `null`).
- **What consumers must do:** ignore unknown keys; handle unknown enum values;
  check `schema_version`.
- A bump is recorded in the CLI reference changelog together with the PR that
  makes it.

### 6.6 `config diff` is excluded

`config diff --format json` keeps its existing unversioned shape
`{ local_sha256, remote_sha256, added, removed, changed }`, including its
current empty-stdout outcomes. Moving it into the envelope would break its
consumers, and #383 requires it to stay "exactly as it does today". The docs
note it as the one legacy shape. Bringing it into the envelope, under a
`schema_version` bump, is left to a separate issue.

## 7. Architecture

### 7.1 Overview

```
 args (format) ──► run_x ──► OutputScope::enter(format)   (json: logs + child stdout → stderr)
                     │
                     ├──► adapter / CLI logic ──► Outcome<R> = Result<R, Failure<R>>
                     │        (logs today's human-readable lines as it goes)
                     │
                     └──► output::finish(command, format, outcome)
                              └── json only: envelope → stdout
```

There are three layers, each with one job:

1. **`edgezero-adapter`: domain outcomes.** Plain Rust types that describe
   what happened. No `serde`, and no new dependencies.
2. **The wire schema** (`edgezero-cli/src/output.rs`). `serde::Serialize`
   structs that match §6.1 and §8 exactly, converted from the domain outcomes.
   They are the only types that define the JSON contract, so an internal
   refactor of an adapter type cannot change the public schema by accident.
3. **Rendering and streams** (also `output.rs`). `OutputScope`, `Failure`, the
   envelope, and `finish`.

**Text output is not re-rendered.** Every command keeps logging its
human-readable lines through the logger exactly as before, in both formats.
`text` mode is therefore byte-identical by construction, and `json` mode is
the same lines on stderr plus the envelope on stdout.

### 7.2 Adapter trait changes

The crates are `publish = false`, and every `Adapter` implementation is in
this workspace (fastly, cloudflare, spin, axum, plus two test adapters), so
changing the trait breaks no external implementor.

```rust
// edgezero-adapter/src/registry.rs (no serde)

#[non_exhaustive]
pub enum ActionOutcome {
    ActiveVersion(ActiveVersionOutcome), // { service_id, version: Option<u64> }
    AuthStatus(AuthStatusOutcome),       // { failure: Option<String>, state: AuthState }
    Build(BuildOutcome),                 // { artifact: Option<PathBuf> }
    Deploy(DeployOutcome),               // { version: Option<u64> }
    Empty,                               // login, logout, serve, manifest command overrides
    Healthcheck(HealthcheckOutcome),     // { attempts, domain, failure, healthy, path, service_id,
                                         //   staging, staging_ip, status_code, version, version_verified }
    Rollback(RollbackOutcome),           // { rolled_back_to, service_id, staging, version }
}

pub enum AuthState { Authenticated, NotApplicable, Unauthenticated }

fn execute(&self, action: AdapterAction, args: &[String]) -> Result<ActionOutcome, String>;
fn provision(/* unchanged params */) -> Result<ProvisionReport, String>;
fn gc_config_entries(/* unchanged params */) -> Result<GcReport, String>;
```

The outcome structs have public fields and no `#[non_exhaustive]`, so adapter
crates can build them. `AuthState`, `ProvisionAction` and `StoreKind` are
deliberately **exhaustive**: the CLI maps each variant onto the wire schema, so
adding a variant fails to compile until the schema covers it.

**Outcomes versus errors.** A negative result that the command still finished
measuring is an `Ok` outcome carrying a `failure` message, not an `Err`:

- an unhealthy healthcheck is `healthy: false`;
- a non-zero exit from `auth status` is `Unauthenticated`
  (`cli_support::native_auth_status`, or a manifest `auth-status` command);
- a gc run with failed deletes has a non-empty `failed` list and a
  `failure_diagnostic` holding exactly the error text it returned before.

The CLI turns these into a non-zero exit, using the same message as before.
`Err(String)` is kept for "could not determine a result": bad arguments, API
failures, a missing binary. This is what lets the failure envelope carry the
partial `result` described in §6.1.

```rust
pub struct ProvisionReport { pub entries: Vec<ProvisionEntry> }   // + into_messages()
pub struct ProvisionEntry {
    pub action: ProvisionAction,          // AlreadyPresent | Created | NotApplicable | Note
                                          // | Updated | WouldCreate | WouldUpdate
    pub message: String,                  // the exact line text mode prints
    pub store: Option<ProvisionStoreRef>, // { kind: StoreKind, logical: Option<String>, platform }
}

pub struct GcReport {
    pub deleted: Option<usize>,           // None on a dry run
    pub entries: usize,
    pub failed: Vec<String>,
    pub failure_diagnostic: Option<String>,
    pub generations_planned: usize,
    pub kept_roots: Vec<String>,
    pub planned: Vec<GcCandidate>,        // { age_secs, key }
    pub referenced_chunks: usize,
    pub retained_recent: usize,
    pub roots: usize,
    pub store_id: Option<String>,
    pub stranded: Vec<String>,
    pub text_lines: Vec<String>,          // text-mode lines only; not on the wire
    pub uncertain: Vec<String>,
    pub unprovable: usize,
    pub warnings: Vec<String>,
}
```

**Data lines move from the Fastly adapter to the CLI.** The adapter used to
print `version=`, `status-code=`, `healthy=` and `rolled-back-to=`, plus the
prose line `active-version` prints after an empty `version=`. It now returns
them in its outcome, and the CLI logs the same bytes through the same
`log::info!` channel. Prose-only lines, such as the staged-rollback message
and the "no FASTLY_API_TOKEN" note, stay in the adapter. Under JSON they move
to stderr like any other log line.

### 7.3 CLI flow (`run_*`)

```rust
pub fn run_healthcheck(args: &HealthcheckArgs) -> Result<(), String> {
    let _scope = OutputScope::enter(args.format);
    output::finish(CommandName::Healthcheck, args.format, healthcheck(args))
}

fn healthcheck(args: &HealthcheckArgs) -> Outcome<HealthcheckResult> { … }
```

- `Outcome<R>` is `Result<R, Failure<R>>`. `Failure` carries the message and an
  optional boxed partial result, and `From<String>` lets `?` keep working on
  existing `String` errors.
- `finish` writes the envelope to stdout only under `json`, then returns
  `Err(message)` on failure so each binary's `main` exits as before.

### 7.4 Stream routing and enforcement

`OutputScope::enter(OutputFormat::Json)` does two things and undoes both when
dropped (an RAII guard). A `text` scope changes nothing, so text-mode tests
running in parallel never touch the global state.

1. **Logger.** `CliLogger` reads an `INFO_TO_STDERR: AtomicBool`, and while it
   is set `Info` is written with `eprintln!`. This has to be process-wide state,
   because the `log` facade has a single global logger.
2. **Child stdout.** A new, always-compiled module `edgezero_adapter::process`
   holds a process-wide policy (`set_child_stdout_to_stderr`) and the one
   sanctioned inheriting spawn, `process::status(&mut Command)`. Under the
   policy it sets `.stdout(Stdio::from(io::stderr()))` before waiting for the
   child. It is not in `cli_support`, because `edgezero-cli` depends on
   `edgezero-adapter` without the `cli` feature.
   - It replaces every `Command::status()` call: `run_native_cli`, Fastly
     `run_fastly_status`, each adapter's `cargo`, `fastly`, `wrangler` and
     `spin` runners, the CLI's `run_shell`, and `git init` in `new`.
   - `run_shell_tee` reads the same policy, so its tee target switches to
     stderr.
   - Calls that capture output with `.output()` or piped stdio already keep
     stdout clean and are unchanged.

**Why a scoped policy and not a parameter.** Adding a parameter to every
spawn helper would change about 15 Fastly helpers plus the runners in every
adapter. It still wouldn't stop a future helper from calling
`Command::status()` directly and leaking. The property that matters is that
every child is spawned through one choke point, and that is enforced
mechanically:

- **Lint.** `clippy.toml` has `disallowed-methods` for
  `std::process::Command::status` and `std::process::Command::spawn`. The
  exceptions are documented `#[expect(clippy::disallowed_methods)]` sites:
  `process::status` itself, and the three spawns whose stdio is fully piped
  (Fastly's config-entry writer, Fastly's curl API client, and
  `run_shell_tee`). The ignored `generated_project_builds` test drives `cargo`
  directly and carries the same `#[expect]`.
- **Existing lint.** `print_stdout` is already denied workspace-wide. The
  envelope is written through `io::stdout()` in exactly one function,
  `output::finish`.
- **End-to-end tests.** See §9.3.

## 8. Per-command result schemas

Every command's `result` includes `adapter` (string), except
`config validate`, which is adapter-independent. `ok` follows §6.1.

### `active-version`

```json
{ "adapter": "fastly", "service_id": "SU1Z0isxPaozGVKXdv0eY", "version": 42 }
```

- `version` is `null` when no version is active.
- With `--require-active` and no active version: `ok: false`, and `result` is
  present with `version: null`.

### `auth status`

```json
{ "adapter": "cloudflare", "state": "authenticated" }
```

- `state` is one of `authenticated`, `unauthenticated` or `not_applicable`.
  axum reports `not_applicable` with `ok: true`.
- `unauthenticated` means `ok: false` with `result` present. This keeps
  today's non-zero exit.
- A native CLI that is missing or fails to spawn gives `ok: false` with
  `result: null`.
- `state` comes from the native CLI's exit status, or from a manifest
  `commands.auth_status` override. Account details are not parsed.

### `build`

```json
{ "adapter": "fastly", "artifact": "/abs/path/target/wasm32-wasip1/release/app.wasm" }
```

`artifact` is `null` when a manifest `commands.build` override ran or the
adapter cannot know the path.

### `deploy`

```json
{ "adapter": "fastly", "staging": false, "service_id": "SU1Z0isxPaozGVKXdv0eY", "version": 43 }
```

- For a staged deploy, `version` is the staged version.
- For a production Fastly deploy with a service id, `version` is the active
  version after the deploy, found the same way as today (captured `version=`
  output, then falling back to the API).
- Otherwise `version` is `null`. `service_id` is `null` when none was given.

### `healthcheck`

```json
{
  "adapter": "fastly", "service_id": "SU1Z0isxPaozGVKXdv0eY", "domain": "app.example.com",
  "path": "/", "version": 43, "staging": true, "staging_ip": "151.101.2.10",
  "healthy": true, "status_code": 200, "attempts": 1, "version_verified": false
}
```

- `attempts` is the number of probes actually made.
- `version_verified` is `true` when the check that the version is active ran
  both before and after the probe (production with an API token).
- Unhealthy means `ok: false` with `result` present, `healthy: false`, and
  `error.message` set to today's failure message.
- A failure before the probe (invalid arguments, version not active, staging
  IP lookup) gives `result: null`.

### `rollback`

```json
{ "adapter": "fastly", "service_id": "SU1Z0isxPaozGVKXdv0eY", "staging": false, "version": 43, "rolled_back_to": 42 }
```

- `version` is the value of `--version`: the version rolled back from, or the
  staged version that was deactivated.
- `rolled_back_to` is `null` for a staged rollback.

### `provision`

```json
{
  "adapter": "fastly", "dry_run": false,
  "entries": [
    { "action": "created",
      "store": { "kind": "kv", "logical": "sessions", "platform": "sessions" },
      "message": "created fastly kv-store `sessions` (logical id `sessions`); appended setup tables to fastly.toml" }
  ]
}
```

- `action` is one of `created`, `already_present`, `would_create`, `updated`,
  `would_update`, `not_applicable` or `note`.
- `store` is `null` for adapter-level notes. `store.logical` is `null` for
  stores EdgeZero owns rather than the manifest, such as Fastly's
  `edgezero_runtime_env`.
- `message` is the exact line text mode prints. It may span several lines, and
  it is meant for people, not parsing.

### `config gc`

```json
{
  "adapter": "fastly", "dry_run": true, "older_than_secs": null,
  "store": { "logical": "app_config", "platform": "app_config", "id": "7Ab…" },
  "summary": { "entries": 40, "roots": 3, "referenced_chunks": 12,
               "orphans_planned": 6, "generations_planned": 2,
               "orphans_too_recent": 1, "unprovable": 0 },
  "kept_roots": ["app_config"],
  "planned_deletions": [ { "key": "app_config.__chunk.…", "age_secs": 90211 } ],
  "deleted": null, "failed": [], "stranded": [], "uncertain": [], "warnings": []
}
```

- `older_than_secs` is `null` when `--older-than` was not given.
- `deleted` is `null` on a dry run.
- If any delete failed: `ok: false`, `result` present, and `error.message` set
  to today's diagnostic, including the recovery commands.
- The dry-run advisory text goes to stderr and is not part of `result`.

### `config validate`

```json
{ "mode": "typed", "manifest": "edgezero.toml", "app_config": "app-demo.toml", "app_name": "app-demo", "strict": false }
```

- `mode` is `raw` in the bundled binary and `typed` in template-generated
  CLIs.
- `app_config` is `null` in `raw` mode.
- A validation failure gives `ok: false` and `result: null`, with the first
  failing check in `error.message`. Validation stops at the first failure
  today.
- A later, additive change may add a `findings` array.

## 9. Testing

The rule is that JSON tests parse the output and assert on its structure,
never on string matches (#383), and text tests assert exact bytes.

1. **Wire and envelope unit tests** (`edgezero-cli/src/output.rs`). Each result
   type is serialized to `serde_json::Value` and compared whole. Envelopes are
   checked against the §6.1 invariants: all five keys present, `ok` matches
   whether `error` is `null`, and a success carries a result. The partial result
   survives a failure, command names match their spelling, and `OutputScope`
   routes only while it is alive.
2. **Text is byte-identical to `main`.** Because text output is not
   re-rendered (§7.1), this is verified by running the branch and `main`
   binaries through the same 21 hermetic scenarios and diffing stdout, stderr
   and exit code. The scenarios cover success and failure paths for build,
   auth, provision, validate, deploy, healthcheck (fake `curl`), active-version,
   rollback, gc, a usage error, and the stub. One case is pinned permanently as
   a test: `healthcheck_text_bytes_match_the_line_contract` asserts the exact
   healthcheck bytes the CI action parses.
3. **End-to-end stream tests** (`edgezero-cli/tests/format_json.rs`). These run
   `env!("CARGO_BIN_EXE_edgezero")` in a temp project and need no network or
   credentials. Each asserts that stdout parses as exactly one JSON value:
   - `build --format json`, where a manifest command writes to stdout: its
     output appears only on stderr;
   - `auth status --format json` with a failing manifest probe: exit 1, and the
     envelope carries the `unauthenticated` partial result;
   - `provision` and `config validate`, with the parsed structure asserted;
   - `healthcheck --format json` against a fake `curl` answering 503: exit 1,
     the full partial result is present, and the line contract is on stderr;
   - text-mode `build` output is unchanged, a usage error leaves stdout empty,
     and the bundled stub still absorbs `--format json`.
4. **Argument parsing** (`args.rs`). Every in-scope command defaults to `Text`
   and accepts `--format json` and `--format=text`. `unified` and `structured`
   are rejected. `build --adapter fastly --format json --release` parses the
   format and forwards `--release`.
5. **Adapter tests.** A Fastly healthcheck test against a fake `curl` asserts
   the whole `HealthcheckOutcome` for both healthy and unhealthy probes. A
   Cloudflare provision test asserts each entry's `action` and store. The
   existing provision tests keep their text assertions through
   `ProvisionReport::into_messages()`. The existing gc tests run unchanged,
   because their harness applies the same failure-diagnostic-to-`Err` mapping
   the CLI uses. `native_auth_status` and `process::status` have unit tests.
6. **Lint gate.** `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   passes with the new `disallowed-methods` entries, and so does every clippy
   variant in `format.yml`, including the wasm matrix.

## 10. Documentation

In `docs/guide/cli-reference.md`:

- Add `--format <text|json>` to the flag list of each in-scope command.
- Add a new section, "Machine-readable output". It covers:
  - the envelope (§6.1)
  - streams (§6.2)
  - exit codes, including "empty stdout means no envelope" (§6.4)
  - the compatibility policy (§6.5)
  - one schema table per command (§8)
  - the `config diff` exception (§6.6)
  - a `jq` example such as `edgezero healthcheck … --format json | jq -e '.ok'`
  - a schema changelog starting at version 1
- Fix the `config gc` / `config validate` descriptions if they say output goes
  to stderr.

## 11. Alternatives considered

| Alternative | Why not |
| --- | --- |
| A `--json` boolean flag | Ruled out by #383, and it can't grow into other formats. |
| Reusing or extending `DiffFormat` | It would advertise `structured` and `unified` on commands that can't honour them, or change `config diff`'s spelling. |
| A report sink passed to adapters (`report.set("version", 5)`) | Less adapter churn, but the schema ends up loosely typed and spread across four crates, so it can't be versioned or reviewed as one contract. |
| Parsing today's `key=value` log lines in JSON mode | Provision and gc output is prose, the log is global and can't be captured per call, and the contract would stay implicit. |
| `Serialize` on the adapter domain types | It adds `serde` to `edgezero-adapter` and ties the public schema to internal type layout. |
| Writing the envelope in `main` | Existing downstream CLIs would never get failure envelopes without regenerating `main.rs`. |
| An explicit child-stdout parameter on every spawn helper | Large churn, and it doesn't stop a new direct `Command::status()`. The lint plus a single spawning API does. |
| Silencing `log::info!` under JSON | CI logs would lose progress and remediation hints. |
| NDJSON event stream | Harder to gate on, and #383 asks for results. It could be added later as another `OutputFormat` value. |
| A schema version per command | More numbers to track for little benefit. The whole surface is released together. |

## 12. Risks

| Risk | Mitigation |
| --- | --- |
| Text output drifts, breaking CI actions | Text output is not re-rendered (§7.1). A 21-scenario byte diff against `main`, plus a permanent healthcheck byte test (§9.2). |
| A missed stdout writer corrupts JSON | The `print_stdout` deny, the `disallowed-methods` spawn lint, and the end-to-end test (§9.3). |
| JSON numbers above 2^53 lose precision in JavaScript consumers | Fastly service versions are small integers. u64 values are documented as JSON numbers, and none of today's fields comes close. |
| A native CLI reads from or checks stdout as a TTY and behaves differently when stdout is redirected to stderr | Only under `--format json`, which is new and opt-in. Interactive `auth login` is out of scope. |
| One large PR | Changes are grouped by crate, with the trait change mechanical across four adapters. The plan orders the commits so that each one compiles and passes its tests. |

## 13. Revision notes

Revision 2 records what changed during implementation. None of it changes the
contract in §5, §6 or §8.

- **The spawn policy lives in `edgezero_adapter::process`, not `cli_support`.**
  `cli_support` is gated behind the adapter crate's `cli` feature, which
  `edgezero-cli` does not enable (§7.4).
- **There is no separate text renderer.** Commands keep logging their existing
  lines in both formats, and JSON mode redirects the logger (§7.1). This removes
  a second rendering path that could drift.
- **Only data lines moved to the CLI** (§7.2). Prose lines, such as the
  staged-rollback message, stay in the adapter.
- **`AuthStatus` carries a `failure` message**, so an unauthenticated status
  keeps its exact error text.
- **The enums mapped to the wire are exhaustive**, not `#[non_exhaustive]`
  (§7.2).
- **Wire types live in a single `output.rs`**, not an `output::wire` submodule.

