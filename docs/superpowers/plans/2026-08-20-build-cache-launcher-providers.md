# Build Cache Launcher and Provider Implementation Plan (plan 4 of 5)

> **Execution:** Start after plan 3 is merged and its provenance gate revision is active. Add the
> launcher/provider adversarial tests to a new gate revision before changing action behavior.

**Goal:** Run the validated app CLI through closed container profiles, freeze every source byte that
can reach a credentialed provider command, and preserve the parent deploy lifecycle without ambient
host state or the legacy `--stage` spelling.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.27 Sections
2, 5, 6.1, 6.6, 7.2, and 9. The parent deploy spec remains normative where the addendum does not
expressly replace it.

## 1. File and ownership boundaries

- Modify the provider composites under `.github/actions/{deploy-fastly,healthcheck-fastly,
rollback-fastly,config-push-fastly}` and their shared `.github/actions/deploy-core` helpers.
- Create `.github/actions/active-version-fastly/action.yml`; preserve plan 3's no-output
  `.github/actions/validate-app-cli-provenance/action.yml` interface while extending its shared runner
  with provider profiles.
- Keep JSON/environment validation, source inventory, output-root ownership, container argv, binary
  recheck, token-ordering, and cleanup in shared typed or narrowly scoped helpers. Provider composites
  may select profiles; they may not reconstruct mount lists or environments ad hoc.
- Do not modify the protocol JSON/archive encoder in this plan. A protocol change returns to plan 3
  and requires a new image/gate/pin sequence.

## 2. Gate update first

- [ ] Add structural fixtures for every operation/profile pair, including exact image digest, network,
      uid/gid, read-only root, capabilities, security options, tmpfs, mounts, environment names,
      resource limits, timeout, command, and token presence.
- [ ] Add hostile source/output fixtures for deleted or modified tracked paths, submodule drift,
      escaping symlinks, mount substitution, overlapping roots, tracked descendants, hardlinks,
      sparse files, special files, nested mounts, inode replacement, cleanup races, and undeclared
      generated output.
- [ ] Add provider lifecycle fixtures for production, staging, first deploy, unhealthy deploy,
      rollback, stale rollback refusal, lost version, cancellation, config push, and mutation output.
- [ ] Prove candidate changes cannot weaken the profile tables, source-freeze checks, binary recheck,
      token boundary, or exclusive `--staging` spelling, then land and activate the new gate revision
      using plan 1's rotation procedure.

## 3. Closed input and environment construction

- [ ] Consume plan 2's sole duplicate-rejecting `app-env` decoder with the exact v1 limits; do not
      introduce a provider-local parser. Require JSON object only,
      valid UTF-8 and at most 65,536 raw bytes before parsing, at most 64 entries, at most 32,768
      aggregate UTF-8 bytes, 1..127-byte ASCII names matching
      `[A-Za-z_][A-Za-z0-9_]*`, values at most 8,192 UTF-8 bytes with no NUL/C0/DEL, and
      ASCII-case-insensitive exact/prefix/target-tool deny rules from the design.
- [ ] Start every target operation with the exact GNU `/usr/bin/env -S` expansion-before-`-i`
      protocol. Emit only the fixed `PATH`, action-owned `HOME`/`TMPDIR`, the operation's exact
      Rust/Cargo variables, validated `app-env`, selected typed `EDGEZERO_*` values, and the one
      provider token required by that profile. Sort final names by ascending ASCII bytes; create the
      exact `-i NAME=${NAME}` placeholder string without literal values. Reject duplicate final names
      and prove caller `PATH`, shell startup variables, wrappers, flags, ambient workflow variables,
      inherited image variables, Docker `HOSTNAME`, and seeded poison do not survive. Cover spaces,
      quotes, backslashes, dollar signs, `#`, `=`, literal `${...}`, and non-ASCII value bytes without
      recursive expansion or splitting.
- [ ] Consume plan 2's shared empty Cargo-config and toolchain checks over the cwd, every ancestor through git root,
      all copied enclosing workspace directories, and fresh `CARGO_HOME`. Reject either Cargo config
      filename and legacy credentials before app code starts.
- [ ] Require the explicit `rust-toolchain` input to equal the pinned image toolchain and confine all
      path dependencies beneath git root. Test exact, missing, inferred-only, nested-workspace, and
      mismatch cases.

## 4. Runner and mount profiles

- [ ] Extend plan 2's closed operation enum with the provider variants; do not add caller-provided
      Docker flags, mounts, environment names, network settings, entrypoint, or command prefix. Supply
      only reviewed profile data, absolute operation executables, and separately validated arguments
      to plan 2's canonical launcher. Reject newline/NUL/control-bearing scalar inputs before logging.
- [ ] Use plan 2's env-file serializer, placeholder builder, Docker create/start/attach lifecycle,
      timeout, and cleanup unchanged. Add provider-profile tests for create/start/attach failure,
      env-file replacement, placeholder/env-file disagreement, newline/NUL values, container-name
      collision, inspectable lifetime, and mandatory removal/reconciliation. Require exact sorted
      placeholder/env-file name equality and prove no Docker or runtime argv element is constructed by
      inserting a token or app-env value.
