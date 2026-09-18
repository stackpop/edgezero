# Provider-Neutral Action Cores Design

## Status

Accepted on 2026-09-18. This design modularizes the existing Fastly GitHub
Actions without adding deployment support for another adapter.

## Problem

EdgeZero's application CLI and adapter registry are provider-neutral, but the
GitHub Action implementation is only partly separated. `build-app-cli`, GitHub
Environment validation, workspace isolation, and part of CLI execution are
shared. Immutable-release packaging and verification are under Fastly-named
paths, while config push, deploy, healthcheck, and rollback each implement some
of their own CLI invocation, credential handling, logging, and output parsing.

Adding another provider action in this shape would require copying security and
release-validation logic. Copies would drift in archive safety, credential
scrubbing, mutation reporting, source-revision checks, and application CLI
handling. A single public action with a generic `adapter` input would avoid some
copying but would erase meaningful provider differences from the action input
and output contracts.

## Goals

- Separate provider-neutral release and lifecycle machinery from Fastly policy.
- Keep Fastly actions as thin provider wrappers with explicit typed inputs.
- Make a future provider action reuse the core without changing the core.
- Preserve the exact existing Fastly public and immutable-release contracts.
- Keep one application CLI binary usable by every publisher and target.
- Preserve the current credential, archive-safety, digest, lifecycle-protocol,
  mutation-reporting, and recovery checks.
- Keep action shell and CI tooling free of Python and pip commands.

## Non-goals

- No Cloudflare, Spin, Axum, or synthetic public deployment action is added.
- No generic public `deploy` action is introduced.
- No public `release-core` or `deploy-core` action is introduced; both remain
  repository-internal script libraries.
- No provider-neutral staging, healthcheck, or rollback semantics are invented.
- No application CLI, Rust package, `edgezero.toml`, or public environment
  variable is renamed.
- No Fastly action input, output, default, archive member, or runtime behavior
  changes. The package action path is the single deliberate action-name change.

## Compatibility contract

The following public actions retain their names and complete input/output
surfaces:

- `build-app-cli`
- `config-push-fastly`
- `deploy-fastly`
- `healthcheck-fastly`
- `rollback-fastly`
- `require-github-environment`

The package wrapper is renamed from `package-fastly-application-release` to
`package-application-release-fastly`, following the same
operation/object/provider naming order as the other Fastly actions. The old path
is removed rather than retained as a compatibility alias. Coordinated consumers,
including the deployer workflow, must update their action reference in the same
rollout. Its complete input/output surface remains unchanged.

The configurable `app-cli-artifact` value remains a GitHub artifact name. The
archive produced by `build-app-cli` remains `app-cli.tar`. The immutable Fastly
release keeps these exact members:

```text
release.json
cli/app-cli.tar
package/app.tar.gz
edgezero.toml
<declared Fastly manifest path>
```

`release.json` retains format 1, lifecycle protocol 1, and its existing exact
schema. The `adapter` value remains `fastly` for a Fastly release. The complete
`edgezero.toml` is unchanged and can declare multiple adapters; a provider
release includes only the selected adapter manifest.

## Architecture

The action stack has three layers.

### Provider-neutral cores

`release-core` owns the immutable application-release protocol:

- confined input-file resolution;
- selected-adapter lookup in `edgezero.toml`;
- exact preservation of the selected adapter manifest path;
- application CLI extraction and metadata validation;
- declarative lifecycle capability probing;
- archive construction and exact-member validation;
- `release.json` syntax, schema, adapter, source-revision, and digest checks;
- path normalization, traversal rejection, duplicate rejection, and symlink
  rejection; and
- verified release outputs used by lifecycle actions.

`release-core` is a private collection of scripts and tests under
`.github/actions/release-core`; it has no `action.yml` and is not a callable
GitHub Action. Provider package actions invoke its scripts with fixed,
wrapper-owned policy data.

The core takes an expected adapter name and a provider-supplied lifecycle
capability declaration. It does not contain provider names, provider CLI names,
credential names, service identifiers, staging rules, or provider output names.
Capability declarations are data: command token arrays and flags that must
appear in the application CLI's help output. The core assigns lifecycle protocol
metadata only after every declared capability succeeds; the provider wrapper
selects the protocol value.

