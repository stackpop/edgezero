# EdgeZero Deploy Actions - Build Caching Spec

**Status:** Design (proposed) - v6.18

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

The app checkout token is used host-side by `compute-app-cli-identity` to verify a private
repository's id. It is never copied into a container, working tree, artifact, or cache.

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

- `workspace-id` hashes, in order, `app-repo-id` and workspace root relative to `git-root`.
- `suffix-hash` hashes the validated `cache-key-suffix`.

Committed golden vectors cover framing, the root representation, empty suffix, and byte-distinct NFC
and NFD paths that must produce different hashes.

### 3.3 Workflow and action revisions

`app-ref` must be a full lowercase 40-hex commit SHA. Branches, tags, abbreviated SHAs, and the
legacy `--stage` spelling are unsupported. Staged provider operations use only `--staging`.

Every non-local external action and reusable workflow reference in this repository and in documented
consumer workflows must use a full 40-hex commit SHA. Version tags, including major and patch tags,
are not accepted. All EdgeZero references in one consumer workflow use one full action revision `P`.

Inside the called workflow:

- `job.workflow_repository` and `job.workflow_file_path` must identify the expected EdgeZero
  reusable workflow.
- the suffix of `job.workflow_ref` must be a full 40-hex SHA, not a branch or tag;
- `job.workflow_sha` must equal that suffix.

These hosted-runner context properties identify the workflow that defines the current job. They are
part of the hosted-only v1 floor.

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
support only anonymously fetchable crates.io and public Git dependencies.

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

The sequence is:

1. Empty the stable host cache directory.
2. Restore the newest matching cache.
3. Audit restored data. On restore or audit failure, clear the directory and continue cold.
4. Start sccache, zero its statistics, and compile once.
5. Capture `sccache --show-stats` and stop the server.
6. Audit the stopped directory again.
7. Save only when the compile succeeded, stop succeeded, the final audit passed, and the captured
   `cache_write_errors` count is zero.

Storage lookup and decompression failures that sccache v0.10 treats as misses remain misses.
`SCCACHE_IGNORE_SERVER_IO_ERROR=1` is set because it covers selected client/server response failures;
it is not described as covering startup, connection, extraction, or every backend error. Any other
sccache error follows pinned v0.10 behavior. An ordinary compiler failure is surfaced once and is
never retried by the cache layer.

If `sccache --stop-server` fails, the action skips save with a warning. If cache write errors are
non-zero, the build may still succeed but save is skipped with a warning. Restore, save, and cache
absence never trigger a second compilation.

### 4.4 Cache audit and disclosure

`SCCACHE_CACHE_SIZE=2G` is the managed sccache capacity. It is not the hard archive bound. The
post-stop audit computes a worst-case upper bound for the final cache archive using the pinned cache
client's tar and compression formats, including every entry header, file padding, end marker, and
compression framing/expansion, and requires that bound to be at most 2 GiB. It also applies a fixed
entry-count ceiling from committed v0.10 layout fixtures.

Before use after restore and before save, the audit requires:

- the canonical audited root is exactly `SCCACHE_DIR`;
- every entry is a regular file or directory beneath that root;
- no symlink, socket, FIFO, device, mount escape, or special file exists, and every regular file has
  `nlink == 1` (directory link counts are not constrained);
- ownership is the expected container uid/gid;
- layout and record names match the pinned v0.10 format fixtures;
- the calculated archive upper bound and entry count satisfy the limits above.

The application is trusted, but cached compilation shares a writable uid and `SCCACHE_DIR` with app
code. App code can therefore place arbitrary bytes in that directory. The audit constrains shape and
size, not authorship or semantic content. The cache is not content-authenticated and the disclosure
acknowledgement covers the entire archived directory, compiler diagnostics, and app-written bytes
that satisfy the audit.

Every cross-repository build requires `disclosure-acknowledged: true`; equal repository ids are the
only exemption.

## 5. Container execution

### 5.1 Image and runner

The EdgeZero image is public and anonymously pullable by digest, retained while referenced, and a
leaf `linux/amd64` image manifest rather than an OCI index. It is built from a digest-pinned base and
contains:

