# PR #388 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Status: retroactive record.** This plan records changes that were already
> applied to the working tree of `spec/383-cli-format-json` on 2026-10-05, in
> response to aram356's "changes requested" review of
> [#388](https://github.com/stackpop/edgezero/pull/388). Every step is checked
> because it has been done and verified. The commit steps describe how to split
> the working tree into reviewable commits; none has been made yet.

**Goal:** Fix the `--format json` results that contradicted the documented
schema before schema version 1 ships, and close the review's lint, docs and test
gaps.

**Architecture:** Adapters return typed outcomes (`edgezero-adapter/src/registry.rs`),
the CLI maps them onto wire structs (`edgezero-cli/src/output.rs`), and
`output::finish` writes the envelope. The fixes correct the outcome producers
(Fastly, Spin adapters), the CLI mapping (`lib.rs`, `config.rs`), and the
outcome types themselves, so that each fact is stored once.

**Tech Stack:** Rust 1.95.0 (edition 2024), clap, serde/serde_json, clippy
`disallowed-methods`, VitePress docs with Prettier.

**Spec:** `docs/superpowers/specs/2026-09-25-cli-format-json-design.md`
(§6 output contract, §8 per-command schemas, §13 "Revision 3" lists these
changes).

## Global Constraints

- Run every cargo command with `cargo +1.95.0` (the `.tool-versions` pin). The
  default toolchain on this machine reports false clippy failures.
- `--format text` output must stay byte-identical, except that `build` now
  rejects a `--format` after a passthrough argument.
- No `http` crate imports, no Tokio in core or adapter crates.
- Tests must not need a network or platform credentials. Fake `curl` / `fastly`
  binaries go on `PATH`.
- No `Co-Authored-By` trailers or AI bylines in commits.
- CI gates: `cargo fmt --all -- --check`;
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
  `cargo test --workspace --all-targets`;
  `cargo test -p edgezero-adapter-fastly --all-targets --features cli`;
  `cargo check --workspace --all-targets --features "fastly cloudflare spin"`;
  `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`;
  the wasm clippy matrix in `.github/workflows/format.yml`;
  `cargo test -p edgezero-cli --test generated_project_builds -- --ignored`;
  `npm run lint` and `npm run format` in `docs/`.

## Review item → task map

| Review comment                                         | Severity | Task |
| ------------------------------------------------------ | -------- | ---- |
| `version_verified: true` on an unhealthy probe         | blocking | 2    |
| `build` swallows a trailing `--format json`            | blocking | 4    |
| `config gc --yes` with nothing to reclaim → `null`     | blocking | 3    |
| Spec approval link                                     | question | (author answers on the PR; no code) |
| Raw `config validate` reports `app_config: null`       | 🤔       | 5    |
| Provision dry-run / real-run actions don't pair        | 🤔       | 7, 11 |
| Deploy went live, version unknown → `result: null`     | 🤔       | 6    |
| `deploy.service_id` reflects only the flag             | 🤔       | 6    |
| Outcome types can hold contradictory values            | ♻️       | 1    |
| `AuthSub::Status`, `OutputFormat ==`, bare block, spec `--require-active` | ⛏ | 8, 11 |
| Docs: nullability, exit code 2, `--help`, logger       | 🤔       | 11   |
| Test gaps                                              | 🤔       | 2, 3, 4, 5, 6, 10 |
| Lint gaps (`io::stdout`, template `clippy.toml`)       | 🌱       | 9    |
| Closed stderr loses the envelope                       | 🌱       | 9    |
| `wrangler whoami` exit code                            | out of scope | not addressed |

## File map

| File | Responsibility | Tasks |
| ---- | -------------- | ----- |
| `crates/edgezero-adapter/src/registry.rs` | Typed outcomes adapters return | 1, 6 |
| `crates/edgezero-adapter/src/cli_support.rs` | `native_auth_status`, `run_native_cli` | 1 |
| `crates/edgezero-adapter-axum/src/cli.rs` | axum `AuthStatus` outcome | 1 |
| `crates/edgezero-adapter-fastly/src/cli.rs` | Fastly healthcheck, gc, deploy, provision | 1, 2, 3, 6, 8 |
| `crates/edgezero-adapter-spin/src/cli.rs` | Spin provision | 7 |
| `crates/edgezero-cli/src/adapter.rs` | Manifest-command dispatch, tee | 1, 9 |
| `crates/edgezero-cli/src/auth.rs` | `auth status` mapping | 1 |
| `crates/edgezero-cli/src/output.rs` | Wire schema, `OutputScope`, `finish` | 1, 3, 5, 8, 9 |
| `crates/edgezero-cli/src/lib.rs` | `build`, `deploy`, healthcheck, logger | 1, 4, 6, 9 |
| `crates/edgezero-cli/src/config.rs` | `config validate`, `config gc` | 1, 5, 9 |
| `crates/edgezero-cli/src/args.rs` | clap argument types | 8 |
| `crates/edgezero-cli/tests/format_json.rs` | End-to-end stream tests | 4, 5, 6, 10 |
| `clippy.toml`, `crates/edgezero-cli/src/templates/root/clippy.toml.hbs` | stdout lint guard | 9 |
| `docs/guide/cli-reference.md`, the spec | Public contract | 11 |

---

### Task 1: Store each outcome fact once

The review found `HealthcheckOutcome { failure, healthy }`,
`AuthStatusOutcome { failure, state }` and
`GcReport { failed, failure_diagnostic, stranded, uncertain }` each record one
fact twice, kept in sync only by doc comments. This task makes the invalid
combinations unrepresentable. It is a pure refactor: JSON and text output do
not change, so the existing tests are the safety net. It goes first because
Tasks 2 and 3 build on the new shapes.

**Files:**
- Modify: `crates/edgezero-adapter/src/registry.rs` (`AuthStatusOutcome`, `AuthState`, `HealthcheckOutcome`, `GcReport`, new `GcFailure`)
- Modify: `crates/edgezero-adapter/src/cli_support.rs` (`run_native_cli`, `native_auth_status`, its test)
- Modify: `crates/edgezero-adapter-axum/src/cli.rs` (`AuthStatus` arm)
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` (`healthcheck`, `gc_report_for`, `gc_fastly_config_store`, tests)
- Modify: `crates/edgezero-cli/src/adapter.rs` (`AuthStatus` arm of the manifest dispatch)
- Modify: `crates/edgezero-cli/src/auth.rs` (`auth_status`)
- Modify: `crates/edgezero-cli/src/output.rs` (`AuthStatusResult::new`, `HealthcheckResult::new`, `GcResult::new`, tests)
- Modify: `crates/edgezero-cli/src/config.rs` (`config_gc` failure mapping)
- Modify: `crates/edgezero-cli/src/lib.rs` (`healthy=` line)

**Interfaces:**
- Produces:
  - `pub struct AuthStatusOutcome { pub state: AuthState }`
  - `pub enum AuthState { Authenticated, NotApplicable, Unauthenticated { reason: String } }` (no longer `Copy`)
  - `HealthcheckOutcome` loses `healthy: bool`; gains `pub const fn healthy(&self) -> bool`
  - `GcReport { ..., failure: Option<GcFailure>, ... }` replaces `failed`, `failure_diagnostic`, `stranded`, `uncertain`
  - `pub struct GcFailure { pub diagnostic: String, pub failed: Vec<String>, pub stranded: Vec<String>, pub uncertain: Vec<String> }` (`Clone, Debug, Default, Eq, PartialEq`)
  - `AuthStatusResult::new(adapter: &str, state: &AuthState) -> Self`

- [x] **Step 1: Change the types in `registry.rs`**

```rust
/// Result of [`AdapterAction::AuthStatus`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthStatusOutcome {
    pub state: AuthState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthState {
    Authenticated,
    /// The adapter has no remote auth surface (axum).
    NotApplicable,
    Unauthenticated {
        /// Why the session is not authenticated.
        reason: String,
    },
}
```

In `HealthcheckOutcome`, delete `pub healthy: bool`, reword the `failure` doc
to `/// \`None\` when healthy; why the probe is unhealthy otherwise.`, and add:

```rust
impl HealthcheckOutcome {
    /// Whether the probe came back healthy.
    #[inline]
    #[must_use]
    pub const fn healthy(&self) -> bool {
        self.failure.is_none()
    }
}
```

In `GcReport`, replace `failed`, `failure_diagnostic`, `stranded` and
`uncertain` with `/// \`None\` unless a delete failed.\npub failure: Option<GcFailure>,`
and add:

```rust
/// The deletes a `config gc` run could not complete.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcFailure {
    /// The operator-facing report and recovery text.
    pub diagnostic: String,
    /// Keys whose delete failed (never empty).
    pub failed: Vec<String>,
    /// Keys left in a generation that lost a sibling to a confirmed delete.
    pub stranded: Vec<String>,
    /// Keys whose generation's only failure has an unknown outcome.
    pub uncertain: Vec<String>,
}
```

- [x] **Step 2: Update the auth producers**

`cli_support.rs`:

```rust
pub fn run_native_cli(program: &str, args: &[&str], install_hint: &str) -> Result<(), String> {
    match native_auth_status(program, args, install_hint)?.state {
        AuthState::Unauthenticated { reason } => Err(reason),
        AuthState::Authenticated | AuthState::NotApplicable => Ok(()),
    }
}
// in native_auth_status:
    Ok(if status.success() {
        AuthStatusOutcome {
            state: AuthState::Authenticated,
        }
    } else {
        AuthStatusOutcome {
            state: AuthState::Unauthenticated {
                reason: format!("`{program} {}` exited with status {status}", args.join(" ")),
            },
        }
    })
```

axum: drop `failure: None,` from its `AuthStatusOutcome`. `edgezero-cli/src/adapter.rs`:

```rust
        (Action::AuthStatus, failure) => Ok(ActionOutcome::AuthStatus(AuthStatusOutcome {
            state: failure.map_or(AuthState::Authenticated, |reason| {
                AuthState::Unauthenticated { reason }
            }),
        })),
```

- [x] **Step 3: Update the auth consumers**

`auth.rs`:

```rust
    let result = AuthStatusResult::new(adapter_name, &status.state);
    match status.state {
        AuthState::Unauthenticated { reason } => Err(Failure::with_result(reason, result)),
        AuthState::Authenticated | AuthState::NotApplicable => Ok(result),
    }
```

`output.rs`: `AuthStatusResult::new` takes `state: &AuthState` and matches
`AuthState::Unauthenticated { .. } => WireAuthState::Unauthenticated`.

- [x] **Step 4: Update the healthcheck and gc consumers**

- `output.rs` `HealthcheckResult::new`: `healthy: outcome.healthy(),`
- `lib.rs` healthcheck: `log::info!("healthy={}", outcome.healthy());`
- `output.rs` `GcResult::new`: start with
  `let failure = report.failure.clone().unwrap_or_default();` and fill
  `failed: failure.failed`, `stranded: failure.stranded`,
  `uncertain: failure.uncertain`.
- `config.rs` `config_gc`:

```rust
    if let Some(failure) = report.failure {
        return Err(Failure::with_result(failure.diagnostic, result));
    }
```

- Fastly `gc_fastly_config_store` failure branch:

```rust
    report.failure = Some(GcFailure {
        diagnostic,
        failed,
        stranded,
        uncertain,
    });
    report.text_lines = out;
    Ok(report)
```

  `gc_report_for` drops the four old fields and sets `failure: None`. Add
  `GcFailure` to the Fastly `use edgezero_adapter::registry::{…}` list.

- [x] **Step 5: Update the tests that named the old fields**

- `cli_support.rs` `native_auth_status_maps_exit_status_to_state`:

```rust
        let unauthenticated = native_auth_status("false", &[], "hint").expect("spawns");
        let AuthState::Unauthenticated { reason } = unauthenticated.state else {
            panic!("expected Unauthenticated, got {:?}", unauthenticated.state);
        };
        assert!(reason.contains("exited with status"), "got: {reason}");
```

- Fastly `healthcheck_reports_a_structured_outcome`: drop `healthy: true,` from
  the expected struct; `assert!(!unhealthy.healthy());`.
- Fastly test helper `run_gc`: `match report.failure { Some(failure) => Err(failure.diagnostic), None => Ok(report.text_lines) }`.
- `output.rs`: drop `healthy: false,` from `failure_envelope_keeps_the_partial_result`;
  `AuthStatusResult::new("axum", &AuthState::NotApplicable)`.

- [x] **Step 6: Verify**

Run: `cargo +1.95.0 test --workspace --all-targets && cargo +1.95.0 test -p edgezero-adapter-fastly --features cli`
Expected: all pass, with no expectation changes beyond the field renames above.

- [ ] **Step 7: Commit**

```bash
git add crates/edgezero-adapter/src/registry.rs crates/edgezero-adapter/src/cli_support.rs \
  crates/edgezero-adapter-axum/src/cli.rs crates/edgezero-cli/src/adapter.rs \
  crates/edgezero-cli/src/auth.rs crates/edgezero-cli/src/output.rs \
  crates/edgezero-cli/src/config.rs crates/edgezero-cli/src/lib.rs
git add -p crates/edgezero-adapter-fastly/src/cli.rs   # only the Task 1 hunks
git commit -m "Store each adapter outcome fact once"
```

---

### Task 2: `version_verified` only when both checks ran (blocking)

The "after probing" `verify_version_active` call runs only in the healthy arm,
but the field was `production_token.is_some()`. An unhealthy production probe
with `FASTLY_API_TOKEN` set reported `version_verified: true`.

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` (`healthcheck`, its doc comment, a new test after `healthcheck_reports_a_structured_outcome`)
- Test: `crates/edgezero-cli/tests/format_json.rs` (Task 10 adds the end-to-end pair)

**Interfaces:**
- Consumes: `HealthcheckOutcome::healthy()` (Task 1)
- Produces: `HealthcheckOutcome::version_verified` is `true` only for a healthy production probe with a token.

- [x] **Step 1: Write the failing test**

```rust
    /// `version_verified` claims the version was ACTIVE before AND after the
    /// probe, so it is true only when both API checks actually ran: an
    /// unhealthy probe skips the "after" check and must report `false`.
    #[cfg(unix)]
    #[test]
    fn healthcheck_version_verified_only_when_both_checks_ran() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");
        let args: Vec<String> = [
            "--domain", "app.example.com", "--service-id", "SVC1", "--version", "7",
            "--retry", "1", "--retry-delay", "0",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        // A fake `curl` serving the versions API (`--config -`, version 7
        // active) and answering health probes with `code`; each API call is
        // logged so the test can count the checks.
        let fake_curl_with_api = |code: u16| {
            let dir = tempdir().expect("tempdir");
            let log = dir.path().join("api.log");
            let script = format!(
                "#!/bin/sh\n\
                 case \" $* \" in *\" --config \"*) cat >/dev/null; echo get >> '{log}'; \
                 printf '%s\\n200' '[{{\"number\":7,\"active\":true}}]'; exit 0;; esac\n\
                 echo {code}\n",
                log = log.display()
            );
            let script_path = dir.path().join("curl");
            fs::write(&script_path, script).expect("write script");
            let mut perms = fs::metadata(&script_path).expect("meta").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script_path, perms).expect("chmod +x");
            (dir, log)
        };
        let api_calls =
            |log: &Path| fs::read_to_string(log).unwrap_or_default().lines().count();

        let (unhealthy_curl, unhealthy_log) = fake_curl_with_api(503);
        let unhealthy_path = PathPrepend::new(unhealthy_curl.path());
        let unhealthy = healthcheck(&args).expect("an unhealthy probe is an outcome");
        assert!(!unhealthy.healthy());
        assert!(
            !unhealthy.version_verified,
            "the after-probe check never ran on an unhealthy probe"
        );
        assert_eq!(api_calls(&unhealthy_log), 1, "only the before-probe check");
        drop(unhealthy_path);

        let (healthy_curl, healthy_log) = fake_curl_with_api(200);
        let _path = PathPrepend::new(healthy_curl.path());
        let healthy = healthcheck(&args).expect("probe runs");
        assert!(healthy.healthy());
        assert!(healthy.version_verified, "both checks ran and passed");
        assert_eq!(api_calls(&healthy_log), 2, "before and after the probe");
    }
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo +1.95.0 test -p edgezero-adapter-fastly --features cli -- healthcheck_version_verified`
Expected: FAIL at `!unhealthy.version_verified` (verified against the old code).

- [x] **Step 3: Implement**

```rust
    // `version_verified` means BOTH checks ran, so it is true only on the
    // healthy arm, the one that runs the "after probing" check.
    let (version_verified, status_code, failure) = match outcome {
        Ok(code) => {
            if let Some(token) = production_token.as_deref() {
                verify_version_active(&service_id, version, token, "after probing")?;
            }
            (production_token.is_some(), Some(code), None)
        }
        Err((last_code, msg)) => (
            false,
            last_code,
            Some(format!(
                "healthcheck for {domain} failed after {} attempt(s): {msg}",
                retry.max(1)
            )),
        ),
    };
