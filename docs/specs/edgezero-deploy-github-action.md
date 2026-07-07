# EdgeZero Deploy GitHub Action — Fastly v0 Spec

**Status:** Implemented Fastly v0 contract

**Date:** 2026-07-02

**Delivery target:** implementation in the `stackpop/edgezero` monorepo

**Action path:** `.github/actions/deploy`

**Pre-release identity:**

```yaml
uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
```

## 1. Executive summary

Add a reusable GitHub composite action to the EdgeZero monorepo that deploys a
checked-out EdgeZero application to Fastly Compute.

The first version intentionally supports only one deployable adapter:

| Adapter      | v0 status                | Target                                 |
| ------------ | ------------------------ | -------------------------------------- |
| `fastly`     | Supported                | `wasm32-wasip1`                        |
| `cloudflare` | Future                   | Not implemented in v0                  |
| `spin`       | Future preview candidate | Not implemented in v0                  |
| `axum`       | Excluded                 | No EdgeZero remote deployment contract |

The action does not check out application source. The caller owns checkout,
repository permissions, ref selection, GitHub Environment policy, concurrency,
timeouts, and any health check or rollback process.

The action owns repeatable deployment setup:

1. validate public inputs;
2. resolve the checked-out application directory and optional `edgezero.toml`;
3. resolve Rust from the application or this repository's `.tool-versions`;
4. install `wasm32-wasip1`;
5. install an action-owned EdgeZero CLI from the same EdgeZero commit selected
   by the `uses:` ref;
6. install the pinned Fastly CLI version defined by this repository's tool
   version policy;
7. optionally restore/save an exact-key application build cache;
8. apply the Fastly build-mode policy;
9. run EdgeZero build when required; and
10. run EdgeZero deploy with typed Fastly credentials scoped to provider
    mutation steps.

The core boundary is EdgeZero itself:

```text
edgezero build --adapter fastly
edgezero deploy --adapter fastly
```

Provider-specific staging, deployment IDs, health checks, and rollback are out
of scope for this generic deploy action.

## 2. Design principles

1. **EdgeZero is the deployment boundary.** The action invokes the EdgeZero CLI
   instead of reproducing provider build/deploy logic in YAML or shell.
2. **The caller owns source.** The action never calls `actions/checkout`.
3. **Fastly-only v0, future-compatible API.** The `adapter` input is required
   even though only `fastly` is accepted in v0.
4. **Full-SHA pre-release only.** Initial consumers pin the action to a full
   EdgeZero commit SHA. No `v1` alias, Marketplace publication, or release tag
   is defined by this spec.
5. **Use repository tool versions.** The action's EdgeZero CLI build toolchain
   and default application Rust toolchain come from the EdgeZero monorepo's
   `.tool-versions` file.
6. **Typed provider credentials.** Fastly credentials are passed through action
   inputs, not caller `env:`, so setup and separate build steps do not inherit
   the API token. Deploy is the only provider mutation step that receives the token.
7. **No shell string APIs.** Passthrough arguments are JSON arrays and are
   invoked without `eval`.
8. **Safe by default.** Caching is opt-in, deploys require committed source,
   and provider credentials are not written to outputs, summaries, caches, or
   action-global environment files.

## 3. Goals

1. Deploy any checked-out EdgeZero Fastly application from GitHub Actions.
2. Support same-repository, separate-repository, private-repository, and
   monorepo checkout layouts.
3. Require explicit `adapter: fastly` for v0.
4. Respect the application's `edgezero.toml` when present.
5. Support explicit `working-directory` and explicit `manifest` selection.
6. Build and install the EdgeZero CLI from the action commit selected by
   `uses:`.
7. Install the Fastly CLI reproducibly from a pinned version.
8. Accept typed Fastly credentials and expose them only to provider mutation steps.
9. Support JSON-array build and deploy passthrough arguments.
10. Support opt-in exact-key application `target/` caching.
11. Produce actionable validation failures before deployment begins.
12. Avoid logging provider credentials or action-managed secret values.

## 4. Non-goals

The v0 action will not:

1. check out application source;
2. choose an application ref;
3. deploy Cloudflare Workers, Fermyon Spin, or Axum applications;
4. deploy more than one adapter per invocation;
5. provision Fastly services, domains, dictionaries, config stores, secrets, or
   other provider resources;
