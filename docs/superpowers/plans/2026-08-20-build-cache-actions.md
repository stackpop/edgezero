# Build Cache Actions Implementation Plan (plan 2 of 5)

> **Execution:** Start only after `2026-08-20-build-cache-container.md` records passing
> `{G,S,D,B}`. Use test-driven development and the gate-rotation procedure for every gate-owned test
> or helper change.

**Goal:** Implement the shared app-source/identity/environment preflight and the optional sccache
restore/compile/save primitive that later plans consume, without making cache availability part of
build correctness or exposing credentials to compilation.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.27 Sections
2 through 5 and 9.

## 1. Fixed decisions

- Pin `actions/cache/restore@v6.1.0` and `actions/cache/save@v6.1.0` exactly.
- Pin host Git LFS to exact `3.7.1` and verify
  `git-lfs-linux-amd64-v3.7.1.tar.gz` against SHA-256
  `1c0b6ee5200ca708c5cebebb18fdeb0e1c98f1af5c1a9cba205a4c0ab5a5ec08` and exact size
  5,524,590 bytes before installation.
- Cache only `${RUNNER_TEMP}/edgezero-sccache-v1`, mounted as `/work/sccache`; never cache target,
  Cargo home, sources, registry data, credentials, or artifacts.
- Cache family is exactly `edgezero-sccache-v1-<platform-id>-<suffix-hash>`, generation is exact
  `job.check_run_id`, and the only restore prefix is `<family>-`.
- `cache: false` selects `uncached-compile` and performs no restore, cache audit, sccache mount/start,
  stats, stop, or save. `cache: true` accepts the documented stale-object and disclosure risks but
  does not authorize save by itself.
- After the required metadata preflight, Cargo has exactly one compile/build invocation. The action
  has no compile retry. Pinned sccache's documented post-`CompileStarted` local fallback is accepted
  internal client behavior.

## 2. Gate update before implementation

- [ ] Add gate-owned failing fixtures and structural tests for the exact cache action versions,
      authority/export boundaries, shared identity/environment/toolchain policy, stable host/path
      literals, key grammar, separate lookup/save predicates, no credential mount/environment, exact
      ordering, warning-only restore/save behavior, and one Cargo compile/build invocation after the
      metadata preflight.
- [ ] Add shell fixtures for sccache 0.10.0 layout/stats, including corrupt records, unexpected paths,
      write errors, stop failure, sparse files, hardlinks, special files, owner mismatch, path limits,
      entry-count limit, and checked-byte overflow.
- [ ] Add closed canonical `.github/actions/deploy-core/host-tools.json`, the trusted Git LFS
      installer, object-first authority materializer, exporter implementation, and focused tests to
      the gate-owned path manifest. These exact runtime helper bytes are part of the gate update; no
      public action metadata or workflow calls them yet. The manifest has exactly the design's literal
      JCS bytes; trusted code constructs the fixed official release URL.
      Permit at most three HTTPS redirects from exact `github.com` to a final exact
      `release-assets.githubusercontent.com` host without credentials. Require final HTTP 200,
      identity encoding, one exact `Content-Length: 5524590`, a streaming cap of 5,524,590 bytes, and
      exact received size. Reject unknown/missing fields, unsupported platforms, a different asset/
      version/size, downgrade/host drift, absent/duplicate/malformed/mismatched length, chunked or
      oversized/partial/trailing transfer, checksum or archive-layout mismatch, and replacement or
      wrong-version execution of the installed binary.
- [ ] Land those tests as a gate-update PR under old `G`, activate the resulting gate revision, and
      complete the full rotation/recovery checklist. Record it as this plan's active gate. Do not mix
      public action wiring/metadata into the gate PR; the reviewed materializer/exporter helper itself
      is gate code and lands here. Do not publish a new image because image-context bytes are unchanged.

## 3. Shared build preflight, identity, and keys

- [ ] Write failing tests for the length-framed `workspace-id` and suffix hash. Commit vectors for root
      `.`, nested workspace, repository IDs of different decimal lengths, empty/255-byte suffixes,
      overlong/control suffixes, malformed UTF-8 paths, and byte-distinct NFC/NFD names.
- [ ] Implement identity calculation in one shared host helper, which remains the sole owner in later
      plans. Validate the authority checkout, canonical `git-root`, workspace and working-directory
      containment, tracked regular `Cargo.lock`, and credential-free `cargo metadata --locked`
      agreement before constructing either hash.
