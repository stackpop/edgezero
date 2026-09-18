# Provider-Neutral Action Cores Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move immutable-release and application-CLI lifecycle machinery into provider-neutral internal cores while retaining thin Fastly public actions and adding no deployment implementation for another adapter.

**Architecture:** Keep `.github/actions/deploy-core` as the provider-neutral workspace, CLI extraction, invocation, environment, log, and output-parsing library. Add an internal `.github/actions/release-core` for generic immutable-release packaging and verification, then make the renamed `package-application-release-fastly` action and existing Fastly lifecycle actions supply only Fastly policy. Move the format-level Rust release verifier into the host-only `edgezero-adapter/cli` feature and make Fastly pass its expected adapter and protocol explicitly.

**Tech Stack:** Bash 3.2-compatible shell, GitHub composite actions, `jq`, Mike Farah `yq` v4, `tar`, Rust 2024, Cargo, ShellCheck, actionlint, VitePress/Prettier/ESLint.

---

## File structure

New provider-neutral files:

- `.github/actions/release-core/scripts/prepare-release.sh` — verify the outer digest, exact archive members, format-1 metadata, expected adapter/protocol/revision, inner digests, manifest relationship, and extraction confinement.
- `.github/actions/release-core/scripts/package-release.sh` — validate generic inputs and lifecycle capabilities, preserve the selected adapter manifest path, construct format-1 metadata, create the archive, and verify it before publishing paths/digests.
- `.github/actions/release-core/tests/run.sh` — synthetic-adapter producer/consumer tests with no provider tool or provider deployment implementation.
- `.github/actions/deploy-core/scripts/invoke-app-cli.sh` — resolve the verified application CLI, import typed credentials, preserve allowed public environment variables, scrub action-private state, emit the mutation signal, and execute an exact NUL-delimited argument vector.
- `crates/edgezero-adapter/src/release.rs` — format-level immutable-release verifier available only with the `cli` feature.

Fastly wrapper files:

- `.github/actions/package-application-release-fastly/action.yml` — renamed public composite action with the unchanged package-action input/output surface.
- `.github/actions/package-application-release-fastly/lifecycle-protocol.json` — fixed Fastly lifecycle protocol 1 capability declaration.
- `.github/actions/package-application-release-fastly/scripts/package-release.sh` — map the unchanged Fastly inputs plus fixed adapter/protocol policy to release-core.
- `.github/actions/package-application-release-fastly/tests/run.sh` — verify the unchanged Fastly archive contract and exact five-command capability declaration.
- `.github/actions/fastly-common/scripts/common.sh` — retain only Fastly validation, credential-alias, and public-environment policy helpers.

Modified consumers:

- `.github/actions/{deploy-fastly,config-push-fastly,healthcheck-fastly,rollback-fastly}/action.yml` — call release-core and pass secrets only through private wrapper carriers.
- Their existing wrapper scripts — validate Fastly inputs, construct exact argv files, call the generic invocation core, and parse Fastly outputs with the existing per-action policy.
- `.github/actions/deploy-core/scripts/common.sh` and tests — generic NUL-file and output-parsing primitives.
- `crates/edgezero-adapter-fastly/src/{cli.rs,lib.rs}` and both adapter Cargo manifests — consume the shared Rust verifier and remove the Fastly-owned module.
- `.github/workflows/deploy-action.yml` and `docs/guide/deploy-action-adoption.md` — use the renamed package action.

Deleted after all callers migrate:

- `.github/actions/fastly-common/scripts/prepare-release.sh`
- `.github/actions/deploy-core/scripts/run-app-cli.sh`
- `crates/edgezero-adapter-fastly/src/release.rs`
- `.github/actions/package-fastly-application-release/`

The untracked `examples/app-demo/crates/app-demo-adapter-cloudflare/.dev.vars` is unrelated and must remain untouched.

`release-core` may source `deploy-core/scripts/common.sh` for low-level generic
filesystem, digest, archive, annotation, and output helpers. The dependency is
one-way: deploy-core never imports release-core, and neither core imports Fastly
policy.

### Task 1: Rename the Fastly release-package action and lock its public contract

**Files:**

- Modify: `.github/actions/deploy-core/tests/run.sh:842-886,2216-2337,2380-2386`
- Move: `.github/actions/package-fastly-application-release/` → `.github/actions/package-application-release-fastly/`
- Modify: `.github/workflows/deploy-action.yml:90-107`
- Modify: `docs/guide/deploy-action-adoption.md:20-40`

