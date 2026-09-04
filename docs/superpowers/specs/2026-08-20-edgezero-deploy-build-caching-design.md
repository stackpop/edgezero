# EdgeZero Deploy Actions - Build Caching Spec

**Status:** Design (proposed) - v6.26

**Related:** `docs/specs/edgezero-deploy-github-action.md`,
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's native CLI without caching. A cross-repository deployer
therefore recompiles the full dependency graph on every run. The solution must also work for real
EdgeZero applications whose crates are public Git dependencies, not only crates.io packages.

The design must preserve the existing deploy, staged deploy, healthcheck, rollback, and config-push
contracts. It must not expose provider credentials to app CLI compilation or to restored cache data.

## 2. Scope and trust model

- The application repository and the app code being compiled are trusted. This includes `build.rs`,
  proc macros, manifest commands, and any native tools they invoke.
- Cache writes run only for authorized deployer events and protected refs. The deployer repository
  owns the repository-scoped GitHub Actions cache and trusts every workflow allowed to write its
  default-branch cache scope.
- The app checkout token and provider token are trusted credentials, but they have disjoint uses.
  The checkout token is host-only. The provider token exists only in the minimum provider operation
  that requires it. Neither credential enters the cached-compile container or `SCCACHE_DIR`.
- The reusable workflow is the only supported artifact producer. It is build-only: it accepts no
  provider inputs and performs no provider mutation.
- Artifact provenance is a consistency and loadability check. It is not producer authentication or
  an attestation. A malicious producer can create a self-consistent archive. Attestation remains out
  of scope.
- Caching has an accepted correctness risk: sccache can miss undeclared filesystem or environment
  inputs, including changed `app-env` values, read by `build.rs` or proc macros. A stale object can
  pass digest, ELF, and smoke checks.
  `cache: true` explicitly accepts this risk; v1 does not and cannot generally detect it.
- v1 supports GitHub-hosted `linux/amd64` runners only. It fails closed on self-hosted runners and
  does not target GitHub Enterprise Server.
- Image publication trusts the digest-pinned official Rust base, Rustup's manifest/checksum chain,
  Debian's signed package archive, and checksum-pinned Fastly/sccache release assets. Candidate source
  cannot change their coordinates or verification steps. The build is not claimed byte-reproducible
  across time: captured leaf digest `D` plus post-build tool/capability verification is the release
  identity.
- Host source materialization trusts the exact checksum-pinned official Git LFS release asset and the
  bounded GitHub release redirect path defined in Section 5.2. It never trusts a runner-preinstalled
  Git LFS binary or a repository-selected download coordinate.

## 3. Terminology and identity

### 3.1 Caller and platform identity

`CallerExpectedIdentity` is the caller-controlled identity that both producer and consumer verify:

- `app-repo-id`: canonical decimal GitHub repository id, verified against `app-repository` through
  the GitHub REST API.
- `source-revision`: the full lowercase 40-hex commit SHA checked out from the app repository.
- `app-cli-package`: Cargo package name.
- `app-cli-bin`: binary name.
- `workspace-id`: the canonical workspace identity described below.

`PlatformIdentity` is action-controlled:

- `platform-id`: the `sha256:<64-lowercase-hex>` image manifest digest from the local action
  revision's `.github/docker/build-app-cli/image.json`.
- `container-ref`: `<repository>@<platform-id>` from that same file.
- `provenance-protocol`: the exact protocol integer from that same file.

Every EdgeZero action derives `PlatformIdentity` locally. Callers cannot provide or override it, and
the reusable workflow does not expose it as an output. Artifact metadata contains both identity
groups so the consumer action can compare caller values and its locally derived platform values.

The app checkout token is used host-side by the reusable producer, the public
`compute-app-cli-identity` action, and each source-bearing provider action to verify and materialize a
private repository independently. It is never copied into a container, exported working copy,
artifact, or cache, and no materialized authority path or handle crosses a public action boundary.

### 3.2 Canonical hashes

Identity hashes use SHA-256 over a fixed-order, length-framed byte encoding. Each UTF-8 field is
encoded as `<byte-length>:<bytes>`, where the length is ASCII decimal with no leading zeroes.

Paths are relative to `git-root`, `/`-separated, have no empty, `.`, or `..` segment, and have no
trailing slash. The root is represented by the single byte `.`. Paths are hashed byte-exactly with
no Unicode normalization; non-UTF-8 paths fail closed.

`workspace-root` must canonicalize beneath `git-root`; `working-directory` must canonicalize beneath
`workspace-root`; and credential-free `cargo metadata --locked` from `working-directory` must report
that exact workspace root. Its `Cargo.lock` must be a tracked regular file. A caller-provided root is
never trusted without those checks.

- `workspace-id` is SHA-256 over the concatenation
  `<repo-id-byte-length>:<canonical-repo-id><root-byte-length>:<canonical-root>` with no separator or
  newline, and is rendered as `sha256:<64-lowercase-hex>`.
- `cache-key-suffix` is valid UTF-8 from 0 through 255 bytes with no Unicode control character.
  `suffix-hash` is SHA-256 over `<suffix-byte-length>:<suffix-bytes>` with no newline and is rendered as
  64 lowercase hexadecimal characters without a `sha256:` prefix.

Committed golden vectors cover framing, both digest renderings, the root representation, empty and
255-byte suffixes, rejected oversized/control-character suffixes, and byte-distinct NFC and NFD paths
that must produce different hashes.

### 3.3 Workflow and action revisions

`app-ref` must be a full lowercase 40-hex commit SHA. Branches, tags, abbreviated SHAs, and the
legacy `--stage` spelling are unsupported. Staged provider operations use only `--staging`.

Every non-local external action and reusable workflow reference in this repository and in documented
consumer workflows must use an exact patch-version tag of the form
`v<major>.<minor>.<patch>`, with canonical decimal components and no leading zero except the value
zero itself. Major-only tags, minor-only tags, prereleases, build metadata, branches, commit SHAs,
abbreviated SHAs, and floating names such as `main` or `latest` are not accepted. Docker action refs
remain digest-only. All EdgeZero references in one consumer workflow use one exact action version.
Third-party refs and released EdgeZero repository/documentation surfaces must name stable releases.
Before the first stable EdgeZero release exists, the four prepublication adoption documents may use
literal `<EDGEZERO_ACTION_VERSION>` under the dual-state documentation gate defined below; those
examples are intentionally non-runnable until documentation revision `R`. The plan-5 disposable
release fixture alone may use distinct candidate `C`, which has the same exact patch-version grammar
but belongs to a GitHub Release whose `prerelease` field is `true`; `C` never contains a SemVer
prerelease suffix. That exception qualifies candidate commit `H` before final `V` is published.

In this document, pinning a non-local GitHub action or reusable workflow always means the exact
patch-version tag above, never a commit SHA. Commit SHAs remain mandatory only where they identify
source or execution provenance: `app-ref`, `job.workflow_sha`, `action-revision`, protected-head
commit `Q`, rollout commits `G`/`S`/`B`/`H`/`P`/`R`, and the organization required-workflow descriptor
bound to gate commit `G`.
SHA-256 values remain content identities for images, archives, binaries, fixtures, and hashes; they
are not GitHub `uses:` refs.

This is an explicit usability tradeoff, not a claim that ordinary Git tags are immutable. A
third-party publisher can move or delete a version tag or create a same-named branch that introduces
symbolic-ref ambiguity; those supply-chain risks are accepted for the separately reviewed, trusted
actions listed by the implementation plans. Initial review must still prove a published stable
release tag exists, no same-named branch exists, and the recorded resolved commit is the reviewed
release commit. EdgeZero's own `V` is
stronger: repository immutable releases must be enabled, `V` must be a published immutable release
whose target is executable commit `P`, and the no-bypass action-version tag ruleset must prohibit
update and deletion. A release version is never silently retargeted; a correction receives a new
patch version.

Inside the called workflow:

- `job.workflow_repository` and `job.workflow_file_path` must identify the expected EdgeZero
  reusable workflow.
- `job.workflow_ref` must be exactly
  `stackpop/edgezero/.github/workflows/build-app-cli.yml@refs/tags/<action-version>` where the action
  version is stable `V` or, only in the release fixture, candidate `C`;
- `job.workflow_sha` must be a full lowercase 40-hex commit SHA and is the resolved executable
  revision for that invocation (`H` under `C`, final `P` under `V`). It is not compared textually with the tag-bearing
  `job.workflow_ref`.

These hosted-runner context properties identify the workflow that defines the current job. They are
part of the hosted-only v1 floor.

`Q` denotes the exact protected-default-branch commit whose post-merge `push` run is being examined.
It is generic: during this rollout it may be a gate candidate `G'`, release source `S`, pin baseline
`B`, an implementation commit, or final action revision `P`. The generic main-push assertion proves
workflow/context identity at `Q`; a release check separately proves that `Q=S` when qualifying the
image source. `H` is reserved for the final action candidate defined in Section 8.

After those checks, the reusable job uses `actions/checkout@v7.0.1` only to place repository
`stackpop/edgezero` at `ref: job.workflow_sha` in a fixed private action-source directory with
`persist-credentials:false`. The trusted Section 5.2 materializer fetches the application at
`app-ref` into a distinct fixed private authority root without initially creating a worktree, proves
the committed filter/submodule policy, and only then checks out materialized bytes. The EdgeZero
checkout must have exact HEAD `job.workflow_sha`, repository id/name, clean state, and no submodule,
LFS, sparse, or untracked content. Every local composite/helper invocation resolves beneath that
verified EdgeZero root. No `./...` action or helper path may resolve against the application
authority, and neither root may overlap, contain, or symlink into the other. The app token is removed
from Git configuration and the credential channel before the workflow exports tracked files and
supported submodules into non-hardlinked Copy A. The authority remains read-only and is verified
before and after use; Copy A contains no `.git`, checkout credential, ignored file, or untracked file.

## 4. Cache design

### 4.1 Cached data and fixed paths

For the reusable workflow's native `build-app-cli` compile, `CARGO_TARGET_DIR` is fresh on every run
and is never cached; only `SCCACHE_DIR` is archived. The pinned image supplies
`/usr/local/bin/sccache` v0.10.0, and cached compilation sets its absolute path as `RUSTC_WRAPPER`.

This section defines `build-app-cli.cache`, the reusable workflow's native CLI compilation cache.
It does not replace the parent's distinct `deploy-fastly.cache`: under `build-mode: always`, the
consumer may restore and save that exact-key Cargo target cache only around the credential-free
`app-build` profile below, before any provider token is introduced. `build-mode: never` receives no
target-cache restore or save.

The host cache path is the fixed `${RUNNER_TEMP}/edgezero-sccache-v1`, emptied before restore. It is
mounted at the constant `/work/sccache`. The fixed host path is required because `actions/cache`
includes the archived path in its cache version. The fixed in-container path and `/work/repo` cwd
also avoid path-only misses in sccache keys.

The cache contains compiled outputs, indexes, and replayable compiler stdout/stderr. Diagnostics can
contain paths, source excerpts, warnings, and compile-time values. Dependency sources, Cargo registry
or Git checkouts, `.crate` archives, credentials, and `CARGO_HOME/bin` are not intentionally cached.

The compile environment has no dependency credentials. Both cached and uncached builds therefore
support only anonymously fetchable crates.io and public Git dependencies. `cache:false` selects a
separate `uncached-compile` profile: after the required metadata preflight, Cargo has exactly one
compile/build invocation with a fresh target/Cargo home and
no sccache mount, process, wrapper, socket, or `SCCACHE_*` variable. It is not the cached profile with
cache steps merely skipped.

### 4.2 Keys, restore, and save

The cache family is exactly:

```text
edgezero-sccache-v1-<platform-id>-<suffix-hash>
```

The primary key is `<family>-<generation>`, where generation is `job.check_run_id`. The only restore
prefix is `<family>-`. `app-cli-artifact` does not affect cache identity; it is unique only because
GitHub artifact names share a run-level namespace.

Each successful writer creates a new immutable entry. Concurrent jobs in one family restore the
newest available entry and fork from it. Their results are not merged, so only one lineage may remain
the newest. This lost warmth is accepted.

There is no cache reservation or fail-closed save protocol. Standard `actions/cache/save` is
best-effort and save failures are warnings. GitHub cache restore, cache absence, and cache save
availability never determine build success. A failure of the compiler-wrapper process itself can
still fail compilation as described below.

GitHub cache storage and eviction are repository-global. The rolling generations can evict unrelated
workflow caches. Entries not accessed for seven days may be removed. This cost and eviction behavior
is accepted; v1 performs no cache deletion.

### 4.3 Restore and runtime failure contracts

For `cache:true`, the sequence is:

1. Authenticate `app-repository`, verify its actual repository id and the authority checkout, then
   validate cache eligibility from that verified id, including the cross-repository disclosure
   acknowledgement, before creating the cache root or invoking a cache action. The unverified
   `app-repo-id` input can never obtain the same-repository exemption.
2. Empty the stable host cache directory.
3. Restore the newest matching cache.
4. Audit restored data. On restore or audit failure, clear the directory and continue cold.
5. Start sccache, zero its statistics, and compile once.
6. Capture `sccache --show-stats --stats-format=json` and stop the server.
7. Audit the stopped directory again.
8. Save only when the compile succeeded, stop succeeded, the final audit passed, the captured
   `cache_write_errors` count is zero, and the save-authorization predicate below is true.

`cache: true` authorizes lookup and restore, not publication. Saving is permitted only when all of the
following action-derived conditions hold: the event is exactly `push` or `workflow_dispatch`,
`github.ref_protected` is the boolean `true`, `github.event.repository.fork` is the boolean `false`,
the event repository id equals `github.repository_id`, the workflow identity checks in Section 3.3
passed, and the disclosure requirement in Section 4.4 passed. Pull-request events
(including `pull_request_target`), `merge_group`, forks,
unprotected refs, missing context, malformed values, and caller-supplied substitutes are restore-only
only after lookup eligibility passed. A cross-repository request without
`disclosure-acknowledged:true` is an input error before restore, not a restore-only row. The cache plan
commits separate lookup-eligibility and save-authorization truth tables; no input can override either
computed decision.

The captured sccache statistics wire is the single JSON value emitted by exact v0.10.0 command
`sccache --show-stats --stats-format=json`. The parser rejects duplicate keys, trailing data,
non-UTF-8, non-object roots, unknown/missing top-level or `stats` fields, wrong types, negative or
non-integer counters, and any integer outside `u64`. Its closed v0.10.0 schema is independently
derived from the pinned `ServerInfo`/`ServerStats` source. The top level has exactly `stats`,
`cache_location`, `cache_size`, `max_cache_size`, `use_preprocessor_cache_mode`, and `version`.
`stats` has exactly these 22 keys:

```text
compile_requests
requests_unsupported_compiler
requests_not_compile
requests_not_cacheable
requests_executed
cache_errors
cache_hits
cache_misses
cache_timeouts
cache_read_errors
non_cacheable_compilations
forced_recaches
cache_write_errors
cache_writes
cache_write_duration
cache_read_hit_duration
compilations
compiler_write_duration
compile_fails
not_cached
dist_compiles
dist_errors
```

`cache_errors`, `cache_hits`, and `cache_misses` are objects with exactly `counts` and `adv_counts`;
each is a map from a 1..255-byte UTF-8 key without Unicode controls to a `u64`. `not_cached` and
`dist_compiles` are maps with the same key and value bounds. The three duration fields are objects
with exactly canonical nonnegative integer `secs` and `nanos`, where both fit `u64` and
`nanos<1_000_000_000`. Every other `stats` field is a `u64`.

For this managed local-disk profile, `cache_location` is exactly
`Local disk: "/work/sccache"`, `cache_size` is a `u64` no greater than 2,147,483,648,
`max_cache_size` is integer `2147483648`, `use_preprocessor_cache_mode` is false, and `version` is
exactly `0.10.0`; null optional-size values are rejected after a successful compile. A committed
schema and golden output enumerate the same fields and bounds. The implementation reads
`stats.cache_write_errors` only after the entire closed document validates.

Storage lookup and decompression failures that sccache v0.10 treats as misses remain misses.
`SCCACHE_IGNORE_SERVER_IO_ERROR=1` is set because it covers selected client/server response failures;
it is not described as covering startup, connection, extraction, or every backend error. Any other
sccache error follows pinned v0.10 behavior. An ordinary compiler failure is surfaced once and is
never retried by the cache layer.

If `sccache --stop-server` fails, the action skips save with a warning. If cache write errors are
non-zero, the build may still succeed but save is skipped with a warning. Restore, save, and cache
absence never cause an action-level retry. After metadata preflight, the action invokes exactly one
Cargo compile/build command. Pinned sccache v0.10 may itself fall back to a local compiler invocation
after `CompileStarted` when the server response is lost; this internal fallback is accepted
pinned-client behavior and is not described or tested as single compiler-process execution.