`deploy-core` owns secure application CLI execution:

- isolated per-invocation workspaces;
- application CLI archive download and extraction;
- exact argument transport through NUL-delimited files and Bash arrays;
- inherited provider-credential removal;
- typed provider-credential import from data rather than interpolated shell;
- action-private environment scrubbing;
- preservation of canonical public runtime store selectors;
- an optional provider-supplied allowlist for additional public runtime
  variables;
- working-directory and manifest selection;
- private lifecycle-log creation and cleanup primitives;
- provider exit-status preservation;
- `mutation-attempted` publication immediately before mutating CLI execution;
  and
- provider-neutral output parsing primitives for last-canonical,
  unique-canonical, and exact-one-canonical policies.

`deploy-core` likewise remains an internal script library with no public
`action.yml`. The invocation core resolves the verified application CLI itself
and receives one NUL-delimited file containing every argument after the
executable. The first entry is therefore the application CLI subcommand, followed
by its exact flags and values. Empty arguments are preserved. Newline-delimited
transport and shell command strings are rejected; the core never uses `eval`.
The executable comes only from the separately verified application CLI result
and cannot be replaced through the argument file.

The invocation core does not add a command, construct provider flags, or
interpret provider output values. A validated boolean controls mutation
reporting. Provider wrappers remain responsible for constructing the complete
argument file, deciding whether an operation can mutate, and choosing the
parsing policy for each output.

The parsing primitives have fixed provider-neutral semantics. Each receives a
literal key, an anchored value pattern, and a lifecycle log:

- **last-canonical** returns the value from the last line that exactly matches
  the key and pattern. Missing or malformed same-key lines do not themselves
  make the primitive fail; the provider wrapper decides whether an empty result
  is valid.
- **unique-canonical** requires at least one same-key line, rejects any same-key
  line that does not exactly match, accepts repeated identical values, and
  rejects more than one distinct value.
- **exact-one-canonical** requires exactly one same-key line and requires that
  line to match exactly. Repeated lines are rejected even when their values are
  identical.

Keys are treated as literals rather than regular-expression source. Value
patterns are fixed in provider wrapper code and cannot come from public action
inputs.

### Lifecycle capability declaration

Release-core receives one JSON capability declaration with this exact shape:

```json
{
  "lifecycle_protocol": 1,
  "probes": [
    {
      "command": ["deploy"],
      "required_flags": ["--adapter"]
    }
  ]
}
```

The declaration must be an object with exactly `lifecycle_protocol` and
`probes`. The protocol must be a positive integer. `probes` must be a non-empty
array. Each probe must contain exactly `command` and `required_flags`;
`command` must be a non-empty array of non-empty printable tokens, and
`required_flags` must be a non-empty array of unique printable tokens beginning
with `-`. Duplicate command arrays are invalid. The command array does not
contain `--help`; release-core appends it, requires a successful exit, and
requires every declared flag to appear as an exact help token or in
`--flag=<value>` form.

Release-core writes the declaration's `lifecycle_protocol` into `release.json`
only after the declaration and every probe pass. Consumer verification receives
the expected protocol from the provider wrapper and requires an exact match.
Release-core is protocol-neutral: it validates a positive integer and matching
producer/consumer expectations, while Fastly selects protocol 1.

The generic core validates structure and executes every declared probe. The
provider wrapper owns semantic completeness because different providers can
have different lifecycle commands. The Fastly wrapper's protocol-1 declaration
is fixed to these probes and is locked by a static contract test:

| Command         | Required flags                                                                                   |
| --------------- | ------------------------------------------------------------------------------------------------ |
| `deploy`        | `--adapter`, `--service-id`, `--application-release`, `--staging`                             |
| `config push`   | `--adapter`, `--manifest`, `--app-config`, `--store`, `--staging`, `--no-env`, `--yes`, `--no-diff` |
| `healthcheck`   | `--adapter`, `--service-id`, `--version`, `--domain`, `--path`, `--retry`, `--retry-delay`, `--timeout`, `--staging` |
| `rollback`      | `--adapter`, `--service-id`, `--version`, `--rollback-to`, `--staging`                       |
| `active-version` | `--adapter`, `--service-id`                                                                       |