```

and `version_verified,` in the returned struct. Fix the stale function doc:

```rust
/// to `--retry` times. An unhealthy probe is still `Ok`, with `failure`
/// set; the CLI emits `status-code` / `healthy` and turns that into a
/// non-zero exit.
```

- [x] **Step 4: Run it to verify it passes**

Run: `cargo +1.95.0 test -p edgezero-adapter-fastly --features cli -- healthcheck`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -p crates/edgezero-adapter-fastly/src/cli.rs   # Task 2 hunks
git commit -m "Report version_verified only when both active checks ran"
```

---

### Task 3: `config gc --yes` with nothing to reclaim reports `deleted: 0` (blocking)

`gc_report_for` started every report at `deleted: None`, and the
`doomed_count == 0` early return came before `report.deleted = Some(deleted)`.
A real run against a store without orphans reported `deleted: null`, which the
docs define as a dry run.

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` (`gc_report_for`, its call site, gc test helpers, two new tests)
- Modify: `crates/edgezero-cli/src/output.rs` (new `gc_failure_fills_the_partial_result` test)

**Interfaces:**
- Consumes: `GcReport::failure`, `GcFailure` (Task 1)
- Produces: `fn gc_report_for(plan: &GcPlan, entries: usize, store_id: &str, dry_run: bool) -> GcReport`; test helper `fn gc_report(dir: &Path, older_than_secs: u64, dry_run: bool) -> Result<GcReport, String>`

- [x] **Step 1: Split the test helper**

```rust
    #[cfg(unix)]
    fn gc_report(dir: &Path, older_than_secs: u64, dry_run: bool) -> Result<GcReport, String> {
        FastlyCliAdapter.gc_config_entries(
            dir,
            None,
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            &AdapterPushContext::new(),
            older_than_secs,
            dry_run,
        )
    }

    #[cfg(unix)]
    fn run_gc(dir: &Path, older_than_secs: u64, dry_run: bool) -> Result<Vec<String>, String> {
        // Mirror the CLI: a report carrying a failure is an error.
        let report = gc_report(dir, older_than_secs, dry_run)?;
        match report.failure {
            Some(failure) => Err(failure.diagnostic),
            None => Ok(report.text_lines),
        }
    }