- [ ] Implement the exact mount table from design Section 5.3. Never mount all of `RUNNER_TEMP`, the
      original checkout writable, the Docker socket, host credential directories, GitHub file-command
      files, or a cache in a token-bearing operation.
- [ ] Enforce the fixed hardened runtime: pinned digest, linux/amd64, uid/gid 1001, read-only root,
      dropped capabilities, `no-new-privileges`, operation-local tmpfs, and the exact design table's
      bridge/none network, memory/no-extra-swap, pid, and wall-time values. Reject Docker host,
      container-sharing, caller-selected network, resource, and timeout flags.
- [ ] Before every app-binary launch, reopen and verify the confined regular path, device/inode,
      SHA-256, size, mode 0755, and link count one. Dynamic binaries run only by direct argv through
      the fixed environment launcher, which directly executes
      `/lib64/ld-linux-x86-64.so.2 --inhibit-cache --glibc-hwcaps-mask '' --library-path
      /opt/edgezero/runtime-lib <binary>`; for static binaries it directly executes the binary. Never use a shell,
      `PATH` lookup, implicit kernel interpreter launch, `ld.so.cache`, default library directory,
      preload file, or hardware-capability substitution.
- [ ] Cover static/dynamic success, wrong interpreter, dependency replacement, hwcaps/default/cache/
      preload substitution, inode swap, symlink/hardlink replacement, malformed ELF, and the explicit
      non-claim for post-startup `dlopen` and child-process behavior.

## 5. Source freeze and generated outputs

- [ ] Define `deploy-fastly` and `config-push-fastly` as the only source-bearing public actions. Give
      each the design's exact repository/ref/id/workspace/cwd/package/bin/toolchain inputs plus required
      sensitive string `app-checkout-token`, supplied from a GitHub secret and masked before use. At
      action start, independently materialize one action-private authority with plan 2's trusted
      object-first helper, verify all five supplied
      `CallerExpectedIdentity` fields, and remove the checkout credential channel before application
      code, provider-token creation, or provider-token injection. Never accept or output an authority
      path, descriptor, or opaque handle from another action. Source-free actions accept none of these
      materialization inputs and no checkout token.
- [ ] For `deploy-fastly`, build Copy B with plan 2's trusted exporter from that action-local,
      credential-free authority. It is a faithful, private, `.git`-free, non-hardlinked, recursive,
      non-sparse copy containing only tracked files and initialized submodules. Keep the authority
      read-only and verify repository id, exact HEAD, index/worktree cleanliness, gitlinks, permitted
      no-filter or pinned-LFS materialization, modes, symlink targets, and full inventory before and
      after app-controlled work. Create one Copy B per invocation; reuse it only among that
      `deploy-fastly` invocation's internal operations, and never share it or its output roots with
      another action.
- [ ] Make `config-push-fastly` the explicit no-Copy-B exception. It executes no app code and mounts
      only its independently materialized credential-free frozen authority read-only. Record and
      verify authority HEAD, index/worktree state, full inventory, and selected tracked
      manifest/file-config identity before token creation, immediately before container start, and
      after the command; apply equivalent identity checks to an action-owned inline config. Add
      substitution/race fixtures at every boundary and fail on any authority, manifest, or config
      change.
- [ ] Parse `generated-output-paths` as the exact bounded canonical JSON array. Reject raw input over
      65,536 bytes, duplicates, overlap, root/dot/git paths, tracked path ancestors, existing roots,
      non-UTF-8 or noncanonical
      segments, components over 255 bytes, joined host paths over 4,096 bytes, escaping parents, and
      any parent reached through a symlink.
- [ ] Resolve the selected `fastly.toml` before app code runs and add exactly the implicit
      `<fastly-project-root>/bin` and `<fastly-project-root>/pkg` roots. Apply the same collision,
      absence, parent-confinement, and ownership rules; do not substitute workspace-root paths.
- [ ] Create every root empty with mode 0700, record its device/inode, and expose repository write
      access only through the corresponding nested writable bind mount beneath read-only
      `/work/repo`. Audit after every app-controlled command and immediately before each token-bearing
      command.
- [ ] Accept beneath roots only real directories and non-sparse regular uid/gid-1001 single-link
      files. Reject symlinks, hardlinks, sparse files, devices, sockets, FIFOs, mounts, ownership
      changes, root replacement, and new paths elsewhere. Across all roots enforce checked totals of
      at most 2,147,483,648 logical bytes and 100,000 descendants plus the component/path bounds.
      Compare all other Copy B bytes/modes/gitlinks to the frozen authority.
- [ ] Implement descriptor-relative, no-follow cleanup on success and failure. Remove only recorded
      action-owned trees, then verify roots are absent and both original and Copy B satisfy their final
      inventories. Cleanup or post-cleanup failure is fatal; a preexisting root is never adopted.

## 6. Credential-free app build and parent target cache

