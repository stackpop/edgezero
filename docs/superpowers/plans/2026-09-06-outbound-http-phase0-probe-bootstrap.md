# Outbound HTTP Phase 0: Protected Probe Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement
> this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put inert exact-SHA Cloudflare and Fastly probe dispatchers on the default branch
and provision their disposable protected environments before any implementation phase needs
host evidence.

**Architecture:** GitHub delivers `workflow_dispatch` only for workflow files already on the
default branch. Two manually dispatched workflows validate a reviewed SHA without secrets,
then run only that SHA's probe driver in a protected disposable environment. They have no
automatic trigger and publish no outbound API or capability behavior.

**Tech Stack:** GitHub Actions, actionlint 1.7.7, `gh`, disposable Cloudflare/Fastly accounts,
and one authenticated observable probe origin.

---

## Preconditions

- [ ] The outbound design and implementation index are approved.
- [ ] A repository administrator can create protected environments, environment secrets,
  required-reviewer rules, and the fixed `refs/heads/outbound-probe-reviewed` branch.
- [ ] No Phase 4 or Phase 6 capability publication has started. This bootstrap is the sole
  intermediate merge allowed by the implementation index.

## Task Protocol

For each task, make the stated change, run every exact local command, inspect the workflow
permissions and secret boundaries, and commit only the listed files. A skipped job, absent
secret, missing driver, zero probe count, or mismatched SHA is never success.

### Task 1: Add inert default-branch dispatchers

**Files:**
- Modify: `.tool-versions`
- Create: `.github/workflows/outbound-cloudflare-deployed.yml`
- Create: `.github/workflows/outbound-fastly-characterization.yml`

- [ ] Pin actionlint 1.7.7 in `.tool-versions`. Require
  `test "$(actionlint -version | head -n 1)" = "1.7.7"` before validation.
- [ ] Give each workflow only `workflow_dispatch` with one required lowercase 40-hex
  `commit_sha` string input and top-level `permissions: contents: read`. Do not add
  `pull_request_target`, `pull_request`, `push`, `schedule`, reusable-workflow inputs, or a
  user-selectable ref/command/path. Put the exact input SHA in the workflow `run-name` and
  protected job name so an environment reviewer can compare it with the reviewed ref before
  approving secret access.
- [ ] Pin every third-party action to a reviewed full commit SHA and annotate its release name;
  floating major-version tags are forbidden in these secret-bearing workflows. Give both jobs
  finite `timeout-minutes`, disable checkout credential persistence, and do not cache any path
  that candidate code can write across protected runs.
- [ ] Give each provider workflow a fixed repository-wide `concurrency` group with
  `cancel-in-progress: false`. Fastly runs must serialize because they deploy into the same two
  disposable services; a queued run may not cancel an active run before its cleanup trap
  restores prior service state.
- [ ] Add an unprivileged `validate` job with no environment and no secret references. It
  fetches only `refs/heads/outbound-probe-reviewed`, rejects a non-lowercase/full SHA,
  requires that commit to be reachable from the fetched ref, checks out the exact object,
  and requires `git rev-parse HEAD` to equal the input byte-for-byte. Export only that
  validated SHA as a job output.
- [ ] Add a `probe` job that `needs: validate`, binds only its named protected environment,
  checks out the validated SHA, and repeats the byte-for-byte HEAD check. Dependency setup,
  metadata checks, and fixture builds receive no secrets. Reference disposable secrets only
  in the final driver step's `env` block.
- [ ] The Cloudflare driver is exactly
  `npm --prefix crates/edgezero-adapter-cloudflare run test:deployed-timing` and receives only
  `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_WORKERS_SUBDOMAIN`,
  `OUTBOUND_PROBE_ORIGIN_URL`, and `OUTBOUND_PROBE_ORIGIN_TOKEN`. The Fastly driver is exactly
  `bash crates/edgezero-adapter-fastly/tests/host/deployed.sh` and receives only
  `FASTLY_DYNAMIC_SERVICE_ID`, `FASTLY_DYNAMIC_SERVICE_URL`, `FASTLY_DISABLED_SERVICE_ID`,
  `FASTLY_DISABLED_SERVICE_URL`, `FASTLY_API_TOKEN`, `OUTBOUND_PROBE_ORIGIN_URL`, and
  `OUTBOUND_PROBE_ORIGIN_TOKEN`.
- [ ] Before the Cloudflare secret-bearing driver step, install the exact Rust and Node
  versions from `.tool-versions`, add `wasm32-unknown-unknown`, run
  `npm ci --prefix crates/edgezero-adapter-cloudflare`, run the fixture's locked Cargo
  metadata and Worker 0.8.3 tree assertions, install
  `worker-build 0.8.3 --locked`, assert `worker-build --version` plus locked Wrangler's
  version, assert fixture/template compatibility settings, and run
  `npm --prefix crates/edgezero-adapter-cloudflare run build:outbound-fixture`. None of these
  setup/build steps receives provider or origin secrets.