- the exact Rust toolchain from `.tool-versions` and an installed `wasm32-wasip1` target;
- exact pinned Fastly CLI and sccache versions with checksum-verified downloads;
- `git`, `jq`, `tar`, `curl`, CA certificates, and a C toolchain;
- the project-owned provenance validator and its protocol/schema assets.

Runtime containers use a read-only root filesystem, uid/gid 1001, dropped capabilities,
`no-new-privileges`, no GitHub file-command channels, explicit mounts, and operation-specific network,
memory, pid, and timeout limits.

### 5.2 Working-copy topology

There are two independent copies because GitHub jobs do not share filesystems:

- **Copy A, producer build job:** a faithful writable copy used only for cached native CLI
  compilation. The reusable workflow uploads its CLI artifact; Copy A is then discarded.
- **Copy B, consumer deployment job:** a fresh faithful writable copy made from the consumer's own
  checkout. Provider actions in that job may reuse Copy B so generated files flow from app build to
  `fastly compute deploy`. Copy A never crosses into this job.

Each copy preserves the entire repository layout, enclosing workspaces, parent Cargo config, sibling
path dependencies, file modes, symlink targets, and initialized submodule state. It includes tracked
files and initialized submodules only; ignored and untracked detritus is absent. Hardlinks to the
original are broken. The read-only original checkout remains the freeze authority.

### 5.3 Mount profiles

`run-app-cli-in-container` has a maximum allowlist and a closed profile for each operation. It never
mounts all of `RUNNER_TEMP`.

| In-container path | Mode | Allowed operations | Source |
| --- | --- | --- | --- |
| `/work/repo` | writable | cached-compile, app-build, provider-deploy | Copy A or Copy B |
| `/work/repo` | read-only | config-push | frozen original checkout |
| `/work/target` | writable | cached-compile, app-build, provider-deploy | fresh or parent target cache as specified below |
| `/work/cargo-home` | writable, fresh | cached-compile, app-build, provider-deploy | operation-specific directory |
| `/work/sccache` | writable | cached-compile only | stable host cache directory |
| `/work/input/artifact.tar` | read-only | provenance-validate only | downloaded artifact |
| `/work/input/expected.json` | read-only | provenance-validate only | host-generated expected identity |
| `/work/validated` | writable, fresh | provenance-validate only | empty host output directory |
| `/work/bin/app-cli` | read-only | binary-smoke and provider operations | validated binary |
| `/work/config/inline.toml` | read-only | config-push only | optional action-owned inline config file |
| `/work/package` | writable, fresh | app-build, provider-deploy | staged Fastly package/output |
| `/work/home`, `/work/tmp` | writable tmpfs | all operations | operation-local tmpfs |

Profiles:

- `cached-compile`: Copy A, fresh target and Cargo home, sccache, tmpfs; no token.
- `app-build`: validated CLI, Copy B, fresh Cargo home, package output, and the parent
  `deploy-fastly.cache` target directory when enabled. It runs `<cli> build` for
  `build-mode: always`, has no provider token and no sccache mount, and saves the parent target cache
  before any provider operation.
- `provider-deploy`: Copy B, fresh target/Cargo home/package, validated CLI, tmpfs, provider token;
  never sccache and never a writable cache. Fastly deploy may compile application source with the
  token for both `build-mode` values. A prior `app-build` is a credential-free validation/prebuild and
  does not claim to suppress this recompile; its parent target cache was already saved before the
  token appeared and is never saved again afterward.
- `provenance-validate`: trusted baked validator, read-only tar, fresh writable output directory,
  read-only expected-identity JSON, and tmpfs; no repository, app-binary execution, token, Cargo,
  target, package, or cache mount.
- `binary-smoke`: validated binary only plus tmpfs; no network, token, repository, Cargo, target,
  package, cache, or validator output write access.
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
capture, mutation signaling, healthcheck ordering, and recovery. This addendum changes isolation and
mounting only. Every staged CLI invocation uses `--staging`, never `--stage`.

### 5.4 Constructed environments

Every operation starts with `env -i` and a closed allowlist. `PATH` is
`/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin`.