- [ ] Under `build-mode: always`, run exactly one credential-free app-build profile before provider
      deploy. It receives the validated binary, Copy B, fresh Cargo home, action-owned target path,
      validated app environment, and declared/implicit output roots, but no provider token or sccache.
- [ ] If the parent `deploy-fastly.cache` option is enabled, restore its exact-key Cargo target cache
      only after plan 2's lookup-eligibility predicate passes, audit it under the parent's contract,
      and save it after successful credential-free app-build and source/output audit only when plan
      2's full protected-event save predicate passes. Cross-repository use without disclosure
      acknowledgement fails before restore. Save must finish before token minting or injection and is
      never retried or attempted after any token-bearing command. Commit independent lookup/save
      truth-table integration tests for this cache family.
- [ ] Under `build-mode: never`, perform no target-cache restore/save and no credential-free build.
      Provider deploy may still compile with the token for either mode; never claim app-build prevents
      that compile and never expose its token-bearing outputs to a cache.
- [ ] Test cache hit/miss/save warning, build failure, audit failure, cancellation, and token-order
      traces. Every path either saves before token introduction or performs no save.

## 7. Provider lifecycle actions

- [ ] Make the first executable step of every public provider action call plan 2's sole
      runner-eligibility helper with step-local values bound directly from `runner.environment`,
      `runner.os`, and `runner.arch`. Require exact `github-hosted`, `Linux`, and `X64` before artifact
      download, source materialization, Docker, token handling, or mutation. Reject caller-input/env
      substitution, missing values, and self-hosted Linux/X64 fixtures. Contract tests require this to
      remain the first executable internal action step.
- [ ] Make every public provider action accept the named artifact, trusted producer
      `action-version`, and complete `CallerExpectedIdentity`; require its own action repository/ref to
      equal that version, derive PlatformIdentity locally, independently download/validate/smoke the
      artifact, and remove its private workspace with `if: always()`. The two source-bearing actions
      also accept exactly the action-local materialization inputs from Section 5 and independently
      verify caller identity before use. No action accepts a host binary or authority path, exposes
      one as a public output, or trusts another action's platform or materialization state. The
      validation-only action has no outputs and removes its extracted binary before success returns.
- [ ] `active-version-fastly` is source-free and token-bearing. Return an empty version only when the
      provider response confirms a first production deploy; reject malformed, ambiguous, or absent
      state otherwise.
- [ ] `deploy-fastly` validates one binary and reuses it only within that action invocation. Preserve
      the parent's production/staging sequence, rollback target capture, healthcheck order,
      reconciliation, and output semantics. Production probes are tokenless; staging probes receive
      only the token required for the staged endpoint.
- [ ] `healthcheck-fastly` remains source-free. Split production and staging token profiles and reject
      an unexpected token in production rather than silently ignoring it. Enforce retry 1..20, delay
      0..300 seconds, per-attempt timeout 1..300 seconds, checked total-budget arithmetic, and the
      3,600-second maximum before launch.
- [ ] `rollback-fastly` remains source-free, validates the rollback target against current provider
      state, publishes `mutation-attempted` immediately before launch, and reconciles cancellation or
      lost output according to the parent contract.
- [ ] `config-push-fastly` mounts its action-local frozen repository read-only, confines the selected
      manifest and file-backed config directly from that credential-free authority without creating
      Copy B, or creates exactly one action-owned read-only inline file. Derive only the typed named
      config overlay; `no-env` exposes none. Assign the pre-token, pre-start, and post-command
      authority/manifest/config identity checks from Section 5 to this action and prove replacement
      races fail.
- [ ] Every mutating action sets `mutation-attempted` host-side before starting the mutating CLI,
      names the container, forwards termination within bounded time, and runs post-cancellation
      reconciliation. No mutation can precede provenance, binary, source, output-root, and token-order
      checks.
- [ ] Remove every implementation, fixture, and document path that accepts or emits legacy `--stage`.
      Add a repository-wide negative test and use only `--staging` for staged CLI invocations.

## 8. Verification and merge

- [ ] Run deploy-core and every provider action suite; protocol and image tests; source/output hostile
      fixtures; shellcheck; `scripts/run-actionlint.sh`; zizmor; repository Rust tests; and docs/pin
      checks.
- [ ] Run Docker-backed integration tests for every profile and provider lifecycle on linux/amd64,
      including read-only root/non-root behavior, exact mount/environment snapshots, controlled
      loader argv, network denial, signal handling, cleanup, and mutation reconciliation.
- [ ] Review logs, summaries, outputs, cache contents, and artifacts for token/app-env/path leakage.
      Redaction is not proof: tests assert the sensitive bytes were never supplied to disallowed
      processes or persistence surfaces.
- [ ] Merge through the one-entry queue, record the resulting commit and active gate revision, and
      hand both to plan 5. Do not designate final action revision `P` yet.

**Gate:** provider actions execute only an independently validated artifact under a closed profile;
no credentialed command can consume source or generated output that escaped the frozen inventory,
and no post-token byte can enter either cache family.
