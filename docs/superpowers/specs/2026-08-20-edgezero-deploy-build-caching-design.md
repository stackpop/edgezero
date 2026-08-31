# EdgeZero Deploy Actions - Build Caching Spec

**Status:** Design (proposed) - v6.19

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

| In-container path           | Mode            | Allowed operations                         | Source                                          |
| --------------------------- | --------------- | ------------------------------------------ | ----------------------------------------------- |
| `/work/repo`                | writable        | cached-compile, app-build, provider-deploy | Copy A or Copy B                                |
| `/work/repo`                | read-only       | config-push                                | frozen original checkout                        |
| `/work/target`              | writable        | cached-compile, app-build, provider-deploy | fresh or parent target cache as specified below |
| `/work/cargo-home`          | writable, fresh | cached-compile, app-build, provider-deploy | operation-specific directory                    |
| `/work/sccache`             | writable        | cached-compile only                        | stable host cache directory                     |
| `/work/input/app-cli`       | read-only       | provenance-package only                    | exact binary produced by cached-compile         |
| `/work/input/artifact.tar`  | read-only       | provenance-validate only                   | downloaded artifact                             |
| `/work/input/expected.json` | read-only       | provenance-package, provenance-validate    | host-generated expected identity                |
| `/work/packaged`            | writable, fresh | provenance-package only                    | empty host archive-output directory             |
| `/work/validated`           | writable, fresh | provenance-validate only                   | empty host output directory                     |
| `/work/bin/app-cli`         | read-only       | binary-smoke and provider operations       | validated binary                                |
| `/work/config/inline.toml`  | read-only       | config-push only                           | optional action-owned inline config file        |
| `/work/package`             | writable, fresh | app-build, provider-deploy                 | staged Fastly package/output                    |
| `/work/home`, `/work/tmp`   | writable tmpfs  | all operations                             | operation-local tmpfs                           |

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
- `provenance-package`: trusted baked validator, the exact compiled binary read-only at
  `/work/input/app-cli`, read-only expected-identity JSON, fresh writable `/work/packaged`, and tmpfs;
  no repository, target, Cargo, package, cache, app-binary execution, network, or token.
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
capture, mutation signaling, healthcheck ordering, and recovery. This addendum expressly supersedes
the parent's app-CLI metadata shape, caller override, archive member naming/ordering, system-tar
packaging/extraction, and artifact-validation rules, in addition to changing isolation and mounting.
Every staged CLI invocation uses `--staging`, never `--stage`.

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
- provenance packaging, provenance validation, and binary smoke: `PATH`, `HOME`, `TMPDIR` only.
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
  equal the validated Cargo package and target names. Before the host mounts the compiled file at the
  fixed `/work/input/app-cli` path, it requires the source basename to equal `app-cli-bin`.
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
  no slash, backslash, NUL, or control character.

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

Program headers are the sole loader-visible authority. ELF and program-header sizes, counts, offsets,
virtual-address mappings, additions, and multiplications are checked before access. Section headers
may be absent and never affect validation; conflicting section data is ignored because the runtime
loader does not use it for this contract. A static primary has no `PT_DYNAMIC`. Every dynamic primary,
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

Thus the only object-acquisition mechanisms in Protocol 1 are the primary's exact `PT_INTERP` and
recursively traversed `DT_NEEDED` entries; environment-driven preloads and runtime `dlopen` remain
outside the credential-free smoke contract. `DT_NEEDED` values containing `/` or `\\` fail. The
validator preserves duplicate direct `DT_NEEDED` values for metadata, sorts them bytewise, and
resolves dependencies recursively against this fixed directory list:

1. `/lib/x86_64-linux-gnu`
2. `/usr/lib/x86_64-linux-gnu`
3. `/lib64`
4. `/usr/lib64`
5. `/lib`
6. `/usr/lib`

For each `DT_NEEDED` basename, inspect `root + directory + basename` in the listed order but do not
silently choose a first match. A nonexistent path is skipped. A present path that is dangling,
escaping, or non-regular fails immediately. Every accepted candidate must canonicalize inside the
immutable image root and beneath one of the six roots. Zero candidates fails. Multiple candidates are
accepted only when `stat` reports the same device and inode; symlink or hardlink aliases to that same
file are one identity, while two different files are ambiguous and fail. The listed order controls
deterministic traversal and diagnostics, not precedence.