6. push runtime config or secrets to Fastly;
7. implement Fastly staging;
8. parse or expose a Fastly service version;
9. perform health checks;
10. perform automatic rollback;
11. configure GitHub job permissions, environments, approvals, concurrency, or
    timeouts;
12. support Windows or macOS runners;
13. publish a stable version alias; or
14. provide a general `setup` action for running arbitrary EdgeZero commands.

## 5. Public action contract

### 5.1 Invocation path

Production examples must pin a full commit SHA:

```yaml
- uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
  with:
    adapter: fastly
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

The action lives at:

```text
.github/actions/deploy/action.yml
```

The root of the EdgeZero repository remains the Rust workspace root and does
not become a GitHub Action entry point.

### 5.2 Inputs

| Input               | Required | Default | Contract                                                                                                                                                                    |
| ------------------- | -------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `adapter`           | Yes      | none    | Must be exactly `fastly` in v0. Unknown adapters, `cloudflare`, `spin`, and `axum` fail before setup.                                                                       |
| `working-directory` | No       | `.`     | Application directory relative to `github.workspace`. Must resolve inside `github.workspace`.                                                                               |
| `manifest`          | No       | empty   | Optional `edgezero.toml` path relative to `working-directory`. If set, the file must exist and is exported as `EDGEZERO_MANIFEST`.                                          |
| `rust-toolchain`    | No       | `auto`  | Explicit application Rust toolchain or automatic discovery.                                                                                                                 |
| `build-mode`        | No       | `auto`  | One of `auto`, `always`, or `never`. For Fastly, `auto` resolves to `never`.                                                                                                |
| `build-args`        | No       | `[]`    | JSON array of strings passed after `edgezero build --adapter fastly --`. Must not contain secrets.                                                                          |
| `deploy-args`       | No       | `[]`    | JSON array of caller-supplied Fastly comment arguments appended after the action-owned deploy flags. Must not contain secrets. Other Fastly deploy args are rejected in v0. |
| `cache`             | No       | `false` | Enable exact-key application `target/` caching. Accepts only `true` or `false`.                                                                                             |
| `fastly-api-token`  | Yes      | none    | Fastly API token. Injected only into the EdgeZero deploy step as `FASTLY_API_TOKEN`.                                                                                        |
| `fastly-service-id` | Yes      | none    | Fastly service ID used by the action-owned deploy flag to prevent accidental service creation.                                                                              |

Boolean inputs accept only the literal strings `true` and `false`.

`build-args` and `deploy-args` must parse as JSON arrays containing only string
values. Strings containing NUL bytes are rejected because operating-system
arguments cannot represent them.

### 5.3 Outputs

| Output                 | Meaning                                                   |
| ---------------------- | --------------------------------------------------------- |
| `adapter`              | Normalized adapter, always `fastly` in v0.                |
| `source-revision`      | Git commit deployed from `working-directory`.             |
| `edgezero-revision`    | EdgeZero action/CLI revision selected by the `uses:` ref. |
| `provider-cli-version` | Installed Fastly CLI version.                             |
| `effective-build-mode` | Resolved build behavior, `always` or `never`.             |

The action intentionally does not expose a Fastly service version or deployment
ID. Provider-specific deployment metadata requires a separate design.

## 6. Checkout and source contract

The caller must check out application source before invoking the action.

### 6.1 Same-repository application

```yaml
jobs:
  deploy:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@<full-commit-sha>
        with:
          persist-credentials: false

      - uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
        with:
          adapter: fastly
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

### 6.2 Separate orchestration and application repositories

```yaml
jobs:
  deploy:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - name: Checkout deployment repository
        uses: actions/checkout@<full-commit-sha>
        with:
          path: deployer
          persist-credentials: false

      - name: Checkout application
        uses: actions/checkout@<full-commit-sha>
        with:
          repository: stackpop/my-edgezero-app
          ref: ${{ inputs.ref }}
          path: app
          persist-credentials: false

      - name: Deploy application
        uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
        with:
          adapter: fastly
          working-directory: app
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

### 6.3 Monorepo application

```yaml
- uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
  with:
    adapter: fastly
    working-directory: apps/api
    manifest: edgezero.toml
    cache: true
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## 7. Execution flow