- cached compile: `PATH`, `RUSTUP_HOME=/usr/local/rustup`, `RUSTUP_TOOLCHAIN`, fresh `CARGO_HOME`,
  fresh `CARGO_TARGET_DIR`, `RUSTC_WRAPPER=/usr/local/bin/sccache`, `SCCACHE_DIR`,
  `SCCACHE_CACHE_SIZE=2G`, `SCCACHE_IGNORE_SERVER_IO_ERROR=1`, `CARGO_INCREMENTAL=0`, empty
  `CARGO_ENCODED_RUSTFLAGS`, `HOME`, `TMPDIR`, and validated `app-env`.
- app build: the Rustup/Cargo variables above except every sccache variable and wrapper, the
  operation's action-owned target/package paths, `HOME`, `TMPDIR`, the validated `app-env` map, and
  validated `EDGEZERO_MANIFEST` when selected; no provider token.
- provider deploy: the Rustup/Cargo variables above except every sccache variable and wrapper, plus
  `FASTLY_API_TOKEN`, the operation's enumerated `EDGEZERO_*` variables, validated `app-env`, and
  validated `EDGEZERO_MANIFEST` when the caller selected a manifest.
- provenance validation and binary smoke: `PATH`, `HOME`, `TMPDIR` only.
- provider operations: `PATH`, `HOME`, `TMPDIR`, only the token required by that operation, and only
  explicitly named `EDGEZERO_*` variables plus validated `app-env`. Config push also receives its
  selected validated overlay names unless `no-env` was selected.

Non-credential application configuration is explicit rather than ambient. `app-env` is a JSON object
input (default `{}`) whose names and values are decoded host-side. Names must match the committed
portable environment-name grammar and must not be provider aliases, `GITHUB_*`, `RUNNER_*`,
`ACTIONS_*`, shell-startup variables, loader variables, compiler/toolchain controls, or action-owned
names. NUL values fail. The caller is responsible for passing no credentials; cross-repository cache
disclosure covers compile-time values. Only the exact validated names are added to operations that
execute app code or the app CLI. Config-push's separately derived typed-config overlay remains subject
to its own prefix and `no-env` rules. This explicit input replaces the parent's ambient workflow-`env`
behavior and is a documented adoption migration.

Caller `PATH`, compiler wrappers, Rust flags, native-tool variables, ambient application variables,
and unlisted `EDGEZERO_*` variables are absent rather than scrubbed after inheritance.

Cargo config across cwd, ancestors, and `CARGO_HOME` permits only the committed allowlist of benign
registry/network keys. `Cargo.lock` must be a tracked regular file. Path dependencies may resolve
anywhere beneath `git-root` and must not escape it. The parent toolchain resolver still runs, but v1
requires its result (including an explicit `rust-toolchain` input) to equal the exact toolchain baked
in `image.json`'s image; a mismatch fails before container launch. Alternate toolchains require a
separate image/protocol and remain out of scope.

## 6. Source freezing and provenance

### 6.1 Freeze and pre-token verification

The original checkout must be full, recursive, non-sparse, have LFS/filter content materialized, and
start clean: `HEAD` equals `source-revision`, no tracked/index or untracked modification exists, and
every initialized submodule is clean at its recorded gitlink. Before and after app-controlled
commands, the original's repository id, HEAD, clean state, and recursive submodule state must remain
unchanged.

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
- entries under an output root must still pass the operation's type and confinement rules.

The declaration authority is the protected caller's `generated-output-paths` JSON-array input to
`deploy-fastly` (default `[]`). Each value is a repository-relative canonical path validated before
app code runs. Action-owned target, Cargo-home, and package paths outside the repository are implicit
and cannot be overridden. Any application whose credential-free build writes inside the repository
must list every permitted root; the action never guesses from observed mutations.

This permits declared generated output while preventing a credential-free build step from rewriting
source that a later token-bearing compile would execute. Source-free lifecycle actions such as
healthcheck and rollback do not receive Copy B and do not run this inventory comparison; they verify
artifact/caller/platform identity instead. Config-push verifies repository id, HEAD, cleanliness, and
confined selected files on its read-only checkout. The consumer repeats the checks applicable to each
mounted source profile before and after provider commands.