The exact interpreter path is resolved with the same confinement and regular-file rules, parsed as an
`ET_DYN` runtime dependency with no `PT_INTERP`, and recursively validated; it is not added to the
primary's `abi.needed`. Recursive inspection uses a device/inode visited set so hardlink aliases and
dependency cycles terminate, and every transitive library satisfies this same profile. The validator
does not read `ld.so.cache`, invoke `ldd` or the loader, or emulate `$ORIGIN`.

### 6.5 Protocol-owner CLI

The synchronous `edgezero-provenance-validator` binary owns both encoding and validation. It has no
Tokio dependency and never executes an app binary. Its stable credential-free interface is:

```text
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
container. Every input and output parent must canonicalize beneath it, except the trusted baked schema
path. `package` validates canonical expected identity, inspects and resolves the source ELF inside the
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

The untrusted app binary never shares a writable mount with the parser/extractor. A successful action
outputs the host path, digest, size, and mode of the verified binary within the invoking action's
private workspace.

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

Protocol 1 selects the official `rust:1.95.0-slim-bookworm` image and pins its `linux/amd64` leaf
manifest, not its multi-platform index. The digest resolved from the official registry on 2026-08-31
is `sha256:6f9e63259f12e1e599296f5ecfed2bae46de4af0ee0525dd8b89c046e236d5c5`; implementation must
re-resolve and compare it immediately before committing the Dockerfile. The exact sccache asset is
`sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz` from the upstream v0.10.0 release, with upstream
checksum `1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b`. The v0.10.0 release has no
GNU Linux client asset; the static musl client is the reviewed Linux x86-64 artifact. Changing either
base digest or tool asset requires a new source revision and image digest.

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

The rollout has three relevant revisions:

- `S` is the full source commit used to build the image. The image has OCI label
  `org.opencontainers.image.revision=S`,
  `org.opencontainers.image.source=https://github.com/stackpop/edgezero`, and a protocol label
  matching the baked validator. The final image overrides inherited source/revision labels, and
  verification requires all three exact values.
- `B` is the baseline revision created after the pin PR commits the verified digest and `S` to
  `image.json` and permanent pin CI is enabled.
- `P` is the later, fully tested action revision that contains the unchanged reviewed pin plus the
  cache, provenance, launcher, and consumer implementation. Consumers pin all EdgeZero
  workflow/action references to full SHA `P`.

There is no literal same-commit requirement between image source and pin record. Compatibility is
enforced by digest, image labels, and exact `provenance-protocol`. Changing the validator/archive
contract requires a protocol bump and a new image before the actions using that protocol are pinned.

Publication order is:

1. Before merging the source candidate, configure and verify the protected release environment,
   protected tag rule, dedicated GitHub App, repository permissions, and the two branch required
   checks after their names have materialized on the candidate PR. The first package may not exist
   yet; its public-visibility gate occurs after its first push and before a pin PR.
2. Land source revision `S`, including validator, schema, fixtures, `.dockerignore`, Dockerfile,
   publisher, always-running container CI, pin-change CI, and publication tests.
3. Build from repository root, push by protected release tag, and capture digest `D` from BuildKit's
   metadata output.
4. Verify `D` is a leaf linux/amd64 image, labels identify `S` and protocol, exact tool versions and
   target are installed, validator capability tests pass, and runtime works read-only/non-root.
5. Ensure the GHCR package is public and linked to `stackpop/edgezero`, then prove an anonymous pull
   and smoke by `D`. The first release stops here until an operator changes package visibility and
   reruns the same tag.
6. Open or update an idempotent PR committing `image.json = {D, S, protocol}`. Required pin CI
   re-verifies the image before merge; merging the passing PR creates baseline `B`.
7. Implement the remaining plans on top of `B`, run the full pin, actionlint, zizmor, schema,
   fixture, container, and contract suites, and designate the passing full commit SHA as `P`.

Source `S` contains a separate `.github/workflows/build-container-ci.yml` triggered for pull-request
types `opened`, `synchronize`, `reopened`, and `labeled`, every merge-queue `merge_group`, and every
push to the protected default branch, with no workflow-level path filter. It exposes two stable
required job names on every candidate:

- `build-container-local` computes the documented image-input path set. It builds and smokes the local
  image when relevant and otherwise runs an explicit successful not-applicable step.
- `build-container-pin` detects every add, change, or deletion of `image.json`. When relevant it
  requires the file to exist, validates its structure, anonymously pulls the exact digest, and runs
  the complete published-image verifier; otherwise it explicitly succeeds as not applicable.

Each job performs its own fail-closed change classification from the checked-out base and head so a
failed shared classifier cannot skip a required job. The local-image set includes `.cargo/**`, both
possible root `rust-toolchain` filenames, and every other Docker build or verifier input listed in the
implementation plan. Classification output is exactly one line, `relevant=true` or `relevant=false`.
An unconditional terminal assertion rejects missing, duplicate, or malformed output and proves
exactly one of the relevant or not-applicable branches ran; an invalid classifier can never make both
conditional paths disappear behind a green job. Contract tests pin pull-request, merge-group, and
push ranges, event triggers, job names, path set, deletion handling, output validation, and explicit
no-op behavior. The existing path-filtered
`deploy-action.yml` remains separate. Thus required checks always materialize without running Docker
on unrelated changes, and no later syntactically valid pin can bypass image, platform, label,
protocol, public-access, target, validator, or exact-version checks.

The same workflow also exposes non-required job `build-container-release-preflight` only for a
same-repository pull request carrying maintainer-applied label `build-container-release-candidate`.
That job uses environment `{name: build-container-release, deployment: false}`, performs no checkout,
and runs no repository script. After the environment reviewer approves it, the pinned token action
consumes the exact stored
App variable and private-key secret with repository `edgezero` and explicit `contents:write` and
`pull_requests:write`. The job requires its installation-ID output to equal the stored expected ID,
reads only `stackpop/edgezero` with the token, and lets the action's mandatory post step revoke the
token. The environment reviewer must inspect the candidate workflow diff before approval. A successful
check run from the GitHub Actions App proves the protected environment's stored credential, rather
than only an operator's local copy, can mint the publisher's exact token before `S`.

The final environment policy is tag-only, so the smoke uses a bounded transition. An administrator
temporarily adds one custom branch deployment policy equal to literal
`refs/pull/<candidate-pr>/merge`, runs the labeled job, then removes that branch policy without
changing the App variables or secret. The final preflight requires the environment to be back to its
sole `build-container-v*` tag policy and the successful workflow run to identify the exact candidate
PR, `stackpop/edgezero` head repository, and current PR head SHA. Every App variable/secret
`updated_at` value is no later than that run's completion time. Any new candidate commit or credential
update invalidates the smoke and requires the bounded transition again. The temporary branch policy
is a literal PR merge ref, never a wildcard or fork branch.

Repository-administrator bypass of environment protection is disabled. GitHub's documented REST
environment representation does not expose that switch, so neither the helper nor its fake-API tests
claim to verify it automatically. Before the credential smoke, an independent maintainer who is not
the preflight verifier opens the repository's `build-container-release` environment settings and
captures a PNG showing the repository, environment name, and disabled administrator-bypass control.
The verifier supplies that file plus the reviewer's login and RFC 3339 review time to the preflight.
The helper rejects a non-PNG file, a reviewer equal to the verifier, a future review time, or evidence
whose recorded candidate head SHA differs from the current PR head; it records that SHA, the literal
basename, and `sha256:<64-lowercase-hex>` file digest under `environment.administrator-bypass` with
`allowed:false`, `verification:"manual-ui"`, `reviewer`, and `reviewed-at`. A separately
authenticated operator attaches the byte-identical PNG with the canonical evidence and digest to the
candidate PR. Release checkpoint 1 requires a maintainer other than the verifier and recorded reviewer
to recompute the attachment digest and confirm the screenshot visibly proves the disabled setting.
Any environment-policy change or new candidate commit invalidates this manual evidence.