- [ ] **Step 1: Add the failing action-name contract test**

Add this assertion before changing the directory:

```bash
test_package_action_name() {
  section "Fastly package action name"
  assert_succeeds "the package action uses operation/object/provider order" \
    test -f "$ACTIONS_DIR/package-application-release-fastly/action.yml"
  assert_fails "the obsolete package-fastly-application-release path is absent" \
    test -e "$ACTIONS_DIR/package-fastly-application-release"
}
```

Call `test_package_action_name` from `main`. Change the golden surface label and path to `package-application-release-fastly`, but do not alter any expected input, output, required flag, or default.

- [ ] **Step 2: Run the contract test and verify the expected failure**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
```

Expected: FAIL because `package-application-release-fastly/action.yml` does not exist and the old directory still exists.

- [ ] **Step 3: Rename the action and update repository consumers**

Use `git mv` for the directory. Update the workflow ShellCheck list, the deploy-core test loops and test runner, and the adoption guide action reference. Keep the action metadata and script behavior unchanged in this task.

- [ ] **Step 4: Prove the old path has no product-contract references**

Run:

```bash
rg --hidden 'package-fastly-application-release' \
  --glob '!target/**' \
  --glob '!docs/node_modules/**' \
  --glob '!docs/superpowers/specs/**' \
  --glob '!docs/superpowers/plans/**'
```

Expected: no matches. The approved spec and this plan may retain the old name only to document the migration.

- [ ] **Step 5: Run focused and repository tests**

Run:

```bash
.github/actions/package-application-release-fastly/tests/run.sh
.github/actions/deploy-core/tests/run.sh
cargo test --workspace --all-targets
```

Expected: PASS, with the package action’s exact public surface unchanged.

- [ ] **Step 6: Commit**

```bash
git add .github/actions .github/workflows/deploy-action.yml docs/guide/deploy-action-adoption.md
git commit -m "refactor(actions): rename Fastly release packager"
```

### Task 2: Extract provider-neutral release consumption

**Files:**

- Create: `.github/actions/release-core/scripts/prepare-release.sh`
- Create: `.github/actions/release-core/tests/run.sh`
- Modify: `.github/actions/{deploy-fastly,config-push-fastly,healthcheck-fastly,rollback-fastly}/action.yml`
- Modify: `.github/actions/deploy-core/tests/run.sh:439-686,2339-2358`
- Delete: `.github/actions/fastly-common/scripts/prepare-release.sh`

- [ ] **Step 1: Write a synthetic-adapter verifier test**

Create a test fixture whose `release.json` uses adapter `synthetic`, lifecycle protocol `7`, and these exact members:

```text
release.json
cli/app-cli.tar
package/app.tar.gz
edgezero.toml
adapters/synthetic.toml
```

The manifest must contain both `synthetic` and an unselected adapter, while the archive contains only `adapters/synthetic.toml`. Invoke the future verifier with:

```bash
GITHUB_OUTPUT="$output" \
EDGEZERO__APP__RELEASE__ARCHIVE="$archive" \
EDGEZERO__APP__RELEASE__SHA256="$archive_digest" \
EDGEZERO__APP__RELEASE__ROOT="$work/release" \
EDGEZERO__APP__RELEASE__EXPECTED_ADAPTER=synthetic \
EDGEZERO__APP__RELEASE__EXPECTED_LIFECYCLE_PROTOCOL=7 \
EDGEZERO__APP__RELEASE__EXPECTED_SOURCE_REVISION="$revision" \
bash "$RELEASE_CORE/scripts/prepare-release.sh"
```

Assert the emitted CLI, package, application-manifest, adapter-manifest, package digest, and source revision all resolve under the requested release root. Add negative cases for an adapter mismatch and protocol mismatch.

- [ ] **Step 2: Run the new test and verify it fails**

Run:

```bash
.github/actions/release-core/tests/run.sh
```

Expected: FAIL because `release-core/scripts/prepare-release.sh` does not exist.

- [ ] **Step 3: Move and parameterize the verifier**

Move the existing Fastly verifier into release-core and replace hard-coded policy with required values:

```bash
expected_adapter="${EDGEZERO__APP__RELEASE__EXPECTED_ADAPTER:-}"
expected_protocol="${EDGEZERO__APP__RELEASE__EXPECTED_LIFECYCLE_PROTOCOL:-}"
require_input expected-adapter "$expected_adapter"
require_input expected-lifecycle-protocol "$expected_protocol"
[[ "$expected_adapter" =~ ^[a-z][a-z0-9_-]*$ ]] || fail "expected-adapter is invalid"
[[ "$expected_protocol" =~ ^[1-9][0-9]*$ ]] || fail "expected-lifecycle-protocol must be a positive integer"
```

Use `jq --arg adapter "$expected_adapter" --argjson protocol "$expected_protocol"` for the exact metadata comparison. Select the manifest entry case-insensitively with `ascii_downcase == $adapter`, require exactly one match, and compare its declared path with `.manifests.adapter.path`. Preserve duplicate-key detection, exact schema, exact archive membership, digest checks, symlink rejection, path normalization, and confined extraction.

- [ ] **Step 4: Point every Fastly release consumer at the generic verifier**

In all four lifecycle action files, replace the Fastly verifier path with:

```yaml
EDGEZERO__APP__RELEASE__EXPECTED_ADAPTER: fastly
EDGEZERO__APP__RELEASE__EXPECTED_LIFECYCLE_PROTOCOL: "1"
run: exec "$GITHUB_ACTION_PATH/../release-core/scripts/prepare-release.sh"
```

Keep every existing release input/output mapping and failure ordering unchanged. Delete `fastly-common/scripts/prepare-release.sh` only after all callers and tests use release-core.

- [ ] **Step 5: Run verifier and lifecycle contract tests**

Run:

```bash
.github/actions/release-core/tests/run.sh
.github/actions/deploy-core/tests/run.sh
cargo test --workspace --all-targets
```

Expected: PASS. The synthetic release verifies without Fastly files or tools, and every Fastly lifecycle action still verifies the immutable release before extracting its CLI.

- [ ] **Step 6: Commit**

```bash
git add .github/actions/release-core .github/actions/deploy-core/tests/run.sh \
  .github/actions/deploy-fastly .github/actions/config-push-fastly \
  .github/actions/healthcheck-fastly .github/actions/rollback-fastly \
  .github/actions/fastly-common