### 6.2 Archive contract

The producer emits deterministic POSIX ustar with exactly two regular members in order:
`app-cli-meta.json`, then the fixed `app-cli-bin` basename. PAX and GNU extensions are rejected.
Headers use uid/gid 0, empty uname/gname, mtime 0, empty prefix, typeflag regular, and mode 0644 for
metadata or 0755 for the binary. Extra, duplicate, renamed, linked, special, traversal, or trailing
content is rejected. Total logical size is at most 512 MiB and metadata is at most 64 KiB.

Metadata is RFC 8785 JCS canonical JSON. Duplicate keys are rejected before parsing. A committed JSON
Schema 2020-12 and procedural validation define the exact fields: both identity groups,
`app-cli-version` (informational), `binary-sha256`, `binary-size`, and `abi`.

`abi` is recomputed from ELF data: canonical machine name, `PT_INTERP` string or null, and sorted
direct `DT_NEEDED` strings. Transitive dependencies must resolve inside the pinned image. Runtime
`dlopen` dependencies are outside this contract.

### 6.3 Split validation boundary

Validation is deliberately two container invocations:

1. **Trusted parse/extract:** `provenance-validate` runs the baked project-owned validator. It strictly
   parses ustar and JSON, validates schema/JCS/duplicates, verifies identity/digest/size/ELF metadata,
   proves required libraries resolve in the image, and extracts exactly one binary to
   `/work/validated/app-cli`. The host then verifies the output directory contains only that regular,
   non-linked file with the expected mode, size, and digest.
2. **Untrusted execution:** `binary-smoke` starts a new hardened container with only the verified
   binary mounted read-only and tmpfs. It runs `--help` with no network or credentials and bounded
   memory, pids, and wall time.

The untrusted app binary never shares a writable mount with the parser/extractor. A successful action
outputs the host path, digest, size, and mode of the verified binary within the invoking action's
private workspace.

The validator, schema, malformed fixtures, valid golden archive, and all required capabilities must
exist and pass before any image digest can be published. Golden tests cover JCS, duplicate keys,
schema rejection, ustar-only parsing, traversal/link/special-file rejection, normalized headers, size
limits, ELF inspection, dependency resolution, exact extraction, and output-directory confinement.

## 7. Reusable workflow and action contract

### 7.1 Reusable workflow

Inputs:

- `app-repository`, `app-ref`, `app-repo-id`, `working-directory` (default `.`), `workspace-root`,
  `app-cli-package`, `app-cli-bin`, `app-cli-artifact`;
- `cache` (default `false`), `cache-key-suffix`, `disclosure-acknowledged`, and `timeout-minutes`
  (default 30), plus `app-env` (default `{}`);
- secret `app-checkout-token`.

`app-cli-artifact` must be unique among artifact uploads in the workflow run. It does not partition
the cache. The workflow has no provider inputs. Checkout persists no credentials.

Outputs are `artifact-name` plus every `CallerExpectedIdentity` field: `app-repo-id`,
`source-revision`, `app-cli-package`, `app-cli-bin`, and `workspace-id`. It does not output
`platform-id`, `container-ref`, or protocol.

The consumer job checks out the app itself and runs `compute-app-cli-identity` against that checkout
using `app-checkout-token`. It compares every computed caller identity field with the reusable
workflow output before validation. Each later action receives `CallerExpectedIdentity` and derives
`PlatformIdentity` from its local action revision.

Matrix callers use unique artifact names and compare identity per leg. Shared workflow outputs are
not used to aggregate matrix results.

### 7.2 Provider actions

Every provider action accepts `app-cli-artifact` and `CallerExpectedIdentity`, derives local
`PlatformIdentity`, downloads exactly the named artifact into an action-private workspace, and runs
the full two-container validation sequence itself. Provider actions do not accept a caller-supplied
host binary path. Before each subsequent container launch, the action rechecks the validated path is
the same confined regular file with the recorded digest, size, mode, and single link. The private
workspace is removed with `if: always()`.

Every provider action also accepts the validated `app-env` JSON object (default `{}`); no provider
action inherits ambient application variables.