Removing a Fastly probe or required flag is therefore a public protocol change,
not an internal refactor.

### Fastly wrappers

Fastly actions own only Fastly policy and translation:

- public Fastly action inputs and validation;
- Fastly credential aliases and typed credential values;
- pinned Fastly CLI installation;
- alphanumeric Fastly service-ID validation;
- construction of Fastly deploy, staging, healthcheck, and rollback arguments;
- the Fastly lifecycle capability declaration used by the release packager;
- Fastly-specific public runtime environment allowances retained for
  compatibility;
- Fastly provider-state reads and rollback-target capture; and
- interpretation and validation of Fastly version, package digest, health, and
  rollback outputs.

The wrappers call `release-core` and `deploy-core` through documented environment
and file contracts. Provider policy data, argument vectors, credential-clear
lists, and public-variable allowlists cross those boundaries through confined
files. Credential values remain opaque JSON data in an action-private environment
carrier and are never interpolated into shell source or command arguments.
Wrappers must not duplicate archive extraction, release schema validation,
provider-environment import, application CLI resolution, or generic output
parsing primitives.

`fastly-common` remains only for shared Fastly policy such as service-ID
validation. It does not contain generic release or CLI-execution machinery.

### Shared Rust release verifier

The immutable-release verifier currently under `edgezero-adapter-fastly` is
format-level logic. It moves behind the host-only `cli` feature of
`edgezero-adapter`, where every registered adapter can use it without introducing
runtime dependencies into adapter WASM builds.

The verifier accepts the expected adapter name and lifecycle protocol. It
returns confined paths and verified metadata for the selected adapter manifest,
application manifest, application CLI, and provider package. It owns exact
schema, normalized paths, exact members, regular-file enforcement, digest
verification, and loaded-manifest identity.

The Fastly adapter supplies `fastly` and protocol 1, then applies Fastly managed
deployment policy to the verified package. No other adapter consumes this API as
part of this change.

## Lifecycle data flow

### Packaging

1. `package-application-release-fastly` maps its existing inputs to the
   release-core contract and supplies adapter `fastly` plus the Fastly lifecycle
   capabilities.
2. Release-core validates confined inputs and extracts the application CLI.
3. Release-core probes the declared lifecycle commands and flags.
4. Release-core reads the Fastly manifest path from the complete
   `edgezero.toml`, confirms the explicit manifest input identifies that file,
   and preserves the declared path.
5. Release-core writes the unchanged format-1 metadata and archive.
6. Release-core invokes the same generic verifier used by consumers before
   publishing outputs.

### Consumer preparation

1. A Fastly lifecycle action maps its existing archive, digest, and expected
   source-revision inputs to release-core and supplies expected adapter `fastly`.
2. Release-core verifies the outer digest, archive safety, exact membership,
   metadata, inner digests, selected manifest relationship, and source revision.
3. Release-core publishes confined paths for the application CLI archive,
   application manifest, selected adapter manifest, package, package digest, and
   source revision.
4. The action extracts the recorded application CLI with the existing generic
   CLI archive verifier.

### CLI invocation

1. The Fastly wrapper validates provider-specific inputs and creates the exact
   post-executable application CLI argument vector in an action-owned
   NUL-delimited file.
2. It supplies the Fastly credential-clear list and typed credential JSON to
   deploy-core.
3. Deploy-core removes inherited aliases, imports only declared typed values,
   preserves allowed public runtime variables, scrubs private carriers, and
   resolves the verified application CLI.
4. The Fastly wrapper creates a private lifecycle log through the deploy-core
   helper and installs its cleanup trap.
5. For a mutating command, deploy-core publishes `mutation-attempted=true`
   immediately before execution.
6. The wrapper pipes deploy-core's unmodified output through `tee` into the
   private log and captures deploy-core's exit status separately from `tee`.
7. While the log still exists, the Fastly wrapper validates and publishes the
   Fastly-specific outputs. Every
   independently valid recovery output is published before another output
   contract can fail.
8. The wrapper exits with the application CLI's status when it failed. The
   cleanup trap removes the private log after output parsing on success and
   failure; cleanup failure can only replace a successful status.

## Security boundaries