The action executes these steps in order:

1. Verify the runner is Linux x86-64. GitHub-hosted `ubuntu-24.04` is the
   tested environment.
2. Validate `adapter`; only `fastly` is accepted.
3. Validate exact boolean inputs.
4. Parse `build-args` and `deploy-args` as JSON string arrays.
5. Reject NUL-containing argument values.
6. Reject Fastly deploy passthrough flags that override typed authentication,
   service selection, endpoint selection, or debug/authentication behavior.
7. Resolve `working-directory` beneath `github.workspace` using canonical paths
   and symlink resolution.
8. Fail if the working directory does not exist or is not a directory.
9. If `manifest` is non-empty:

- resolve it relative to `working-directory`;
- fail if it resolves outside `github.workspace`;
- fail if it does not exist or is not a regular file; and
- export it as `EDGEZERO_MANIFEST` for EdgeZero CLI invocations.

10. Resolve the application Git root from `working-directory`.
11. Record `source-revision` and fail if the application working tree has
    uncommitted or staged changes.
12. Resolve the application Rust toolchain.
13. Install the Rust toolchain and `wasm32-wasip1` target.
14. Build/install the action-owned EdgeZero CLI from the action repository root
    at the selected action commit.
15. Install the pinned Fastly CLI.
16. If `cache: true`, restore the exact-key application `target/` cache.
17. Print non-sensitive diagnostics.
18. Resolve `build-mode`:
    - `always` runs a separate EdgeZero build;
    - `never` skips the separate build; and
    - `auto` resolves to `never` for Fastly.
19. If effective build mode is `always`, run:

    ```text
    edgezero build --adapter fastly -- <build-args...>
    ```

    The build step receives no Fastly credential values from typed inputs.

20. In a separate deploy step, set only the typed Fastly API token and run
    EdgeZero deploy with action-owned Fastly CI flags followed by caller
    `deploy-args`:

    ```text
    edgezero deploy --adapter fastly -- --service-id <fastly-service-id> --non-interactive <deploy-args...>
    ```

    The action owns `--service-id` and `--non-interactive` so deployments cannot
    prompt in CI or silently create/select an unintended service.

21. Clean action-owned temporary tool, auth, log, and cache state with
    `if: always()` where GitHub permits cleanup to run.
22. Save the application cache when enabled and safe to save.
23. Set outputs and write a non-sensitive GitHub step summary with
    `if: always()`.

When an argument array is empty, the trailing `--` may be omitted.

## 8. Toolchain and tool installation

### 8.1 Rust toolchain resolution

Application Rust toolchain resolution uses this precedence:

1. explicit `rust-toolchain` input when not `auto`;
2. nearest `rust-toolchain.toml` or `rust-toolchain`, walking from
   `working-directory` to the application Git root;
3. nearest `rust` entry in `.tool-versions` over the same path; and
4. the EdgeZero repository root `.tool-versions` `rust` entry.

At each directory, Rustup-native files take precedence over `.tool-versions`.
Malformed toolchain files fail instead of silently selecting a different
compiler.

The action-owned EdgeZero CLI build also uses the EdgeZero repository root
`.tool-versions` `rust` entry.

### 8.2 EdgeZero CLI installation

The action installs the EdgeZero CLI from the same repository revision selected
by the `uses:` ref. It does not install the CLI from the caller application's
Cargo dependencies, branch, tag, or path.

For v0, the installed executable is the current repository binary name,
`edgezero`. The crate/package remains `edgezero-cli`, but the action invokes the
installed binary. If the project later renames the installed binary, the action
implementation and documentation must change together. The public action
contract should not require callers to know the internal executable path.

The CLI is installed into an action-owned directory below `RUNNER_TEMP` and
prepended to `PATH` for action steps only.

### 8.3 Fastly CLI installation

The Fastly CLI version is pinned by the EdgeZero repository tool-version policy.
The initial implementation should follow the repository `.tool-versions` Fastly
CLI version and record the exact installer artifact and SHA-256 checksum in the
action implementation metadata.