`deploy-fastly` additionally accepts `app-env` and `generated-output-paths`. It reuses one validated
binary for its `active-version`, optional credential-free `app-build`, and provider deploy operations
within that invocation. `active-version-fastly` is also a source-free action with inputs
`app-cli-artifact`, `CallerExpectedIdentity`, `fastly-service-id`, and `fastly-api-token`; it outputs
`version`, where an empty value is success only for a confirmed first production deploy.

`validate-app-cli-provenance`, `deploy-fastly`, `active-version-fastly`, `healthcheck-fastly`,
`rollback-fastly`, and `config-push-fastly` all apply this handoff. An identity or path mismatch fails
before app code or provider mutation.

`config-push-fastly` validates and confines the selected repository/manifest/config file and derives
the exact named app-config environment overlay before container launch. Inline config is written to
one fresh host file and mounted read-only. `no-env` exposes no app-config overlay.

Mutation actions publish `mutation-attempted` host-side before launching the mutating CLI. Named
containers receive bounded signal forwarding and post-cancellation reconciliation as specified by the
parent deploy contract.

## 8. Image publication and compatibility

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

The release has two revisions:

- `S` is the full source commit used to build the image. The image has OCI label
  `org.opencontainers.image.revision=S` and a protocol label matching the baked validator.
- `B` is the baseline revision created after the pin PR commits the verified digest and `S` to
  `image.json` and permanent pin CI is enabled.
- `P` is the later, fully tested action revision that contains the unchanged reviewed pin plus the
  cache, provenance, launcher, and consumer implementation. Consumers pin all EdgeZero
  workflow/action references to full SHA `P`.

There is no literal same-commit requirement between image source and pin record. Compatibility is
enforced by digest, image labels, and exact `provenance-protocol`. Changing the validator/archive
contract requires a protocol bump and a new image before the actions using that protocol are pinned.

Publication order is:

1. Land source revision `S`, including validator, schema, fixtures, `.dockerignore`, Dockerfile,
   publisher, local-image CI, pin-change CI, and publication tests.
2. Build from repository root, push by protected release tag, and capture digest `D` from BuildKit's
   metadata output.
3. Verify `D` is a leaf linux/amd64 image, labels identify `S` and protocol, exact tool versions and
   target are installed, validator capability tests pass, and runtime works read-only/non-root.
4. Ensure the GHCR package is public, then prove an anonymous pull and smoke by `D`. The first release
   stops here until an operator changes package visibility and reruns the same tag.
5. Open or update an idempotent PR committing `image.json = {D, S, protocol}`. Required pin CI
   re-verifies the image before merge; merging the passing PR creates baseline `B`.
6. Implement the remaining plans on top of `B`, run the full pin, actionlint, zizmor, schema,
   fixture, container, and contract suites, and designate the passing full commit SHA as `P`.

Source `S` also contains a required CI job that, for every add/change/delete of `image.json`, requires
the file to exist, validates its structure, anonymously pulls its exact digest, and runs the complete
published-image verifier before merge. Thus no later syntactically valid pin can bypass image,
platform, label, protocol, public-access, target, validator, or exact-version checks.

The release tag and environment are protected external prerequisites. The workflow also verifies `S`
is an ancestor of the protected default branch. All publication and pin-record mutation is serialized
under one repository-global concurrency group with `cancel-in-progress: false`; different release
tags cannot race the single `image.json`. Pin branches remain source/digest-derived and idempotent.
The publisher checks out without persisted credentials, proves `HEAD == S` and the recursive checkout
is clean immediately before the repository-root build, and excludes `.git`, build outputs, and local
detritus through the reviewed root `.dockerignore`.

Pin branches and PRs use a short-lived, protected-environment GitHub App installation token scoped to
repository contents and pull requests. They do not use `GITHUB_TOKEN`: its push does not trigger push
workflows, and checks on its automation-created PR require manual approval, so it cannot guarantee the
automatic required-check path. The branch updater records the remote OID and uses an explicit
force-with-lease; ambiguous, closed, superseded, and already-merged PR states follow the fixture-tested
fail-closed state machine in the implementation plan. The App token is minted only after build and
anonymous image verification, so it cannot enter the repository-root build context.