### 4.4 Cache audit and disclosure

`SCCACHE_CACHE_SIZE=2G` is the managed sccache capacity. The independent hard tree bound is
2,147,483,648 bytes: the audit sums `st_size` for every regular file with checked integer arithmetic
and rejects a greater total. It also rejects sparse files (`st_blocks * 512 < st_size`), more than
100,000 descendants, a path longer than 4,096 bytes, or a single path component longer than 255
bytes. Directory `st_size` values do not contribute. These are filesystem-tree limits, not a claim
about `actions/cache`'s host-selected tar, zstd, framing, or wire size. The selected cache action and
host archiver are outside the trusted data parser; an upload-size or archiver failure remains the
warning-only save failure defined above. The cache implementation plan pins
`actions/cache/restore@v6.1.0` and `actions/cache/save@v6.1.0` and tests these format-independent tree
bounds.

Before use after restore and before save, the audit requires:

- the canonical audited root is exactly `SCCACHE_DIR`; it is the same recorded device/inode as the
  action-created mode-0700 real directory, is owned by uid/gid 1001, and is not itself a mount;
- every entry is a regular file or directory beneath that root;
- no symlink, socket, FIFO, device, mount escape, or special file exists, and every regular file has
  `nlink == 1` (directory link counts are not constrained);
- ownership is the expected container uid/gid;
- layout and record names match the pinned v0.10 format fixtures;
- the logical-byte, non-sparse-file, path-length, and entry-count bounds above all pass.

The application is trusted, but cached compilation shares a writable uid and `SCCACHE_DIR` with app
code. App code can therefore place arbitrary bytes in that directory. The audit constrains shape and
size, not authorship or semantic content. The cache is not content-authenticated and the disclosure
acknowledgement covers the entire archived directory, compiler diagnostics, and app-written bytes
that satisfy the audit.

Every cross-repository cached build requires `disclosure-acknowledged: true` before either cache
family is restored or saved; equality between the authenticated application repository id and event
repository id is the only exemption. The
parent `deploy-fastly.cache` target cache uses the same lookup-eligibility rule and the same
action-derived protected-event save predicate as the sccache family. A denied parent save remains
restore-only only when lookup was eligible. Its existing key/content/audit contract remains owned by
the parent deploy spec.

At invocation start, the fixed host cache root must be absent beneath a real, non-symlinked
`${RUNNER_TEMP}`; the action creates it, records it, and never adopts a preexisting path. After the
single save attempt or any earlier terminal path, descriptor-relative no-follow cleanup removes that
recorded tree and verifies absence. Cleanup failure fails the action even when restore/save failure
itself was warning-only, because a dirty fixed root could contaminate another invocation in the job.

## 5. Container execution

### 5.1 Image and runner

The EdgeZero image is public and anonymously pullable by digest and is a leaf `linux/amd64` image
manifest rather than an OCI index. It is built from a digest-pinned base and contains:

- the exact Rust toolchain from `.tool-versions` and an installed `wasm32-wasip1` target;
- exact pinned Fastly CLI and sccache versions with checksum-verified downloads;
- `git`, `jq`, `tar`, `curl`, CA certificates, and a C toolchain;
- the project-owned provenance validator and its protocol/schema assets.

Runtime containers use a read-only root filesystem, uid/gid 1001, dropped capabilities,
`no-new-privileges`, no GitHub file-command channels, explicit mounts, and operation-specific network,
memory, pid, and timeout limits.

### 5.2 Working-copy topology

There are two independent exported-copy roles because GitHub jobs do not share filesystems. The
producer workflow and every consumer-side public action that needs repository source independently
create a private **authority checkout** at the exact app SHA through the object-first trusted
materializer defined below. The public identity action also creates its own short-lived authority for
identity calculation. The helper may use the app token only through its bounded host credential
channel; no application-controlled command, filter, hook, or worktree operation runs before policy
validation. No authority directory, descriptor, path, or opaque handle is accepted from or returned
to another public action. After credential removal and source validation, a trusted exporter creates
the applicable action-local copy from that authority checkout:

- **Copy A, producer build job:** a private faithful copy used only for cached native CLI
  compilation. It is mounted read-only; Cargo target, Cargo home, sccache, home, and temporary output
  live in separate action-owned paths. The reusable workflow uploads its CLI artifact; Copy A is then
  discarded.
- **Copy B, source-bearing consumer action:** a fresh private faithful copy exported from that
  action's independently materialized authority. Its repository view is mounted read-only except for
  the action-created nested output-root mounts, so generated files can flow from app build to
  `fastly compute deploy` without granting general source write access. `deploy-fastly` creates one
  Copy B at invocation start and may reuse it only between its internal app-build and deploy
  operations; its active-version operation does not mount the copy. No authority, Copy B, or generated
  root is shared across public action invocations. Source-free actions create no authority or Copy B.
  Copy A never crosses into the consumer job.

`config-push-fastly` is the sole source-bearing Copy-B exception: it executes no application code and
creates no Copy B. It independently materializes its own credential-free frozen authority and mounts
that authority read-only so its selected tracked manifest and file-backed config retain
repository-relative semantics. Before token creation and again immediately before container start,
the host records and verifies authority HEAD, index/worktree state, full tracked/submodule inventory,
and the selected manifest/config device, inode, digest, size, mode, and link count. It repeats those
checks after the command and before cleanup. Inline config is action-owned and receives the same
identity checks. Any authority or selected-file change, replacement, or race fails; fixtures exercise
replacement between every check and launch boundary.

Each copy preserves the entire repository layout, enclosing workspaces, parent Cargo config, sibling
path dependencies, file modes, symlink targets, and initialized submodule state. It includes tracked
files and initialized submodules only; ignored and untracked detritus, `.git` directories/files, LFS
object stores, checkout credentials, and action metadata are absent. Every regular file has a newly
created inode, so hardlinks to the authority are forbidden. The authority is made read-only
after export and remains the freeze authority.

Protocol 1 permits only paths with no Git filter or `filter=lfs`; any custom clean/smudge/process
filter, required filter other than LFS, or submodule using one fails. Before authority checkout, a
trusted host helper installs Git LFS 3.7.1 from exact asset
`git-lfs-linux-amd64-v3.7.1.tar.gz`, whose SHA-256 is
`1c0b6ee5200ca708c5cebebb18fdeb0e1c98f1af5c1a9cba205a4c0ab5a5ec08`. The closed canonical
gate-owned `.github/actions/deploy-core/host-tools.json` contains the following single data line with
no terminating newline; the Markdown fence line break is not file content:

```text
{"git-lfs":{"asset":"git-lfs-linux-amd64-v3.7.1.tar.gz","sha256":"1c0b6ee5200ca708c5cebebb18fdeb0e1c98f1af5c1a9cba205a4c0ab5a5ec08","size":5524590,"version":"3.7.1"},"schema-version":1}
```

The download URL is constructed in trusted code as
`https://github.com/git-lfs/git-lfs/releases/download/v3.7.1/git-lfs-linux-amd64-v3.7.1.tar.gz`, not
read from data. Every hop is HTTPS, the initial host is exactly `github.com`, redirects are bounded
to three and may terminate only at `release-assets.githubusercontent.com`, and no credential is sent
on the public download. The final response must be HTTP 200 with identity content encoding and exactly
one decimal `Content-Length: 5524590`; the streaming receiver rejects an absent, duplicate, malformed,
or different length, more than 5,524,590 received bytes, early EOF, or trailing data. An unexpected
host, downgrade, redirect count, checksum, archive layout, or installed `git-lfs version` also fails
before authority materialization.

The trusted materializer creates fresh Git object repositories with system/global configuration,
configuration includes, credential helpers, template hooks, and hook execution disabled. While no
application-controlled process is running, it uses the app token only through a noninteractive,
non-logging, host-only credential channel scoped to each canonical GitHub repository origin and
fetches exact app/submodule commits into object databases without creating a worktree or initializing
a submodule. Before any checkout, it recursively inspects the committed trees, `.gitattributes`,
`.lfsconfig`, `.gitmodules`, gitlinks, and submodule target trees using trusted Git plumbing. It
rejects `.lfsconfig`; any custom clean/smudge/process filter; any required filter other than LFS; any
repository/local/global/system `lfs.*` URL or transfer override; non-GitHub origin/submodule URLs; and
inconsistent, missing, or unlisted submodule commits. This validation may fetch a verified canonical
submodule origin but never creates its worktree or runs a filter, hook, or repository command.

Only after the complete recursive object graph passes does the helper create the authority worktrees
with every filter disabled, including automatic LFS smudging. It invokes the absolute verified Git
LFS 3.7.1 binary directly to fetch and materialize the exact permitted LFS objects, checks out exact
submodule commits, runs `git lfs fsck --objects` in each repository, and rejects any worktree file
that remains a valid LFS pointer. It then removes the credential channel and every credential before
export. The exporter copies materialized worktree bytes, not pointer blobs. Tests prove forbidden
filter commands, hooks, and non-GitHub origins are never contacted or executed and cover absent/
corrupt objects, pointer residue, nested submodules, credential cleanup, and configuration races.

### 5.3 Mount profiles

`run-app-cli-in-container` has a maximum allowlist and a closed profile for each operation. It never
mounts all of `RUNNER_TEMP`.

| In-container path           | Mode            | Allowed operations                                           | Source                                          |
| --------------------------- | --------------- | ------------------------------------------------------------ | ----------------------------------------------- |
| `/work/repo`                | read-only       | cached-compile, uncached-compile, app-build, provider-deploy | Copy A or Copy B                                |
| `/work/repo`                | read-only       | config-push                                                  | credential-free frozen authority; no Copy B     |
| `/work/repo/<output-root>`  | writable        | app-build, provider-deploy                                   | action-created declared/implicit directory      |
| `/work/target`              | writable        | cached-compile, uncached-compile, app-build, provider-deploy | fresh or parent target cache as specified below |
| `/work/cargo-home`          | writable, fresh | cached-compile, uncached-compile, app-build, provider-deploy | operation-specific directory                    |
| `/work/sccache`             | writable        | cached-compile only                                          | stable host cache directory                     |
| `/work/input/app-cli`       | read-only       | provenance-package only                                      | exact binary produced by cached-compile         |
| `/work/input/artifact.tar`  | read-only       | provenance-validate only                                     | downloaded artifact                             |
| `/work/expected`            | writable, fresh | expected-write                                               | empty host expected-identity output directory   |
| `/work/release`             | writable, fresh | release-request-write                                        | empty host release-request output directory     |
| `/work/input/expected.json` | read-only       | provenance-package, provenance-validate                      | validator-generated expected identity           |
| `/work/packaged`            | writable, fresh | provenance-package only                                      | empty host archive-output directory             |
| `/work/validated`           | writable, fresh | provenance-validate only                                     | empty host output directory                     |
| `/work/bin/app-cli`         | read-only       | binary-smoke and provider operations                         | validated binary                                |
| `/work/config/inline.toml`  | read-only       | config-push only                                             | optional action-owned inline config file        |
| `/work/home`, `/work/tmp`   | writable tmpfs  | all operations                                               | operation-local tmpfs                           |

Profiles:

- `cached-compile`: Copy A, fresh target and Cargo home, sccache, tmpfs; no token.
- `uncached-compile`: Copy A, fresh target and Cargo home, and tmpfs; no token, sccache mount,
  wrapper, socket, or sccache process.
- `app-build`: validated CLI, Copy B, fresh Cargo home, and the parent
  `deploy-fastly.cache` target directory when enabled. It runs `<cli> build` for
  `build-mode: always`, has no provider token and no sccache mount, and saves the parent target cache
  before any provider operation.
- `provider-deploy`: Copy B, fresh target/Cargo home, validated CLI, tmpfs, provider token;
  never sccache and never a writable cache. Fastly deploy may compile application source with the
  token for both `build-mode` values. A prior `app-build` is a credential-free validation/prebuild and
  does not claim to suppress this recompile; its parent target cache was already saved before the
  token appeared and is never saved again afterward.
- `expected-write`: trusted baked validator, fresh writable `/work/expected`, and tmpfs; no
  repository, app binary, target, Cargo, cache, network, or token. It converts typed identity scalars
  into the only supported `expected.json` encoding.
- `release-request-write`: trusted baked validator, fresh writable `/work/release`, and tmpfs; no
  repository, app binary, target, Cargo, cache, network, or token. It converts typed release scalars
  into the only supported `release-request.json` encoding.
- `provenance-package`: trusted baked validator, the exact compiled binary read-only at
  `/work/input/app-cli`, read-only expected-identity JSON, fresh writable `/work/packaged`, and tmpfs;
  no repository, target, Cargo, package, cache, app-binary execution, network, or token.
- `provenance-validate`: trusted baked validator, read-only tar, fresh writable output directory,
  read-only expected-identity JSON, and tmpfs; no repository, app-binary execution, token, Cargo,
  target, package, or cache mount.
- `binary-smoke`: validated binary only plus tmpfs; no network, token, repository, Cargo, target,
  package, cache, or validator output write access.
- `self-test`: no host bind mount. It reads only the image-owned validator, schema, and exact fixture
  directory and writes only to `/work/home` and `/work/tmp` tmpfs; no network, repository, app
  binary, token, Cargo, target, package, cache, or output bind mount.
- `provider-read`: validated binary and tmpfs. `active-version` receives the provider token.
  Production healthcheck receives no token; staging healthcheck receives the token needed for the
  staged endpoint.
- `provider-rollback`: validated binary and tmpfs plus the provider token; no repository, Cargo,
  target, package, or cache mount.
- `config-push`: validated binary, frozen repository read-only, tmpfs, provider token, and the
  enumerated app-config overlay. A selected manifest and file-backed app config must canonicalize
  beneath the frozen repository; inline config is one fresh host file mounted at the exact path
  above. It receives no writable repository, package, Cargo, target, or sccache mount.

The parent deploy spec remains normative for production/staging lifecycle semantics, rollback target
capture, mutation signaling, healthcheck ordering, and recovery. This addendum expressly supersedes
the parent's app-CLI metadata shape, caller override, archive member naming/ordering, system-tar
packaging/extraction, and artifact-validation rules, in addition to changing isolation and mounting.
Every staged CLI invocation uses `--staging`, never `--stage`.

Network and resource limits are closed by operation. An enabled network is Docker's ordinary isolated
bridge, never host or another container's namespace. Memory and memory-plus-swap limits are equal, so
no operation receives additional swap:

| Operations                                         | Network | Memory  | Pids | Hard wall timeout |
| -------------------------------------------------- | ------- | ------- | ---- | ----------------- |
| cached-compile, uncached-compile                   | bridge  | 6 GiB   | 512  | `timeout-minutes` |
| app-build, provider-deploy                         | bridge  | 6 GiB   | 512  | 60 minutes        |
| expected-write, release-request-write              | none    | 256 MiB | 32   | 60 seconds        |
| provenance-package, provenance-validate, self-test | none    | 2 GiB   | 64   | 10 minutes        |
| binary-smoke                                       | none    | 512 MiB | 64   | 60 seconds        |
| active-version, provider-rollback, config-push     | bridge  | 1 GiB   | 128  | 10 minutes        |
| production/staging healthcheck                     | bridge  | 1 GiB   | 128  | computed below    |

`timeout-minutes` is a canonical decimal integer from 1 through 120 and defaults to 30. Healthcheck
`retry` is 1..20, `retry-delay` is 0..300 seconds, and per-attempt `timeout` is 1..300 seconds. Checked
arithmetic computes `retry * timeout + (retry - 1) * retry-delay + 30` seconds; the value must be at
most 3,600 and becomes the container hard wall timeout. The supervisor sends TERM on expiry or runner
cancellation, waits at most 10 seconds, then sends KILL and enters the parent reconciliation path.
Timeout, OOM, resource-limit, and forced-kill outcomes are failures; they never relax cleanup,
mutation, cache-save, or reconciliation rules.

### 5.4 Constructed environments

Every operation starts with `env -i` and a closed allowlist. `PATH` is
`/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin`.

- cached compile: `PATH`, `RUSTUP_HOME=/usr/local/rustup`, `RUSTUP_TOOLCHAIN`, fresh `CARGO_HOME`,
  fresh `CARGO_TARGET_DIR`, `RUSTC_WRAPPER=/usr/local/bin/sccache`, `SCCACHE_DIR`,
  `SCCACHE_CACHE_SIZE=2G`, `SCCACHE_IGNORE_SERVER_IO_ERROR=1`, `CARGO_INCREMENTAL=0`, empty
  `CARGO_ENCODED_RUSTFLAGS`, `HOME`, `TMPDIR`, and validated `app-env`.
- uncached compile: the same Rustup/Cargo, `HOME`, `TMPDIR`, and validated `app-env` values, but no
  `RUSTC_WRAPPER`, `SCCACHE_*`, or sccache socket variable.
