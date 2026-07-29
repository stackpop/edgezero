# Deploying from GitHub Actions

EdgeZero ships a set of reusable GitHub composite actions that deploy a
checked-out EdgeZero application to Fastly Compute. They are **layered** so that
adding another provider later does not rewrite the deploy engine, and the
**EdgeZero CLI is the boundary** — the actions never reproduce provider build or
deploy logic in YAML; they compile your CLI, scope credentials, and invoke it.

The design reference lives in
[`docs/specs/edgezero-deploy-github-action.md`](https://github.com/stackpop/edgezero/blob/main/docs/specs/edgezero-deploy-github-action.md);
this page is the practical how-to.

## The three layers

| Action               | Role                                                                                      |
| -------------------- | ----------------------------------------------------------------------------------------- |
| `build-app-cli`      | Compile the CLI package **your app provides** once, publish it as an artifact.            |
| `deploy-fastly`      | Deploy a checked-out Fastly app using that CLI artifact (production, or a staged draft).  |
| `healthcheck-fastly` | Probe a deployed/staged version; exit non-zero when unhealthy so you can gate a rollback. |
| `rollback-fastly`    | Production: activate the previous version. Staging: deactivate the staged version.        |

Under the hood a private `deploy-core` engine (a set of shared scripts) holds all
provider-neutral behavior; the wrappers above are thin.

**Runner support:** Linux x86-64 only (`ubuntu-24.04` is tested).

## What you provide

- **Checkout.** The actions never call `actions/checkout` — you own checkout, ref
  selection, permissions, environments, concurrency, and timeouts.
- **A CLI package.** Name a Cargo package in your own workspace (the crate that
  builds your `edgezero`-based CLI binary) via `app-cli-package`. `build-app-cli`
  compiles exactly that, from your checkout's `Cargo.lock`, so the CLI and your
  app can never disagree on schema.
  - **Required command surface.** The deploy actions drive your CLI, so it must
    expose the built-in commands they invoke: `build`, `deploy`, and — for the
    Fastly lifecycle — `active-version`, `healthcheck`, and `rollback` (plus
    `config` for `config-push-fastly`). The scaffolded template wires all of
    them; if you hand-write your CLI, dispatch each to its `edgezero_cli::run_*`
    handler. Two easy-to-miss requirements when hand-writing:
    - **Initialise the logger.** Call `edgezero_cli::init_cli_logger()` in `main`.
      The handlers print their machine-readable contract lines (`version=<N>`,
      `pushed-key=<key>`, `pushed-store=<id>`, `rolled-back-to=<N>`) via
      `log::info!`; without the logger they are swallowed, so the provider
      mutation SUCCEEDS and the wrapper then fails to parse the output.
    - **Route `config` through the TYPED path.** Dispatch `config push` to
      `run_config_push_typed::<YourAppConfig>` (and `validate`/`diff` likewise) —
      the bundled untyped path returns an unsupported error. It must emit BOTH
      `pushed-key=` and `pushed-store=`, which `config-push-fastly` requires.

    A production deploy runs `active-version` to capture the rollback target and
    fails fast (before touching the provider) if it is missing.

- **Typed provider credentials.** Pass `fastly-api-token` / `fastly-service-id`
  through the wrapper inputs — never through workflow `env:`. They reach only the
  steps that call the provider (the deploy, and the Fastly lifecycle steps such
  as rollback-target capture); a production healthcheck needs none.

## Quick start (same repository)

```yaml
jobs:
  deploy:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false

      - id: cli
        uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
        with:
          app-cli-package: my-app-cli # the CLI crate in your workspace

      - id: deploy # recovery/rollback below reads steps.deploy.outputs.*
        uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

Use a trusted `@<ref>` — a released tag, or a full commit SHA when you need a
reproducible production deploy.

## Separate deployer and application repositories

Check the application into a path and point both actions at it. A **private** app
repository is not readable with the deployer job's default `GITHUB_TOKEN` — mint
an app-scoped token first (a GitHub App installation token, or a fine-grained PAT
with `contents: read`) and pass it to the application checkout.

```yaml
steps:
  - name: Checkout deployer
    uses: actions/checkout@v4
    with:
      path: deployer
      persist-credentials: false

  - name: Checkout application
    uses: actions/checkout@v4
    with:
      repository: stackpop/my-edgezero-app
      ref: ${{ inputs.ref }}
      path: app
      persist-credentials: false
      token: ${{ steps.app-token.outputs.token }} # app-scoped token

  - id: cli
    uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
    with:
      app-cli-package: my-app-cli
      working-directory: app

  - id: deploy # recovery/rollback below reads steps.deploy.outputs.*
    uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
    with:
      app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
      working-directory: app
      fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
      fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Monorepo application

Select the app subdirectory and, when needed, an explicit manifest. Caching keys
on the **Cargo workspace root** for that subdirectory (which in a nested
workspace may be the subdirectory itself), so a monorepo caches the right
`target/`.

```yaml
- id: cli
  uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
  with:
    app-cli-package: api-cli
    working-directory: apps/api

- id: deploy # recovery/rollback below reads steps.deploy.outputs.*
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    working-directory: apps/api
    manifest: edgezero.toml
    cache: true
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Keeping the credential out of the build phase

**First, a limit you cannot design around: deploying an application runs its code
with your provider token.** The deploy step executes the built CLI, and with
Fastly's default `build-mode: never` the deploy _also recompiles the checked-out
application_ (`fastly compute deploy` builds) — both with the token in scope. So
you **must trust the application you deploy**, including its source and its
dependencies. No workflow layout makes it safe to deploy code you do not trust; a
malicious CLI artifact or source tree simply runs with the credential at deploy
time.

Given that, splitting the build into its own job does **not** make an untrusted
app deployable. What it _does_ do is keep the credential entirely out of the long,
dependency-heavy build phase, so a build-phase-only compromise (a build script
that tries to read the environment) cannot reach a token that is not there, and
the credential lives only in the minimal deploy job. That is worthwhile
blast-radius reduction if you want the token's exposure as narrow as possible —
build in one job with no secrets, deploy in another. `build-app-cli` already
uploads the CLI as an artifact, so this needs only `needs:` and a **literal**
`app-cli-artifact` name (step outputs like `steps.cli.outputs.*` do not cross job
boundaries).

```yaml
jobs:
  build:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false
      - id: cli
        uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
        with:
          app-cli-package: my-app-cli
    # No provider secret is available anywhere in this job.

  deploy:
    needs: build
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false
      - id: deploy # recovery/rollback below reads steps.deploy.outputs.*
        uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          app-cli-artifact: edgezero-cli # the build job's artifact name
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

The deploy job re-runs the checkout (to satisfy the committed-source guard) and
downloads the prebuilt CLI; the credential is never present while the build
compiles. It is still present when the deploy runs the built CLI and recompiles
the source — which is why "trust the app you deploy" remains the real boundary.

## Inputs and outputs

### `build-app-cli`

| Input                | Required | Default                 | Meaning                                                                                    |
| -------------------- | -------- | ----------------------- | ------------------------------------------------------------------------------------------ |
| `app-cli-package`    | Yes      | —                       | Cargo package name of the CLI, in your app's workspace.                                    |
| `app-cli-bin`        | No       | `<app-cli-package>`     | Binary name the package produces.                                                          |
| `working-directory`  | No       | `.`                     | App directory (relative to `github.workspace`).                                            |
| `rust-toolchain`     | No       | `auto`                  | Explicit toolchain, or `auto` (rustup files → `.tool-versions`).                           |
| `app-cli-artifact`   | No       | `edgezero-cli`          | Uploaded artifact name.                                                                    |
| `provider-env-clear` | No       | shipped-adapter aliases | JSON array of env var names stripped before your app's code is compiled or run. See below. |

Outputs: `app-cli-version`, `app-cli-package`, `app-cli-bin`, `app-cli-artifact`.

**Keeping provider secrets out of your build.** This step compiles _your_ code and
runs your CLI's `--help`, so it keeps provider credentials out of the environment
in two layers:

- **Static** — the step blanks the shipped adapters' aliases (Fastly, Cloudflare,
  Spin) in its own `env:`. Because they are set there, the step's process never
  had them, and this is the layer that also covers the artifact-upload step.