Provider credentials never become generic public action inputs. A provider
wrapper maps its typed secret input into a JSON object and supplies the complete
alias-clear list. Deploy-core rejects a credential name absent from that list,
clears inherited aliases before importing values, and removes all action-private
`EDGEZERO__*` carriers before invoking application code.

Canonical store-selection variables remain public runtime inputs. Additional
provider-specific public variables require an explicit wrapper-owned allowlist;
deploy-core does not know their names.

`build-app-cli` and `require-github-environment` continue statically blanking the
credential aliases for every shipped provider. GitHub composite-action step
environments cannot dynamically remove unknown names before shell startup, so
this explicit security list is intentionally product-wide rather than a deploy
provider abstraction.

All arguments cross the core boundary as array elements. No core or provider
wrapper uses `eval`, shell command strings, or interpolated secrets. Temporary
logs, inline configuration, and extracted releases stay under the invocation's
confined workspace and are removed on every normal or error exit.

## Error and output behavior

Core failures use provider-neutral diagnostics that identify the violated
contract without printing credentials or configuration values. Provider wrappers
name provider-specific inputs and state.

The original application or provider CLI exit status wins over wrapper cleanup
or output errors. When the provider may already have mutated state, the wrapper
publishes each independently validated recovery value before rejecting a missing,
malformed, or conflicting output according to that action's existing contract.
Deploy requires every same-key line to be canonical, accepts repeated identical
values, and rejects multiple distinct values. Healthcheck, rollback, and config
push retain their last-canonical-value behavior. Exact-one parsing remains
available for Fastly state reads that require exactly one contract line. An
absent `mutation-attempted` remains insufficient proof that no provider mutation
occurred after a hard runner loss.

## Extensibility rule

A future provider adds public provider actions and thin wrapper scripts. It
supplies:

- its typed credentials and alias-clear list;
- its provider CLI installer, if any;
- its lifecycle capability declaration;
- its exact CLI argument vectors;
- its public runtime variable additions, if any; and
- its provider-specific output and recovery semantics.

It reuses release-core and deploy-core unchanged. If a proposed provider needs a
core change, that change must express a provider-neutral capability and include a
synthetic-adapter test. Provider names and special cases are not accepted in a
core.

## Verification requirements

Tests must prove:

- every Fastly action retains its exact public input/output surface;
- `package-application-release-fastly` replaces
  `package-fastly-application-release`, the old path is absent, and repository
  product-contract references use the new path;
- the format-1 `release.json` schema and archive member paths are unchanged;
- `app-cli.tar`, configurable artifact naming, application CLI binary naming,
  and selected-manifest path preservation are unchanged;
- release-core packages and verifies a synthetic adapter without any Fastly
  files, names, credentials, tools, or deployment implementation;
- release-core rejects an adapter mismatch, unsafe path, symlink, extra member,
  missing member, duplicate member, digest mismatch, source-revision mismatch,
  structurally incomplete lifecycle capability declaration, or failed declared
  probe;
- the Fastly package wrapper supplies exactly the five protocol-1 probes and
  required flags defined by this design;
- the shared Rust verifier accepts a synthetic adapter release and the Fastly
  adapter consumes it with expected adapter `fastly`;
- deploy-core invokes a synthetic application CLI with exact argument boundaries,
  including empty arguments, clears inherited aliases, imports only typed
  credentials, preserves allowed public runtime variables, and scrubs
  action-private carriers;
- mutation reporting happens after setup and immediately before execution;
- provider exit codes and independently valid recovery outputs are preserved;
- deploy accepts repeated identical canonical values but rejects malformed or
  distinct same-key values;
- healthcheck, rollback, and config push retain last-canonical-value parsing;
- exact-one parsing remains available to Fastly state reads that require it;
- lifecycle output remains available until the provider wrapper finishes
  recovery parsing, then its private log is removed;
- provider-neutral core files contain no Fastly, Cloudflare, Spin, or Axum
  policy branches;
- all existing Fastly config-push, production, staging, healthcheck, rollback,
  recovery, and store-free smoke tests pass without changed expectations; and
- ShellCheck, actionlint, archive contract tests, Rust workspace tests, adapter
  WASM checks, formatting, Clippy, and documentation checks remain green.