- app build: the Rustup/Cargo variables above except every sccache variable and wrapper, the
  operation's action-owned target paths, `HOME`, `TMPDIR`, the validated `app-env` map, and
  validated `EDGEZERO_MANIFEST` when selected; no provider token.
- provider deploy: the Rustup/Cargo variables above except every sccache variable and wrapper, plus
  `FASTLY_API_TOKEN`, the operation's enumerated `EDGEZERO_*` variables, validated `app-env`, and
  validated `EDGEZERO_MANIFEST` when the caller selected a manifest.
- expected writing, release-request writing, provenance packaging, provenance validation, binary smoke, and self-test:
  `PATH`, `HOME`, `TMPDIR` only. `self-test` argv is exactly
  `edgezero-provenance-validator self-test --fixtures /usr/local/share/edgezero/provenance-fixtures`.
- provider operations: `PATH`, `HOME`, `TMPDIR`, only the token required by that operation, and only
  explicitly named `EDGEZERO_*` variables plus validated `app-env`. Config push also receives its
  selected validated overlay names unless `no-env` was selected.

Non-credential application configuration is explicit rather than ambient. `app-env` is a JSON object
input (default `{}`). Its raw representation is valid UTF-8 and at most 65,536 bytes before parsing,
and duplicate keys are rejected before object construction. It has at most 64 entries and at most
32,768 UTF-8 bytes across names and values. Every value is a JSON string; numbers, booleans, null,
arrays, and objects fail. A name is 1..127 ASCII bytes matching
`[A-Za-z_][A-Za-z0-9_]*`; a value is at most 8,192 UTF-8 bytes and contains no NUL, C0 control, or DEL.
Reserved-name comparison is ASCII-case-insensitive. The exact deny set is:

- exact names `PATH`, `HOME`, `TMPDIR`, `TMP`, `TEMP`, `PWD`, `OLDPWD`, `SHELL`, `BASH_ENV`, `ENV`,
  `CDPATH`, `IFS`, `GLOBIGNORE`, `SHELLOPTS`, `BASHOPTS`, `CC`, `CXX`, `AR`, `AS`, `LD`, `NM`,
  `OBJCOPY`, `OBJDUMP`, `RANLIB`, `STRIP`, `CFLAGS`, `CXXFLAGS`, `CPPFLAGS`, `LDFLAGS`, `RUSTFLAGS`,
  `RUSTDOCFLAGS`, `RUSTC`, `RUSTDOC`, `ARFLAGS`, `CXXSTDLIB`, `CXXSTDLIB_STATIC`,
  `CRATE_CC_NO_DEFAULTS`, `CC_KNOWN_WRAPPER_CUSTOM`, `CC_SHELL_ESCAPED_FLAGS`,
  `CC_ENABLE_DEBUG_OUTPUT`, `NUM_JOBS`, `MAKEFLAGS`, `MFLAGS`, `LIBRARY_PATH`, `CPATH`,
  `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, `GCC_EXEC_PREFIX`, `COMPILER_PATH`, `PKG_CONFIG`,
  `PKG_CONFIG_PATH`, `PKG_CONFIG_LIBDIR`, and `PKG_CONFIG_SYSROOT_DIR`;
- prefixes `GITHUB_`, `RUNNER_`, `ACTIONS_`, `EDGEZERO_`, `FASTLY_`, `CARGO_`, `RUST_`, `RUSTC_`,
  `RUSTDOC_`, `RUSTUP_`, `SCCACHE_`, `CRATE_CC_`, `CXXSTDLIB_`, `LD_`, `DYLD_`, and `BASH_FUNC_`; and
- target- or build-kind-qualified native-tool names matching either
  `(CC|CXX|AR|AS|LD|NM|OBJCOPY|OBJDUMP|RANLIB|STRIP|ARFLAGS|CFLAGS|CXXFLAGS|CPPFLAGS|LDFLAGS)_.+` or
  `.+_(CC|CXX|AR|AS|LD|NM|OBJCOPY|OBJDUMP|RANLIB|STRIP|ARFLAGS|CFLAGS|CXXFLAGS|CPPFLAGS|LDFLAGS)`.
  This includes cc-rs forms such as `CC_<target>`, `<target>_CC`, `HOST_CC`, `TARGET_CC`, and their
  target/build-kind flag equivalents.

The caller is responsible for passing no credential under an otherwise allowed application name;
cross-repository cache disclosure covers compile-time values. Only the exact validated names are
added to operations that execute app code or the app CLI. Config-push's separately derived typed-
config overlay remains subject to its own prefix and `no-env` rules. This explicit input replaces the
parent's ambient workflow-`env` behavior and is a documented adoption migration.

Caller `PATH`, `RUSTC`, `RUSTDOC`, compiler wrappers, Rust flags, native-tool variables, ambient
application variables, and unlisted `EDGEZERO_*` variables are absent rather than scrubbed after
inheritance. Cached and uncached profiles both prove those variables cannot be reintroduced through
`app-env`.

No environment value is placed literally in Docker CLI argv. The action writes the already validated
operation environment to one fresh mode-0600, single-link, action-owned env file outside every
checkout/cache/output root, invokes `docker create --env-file <path>` for a uniquely named container,
and deletes and verifies absence of the env file before `docker start --attach`. File serialization
rejects newline and NUL in every name/value; provider inputs whose token format permits either are
therefore invalid. For token-bearing profiles, the final source/output/binary checks occur immediately
before this env file is created. Create/start/attach failure still triggers named-container removal,
env-file removal, private-workspace cleanup, and any required mutation reconciliation. The token may
exist in the isolated runner's Docker container metadata while that container exists; no other
container receives Docker-socket access, and removal is mandatory before the action completes.

Protocol 1's Cargo-config allowlist is empty. Before compilation, the action rejects `.cargo/config`,
`.cargo/config.toml`, and legacy `.cargo/credentials*` at the cwd, every ancestor through `git-root`,
and every enclosing workspace directory copied into `/work/repo`; the fresh action-owned
`CARGO_HOME` must contain none of those files. Cargo environment controls are absent under the closed
environment above. `Cargo.lock` must be a tracked regular file. Path dependencies may resolve
anywhere beneath `git-root` and must not escape it. The parent toolchain resolver still runs, but v1
requires its result (including an explicit `rust-toolchain` input) to equal the exact toolchain baked
in `image.json`'s image; a mismatch fails before container launch. Alternate toolchains or Cargo
configuration require a separate protocol revision and remain out of scope.

## 6. Source freezing and provenance

### 6.1 Freeze and pre-token verification

The authority checkout must be full, recursive, non-sparse, satisfy the no-filter-or-pinned-LFS
contract in Section 5.2, and start clean: `HEAD` equals `source-revision`, no tracked/index or
untracked modification exists, and every initialized submodule is clean at its recorded gitlink.
Credentials are removed before export, and Copy A or Copy B must pass the trusted export-inventory
comparison before any application command. Before and after app-controlled commands, the authority's
repository id, HEAD, clean state, filter policy, LFS object/materialization state, and recursive
submodule state must remain unchanged.

Immediately before any token-bearing operation that mounts Copy B, executes repository source, or
consumes its derived package, compare the complete Copy B inventory with the frozen source. This runs
whether or not `app-build` ran and, when it did, runs after that credential-free build:

- every tracked file's bytes and executable mode, every symlink target, and every gitlink commit must
  match, including deletion detection;
- no new path may exist except at or beneath a validated declared output root;
- each output root must canonicalize beneath the repository, must not be `.`, `.git`, or a symlink,
  and must not equal or be an ancestor of any tracked path;
- output roots must not overlap each other, and each parent segment must remain confined beneath the
  repository;
- every output-root entry must be a directory or regular file owned by the container uid/gid; regular
  files must have `nlink == 1` and must not be sparse (`st_blocks * 512 < st_size` fails), and
  symlinks, sockets, FIFOs, devices, mounts, and hardlinks fail;
- across all output roots, checked arithmetic permits at most 2,147,483,648 summed regular-file
  `st_size` bytes and 100,000 descendants. Existing component and joined-path bounds remain in force.

The declaration authority is the protected caller's `generated-output-paths` JSON-array input to
`deploy-fastly` (default `[]`). The raw input is valid UTF-8 and at most 65,536 bytes before parsing;
the array has at most 32 unique entries. Each entry is 1..1,024 UTF-8
bytes, uses `/` separators, is already in repository-relative lexical normal form, and has no empty,
`.`, `..`, NUL, control-character, or backslash component; every component is at most 255 UTF-8 bytes.
The joined absolute host path is at most 4,096 bytes. Each nearest existing parent is resolved before
app code runs, must be a real directory reached without a symlink, and must canonicalize beneath the
repository. Declared roots must be pairwise nonoverlapping, contain no tracked path, and be absent
initially. The action creates each as an empty mode-0700 directory, records its device/inode as action-
owned, and grants Copy B its only repository write access through those directory mounts. It audits
the closed file-type, ownership, hardlink, and confinement rules above after every app-controlled
command and immediately before every token-bearing operation.

Action-owned target and Cargo-home paths outside the repository are implicit and cannot be
overridden. The current Fastly CLI changes cwd to the resolved project directory containing the
selected `fastly.toml` and writes both `bin/main.wasm` and `pkg/<package>.tar.gz` there. Therefore
`<fastly-project-root>/bin` and `<fastly-project-root>/pkg`, not workspace-root paths, are two
additional implicit output roots. The manifest is resolved before any app process; both roots obey
the same absent-before-create, mount, audit, and ownership contract and callers do not repeat them.
The action keeps all output roots only until the last operation that consumes them, then a trusted
descriptor-relative cleanup routine removes each recorded action-owned tree without following links
and verifies the original checkout and Copy B again. Cleanup runs on success and ordinary failure;
cleanup or post-cleanup verification failure fails the action. A preexisting root fails before any
provider mutation, so cleanup never adopts or deletes caller-owned content. Any application whose
credential-free build writes elsewhere must declare every additional root; the action never infers
permission from an observed mutation.

This permits declared generated output while preventing a credential-free build step from rewriting
source that a later token-bearing compile would execute. Source-free lifecycle actions such as
healthcheck and rollback do not receive Copy B and do not run this inventory comparison; they verify
artifact/caller/platform identity instead. Config-push verifies repository id, HEAD, cleanliness, and
confined selected files on its read-only checkout. The consumer repeats the checks applicable to each
mounted source profile before and after provider commands.

### 6.2 Protocol-1 JSON contract

Protocol 1 has two closed JSON documents. Both are UTF-8 RFC 8785 JCS bytes with no BOM, surrounding
whitespace, or trailing newline. Duplicate object keys are rejected while parsing, before an object
or generic JSON value is constructed. Unknown and missing fields, wrong JSON types, noncanonical JCS
bytes, and values outside the bounds below fail closed. The committed JSON Schema 2020-12 file checks
the local shape; procedural validation enforces canonical bytes, duplicate rejection, cross-field
relationships, and exact expected-versus-observed identity.

`expected.json` contains exactly the identity the protected caller and local action computed:

```json
{
  "caller": {
    "app-cli-bin": "edgezero",
    "app-cli-package": "edgezero-cli",
    "app-repo-id": "123456",
    "source-revision": "<40-lowercase-hex>",
    "workspace-id": "sha256:<64-lowercase-hex>"
  },
  "platform": {
    "container-ref": "ghcr.io/stackpop/edgezero-build-app-cli@sha256:<64-lowercase-hex>",
    "platform-id": "sha256:<64-lowercase-hex>",
    "provenance-protocol": 1
  },
  "schema-version": 1
}
```

`app-cli-meta.json` contains exactly the same identity plus observed binary data:

```json
{
  "abi": {
    "interpreter": "/lib64/ld-linux-x86-64.so.2",
    "machine": "x86_64",
    "needed": ["libc.so.6"]
  },
  "app-cli-version": "0.1.0",
  "binary-sha256": "sha256:<64-lowercase-hex>",
  "binary-size": 123,
  "caller": {
    "app-cli-bin": "edgezero",
    "app-cli-package": "edgezero-cli",
    "app-repo-id": "123456",
    "source-revision": "<40-lowercase-hex>",
    "workspace-id": "sha256:<64-lowercase-hex>"
  },
  "platform": {
    "container-ref": "ghcr.io/stackpop/edgezero-build-app-cli@sha256:<64-lowercase-hex>",
    "platform-id": "sha256:<64-lowercase-hex>",
    "provenance-protocol": 1
  },
  "schema-version": 1
}
```

The examples are line-wrapped for review; the wire fixtures contain compact JCS bytes. Field rules
are exact:

- `schema-version` and `provenance-protocol` are JSON integers equal to `1`. Protocol 1 does not
  evolve them independently; an incompatible JSON, archive, or ELF rule requires both to change.
- `app-repo-id` is a string containing the canonical nonzero decimal representation of a `u64`: no
  sign and no leading zero.
- `source-revision` is a nonzero full lowercase 40-hex commit SHA.
- `app-cli-package`, `app-cli-bin`, and `app-cli-version` are 1 through 255 UTF-8 bytes, contain no
  Unicode control character, and contain neither `/` nor `\\`. The package and binary values must
  equal the validated Cargo package and target names; `app-cli-version` must equal that package's
  version from the same credential-free locked Cargo metadata result. Before the host mounts the
  compiled file at the fixed `/work/input/app-cli` path, it requires the source basename to equal
  `app-cli-bin`.
- `workspace-id`, `platform-id`, and `binary-sha256` use
  `sha256:<64-lowercase-hex>` and reject the all-zero digest.
- `container-ref` is exactly
  `ghcr.io/stackpop/edgezero-build-app-cli@<platform-id>`; no tag or alternate repository is valid.
- `binary-size` is a JSON integer from 1 through 536,870,912 and equals the exact
  `app-cli-bin` member payload length. `binary-sha256` equals SHA-256 over those exact payload bytes,
  with no header or padding bytes included.
- `abi.machine` is exactly `x86_64`; `abi.interpreter` is either the exact string defined in Section
  6.4 or JSON null; and `abi.needed` preserves every direct `DT_NEEDED` occurrence, including
  duplicates, sorted by UTF-8 bytes. Each entry is 1 through 255 bytes and is a basename containing
  no slash, backslash, dollar sign, NUL, or control character.

`expected.json` is at most 16 KiB and `app-cli-meta.json` is at most 64 KiB. The validator compares
the complete `caller`, `platform`, and `schema-version` values for equality. The artifact is a
consistency record, not producer authentication.

### 6.3 Protocol-1 ustar contract

The protocol crate is the only archive encoder and decoder. The producer must not construct metadata
with `jq` or archives with system `tar` or a general-purpose tar library. It emits deterministic POSIX
ustar with exactly two regular members in order: literal `app-cli-meta.json`, then literal
`app-cli-bin`. The caller's binary name remains in metadata and is not used as an archive path.

Every 512-byte header is byte-exact:

- `name` is the member name followed by NUL bytes to width 100; `prefix`, `linkname`, `uname`, and
  `gname` are all NUL bytes;
- `mode` is `0000644\0` for metadata and `0000755\0` for the binary;
- `uid`, `gid`, `devmajor`, and `devminor` are `0000000\0`; `mtime` is `00000000000\0`;
- `size` is eleven lowercase octal digits with leading zeroes followed by NUL;
- `chksum` is six lowercase octal digits with leading zeroes, NUL, and space; its unsigned-byte sum
  is computed with all eight checksum bytes replaced by spaces;
- `typeflag` is ASCII `0`, `magic` is `ustar\0`, `version` is `00`, and bytes 500 through 511 are NUL.

Base-256 numbers, alternate octal padding, embedded-NUL garbage, PAX/GNU extensions, sparse records,
links, special files, extra or duplicate members, renamed paths, and traversal are rejected. Payload
padding through the next 512-byte boundary is all zero. Exactly two all-zero end blocks follow the
binary payload, followed immediately by EOF; extra zero blocks or any trailing byte fail. The sum of
the two logical payload sizes is at most 512 MiB, and the metadata payload is nonempty and at most 64
KiB. Overflow in any size, offset, padding, or checksum calculation fails before reading or writing.

### 6.4 Protocol-1 ELF and loader profile

Protocol 1 intentionally models one conservative immutable runtime rather than general Linux loader
behavior. The primary app binary and every parsed dependency must be ELF64, little-endian, and
`EM_X86_64`; metadata records the machine as `x86_64`. The primary is `ET_EXEC` or `ET_DYN` and may
contain at most one `PT_INTERP`. If it has an interpreter or any `DT_NEEDED`, it must use exactly
`/lib64/ld-linux-x86-64.so.2`; it is treated as static only when both are absent. A resolved library
must be `ET_DYN`, must not contain `PT_INTERP`, and may have its own `DT_NEEDED` entries.

Program headers are the sole loader-visible authority. In addition to the ELF magic, `EI_CLASS` is
`ELFCLASS64`, `EI_DATA` is `ELFDATA2LSB`, `EI_VERSION` and `e_version` are `EV_CURRENT`, `e_ehsize`
is exactly 64, `EI_OSABI` is `ELFOSABI_SYSV`, `EI_ABIVERSION` is zero, every `EI_PAD` byte is zero,
`e_flags` is zero, and `e_phentsize` is exactly 56. `e_phnum` is nonzero and is not `PN_XNUM`; extended
program-header numbering is rejected rather than consulting section header zero. ELF and
program-header sizes, counts, offsets, virtual-address mappings, additions, and multiplications are
checked before access. Section headers may be absent and never affect validation; conflicting section
data is ignored because the runtime loader does not use it for this contract. A static primary has no
`PT_DYNAMIC`. Every dynamic primary,
interpreter, and library has exactly one bounded `PT_DYNAMIC`; multiple segments fail. Its entry width
is the ELF64 width, it contains a terminating `DT_NULL`, and every remaining byte in that segment is
zero. Missing termination or a nonzero trailing entry fails.

Protocol 1 defines loader-visible string tags as exactly `DT_NEEDED`, `DT_SONAME`, `DT_RPATH`,
`DT_RUNPATH`, `DT_AUDIT`, `DT_DEPAUDIT`, `DT_CONFIG`, `DT_AUXILIARY`, and `DT_FILTER`. If any of these
tags exists, the table has exactly one `DT_STRTAB` and one `DT_STRSZ`. Their complete nonempty range
must map into exactly one readable `PT_LOAD` file range. Duplicate or conflicting table tags,
unmapped/overlapping ranges, an out-of-range string offset, or a string without NUL before `DT_STRSZ`
fails. `PT_INTERP` follows the same bounded-range rules, contains exactly one trailing NUL, and
contains no interior NUL. Every accepted dynamic string is valid UTF-8 and has no NUL or control
character before its terminator.

Only `DT_NEEDED` may induce a library lookup. `DT_SONAME` is accepted only as nonempty descriptive
metadata, is bounded to 255 UTF-8 bytes, and contains neither `/` nor `\\`; it never adds a dependency.
`DT_RPATH`, `DT_RUNPATH`, `DT_AUDIT`, `DT_DEPAUDIT`, `DT_CONFIG`, `DT_AUXILIARY`, `DT_FILTER`, and
`DT_POSFLAG_1` are always rejected.

The accepted dynamic-tag vocabulary is numeric and closed; symbolic constants are labels only. It is
exactly core values `0..14`, `16..28`, `30`, and `32..37`; GNU values `0x6ffffef5`
(`DT_GNU_HASH`), `0x6ffffef6` (`DT_TLSDESC_PLT`), `0x6ffffef7` (`DT_TLSDESC_GOT`), `0x6ffffff0`
(`DT_VERSYM`), and `0x6ffffff9..0x6fffffff` (`DT_RELACOUNT` through `DT_VERNEEDNUM`); and x86-64
values `0x70000000`, `0x70000001`, and `0x70000003` (`DT_X86_64_PLT`, `DT_X86_64_PLTSZ`, and
`DT_X86_64_PLTENT`). Rejected string/acquisition tags above remain rejected even though their values
fall outside this allowlist. Every other value, including future standard, OS-specific, GNU, or
processor-specific tags, fails until a protocol revision explicitly adds it.

For accepted `DT_FLAGS` (value `30`), no bit outside mask `0x0000001e` may be set; this allows only
`DF_SYMBOLIC`, `DF_TEXTREL`, `DF_BIND_NOW`, and `DF_STATIC_TLS`. For accepted `DT_FLAGS_1`
(`0x6ffffffb`), no bit outside mask `0x5eff976f` may be set. This mask deliberately excludes
`DF_1_LOADFLTR`, `DF_1_ORIGIN`, `DF_1_NODEFLIB`, `DF_1_CONFALT`, `DF_1_ENDFILTEE`,
`DF_1_GLOBAUDIT`, and `DF_1_WEAKFILTER`; every undefined bit also fails. Multiple `DT_FLAGS` or
`DT_FLAGS_1` entries fail rather than combining masks. Apart from repeatable `DT_NEEDED` and the
all-zero bytes after the first `DT_NULL`, every accepted tag appears at most once; duplicate
`DT_SONAME`, table, size, relocation, version, flag, initialization, hash, or x86-64 tags fail.
Accepted non-string tags describe relocation, symbol, version, initialization, or hash tables but do
not participate in protocol identity or dependency discovery.

Protocol 1 metadata describes the primary's load-time ELF closure: its exact `PT_INTERP` and the
recursively traversed `DT_NEEDED` entries. It does not certify later application-directed `dlopen` or
child-process behavior, and the security model does not represent an application binary as trusted
merely because this structural profile passes. `DT_NEEDED` values containing `/`, `\\`, or `$` fail.
Rejecting `$` prevents glibc's `$ORIGIN`, `${ORIGIN}`, `$LIB`, `${LIB}`, `$PLATFORM`, and
`${PLATFORM}` expansion from making startup resolution differ from the validator's literal lookup.

The image build copies the complete reviewed x86-64 startup-library closure into the flat directory
`/opt/edgezero/runtime-lib`; that directory contains regular files only and no subdirectory, symlink,
or duplicate basename. A dynamic primary's `PT_INTERP` is exactly
`/lib64/ld-linux-x86-64.so.2`. The validator preserves duplicate direct `DT_NEEDED` values for
metadata, sorts them bytewise, and resolves every dependency basename only as
`/opt/edgezero/runtime-lib/<basename>`. A missing, escaping, non-regular, multiply linked, or duplicate
candidate fails. Recursive inspection uses a device/inode visited set so dependency cycles terminate,
and every transitive library satisfies this same profile. The interpreter resolves inside the
immutable image root, is a regular `ET_DYN` file with no `PT_INTERP`, and is parsed and recorded as a
member of the validator's visited runtime closure but is not added to the primary's
`abi.needed` metadata array.

The final image has no `/etc/ld.so.preload`. Every dynamic `binary-smoke` and provider-action launch
uses the container runtime's argv/entrypoint API directly, without a shell, to invoke that validated
interpreter with exact arguments `--inhibit-cache`, `--glibc-hwcaps-mask`, the empty-string mask,
`--library-path`, `/opt/edgezero/runtime-lib`, then `/work/bin/app-cli` and the validated operation
arguments. Thus `/etc/ld.so.cache`, default-directory precedence, and hardware-capability
subdirectories cannot select a different object for the validated startup closure. A static primary
is launched directly and has no `PT_INTERP` or `PT_DYNAMIC`. The verifier never invokes `ldd` and
never infers trust from loader output. Tests include preload presence, cache-only libraries,
hardware-capability alternates, default-directory duplicates, wrong interpreter, missing flat-closure
members, and an application-directed `dlopen` fixture demonstrating that such runtime behavior is
outside the metadata claim rather than silently certified.

### 6.5 Protocol-owner CLI

The synchronous `edgezero-provenance-validator` binary owns both encoding and validation. It has no
Tokio dependency and never executes an app binary. Its stable credential-free interface is:

```text
edgezero-provenance-validator write-expected \
  --work-root /work \
  --app-repo-id <canonical-decimal-u64> \
  --source-revision <40-lowercase-hex> \
  --app-cli-package <package> \
  --app-cli-bin <binary> \
  --workspace-id sha256:<64-lowercase-hex> \
  --platform-id sha256:<64-lowercase-hex> \
  --provenance-protocol 1 \
  --output /work/expected/expected.json