git commit -m "refactor(actions): share immutable release verification"
```

### Task 3: Extract provider-neutral release production

**Files:**

- Create: `.github/actions/release-core/scripts/package-release.sh`
- Modify: `.github/actions/release-core/tests/run.sh`
- Create: `.github/actions/package-application-release-fastly/lifecycle-protocol.json`
- Modify: `.github/actions/package-application-release-fastly/scripts/package-release.sh`
- Modify: `.github/actions/package-application-release-fastly/tests/run.sh`
- Modify: `.github/actions/package-application-release-fastly/action.yml`
- Modify: `.github/actions/deploy-core/tests/run.sh:2380-2386`

- [ ] **Step 1: Add failing generic packager and capability tests**

Extend release-core tests with a synthetic capability declaration:

```json
{
  "lifecycle_protocol": 7,
  "probes": [
    {
      "command": ["publish"],
      "required_flags": ["--adapter", "--package"]
    }
  ]
}
```

Use a fake CLI that supports top-level `--help` and `publish --help`, but
implements no provider deployment. Assert the generic packager emits adapter
`synthetic`, protocol `7`, preserves `adapters/synthetic.toml`, and produces the
exact five-member archive. Add rejection cases for unknown declaration keys,
zero protocol, an empty probe list, empty or non-printable command/flag tokens,
duplicate command arrays, duplicate flags, a flag without a leading `-`,
`--help` in a command, a missing required flag, a failed help command, a
symlinked manifest, and an extra archive member.

- [ ] **Step 2: Run the release-core test and verify it fails**

Run:

```bash
.github/actions/release-core/tests/run.sh
```

Expected: FAIL because `release-core/scripts/package-release.sh` does not exist.

- [ ] **Step 3: Implement the generic package contract**

Move the format-level logic from the Fastly package script into release-core. Its required provider-neutral inputs are:

```text
EDGEZERO__RELEASE__ADAPTER
EDGEZERO__RELEASE__LIFECYCLE_CAPABILITIES
EDGEZERO__RELEASE__POLICY_ROOT
EDGEZERO__RELEASE__APP_CLI_ARCHIVE
EDGEZERO__RELEASE__PACKAGE
EDGEZERO__RELEASE__APPLICATION_MANIFEST
EDGEZERO__RELEASE__ADAPTER_MANIFEST
EDGEZERO__RELEASE__SOURCE_REVISION
EDGEZERO__RELEASE__ARTIFACT_NAME
```

Require `POLICY_ROOT` to be a canonical directory and the capability declaration
to be a regular, non-symlink file beneath it. This permits a provider wrapper to
use checked-in policy without trusting application workspace content. Validate
the capability JSON’s exact keys and types with `jq`/`yq`. Require a positive
integer protocol and a non-empty probe array. Require each command to contain
non-empty printable tokens, reject `--help`, reject duplicate command arrays,
and require each non-empty flag array to contain unique printable tokens that
begin with `-`. Append `--help` to each validated command array, execute it as a Bash array under
`env -i PATH=/usr/bin:/bin HOME="${HOME:-/tmp}"`, and match each required flag as
an exact whitespace-delimited token or `--flag=<value>`. Never use `eval` or a
shell command string. Read the selected adapter manifest dynamically from
`edgezero.toml`; retain the reserved-member and confinement checks. Write the
existing exact format-1 schema with dynamic adapter/protocol values, then call
release-core’s consumer verifier before publishing outputs.

- [ ] **Step 4: Make the Fastly package script a policy-only wrapper**

Check in the exact Fastly declaration:

```json
{
  "lifecycle_protocol": 1,
  "probes": [
    {"command":["deploy"],"required_flags":["--adapter","--service-id","--application-release","--staging"]},
    {"command":["config","push"],"required_flags":["--adapter","--manifest","--app-config","--store","--staging","--no-env","--yes","--no-diff"]},
    {"command":["healthcheck"],"required_flags":["--adapter","--service-id","--version","--domain","--path","--retry","--retry-delay","--timeout","--staging"]},
    {"command":["rollback"],"required_flags":["--adapter","--service-id","--version","--rollback-to","--staging"]},
    {"command":["active-version"],"required_flags":["--adapter","--service-id"]}
  ]
}
```

The wrapper must set `EDGEZERO__RELEASE__ADAPTER=fastly`, set its own action
directory as `EDGEZERO__RELEASE__POLICY_ROOT`, point at this checked-in
declaration, map the existing `fastly-package` action input to
`EDGEZERO__RELEASE__PACKAGE`, and `exec` the generic packager. Do not add a
generic public action or any other provider action.

- [ ] **Step 5: Lock the Fastly declaration and archive contract**

Update the Fastly package test to assert exact declaration equality, the unchanged public surface, the unchanged format-1 `release.json`, the exact member list, `cli/app-cli.tar`, the selected manifest path, and all existing safety failures. Add a static test that fails if provider names, credential aliases, or provider-specific branches appear in `release-core`.

- [ ] **Step 6: Run package, release, and repository tests**

Run:

```bash
.github/actions/release-core/tests/run.sh
.github/actions/package-application-release-fastly/tests/run.sh
.github/actions/deploy-core/tests/run.sh
cargo test --workspace --all-targets
```

Expected: PASS, including the synthetic adapter release and the unchanged Fastly archive.

- [ ] **Step 7: Commit**

```bash
git add .github/actions/release-core .github/actions/package-application-release-fastly \
  .github/actions/deploy-core/tests/run.sh