The installer must:

- use an official Fastly distribution;
- verify the downloaded artifact checksum;
- place the binary in an action-owned directory below `RUNNER_TEMP`;
- print `fastly --version`; and
- avoid printing authentication state.

## 9. Build behavior

The public `build-mode` input controls whether the action runs a separate
EdgeZero build before deploy.

| Value    | Behavior                                                                              |
| -------- | ------------------------------------------------------------------------------------- |
| `auto`   | Apply the v0 Fastly policy.                                                           |
| `always` | Run `edgezero build --adapter fastly` before deploy.                                  |
| `never`  | Skip the separate build and rely on deploy to build or consume the required artifact. |

The v0 Fastly `auto` policy is:

| Adapter  | Effective mode | Reason                                                                |
| -------- | -------------- | --------------------------------------------------------------------- |
| `fastly` | `never`        | Fastly `compute deploy` builds unless a prebuilt package is provided. |

`always` is useful for a separate validation build, but Fastly deploy may still
compile again. `never` is the default Fastly behavior and assumes the deploy
command is self-contained.

## 10. Manifest behavior

When `manifest` is empty, the action leaves `EDGEZERO_MANIFEST` unset and runs
from `working-directory`. EdgeZero then applies its normal behavior:

- load `edgezero.toml` from the current working directory when present; or
- use the built-in Fastly adapter fallback when no default manifest exists.

When `manifest` is provided, the action sets `EDGEZERO_MANIFEST` to the
canonical absolute path of that file. Missing explicit manifests are hard
errors.

Provider manifest discovery remains EdgeZero's responsibility. The action must
not guess between multiple `fastly.toml` files. Monorepos with multiple Fastly
manifests should select a deterministic `working-directory` or define explicit
Fastly commands in `edgezero.toml`.

Because v0 injects action-owned Fastly deploy flags after `--`, a manifest-defined
Fastly deploy command used with this action must forward or accept Fastly
Compute deploy/publish flags such as `--service-id`, `--non-interactive`, and
caller-supplied safe flags like `--comment`. Wrapper scripts are allowed, but
they must preserve this contract.

## 11. Fastly credential contract

Fastly authentication uses typed action inputs only:

```yaml
with:
  fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
  fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

The deploy step maps these inputs as follows:

| Input               | Deploy-step use                                                                                                                              |
| ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `fastly-api-token`  | Exported only for deploy as `FASTLY_API_TOKEN`.                                                                                              |
| `fastly-service-id` | Passed as the action-owned `--service-id` deploy flag and may also be exported as `FASTLY_SERVICE_ID` for Fastly CLI fallback compatibility. |

Setup and separate build steps must clear Fastly authentication aliases from
their environments, including values accidentally provided through caller
`env:`.

The deploy step must clear known Fastly authentication and endpoint aliases
before exporting only the typed values needed for v0. This prevents caller `env:`
from silently overriding the typed credential contract.

Application configuration may still be passed through normal workflow `env:`:

```yaml
- uses: stackpop/edgezero/.github/actions/deploy@<full-commit-sha>
  with:
    adapter: fastly
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
  env:
    MY_APP_SETTING: ${{ vars.MY_APP_SETTING }}
```

Callers should not duplicate provider authentication credentials in `env:`.
Runtime secrets should prefer provider-managed runtime secret stores rather
than deploy-time environment variables.

## 12. Passthrough arguments

`build-args` and `deploy-args` are JSON arrays so argument boundaries are
explicit:

```yaml
with:
  build-args: '["--features", "fastly"]'
  deploy-args: '["--comment", "deployed by GitHub Actions"]'