- [ ] Consume, without modifying, the active gate's shared authority materializer and trusted
      exporter. Wire validated repository/ref/token inputs to those immutable helper bytes. They
      install the exact verified Git LFS binary; fetch exact commits with system/global config,
      includes, templates, hooks, credential helpers, worktree creation, filter execution, and
      submodule initialization disabled; recursively validate committed attributes/config/gitlinks;
      then create worktrees with filters disabled and materialize through the absolute LFS binary.
      Require their existing tests for no forbidden command/origin contact, pointer residue,
      configuration races, credential cleanup, and `.git`-free non-hardlinked Copy A/B to pass. Any
      required helper change stops action work and returns to a separate gate rotation.
- [ ] Move the duplicate-rejecting bounded `app-env` decoder, empty Cargo-config/credentials policy,
      path-dependency confinement, and exact `rust-toolchain` comparison into shared helpers here.
      Deny exact `RUSTC`/`RUSTDOC`, every `RUSTC_`/`RUSTDOC_` name, and native-tool/flag prefix and
      suffix forms including `CC_<target>`, `<target>_CC`, `HOST_CC`, `TARGET_CC`, and corresponding
      `ARFLAGS`/`CFLAGS`/`CXXFLAGS`/`CPPFLAGS`/`LDFLAGS` variants. Also deny `CXXSTDLIB`,
      `CXXSTDLIB_STATIC`, `CRATE_CC_NO_DEFAULTS`, all `CRATE_CC_*`, and every other exact design
      control. Commit the full valid/invalid name/value/filter/config/toolchain fixtures and prove both
      cached and uncached profiles reject compiler/wrapper/native-build replacement. Plans 3 and 4
      call these helpers and must not reimplement them.
- [ ] Emit shell-safe typed outputs and reject duplicate, missing, multiline, or malformed output.
      Tests independently recompute SHA-256 bytes; they do not call the implementation as oracle.
- [ ] Implement the sole runner-eligibility helper and require context-derived
      `runner.environment:github-hosted`, `runner.os:Linux`, and `runner.arch:X64`, exact workflow
      repository/path/ref/SHA identity, full lowercase app ref, and canonical positive
      `job.check_run_id` before any cache action runs. The values are not caller inputs; missing,
      empty, differently cased, self-hosted, or architecture-only proofs fail. Plans 3 and 4 must call
      this helper as every public action's first executable step.
- [ ] Implement the sole canonical container-launch helper. It owns the closed operation enum,
      profile-to-environment mapping, sorted env-file serializer, placeholder split-string builder,
      exact `/usr/bin/env -S` argv, `docker create` array construction, env-file deletion before
      start, attach, timeout, named-container removal, and cleanup verification. Its tests require
      exact placeholder/env-file name equality and prove no argv element is constructed by inserting
      an environment value. Plans 3 and 4 may add reviewed enum variants and profile data, but must
      call this serializer and lifecycle unchanged rather than reimplementing them.

## 4. Restore and pre-use audit

- [ ] Compute lookup eligibility before creating the host root or invoking `actions/cache/restore`.
      First authenticate `app-repository`, verify its actual repository id and authority checkout,
      and reject a mismatched caller `app-repo-id`. Same-repository builds are eligible only when that
      verified id equals the event repository id; cross-repository `cache:true` requires boolean
      `disclosure-acknowledged:true`. Missing, false, stringified, or malformed acknowledgement fails
      the action before restore rather than degrading to restore-only. Commit a complete lookup truth
      table, including a forged same-repository id, independent of save authorization.
- [ ] Create the stable host directory fresh and prove its canonical path equals the fixed runner-temp
      child. Require it absent before create, mode 0700, uid/gid 1001, non-mount status, and stable
      device/inode. Reject symlinked runner temp, any preexisting path, root replacement, nested mount,
      wrong owner, or cleanup failure.
- [ ] Restore with exact primary key and sole family prefix. Restore failure, absence, download error,
      or audit failure emits a warning, removes the complete host directory without following links,
      recreates it empty, and continues cold.
- [ ] Implement the audit over descriptor-relative traversal. Accept only expected sccache 0.10.0
      regular files/directories beneath root with container uid/gid and `nlink==1` for files. Reject
      links, sockets, FIFOs, devices, nested mounts, path escape, unknown layout, and arithmetic error.
- [ ] Enforce at most 2,147,483,648 summed regular-file `st_size` bytes, no sparse file
      (`st_blocks*512 < st_size`), at most 100,000 descendants, path length at most 4,096 bytes, and
      component length at most 255 bytes. Do not inspect or predict the cache action's tar/zstd wire
      representation.

## 5. Compile lifecycle

- [ ] Implement a closed `uncached-compile` branch with Copy A, fresh target/Cargo home, and the shared
      validated environment, but no sccache mount, process, socket, wrapper, or `SCCACHE_*` variable.
      Invoke Cargo compile/build exactly once after metadata and test the complete mount/environment/
      process snapshot.