edgezero-provenance-validator write-release-request \
  --work-root /work \
  --gate-sha <40-lowercase-hex> \
  --provenance-protocol 1 \
  --release-tag build-container-v<positive-decimal> \
  --output /work/release/release-request.json

edgezero-provenance-validator package \
  --work-root /work \
  --binary /work/input/app-cli \
  --schema /usr/local/share/edgezero/provenance.schema.json \
  --expected /work/input/expected.json \
  --app-cli-version <version> \
  --archive /work/packaged/artifact.tar

edgezero-provenance-validator validate \
  --work-root /work \
  --archive /work/input/artifact.tar \
  --schema /usr/local/share/edgezero/provenance.schema.json \
  --expected /work/input/expected.json \
  --output /work/validated/app-cli

edgezero-provenance-validator self-test \
  --fixtures /usr/local/share/edgezero/provenance-fixtures
```

`--work-root` is required for output-producing commands and must canonicalize to `/work` in the
container. Every caller-selected input and output parent for those commands must canonicalize beneath
it, except the trusted baked schema path, which must be the exact literal path shown above and must
resolve to the image-owned regular file. `self-test` accepts only the exact baked fixture path shown
above, which must resolve to the image-owned fixture directory. `write-expected` accepts only the typed bounded scalars shown above, fixes `schema-version` to
`1`, derives `container-ref` from the fixed repository plus `platform-id`, and atomically publishes
canonical `expected.json`; shell, `jq`, and generic JSON encoders are not supported producers.
`write-release-request` accepts only the exact typed gate SHA, protocol, and release-tag scalars,
fixes the three-key JCS shape from Section 8, and create-new publishes only the literal basename
`release-request.json` in a fresh `/work/release` output directory. It is the sole supported producer
of that file.
`package` validates canonical expected identity, inspects and resolves the source ELF inside the
pinned image, generates canonical metadata, and atomically publishes the deterministic archive.
`validate` performs the inverse checks and atomically publishes exactly one mode-0755 regular output
file. Each output parent is a fresh canonical directory, must be writable and empty, and the final
file must have link count one.

The implementation writes a create-new temporary sibling, flushes and validates it, then performs a
Linux no-replace atomic rename to the final basename. Handled errors remove the temporary file before
return. SIGKILL, OOM, runner cancellation, or a container timeout may prevent in-process cleanup; the
host therefore removes the entire action-owned output parent after every abnormal/nonzero exit and
verifies it is absent before reporting failure or retrying. On success the host requires exactly the
one final file and no temporary sibling. `self-test` verifies a compiled manifest of exact fixture
paths, SHA-256 values, and expected valid/invalid outcomes; missing, extra, or changed fixtures fail.

### 6.6 Split validation boundary

Validation is deliberately two container invocations:

1. **Trusted parse/extract:** `provenance-validate` runs the baked project-owned validator. It strictly
   parses ustar and JSON, validates schema/JCS/duplicates, verifies identity/digest/size/ELF metadata,
   proves required libraries resolve in the image, and extracts exactly one binary to
   `/work/validated/app-cli`. The host then verifies the output directory contains only that regular,
   non-linked file with the expected mode, size, and digest.
2. **Untrusted execution:** `binary-smoke` starts a new hardened container with only the verified
   binary mounted read-only and tmpfs. It runs `--help` with no network or credentials and bounded
   memory, pids, and wall time.

The untrusted app binary never shares a writable mount with the parser/extractor. Within one composite
action invocation, the trusted validation step records the host path, digest, size, mode, device, and
inode only in action-private state for its later steps. No public action or reusable-workflow output
exposes a host binary path; the path becomes invalid when the mandatory end-of-action cleanup removes
the private workspace.

The validator, schema, malformed fixtures, valid golden archive, and all required capabilities must
exist and pass before any image digest can be published. Golden tests cover both JSON documents,
JCS, duplicate keys, schema rejection, byte-exact ustar encoding and parsing, traversal/link/special-
file rejection, header and padding normalization, size limits, ELF inspection, dependency resolution,
exact extraction, and output-directory confinement. Repeated `package` runs over the same inputs must
produce byte-identical archives, and `validate` must accept that golden output.

## 7. Reusable workflow and action contract

### 7.1 Reusable workflow

Inputs:

- `app-repository`, `app-ref`, `app-repo-id`, `working-directory` (default `.`), `workspace-root`,
  `app-cli-package`, `app-cli-bin`, `app-cli-artifact`, `rust-toolchain`;
- `cache` (default `false`), `cache-key-suffix`, `disclosure-acknowledged`, and `timeout-minutes`
  (default 30), plus `app-env` (default `{}`);
- secret `app-checkout-token`.

The reusable-workflow input schema is exact. `app-repository`, `app-ref`, `app-repo-id`,
`workspace-root`, `app-cli-package`, `app-cli-bin`, `app-cli-artifact`, and `rust-toolchain` are
required strings.
`working-directory`, `cache-key-suffix`, and `app-env` are optional strings with defaults `.`, the
empty string, and `{}`. `cache` and `disclosure-acknowledged` are boolean inputs defaulting to false.
`timeout-minutes` is a number input defaulting to 30 and must be an integer in the range already
defined. `app-checkout-token` is a required secret. No compatibility alias, platform/container input,
provider input, generic environment map, or arbitrary Cargo/action argument is accepted.

The workflow verifies its hosted identity and materializes its resolved action source before it
processes application input. The EdgeZero source checkout, app authority checkout, and Copy A use
three distinct fixed children of a fresh workspace. After token removal, the trusted exporter creates
Copy A from the validated authority checkout under Section 5.2; checkout never writes directly into
Copy A. Every local action/helper is invoked only from the verified EdgeZero checkout at
`job.workflow_sha`; application paths cannot shadow action code.

`app-cli-artifact` is 1..128 ASCII bytes matching
`[A-Za-z0-9][A-Za-z0-9._-]{0,127}` and must be unique among artifact uploads in the workflow run. It
does not partition the cache. The workflow has no provider inputs. Checkout persists no credentials.

The producer pins `actions/upload-artifact@v7.0.1`, uploads the one literal `artifact.tar` path with
`archive:true`, `compression-level:0`, `include-hidden-files:false`, `if-no-files-found:error`,
`overwrite:false`, and `retention-days:1`, and requires nonempty artifact id/digest outputs. The ZIP
wrapper and service digest are transport checks, not protocol provenance. Each consumer pins
`actions/download-artifact@v8.0.1`, supplies exact `name`, private destination `path`, current repository and
run id, `merge-multiple:false`, `skip-decompress:false`, and `digest-mismatch:error`, and supplies no
cross-repository token, pattern, or artifact id. It then requires that the destination contains only
one regular single-link `artifact.tar`; archive parsing remains the protocol validator's job.

Outputs are `artifact-name`, `action-version`, `action-revision`, plus every
`CallerExpectedIdentity` field: `app-repo-id`, `source-revision`, `app-cli-package`, `app-cli-bin`,
and `workspace-id`. `action-version` is exact `V` or release-fixture `C` parsed from
`job.workflow_ref`, and
`action-revision` is exact `job.workflow_sha`. It does not output `platform-id`, `container-ref`, or
protocol.

The consumer job invokes public action `stackpop/edgezero/.github/actions/compute-app-cli-identity`
at the same exact `action-version` as the producer and every provider action. Its exact required
string inputs are `action-version`, `app-repository`, `app-ref`, `app-repo-id`, `workspace-root`,
`app-cli-package`, `app-cli-bin`, and `rust-toolchain`; `working-directory` is an optional string
defaulting to `.`, and `app-checkout-token` is a required sensitive string input supplied by the
caller from a GitHub secret and masked before use. The action requires
`github.action_repository==stackpop/edgezero` and `github.action_ref==action-version`, independently
materializes and validates a short-lived authority through Section 5.2, removes the credential
channel, computes identity, and cleans the authority on every exit. It outputs exactly the five
`CallerExpectedIdentity` fields and no authority path, descriptor, opaque handle, credential,
platform field, or other host state. The job compares every output with the reusable-workflow output
before invoking another EdgeZero action.

Every later EdgeZero action receives `action-version`, requires the same runner-provided action
repository/ref equality, receives the consumer-verified `CallerExpectedIdentity`, and derives
`PlatformIdentity` from its local action files. Immutable release enforcement binds that version to
the producer's recorded `action-revision`; the consumer does not accept either value as a
caller-authored replacement. A source-bearing action does not trust the identity action's destroyed
authority: it independently repeats materialization and caller-identity verification under its own
action lifetime as defined below.

Matrix callers use unique artifact names and compare identity per leg. Shared workflow outputs are
not used to aggregate matrix results.

### 7.2 Provider actions

Every provider action accepts `app-cli-artifact`, producer `action-version`, and
`CallerExpectedIdentity`, verifies its own repository/version context, derives local
`PlatformIdentity`, downloads exactly the named artifact into an action-private workspace, and runs
the full validation sequence itself. Before `validate`, it invokes the baked validator's
`write-expected` command with the supplied, already consumer-verified caller fields plus its locally
derived platform fields into a fresh action-private expected directory. It never accepts expected
JSON, a platform field, or a host binary path from the caller, and never reuses an expected file from
another action invocation. Before each subsequent container launch, the action rechecks the
validated path is the same confined regular file with the recorded digest, size, mode, and single
link. The private workspace is removed with `if: always()`.

Every provider action also accepts the validated `app-env` JSON object (default `{}`); no provider
action inherits ambient application variables.

The source-bearing actions are exactly `deploy-fastly` and `config-push-fastly`. In addition to the
common inputs, both require string inputs `app-repository`, `app-ref`, `app-repo-id`,
`workspace-root`, `app-cli-package`, `app-cli-bin`, and `rust-toolchain`, accept optional string
`working-directory` defaulting to `.`, and require sensitive string input `app-checkout-token`, which
the caller supplies from a GitHub secret and the action masks before use. Each uses those inputs to
independently materialize an action-local authority, removes the checkout credential channel before
any application code, provider-token creation, or provider-token injection, and recomputes all five
caller-identity fields. A mismatch with supplied `CallerExpectedIdentity` fails before artifact
execution or provider mutation. Cleanup removes and verifies the action-local authority and any Copy
B on every exit. No source-free action accepts repository-materialization inputs or
`app-checkout-token`.

`deploy-fastly` additionally accepts `generated-output-paths`. It reuses one validated
binary for its `active-version`, optional credential-free `app-build`, and provider deploy operations
within that invocation. `active-version-fastly` is also a source-free action with inputs
`app-cli-artifact`, `CallerExpectedIdentity`, `fastly-service-id`, and `fastly-api-token`; it outputs
`version`, where an empty value is success only for a confirmed first production deploy.

`validate-app-cli-provenance`, `deploy-fastly`, `active-version-fastly`, `healthcheck-fastly`,
`rollback-fastly`, and `config-push-fastly` all apply this handoff. An identity or path mismatch fails
before app code or provider mutation. `validate-app-cli-provenance` is validation-only and has no
outputs; success means that its independent download, parse/extract, host check, and binary smoke all
completed before its private workspace was removed.

`config-push-fastly` validates and confines the selected repository/manifest/config file and derives
the exact named app-config environment overlay before container launch. Inline config is written to
one fresh host file and mounted read-only. `no-env` exposes no app-config overlay.

Mutation actions publish `mutation-attempted` host-side before launching the mutating CLI. Named
containers receive bounded signal forwarding and post-cancellation reconciliation as specified by the
parent deploy contract.

## 8. Image publication and compatibility

Protocol 1 selects the official `rust:1.95.0-slim-bookworm` image and pins its `linux/amd64` leaf
manifest, not its multi-platform index. The digest resolved from the official registry on 2026-08-31
is `sha256:6f9e63259f12e1e599296f5ecfed2bae46de4af0ee0525dd8b89c046e236d5c5`; implementation must
re-resolve and compare it immediately before committing the Dockerfile. The exact sccache asset is
`sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz` from the upstream v0.10.0 release, with upstream
checksum `1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b`. The v0.10.0 release has no
GNU Linux client asset; the static musl client is the reviewed Linux x86-64 artifact. Changing either
base digest or tool asset requires a new source revision and image digest.

The validator lives at `.github/tools/edgezero-provenance-validator` as a self-contained Cargo
workspace: its package manifest contains its own `[workspace]`, it has its own committed lockfile,
and it has no path dependency outside that directory. The staged Docker context excludes the root
workspace `Cargo.toml` and `Cargo.lock`; it includes the complete validator directory plus the other
explicit image assets only. The Dockerfile invokes one exact
`cargo build --locked --release --manifest-path
.github/tools/edgezero-provenance-validator/Cargo.toml`. Gate tests run
`cargo metadata --locked --manifest-path` inside the staged context and then the exact Docker build,
so a manifest that names an absent workspace member or path dependency cannot pass.

`image.json` is a reviewed record with exactly these typed fields:

```json
{
  "repository": "ghcr.io/stackpop/edgezero-build-app-cli",
  "tag": "build-container-v1",
  "digest": "sha256:<64-lowercase-hex>",
  "image-source-revision": "<40-lowercase-hex>",
  "provenance-protocol": 1
}
```

`tag` is informational. Runtime pulls use only `repository@digest`.

The pin PR changes exactly `image.json` and canonical `image-release-evidence.json` together. The
latter is UTF-8 RFC 8785 JCS with no BOM, surrounding whitespace, or trailing newline. It contains
the following single data line; the Markdown fence line break is not file content:

```text
{"approval-challenge":"<64-lowercase-hex>","approver-login":"<verified-login>","image-digest":"sha256:<64-lowercase-hex>","release-tag":"build-container-v<positive-decimal>","reviewed-at":"<RFC3339-UTC>","run-attempt":"<canonical-positive-u32>","run-id":"<canonical-positive-u64>","schema-version":1,"screenshot-sha256":"sha256:<64-lowercase-hex>","source-revision":"<40-lowercase-hex>"}
```

Its values equal the current approval record and `image.json`; run identifiers are strings to avoid
JSON number precision loss. A gate-owned typed writer is the sole producer. Unknown/duplicate/missing
keys, wrong types/order/JCS bytes, invalid bounds, or cross-file mismatch fail. Runtime actions do not
read this evidence file.

The rollout has five relevant commits and two action-release tags:

- `G` is the protected gate and image-source baseline. It contains the protocol validator, schema,
  golden/malformed fixtures, image verifier, fail-closed classifier, release-policy verifier,
  approval gate, pin-PR updater, publisher contract checker, exact Dockerfile, root `.dockerignore`,
  canonical image-context manifest, and both `.github/workflows/build-container-ci.yml` and
  `.github/workflows/publish-build-container.yml`, plus the gate-rotation lock workflow and verifier.
  Repository variable
  `EDGEZERO_BUILD_CONTAINER_GATE_SHA` equals `G`. Organization ruleset
  `edgezero-build-container-required-workflow` has target `branch`, active enforcement, no bypass
  actors, conditions containing repository id `[<edgezero-id>]` and ref-name include
  `["refs/heads/main"]` with no excludes, and exactly one `workflows` rule. That rule has
  `do_not_enforce_on_create=false` and exactly one descriptor with the EdgeZero repository id, the
  literal workflow path, full SHA `G`, and no `ref`. The workflow checks out `G` separately from
  candidate source and executes only gate code from `G`; candidate files are data under test. A
  change to a gate-owned or image-context path must land as a separately reviewed new `G`, and the
  ruleset must be updated to that SHA, before any dependent release request.
- `S` is the full release-source commit. It descends from `G`, changes exactly the canonical
  `release-request.json` described below, and leaves every gate-owned and image-context byte equal to
  `G`. The publisher uses a trusted `G` helper to construct a fresh Docker context containing only the
  manifested paths copied from the verified `G` checkout; it never builds a candidate Dockerfile or
  admits an unmanifested repository path. The image has OCI label
  `org.opencontainers.image.revision=S`,
  `org.opencontainers.image.source=https://github.com/stackpop/edgezero`, and a protocol label
  matching the baked validator. The final image overrides inherited source/revision labels, and
  verification requires all three exact values.