```

The action must:

- parse arrays with `jq` or equivalent safe JSON parsing;
- reject non-arrays;
- reject non-string entries;
- reject strings containing NUL bytes;
- construct commands as Bash arrays;
- never use `eval`; and
- avoid printing the raw JSON input arrays during validation.

Arguments must not contain secrets. EdgeZero, provider CLIs, or manifest-defined
wrapper commands may print command arguments as part of normal diagnostics;
GitHub secret masking is a final defense, not the primary security boundary.

For v0, `deploy-args` are intentionally allowlisted to Fastly deploy comments:
`--comment VALUE` or `--comment=VALUE`. All other caller-supplied deploy args
are rejected so future Fastly flags cannot bypass the typed credential/service
contract, non-interactive mode, or endpoint/debug behavior. The
implementation must maintain accept/reject tests for allowed comments and
blocked service ID, service name, API token, endpoint, profile, interactive,
short-flag, and debug-mode overrides.

## 13. Caching

The `cache` input enables opt-in application build caching.

Caching defaults to `false` because deployment builds run trusted application
code and build output may contain sensitive generated data.

When enabled, the action caches only the canonical application Git root
`target/` directory. It must not cache:

- provider authentication files;
- action-owned tool installations;
- Fastly logs;
- temporary deploy state;
- arbitrary workspace paths; or
- files outside the application Git root.

The cache key must be exact and include at least:

- runner OS;
- runner architecture;
- resolved Rust toolchain;
- Rust target, `wasm32-wasip1`;
- EdgeZero action/CLI revision;
- application source revision; and
- application `Cargo.lock` hash.

The action must not use broad restore prefixes in v0. If `cache: true` and the
application lockfile cannot be found, the action fails before deployment with a
remediation message.

Callers may enable caching only for trusted immutable refs and applications
whose builds do not write secret-derived data into `target/`.

## 14. Logging and summary

The action should log and summarize non-sensitive facts:

- selected adapter;
- resolved application directory relative to `github.workspace`;
- source revision;
- explicit manifest path or default discovery;
- Rust toolchain and target;
- EdgeZero action/CLI revision;
- Fastly CLI version;
- requested and effective build mode;
- cache enabled/disabled and cache key fingerprint, not full secrets or args;
- final result.

The action must not log:

- `fastly-api-token`;
- full process environments;
- application secret values;
- provider authentication state; or
- values written to provider auth files.

## 15. Error handling

All validation and setup failures must stop before invoking Fastly deployment.

Expected failures and diagnostics:

| Failure                                 | Required diagnostic                                                            |
| --------------------------------------- | ------------------------------------------------------------------------------ |
| Missing `adapter`                       | State that `adapter` is required and v0 supports `fastly`.                     |
| Unsupported adapter                     | State that v0 supports only `fastly`.                                          |
| `axum` selected                         | State that Axum has no EdgeZero remote deployment contract.                    |
| `cloudflare` or `spin` selected         | State that the adapter is planned for future work but not implemented in v0.   |
| Invalid boolean                         | Name the input and allowed values.                                             |
| Missing working directory               | Print the workspace-relative requested path.                                   |
| Path escapes workspace                  | Name the input and state that paths must stay under `github.workspace`.        |
| Missing explicit manifest               | Print the workspace-relative requested path.                                   |
| Invalid JSON arguments                  | Name the invalid input without printing its value.                             |
| Non-string argument entry               | State that every array element must be a string.                               |
| Unsupported Fastly deploy arg           | State the allowlist and rejected argument position without printing the array. |
| Rust toolchain cannot be resolved       | List files checked and suggest explicit `rust-toolchain`.                      |
| Dirty working tree                      | State that deployments require committed source.                               |
| Missing `Cargo.lock` when cache enabled | Explain the exact-key cache requirement.                                       |
| EdgeZero CLI installation fails         | Print the action revision and Rust toolchain, not secrets.                     |
| Fastly CLI installation fails           | Print the pinned Fastly version and installer source.                          |
| Missing Fastly credential input         | Name the missing input, never its value.                                       |
| Build command fails                     | Preserve exit status and state that deploy was not attempted.                  |
| Deploy command fails                    | Preserve exit status and state that rollback is caller-owned.                  |
| Cleanup fails                           | Mark the action failed and identify the cleanup area without printing secrets. |

Provider CLI stderr may pass through so Fastly API errors remain actionable.
The action must not construct its own error messages containing credentials.

## 16. Security requirements

1. Require production examples to pin this action and third-party actions to
   full commit SHAs.
2. Do not accept caller-selected EdgeZero CLI source, branch, tag, or commit.
3. Install the EdgeZero CLI from the selected action repository revision.
4. Use the EdgeZero repository `.tool-versions` Rust version for the action CLI
   build and application fallback.
5. Download provider tools only from official release locations and verify
   SHA-256 checksums.
6. Install action-owned binaries below `RUNNER_TEMP`.
7. Use Bash arrays; never use `eval`.
8. Allow-list `adapter` before using it in file selection or command arguments.
9. Treat the checked-out application and `edgezero.toml` as executable code.
10. Require trusted immutable source refs for deployment workflows.
11. Inject Fastly credentials only into the EdgeZero deploy step.
12. Do not write Fastly credentials to `GITHUB_ENV`, `GITHUB_OUTPUT`, caches, or summaries.
13. Clear provider auth aliases from non-provider steps.
14. Reject caller paths outside `github.workspace`, including symlink escapes.
15. Escape percent, carriage return, and newline characters before emitting
    user-influenced GitHub annotations or masking commands.
16. Reject carriage returns and newlines in single-line output values.
17. Disable caching by default and use exact keys only when enabled.
18. Do not automatically retry Fastly deployment. Retries are limited to
    idempotent downloads.
19. Do not use `github.token` for provider authentication.
20. Document least-privilege workflow permissions: `contents: read` unless the
    caller has additional needs.
21. Document caller-owned environment protection, concurrency, and timeouts.
22. Allowlist Fastly passthrough deploy args to comments so caller input cannot
    override typed service selection, authentication, non-interactive mode,
    endpoint, profile, debug controls, or future Fastly flags.

## 17. Testing strategy

### 17.1 Static validation

CI for the action must run:

- `actionlint` over workflow files;
- `shellcheck` over shell scripts;
- YAML parsing for `.github/actions/deploy/action.yml`;
- metadata contract tests for public inputs and outputs;
- a check that no unsupported provider credential inputs exist in v0;
- a workflow security scanner such as `zizmor`;
- checksum verification for provider installer metadata;
- a check that action tool versions agree with `.tool-versions`; and
- Markdown/example validation.

### 17.2 Script contract tests

Use temporary directories and fake binaries to test:

- required `adapter` validation;
- `fastly` acceptance;
- `cloudflare`, `spin`, `axum`, and unknown adapter rejection;
- exact boolean parsing;
- toolchain precedence;
- malformed toolchain files;
- working-directory confinement;
- symlink escape rejection;
- dirty source rejection;
- source revision output;
- explicit and default manifest behavior;
- JSON argument parsing;
- argument boundary preservation;
- rejected non-string and NUL-containing arguments;
- unsupported Fastly deploy-arg rejection, including short override flags;
- build-mode resolution;
- build failure preventing deploy;
- deploy exit-code propagation;
- credential presence validation;
- credentials absent from setup and separate build processes;
- credentials present only in deploy;
- cache key construction;
- missing lockfile failure when cache is enabled;
- cleanup on success and failure; and
- redaction of credentials from action-owned logs.

These tests must not need live Fastly credentials.

### 17.3 Composite-action smoke tests

A GitHub Actions workflow should exercise the local composite action with a
minimal fixture EdgeZero Fastly app.

The smoke test should:

1. check out this repository;
2. create or use a fixture application;
3. install the real pinned Rust and Fastly tools where practical;
4. invoke `./.github/actions/deploy` locally;
5. use manifest build/deploy commands or fake Fastly binaries that write marker
   files instead of contacting Fastly;
6. assert invocation order, working directory, argument boundaries, cache behavior,
   and credential scope; and
7. verify public outputs.

### 17.4 Installer tests

Scheduled or manually triggered CI should verify that the pinned Fastly CLI
installer still produces a runnable binary matching the expected version.

This test verifies installation only and must not deploy.

### 17.5 Live deployment gate

A protected manual workflow should eventually deploy a disposable Fastly fixture
before any stable version alias is created.

The live gate must:

- run only from protected release branches or explicitly approved manual
  dispatch;
- use isolated Fastly resources;
- never run for pull requests from forks;
- verify the deployed endpoint or provider deployment record;
- clean up through provider-specific steps; and
- treat rollback/cleanup as caller-owned provider logic, not generic action
  behavior.

This live gate is not required to publish the initial full-SHA pre-release, but
it is required before advertising a stable version alias.

## 18. Documentation requirements

Before implementation is considered complete, user-facing docs must include:

1. action location and full-SHA pinning guidance;
2. supported adapter table showing Fastly-only v0;
3. runner support;
4. same-repository checkout example;
5. separate-repository checkout example;
6. monorepo `working-directory` and `manifest` example;
7. complete input and output tables;
8. typed Fastly credential guidance;
9. explanation of why provider credentials should not be passed through
   caller `env:`;
10. build-mode behavior;
11. cache behavior and security caveats;
12. trusted-ref requirement;
13. least-privilege permissions example;
14. protected environment, timeout, and concurrency recommendations;
15. explicit non-goals; and
16. future adapter notes for Cloudflare and Spin.

## 19. Acceptance criteria

The v0 design is implemented when all of the following are true:

1. A caller can check out an EdgeZero Fastly application and invoke
   `stackpop/edgezero/.github/actions/deploy@<full-commit-sha>`.
2. The action requires `adapter: fastly`.
3. Unknown adapters, `cloudflare`, `spin`, and `axum` fail before tool
   installation.
4. The action contains no hard-coded application repository, application path,
   Fastly domain, deployment environment, or service ID.
5. The action invokes EdgeZero CLI for build and deploy operations.
6. The EdgeZero CLI is built from the selected action commit.
7. Rust versions come from application discovery or the EdgeZero repo
   `.tool-versions` fallback.
8. Fastly selects `wasm32-wasip1` and installs the pinned Fastly CLI.
9. The caller can select a non-root working directory and explicit manifest.
10. Typed Fastly credentials reach only deploy.
11. Fastly credentials never appear in outputs, caches, action-owned logs, or summaries.
12. Passthrough argument boundaries are preserved.
13. `build-mode: auto` resolves to `never` for Fastly.
14. A failed required build prevents deployment.
15. A failed deployment returns a failing action status and does not trigger
    rollback.
16. `cache: true` uses exact keys and caches only the application Git root
    `target/` directory.
17. Static checks, script contract tests, composite smoke tests, and installer
    tests pass.
18. README or docs examples include same-repository, separate-repository,
    and monorepo checkout models.

## 20. Risks and mitigations

| Risk                                                          | Mitigation                                                                                          |
| ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| EdgeZero CLI and application manifest schema are incompatible | Pin the action to a full EdgeZero commit SHA and publish compatibility notes before stable aliases. |
| Fastly deploy builds while credentials are in scope           | Require trusted immutable refs; keep separate build credential-free; document caching caveats.      |
| Mutable refs execute unexpected manifest commands             | Caller owns checkout; document full SHA/tag protection and GitHub Environment approvals.            |
| Caching stores sensitive generated output                     | Disable by default; exact keys only; cache only `target/`; document when not to enable.             |
| Provider CLI installer changes or disappears                  | Pin versions and checksums; run scheduled installer tests.                                          |
| Monorepo has multiple `fastly.toml` files                     | Require deterministic `working-directory` or explicit `edgezero.toml`; action does not guess.       |
| Generic action grows provider-specific behavior               | Keep staging, rollback, health checks, and deployment metadata out of v0.                           |

## 21. Future work

Future designs may add:

1. Cloudflare Workers deployment;
2. Spin/Fermyon Cloud preview deployment;
3. provider-specific deployment metadata outputs;
4. Fastly staging as a separate provider-specific action;
5. generic or provider-specific health checks;
6. provider-specific rollback actions;
7. reusable setup action for multiple EdgeZero commands;
8. release artifact reuse between build and deploy jobs;
9. prebuilt and attested EdgeZero CLI binaries;
10. stable version aliases such as `v1`; and
11. Linux arm64, macOS, or other runner support.

## 22. References

- EdgeZero CLI reference: `docs/guide/cli-reference.md`
- EdgeZero Fastly adapter: `crates/edgezero-adapter-fastly/src/cli.rs`
- EdgeZero CLI dispatch: `crates/edgezero-cli/src/main.rs`
- Fastly Compute deploy reference: <https://www.fastly.com/documentation/reference/cli/compute/deploy/>
- GitHub Actions secure use reference: <https://docs.github.com/en/actions/security-guides/security-hardening-for-github-actions>
