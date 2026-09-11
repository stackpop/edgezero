# Build Cache Implementation Evidence

Implementation branch: `feature/build-app-cli-cache` in the existing
`edgezero-actions-improve` worktree. The implementation recorded below follows design v6.29;
the subsequent v6.30, v6.31, v6.34, v6.35, v6.36, v6.37, v6.38, and v6.39 design reconciliations are recorded separately at
the end.

## Version Reference Review

On 2026-09-05, the upstream release API reported the following releases as neither
draft nor prerelease. Anonymous `git ls-remote` confirmed each tag and its peeled
commit, with no same-named branch. Executable YAML uses the version column, not
the recorded commit. Existing workflow major-version choices are preserved.

| Release                                                                                                                          | Resolved Commit                            |
| -------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| [actions/checkout v6.1.0](https://github.com/actions/checkout/releases/tag/v6.1.0)                                               | `d23441a48e516b6c34aea4fa41551a30e30af803` |
| [actions/checkout v7.0.1](https://github.com/actions/checkout/releases/tag/v7.0.1)                                               | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| [actions/cache v5.1.0](https://github.com/actions/cache/releases/tag/v5.1.0)                                                     | `caa296126883cff596d87d8935842f9db880ef25` |
| [actions/cache v6.1.0](https://github.com/actions/cache/releases/tag/v6.1.0)                                                     | `55cc8345863c7cc4c66a329aec7e433d2d1c52a9` |
| [actions/upload-artifact v7.0.1](https://github.com/actions/upload-artifact/releases/tag/v7.0.1)                                 | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` |
| [actions/download-artifact v8.0.1](https://github.com/actions/download-artifact/releases/tag/v8.0.1)                             | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` |
| [actions/setup-node v6.5.0](https://github.com/actions/setup-node/releases/tag/v6.5.0)                                           | `249970729cb0ef3589644e2896645e5dc5ba9c38` |
| [actions-rust-lang/setup-rust-toolchain v1.17.0](https://github.com/actions-rust-lang/setup-rust-toolchain/releases/tag/v1.17.0) | `166cdcfd11aee3cb47222f9ddb555ce30ddb9659` |
| [github/codeql-action v4.37.9](https://github.com/github/codeql-action/releases/tag/v4.37.9)                                     | `cdf488f595d80d6e07e03d4674febd5ab45fa938` |
| [actions/configure-pages v6.0.0](https://github.com/actions/configure-pages/releases/tag/v6.0.0)                                 | `45bfe0192ca1faeb007ade9deae92b16b8254a0d` |
| [actions/upload-pages-artifact v5.0.0](https://github.com/actions/upload-pages-artifact/releases/tag/v5.0.0)                     | `fc324d3547104276b827a68afc52ff2a11cc49c9` |
| [actions/deploy-pages v5.0.1](https://github.com/actions/deploy-pages/releases/tag/v5.0.1)                                       | `368f82528645a54fb793d4d04e342629a3f51346` |
| [actions/create-github-app-token v3.2.0](https://github.com/actions/create-github-app-token/releases/tag/v3.2.0)                 | `bcd2ba49218906704ab6c1aa796996da409d3eb1` |

The four actionlint 1.7.12 archive digests in plan 1 were independently compared
with the [upstream checksum file](https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_1.7.12_checksums.txt).
The macOS arm64 archive was downloaded, verified, and installed only under a
temporary tools directory for local verification.

## Hosted Prerequisites

Read-only inspection found no build-container environments or repository Actions
variables, no organization App installations, immutable releases disabled, and no
required build-container workflow or tag/pin rulesets. Main's existing ruleset
does not satisfy the reviewed two-approval, code-owner, last-push, and merge-queue
requirements.

The current credential cannot inspect organization rulesets/Actions policy or
GHCR package state with the scopes required by the design. These are not negative
proofs of feature availability or package absence. The dedicated audit credentials,
App, approvals, protected baseline `G`, source release `S`, and image pin baseline
`B` remain required before plans 2 through 5 can be executed. No release, ruleset,
credential, environment, or repository-policy mutation has been performed.

## Task 0 Verification

Implemented exact stable version references, structural YAML and Markdown gates,
offline state checks plus the hosted release-binding verifier, and the exact
actionlint 1.7.12 compatibility wrapper. The protected `G` checkout and required
workflow integration remain Task 2 work; today's candidate-owned workflow is not
a protected release approval authority.

Verification on 2026-09-05:

- Action suite: 301 passed, zero failed, four Linux-only tests skipped on macOS.
- Documentation contract suite: 12 passed, including inherited CI/event/token
  isolation, committed-snapshot selection, and cross-boundary rename rejection.
- Repository scanners: 40 external executable refs and 39 external documentation
  refs, bootstrap state.
- Pinned actionlint, ShellCheck, and repository-wide offline zizmor: passed.
- Workspace tests, fmt, all-feature Clippy, feature compilation, and Spin WASM
  compilation: passed. The workspace tests need localhost binding permission;
  sandbox-only execution failed those existing server tests and was rerun with
  that permission enabled.
- Documentation Prettier, ESLint, and VitePress production build: passed.
- Two independent read-only reviewers verified the flow-map/alias-key fixes,
  scanner transition and credential boundaries, and CI input coverage. No
  remaining reproduced Task 0 blocker was reported.

No hosted GitHub workflow result or Linux-only local test is claimed by this
record. The standalone protocol crate is a separate Task 1 tranche.

## JSON Protocol Tranche

Task 1 Section 5.1 now has the standalone crate, closed expected/metadata types,
typed constructors, exact canonical encoders, a Draft 2020-12 schema, and valid
and invalid wire fixtures. The root Cargo workspace and dependency graph are
unchanged. Production dependencies are exact serde 1.0.228 and serde_json 1.0.150;
schema conformance uses dev-only
[jsonschema 0.54.0](https://docs.rs/jsonschema/0.54.0/jsonschema/) with default
features disabled, including external HTTP/file resolution.

Fifteen colocated tests cover duplicate/escaped keys, unknown and missing fields,
required null, alternate byte encodings, malformed surrogates, field types and
bounds, schema constraints, container derivation, complete identity equality,
UTF-8 ordering, duplicate preservation, exact 64-KiB acceptance, and overflow.
Failing skeleton tests were observed before implementation. Full standalone tests,
fmt, and strict Clippy passed; both read-only reviewers reported no remaining
blocker for this tranche. CI now explicitly tests the standalone manifest with
the repository's Rust version.

Archive/extraction, ELF/loadability, the CLI/capability corpus, container gate and
publisher, image release/pin, and plans 2 through 5 are not implemented by this
tranche. No runnable or published caching release is claimed.

## Hosted Identity Blocker

Task 0 was pushed as `b3589741814a76d5598ff17aab42f2cff0f67811` to
[PR 347](https://github.com/stackpop/edgezero/pull/347).
Its [static-checks job](https://github.com/stackpop/edgezero/actions/runs/34013647236/job/101433619629)
failed at the strict synthetic-merge parent assertion. GitHub's PR and run APIs
reported base `37f1a137bc856358bb4d69c499b61129d29c192e`, while the current main ref
and synthetic merge `fd99c1e1a73d211be84d4077decbbb80441a5e59` use first parent
`593fc9282a1c56e12bae15f91eef2162f4b6a1b7`; its second parent is the exact pushed
head. The scanner now reports expected and observed parent identities on failure.
It still enforces design Section 8's literal event-base contract. No branch merge,
rebase, gate bypass, or event-selection design amendment has been performed.

The user has been asked to choose a branch refresh or a reviewed event-selection
amendment, and to identify the maintainer who will provision release prerequisites
and independent approvals. These hosted checkpoints remain open.

## v6.30 Design Reconciliation

The reviewed amendment selects the authenticated synthetic first parent for PR comparisons, retaining
payload-base ancestry and exact event/head identity checks. Plan 1 explicitly schedules the existing
scanner's implementation and hosted regression follow-up before `G` is frozen. The executable
selector has not been changed by this documentation correction, so the known hosted failure is not
claimed fixed.

The existing caching design and all five existing plans have been reconciled for Bookworm ELF and
loader aliases, post-`B` gate rotation, credential scope/freshness, publisher attempt binding, artifact
retention, and App-authenticated migration. The original deployment spec/adoption companion now live
under `docs/superpowers/specs/`; the original deployment plan is under `docs/superpowers/plans/`.
References and the scanner's four-document bootstrap allowlist follow those moves without widening
the placeholder exception to other internal documents. No new image, gate activation, source tag,
action release, or completed caching implementation is asserted by these edits.

Follow-up review also made rotation policy observations explicitly local-auditor/reviewer evidence,
normalized numeric REST attempt fields before string comparisons, and required publishers to verify
the latest evidence-bound rotation attempt succeeded. The rotation receipt precedes lock completion;
the final publisher prerequisite record follows it, avoiding a circular approval prerequisite.

Local verification for the documentation correction and relocation:

- Documentation scanner: 39 external references, bootstrap state; 13 scanner tests passed.
- Action contract suite: 301 passed, zero failed, four Linux-only checks skipped on macOS.
- Docs formatting, ESLint, VitePress build, and the pinned actionlint workflow check passed.
- Workspace tests, formatting, all-feature Clippy, the combined Fastly/Cloudflare/Spin check, and
  the Spin `wasm32-wasip2` check passed. The network-fetching generated-workspace test stays ignored.

These are local regression checks, not hosted rotation, credential, image, or release evidence.

## v6.31 Design Reconciliation

The reviewed correction defines the publisher-prerequisite repository variable, its canonical inert
and source-bound records, the separately credentialed single-variable local writer, and the exact
post-rotation/post-merge lifecycle. It replaces numeric run-id ordering with unique latest creation
time, closes first-shell startup variables, fixes the parent target-cache path/lifecycle and metadata
execution profile, completes the gate manifest, and assigns the missing cache/disclosure/retention
tests. The metadata profile uses an identity-only consumer copy when no action-local Copy A or Copy B
exists; it never mounts the authority. The fixed parent target root is reserved for cache-enabled
builds; cache-disabled builds use and clean a fresh invocation-private target. The exact-version
`uses:` policy is unchanged.

This record does not claim those future publisher, rotation, cache, metadata, or hosted qualification
tasks are implemented. Task 0 selector, whole-document placeholder, and hosted API-contract fixes are
tracked separately. Their scanner implementation confines subject Git and hosted release-verifier
subprocesses to the v6.31 allowlisted environments. The selector and placeholder paths pass local and
hosted contract suites; the transition verifier passes its adversarial fixtures in hosted CI. The real
immutable-release transition remains a separate consumer-plan Section 9 checkpoint at documentation
revision `R`, after stable release `V` exists.

Local v6.31 reconciliation verification on 2026-09-07:

- The focused documentation-gate suite passed 14 tests, including whole-document placeholders,
  advanced-base synthetic merges, unavailable/shallow/replaced/grafted history, exact API response
  metadata, and poisoned curl/Git child environments. The repository scanner passed with 39 external
  references in bootstrap state.
- The complete deploy-core action suite passed 302 tests with zero failures and five platform-specific
  skips on macOS. Pinned actionlint 1.7.12 passed through the repository compatibility wrapper; zizmor
  1.16.3 reported no findings under the committed suppression policy.
- Docs Prettier, ESLint, and VitePress build passed. Workspace tests, Rust formatting, all-target/all-
  feature Clippy with warnings denied, the combined Fastly/Cloudflare/Spin check, and the Spin
  `wasm32-wasip2` check passed. The standalone provenance validator passed 15 tests, formatting, and
  strict Clippy.
- Independent design/plan, implementation-compliance, and code-quality reviews reported no remaining
  local issue. PR 347's hosted `static-checks` job passed 319 action-contract tests, including the
  reproduced advanced-base selector fixture and release-verifier adversarial fixtures. Task 0 is
  complete. The real hosted release API/ref proof remains required at documentation revision `R`; it
  cannot run in bootstrap because the design requires the release record to remain absent through `P`.

## Archive/Extraction Protocol Tranche

Task 1 Section 5.2 now has the sole protocol-1 archive encoder/parser and atomic binary extractor.
The encoder emits the exact two-member deterministic ustar form from design Section 6.3. The parser
rejects noncanonical headers, alternate numeric encodings, extensions and non-regular members,
renamed/duplicate/extra/out-of-order members, malformed sizes and checksums, nonzero padding,
incorrect end blocks, trailing bytes, and checked-arithmetic or seek failures. Metadata allocation is
bounded to 64 KiB; binary payloads are skipped and extracted through bounded 8-KiB I/O rather than
loaded into memory.

The committed archive corpus includes one byte-exact golden archive and malformed category fixtures.
Review corrected the embedded-NUL vector so it preserves the canonical member name before post-NUL
garbage. Because the eleven-octal-digit protocol field cannot encode a `u64` overflow, the former
misnamed overflow tar is now the maximum-size vector and a synthetic `Read + Seek` fixture exercises
near-`u64::MAX` offset overflow directly. Separate bounded readers prove zero/oversized metadata and
aggregate-size failures occur before payload I/O.

The extractor requires a fresh canonical empty output parent, writes one create-new temporary sibling,
flushes and verifies it, and publishes with no replacement. Linux uses the required
`renameat2(RENAME_NOREPLACE)` path; the macOS test path uses an atomic no-replace hard-link publication
followed by source removal. Checked cleanup covers pre- and post-publication handled failures, reports
cleanup failure without hiding the primary failure, and preserves independently created collision
sentinels. Success requires one regular mode-0755, link-count-one `app-cli`. The output parent remains
the invocation-private trusted directory from design Sections 6.5 and 6.6; no stronger concurrent
same-uid adversary is claimed.

TDD evidence was recorded before implementation and during review hardening: the initial focused suite
failed on the unimplemented encoder/parser/extractor; bounded-I/O, temporary-ownership, fixture
semantics, canonical-path, and exact-seek tests each failed for their intended missing behavior before
the corresponding production or fixture correction. Final local verification on 2026-09-08:

- Focused archive/extraction tests: 23 passed, zero failed.
- Complete standalone validator tests: 38 passed, zero failed; formatting and all-target/all-feature
  Clippy with warnings denied passed.
- The standalone validator compiled for `x86_64-unknown-linux-gnu`; repository workspace tests,
  formatting, all-target/all-feature Clippy, and the combined Fastly/Cloudflare check passed.
- Two-stage read-only review approved Section 5.2 specification compliance and code quality after
  checked cleanup, no-replace collision, inclusive-boundary, canonical-parent, and adversarial seek
  coverage was added.

The Linux publication branch is compile-checked locally and is exercised by hosted Linux tests after
this tranche is pushed; this local record does not claim a macOS runtime executed `renameat2`.
ELF/loadability, CLI/capability integration, image publication, and the later plans remain open.

## ELF/Loadability Protocol Tranche

Task 1 Section 5.3 now implements the protocol-1 ELF parser and recursive startup-closure resolver.
It accepts only ELF64 little-endian x86-64 objects with the exact header, program-header, object-role,
interpreter, dynamic-table, string-table, tag, flag, SONAME, and dependency-name profile in design
Section 6.4. Program-header ranges and virtual-to-file mappings use checked arithmetic and bounded
8-KiB reads. Ambiguous, partial, unreadable, non-file-backed, or contradictory mappings fail closed.

Dynamic dependency resolution is confined to `/opt/edgezero/runtime-lib`, except that the fixed loader
basename resolves to the already validated `/lib64/ld-linux-x86-64.so.2`. The resolver validates every
flat-directory object and alias, rejects symlinks, hard links, subdirectories, reserved names, missing
closure members, mixed architectures, and `/etc/ld.so.preload`, and terminates cycles by device/inode.
Direct `DT_NEEDED` values remain duplicate-preserving and are byte-sorted only for metadata. SHA-256
and size are measured from the opened primary file. The structural launch contract fixes the dynamic
loader options and records that the claim covers startup only, not later `dlopen` or child processes.

TDD evidence was recorded before implementation and during review hardening. The initial focused
suite failed because the ELF API and SHA-256 dependency did not exist. Subsequent focused failures
proved missing preload rejection, malformed `PT_LOAD` memory bounds, partial mapping ambiguity, and
unbounded repeatable `DT_NEEDED` retention before those behaviors were implemented. A metadata-
impossible dependency count now fails after at most 21,846 entries, and dynamic entries are scanned in
bounded chunks rather than with one seek per entry. Characterization tests cover existing rejection
paths that required no production change. Final local verification on 2026-09-08:

- Focused ELF/loadability tests: 32 passed, zero failed.
- Complete standalone validator tests: 70 passed, zero failed; formatting and all-target/all-feature
  Clippy with warnings denied passed.
- Repository workspace tests passed after the production changes; `git diff --check` passed.
- Independent specification-compliance and code-quality reviews approved the tranche after preload,
  mapping-overlap, closed-tag boundary, empty-dynamic-table, and denial-of-service coverage was added.

The real GNU-target CLI, pinned Bookworm loader/libc closure, and application-directed `dlopen` runtime
fixture remain the explicit Task 2 image-verification gate. Section 5.3 does not invoke `ldd`, a loader,
or an inspected artifact. CLI/capability integration, image publication, and the later plans remain open.

## CLI/Capability Protocol Tranche

Task 1 Section 5.4 now provides the synchronous provenance-validator executable with
`write-expected`, `write-release-request`, `package`, `validate`, and `self-test` commands. Typed
arguments enforce protocol `1`, the canonical release tag, fixed release-request path, work-root
confinement, and create-new/no-replace publication. The package and validation paths compose the
reviewed JSON, archive, and ELF protocols; validation publishes mode-0755 output without executing
the application binary.

The self-test uses a compiled, bounded manifest of exact fixture paths, SHA-256 values, and expected
outcomes. Its valid golden archive is a coherent package containing a static x86-64 ELF and canonical
metadata, not merely a syntactically valid tar. The fixture identities are:

- `app-cli`: 2,048 bytes,
  `6b25433eed518a44b19e8c821749f4dd07156d0962727ed27c15f8816c3c1c96`.
- `static-meta.json`: 710 bytes,
  `8ad2590f982dacf2272e8afb1f88b9e910349f43aab38675bac9cd43eb2e47d2`.
- `archive.tar`: 5,120 bytes,
  `bb041c59d4c24ecc41f3f329040aef224a8afd5477ee515b59c6ae94d5c258f1`.

TDD evidence was observed before implementation and during review hardening. The initial process
suite failed because the binary and command API did not exist. Later focused failures demonstrated
missing canonical-root and traversal checks, publication-identity checks, staged-archive rehashing,
integrated archive tamper rejection, coherent golden-fixture equality, and bounded collision-watcher
shutdown before those behaviors were added. Final local verification on 2026-09-08:

- Complete standalone validator tests: 76 unit and 21 process/integration tests passed; standalone
  formatting and all-target/all-feature Clippy with warnings denied passed.
- Repository formatting, all-target/all-feature Clippy, workspace tests, the combined
  Fastly/Cloudflare/Spin check, and the Spin `wasm32-wasip2` check passed.
- Documentation install, Prettier, ESLint, and VitePress build passed. Placeholder-pin,
  legacy-typed-read, nested-app-config, action-reference, and documentation-reference scanners
  passed. The complete deploy-core action suite passed 302 tests with five macOS skips.
- Fastly adapter tests passed 287 tests; both required Fastly Clippy modes passed. The generated
  consumer workspace test passed, and the app-demo workspace passed formatting, strict Clippy, and
  all 33 tests.
- Independent specification-compliance and code-quality reviews approved the tranche after staged
  identity/cleanup, fixture coherence, process-path coverage, and watcher-lifecycle hardening.

The Linux no-replace branch is compile-checked but still requires hosted execution of
`renameat2(RENAME_NOREPLACE)`. Positive literal-`/work` CLI round trips, the real GNU-target CLI,
pinned Bookworm startup closure, controlled loader execution, and abnormal host cleanup remain Task
2 container/image-verification obligations. This tranche does not claim an image publication,
release, or cache-enabled deployment.

## Pin and Release-Evidence Record Tranche

Task 2 Section 6.1 now has the five-field image-record validator, exact ten-field canonical
release-evidence validator, and sole typed pair writer. The validator performs a bounded regular-file
check and consumes `jq --stream` events before ordinary object construction, so duplicate and escaped
duplicate top-level keys cannot be hidden by last-value-wins parsing. It enforces exact fields and
types, the fixed GHCR repository, nonzero lowercase image digest and source revision, informational
release-tag grammar, protocol `1`, canonical positive u32/u64 run strings, GitHub login grammar,
calendar-valid fresh UTC review time, exact no-newline JCS evidence bytes, and all cross-record
identities.

Only typed `runtime-ref`, `source-revision`, and `provenance-protocol` modes expose record data. The
runtime reference is always `repository@digest`; there is no tag accessor or tag-based pull output.
The pair writer accepts individual trusted scalar flags, fixes schema version internally, emits both
records through create-new hard-link publication in one physical parent, and rolls back a one-sided
publication only when it still owns that inode. It rejects raw JSON, duplicate/unknown/missing flags,
split parents, normalized protocol spellings, and either pre-existing destination. No placeholder
`image.json` or `image-release-evidence.json` was created.

TDD evidence was observed before implementation. The corrected initial image-contract run had 23
failures for missing fields, duplicate/unknown acceptance, absent typed modes, invalid digest/source/
protocol/tag acceptance, and absent pair validation. The writer suite failed because the typed writer
did not exist. Subsequent review-driven tests exposed the portable `wc -c` whitespace assumption and
then passed after normalization. Final local verification on 2026-09-08:

- Image record and typed-output contract: 44 passed, zero failed.
- Release-evidence, pair, and writer contract: 66 passed, zero failed.
- Bash syntax and ShellCheck at warning severity passed for both production scripts and both focused
  tests.
- The complete deploy-core action suite passed 304 tests with zero failures and five expected macOS
  skips. Action-reference, documentation-reference, and placeholder-pin scanners passed.
- Hosted CI for the preceding CLI/capability commit `525a18d1` passed every reported job, including
  Rust CodeQL, all four wasm Clippy/test legs, static checks, smoke tests, formatting, and workspace
  tests.

The paired validator requires both current records and the writer cannot partially add or replace a
pair. Git range-level atomic add/change/delete classification remains the explicit Section 6.5
classifier obligation. Image creation, toolchain/runtime verification, publication, protected gate
activation, and caching integration were open at that checkpoint. The following tranche completes
local image creation and runtime verification; publication, gate activation, and caching integration
remain open.

## Build Image and Runtime Verification Tranche

Task 2 Section 6.2 now has the closed staged image context, pinned multi-stage Dockerfile, immutable-
identity local and published-image verifier modes, and an exact runtime-capability verifier. The image
contains the standalone validator, protocol schema/corpus, Rust 1.95.0 toolchain with exactly the GNU
host and `wasm32-wasip1` targets, Fastly CLI 15.1.0, sccache 0.10.0, and the reviewed Bookworm startup
closure. Version checks combine strict stable-SemVer extraction with byte-exact complete command
output; target inspection compares the complete sorted set and compiles the committed wasm fixture.

The measured runtime closure is four regular, single-link ELF64 little-endian x86-64 shared objects:

- `/lib64/ld-linux-x86-64.so.2` (GNU OSABI):
  `02bcda52c1a5dfc236f94d9e5255b4a0e26347d8a372a5223b650e31f291ce3c`.
- `libc.so.6` (GNU OSABI):
  `6b4a45352fd0c540a9c7c718f35ce8c8e46a4e482f9d3885a910c32d1a0e1421`.
- `libgcc_s.so.1` (System-V OSABI):
  `2bd1552c47799ef67e701e81d4383061fd76059868e446e63560f0dd0d5ec14e`.
- `libm.so.6` (GNU OSABI):
  `7f2ca87f652f56b094462474b076749e90e689d0ecb9cb63c7679820b271b4e7`.

The closure verifier requires exact member count, bytes, ELF role, machine, SONAME, direct needed
set, supported dependency interpreter metadata, and absent `/etc/ld.so.preload`. The loader has no
`PT_INTERP`, has exact SONAME `ld-linux-x86-64.so.2`, and has no `DT_NEEDED`. Existing isolated ELF
tests separately reject primary/dependency alias collisions, prove the fixed loader alias resolves to
the already validated interpreter, ignore cache/default/hwcaps substitutions, and record `dlopen` as
outside the startup-only claim.

The image verifier compiles a committed real GNU Rust CLI with the selected image toolchain, packages
it through the provenance validator, validates it into a separate profile, and invokes only the
validated output through the fixed loader arguments. Every container uses a named create/inspect/
start/remove lifecycle, immutable linux/amd64 identity, uid/gid 1001, read-only root, no capabilities,
no-new-privileges, no network, bounded memory/pids/time, profile-specific mounts, and only home/temp
tmpfs. Before start, the verifier compares Docker's persisted `Path`/`Args`, removes the mode-0600
single-link env file, and verifies absence. The real environment probe preserves empty and special
UTF-8 values without recursive expansion or resplitting and proves inherited image/Docker variables
are absent after `env -i`.

Final local verification on 2026-09-08:

- Toolchain/runtime command fixtures: 38 passed, zero failed.
- Published/local image verifier fixtures: 52 passed, zero failed.
- Dockerfile/context contract: 13 passed, zero failed; isolated context staging: 26 passed, zero
  failed.
- Complete deploy-core action suite: 308 passed, zero failed, five platform-specific checks skipped
  on macOS.
- Complete standalone validator: 76 unit and 21 process/integration tests passed. The complete
  repository workspace test suite, repository and standalone formatting, strict all-target/all-
  feature Clippy, and the combined Fastly/Cloudflare feature check passed.
- Documentation Prettier, ESLint, and VitePress production build passed.
- Shell syntax and ShellCheck at warning severity passed. A real linux/amd64 image build completed,
  and the complete verifier passed against its immutable local image ID, including actual Docker
  env-file deletion, persisted process inspection, real GNU compile/package/validate/smoke, golden
  deterministic archive equality, and every malformed archive fixture.

No image has been published and no gate SHA, release tag, digest pin, release evidence, protected
workflow, or cache-enabled action is claimed by this tranche. Section 6.3 and later Task 2 work remain
open, and the same image verification must run again against the eventual protected/published
identities.

## v6.34 Design Reconciliation

Plan-only reconciliation recorded on 2026-09-10. **Implementation status: pending.** This section does
not claim that any v6.34 helper, test, workflow, hosted proof, or release operation has been
implemented or rerun, and it does not supersede the historical v6.30/v6.31 evidence above.

- At this reconciliation, normative plan references and completion reviews targeted design v6.34 while
  preserving historical reconciliation references.
- The four prerequisite-auditor CLIs now require `--policy-token-review-png`; the separate variable
  writer requires `--writer-token-review-png`. Policy-token, writer-token, and administrator-bypass
  screenshots require canonical absolute regular non-symlink PNGs, the exact signature, and the
  8..10,485,760-byte bound; each digest is recomputed and bound to its canonical review/evidence.
- Auditor evidence is canonical bounded JSON, but the downstream writer treats its 1..1,048,576 raw
  bytes as opaque and binds only their SHA-256 in the separate small JCS transition record.
- The prerequisite record now has exactly the v6.34 six-field shape without
  `required-workflow-sha`, retains exact rotation history, requires live gate/descriptor equality,
  and binds verified history to the exact second approval-line JSON object bytes using the digest from
  the first line. Bootstrap is a separate manually reviewed repository-variable POST; the writer is
  PATCH-only, requires an existing-variable GET 200, performs exact-byte idempotence before predecessor
  CAS, and permits same-gate source clear/inert refresh only when verified history advances for rollback.
- API plans now freeze GET/POST/PATCH/DELETE status and body contracts, literal ordered list queries,
  bounded synthesized pagination, Link/count/duplicate/truncation failures, exact check names, and
  `app_id=15368`. The deployment-protection-rules endpoint is a scalar no-query GET.
- The pin updater now carries the exact v6.34 CLI, App-token REST allowlist, PR bodies, evidence URL,
  exact `chore(actions): pin build container for <S>` title, private Git/askpass/worktree flow,
  conditional absent-target fetch, immediate pre-create absence recheck, force-with-lease push, and
  final remote/API readback contract.
- Repository-administrator workflow-run deletion is an accepted privileged risk. The plan does not
  claim a newer unrecorded failed run remains detectable after deletion, and no repository workflow
  receives run-deletion authority.
- The typed image/release-record writer plan now includes its exact CLI and current bytes, private
  mode-0700 outside-repository output parent, mode-0644 atomic pair, silence, cleanup, and focused tests.
- The yq plan now pins 4.53.3's exact release URL and four platform hashes and includes
  `install-yq.test.sh` in the planned and gate-manifest surfaces.
- Final qualification now checks all untracked files without ignoring submodules and verifies Node
  exactly against `.tool-versions` before `npm ci`.
- The literal gate manifest remains unfrozen until every helper and test exists; only then is its exact
  sorted list copied as a fenced block into the plan and required byte-equal to `gate-paths.txt`. No
  final list is invented by this reconciliation.

## v6.35 Design Reconciliation

Plan-only reconciliation recorded on 2026-09-10. **Implementation status: pending.** This section does
not claim that the v6.35 rotation-history or descriptor-flow changes are implemented or verified.

- At this reconciliation, normative plan references and completion reviews targeted design v6.35.
- Verified rotation history binds exact `created-at` and `updated-at` values. Auditors and publishers
  enumerate complete unfiltered history, select the unique greatest `updated_at`, and require selected
  run detail to reproduce the listed identity, timestamps, and current attempt. Tests include rerunning
  an older run id after a newer successful run and list/detail read races.
- The trusted local auditor, not the Actions publisher or opaque-record writer, verifies the live
  organization required-workflow descriptor. The writer independently checks only the live active-gate
  variable. For verified history, the publisher authenticates descriptor equality through the exact
  approved rotation receipt; bootstrap descriptor equality remains an independently reviewed auditor
  snapshot.

## v6.36 Design Reconciliation

Plan-only reconciliation recorded on 2026-09-10. **Implementation status: pending.** This section does
not claim that the v6.36 history snapshot or trust-boundary changes are implemented or verified.

## v6.37 Design Reconciliation

The publication verifier now requires a final exact run-detail reread after jobs and approvals, which
is its explicit current-attempt linearization point. Full repository checks exclude partial/promisor
clones and object alternates and disable lazy object fetching. The writer review's `token-id` is the
screenshot-bound credential inventory id rather than `/user.id`, and the local variable writer is an
operator-serialized procedure whose predecessor digest is explicitly not an atomic GitHub API CAS.
Implementation and focused adversarial verification of these corrections remain part of Task 2.

## v6.38 Design Reconciliation

The tag-triggered publisher lacked an authenticated ingress for the pin updater's required source PR
and exact evidence-comment URL. The release procedure now pre-creates an inert stable candidate-PR
comment, passes that URL to release audit, attaches the resulting evidence to the same comment, and
binds both PR and URL into an incompatible schema-version-2 prerequisite record. Inert records carry
nulls for the source revision, source PR, and evidence URL; release-bound records carry all three.

## v6.39 Design Reconciliation

The pin updater now resolves the reviewed real GitHub App bot identity through
`GET /users/<app-slug>[bot]`; installation tokens do not call the user-token-only `/user` endpoint.
The workflow installation-ID guard and final PR-author checks remain mandatory. Existing proposal
commits may be reconciled from an ancestor main parent, record blobs must be mode 0644, and a
same-source digest replacement verifies the closed old PR at the post-push branch OID.

- The publisher now selects the greatest documented per-workflow `run_number`, never `updated_at`, and
  binds a canonical digest of every current `{run-attempt,run-id,run-number}` tuple. Any rerun, including
  an older run id, invalidates the record; the writer cannot refresh a changed snapshot for the same
  selected run, so recovery requires a fresh successful dispatch.
- Two byte-identical complete history reads plus a repeated selected-run detail establish the guard's
  explicit post-concurrency linearization point. Work entering FIFO concurrency afterward is later work
  for the next publisher; the plan no longer claims ordinary REST reads atomically block it.
- Bootstrap review and descriptor evidence are explicitly a manual administrative trust-root procedure.
  Machine rejection covers malformed, stale, and cross-release records; authenticated rotation review
  starts with the first verified rotation receipt.

## v6.39 Protected Gate Implementation

The local Task 2 Sections 6.3 and 6.4 implementation is complete. This supersedes the historical
"implementation pending" status in the v6.34-v6.37 reconciliation notes without changing their design
history. The implementation includes the frozen gate manifest and exact CODEOWNERS expansion, protected
classifier and event-range selection, required CI workflow, release-prerequisite auditor, prerequisite
writer, approval gate, App-token boundary, pin updater, publisher structural checker, publication
verifier, rotation-lock verifier, publisher workflow, rotation workflow, and their adversarial fixture
suites.

Local completion does not activate the trust root or publish an image. Task 2 Section 6.5 and later
hosted steps remain open: merge and record `G`, configure repository rules, variables, environments,
and required workflows, obtain live queue-capacity and post-concurrency freshness evidence, execute the
credential smoke, publish and anonymously verify the image, merge the pin PR, and record `{G,S,D,B}`.
The cache primitive, producer/consumer provenance integration, provider lifecycle integration, and
application-repository adoption remain the separate follow-on plans 2 through 5.