git commit -m "refactor(actions): share immutable release packaging"
```

### Task 4: Move the Rust release verifier to the adapter boundary

**Files:**

- Create: `crates/edgezero-adapter/src/release.rs`
- Modify: `crates/edgezero-adapter/src/lib.rs`
- Modify: `crates/edgezero-adapter/Cargo.toml`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:1-35,730-770`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs:1-25`
- Delete: `crates/edgezero-adapter-fastly/src/release.rs`
- Modify: `crates/edgezero-adapter-fastly/Cargo.toml`

- [ ] **Step 1: Add a failing shared-verifier test**

Declare `#[cfg(feature = "cli")] pub mod release;` in `edgezero-adapter/src/lib.rs` and start the new module with a synthetic fixture. Call this intended API:

```rust
let verified = verify_application_release(
    fixture.root.path(),
    &fixture.application_manifest,
    &fixture.adapter_manifest,
    "synthetic",
    7,
)?;
```

Assert confined paths and metadata, then mutate the adapter and protocol independently and assert both fail. Retain the existing exact-schema, duplicate JSON field, normalized path, symlink, exact member, digest, and loaded-manifest identity cases.

- [ ] **Step 2: Run the shared crate test and verify it fails**

Run:

```bash
cargo test -p edgezero-adapter --features cli release
```