- `B` is the baseline revision created after the pin PR commits the verified digest and `S` to
  `image.json` plus its bound `image-release-evidence.json`, and permanent pin CI is enabled.
- `H` is the protected-main commit proposed as the final action revision. It contains the unchanged
  reviewed pin plus the cache, provenance, launcher, and consumer implementation. The prepublication
  adoption documents still contain only the explicit action-version placeholder allowed by the
  bootstrap documentation gate.
- `C` is a unique canonical exact patch-version tag published as an immutable
  GitHub Release at `H` with `prerelease:true`. Its tag has no prerelease suffix. The complete local
  suite runs from a clean detached `H`; the complete hosted cross-repository/provider suite invokes
  `C`. A failure leaves `C` immutable and requires a new commit plus a newly selected unused `C`.
- `P` is defined as `H` only after the exact-`H` local suite and complete hosted suite through `C`
  pass. Before that point, no text may call `H` final revision `P`.
- `V` is a canonical exact stable action version `v<major>.<minor>.<patch>`, distinct from `C`,
  selected only after `H` qualifies as `P`. An immutable stable release `V` is published at `P`, then
  a final hosted identity and consumer smoke uses literal `V`. The release API and remote refs must
  both resolve `C` and `V` to `P`.
- `R` is the later protected-main documentation revision. It changes only tracked Markdown adoption
  surfaces and `docs/.edgezero-action-release.json`, replacing the bootstrap placeholder with literal
  `V` and activating the documentation gate's released state. It does not alter action/workflow code
  and is not a new action revision; published consumers pin `V`, never `P`, `C`, a major/minor tag, or
  a branch.

`docs/.edgezero-action-release.json` is absent through `P`. Revision `R` adds it as UTF-8 RFC 8785
JCS with no BOM, surrounding whitespace, or trailing newline. It contains the following single data
line; the Markdown fence line break is not file content:

```text
{"action-revision":"<P>","action-version":"<V>","schema-version":1}
```

The action revision and version obey their exact grammars and must match the already published stable
immutable release and peeled remote tag. Unknown, duplicate, missing, reordered, or mistyped fields
fail.

`release-request.json` is UTF-8 RFC 8785 JCS with no BOM, surrounding whitespace, or trailing
newline. It contains the following single data line; the Markdown fence line break is not file
content:

```text
{"gate-sha":"<G>","provenance-protocol":1,"release-tag":"build-container-v<positive-decimal>"}
```

`gate-sha` is lowercase full hex and must equal the active gate variable and required-workflow SHA;
the decimal has no sign or leading zero. The source PR changes this file and no other path. Its tag is
unused until the reviewed operator creates that exact protected tag at `S`. A changed Dockerfile,
`.dockerignore`, image manifest, validator input, workspace lockfile, or other repository image-context
byte is a gate update, never an ordinary `S` candidate.

The reviewed operator creates this file using a local image built solely from the canonical context
staged from clean `G`. The build records local image/config identity `L` from BuildKit's `--iidfile`,
and the gate-owned verifier requires `L` to be a leaf `linux/amd64` image with revision label `G`, the
expected protocol, and the complete validator/toolchain contract. The operator then invokes
`write-release-request` by immutable local identity `L`, with no network, credentials, repository
mount, or tag lookup and with only the closed `release-request-write` profile. This local bootstrap
image is not published and is not digest `D`; no already-published or pinned `G` image is assumed.

There is no literal same-commit requirement between image source and pin record. Compatibility is
enforced by digest, image labels, and exact `provenance-protocol`. Changing the validator/archive
contract requires a protocol bump and a new image before the actions using that protocol are pinned.

The gate owns a canonical path manifest and `.github/CODEOWNERS`; the latter assigns every manifested
path and itself to `@stackpop/edgezero-build-container-gate-reviewers`. The no-bypass default-branch
ruleset requires code-owner review, at least two approving reviews, dismissal of stale approvals, and
the merge queue. A gate-update PR changes only paths in the union of the old and candidate canonical
manifests. Old `G` validates the candidate manifest's canonical sorted form, validates candidate
`CODEOWNERS` coverage, classifies the change as `mode=gate-update`, executes only old-`G` static and
subject-data checks, and never runs candidate gate code. A mixed gate/non-gate change fails. If that PR
passes the old gate and required human reviews, merging it creates candidate `G'`, not `S`. Until
activation, the protected base's manifested bytes differ from active `G`, so every ordinary candidate
and release preflight fails.

To activate `G'`, dispatch the gate-owned rotation-lock workflow from protected main while main and
both gate pointers still equal old `G`. It uses the exact repository-global
`edgezero-build-container-publication` concurrency group with `cancel-in-progress:false` and
`queue:max`; after all older publishers finish, its unprivileged acquire job records its run id and its
second job waits for independent approval on environment `build-container-gate-rotation-lock`. The
waiting workflow holds the concurrency group. Before first use, a live fixture must prove that a
publisher dispatched behind this waiting job remains pending and starts no build/push step.

While the lock is held, the operator verifies no older publication is active or pending ahead of it,
sets repository variable `EDGEZERO_BUILD_CONTAINER_RELEASE_STATE` from `enabled` to
`disabled:<lock-run-id>:<old-G>`, removes the release environment's tag policy, and verifies release is
disabled. The operator then merges the gate update through the one-entry queue. From clean detached
`G'`, run the full gate suite and require the generic post-merge main-push assertion at `Q=G'`; update
the disabled marker to `disabled:<lock-run-id>:<old-G>:<G'>`; then update both
`EDGEZERO_BUILD_CONTAINER_GATE_SHA` and the organization required-workflow descriptor SHA to `G'`.
Either intermediate mismatch fails all required runs. Verify the base's complete manifested tree
equals `G'`, restore the sole tag policy, produce new independently reviewed prerequisite evidence,
and set release state back to exact `enabled`. Only then may a reviewer enter the canonical rotation
evidence comment and approve the waiting lock job, whose old-`G` helper verifies its own run/context,
the main head, public repository variables, and evidence digest before releasing concurrency. No
ruleset bypass is used for a gate update.

If activation fails before both pointers and all post-activation checks agree, keep release disabled
and restore both pointers to old `G` while the lock remains held. Because the base then still contains
`G'`, ordinary work remains blocked. Old `G` has a distinct `mode=gate-rollback`: both active pointers
must equal old `G` and release must remain disabled; the current base must be exactly the failed `G'`
tree that old `G` validates as a canonical gate update; the proposed head's complete manifested tree
must be byte-identical to old `G`; changed paths must be confined to the union of the `G'` and old-`G`
manifests; and no release request, pin record, or non-gate path may change. Merge that separately
reviewed rollback through the one-entry queue, require generic main-push evidence at the resulting
head `Q`, verify the base tree and both pointers equal old `G`, and only then restore release policy.
If old `G` cannot validate either the current `G'` tree or the exact restoration, recovery is a manual
trust-root operation requiring the same independent review as bootstrap, and release remains disabled
throughout. No mixed `{variable, descriptor, base manifest}` state is a degraded operating mode.
If manual recovery cannot finish before the lock run expires, leave release state disabled and the tag
policy absent before canceling it; every later publisher must acquire concurrency and fail closed on
the disabled state before image build or push.

`build-container-gate-rotation-lock` has no secret or variable and no enabled GitHub App custom
protection rule. It permits only protected `main`, disables administrator bypass, requires a nonempty
reviewer set with self-review prevention, and is referenced with `deployment:false`. Its one approval
comment is exactly:

```text
edgezero-gate-rotation-v1 {"evidence-sha256":"sha256:<64-lowercase-hex>","head-sha":"<Q>","lock-run-id":"<canonical-positive-u64>","new-gate-sha":"<G'>","old-gate-sha":"<G>","result":"activated|rolled-back","reviewed-at":"<RFC3339-UTC>"}
```

The compact JSON uses the shown key order/types and no extra whitespace. The reviewer differs from
the operator, `reviewed-at` uses the release approval's exact time grammar and 15-minute freshness
window, run id equals the lock run, and `head-sha` is the current protected head. The attached
canonical evidence proves the exact policy/pointer/base checks; the helper recomputes its digest and
requires exactly one current-run protocol comment. An activated result requires head tree and both
pointers at `G'`, restored tag policy, and release state enabled. A rolled-back result requires an
exact old-`G` tree, both pointers at old `G`, restored tag policy, and release state enabled.

The dedicated repository ruleset `edgezero-build-container-main` has target `branch`, enforcement
`active`, no bypass actors, and ref-name conditions including exactly `refs/heads/main` with an empty
exclude list. Its rules are exactly:

- `pull_request` with `allowed_merge_methods:["squash"]`,
  `dismiss_stale_reviews_on_push:true`, `require_code_owner_review:true`,
  `require_last_push_approval:true`, `required_approving_review_count:2`, and
  `required_review_thread_resolution:true`; and
- `merge_queue` with `check_response_timeout_minutes:60`, `grouping_strategy:"ALLGREEN"`,
  `max_entries_to_build:1`, `max_entries_to_merge:1`, `merge_method:"SQUASH"`,
  `min_entries_to_merge:1`, and `min_entries_to_merge_wait_minutes:0`.

The one-entry build and merge limits prevent a passing merge-group result from authorizing a
different batched tree. Missing, extra, or changed semantic rule fields fail the prerequisite audit.

The two release-tag repository rulesets both have source type `Repository`, source
`stackpop/edgezero`, target `tag`, active enforcement, and ref-name conditions including exactly
`refs/tags/build-container-v*` with an empty exclude list. Ruleset
`edgezero-build-container-tag-immutability` has no bypass actors and exactly `update` with
`update_allows_fetch_and_merge:false` plus `deletion` rules. Ruleset
`edgezero-build-container-tag-creation` has exactly one bypass actor, the numeric team id for
`edgezero-build-container-releasers` with type `Team` and mode `always`, and exactly one `creation`
rule. It has no update or deletion rule. Missing, extra, defaulted, or changed source, target,
condition, actor, mode, parameter, or rule fails the prerequisite audit.