Before designating or tagging `S`, an operator runs the repository-owned preflight with a
read-administrative GitHub token, a separate package-audit token, the candidate PR number, the
expected App and installation IDs, and the App private key from a local file. The package-audit token
is a classic personal access token belonging to an active `stackpop` organization owner. Its granted
normalized OAuth-scope set is exactly `{read:org,read:packages}`; the helper verifies the
authenticated login, active owner membership, and returned `X-OAuth-Scopes` header before using that
same token for every package query. Neither local token is stored in GitHub Actions. They are supplied
only as `EDGEZERO_RELEASE_REPOSITORY_ADMIN_TOKEN` and
`EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN`, respectively. The helper rejects byte-equal token values
before making an API request and never logs either value. It never receives a PR-write token and never
mutates repository settings, packages, or comments. It requires all of the following and emits
canonical evidence for a separately authenticated operator to attach to the candidate PR:

- environment `build-container-release` has administrator bypass disabled, a nonempty
  `required_reviewers` rule with `prevent_self_review=true`, uses custom deployment policies, and has
  exactly one deployment policy, type `tag`, with name `build-container-v*`; its separately supplied
  administrator-bypass evidence satisfies the manual contract above;
- an active repository tag ruleset targets `build-container-v*` and restricts tag creation, update,
  and deletion. Its only bypass actor is team `edgezero-build-container-releasers`, with the numeric ID
  stored in `EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID` and bypass mode `always`; the verifier actor is
  an active member of that team. An active default-branch ruleset requires the stable check names
  `build-container-local` and `build-container-pin`. Each required-status-check entry has a non-null
  `integration_id` equal to the single GitHub Actions App ID observed on the candidate's successful
  check runs for those names; matching names from another integration do not satisfy the rule;
- protected-environment variables `EDGEZERO_BUILD_CONTAINER_APP_ID` and
  `EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID` equal the reviewed numeric IDs, environment variable
  `EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID` equals the ruleset's reviewed team ID, and secret metadata
  includes `EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY` without exposing its value. Their `updated_at`
  values are no later than the successful credential-smoke completion time;
- an App JWT made from that key identifies the expected dedicated App; the expected installation is
  active on account `stackpop`, uses selected repositories, grants exactly `contents:write`,
  `pull_requests:write`, and implicit `metadata:read`, and its repository list is exactly
  `stackpop/edgezero`;
- an installation token can be minted for only the EdgeZero repository ID with explicit
  `contents:write` and `pull_requests:write`, its response reports only those requested permissions
  plus implicit metadata read, it can read `stackpop/edgezero`, and it is revoked before the helper
  exits; and
- the candidate's `build-container-release-preflight` check run completed successfully, came from the
  same GitHub Actions App integration, and belongs to a workflow run whose pull request, head
  repository, and head SHA equal the current candidate values. It records the expected installation ID
  without exposing a token; and
- repository/package identity and the absent-before-first-push or public-and-repository-linked package
  state are the exact release state expected by the invocation. Absence is established only by a
  successful, fully paginated organization-container-package listing made with the verified active
  organization owner's package-audit token and containing no exact name match; a listing made by any
  other identity, a GET 404, or an authorization failure is never absence.

API failure, pagination truncation, ambiguity, extra bypass actor, extra repository or write
permission, credential failure, or evidence-post failure blocks `S`. After the first push creates the
package, publication stops until an operator makes it public and confirms it is linked to
`stackpop/edgezero`. GHCR exposes no enforceable per-version retention lock, so this contract does not
claim one. Repository workflows contain no package-deletion endpoint or delete-scoped credential;
manual deletion by a package or organization administrator is an accepted operational risk that can
break existing digest-pinned consumers and requires an emergency rebuild plus new reviewed pin. The
workflow also verifies `S` is an ancestor of the protected default branch. All publication and
pin-record mutation is serialized
under one repository-global concurrency group with `cancel-in-progress: false`; different release
tags cannot race the single `image.json`. Pin branches remain source/digest-derived and idempotent.
The publisher has two jobs. `build-and-verify` does not reference the protected environment; it checks
out without persisted credentials, proves `HEAD == S` and the recursive checkout is clean immediately
before the repository-root build, pushes and anonymously verifies `D`, and exports only non-secret
`{S,D,protocol,tag}` job outputs. It excludes `.git`, build outputs, and local detritus through the
reviewed root `.dockerignore`. Only after that job succeeds does `update-pin` start with
`environment: build-container-release`. That job does no image build, receives the non-secret outputs,
checks out without persisted credentials, mints the scoped App token, and performs only the pin branch
and PR mutation. Thus the environment's private key is unavailable to the repository-root build job.