## 9. Testing

Required automated coverage includes:

- cold, warm, corrupt-restore, stop-failure, write-error, audit-failure, and save-warning cache paths;
- fixed host path restoration, cross-host-checkout-path hits, nested workspace and sibling path deps,
  public Git dependencies, concurrent generations, seven-day expiry as documented behavior, and no
  compiler retry;
- cache audit type/owner/path/layout/size checks and arbitrary app-written regular data disclosure;
- full source inventory, deleted/modified tracked paths, gitlinks, escaping symlinks, overlapping or
  tracked-containing output roots, caller-declared generated output, undeclared output rejection,
  source-free lifecycle bypass of Copy B checks, and unchanged original checkout;
- every environment and mount profile, including token absence, production healthcheck tokenlessness,
  staging token presence, credential-free `app-build`, explicit `app-env` allow/deny behavior,
  config-push repo/config confinement, and deploy-without-sccache;
- strict caller identity, full-SHA app/workflow/action refs, locally derived platform identity, matrix
  artifacts, and consumer recomputation for private repositories;
- all provenance golden/malformed fixtures, provider actions independently validating named
  artifacts and rechecking the binary handoff, and the split parse/extract versus binary-smoke boundary;
- exact Rust/Fastly/sccache versions, installed wasm target plus a minimal wasm compile, image labels,
  leaf-manifest platform checks, anonymous pulls, and release rerun/idempotency;
- production/staging deploy, active-version, healthcheck, rollback, config push, mutation signaling,
  cancellation, and the exclusive `--staging` spelling.

Warm reuse is asserted by zeroing and comparing sccache statistics. Dependency fetching remains
online because source archives are not cached. Wall-clock improvement is telemetry, not a pass/fail
condition.

## 10. Rollout and migration

Before implementation is published:

1. Migrate every existing non-local external action and reusable workflow reference in the repository
   to a reviewed full 40-hex commit SHA and change the repository-wide pin gate accordingly.
2. Land the validator/schema/fixture capability set before the container publication tasks.
3. Publish and anonymously verify the image, then commit the pin and permanent gate as baseline `B`.
4. Land reusable workflow, cache, provenance, launcher, and consumer integration, then designate the
   passing final action revision as `P`.
5. Update the parent spec, implementation plan, adoption guide, and public guide together. Remove
   direct-composite producer guidance; document the two-job producer/consumer topology, explicit
   `app-env` migration from ambient workflow environment, and `generated-output-paths` for
   repository-writing credential-free app builds.

Caching remains off by default. Container execution and provenance validation are unconditional.

## 11. Out of scope

- Detecting sccache staleness from undeclared proc-macro or `build.rs` inputs.
- Authenticating the artifact producer or proving workflow-bound attestation.
- Caching dependency source archives, private dependency credentials, native-tool sccache wrapping,
  self-hosted runners, alternate toolchains, non-default feature sets, or non-Fastly adapters.
- Cache lineage merging, family-local eviction, or action-managed cache deletion.

## 12. History

- **v6.17:** introduced the build-only reusable workflow, consumer deployment job, deploy-compile
  profile, full working-copy verification, `job.check_run_id`, and explicit undeclared-input risk.
- **v6.18:** split trusted provenance extraction from untrusted binary execution; completed provider
  mount/environment profiles; strengthened full-inventory source verification; made cache family and
  warning-only saves coherent; corrected sccache error/size/audit contracts; made platform identity
  action-derived; replaced impossible same-SHA publication with image source `S`, pin baseline `B`,
  and final action revision `P`; made full-SHA external references normative; and made validator
  capability fixtures a hard publication prerequisite.

## 13. Deferred implementation mechanics

Implementation plans may choose helper names and internal module boundaries. They must commit exact
schema files, golden bytes, malformed fixtures, sccache v0.10 layout/stats fixtures, exact
tar/compression archive-bound and entry-count vectors, provider environment name allowlists, release
SHAs/checksums, and command-level tests before publication.
Those are mechanics, not permission to weaken the contracts above.