Two additional repository rulesets apply the same creation/immutability split to action releases.
Both have source type `Repository`, source `stackpop/edgezero`, target `tag`, active enforcement, and
ref-name conditions including exactly `refs/tags/v*` with no excludes.
`edgezero-action-version-tag-immutability` has no bypass actors and exactly `update` with
`update_allows_fetch_and_merge:false` plus `deletion`; `edgezero-action-version-tag-creation` has only
the same reviewed releaser Team actor in `always` mode and exactly `creation`. Repository immutable
releases are enabled. The broader `v*` ruleset protects distinct exact patch versions `C` and `V`,
while the release procedure separately enforces their canonical grammars, absence, release states,
and commit targets.

The action-release operator uses a short-lived fine-grained personal access token selected only for
repository `stackpop/edgezero`, expiring within 24 hours, with exactly repository `Contents:write`
and `Workflows:write`, implicit metadata read, and organization `Members:read`; every other repository
or organization grant is disabled. A classic PAT, installation token, `GITHUB_TOKEN`, broader
repository selection, extra grant, or token shared with Actions is invalid. The operator preserves
the fine-grained token settings as review evidence, supplies the token to a local non-logging helper
through a private descriptor rather than argv or environment, and destroys it after release.

That helper permits no redirect and only requests with
`Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2026-03-10`,
`User-Agent: edgezero-action-release/1`, and its authorization header:
`GET /user`, `GET /organizations/<stackpop-id>/team/<releaser-team-id>/memberships/<login>`,
`POST /repos/stackpop/edgezero/releases`, `PATCH /repos/stackpop/edgezero/releases/<release-id>`, and
`GET /repos/stackpop/edgezero/releases/<release-id>`. The membership response must be active. POST
creates a draft with exact tag, full target commit, no assets, and required prerelease boolean; PATCH
changes only `draft` to false. GET verifies author, tag, target, draft/prerelease/immutable state, and
release id. Any other method, host, path, query, body field, redirect, credential type, or response
shape fails. Anonymous remote-ref resolution and release-attestation verification are separate
read-only checks.

Repository ruleset `edgezero-build-container-pin-branches` has source type `Repository`, source
`stackpop/edgezero`, target `branch`, active enforcement, conditions including exactly
`refs/heads/edgezero-build-container-pin/*` with no excludes, and exactly one bypass actor: the
dedicated publisher App's numeric integration id, type `Integration`, mode `always`. Its rules are
exactly `creation`, `update` with `update_allows_fetch_and_merge:false`, and `deletion`. Thus only that
App can create, move, or delete a canonical pin branch. A pin branch is exactly
`edgezero-build-container-pin/<S>` and its PR title is exactly
`chore(actions): pin build container for <S>`; any different head repository, branch, author,
integration, title, or changed-path set fails required pin CI.

Publication order is:

1. Land `G`, then configure and verify the exact active organization ruleset and its sole
   required-workflow descriptor
   `{repository_id:<edgezero-id>,path:".github/workflows/build-container-ci.yml",sha:G}`
   and an active default-branch ruleset that requires the merge queue with no bypass actor. Configure
   the protected release and rotation-lock environments, split tag-creation/immutability rulesets,
   dedicated GitHub App, exact release-state `enabled` and publisher App/bot identity repository
   variables, and repository permissions. Prove the
   rotation workflow holds publication concurrency before the first gate update. The one-time
   bootstrap of `G` is explicitly a human-reviewed trust-root operation;
   candidate-controlled checks are not represented as independent proof of `G`.
2. Open the isolated `release-request.json` candidate, run it through the organization-required
   workflow from `G`, complete the credential smoke, and merge it only through the verified merge
   queue. The resulting default-branch commit is `S`. Before tagging, require the repository-local
   workflow's latest-attempt run API record to have event `push`, path
   `.github/workflows/build-container-ci.yml`, and `head_sha=S`. Require both stable jobs to report
   `head_sha=S`, success, and exactly one successful step named `assert-exact-main-push-context`. That
   immutable step invokes only the `G` helper and internally asserts `github.ref==refs/heads/main`,
   `github.event.after==github.sha==github.workflow_sha==S`, and active gate SHA `G`; the REST API
   does not expose those context fields, so the step result is the external evidence. A PR or
   `merge_group` result cannot substitute for this exact post-merge run. The first package may not
   exist yet; its public-visibility gate occurs after its first push and before a pin PR.
3. The publisher verifies `S`'s manifested bytes equal `G`, constructs a fresh context solely from
   the clean `G` checkout and canonical context manifest, builds with the gate-owned Dockerfile, pushes
   by protected release tag, and captures digest `D` from BuildKit's metadata output.
4. Verify `D` is a leaf linux/amd64 image, labels identify `S` and protocol, exact tool versions and
   target are installed, validator capability tests pass, and runtime works read-only/non-root.
5. Ensure the GHCR package is public and linked to `stackpop/edgezero`, then prove an anonymous pull
   and smoke by `D`. The first release stops here until an operator changes package visibility, reruns
   the `G` preflight in package-present mode, attaches its evidence, and reruns the same tag.
6. Open or update an idempotent App-authored PR on the exact protected pin branch, committing
   `image.json = {D, S, protocol}` plus its canonical run/approval evidence record. Required pin CI
   verifies App/branch/run/approval origin and re-verifies the image before merge; merging the passing
   PR creates baseline `B`.
7. Select currently absent canonical patch version `C` for candidate qualification. Implement the
   remaining executable plans on top of `B`; keep the four prepublication adoption documents at the
   exact bootstrap placeholder. Merge the executable candidate through the queue and record the
   resulting protected-main commit as candidate `H`.
8. From a clean detached checkout of exact `H`, rerun the complete pin, actionlint, zizmor, schema,
   fixture, container, Rust, documentation, and contract suites. A locally authenticated active member
   of the exact releaser team authorized by the creation ruleset creates a draft for `C` with exact
   target `H`, no assets, and `prerelease:true`, then publishes it. Require release API fields
   `draft:false`, `prerelease:true`, and `immutable:true`, remote peeled ref `C==H`, authenticated
   operator identity equal to release author, and recorded team membership plus release attestation.
   Run the complete hosted cross-repository/provider suite with every EdgeZero workflow/action ref
   equal to literal `C`. Only after both suites pass, designate `H` as `P`, select a distinct currently
   absent stable version `V`, and repeat the same actor/draft/publish/evidence procedure for `V` at
   `P`, with `prerelease:false`. Verify API and remote-ref resolution and run a final hosted identity/
   consumer smoke with literal `V`. A candidate failure requires a new commit and unused `C`; no `V`
   has yet been selected. A post-publication verification failure cannot retarget `V` and is corrected by a new patch
   release. Deletion of the GitHub Release object, or mutation of its title, notes, prerelease, or
   latest metadata by an actor with sufficient repository privilege, remains an accepted availability/
   discovery risk; the no-bypass tag rules still prevent tag deletion or retargeting, and GitHub's
   immutable-release tombstone prevents tag-name reuse. Preserve both generated release attestations
   in the release evidence and verify release existence and required state during the final audit.
9. Only after the literal-`V` smoke passes, open documentation-only revision `R`. It adds the exact
   action-release record bound to `{V,P}`, replaces every bootstrap placeholder in tracked Markdown
   with literal `V`, and changes no executable, workflow, action metadata, gate-owned, or non-document
   path. The already active dual-state gate verifies the stable immutable release and remote ref,
   applies final-mode documentation checks, and rejects deletion/downgrade of the release record.
   Merge `R` through the queue, run the final documentation build/pin scan on protected main, and
   record `R`; no action release or action revision changes at this step.

Gate baseline `G` contains `.github/workflows/build-container-ci.yml`. The active organization ruleset
uses its exact repository id, path, and SHA `G`; it uses neither a branch nor a candidate-controlled
ref. A repository PR cannot substitute its own workflow or helper implementation. The workflow
supports `pull_request`, `merge_group`, protected-default-branch `push`, and a manual
`workflow_dispatch` credential-smoke mode, with no workflow-level path filter. It exposes two stable
required job names on every candidate and grants only workflow-level `contents:read`, `actions:read`,
and `pull-requests:read`; neither job references an environment or mutation credential:

- `build-container-local` computes the documented image-input path set. It builds and smokes the local
  image when relevant and otherwise runs an explicit successful not-applicable step.
- `build-container-pin` detects every add, change, or deletion of either pin-record file. When
  relevant, both must exist and be the only changed paths; it validates both structures and their
  equality, verifies the exact pin branch/PR title/head repository and dedicated App bot id/login,
  validates the bound publisher run/attempt and approval evidence described below, anonymously pulls
  the exact digest, and runs the complete published-image verifier. Otherwise it explicitly succeeds
  as not applicable.

Each organization-required job parses `github.workflow_ref` and requires its repository and path to be
exactly `stackpop/edgezero/.github/workflows/build-container-ci.yml`; it also requires
`github.workflow_sha` to equal repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA`. The variable
is a full SHA and equals `G` and the ruleset descriptor SHA. On a protected-default-branch push, each
stable job instead requires exactly one successful `assert-exact-main-push-context` step whose trusted
helper proves `github.ref==refs/heads/main` and `github.event.after==github.sha==github.workflow_sha==Q`;
the job still checks out gate code at the variable's active `G`. Each job checks out
`G` and the candidate revision into distinct roots and invokes only the protected gate's classifier and
verification driver. Candidate scripts are never sourced or
executed as gate authority. Each job performs its own fail-closed change classification from the
candidate base and head so a failed shared classifier cannot skip a required job. The local-image set
is not a hand-maintained approximation of a root context: it is the release request plus the complete
canonical image-context manifest, and every manifested image path is also gate-owned. Classification
output is exactly two lines in fixed order: `mode=ordinary`, `mode=gate-update`, or
`mode=gate-rollback`, followed by `relevant=true` or `relevant=false`. Gate-update and gate-rollback
always report relevant true. An unconditional terminal assertion rejects missing, duplicate, or
malformed output and proves exactly one of the relevant ordinary, gate-update, gate-rollback, or not-
applicable branches ran; an invalid classifier can never make all conditional paths disappear behind
a green job.
Contract tests pin pull-request, merge-group, and push ranges, protected workflow/helper provenance,
event triggers, job names, context-manifest closure, deletion handling, output validation, and
explicit no-op behavior. The existing path-filtered
`deploy-action.yml` remains separate. Thus required checks always materialize without running Docker
on unrelated changes, and no later syntactically valid pin can bypass image, platform, label,
protocol, public-access, target, validator, or exact-version checks.

For a pin candidate, the trusted `G` helper reads `image-release-evidence.json` and uses only the
required job's read-only `GITHUB_TOKEN`. It polls the identified same-repository workflow run at most
30 times with a fixed 10-second delay, then requires completion/success, event `push`, workflow path
`.github/workflows/publish-build-container.yml`, `head_sha=S`, `head_branch=<release-tag>`, and exact
run attempt. The run has successful `build-and-verify` and `update-pin` jobs, each with exactly one
successful `assert-exact-publisher-context` step; that trusted `G` step internally proves the protected
tag ref, `github.sha==github.workflow_sha==S`, active gate, release state enabled, run id/attempt, and
tag. The helper also reads the run's non-paginated approvals, requires exactly one current-attempt
release protocol record, and compares every comment field and API reviewer login with the evidence
file. Missing, pending, duplicate, stale, foreign, or contradictory evidence fails before image pull.
The pin check never accepts a candidate-provided check name or PR body as publisher evidence.

For ordinary image or pin candidates, each job also compares every path in the active gate manifest
between the protected base and its `G` checkout and fails on any mismatch. Gate-update and the exact
recovery-only gate-rollback are the only exceptions. For gate update, old `G` must classify the
candidate's manifested-path change explicitly, verify the base
still equals old `G`, require every changed path to be in the union of old and candidate manifests,
require no changed release request or pin, run the protected static/subject-data gate-update checks,
and record that mode in its terminal marker. Candidate gate scripts are not executed in either gate
mode. Gate rollback obeys the exact failed-`G'`-to-old-`G` restoration contract above. An
ordinary release request must change exactly `release-request.json`; its `gate-sha` is `G`, its
protocol is 1, and every repository image-context input in `S` is byte-identical to `G`.

The protected workflow also exposes non-required job `build-container-release-preflight` only for a
`workflow_dispatch` request whose body uses `ref:"main"` while protected `main` is exactly `G`. A
workflow dispatch ref is a branch or tag name, not a raw commit SHA. Its required typed inputs are a
same-repository candidate PR number, exact head repository, and full lowercase head SHA. Its
`run-name` is exactly
`build-container-release-preflight pr=<number> repo=<owner/name> sha=<40-lowercase-hex>`, making the
claimed binding API-visible. The job uses environment
`{name: build-container-release, deployment: false}`, checks out only exact `G` into a private gate
root, and runs no candidate repository script. Its fixed step `assert-exact-g-dispatch-context`
invokes only the protected `G` helper, uses the read-only `GITHUB_TOKEN` to fetch the named PR, and
requires the three inputs to equal the current PR number/head repository/head SHA as well as event `workflow_dispatch`, `github.ref==refs/heads/main`,
`github.sha==github.workflow_sha==G`, and repository variable
`EDGEZERO_BUILD_CONTAINER_GATE_SHA==G`. After the environment reviewer approves it,
`actions/create-github-app-token@v3.2.0` consumes the exact stored
App variable and private-key secret with repository `edgezero` and explicit `contents:write` and
`pull_requests:write`. The job requires its installation-ID output to equal the stored expected ID,
reads only `stackpop/edgezero` with the token, and lets the action's mandatory post step revoke the
token. A successful check run from the GitHub Actions App proves the protected environment's stored credential, rather
than only an operator's local copy, can mint the publisher's exact token before `S`.

The final environment policy is tag-only, so the smoke uses a bounded transition. An administrator
temporarily adds one custom branch deployment policy equal to the literal protected default-branch
name `main`, dispatches the workflow with body `ref:"main"` while `main==G`, then removes that branch
policy without changing the App variables or secret. The final preflight requires the environment to
be back to its sole `build-container-v*` tag policy and the successful workflow run API record to have
event `workflow_dispatch`, path `.github/workflows/build-container-ci.yml`, `head_sha=G`, and exact
display title/run-name for the current candidate. Its job
must contain exactly one successful `assert-exact-g-dispatch-context` step and identify the exact
candidate PR input, `stackpop/edgezero` head repository, and current PR head SHA. The step's success is
the API-visible evidence for the internal workflow-SHA assertion. Every App variable/secret
`updated_at` value is no later than that run's completion time. Any new candidate commit or credential
update invalidates the smoke and requires the bounded transition again. The temporary branch policy
is literal `main`, never a wildcard or caller-supplied branch.

Repository-administrator bypass of environment protection is disabled. GitHub's documented REST
environment representation does not expose that switch, so neither the helper nor its fake-API tests
claim to verify it automatically. After the bounded credential smoke is complete and the final
tag-only policy is restored, an independent maintainer who is not the preflight verifier opens the
repository's `build-container-release` environment settings and captures a PNG showing the repository,
environment name, disabled administrator-bypass control, and final deployment-policy list.
The verifier supplies that file plus the reviewer's login and RFC 3339 review time to the preflight.
The helper rejects a non-PNG file, a reviewer equal to the verifier, a future review time, or evidence
whose recorded candidate head SHA differs from the current PR head; it records that SHA, the literal
basename, and `sha256:<64-lowercase-hex>` file digest under `environment.administrator-bypass` with
`allowed:false`, `verification:"manual-ui"`, `reviewer`, and `reviewed-at`. A separately
authenticated operator attaches the byte-identical PNG with the canonical evidence and digest to the
candidate PR. Release checkpoint 1 requires a maintainer other than the verifier and recorded reviewer
to recompute the attachment digest and confirm the screenshot visibly proves the disabled setting.
Any subsequent environment-policy change or new candidate commit invalidates this manual evidence.

Before designating or tagging `S`, an operator runs the repository-owned preflight with a dedicated
policy-audit token, a separate package-audit token, the candidate PR number, the expected App and
installation IDs, and the App private key from a local file. The helper is executed only from a clean,
detached checkout whose `HEAD` is exact gate SHA `G`; it verifies that condition before reading a
credential. The policy-audit token is a short-lived fine-grained personal access token owned by the
verified active `stackpop` organization-owner login, selected for only `stackpop/edgezero`, with
repository permissions `Actions:read`, `Checks:read`, `Environments:read`, `Pull requests:read`,
`Variables:read`, implicit `Metadata:read`, and `Administration:write`, plus organization permissions `Members:read`
and `Administration:write`. The two administration grants are unavoidable for the reviewed GitHub API:
repository ruleset bypass actors are hidden without ruleset write access, and organization required-
workflow ruleset inspection requires organization administration access. This is an administrative
credential even though the helper is read-only; the contract does not claim that GitHub exposes a
machine-verifiable complete grant set for a supplied fine-grained PAT.

Before use, a second operator records the token's selected repository, exact displayed grants,
expiration, and a screenshot digest in the prerequisite evidence. The helper authenticates the
expected verifier login and uses the policy token only through one wrapper whose literal allowlist is
`GET` on these routes, with only documented pagination and filter query keys:

```text
/user
/orgs/stackpop/memberships/{verifier-login}
/orgs/stackpop/teams/edgezero-build-container-releasers/memberships/{verifier-login}
/orgs/stackpop/actions/permissions
/orgs/stackpop/rulesets
/orgs/stackpop/rulesets/{ruleset-id}
/repos/stackpop/edgezero
/repos/stackpop/edgezero/actions/permissions
/repos/stackpop/edgezero/immutable-releases
/users/{publisher-bot-login}
/repos/stackpop/edgezero/pulls/{candidate-pr}
/repos/stackpop/edgezero/rulesets
/repos/stackpop/edgezero/rulesets/{ruleset-id}
/repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_GATE_SHA
/repos/stackpop/edgezero/actions/variables/{approved-repository-variable-name}
/repos/stackpop/edgezero/environments/build-container-release
/repos/stackpop/edgezero/environments/build-container-release/deployment-branch-policies
/repos/stackpop/edgezero/environments/build-container-release/deployment_protection_rules
/repos/stackpop/edgezero/environments/build-container-release/variables/{approved-variable-name}
/repos/stackpop/edgezero/environments/build-container-release/secrets/EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY
/repos/stackpop/edgezero/commits/{candidate-sha}/check-runs
/repos/stackpop/edgezero/actions/runs/{run-id}
/repos/stackpop/edgezero/actions/runs/{run-id}/jobs
/repos/stackpop/edgezero/actions/runs/{run-id}/approvals
```

Every GitHub REST request made by a gate helper, including the approval gate and bounded App-token
test, sets exact non-authorization headers `Accept: application/vnd.github+json`,
`X-GitHub-Api-Version: 2026-03-10`, and `User-Agent: edgezero-build-container-gate/1`. A missing or
different value fails before network access. Authorization is added only by the credential-specific
wrapper. Responses must report the selected API version, use the expected JSON media type, and obey
the endpoint's exact success status; a redirect or silent fallback is failure. Specifically, every
response must contain `X-GitHub-Api-Version-Selected: 2026-03-10`. A response with a body must parse
its `Content-Type` to media type exactly `application/json`, with no charset or charset `utf-8`, and
must contain exactly one complete JSON value. GET succeeds only with 200, bounded test-token creation
only with 201, and token revocation only with 204 and an empty body; the 204 response has no JSON
media-type requirement.

`{approved-variable-name}` is exactly one of `EDGEZERO_BUILD_CONTAINER_APP_ID`,
`EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID`, or
`EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID`.
`{approved-repository-variable-name}` is exactly one of
`EDGEZERO_BUILD_CONTAINER_RELEASE_STATE`, `EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID`,
`EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID`, or `EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN`.
Release state obeys the exact state grammar above, App/Bot IDs are canonical positive decimals, and
the bot login equals the independently verified dedicated App bot account. The user lookup must
return that exact login, numeric id equal to `EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID`, and
`type:"Bot"`. All other numeric
placeholders are canonical positive decimals; SHA and login placeholders must equal values already
validated from the candidate or authenticated API response. Paginated list calls require
`per_page=100` and a canonical positive `page`; check-run
calls also
fix the documented app/latest filters used by the verifier. No redirect is followed. A request with
any other credential, method, path, placeholder value, query key/value, or fixed header fails before
network access; fake-API and static tests cover the complete allowlist. The package-audit token
is a classic personal access token belonging to an active `stackpop` organization owner. Its granted
normalized OAuth-scope set is exactly `{read:org,read:packages}`; the helper verifies the
authenticated login, active owner membership, and returned `X-OAuth-Scopes` header before using that
same token for every package query. Neither local token is stored in GitHub Actions. They are supplied
only as `EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN` and
`EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN`, respectively. The helper rejects byte-equal token values
before making an API request and never logs either value. The policy- and package-token wrappers never
issue a mutation request. The helper's only non-GET requests are the bounded test-token creation and
revocation calls described below; it never mutates persistent repository settings, packages, pull
requests, rulesets, or comments. It requires all of the following and emits canonical evidence for a
separately authenticated operator to attach to the candidate PR:

- environment `build-container-release` has administrator bypass disabled, a nonempty
  `required_reviewers` rule with `prevent_self_review=true`, uses custom deployment policies, and has
  no GitHub App custom deployment-protection rule and exactly one deployment policy, type `tag`, with
  name `build-container-v*`. The protection-rules endpoint must return HTTP 200, `total_count:0`, and
  an empty `custom_deployment_protection_rules` array; its separately supplied
  administrator-bypass evidence satisfies the manual contract above;
- organization and repository Actions permission responses both have
  `sha_pinning_required:false`; if an enterprise override still rejects exact version tags, the
  hosted exact-version prerequisite fails and release is blocked. Consumer repositories must likewise
  permit version-tag action refs;
- an active organization ruleset with no bypass actor has exactly one required-workflow descriptor:
  the EdgeZero repository id, `.github/workflows/build-container-ci.yml`, full SHA `G`, and no ref;
  `do_not_enforce_on_create` is false. Repository ruleset `edgezero-build-container-main` has the exact
  target, enforcement, ref conditions, no-bypass state, pull-request parameters, and seven merge-queue
  parameters defined above; no default or omitted field may weaken them. `.github/CODEOWNERS` assigns
  the canonical gate-owned path manifest and every listed path to the exact gate-reviewer team. The
  protected workflow's source repository, path, workflow SHA, and candidate SHA are recorded. The
  exact App-only pin-branch ruleset is active and its Integration actor id equals the dedicated
  publisher App id. The immutable-releases endpoint returns HTTP 200; parsed field `enabled` is
  exactly boolean `true`, and `enforced_by_owner` is present as a boolean and recorded. Additional
  response fields do not change this decision. Four active repository
  tag rulesets have the exact source/target/ref/rule objects defined above: the image and action
  creation/immutability pairs. Each immutability ruleset prohibits update and deletion and has no
  bypass actor. Each creation ruleset prohibits creation and has exactly one bypass actor: team
  `edgezero-build-container-releasers`, with the numeric ID stored in
  `EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID` and bypass mode `always`; it contains no update or deletion
  rule. The verifier actor is an active member of that team. The candidate's successful
  required-workflow run is a `merge_group`
  run for its final queue merge candidate, uses that exact protected workflow source, and contains
  successful `build-container-local` and `build-container-pin` jobs from the GitHub Actions App;
- protected-environment variables `EDGEZERO_BUILD_CONTAINER_APP_ID` and
  `EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID` equal the reviewed numeric IDs, environment variable
  `EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID` equals the ruleset's reviewed team ID, and secret metadata
  includes `EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY` without exposing its value. Their `updated_at`
  values are no later than the successful credential-smoke completion time. Repository variable
  `EDGEZERO_BUILD_CONTAINER_GATE_SHA` equals `G`, the ruleset descriptor SHA, and the successful
  credential-smoke workflow SHA. Repository release state equals `enabled`; publisher App id equals
  the environment App id and pin-branch bypass actor; and publisher bot id/login identify the verified
  App bot account used for pin PRs;
- an App JWT made from that key identifies the expected dedicated App; the expected installation is
  active on account `stackpop`, uses selected repositories, grants exactly `contents:write`,
  `pull_requests:write`, and implicit `metadata:read`, and its repository list is exactly
  `stackpop/edgezero`;
- an installation token can be minted for only the EdgeZero repository ID with explicit
  `contents:write` and `pull_requests:write`, its response reports only those requested permissions
  plus implicit metadata read, it can read `stackpop/edgezero`, and it is revoked before the helper
  exits; and
- the candidate's `build-container-release-preflight` check run completed successfully, came from the
  same protected workflow and GitHub Actions App integration, and belongs to a `workflow_dispatch` run
  whose run record has exact path, `head_sha=G`, and exact candidate-bound display title; whose
  separately supplied input PR, head repository, and head SHA are independently resolved through the
  PR API by the trusted step and equal the current candidate values; and whose sole
  `assert-exact-g-dispatch-context` step succeeded. It records the expected installation ID without
  exposing a token. After queue merge, the repository-local workflow's latest-attempt `push` run for
  exact `S` has the required event/path/head fields, both stable jobs succeeded for `head_sha=S`, and
  each has exactly one successful `assert-exact-main-push-context` step before the release tag is created;
  and
- repository/package identity and the absent-before-first-push or public-and-repository-linked package
  state are the exact release state expected by the invocation. Absence is established only by a
  successful, fully paginated organization-container-package listing made with the verified active
  organization owner's package-audit token and containing no exact name match; a listing made by any
  other identity, a GET 404, or an authorization failure is never absence.

The package-audit token has its own GET-only wrapper limited to `/user`,
`/orgs/stackpop/memberships/{package-login}`, the fully paginated
`/orgs/stackpop/packages?package_type=container&per_page=100&page={page}` listing, and
`/orgs/stackpop/packages/container/edgezero-build-app-cli`. The App JWT wrapper permits only
`GET /app`, `GET /app/installations/{expected-installation-id}`, and
`POST /app/installations/{expected-installation-id}/access_tokens` with the exact repository id and
requested permission body. The resulting installation-token wrapper permits only
`GET /installation/repositories?per_page=100&page={page}`, `GET /repos/stackpop/edgezero`, and
`DELETE /installation/token`. The POST and DELETE create and revoke only the bounded test token; no credential
may call a persistent repository, organization, package, pull-request, comment, or ruleset mutation
endpoint.

API failure, pagination truncation, ambiguity, extra bypass actor, extra repository or write
permission, credential failure, or evidence-post failure blocks release designation or publication.
After the first push creates the
package, publication stops until an operator makes it public and confirms it is linked to
`stackpop/edgezero` through a fresh package-present preflight from `G`. GHCR exposes no enforceable per-version retention lock, so this contract does not
claim one. Repository workflows contain no package-deletion endpoint or delete-scoped credential;
manual deletion by a package or organization administrator is an accepted operational risk that can
break existing digest-pinned consumers and requires an emergency rebuild plus new reviewed pin. The
workflow also verifies `S` is an ancestor of the protected default branch. All publication,
pin-record mutation, and gate rotation uses the exact repository-global concurrency group
`edgezero-build-container-publication` with `cancel-in-progress: false` and `queue: max`; therefore at
most one run executes the mutation path and up to 100 wait. A run rejected because that queue is full
publishes no pin and must be rerun after capacity is available. Pin branches remain exactly source-
derived as `edgezero-build-container-pin/<S>` and updates are idempotent under explicit force-with-
lease.

GitHub accepts `queue: max` and the four `job.workflow_*` reusable-workflow identity properties, but
pinned actionlint 1.7.12 predates both additions. The gate does not pretend the raw linter accepts
them. A gate-owned compatibility wrapper first uses pinned mikefarah yq 4.53.3 to require `queue` only
at workflow-level in exactly the publisher and rotation-lock workflows, with scalar `max`, exact
shared group, and literal `cancel-in-progress:false`. It also permits only
`job.workflow_repository`, `job.workflow_file_path`, `job.workflow_ref`, and `job.workflow_sha`, only
in the exact checked expressions/checkout ref locations of `.github/workflows/build-app-cli.yml`; a
misspelling, extra property, other workflow/location, alias, duplicate, or dynamic expression fails.

After structural validation, the wrapper creates line-count-preserving temporary copies: it replaces
only those exact approved `job.workflow_*` scalar expressions with same-type constants and replaces
only the two approved `queue` lines with blank lines. It runs unfiltered actionlint 1.7.12 over those
copies and remaps any diagnostic to the real path/line; no `-ignore` rule or diagnostic filter is used.
Self-tests require raw 1.7.12 to emit exactly the reviewed unsupported-diagnostic set for canonical
queue and job-context fixtures, require the sanitized copies to pass, and require every malformed or
additional use to fail before sanitization. Remove each compatibility rewrite independently once a
reviewed actionlint release natively supports that syntax.