- **Dynamic** — `provider-env-clear` names any _other_ provider secret your job
  exposes. The step **re-executes** itself with those removed (not merely
  `unset`, which is still readable through `/proc/<pid>/environ` on Linux), and
  the `run:` body `exec`s the script so no ancestor shell keeps a copy either.
  `BASH_ENV`/`ENV` are blanked as well, since a shell sources them at startup —
  before that scrub can run.

Your build code also cannot _easily_ reach out of this step. The script re-execs
itself with **every** GitHub file-command channel removed from its process image
(`env -u GITHUB_ENV GITHUB_OUTPUT GITHUB_PATH GITHUB_STATE GITHUB_STEP_SUMMARY`),
so the process that runs `cargo` — and every child — has none of their real paths
in its environment or `/proc`. A build script therefore cannot recover one path
and derive the sibling `GITHUB_ENV` to append (for example) `LD_PRELOAD` for a
later, secret-bearing step. (Blanking those channels in the step's `env:` would
_not_ work — the runner reinjects reserved `GITHUB_*` values after step-env
evaluation — so the re-exec is what actually enforces this.) Because the compile
step never emits through `GITHUB_OUTPUT`, a **separate** publish step (which runs
no application code) collects the build's outputs and emits them, re-validating
each (`tarball-path`, which drives the upload, is confined to the action's own
temp area).