Expected: FAIL because the generic verifier API is not implemented.

- [ ] **Step 3: Move and generalize the implementation**

Move the existing implementation into `edgezero-adapter`. Make
`VerifiedApplicationRelease`, `adapter_manifest()`, `package()`,
`package_sha256()`, and `verify_application_release` public under the `cli`
feature. Keep the remaining recorded-member fields private to the verifier.
Replace the Fastly constants with the two expected arguments and compare them
exactly:

```rust
if metadata.lifecycle_protocol != expected_lifecycle_protocol { /* generic error */ }
if metadata.adapter != expected_adapter { /* generic error */ }
```

Add optional `serde`, `serde_json`, `sha2`, and `walkdir` dependencies to `edgezero-adapter`’s `cli` feature. Do not enable them for default/WASM builds.

- [ ] **Step 4: Make Fastly supply its policy**

Import from `edgezero_adapter::release` in Fastly and call with `"fastly", 1`. Remove the Fastly release module declaration/file. Remove dependencies from the Fastly crate only when `rg` confirms no remaining Fastly code uses them.

- [ ] **Step 5: Run targeted and workspace checks**

Run:

```bash
cargo test -p edgezero-adapter --features cli
cargo test -p edgezero-adapter-fastly --all-targets --features cli
cargo test --workspace --all-targets
cargo check -p edgezero-adapter --no-default-features
cargo check -p edgezero-adapter-fastly --no-default-features --lib
```

Expected: PASS. The synthetic fixture proves provider neutrality; Fastly still enforces adapter `fastly` and protocol `1`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.lock crates/edgezero-adapter crates/edgezero-adapter-fastly
git commit -m "refactor(adapter): share application release verification"
```

### Task 5: Add the generic application-CLI invocation core

**Files:**

- Create: `.github/actions/deploy-core/scripts/invoke-app-cli.sh`
- Modify: `.github/actions/deploy-core/scripts/common.sh:220-280`
- Modify: `.github/actions/deploy-core/tests/run.sh:217-345,988-1210,1155-1220,1777-1808`

- [ ] **Step 1: Write failing exact-argv and environment tests**

Create a fake CLI that writes its arguments to a NUL-delimited capture file. Invoke the future core with an argv file containing:

```bash
printf '%s\0' publish --adapter synthetic --label 'two words' '' $'line one\nline two' >"$args"
```

Assert the capture file is byte-for-byte equal to the expected post-executable argv. Add tests that:

- reject a missing/unterminated argument file;
- reject mutation values other than `true` or `false`;
- clear every alias in the provider-clear file before importing typed JSON values;
- reject typed credential names absent from that clear file;
- preserve canonical store selectors and wrapper-allowlisted public variables;
- scrub every other `EDGEZERO__*` variable;
- export only the verified manifest as `EDGEZERO_MANIFEST`;
- default the application CLI working directory to the core caller's `$PWD`
  when no explicit working directory is supplied;
- honor an explicit existing working directory and reject a nonexistent one;
- emit `mutation-attempted=true` after successful setup and immediately before a mutating CLI starts;
- omit the signal for a non-mutating call;
- preserve the application CLI exit status; and
- leave the existing provider-neutral `build-app-cli` credential scrub and
  untrusted-build isolation unchanged.

- [ ] **Step 2: Add failing parser-policy tests**

Test these exact cases against new helpers in `common.sh`:

```text
last-canonical: malformed, value=41, value=42 -> 42
unique-canonical: value=42, value=42 -> 42
unique-canonical: value=42, value=43 -> failure
unique-canonical: value=bad, value=42 -> failure
exact-one-canonical: value=42 -> 42
exact-one-canonical: value=42, value=42 -> failure
```

Use a key containing regex punctuation in a unit test to prove keys are literal. Patterns come from test/wrapper code, never public inputs.

- [ ] **Step 3: Run the deploy-core tests and verify they fail**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
```

Expected: FAIL because `invoke-app-cli.sh` and the generic parsing helpers do not exist.

- [ ] **Step 4: Implement the exact invocation contract**

The new script reads:

```text
EDGEZERO__APP__CLI__PATH
EDGEZERO__APP__CLI__ARGS_FILE
EDGEZERO__APP__CLI__MUTATES=true|false
EDGEZERO__PROJECT__WORKING_DIRECTORY (optional; defaults to the caller's current directory)
EDGEZERO__PROJECT__MANIFEST_PATH (optional)
EDGEZERO__PROVIDER__ENV_CLEAR_FILE (optional NUL list)
EDGEZERO__PROVIDER__ENV (optional JSON object)
EDGEZERO__PUBLIC_RUNTIME_ENV_ALLOW_FILE (optional NUL list)
```

Require the CLI path and the argument, provider-clear, and public-allowlist files
to be regular, non-symlink paths beneath `EDGEZERO__ACTION__WORKSPACE`; require
the CLI to be executable and the argument file to end in NUL. Do not resolve a
bare binary name through `PATH`. Collect arguments without namerefs, prepend only
the verified CLI path, and execute `"${ARGV[@]}"`. Treat canonical adapter
host/port, logging endpoint/level, Config/KV/Secret selectors as built-in public
runtime variables; accept additional names only from the validated allowlist
file. Capture their values before scrubbing `EDGEZERO__*`, then restore them.
Capture the caller's `$PWD` before scrubbing and use it when
`EDGEZERO__PROJECT__WORKING_DIRECTORY` is absent. Require either working
directory to be an existing directory, then execute the CLI from it. Deploy and
config push pass their validated project directory explicitly; active-version,
healthcheck, and rollback omit it and preserve their wrapper's current working
directory. The application CLI is trusted lifecycle code; `build-app-cli`
retains its separate process-image isolation for untrusted build code.

- [ ] **Step 5: Implement the parsing primitives**

Add `read_last_canonical`, `read_unique_canonical`, and `read_exact_one_canonical` with the exact approved semantics. Compare the prefix before `=` to the literal key; do not splice the key into a regular expression. Retain `read_numeric_line` and `read_bool_line` as temporary delegating shims until every Fastly caller migrates.

- [ ] **Step 6: Run focused and repository tests**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
cargo test --workspace --all-targets
```

Expected: PASS. Existing callers still use `run-app-cli.sh`; the new core is independently covered by synthetic tests.

- [ ] **Step 7: Commit**

```bash
git add .github/actions/deploy-core
git commit -m "refactor(actions): add generic CLI invocation core"
```

### Task 6: Migrate Fastly deploy and active-version wrappers

**Files:**

- Modify: `.github/actions/deploy-fastly/scripts/deploy.sh`
- Modify: `.github/actions/deploy-fastly/scripts/capture-previous.sh`
- Modify: `.github/actions/deploy-fastly/action.yml`
- Modify: `.github/actions/fastly-common/scripts/common.sh`
- Modify: `.github/actions/deploy-core/tests/run.sh:264-345,1081-1154,3069-3169,3470-3755`

- [ ] **Step 1: Make tests require core delegation**

Update deploy/capture tests to fail unless each wrapper calls
`deploy-core/scripts/invoke-app-cli.sh`. Retain all existing assertions for
deploy argument ordering, passthrough after `--`, source revision,
`--application-release`, staging, package digest, recovery outputs, exact-one
active-version parsing, mutation timing, credential scrubbing, and original exit
status. Keep the independent `build-app-cli` isolation tests unchanged.

- [ ] **Step 2: Run the tests and verify the expected failure**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
```

Expected: FAIL because deploy and active-version still invoke the application
CLI directly or through the obsolete mode-based runner.

- [ ] **Step 3: Add Fastly-only policy helpers**

In `fastly-common/scripts/common.sh`, add helpers that write the full fixed Fastly credential-alias list and the Fastly-specific public runtime allowlist as NUL-delimited files beneath `EDGEZERO__ACTION__WORKSPACE`. The latter contains only:

```text
EDGEZERO__LOGGING__USE_FASTLY_LOGGER
EDGEZERO__LOGGING__ECHO_STDOUT
```

Keep service-ID validation there. Do not move provider names or aliases into deploy-core.

- [ ] **Step 4: Route deploy and active-version through the generic core**

Deploy constructs:

```text
deploy --adapter fastly <typed deploy flags> [-- <allowlisted passthrough>]
```

Active-version constructs:

```text
active-version --adapter fastly --service-id <id>
```