```

- [x] **Step 2: Write the failing tests**

```rust
    /// `deleted` is `None` only on a dry run: a real run that finds nothing to
    /// reclaim deleted zero entries, and says so.
    #[cfg(unix)]
    #[test]
    fn gc_report_deleted_is_zero_for_a_real_run_with_nothing_to_reclaim() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");
        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let real = gc_report(dir.path(), 86_400, false).expect("gc runs");
        assert_eq!(real.deleted, Some(0));
        assert_eq!(real.failure, None);
        assert!(real.planned.is_empty());

        let preview = gc_report(dir.path(), 86_400, true).expect("gc runs");
        assert_eq!(preview.deleted, None, "a dry run deletes nothing at all");
    }

    /// A failed delete is still a report: `failure` names the failed key, and
    /// the keys whose outcome is unknown, alongside the recovery text.
    #[cfg(unix)]
    #[test]
    fn gc_report_carries_a_failed_delete() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");
        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID, &[], &listing, Some(&dead_chunks[0]), false, &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let report = gc_report(dir.path(), 86_400, false).expect("a failed delete is a report");
        assert_eq!(report.deleted, Some(0));
        assert_eq!(report.planned.len(), dead_chunks.len());
        let failure = report.failure.expect("the failed delete is reported");
        assert_eq!(failure.failed, vec![dead_chunks[0].clone()]);
        assert!(failure.stranded.is_empty(), "nothing was deleted before the failure");
        assert!(!failure.uncertain.is_empty(), "the failed first delete has an unknown outcome");
        assert!(failure.diagnostic.contains("unknown outcome"), "{}", failure.diagnostic);
    }