After acquiring that group, every publisher requires
`EDGEZERO_BUILD_CONTAINER_RELEASE_STATE==enabled`, verifies the active gate variable and organization
descriptor agree through the already reviewed prerequisite evidence, and proves no rotation lock is
active before image build or registry authentication. The publisher has two jobs. `build-and-verify`
does not reference the protected environment; it checks out without persisted credentials, proves
`HEAD == S` and the recursive checkout is clean immediately
before the gate-context build, pushes and anonymously verifies `D`, and exports only non-secret
`{S,D,protocol,tag,approval-challenge}` job outputs. After anonymous verification it obtains 32 bytes
from the runner OS CSPRNG and renders `approval-challenge` as 64 lowercase hexadecimal characters. It
writes the exact challenge, `S`, `D`, tag, run id, and run attempt to the job summary so the approver
can inspect them; the challenge is public but unpredictable before this attempt reaches that point. It
builds only the freshly staged `G` context, which contains the reviewed `.dockerignore`. Only after
that job succeeds does `update-pin` start with
`environment: build-container-release`. That job does no image build, receives the non-secret outputs,
checks out without persisted credentials, mints the scoped App token, and performs only the pin branch
and PR mutation. Thus the environment's private key is unavailable to the gate-context build job.
Each publisher job has exactly one fixed `assert-exact-publisher-context` step, executed from the
active `G` checkout before sensitive work. It verifies tag event/ref, `github.sha==github.workflow_sha`
and equals release source `S`, exact run id/attempt/tag, active gate, and enabled release state; a
missing, duplicate, failed, candidate-resolved, or differently named assertion is fatal.
The publisher workflow itself is a gate-owned file unchanged from `G`; it checks out trusted helper
code at repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA` into a separate root. Before image
build, the helper validates `S`'s isolated release request, verifies every gate/context path at `S` is
byte-identical to `G`, and copies only canonical manifested paths from the clean `G` root into a fresh
context outside both checkouts. The Dockerfile comes from `G`; candidate Dockerfiles, tools,
validators, context files, and post-install replacement steps are unreachable. The workflow executes
the approval gate and pin updater only from the same `G` root. The protected gate's structural
publisher checker rejects any candidate that changes this topology, permissions, ordering, action
pin, checkout source, context-construction source, or helper invocation.

Pin branches and PRs use a short-lived, protected-environment GitHub App installation token requested
for repository `edgezero` with explicit `contents:write` and `pull_requests:write`. The publisher
requires the token action's installation-ID output to equal
`EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID` before use. They do not use `GITHUB_TOKEN`: its push
does not create a new workflow run, so it cannot guarantee the automatic protected pin-check path. The branch updater records the remote OID and
uses an explicit force-with-lease; ambiguous, closed, superseded, and already-merged PR states follow
the fixture-tested fail-closed state machine in the implementation plan. It writes both pin records
through gate-owned typed encoders, uses the exact protected branch/title, and records its own run and
current approval values; it cannot accept a caller-supplied evidence file.

Let `I` be `image-source-revision` in the protected default branch's current `image.json`; absence is
the first-pin state. Normal publication may propose `S` only when `I` is absent, `I==S`, or `I` is an
ancestor of `S`. An older or incomparable `S` fails before mutation. The publisher fully paginates all
open pin PRs whose author id/login equals the authenticated dedicated App and whose head repository is
exactly `stackpop/edgezero`, and fails if a matching pin branch or title is owned by another actor or
repository. It compares every proposed source with `S`: it closes older-source PRs when
superseding them; updates one exact `{S,D}` PR idempotently; closes and replaces one same-`S`, different-`D`
PR; treats a run older than an existing proposal as superseded success without mutation; and fails on
incomparable, malformed, multiple-same-source, or otherwise ambiguous state. Required pin
CI recomputes the `I`-to-`S` ancestry relation against the PR's current merge-queue base, so a stale
older PR cannot merge after a newer pin. Operational rollback never regresses default-branch
`image.json`; consumers select an earlier reviewed exact action version containing its corresponding
pin.

The administrator-bypass screenshot is repeated at the protected-secret boundary. For every workflow
run attempt in which `update-pin` is eligible, including every same-tag rerun, its environment approver
waits for `build-and-verify` to succeed, opens the environment settings, and captures a fresh PNG that
visibly includes the repository, environment, disabled administrator-bypass control, and sole final
tag policy. The
approver computes its digest and enters exactly one line as the environment review comment before
approving the job:

```text
edgezero-release-evidence-v1 {"challenge":"<64-lowercase-hex>","image-digest":"<D>","png-sha256":"sha256:<64-lowercase-hex>","release-tag":"<tag>","reviewed-at":"<RFC3339-UTC>","run-attempt":"<canonical-positive-u32>","run-id":"<canonical-positive-u64>","source-revision":"<S>"}
```

The JSON is compact, uses the shown key order and string types with no extra key or whitespace, and every
placeholder obeys its already defined syntax. `RFC3339-UTC` here is exactly a valid calendar instant
in `YYYY-MM-DDTHH:MM:SSZ` form, with no fractional seconds or offset spelling. The exact `reviewed-at` instant is neither future nor
more than 15 minutes before the machine check. `update-pin` initially has only `actions:read` and
`contents:read`. After checking out exact gate SHA `G` without persisted credentials and before
App-token minting, its trusted gate helper uses the current `GITHUB_TOKEN` only for exact no-redirect
`GET /repos/stackpop/edgezero/actions/runs/{github.run_id}` and
`GET /repos/stackpop/edgezero/actions/runs/{github.run_id}/approvals`, with the fixed REST headers
defined above. The approval endpoint is
non-paginated; the helper requires one complete, valid HTTP 200 JSON array. It requires the API run id and
`run_attempt` to equal `github.run_id` and `github.run_attempt`; exactly one approved review for
`build-container-release` must have the exact current challenge, `D`, and remaining fields above, and
the API reviewer's login becomes the recorded approver. Every protocol-prefixed review claiming the
current run id and attempt is parsed: there must be exactly one, it must be approved and exact, and no
second current-attempt protocol record may exist. A rejected or mismatched current-attempt record,
missing or malformed history, environment bypass without the approval, stale or future time, or a
different challenge fails before any mutation credential exists. Records for earlier attempts remain
historical data but can never satisfy the current attempt; a prior reviewer cannot predeclare a useful
future-attempt approval because that attempt's CSPRNG challenge does not yet exist.

Only after that gate passes may `actions/create-github-app-token@v3.2.0` mint the App token. The release operator then
attaches a canonical record containing the API reviewer login and exact comment plus the byte-identical
PNG to the release evidence. The comment cryptographically binds the reviewer attestation to the PNG
bytes and run attempt; the screenshot's visible meaning remains a required human review, not a claim
of machine image interpretation. The initial private-package stop and every canceled or rerun attempt
require a new capture and approval. This check is required because the API-invisible setting cannot be
proven current by the pre-`S` helper.

The repository's zizmor policy uses `ref-pin` for every non-local action. The structural pin scanner
is the stronger authority: it accepts only canonical exact stable `v<major>.<minor>.<patch>` refs,
strict Docker `sha256` digests, and the exact scanned workflow/action/documentation surfaces. It
rejects commit SHAs as well as floating major/minor tags and branches.

Gate `G` includes the permanent dual-state documentation scanner from the start. It parses fenced
YAML in every tracked Markdown file. Its trusted workflow selects the comparison range from this
closed event table; every SHA is a full lowercase 40-hex object present in the full subject checkout,
and any other event or malformed/inconsistent field fails closed:

- **`pull_request`:** base is `github.event.pull_request.base.sha` and candidate is `github.sha`.
  Require `github.event.pull_request.base.repo.full_name==stackpop/edgezero`,
  `github.event.pull_request.base.ref==main`,
  `github.ref==refs/pull/<number>/merge`, candidate first parent equal to the base SHA, and candidate
  second parent equal to `github.event.pull_request.head.sha`;
- **`merge_group` `checks_requested`:** base is `github.event.merge_group.base_sha` and candidate is
  `github.event.merge_group.head_sha`. Require base ref `refs/heads/main`, head ref equal to
  `github.ref`, head ref beneath exact prefix `refs/heads/gh-readonly-queue/main/`, candidate equal to
  `github.sha`, and base an ancestor of candidate;
- **protected-main `push`:** base is `github.event.before` and candidate is `github.event.after`.
  Require `github.ref==refs/heads/main`, nonzero base, candidate equal to both `github.sha` and
  `github.workflow_sha`, and base an ancestor of candidate.

`workflow_dispatch` is a separate credential-smoke mode and invokes neither the change classifier nor
the documentation scanner. The “other event” rejection above applies whenever either range consumer
is invoked.

The scanner compares the selected base and candidate states as follows:

- **bootstrap:** `docs/.edgezero-action-release.json` is absent from both base and candidate. Only the
  four named prepublication adoption documents may use literal `<EDGEZERO_ACTION_VERSION>` for an
  EdgeZero ref; every other external ref still obeys the exact stable-version rule;
- **transition:** the record is absent from base and added by the candidate. The candidate changes
  only tracked Markdown plus that record, every EdgeZero ref equals its literal `V`, no placeholder
  remains, and a fixed no-redirect versioned API/ref verifier proves `V` is a published
  `draft:false`, `prerelease:false`, `immutable:true` release whose peeled tag equals recorded `P`;
- **released:** the record exists on the base and cannot be deleted. It is either byte-identical or a
  documentation-only candidate atomically replaces it and all EdgeZero documentation refs with a
  strictly greater canonical stable version whose immutable release/ref binding to its new `P` passes
  the same hosted proof. Every candidate remains placeholder-free and every EdgeZero ref in each
  fenced workflow equals the candidate record's `V`; downgrade, partial update, or non-document
  change fails closed.

The first transition therefore happens only in `R`, after `V` exists. Candidate `H` never puts an
unpublished or retired version into protected-main documentation, no later PR can return to
bootstrap mode, and later action releases repeat the same post-release atomic record/documentation
update. During the queue, “candidate” means the synthetic pull-request or merge-group commit selected
above; `R` names only the resulting protected-main commit after the push range passes.

## 9. Testing

Required automated coverage includes:

- cold, warm, uncached, corrupt-restore, stop-failure, write-error, audit-failure, and save-warning
  cache paths, plus separate complete lookup-eligibility and protected-event save-authorization truth
  tables for both cache families;
- fixed host path restoration, cross-host-checkout-path hits, nested workspace and sibling path deps,
  public Git dependencies, concurrent generations, seven-day expiry as documented behavior, exactly
  one Cargo compile/build invocation after metadata preflight, no action-level retry, and pinned
  sccache response-loss fallback behavior;
- cache audit type/owner/path/layout/logical-byte/non-sparse/path-length/entry-count checks and
  arbitrary app-written regular data disclosure;
- full source inventory, deleted/modified tracked paths, gitlinks, escaping symlinks, overlapping or
  tracked-containing output roots, nested-project implicit Fastly `bin` and `pkg` roots, absent-root
  precreation, special-file/hardlink rejection, descriptor-relative cleanup, caller-declared generated
  output, undeclared output rejection, source-free lifecycle bypass of Copy B checks, and unchanged
  original checkout;
- every environment and mount profile, including token absence, production healthcheck tokenlessness,
  staging token presence, credential-free `app-build`, every exact `app-env` name/value/count/size
  boundary, empty Cargo-config policy, config-push repo/config confinement, and deploy-without-sccache;
- strict caller identity, full-SHA app refs, exact-version workflow/action refs, resolved workflow
  SHA, immutable EdgeZero release enforcement, locally derived platform identity, matrix artifacts,
  and consumer recomputation for private repositories;
- exact canonical metadata and expected JSON, typed `write-expected`, schema versions, duplicate keys,
  byte-exact ustar
  headers/padding/end blocks, deterministic package output, every accepted/rejected dynamic string and
  object-acquisition tag, conservative ELF/loadability vectors, all provenance golden/malformed
  fixtures, exact ELF header sizes/versions and extended-numbering rejection, dynamic-token rejection,
  controlled direct-loader invocation, absent system preload, inhibited cache, flat runtime-library
  closure, hardware-capability/default-path non-substitution, explicit `dlopen` non-claim,
  every consumer independently writing fresh expected identity, provider actions independently
  validating named artifacts and rechecking the binary
  handoff, and the split parse/extract versus binary-smoke boundary;
- exact Rust/Fastly/sccache versions, installed wasm target plus a minimal wasm compile, image labels,
  leaf-manifest platform checks, anonymous pulls, always-materialized required container jobs,
  protected gate/workflow identity, gate-owned staged build context and post-install replacement
  resistance, release-request isolation, required-workflow descriptor and bypass checks, exact
  merge-queue payload and single-entry behavior, API-visible exact-`S` push assertion-step evidence,
  image-pin deletion and source-ancestry ordering, environment
  reviewer/self-review/deployment-policy checks, per-attempt approval comment and token-ordering checks,
  policy-API method/path/header/version allowlisting, App installation/repository/permission/token-
  scope checks, actionlint queue-compatibility isolation, publication concurrency and queue-overflow
  cancellation, gate-rotation failure recovery, and release rerun/idempotency;
- production/staging deploy, active-version, healthcheck, rollback, config push, mutation signaling,
  cancellation, and the exclusive `--staging` spelling.

Cold evidence starts from an empty audited cache root and, after zeroing statistics, requires
`cache_misses.counts["Rust"]>=1`, `cache_writes>=1`, and zero write errors. Warm evidence runs in a
new job with fresh target/Cargo-home directories, restores the recorded cold generation through the
sole family prefix, zeros statistics, and requires `cache_hits.counts["Rust"]>=1`; the rebuilt binary
digest must equal the cold binary digest. Default-off evidence proves no cache action or sccache
process ran. Dependency fetching remains online because source archives are not cached. Wall-clock
improvement is telemetry, not a pass/fail condition.

## 10. Rollout and migration

To publish final action revision `P`, exact version `V`, and its adoption documentation:

1. Migrate every existing non-local external action and reusable workflow reference in the repository
   to a reviewed exact stable patch-version tag, change the repository-wide pin gate accordingly, and
   retain zizmor `ref-pin` as defense in depth. Record the accepted third-party tag-movement risk.
2. Implement and separately land the validator, schema, fixtures, protected classifier/verifier
   helpers, publisher contract checker, and required workflow as gate baseline `G`. Configure the
   organization required-workflow rule directly to gate commit `G` (this is not a consumer `uses:`
   ref) and the mandatory default-branch merge queue.
3. Include the exact Dockerfile and complete image-context closure in `G`. Open an isolated canonical
   `release-request.json` candidate, run it through the protected gate, complete the credential smoke,
   merge it only through the merge queue as `S`, and require the API-visible exact post-merge `S` push
   assertion-step evidence before tagging. Build only from the freshly staged `G` context.
4. Publish and anonymously verify the image, then merge the ancestry-checked pin PR as baseline `B`.
5. Select unused canonical patch version `C`. Land the reusable workflow, cache, provenance,
   launcher, and consumer integration while leaving prepublication adoption examples at the gated
   placeholder. Record resulting main commit `H`.
6. Rerun the complete local suite from detached `H`; have a verified active releaser-team member use
   a local credential to draft and publish immutable `C` at `H` with `prerelease:true`; and run the
   complete hosted cross-repository/provider suite through literal `C`. On success designate `H=P`,
   select unused stable `V`, use the same auditable actor procedure to draft and publish it at `P`,
   verify both release/ref resolutions and attestations, and run the final literal-`V` hosted smoke.
   A candidate failure follows the new-commit/new-`C` rules in Section 8.
7. After the literal-`V` smoke passes, merge documentation-only `R` through the protected queue. Add
   the `{V,P}` action-release record and replace every gated placeholder with literal `V`; the
   preinstalled dual-state gate proves the release/ref binding and permanently enters released mode.
   Run the docs build and exact-version scan on protected main and record `R`.

Caching remains off by default. Container execution and provenance validation are unconditional.

## 11. Out of scope

- Detecting sccache staleness from undeclared proc-macro or `build.rs` inputs.
- Authenticating the artifact producer or proving workflow-bound attestation.
- Caching dependency source archives, private dependency credentials, native-tool sccache wrapping,
  self-hosted runners, alternate toolchains, non-default feature sets, or non-Fastly adapters.
- Cache lineage merging, family-local eviction, or action-managed cache deletion.
- Preventing a GHCR package administrator from manually deleting a supported image digest; GitHub does
  not expose a per-version retention lock for this contract.

## 12. History

- **v6.17:** introduced the build-only reusable workflow, consumer deployment job, deploy-compile
  profile, full working-copy verification, `job.check_run_id`, and explicit undeclared-input risk.
- **v6.18:** split trusted provenance extraction from untrusted binary execution; completed provider
  mount/environment profiles; strengthened full-inventory source verification; made cache family and
  warning-only saves coherent; corrected sccache error/size/audit contracts; made platform identity
  action-derived; replaced impossible same-SHA publication with image source `S`, pin baseline `B`,
  and final action revision `P`; made full-SHA external references normative; and made validator
  capability fixtures a hard publication prerequisite.
- **v6.19:** froze the protocol-1 metadata and expected-identity JSON shapes, schema/version bounds,
  byte-exact ustar encoding, conservative ELF/loader profile including exact dynamic-string and
  object-acquisition semantics, and shared package/validate authority; selected the exact base and
  sccache artifacts; moved release prerequisites before `S` with verifiable environment and
  least-privilege App controls, including explicit manual evidence for the API-invisible administrator
  bypass setting and an organization-owner package-audit identity; aligned zizmor with full-SHA policy;
  removed unenforceable GHCR retention claims; and replaced path-filtered required image jobs with an
  always-triggered workflow whose stable jobs explicitly succeed when not applicable.
- **v6.20:** made cache writes an action-derived protected-event decision; added the typed canonical
  expected-identity producer; froze suffix and ELF edge cases; replaced candidate-controlled image
  checks with an immutable-SHA organization required workflow and mandatory merge queue; required an
  exact post-merge source check; split tag creation authority from no-bypass immutability; specified the policy-audit credential and request allowlist; bound
  each protected-secret approval to a per-attempt CSPRNG challenge, image and screenshot digest before token minting; defined
  forward-only pin ancestry and bounded publication queue semantics; and assigned current Fastly
  `pkg` output to the source-freeze contract.
- **v6.21:** replaced archiver-dependent cache sizing with exact filesystem-tree bounds; froze the
  app-environment, Cargo-config, generated-output, nested Fastly `bin`/`pkg`, cleanup, and controlled
  loader contracts; moved every repository image-context input into gate `G` and made `S` an isolated release
  request built from a staged trusted context; defined gate rotation and recovery, exact one-entry
  merge-queue policy, API-visible dispatch/push assertion evidence, REST API headers/version, and the
  narrowly scoped actionlint 1.7.12 `queue: max` compatibility check.
- **v6.22:** separated the self-referential action release from its documentation by defining
  documentation-only revision `R`, whose concrete examples pin already-known full SHA `P`; froze the
  reusable-workflow input, self-checkout, artifact transport, network/resource, and private binary-
  state contracts; froze the organization required-workflow ruleset's repository/ref target; and made
  the protected release environment's machine-verified absence of GitHub App custom deployment-
  protection rules an explicit prerequisite for the non-deployment credential-smoke job.
- **v6.23:** replaced full-SHA action/workflow references with exact stable patch-version tags and an
  immutable EdgeZero release process; removed the documentation-only self-reference workaround;
  defined authority-checkout export and pinned-LFS rules, uncached compilation, pre-restore
  disclosure, parent-cache authorization, closed sccache JSON statistics, consumer expected-file
  production, candidate-bound credential-smoke evidence, standalone validator workspace/build
  closure, exact release-request production, self-test isolation, and generated-output sparse/size
  limits.
- **v6.24:** made exact version tags unambiguous for every non-local `uses:` reference while retaining
  commit SHAs only for source/provenance identity; removed the last cache-action SHA-pinning
  contradiction; assigned distinct notation to the current image source pin; froze the complete
  sccache statistics schema and warm-evidence counters; and specified the exact checksummed host Git
  LFS installation, authenticated materialization, and credential-removal sequence.
- **v6.25:** made release-candidate `uses:` refs exact patch versions without prerelease suffixes;
  removed circular `P` qualification; matched the immutable-release settings response; distinguished
  immutable tag identity from accepted privileged release-object deletion risk; and replaced
  unprovable historical release-name absence with current ref/release absence plus creation success;
  reserved `H` for the final action candidate and renamed the generic protected head `Q`; made every
  normative JCS example a literal single line; closed Rust/native compiler overrides; moved filter/
  submodule validation before worktree creation; fixed Git LFS asset size/transfer bounds; defined
  local bootstrap image `L`; assigned config-push authority checks, permanent documentation pin
  enforcement, and the auditable action-release actor/procedure.
- **v6.26:** moved literal stable-version documentation into post-release revision `R` under a
  one-way dual-state gate so failed candidates cannot strand unpublished refs on main; selected `V`
  only after `H` qualifies as `P`; froze the local fine-grained release PAT permissions, API
  allowlist, actor proof, and credential handling; and defined the public consumer identity action
  plus per-invocation source materialization so no authority path crosses a public action boundary;
  and closed the documentation scanner's pull-request, merge-group, and protected-push ranges.

## 13. Deferred implementation mechanics

Implementation plans may choose helper names and internal module boundaries. They must commit the
schema implementing Section 6.2, golden bytes and malformed fixtures for Sections 6.2 through 6.5,
sccache v0.10 layout/stats fixtures, exact cache tree-bound and entry-count vectors, release
versions/checksums, and command-level tests before exact action version `V` at final action revision
`P` is published.
Those are mechanics, not permission to weaken the contracts above.
