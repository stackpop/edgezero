# Build Cache Implementation Evidence

Implementation branch: `feature/build-app-cli-cache` in the existing
`edgezero-actions-improve` worktree. The reviewed contract is design v6.29.

## Version Reference Review

On 2026-09-05, the upstream release API reported the following releases as neither
draft nor prerelease. Anonymous `git ls-remote` confirmed each tag and its peeled
commit, with no same-named branch. Executable YAML uses the version column, not
the recorded commit. Existing workflow major-version choices are preserved.

| Release | Resolved Commit |
| --- | --- |
| [actions/checkout v6.1.0](https://github.com/actions/checkout/releases/tag/v6.1.0) | `d23441a48e516b6c34aea4fa41551a30e30af803` |
| [actions/checkout v7.0.1](https://github.com/actions/checkout/releases/tag/v7.0.1) | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| [actions/cache v5.1.0](https://github.com/actions/cache/releases/tag/v5.1.0) | `caa296126883cff596d87d8935842f9db880ef25` |
| [actions/cache v6.1.0](https://github.com/actions/cache/releases/tag/v6.1.0) | `55cc8345863c7cc4c66a329aec7e433d2d1c52a9` |
| [actions/upload-artifact v7.0.1](https://github.com/actions/upload-artifact/releases/tag/v7.0.1) | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` |
| [actions/download-artifact v8.0.1](https://github.com/actions/download-artifact/releases/tag/v8.0.1) | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` |
| [actions/setup-node v6.5.0](https://github.com/actions/setup-node/releases/tag/v6.5.0) | `249970729cb0ef3589644e2896645e5dc5ba9c38` |
| [actions-rust-lang/setup-rust-toolchain v1.17.0](https://github.com/actions-rust-lang/setup-rust-toolchain/releases/tag/v1.17.0) | `166cdcfd11aee3cb47222f9ddb555ce30ddb9659` |
| [github/codeql-action v4.37.9](https://github.com/github/codeql-action/releases/tag/v4.37.9) | `cdf488f595d80d6e07e03d4674febd5ab45fa938` |
| [actions/configure-pages v6.0.0](https://github.com/actions/configure-pages/releases/tag/v6.0.0) | `45bfe0192ca1faeb007ade9deae92b16b8254a0d` |
| [actions/upload-pages-artifact v5.0.0](https://github.com/actions/upload-pages-artifact/releases/tag/v5.0.0) | `fc324d3547104276b827a68afc52ff2a11cc49c9` |
| [actions/deploy-pages v5.0.1](https://github.com/actions/deploy-pages/releases/tag/v5.0.1) | `368f82528645a54fb793d4d04e342629a3f51346` |
| [actions/create-github-app-token v3.2.0](https://github.com/actions/create-github-app-token/releases/tag/v3.2.0) | `bcd2ba49218906704ab6c1aa796996da409d3eb1` |

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