```

In `output.rs` tests (add `GcFailure` to the test module's registry import):

```rust
    #[test]
    fn gc_failure_fills_the_partial_result() {
        let report = GcReport {
            deleted: Some(1),
            failure: Some(GcFailure {
                diagnostic: "config gc: 1 of 3 deletes FAILED".to_owned(),
                failed: vec!["c.2".to_owned()],
                stranded: vec!["c.3".to_owned()],
                uncertain: vec!["d.1".to_owned()],
            }),
            store_id: Some("7Ab".to_owned()),
            ..GcReport::default()
        };
        let store = ResolvedStoreId::from_logical("app_config");
        let result = GcResult::new("fastly", &store, false, Some(3_600), &report);
        let failed: Outcome<GcResult> = Err(Failure::with_result(
            report.failure.expect("failure").diagnostic,
            result,
        ));
        let envelope = envelope_of(CommandName::ConfigGc, &failed);
        assert_envelope_invariants(&envelope);
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["error"], json!({"message": "config gc: 1 of 3 deletes FAILED"}));
        let gc = &envelope["result"];
        assert_eq!(gc["deleted"], json!(1));
        assert_eq!(gc["failed"], json!(["c.2"]));
        assert_eq!(gc["stranded"], json!(["c.3"]));
        assert_eq!(gc["uncertain"], json!(["d.1"]));
        assert_eq!(gc["older_than_secs"], json!(3_600));
    }
```

- [x] **Step 3: Run them to verify the first fails**

Run: `cargo +1.95.0 test -p edgezero-adapter-fastly --features cli -- gc_report_`
Expected: `gc_report_deleted_is_zero_for_a_real_run_with_nothing_to_reclaim` FAILS (`left: None, right: Some(0)`); `gc_report_carries_a_failed_delete` passes, since it pins existing behavior.

- [x] **Step 4: Implement**

```rust
/// The typed `config gc` report for `plan`, before any delete runs.
fn gc_report_for(plan: &GcPlan, entries: usize, store_id: &str, dry_run: bool) -> GcReport {
    GcReport {
        // A real run that finds nothing to reclaim deleted zero entries; only a
        // dry run reports `None`.
        deleted: (!dry_run).then_some(0),
        entries,
        failure: None,
        // ... remaining fields unchanged
    }
}
```

Call site: `let mut report = gc_report_for(&plan, items.len(), &resolved_id, dry_run);`

- [x] **Step 5: Run to verify they pass**

Run: `cargo +1.95.0 test -p edgezero-adapter-fastly --features cli -- gc_ && cargo +1.95.0 test -p edgezero-cli --lib output::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -p crates/edgezero-adapter-fastly/src/cli.rs crates/edgezero-cli/src/output.rs
git commit -m "Report deleted: 0 for a real config gc run with nothing to reclaim"
```

---

### Task 4: `build` rejects a `--format` after passthrough args (blocking)

`BuildArgs::adapter_args` uses `trailing_var_arg = true`, so
`edgezero build --adapter axum --release --format json` forwarded
`--format json` to the build command, printed text, and exited 0.

**Files:**
- Modify: `crates/edgezero-cli/src/lib.rs` (`build`)
- Test: `crates/edgezero-cli/tests/format_json.rs`

**Interfaces:**
- Consumes: none
- Produces: `build` returns `Err` (exit 1, empty stdout) when `adapter_args` contains `--format` or `--format=*`.

- [x] **Step 1: Write the failing test**

```rust
    #[test]
    fn json_build_rejects_a_format_after_passthrough_args() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["build", "--adapter", "axum", "--release", "--format", "json"],
            None,
        );
        assert_eq!(output.status.code(), Some(1_i32));
        // The late `--format` was never read, so this is a text-mode failure:
        // nothing on stdout, and the build command never ran.
        assert!(output.stdout.is_empty(), "stdout: {:?}", stdout_of(&output));
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("`--format` after a passthrough argument"),
            "stderr: {stderr}"
        );
        assert!(!stderr.contains("child-wrote-to-stdout"), "stderr: {stderr}");
    }
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo +1.95.0 test -p edgezero-cli --test format_json json_build_rejects`
Expected: FAIL, exit code `Some(0)`.

- [x] **Step 3: Implement** (first statement of `fn build`, before the manifest loads, so the check is hermetic)

```rust
    // `trailing_var_arg` captures everything after the first passthrough token,
    // so a late `--format json` would be forwarded to the build command and the
    // run would silently print text. Fail closed instead.
    if let Some(flag) = args
        .adapter_args
        .iter()
        .find(|arg| *arg == "--format" || arg.starts_with("--format="))
    {
        return Err(format!(
            "`{flag}` after a passthrough argument is forwarded to the build command, not read \
             as EdgeZero's `--format`. Put `--format` before the first passthrough argument."
        )
        .into());
    }
```

`deploy -- --format=json` is deliberately left alone: the explicit `--` makes
the intent clear.

- [x] **Step 4: Run it to verify it passes**

Run: `cargo +1.95.0 test -p edgezero-cli --test format_json`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-cli/src/lib.rs crates/edgezero-cli/tests/format_json.rs
git commit -m "Reject a build --format that follows passthrough args"
```

---

### Task 5: `config validate` always reports `app_config`

Both flows load and require `<app_name>.toml`, but raw mode reported
`app_config: null` and the mode was inferred from that null.

**Files:**
- Modify: `crates/edgezero-cli/src/output.rs` (`ValidateResult`, `ValidateMode`, unit test)
- Modify: `crates/edgezero-cli/src/config.rs` (`validate_raw`, `validate_typed`, import)
- Test: `crates/edgezero-cli/tests/format_json.rs`

**Interfaces:**
- Produces: `ValidateResult::new(manifest: &Path, app_config: &Path, app_name: &str, strict: bool, mode: ValidateMode) -> Self`; `pub(crate) enum ValidateMode { Raw, Typed }`; wire key `app_config: String` (non-null; compatible under spec §6.5).

- [x] **Step 1: Update the tests first**

`format_json.rs` `json_config_validate_reports_the_validated_inputs`: expect
`"app_config": "demo-app.toml"`. Add:

```rust
    #[test]
    fn json_config_validate_failure_has_no_result() {
        // No `demo-app.toml`: the raw validator requires it.
        let dir = project();
        let output = edgezero(dir.path(), &["config", "validate", "--format", "json"], None);
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["ok"], json!(false));
        assert!(envelope["result"].is_null(), "{envelope}");
        let message = envelope["error"]["message"].as_str().expect("error message");
        assert!(message.contains("demo-app.toml"), "{message}");
    }
```

`output.rs`: rename the unit test to
`validate_result_reports_the_app_config_in_both_modes`, pass
`Path::new("demo.toml")` and `ValidateMode::Raw` / `ValidateMode::Typed`, and
expect `"app_config": "demo.toml"` for raw.

- [x] **Step 2: Run to verify the e2e test fails**