- [ ] Launch the pinned image by digest with read-only root, uid/gid 1001, dropped capabilities,
      `no-new-privileges`, 6 GiB memory/no extra swap, 512 pids, bridge network, numeric integer
      `timeout-minutes` in 1..120, and only the cached-compile mounts from the design.
      Checkout tokens, GitHub file-command paths, provider inputs, and provider tokens must be absent.
- [ ] Construct the exact closed environment, including absolute `RUSTC_WRAPPER`, `SCCACHE_DIR`, 2G
      managed size, `SCCACHE_IGNORE_SERVER_IO_ERROR=1`, zero incremental mode, empty encoded rustflags,
      and validated `app-env`. Use the sole canonical launcher to serialize sorted `NAME=value`
      env-file lines and launch fixed `/usr/bin/env`, literal `-S`, and the exact sorted
      `-i NAME=${NAME}` placeholder string before the absolute Cargo command. Prove inherited
      `RUST_VERSION`, Docker `HOSTNAME`, and seeded poison are absent at target-command entry and no
      argv element is value-derived. Enforce the design's empty Cargo-config policy before launch.
- [ ] Start sccache, zero stats, invoke Cargo compile/build exactly once after metadata with locked inputs, capture exact
      `sccache --show-stats --stats-format=json`, and stop the server. Validate the complete closed
      v0.10.0 `ServerInfo`/`ServerStats` schema before reading `stats.cache_write_errors`: the six
      top-level fields and all 22 exact `stats` fields listed by the design, exact language/count-map
      and duration shapes, `cache_location` equal to `Local disk: "/work/sccache"`, bounded nonnull
      `cache_size`, `max_cache_size:2147483648`, false preprocessor mode, and version `0.10.0`. Reject
      duplicate/unknown/missing fields, wrong types, invalid duration nanoseconds, negative/overflow
      counters, wrong version/cache path, and trailing data. Surface Cargo failure once. A wrapper startup/connection or
      nonaccepted sccache failure is a build failure, not an action retry.
- [ ] On successful compile, require stop success, parse exact 0.10.0 stats, and rerun the complete
      stopped-directory audit. Stop failure, malformed stats, nonzero `cache_write_errors`, or final
      audit failure skips save with a warning but preserves successful build output.

## 6. Save authorization

- [ ] Commit a complete save truth table distinct from lookup eligibility. Save is true only for exact `push` or `workflow_dispatch`, boolean
      `github.ref_protected==true`, boolean `github.event.repository.fork==false`, equal event/current
      repository IDs, passed workflow identity, successful compile/stop/audit, zero write errors, and
      satisfied cross-repository disclosure. Missing, stringified, malformed, or caller-supplied
      substitutes are false.
- [ ] Explicitly cover `pull_request`, `pull_request_target`, `merge_group`, fork events, deleted or
      unprotected refs, repository mismatch, same-repository disclosure exemption, and cross-repository
      acknowledgement. A save-denied row remains restore-only only when lookup eligibility passed.
- [ ] Save with the immutable generation key. Save failure is warning-only. Verify no cache deletion,
      reservation protocol, fallback key, mutable exact-key overwrite, or post-token save exists.
- [ ] After the sole save attempt or every earlier terminal path, remove only the recorded root by
      descriptor-relative no-follow traversal and verify it is absent. Cleanup failure is fatal even
      when restore/save failure was warning-only; test consecutive invocations in one job cannot
      inherit a dirty fixed directory.

## 7. Integration and completion

- [ ] Integrate the cache/compile primitive only into a non-public test harness under the protected
      contract suite. The build-only reusable workflow does not exist before plan 3, and the current
      direct-composite producer must not expose an intermediate cache/provenance contract. Its sole
      change in this plan is to call the shared runner-eligibility helper as its first executable step;
      tests require rejection of missing/malformed/self-hosted context before existing producer work.
      Do not otherwise change provider or public producer behavior in this plan.
- [ ] Run cold, warm, corrupt-restore, concurrent-generation, stop-failure, write-error, audit-failure,
      save-denied, save-warning, and sccache response-loss fixtures. Assert the design's exact cold
      Rust miss/write counters, a new-job warm restore with at least one post-zero Rust cache hit and
      equal binary digest, and complete default-off absence; elapsed time is never evidence.
- [ ] Run shellcheck, the protected contract/container harness, `scripts/run-actionlint.sh`, zizmor,
      and all Rust checks. Defer real reusable-workflow cold/warm evidence to plan 3 after the workflow
      exists. Confirm all non-local actions remain exact-version pinned.
- [ ] Merge through the one-entry queue. Record the resulting commit and active gate revision for plan
      3; do not designate final `P` yet.

**Gate:** the isolated primitive compiles correctly with an empty or unavailable cache; no restored
byte or application process can observe a credential; and only an action-derived authorized event can
attempt a save. It is not a supported producer until plan 3 integrates it.