Pin branches and PRs use a short-lived, protected-environment GitHub App installation token requested
for repository `edgezero` with explicit `contents:write` and `pull_requests:write`. The publisher
requires the token action's installation-ID output to equal
`EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID` before use. They do not use `GITHUB_TOKEN`: its push
does not trigger push workflows, and checks on its automation-created PR require manual approval, so
it cannot guarantee the automatic required-check path. The branch updater records the remote OID and
uses an explicit force-with-lease; ambiguous, closed, superseded, and already-merged PR states follow
the fixture-tested fail-closed state machine in the implementation plan. The App token is minted only
after build and anonymous image verification, so it cannot enter the repository-root build context.

The administrator-bypass screenshot is repeated at the protected-secret boundary. For every workflow
run in which `update-pin` is eligible, including every same-tag rerun, its environment approver waits
for `build-and-verify` to succeed, opens the environment settings, and captures a fresh PNG before
approving `update-pin`. The record binds the screenshot digest, approver login, review time, workflow
run ID, exact source revision `S`, and release tag. The same login supplies the recorded environment
approval within 15 minutes of the review. The release operator attaches the record and byte-identical
PNG to the release evidence. A missed window, bypassed approval, run-ID mismatch, or known
environment-policy change invalidates the run and requires a new capture, approval, and workflow run.
The initial private-package stop does not carry evidence forward to its rerun. This per-attempt manual
check is required because the API-invisible setting cannot be proven current by the pre-`S` helper.

The repository's zizmor policy uses `hash-pin` for every non-local action. The structural pin scanner
remains authoritative for lowercase 40-hex refs, strict Docker `sha256` digests, exact scanned
surfaces, and the documentation-only EdgeZero placeholder.

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
- exact canonical metadata and expected JSON, schema versions, duplicate keys, byte-exact ustar
  headers/padding/end blocks, deterministic package output, every accepted/rejected dynamic string and
  object-acquisition tag, conservative ELF/loadability vectors, all provenance golden/malformed
  fixtures, provider actions independently validating named artifacts and rechecking the binary
  handoff, and the split parse/extract versus binary-smoke boundary;
- exact Rust/Fastly/sccache versions, installed wasm target plus a minimal wasm compile, image labels,
  leaf-manifest platform checks, anonymous pulls, always-materialized required container jobs,
  image-pin deletion, environment reviewer/self-review/deployment-policy checks, App
  installation/repository/permission/token-scope checks, and release rerun/idempotency;
- production/staging deploy, active-version, healthcheck, rollback, config push, mutation signaling,
  cancellation, and the exclusive `--staging` spelling.

Warm reuse is asserted by zeroing and comparing sccache statistics. Dependency fetching remains
online because source archives are not cached. Wall-clock improvement is telemetry, not a pass/fail
condition.

## 10. Rollout and migration

Before implementation is published:

1. Migrate every existing non-local external action and reusable workflow reference in the repository
   to a reviewed full 40-hex commit SHA, change the repository-wide pin gate accordingly, and set
   zizmor to `hash-pin`.
2. Implement the validator/schema/fixtures, container, publisher, and always-running container checks
   on one source-candidate PR; no image is published from that branch.
3. After the stable check names materialize on the candidate PR, configure and verify every external
   release prerequisite, require both checks, and merge the passing candidate as source `S`.
4. Publish and anonymously verify the image, then commit the pin and permanent gate as baseline `B`.
5. Land reusable workflow, cache, provenance, launcher, and consumer integration, then designate the
   passing final action revision as `P`.
6. Update the parent spec, implementation plan, adoption guide, and public guide together. Remove
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

## 13. Deferred implementation mechanics

Implementation plans may choose helper names and internal module boundaries. They must commit the
schema implementing Section 6.2, golden bytes and malformed fixtures for Sections 6.2 through 6.5,
sccache v0.10 layout/stats fixtures, exact cache tar/compression archive-bound and entry-count vectors,
provider environment name allowlists, release SHAs/checksums, and command-level tests before
publication.
Those are mechanics, not permission to weaken the contracts above.