Run: `cargo +1.95.0 test -p edgezero-cli --test format_json json_config_validate`
Expected: FAIL, `left: … "app_config": String("demo-app.toml") … right: … Null`
(after Step 3 compiles), or a compile error in `output.rs` before it.

- [x] **Step 3: Implement**

```rust
pub(crate) struct ValidateResult {
    app_config: String,
    app_name: String,
    manifest: String,
    mode: ValidateMode,
    strict: bool,
}

impl ValidateResult {
    pub(crate) fn new(
        manifest: &Path,
        app_config: &Path,
        app_name: &str,
        strict: bool,
        mode: ValidateMode,
    ) -> Self {
        Self {
            app_config: path_string(app_config),
            app_name: app_name.to_owned(),
            manifest: path_string(manifest),
            mode,
            strict,
        }
    }
}

/// Which `config validate` flow ran.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidateMode {
    Raw,
    Typed,
}
```

`config.rs`: both flows call
`ValidateResult::new(&args.manifest, &ctx.app_config_path, &ctx.app_name, args.strict, ValidateMode::Raw)`
(`ValidateMode::Typed` in `validate_typed`), and the `use crate::output::{…}`
list gains `ValidateMode`.

- [x] **Step 4: Run to verify it passes**

Run: `cargo +1.95.0 test -p edgezero-cli`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-cli/src/output.rs crates/edgezero-cli/src/config.rs crates/edgezero-cli/tests/format_json.rs
git commit -m "Always report app_config from config validate"
```

---

### Task 6: `deploy` keeps its result when it went live, and reports the resolved service id

Two review items in one function: (a) when a Fastly production deploy succeeded
but neither the captured output nor the API fallback resolved the version, `?`
turned it into `result: null`, the same shape as "never ran"; (b)
`service_id` echoed `--service-id`, so a staged deploy using
`FASTLY_SERVICE_ID` reported `null`.

**Files:**
- Modify: `crates/edgezero-adapter/src/registry.rs` (`DeployOutcome`)
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` (`Deploy` / `DeployStaging` arms)
- Modify: `crates/edgezero-cli/src/lib.rs` (`deploy`, `fastly_production_deploy`, `deployed_version` → `deployed`)
- Test: `crates/edgezero-cli/tests/format_json.rs`

**Interfaces:**
- Consumes: `Failure::with_result` (`output.rs`)
- Produces:
  - `DeployOutcome { pub service_id: Option<String>, pub version: Option<u64> }`
  - `fn fastly_production_deploy(..) -> Result<Result<u64, String>, String>`: outer `Err` = deploy failed; inner `Err` = deployed, version unresolved
  - `fn deployed(outcome: ActionOutcome) -> (Option<String>, Option<u64>)`

- [x] **Step 1: Write the failing tests** (uses `FASTLY_MANIFEST`, `DEPLOY` and `project_with` from Task 10 Step 1)

```rust
    #[test]
    fn deploy_reads_the_version_from_the_captured_output() {
        let dir = project_with(FASTLY_MANIFEST);
        let text = edgezero(dir.path(), &DEPLOY, None);
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        let stdout = stdout_of(&text);
        // The manifest command receives `--service-id SVC1` as passthrough.
        assert!(
            stdout.ends_with(
                "SUCCESS: Deployed package (service SVC1, version 12) --service-id SVC1\n\
                 version=12\n"
            ),
            "the tee echoes the child live, then the version line: {stdout:?}"
        );

        let mut args = DEPLOY.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero(dir.path(), &args, None);
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "staging": false, "version": 12_i32})
        );
        // The tee sends the captured child's stdout to stderr under JSON.
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("SUCCESS: Deployed package (service SVC1, version 12) --service-id"),
            "stderr: {stderr}"
        );
        assert!(stderr.contains("version=12\n"), "stderr: {stderr}");
    }

    #[test]
    fn deploy_that_went_live_without_a_version_keeps_its_result() {
        let manifest = FASTLY_MANIFEST.replace(
            "echo 'SUCCESS: Deployed package (service SVC1, version 12)'",
            "echo deployed",
        );
        let dir = project_with(&manifest);
        let mut args = DEPLOY.to_vec();
        args.extend(["--format", "json"]);
        // No token: the API fallback cannot resolve the version either.
        let output = edgezero(dir.path(), &args, None);
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(
            envelope["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "staging": false, "version": null}),
            "the deploy went live, so the result is present"
        );
        let message = envelope["error"]["message"].as_str().expect("error message");
        assert!(message.starts_with("deploy succeeded but"), "{message}");
    }
```

- [x] **Step 2: Run to verify the second fails**

Run: `cargo +1.95.0 test -p edgezero-cli --test format_json deploy_`
Expected: `deploy_that_went_live_without_a_version_keeps_its_result` FAILS (`result` is `null`); the first passes and pins the tee path.

- [x] **Step 3: Add `service_id` to `DeployOutcome`**

```rust
pub struct DeployOutcome {
    /// The service the deploy targeted, when the adapter resolved one.
    pub service_id: Option<String>,
    /// The staged (or activated) platform version, when known.
    pub version: Option<u64>,
}
```

Fastly arms:

```rust
            AdapterAction::Deploy => deploy(args).map(|()| {
                ActionOutcome::Deploy(DeployOutcome {
                    service_id: None,
                    version: None,
                })
            }),
            // ...
            AdapterAction::DeployStaging => deploy_staging(args).map(|version| {
                ActionOutcome::Deploy(DeployOutcome {
                    // `deploy_staging` already resolved this (flag, then
                    // `FASTLY_SERVICE_ID`), so it cannot fail here.
                    service_id: resolve_service_id(args).ok(),
                    version: Some(version),
                })
            }),
```

- [x] **Step 4: Rework `deploy` in `lib.rs`**

```rust
    // `service_id`: the id the adapter resolved, else `--service-id` as passed.
    let result = |service_id: Option<String>, version: Option<u64>| {
        DeployResult::new(
            &args.adapter,
            service_id.or_else(|| args.service_id.clone()),
            args.staging,
            version,
        )
    };
```

Staged branch: `let (service_id, version) = deployed(outcome); … return Ok(result(service_id, version));`

Production Fastly branch:

```rust
    if args.service_id.is_some() && args.adapter.eq_ignore_ascii_case("fastly") {
        return match fastly_production_deploy(&args.adapter, manifest.as_ref(), &passthrough)? {
            Ok(version) => {
                log::info!("version={version}");
                Ok(result(None, Some(version)))
            }
            Err(unresolved) => Err(Failure::with_result(unresolved, result(None, None))),
        };
    }
```

Fallthrough: `let (service_id, version) = deployed(outcome); Ok(result(service_id, version))`.