Deploy passes the token and service ID as typed JSON whose names appear in the
Fastly clear list. Active-version passes only the token because the service ID is
already an argument and is not currently exported to that command. Deploy uses
`MUTATES=true` and the Fastly public-runtime allowlist; active-version uses
`MUTATES=false` and no runtime allowlist. Keep the lifecycle log in the wrapper.
Deploy uses `unique-canonical` for `version` and `package-sha256`;
active-version uses `exact-one-canonical` with the existing optional-digits
pattern.

- [ ] **Step 5: Run deploy and workspace tests**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
cargo test --workspace --all-targets
```

Expected: PASS with unchanged Fastly action surfaces and recovery behavior.

- [ ] **Step 6: Commit**

```bash
git add .github/actions/deploy-core/tests/run.sh .github/actions/deploy-fastly \
  .github/actions/fastly-common
git commit -m "refactor(fastly): delegate deploy lifecycle invocation"
```

### Task 7: Migrate config push, healthcheck, and rollback wrappers

**Files:**

- Modify: `.github/actions/config-push-fastly/scripts/config-push.sh`
- Modify: `.github/actions/config-push-fastly/action.yml`
- Modify: `.github/actions/healthcheck-fastly/scripts/healthcheck.sh`
- Modify: `.github/actions/healthcheck-fastly/action.yml`
- Modify: `.github/actions/rollback-fastly/scripts/rollback.sh`
- Modify: `.github/actions/rollback-fastly/action.yml`
- Modify: `.github/actions/deploy-core/tests/run.sh:1289-1448,1627-1776,3170-3293,3443-3469`
- Delete: `.github/actions/deploy-core/scripts/run-app-cli.sh`

- [ ] **Step 1: Make every lifecycle test require generic invocation**

Add a static contract assertion that all lifecycle application CLI executions in
deploy, active-version, config push, healthcheck, and rollback delegate to
`invoke-app-cli.sh`; capability probes in release-core and the separate
`build-app-cli` action are the exceptions. Retain the current command-specific
argv, environment, log cleanup, output, rollback, and mutation tests.

- [ ] **Step 2: Run the tests and verify the expected failure**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
```

Expected: FAIL because config push, healthcheck, and rollback still execute the CLI directly.

- [ ] **Step 3: Migrate config push without changing mutation ordering**

Keep path confinement, committed-source validation, inline-config creation, and its cleanup trap in the Fastly wrapper. Write this exact argv to the core file:

```text
config push --adapter fastly --manifest <verified edgezero.toml>
  --app-config <confined path> [--store <id>] [--staging] [--no-env]
  --yes --no-diff
```

Pass only `FASTLY_API_TOKEN` through typed JSON, use `MUTATES=true`, and rely on the core’s canonical store-selector preservation. Parse `pushed-key` and `pushed-store` with `last-canonical`, preserving the existing missing-output failures and `store` output name.

- [ ] **Step 4: Migrate healthcheck and rollback**

Healthcheck uses `MUTATES=false`, imports a token only for staging, and keeps its
exact domain/path/retry arguments. Parse `healthy` and `status-code` with
`last-canonical`. When the CLI fails, publish independently valid recovery
outputs best-effort and then return its original status. On success, publish
`healthy=false` when no verdict exists before evaluating the health verdict.

Rollback uses `MUTATES=true`, imports the token through typed JSON, and keeps production `--rollback-to` versus staging `--staging`. Parse `rolled-back-to` with `last-canonical`, retaining the production-only requirement and original CLI status precedence.

In each action file, pass the public secret input only to a private `EDGEZERO__FASTLY__API_TOKEN` carrier; explicitly blank every public Fastly alias on the step as before.

- [ ] **Step 5: Remove transitional code**

Delete `run-app-cli.sh`, remove its tests and delegating parser shims only after `rg --hidden 'run-app-cli\.sh' .github/actions` returns no callers. Ensure provider-neutral deploy-core scripts contain no Fastly credential names, flags, or policy branches.

- [ ] **Step 6: Run lifecycle, release, and repository tests**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
.github/actions/release-core/tests/run.sh
.github/actions/package-application-release-fastly/tests/run.sh
cargo test --workspace --all-targets
```

Expected: PASS across production, staging, failure-before-mutation, failure-after-mutation, recovery, and store-free fixtures.

- [ ] **Step 7: Commit**

```bash
git add .github/actions/config-push-fastly .github/actions/healthcheck-fastly \
  .github/actions/rollback-fastly .github/actions/deploy-core