> **Security boundary — read this.** The build-step scrubbing here defends against
> _accidental_ leakage and low-effort exfiltration; it is **not** a hard boundary
> against a deliberately malicious build. Your build runs as the same OS user as the
> rest of the job, so it can still reach the runner's command storage directly (for
> example by listing `$RUNNER_TEMP/_runner_file_commands/`), read job state on disk,
> or **detach a background process that survives into a later step** (the runner
> reaps orphans at job cleanup, not between steps).
>
> But the deeper point is that **deploying an app inherently runs its code with your
> provider token** — the deploy executes the built CLI and (for Fastly's default
> `build-mode`) recompiles the source, both with the credential. So you **must trust
> the application you deploy**, dependencies included; the scrubbing does not change
> that, and no layout lets you safely deploy code you do not trust. For your own
> first-party app that is a given, and building + deploying in one job (as the
> examples above) is fine. If you want to narrow _when_ the credential is present,
> keep it out of the compile-heavy build phase — see
> [Keeping the credential out of the build phase](#keeping-the-credential-out-of-the-build-phase).
>
> Regardless of layout, pass provider tokens only to the deploy / lifecycle steps,
> and scope any custom secret to the single step that needs it — never to job-level
> `env:`.

The default `provider-env-clear` list repeats the shipped aliases so the dynamic
layer is self-contained. Add your own provider's aliases if you have one:

```yaml
- uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
  with:
    app-cli-package: my-app-cli
    # Defaults cover Fastly/Cloudflare/Spin; add your own provider's aliases.
    provider-env-clear: '["FASTLY_API_TOKEN","ACME_DEPLOY_TOKEN"]'
```

The value must be a JSON array of non-empty variable names; anything else fails
the build rather than silently scrubbing nothing. A custom alias is scrubbed from
the compile step's environment, but — per the security boundary above — treat that
as defense-in-depth, not a guarantee against a malicious build: keep the secret
out of the build's job entirely, or scope it to the one step that needs it.

### `deploy-fastly`

| Input               | Required | Default         | Meaning                                                                                      |
| ------------------- | -------- | --------------- | -------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                         |
| `fastly-api-token`  | Yes      | —               | Injected only into the provider steps (rollback-target capture + deploy); blanked elsewhere. |
| `fastly-service-id` | Yes      | —               | Passed as the typed `--service-id` flag.                                                     |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                             |
| `working-directory` | No       | `.`             | App directory.                                                                               |
| `manifest`          | No       | empty           | Optional `edgezero.toml` path relative to `working-directory`.                               |
| `build-mode`        | No       | `auto`          | `auto` (→ `never` for Fastly), `always`, or `never`.                                         |
| `build-args`        | No       | `[]`            | JSON array passed to `<cli> build`. No secrets.                                              |
| `deploy-args`       | No       | `[]`            | JSON array — allowlisted to `--comment` for Fastly. No secrets.                              |
| `stage`             | No       | `false`         | Deploy to a staged draft version instead of activating.                                      |
| `cache`             | No       | `false`         | Exact-key Cargo-workspace `target/` caching.                                                 |

Outputs: `fastly-version`, `source-revision`, `app-cli-version`,
`provider-cli-version` (the Fastly CLI version this action installed),
`mutation-attempted`, and (production only) `previous-version` — the version that
was active _before_ this deploy. `mutation-attempted` is `true` once the deploy
CLI is invoked; if the action fails, read it via `if: always()` to know a deploy
may have occurred and reconcile rather than assume nothing happened.
Thread `previous-version` into `rollback-fastly`'s `rollback-to` so a later
rollback has a real target (Fastly cannot infer one — see `rollback-fastly`).

**If a _production_ deploy fails with `mutation-attempted=true` but no
`fastly-version`** (the CLI ran and may have activated a version, but its version
line was lost), you cannot roll back blindly — `rollback-fastly` needs the version
to roll back _from_. Recover it from the provider: `active-version` reports the
version that is live **now** (the one the deploy activated); if it differs from
the `previous-version` you captured before the deploy, roll back to that. There is
no `active-version` action, so run the CLI yourself from the same artifact.

Three things to know before you rely on this:

- **The conditions use `failure() || cancelled()`.** A cancel or timeout mid-deploy
  is exactly when a version may have been activated with the line lost, and
  `failure()` alone does **not** cover cancellation.
- **Cancellation recovery is best-effort, not guaranteed.** A step whose `if:`
  includes `cancelled()` is _eligible_ to run after a cancel, but GitHub only grants
  a cancelled job a short grace period, and a job `timeout-minutes` cut-off or the
  runner being reclaimed can skip it entirely; no job or `needs:` structure changes
  that. So treat the durable `mutation-attempted` output — visible in the run — as
  the real backstop: if automated recovery does not complete, an **operator**
  reconciles from it after the fact. (Set a generous job `timeout-minutes` to widen
  the window.)
- **It assumes a single mutation authority for the service.** The recovery treats
  "the version active now" as the one _this run_ activated. If another deployment can
  touch the same service concurrently, that is false and you could roll back
  someone else's deploy — serialize deploys per service (a `concurrency` group, or a
  single deploy pipeline; see
  [Recommended job hardening](#recommended-job-hardening)) before enabling this.

The `id: deploy` on your deploy step is what makes `steps.deploy.outputs.*` below
resolve, and the artifact name is a literal (`edgezero-cli`) so this works whether
the build ran in this job or a
[separate one](#keeping-the-credential-out-of-the-build-phase) — `steps.cli.*` does
not cross job boundaries.

```yaml
- name: Fetch the app CLI for recovery
  if: >-
    (failure() || cancelled()) &&
    steps.deploy.outputs['mutation-attempted'] == 'true'
  uses: actions/download-artifact@<sha>
  with:
    name: edgezero-cli # the same app-cli-artifact name the deploy used
    path: ${{ runner.temp }}/recover-cli
- name: Read the currently-active version
  id: recover
  if: >-
    (failure() || cancelled()) &&
    steps.deploy.outputs['mutation-attempted'] == 'true'
  env:
    FASTLY_API_TOKEN: ${{ secrets.FASTLY_API_TOKEN }} # active-version calls the API
  # Explicit `shell: bash` runs with `-eo pipefail`; the default shell omits
  # pipefail, so a failing `active-version` in the pipe below would be masked by
  # `sed` succeeding, silently yielding an empty version and skipping rollback.
  shell: bash
  run: |
    dir="${{ runner.temp }}/recover-cli"
    tar -C "$dir" -xf "$dir"/*.tar
    bin="$dir/$(jq -r '."app-cli-bin"' "$dir/app-cli-meta.json")"
    v="$("$bin" active-version --adapter fastly \
          --service-id '${{ vars.FASTLY_SERVICE_ID }}' | sed -n 's/^version=//p')"
    echo "version=$v" >>"$GITHUB_OUTPUT"
- name: Roll back only if the deploy activated a NEW version over a known previous one
  if: >-
    (failure() || cancelled()) &&
    steps.deploy.outputs['previous-version'] != '' &&
    steps.recover.outputs.version != '' &&
    steps.recover.outputs.version != steps.deploy.outputs['previous-version']
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-cli-artifact: edgezero-cli
    deploy-to: production
    fastly-version: ${{ steps.recover.outputs.version }} # current (bad) version
    rollback-to: ${{ steps.deploy.outputs['previous-version'] }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

**First-ever deploy** is the one case this cannot automate: if `previous-version`
is empty there is no earlier version to activate, so `rollback-fastly` (which
requires a numeric `rollback-to`) does not apply. If the recovered active version
is non-empty in that case, the deploy activated the service's first version;
undoing it is manual (deactivate or delete that version via the Fastly UI or CLI).

**Staging is different.** `active-version` returns the _production_-active version,
so it cannot reveal a staged version. And a staged deploy does not leave a merely
"inactive draft": the CLI runs `service-version stage` before it emits the version,
so a lost-version failure may leave a version that is **already staged — serving
staging traffic via the staging selector**, not just an unactivated draft. There is
no captured version to pass to `rollback-fastly --staging`, so recover manually:
list the service's versions (`fastly service-version list`), identify the stray
staged version, and un-stage/deactivate it.

The action always adds `--non-interactive` to the deploy itself, so a deploy
declared as an `edgezero.toml` command (`[adapters.fastly.commands] deploy =
"fastly compute deploy"`) cannot block on a prompt in CI. You do not need to —
and cannot — pass it through `deploy-args`.

### `healthcheck-fastly`

| Input               | Required | Default         | Meaning                                                                                                        |
| ------------------- | -------- | --------------- | -------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                                           |
| `fastly-api-token`  | Staging  | —               | Needed only for `deploy-to: staging` (staging-IP resolution); a production probe needs none and receives none. |
| `fastly-service-id` | Yes      | —               | Service to probe.                                                                                              |
| `fastly-version`    | Yes      | —               | Version to probe — thread the deploy's `fastly-version`.                                                       |
| `domain`            | Yes      | —               | Domain to probe, e.g. `www.example.com`.                                                                       |
| `path`              | No       | `/`             | URL path to probe (must begin with `/`). Covers production and staging alike (staging reroutes the same URL).  |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                                               |
| `deploy-to`         | No       | `production`    | `staging` probes the staged version via its resolved edge IP.                                                  |
| `retry`             | No       | `3`             | Attempts before declaring the deployment unhealthy.                                                            |
| `retry-delay`       | No       | `5`             | Seconds between attempts.                                                                                      |
| `timeout`           | No       | `10`            | Per-attempt timeout in seconds.                                                                                |

Outputs: `healthy`, `status-code`.

**This action fails when the deployment is unhealthy** — that is the point. Gate
your rollback on the step failing (`if: failure()`), not on the `healthy` output.

### `rollback-fastly`

| Input               | Required | Default         | Meaning                                                                                                                                                               |
| ------------------- | -------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                                                                                                  |
| `fastly-api-token`  | Yes      | —               | Fastly API token.                                                                                                                                                     |
| `fastly-service-id` | Yes      | —               | Service to roll back.                                                                                                                                                 |
| `fastly-version`    | Yes      | —               | The current (bad) version to roll back **from**.                                                                                                                      |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                                                                                                      |
| `rollback-to`       | No\*     | empty           | **Production only:** the version to re-activate. Wire it from `deploy-fastly`'s `previous-version` output. Required for `deploy-to: production`; ignored for staging. |
| `deploy-to`         | No       | `production`    | `production` activates `rollback-to`; `staging` deactivates the staged one.                                                                                           |

Outputs: `rolled-back-to` (production only — the version that was activated) and
`mutation-attempted` (`true` once the rollback CLI is invoked; read it via
`if: always()` on failure to know the active version may have changed).

\* Fastly's version metadata cannot distinguish a previously-live version from a
staged draft, so a production rollback **cannot infer its target** — you must
supply it. Capture it at deploy time: `deploy-fastly` emits `previous-version`
(the version active _before_ that deploy), which you thread straight into
`rollback-to`. A production rollback with no `rollback-to` fails closed rather
than guess a version.

### `config-push-fastly`

Pushes your app's typed config to a Fastly config store. This is **separate from
deploy** — deploy activates code, it never writes runtime config — so you run it
as its own step, whenever config should move.

| Input               | Required | Default         | Meaning                                                                                                              |
| ------------------- | -------- | --------------- | -------------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                                                 |
| `fastly-api-token`  | Yes      | —               | Fastly API token. Injected only into the push step.                                                                  |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                                                     |
| `working-directory` | No       | `.`             | App directory (holds the manifest + typed config).                                                                   |
| `manifest`          | No       | empty           | `edgezero.toml` path relative to `working-directory`.                                                                |
| `app-config`        | No       | empty           | Typed config file path (default: resolved from the manifest). Mutually exclusive with `app-config-inline`.           |
| `app-config-inline` | No       | empty           | Raw typed-config (TOML) supplied inline — for config that lives in a GitHub variable with no file on disk.           |
| `no-env`            | No       | `false`         | `true` passes `--no-env` so the CLI does not overlay `<APP_NAME>__…__<KEY>` env vars onto the config before pushing. |
| `store`             | No       | empty           | Logical config-store id (default: the manifest's resolved id).                                                       |
| `key`               | No       | empty           | Explicit base key for a **production** push (default: the logical store id). Not allowed with `deploy-to: staging`.  |
| `deploy-to`         | No       | `production`    | `staging` writes the `<logical-store-id>_staging` variant in the **same** store.                                     |

Outputs: `pushed-key` (the key written — the base key, or the derived `_staging`
variant), `store` (the logical store id the CLI **resolved** — always emitted,
not only when the `store` input was supplied), `provider-cli-version` (the Fastly
CLI version this action installed), and `mutation-attempted`. Your app CLI must
print both `pushed-key=` and `pushed-store=`; the action fails if either is
missing — but `mutation-attempted` is `true` once the push CLI is invoked, so on
that failure you can read it via `if: always()` and reconcile the config store
rather than assume it is unchanged.

**Staging config is the same store, a different key.** Fastly config stores are
not versioned like staged service versions, so `deploy-to: staging` writes your
config under `<logical-store-id>_staging` alongside the production key — never
overwriting what the live service reads. The staging key is _derived_ from the
store's logical id, so `key` is production-only: combining `key` with
`deploy-to: staging` is rejected up front (an explicit staging key would be
written where no staged version ever reads).

What makes a _staged version_ actually read that key is the other half: a staged
deploy re-points its own `edgezero_runtime_env` link at a **per-service**
`edgezero_runtime_env_staging_<service-id>` selector store, mirroring
production's runtime overrides into it and redirecting only the config selectors
to `<logical-store-id>_staging`. The staged deploy creates and populates that
twin on demand — no separate setup step — so the staged version reads
`<logical>_staging` while production keeps reading `<logical>`. (The store is
named per service because Fastly config stores are account-wide and versionless,
so a shared twin could let one service's staged deploy clobber another's.) If the deploy
cannot even read the store listing (so it cannot tell whether production config
exists), it fails closed rather than risk serving production config. A typo like
`deploy-to: Staging` is likewise rejected up front, never silently pushed to
production.

## Strict lifecycle values (fail closed)

`stage` and `deploy-to` are validated exactly, and a bad value **fails the run**
rather than falling back to production:

- `stage` must be exactly `true` or `false`.
- `deploy-to` must be exactly `production` or `staging`.

A typo like `stage: True` or `deploy-to: Staging` is rejected up front — it will
never silently deploy to, probe, or roll back production.

## Credentials

Fastly credentials are typed inputs, not workflow `env:`. Setup and build steps
never see the token, and it never reaches outputs, caches, logs, or step
summaries. Do not duplicate provider credentials in `env:`; prefer
provider-managed runtime secret stores for application secrets.

The deploy step enforces a hard credential boundary: before the CLI runs, every
known provider alias (`FASTLY_TOKEN`, `FASTLY_ENDPOINT`, `FASTLY_API_URL`, …) is
**cleared**, and only the typed values you passed are exported. An inherited
`FASTLY_ENDPOINT` or `FASTLY_TOKEN` from the surrounding workflow cannot reach
the deploy.

Deploy runs trusted application code: because Fastly's default `build-mode:
never` lets `fastly compute deploy` build during deploy, the application is
compiled while the token is in scope. **Deploy only trusted, immutable refs**
(full SHAs or protected tags) and use GitHub Environment approvals.

## Fastly staging lifecycle

Staging parity with `stackpop/trusted-server-actions` is supported for Fastly.
The capability is scaffolded into the CLI's Fastly adapter and exposed through
your app CLI; the actions are thin wrappers. You wire the trio — the actions
carry no orchestration policy of their own.

```yaml
- id: cli
  uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
  with: { app-cli-package: my-app-cli }

- id: stage
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    stage: true
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}

- id: check
  uses: stackpop/edgezero/.github/actions/healthcheck-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    deploy-to: staging
    domain: staging.example.com
    fastly-version: ${{ steps.stage.outputs.fastly-version }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}

- if: >-
    (failure() || cancelled()) && steps.stage.outputs.fastly-version != ''
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    deploy-to: staging
    fastly-version: ${{ steps.stage.outputs.fastly-version }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

- `deploy-fastly` with `stage: true` clones the active version, uploads the built
  package to a new draft, marks it staged, and outputs `fastly-version`.
- `healthcheck-fastly` resolves the staged version's Fastly staging IP and probes
  it, retrying and exiting non-zero when unhealthy.
- `rollback-fastly` deactivates the staged version (or, for `deploy-to:
production`, activates `rollback-to`).

### Production rollback needs an explicit target

A production version, once superseded, cannot be told apart from a staged draft
in Fastly's version metadata — so a production rollback **cannot infer** what to
re-activate. Capture the target at deploy time and thread it through:

```yaml
- id: deploy
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}

# ... run your production health checks here ...

- if: >-
    (failure() || cancelled()) && steps.deploy.outputs.fastly-version != '' &&
    steps.deploy.outputs.previous-version != ''
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    fastly-version: ${{ steps.deploy.outputs.fastly-version }}
    rollback-to: ${{ steps.deploy.outputs.previous-version }}
```

The rollback needs **both** outputs, so it guards on both. `previous-version` is
empty on a first-ever deploy (nothing to roll back to). `fastly-version` is empty
when the deploy failed before/without reporting a version — in that case do **not**
call rollback with an empty `fastly-version` (it would fail with a misleading
secondary error); if `mutation-attempted` is `true`, follow the lost-version
recovery above to obtain the current version first. The `failure() || cancelled()`
guard covers a cancel/timeout mid-deploy, but — as noted for recovery — that path
is best-effort; the durable `mutation-attempted` signal is what a later reconcile
relies on.

## Build behavior and caching

`build-mode: auto` resolves to `never` for Fastly, because `fastly compute
deploy` builds unless a prebuilt package is provided. `always` runs a separate
credential-free validation build first; the deploy may still recompile.

Caching is opt-in (`cache: false` by default) and, when enabled, caches only the
Cargo workspace root `target/` under an exact key (runner OS/arch, toolchain,
target, CLI version, source revision, and `Cargo.lock` hash). Enable it only for
trusted, immutable refs.

## Recommended job hardening

Serialize on the **Fastly service**, not the ref. Every deploy and rollback for a
service mutates the same live resource, so a service-scoped group is what
actually prevents two workflows (or two refs) from racing each other:

```yaml
permissions:
  contents: read
concurrency:
  # Service-scoped: all deploys AND rollbacks for this service run one at a time.
  group: fastly-${{ vars.FASTLY_SERVICE_ID }}
  cancel-in-progress: false
```

**Why this matters for rollback.** A production rollback checks that the version
it is rolling back _from_ is still the active one and refuses otherwise, so a
stale rollback will not clobber a much newer deploy. That check is **best-effort,
not atomic**: Fastly's activate endpoint takes no precondition, so a deploy that
lands in the window between the check and the activation can still be
overwritten. The guard only narrows that window.

Serialization closes it **only when every mutation shares one deployment
authority.** A GitHub `concurrency` group is scoped to a single repository's
runs, so it serializes deploys and rollbacks _within that repo_ — it cannot
serialize another repository's workflow, a `fastly` CLI run from a laptop, or a
Fastly-console activation. If more than one authority can activate versions on
the service, route them all through the same serialized workflow (or accept the
residual race).

Add `timeout-minutes`, a protected GitHub Environment with required reviewers,
and pin third-party actions to readable released tags (or full SHAs for
production).

## Non-goals

The actions do not check out source, expand or convert configuration, or push
runtime config as a side effect of deploy. Config push and provisioning are
explicit subcommands you run as separate steps — via the `config-push-fastly`
action, or your **app-owned** CLI's `<app-cli> config push` / `<app-cli> provision`
(the typed `config push` is only available on your app's CLI; the bundled
`edgezero` binary has no typed config in scope and returns an unsupported error).
Cloudflare and Spin deploy wrappers are future work; today these actions target
Fastly.