- [ ] Before the Fastly secret-bearing driver step, install the exact Rust and Fastly CLI
  versions from `.tool-versions`, add `wasm32-wasip1`, require
  `fastly version` to report 15.1.0, assert both locked dependency graphs select Fastly SDK
  0.12.1, run the standalone fixture's locked metadata/tree checks, and build it without
  secrets using
  `fastly compute build --non-interactive --dir crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly`.
  Require the fixed
  `tests/fixtures/outbound-fastly/pkg/edgezero-outbound-probe.tar.gz` artifact. The final
  driver may deploy only that prebuilt package; it must not compile, download candidate
  dependencies, or select a different artifact after secrets enter its environment.
- [ ] Run `actionlint .github/workflows/outbound-cloudflare-deployed.yml
  .github/workflows/outbound-fastly-characterization.yml` and `git diff --check`. Then run
  the following negative trigger audit; status 1 is success, a match or scan error is failure:

```sh
if git grep -nE 'pull_request_target|pull_request:|push:|schedule:' -- \
  .github/workflows/outbound-cloudflare-deployed.yml \
  .github/workflows/outbound-fastly-characterization.yml; then
  exit 1
else
  status=$?
  test "$status" -eq 1
fi
```
- [ ] Commit: `ci: bootstrap protected outbound probes`.

### Task 2: Provision disposable protected resources

**Repository/environment state, no source files:**

- [ ] Create `outbound-cloudflare-probe` and `outbound-fastly-probe` as protected
  environments with required maintainers, no self-approval, no production secrets, and no
  deployment branches except the default-branch workflow. Populate only the exact secrets
  listed in Task 1.
- [ ] Create or verify `refs/heads/outbound-probe-reviewed` as a repository-owned protected
  branch with deletion and force pushes disabled. A maintainer reproduces a reviewed tree on
  this ref with a fast-forward-only push before requesting environment approval; workflows
  reject any SHA not reachable from it. Never dispatch a fork SHA directly. The environment
  reviewer compares the SHA shown in the run/job name with the reviewed commit before approval.
- [ ] Provision a disposable Workers account/subdomain and a least-privilege token limited to
  Workers Scripts edit in that account. Provision two disposable Fastly services: one with
  dynamic backends enabled and one with them disabled, plus a token limited to those services.
- [ ] Freeze the observable origin protocol: authenticated `POST /v1/probes/{run_id}/arm`
  creates isolated state; requests use `/v1/probes/{run_id}/{case_id}` for raw bytes, delayed
  headers/chunks, stalled upload reads, and cancellation; authenticated
  `GET /v1/probes/{run_id}/observations` returns timestamped bytes/EOF/disconnect facts;
  authenticated `DELETE /v1/probes/{run_id}` removes the state. Run IDs and tokens are
  unguessable, state is isolated per run, and stale state has a bounded retention policy.
- [ ] Verify the origin arm/observe/delete control plane with a unique smoke run and delete
  it. Verify both provider tokens can inspect only their disposable resources. Record the
  environment settings and resource identifiers in the repository's private operational
  record, never in committed logs or docs.

### Task 3: Merge and prove dispatch availability

- [ ] Open and review the bootstrap PR independently of the implementation branch. Confirm
  its diff contains only `.tool-versions` and the two inert workflow files.
- [ ] Merge it to the default branch. Run
  `gh workflow view outbound-cloudflare-deployed.yml --ref main` and
  `gh workflow view outbound-fastly-characterization.yml --ref main`; both must identify the
  merged workflow and `workflow_dispatch` trigger.
- [ ] Record the bootstrap merge SHA in the outbound implementation PR. Do not dispatch a
  probe until its owning phase has added a reviewed nonzero driver; a missing driver must
  fail rather than skip.

## Phase Verification

- [ ] `test "$(actionlint -version | head -n 1)" = "1.7.7"`
- [ ] `actionlint .github/workflows/outbound-cloudflare-deployed.yml .github/workflows/outbound-fastly-characterization.yml`
- [ ] Repeat Task 1's negative trigger audit and require exit 0 with no matches.
- [ ] `git diff --check`
- [ ] Both workflows are manually visible from the default branch.
- [ ] Both protected environments require reviewer approval and contain only disposable
  resources/secrets.
- [ ] The observable origin smoke state was deleted.

Expected result: later phases can dispatch immutable reviewed SHAs through protected
default-branch workflows without merging any partial outbound implementation.