git commit -m "refactor(fastly): delegate lifecycle CLI execution"
```

### Task 8: Finish contract documentation and run all required checks

**Files:**

- Modify: `.github/workflows/deploy-action.yml`
- Modify: `docs/guide/deploy-action-adoption.md`
- Modify: `docs/superpowers/specs/2026-09-18-provider-neutral-action-cores-design.md` only if implementation-discovered contract details require factual correction
- Modify: `.github/actions/deploy-core/tests/run.sh`
- Modify: `.github/actions/release-core/tests/run.sh`

- [ ] **Step 1: Add final static architecture assertions**

Tests must fail when:

- a provider-neutral core contains a provider policy branch or credential alias;
- a public Fastly action surface differs from its golden contract;
- the old package action path or a product-contract reference remains;
- a lifecycle wrapper executes the app CLI outside the generic core;
- a Fastly release declaration differs from the exact protocol-1 declaration;
- an executable Python/pip command appears in action or CI shell code;
- the immutable archive member names/schema differ from format 1; or
- `build-app-cli` stops publishing `app-cli.tar`, changes the configurable
  artifact name, or emits a `tarball-path` that does not resolve to that file.

Allow provider names in synthetic negative-test data only when the assertion can distinguish fixtures from core production scripts.

- [ ] **Step 2: Run action tests and shell/static checks**

Run:

```bash
.github/actions/deploy-core/tests/run.sh
.github/actions/release-core/tests/run.sh
.github/actions/package-application-release-fastly/tests/run.sh
.github/actions/require-github-environment/tests/run.sh
shellcheck -e SC1091 \
  .github/actions/*/scripts/*.sh \
  .github/actions/*/tests/*.sh
actionlint_dir="$(mktemp -d)"
INSTALL_DIR="$actionlint_dir" scripts/install-actionlint.sh 1.7.7
"$actionlint_dir/actionlint" -shellcheck='shellcheck -S warning'
rg --hidden -n '(^|[;&|][[:space:]]*)(python|python3|pip|pip3)([[:space:]]|$)' \
  .github/actions .github/workflows scripts
```

Expected: every test passes, ShellCheck emits no warning, and the Python/pip search returns no matches.

- [ ] **Step 3: Run the required Rust checks**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p edgezero-adapter-fastly --features cli --all-targets -- -D warnings
cargo clippy -p edgezero-adapter-fastly --no-default-features --lib -- -D warnings
cargo test --workspace --all-targets
cargo test -p edgezero-adapter-fastly --all-targets --features cli
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Expected: all commands exit 0.

- [ ] **Step 4: Run documentation checks**

Run:

```bash
cd docs
npm run format
npm run lint
npm run build
```

Expected: all commands exit 0.

- [ ] **Step 5: Inspect the final contract and workspace**

Run:

```bash
rg --hidden 'package-fastly-application-release' \
  --glob '!target/**' \
  --glob '!docs/node_modules/**' \
  --glob '!docs/superpowers/specs/**' \
  --glob '!docs/superpowers/plans/**'
rg --hidden 'run-app-cli\.sh|fastly-common/scripts/prepare-release\.sh' \
  --glob '!target/**' .github/actions
rg --hidden 'edgezero-cli\.tar' \
  --glob '!target/**' \
  --glob '!docs/node_modules/**'
git diff --check
git status --short
```

Expected: all three searches return no matches, `git diff --check` passes, and
the only unrelated untracked path remains
`examples/app-demo/crates/app-demo-adapter-cloudflare/.dev.vars`.

- [ ] **Step 6: Commit final test/documentation adjustments**

```bash
git add .github/actions .github/workflows/deploy-action.yml \
  docs/guide/deploy-action-adoption.md \
  docs/superpowers/specs/2026-09-18-provider-neutral-action-cores-design.md
git commit -m "test(actions): verify provider-neutral action boundaries"
```

- [ ] **Step 7: Update PR #381 and issue #380 after verification**

Rewrite the PR title/body and issue summary around the final implementation:
environment-selected Fastly runtime stores plus provider-neutral internal
release/lifecycle cores, the renamed Fastly package action, unchanged Fastly
input/output and release schemas, no deployment action for another adapter, and
the exact validation commands above. State generically that coordinated
downstream consumers must use `app-cli.tar` and
`package-application-release-fastly`; EdgeZero code and documentation must not
name or depend on a specific application or deployer repository.