`fastly_production_deploy` (doc: "The outer `Err` means the deploy itself
failed. The inner `Err` means the deploy went live but the activated version
could not be resolved."): `return Ok(Ok(version));` on a parsed version, and end
with:

```rust
    // `--require-active` makes EmitVersion fail rather than report none.
    Ok(
        match adapter::execute(
            adapter_name,
            adapter::Action::EmitVersion,
            manifest,
            &emit_args,
        ) {
            Ok(outcome) => active_version(&outcome)
                .ok_or_else(|| unresolved("no active version was reported")),
            Err(err) => Err(unresolved(&err)),
        },
    )
```

Replace `deployed_version`:

```rust
/// The service id and version a deploy outcome reports, if any.
#[cfg(feature = "cli")]
fn deployed(outcome: ActionOutcome) -> (Option<String>, Option<u64>) {
    if let ActionOutcome::Deploy(deployed) = outcome {
        (deployed.service_id, deployed.version)
    } else {
        (None, None)
    }
}
```

The text-mode error message is unchanged: `Failure`'s message is the same
`unresolved(..)` string.

- [x] **Step 5: Run to verify**

Run: `cargo +1.95.0 test -p edgezero-cli && cargo +1.95.0 clippy -p edgezero-cli --all-targets --all-features -- -D warnings`
Expected: PASS. (A `match outcome { Ok(outcome) => … }` form trips
`clippy::shadow_reuse`; the inline `match adapter::execute(..)` avoids it.)

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-adapter/src/registry.rs crates/edgezero-cli/src/lib.rs
git add -p crates/edgezero-adapter-fastly/src/cli.rs crates/edgezero-cli/tests/format_json.rs
git commit -m "Keep the deploy result when the version is unresolved; report the resolved service id"
```

---

### Task 7: Spin reports `created` for an added label

The Spin dry run reports `would_create`, but adding the label reported
`updated`, so a consumer couldn't pair a plan with its outcome. The text line is
unchanged.

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/cli.rs` (`provision` add branch; `provision_writes_kv_labels_into_resolved_component` test)

**Interfaces:**
- Produces: Spin real run → `ProvisionAction::Created` (added) or `AlreadyPresent`.

- [x] **Step 1: Write the failing assertion**

```rust
        let report = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("real run succeeds");
        // Pairs with the dry run's `WouldCreate`.
        assert_eq!(report.entries[0].action, ProvisionAction::Created);
        let out = report.into_messages();
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("added KV label `sessions`"), "got: {out:?}");
```

- [x] **Step 2: Run to verify it fails**

Run: `cargo +1.95.0 test -p edgezero-adapter-spin --features cli provision_writes_kv_labels`
Expected: FAIL, `left: Updated, right: Created`.

- [x] **Step 3: Implement**

In the `if added {` branch, change `ProvisionAction::Updated` to `ProvisionAction::Created`.

- [x] **Step 4: Run to verify it passes**

Same command. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-adapter-spin/src/cli.rs
git commit -m "Report created for a Spin label provision adds"
```

(The Fastly dry run also reports `would_create` before checking
`setup_block_present`. Making it check would change text output, so the review
agreed to document it instead; see Task 11.)

---

### Task 8: Review nits

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs` (`AuthSub::Status`)
- Modify: `crates/edgezero-cli/src/output.rs` (`OutputScope::enter`, `finish`)
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` (`provision_runtime_env_store`)

**Interfaces:** no signature changes.

- [x] **Step 1: `#[non_exhaustive]` on the variant that gained `format`**

```rust
    /// Show the current session (`wrangler whoami` / `fastly profile
    /// list` / `spin cloud info`).
    #[non_exhaustive]
    Status {
```

- [x] **Step 2: Exhaustive `match` on `OutputFormat`** (it is `#[non_exhaustive]`, but an in-crate `match` is still checked, so a future `Ndjson` fails to compile here instead of silently acting like `text`)

```rust
        let previous = match format {
            OutputFormat::Text => None,
            OutputFormat::Json => Some((
                process::set_child_stdout_to_stderr(true),
                crate::set_info_to_stderr(true),
            )),
        };
```

and in `finish`: `match format { OutputFormat::Text => {} OutputFormat::Json => { … } }`
around the existing envelope write.

- [x] **Step 3: Flatten the leftover bare block**

In `provision_runtime_env_store`, remove the `{` … `}` that wraps everything
after the `setup_block_present` early return, and dedent its body by one level.
No logic changes.

- [x] **Step 4: Verify**

Run: `cargo +1.95.0 fmt --all && cargo +1.95.0 clippy --workspace --all-targets --all-features -- -D warnings && cargo +1.95.0 test -p edgezero-cli`
Expected: clean, PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-cli/src/args.rs
git add -p crates/edgezero-cli/src/output.rs crates/edgezero-adapter-fastly/src/cli.rs
git commit -m "Address review nits in the --format plumbing"
```

---

### Task 9: Guard every stdout writer, and survive a closed stderr

`disallowed-methods` caught `status` / `spawn`, but not
`writeln!(io::stdout(), …)` (`print_stdout` only covers the macros). And the
logger's `eprintln!` panicked on a closed stderr, which under JSON is the busy
stream, so the command could die (exit 101) before writing its envelope.

**Files:**
- Modify: `clippy.toml`
- Modify: `crates/edgezero-cli/src/templates/root/clippy.toml.hbs`
- Modify: `crates/edgezero-cli/src/output.rs` (`finish`), `crates/edgezero-cli/src/adapter.rs` (`run_shell_tee`), `crates/edgezero-cli/src/config.rs` (`print_unified_diff_inline`)
- Modify: `crates/edgezero-cli/src/lib.rs` (`CliLogger::log`, imports)

**Interfaces:** none.

- [x] **Step 1: Add the lint entry**

`clippy.toml` (and extend the comment above it with "as does every direct
`std::io::stdout()` writer (`print_stdout` covers only the macros)"):

```toml
    { path = "std::io::stdout", reason = "`--format json` reserves stdout for the JSON envelope; write through the logger, or `#[expect]` with a reason" },
```

- [x] **Step 2: Run clippy to see it fire**

Run: `cargo +1.95.0 clippy --workspace --all-targets --all-features -- -D warnings`
Expected: three `disallowed_methods` errors: `output.rs` (`finish`),
`adapter.rs` (`run_shell_tee`), `config.rs` (`print_unified_diff_inline`).

- [x] **Step 3: `#[expect]` the three deliberate writers**

```rust
            // output.rs, finish:
            #[expect(
                clippy::disallowed_methods,
                reason = "the JSON envelope is the one thing `--format json` writes to stdout"
            )]
            let mut stdout = io::stdout().lock();

    // adapter.rs, run_shell_tee:
    } else {
        #[expect(
            clippy::disallowed_methods,
            reason = "text mode echoes the child's stdout to stdout, exactly as before `--format`"
        )]
        let stdout = io::stdout();
        tee_stream(child_stdout, stdout)
    };

    // config.rs, print_unified_diff_inline:
    #[expect(
        clippy::disallowed_methods,
        reason = "`config push` has no `--format`; its inline diff is text-only output"
    )]
    let mut stdout = stdout().lock();
```

- [x] **Step 4: Carry the guard into generated projects**

Append to `templates/root/clippy.toml.hbs`:

```toml

# The built-in commands' `--format json` reserves stdout for one JSON envelope.
# Mirror the EdgeZero workspace guard so your own subcommands keep stdout clean
# too: route output through the logger (`edgezero_cli::init_cli_logger`), and
# `#[expect]` any deliberate stdout write or child spawn with a reason.
disallowed-methods = [
    { path = "std::process::Command::status", reason = "the child inherits stdout; pipe or redirect it, or `#[expect]` with a reason" },
    { path = "std::process::Command::spawn", reason = "the child inherits stdout by default; pipe or redirect it, or `#[expect]` with a reason" },
    { path = "std::io::stdout", reason = "`--format json` reserves stdout for the JSON envelope; write through the logger, or `#[expect]` with a reason" },
]
```

The reasons don't point at `edgezero_adapter::process`: a generated CLI depends
only on `edgezero-cli`.

- [x] **Step 5: Make the logger's stderr writes infallible**

`lib.rs` imports: `use std::io::{self, ErrorKind, Write as _};`. In `CliLogger::log`:

```rust
        // `writeln!` rather than `eprintln!`: a closed stderr must not panic
        // the command before `--format json` writes its envelope.
        match record.level() {
            // warn/error always go to stderr; `--format json` sends info there
            // too, keeping stdout for the JSON envelope.
            log::Level::Error | log::Level::Warn => {
                let _ignored = writeln!(io::stderr(), "{}", record.args());
            }
            log::Level::Info if INFO_TO_STDERR.load(Ordering::SeqCst) => {
                let _ignored = writeln!(io::stderr(), "{}", record.args());
            }
            // the stdout `info` arm is unchanged
```

- [x] **Step 6: Verify**

Run: `cargo +1.95.0 clippy --workspace --all-targets --all-features -- -D warnings && cargo +1.95.0 test -p edgezero-cli --test generated_project_builds -- --ignored`
Expected: clippy clean (the three `#[expect]`s are fulfilled, so none is
reported as unfulfilled); the generated workspace compiles (about 2 minutes).

- [ ] **Step 7: Commit**

```bash
git add clippy.toml crates/edgezero-cli/src/templates/root/clippy.toml.hbs crates/edgezero-cli/src/adapter.rs
git add -p crates/edgezero-cli/src/output.rs crates/edgezero-cli/src/config.rs crates/edgezero-cli/src/lib.rs
git commit -m "Lint direct stdout writers and never panic on a closed stderr"
```

---

### Task 10: End-to-end coverage for active-version, rollback and the token healthcheck

The review listed commands whose result mapping, and the
`version=` / `rolled-back-to=` / "no active version yet" lines that moved into
the CLI, had no tests. These tests also commit a reduced form of the text-mode
byte comparison: each checks the exact text bytes as well as the JSON.

**Files:**
- Test: `crates/edgezero-cli/tests/format_json.rs`

**Interfaces:**
- Produces (test helpers, used by Tasks 4–6 tests too): `const FASTLY_MANIFEST: &str`, `const VERSIONS: &str`, `const ACTIVE_VERSION: [&str; 5]`, `const ROLLBACK: [&str; 9]`, `const DEPLOY: [&str; 5]`, `fn project_with(manifest: &str) -> TempDir`, `fn edgezero_with_token(dir: &Path, args: &[&str], bin_dir: Option<&Path>, token: Option<&str>) -> Output`, `fn fake_curl_api(code: u16) -> TempDir`, `fn fake_curl_script(body: &str) -> TempDir`, `fn api_calls(curl: &TempDir) -> Vec<String>`

- [x] **Step 1: Add the helpers**

```rust
    /// A Fastly app whose production deploy is a manifest command, so `deploy`
    /// runs it through the capturing (tee) path.
    const FASTLY_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"

[adapters.fastly.commands]
deploy = "echo 'SUCCESS: Deployed package (service SVC1, version 12)'"
"#;

    /// The versions API answer: 6 inactive, 7 active.
    const VERSIONS: &str = r#"[{"number":6,"active":false},{"number":7,"active":true}]"#;

    const ACTIVE_VERSION: [&str; 5] =
        ["active-version", "--adapter", "fastly", "--service-id", "SVC1"];

    const ROLLBACK: [&str; 9] = [
        "rollback", "--adapter", "fastly", "--service-id", "SVC1",
        "--version", "7", "--rollback-to", "6",
    ];

    const DEPLOY: [&str; 5] = ["deploy", "--adapter", "fastly", "--service-id", "SVC1"];
```

These consts sit with `HEALTHCHECK`, before the functions
(`clippy::arbitrary_source_item_ordering` rejects consts between tests).

```rust
    /// A temp project holding `manifest` as `edgezero.toml`.
    fn project_with(manifest: &str) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        fs::write(dir.path().join("edgezero.toml"), manifest).expect("write manifest");
        dir
    }
```

`project()` becomes `project_with(MANIFEST)`. `edgezero(dir, args, bin_dir)`
becomes `edgezero_with_token(dir, args, bin_dir, None)`, and
`edgezero_with_token` is the old body plus `.env_remove("FASTLY_SERVICE_ID")`
and:

```rust
        if let Some(value) = token {
            command.env("FASTLY_API_TOKEN", value);
        }
```

(`value`, not `token`: clippy rejects shadowing the parameter.)

```rust
    /// A fake `curl` answering every probe with HTTP `code`.
    fn fake_curl(code: u16) -> TempDir {
        fake_curl_script(&format!("echo {code}"))
    }

    /// A fake `curl` that also serves the Fastly API: a request read from
    /// `--config -` gets [`VERSIONS`] (GET) or a bare 200 (PUT), and is logged
    /// to `api.log` as `GET` / `PUT`. A health probe gets HTTP `code`.
    fn fake_curl_api(code: u16) -> TempDir {
        fake_curl_script(&format!(
            "case \" $* \" in *\" --config \"*)\n\
             \x20 cfg=$(cat)\n\
             \x20 case \"$cfg\" in *'request = \"PUT\"'*) echo PUT >> \"$(dirname \"$0\")/api.log\"; printf 200;;\n\
             \x20 *) echo GET >> \"$(dirname \"$0\")/api.log\"; printf '%s\\n200' '{VERSIONS}';; esac\n\
             \x20 exit 0;;\n\
             esac\n\
             echo {code}"
        ))
    }

    fn fake_curl_script(body: &str) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        let script = dir.path().join("curl");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write curl");
        let mut perms = fs::metadata(&script).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod +x");
        dir
    }

    /// The Fastly API calls a [`fake_curl_api`] served, in order.
    fn api_calls(curl: &TempDir) -> Vec<String> {
        fs::read_to_string(curl.path().join("api.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
```

- [x] **Step 2: Add the tests**

```rust
    #[test]
    fn json_healthcheck_with_a_token_verifies_the_version() {
        let dir = project();
        let curl = fake_curl_api(200);
        let mut args = HEALTHCHECK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("tok"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["result"]["healthy"], json!(true));
        assert_eq!(envelope["result"]["version_verified"], json!(true));
        assert_eq!(api_calls(&curl), ["GET", "GET"], "before and after the probe");
    }

    #[test]
    fn json_unhealthy_healthcheck_with_a_token_is_not_verified() {
        let dir = project();
        let curl = fake_curl_api(503);
        let mut args = HEALTHCHECK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("tok"));
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["result"]["healthy"], json!(false));
        assert_eq!(
            envelope["result"]["version_verified"],
            json!(false),
            "the after-probe check never ran"
        );
        assert_eq!(api_calls(&curl), ["GET"]);
    }

    #[test]
    fn active_version_text_and_json() {
        let dir = project();
        let curl = fake_curl_api(200);
        let text = edgezero_with_token(dir.path(), &ACTIVE_VERSION, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(stdout_of(&text), "version=7\n");

        let mut args = ACTIVE_VERSION.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "version": 7_i32})
        );
        assert!(stderr_of(&output).contains("version=7\n"));
    }

    #[test]
    fn active_version_with_none_active_reports_null() {
        let dir = project();
        // Overwrite the API answer: no version is active yet.
        let curl = fake_curl_script(
            "case \" $* \" in *\" --config \"*) cat >/dev/null; \
             printf '%s\\n200' '[{\"number\":1,\"active\":false}]'; exit 0;; esac",
        );
        let text = edgezero_with_token(dir.path(), &ACTIVE_VERSION, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(
            stdout_of(&text),
            "version=\nservice SVC1 has no active version yet; emitting an empty rollback target\n"
        );

        let mut args = ACTIVE_VERSION.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(sole_json_document(&output)["result"]["version"], json!(null));
    }

    #[test]
    fn rollback_text_and_json() {
        let dir = project();
        let curl = fake_curl_api(200);
        let text = edgezero_with_token(dir.path(), &ROLLBACK, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert!(
            stdout_of(&text).ends_with("rolled-back-to=6\n"),
            "stdout: {:?}",
            stdout_of(&text)
        );

        let mut args = ROLLBACK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({
                "adapter": "fastly", "rolled_back_to": 6_i32, "service_id": "SVC1",
                "staging": false, "version": 7_i32
            })
        );
        assert!(stderr_of(&output).contains("rolled-back-to=6\n"));
    }
```

- [x] **Step 3: Run**

Run: `cargo +1.95.0 test -p edgezero-cli --test format_json`
Expected: 18 passed. `json_unhealthy_healthcheck_with_a_token_is_not_verified`
fails without Task 2; the others pin existing behavior.

- [ ] **Step 4: Commit** (Tasks 4–6 commit their own tests; if they land first, this commit holds only the helpers and the five tests above)

```bash
git add crates/edgezero-cli/tests/format_json.rs
git commit -m "Cover active-version, rollback and the token healthcheck end to end"
```

---

### Task 11: Docs and spec

**Files:**
- Modify: `docs/guide/cli-reference.md` ("Machine-readable output")
- Modify: `docs/superpowers/specs/2026-09-25-cli-format-json-design.md` (§6.1, §6.4, §8, §13)

**Interfaces:** none (documents Tasks 2–9).

- [x] **Step 1: CLI reference, Streams**

After the "empty stdout" paragraph, add:

```markdown
`--help` and `--version` are handled by clap before `--format` is read: they
print clap's text to stdout and exit `0`, with no envelope.

The routing relies on the EdgeZero logger (`edgezero_cli::init_cli_logger()`).
A downstream CLI that installs a different logger, such as `simple_logger`,
which writes every level to stdout, will put log lines on stdout and break the
one-document rule.
```

Extend the exit-code paragraph with: "In a generated CLI, `2` also means a
usage error or an unsupported `config diff`; only the envelope tells them apart,
and an empty stdout means there is none."

- [x] **Step 2: CLI reference, Results**

Replace the prose list with one table per command with columns
`Key | Type | Nullable | Meaning`, covering every key. The nullable keys under
schema 1 are exactly: `active-version.version`, `build.artifact`,
`deploy.service_id`, `deploy.version`, `healthcheck.staging_ip`,
`healthcheck.status_code`, `rollback.rolled_back_to`, `provision.entries[].store`,
`provision.entries[].store.logical`, `config gc` `older_than_secs`, `deleted` and
`store.id`. `config validate.app_config` is now non-null. Notes under the
tables:

- `build`: a `--format` after a passthrough argument is rejected.
- `deploy`: `service_id` resolution order; a deploy that went live without a
  resolvable version gives `ok: false` with the result present and
  `version: null`.
- `healthcheck.version_verified`: true only for a healthy production probe with
  `FASTLY_API_TOKEN` set.
- `provision`: `would_create` → `created` or `already_present`; `would_update` →
  `updated`. The Fastly and Spin dry runs report the plan without checking
  current state; Fastly's real run omits the `edgezero_runtime_env` entry when
  it is already declared.
- `config gc.deleted`: `null` on a dry run, `0` for a real run with nothing to
  reclaim.

- [x] **Step 3: Spec**

- §6.1 `result` row: replace "`active-version --require-active` with nothing
  active" with "a deploy that went live without a resolvable version".
- §6.4: add a `--help` / `--version` row (exit 0, clap's text) and the exit-2
  paragraph.
- §8: drop the `--require-active` bullet from `active-version`; update
  `deploy`, `healthcheck`, `provision`, `config gc` and `config validate` to
  match Step 2.
- §13: reword the `AuthStatus` bullet to "`AuthState::Unauthenticated` carries
  its reason", and append a "Revision 3" list naming each change in Tasks 1–9.

- [x] **Step 4: Format and lint the docs**

Run: `cd docs && npx prettier --write guide/cli-reference.md superpowers/specs/2026-09-25-cli-format-json-design.md && npm run lint && npm run format`
Expected: "All matched files use Prettier code style!" and no ESLint errors.

- [ ] **Step 5: Commit**

```bash
git add docs/guide/cli-reference.md docs/superpowers/specs/2026-09-25-cli-format-json-design.md
git commit -m "Document result nullability and the review's contract fixes"
```

---

### Task 12: Final verification, push, reply

- [x] **Step 1: Run every CI gate** (see Global Constraints)

Results on 2026-10-05: fmt clean; workspace clippy clean; workspace tests pass
(edgezero-cli lib 217, `format_json` 18); Fastly `--features cli` 291 pass; the
feature check and Spin wasm32 check pass; the wasm clippy matrix (cloudflare,
fastly, fastly+cli, spin) is clean; app-demo fmt, clippy and tests pass;
`generated_project_builds` passes (116 s); docs ESLint and Prettier are clean.

- [ ] **Step 2: Push** `spec/383-cli-format-json` (with the author's go-ahead).

- [ ] **Step 3: Reply in each review thread**

Use `gh api repos/stackpop/edgezero/pulls/388/comments/{id}/replies`, one reply
per inline comment, naming the fix and its commit. The spec-approval question
and the out-of-scope `wrangler whoami` note are the author's to answer.
