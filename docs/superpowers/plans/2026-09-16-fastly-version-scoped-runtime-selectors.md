# Fastly Version-Scoped Runtime Selectors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Fastly production and staging deployments resolve canonical store variables into immutable service-version descriptors and exact resource links before the version is published.

**Architecture:** The generic CLI asks each registered adapter, through a provider-neutral preflight hook, whether the manifest command or the adapter owns deployment. Fastly owns staging and every deployment with declared stores, writes one canonical descriptor into the shared `edgezero_runtime_env` store under a service/version key, reconciles links on an unreachable draft, verifies both, and only then stages or activates. Runtime and deploy share a strict descriptor module; other adapters keep their existing process-environment behavior and unregistered custom adapters keep manifest-command deployment.

**Tech Stack:** Rust 1.95, serde/serde_json, thiserror, Fastly SDK 0.12.1 and CLI 15.1.0, Bash composite actions, VitePress/Prettier/ESLint

---

## Scope and file map

This is one integrated plan because the descriptor wire format, deployment ordering,
runtime lookup, action output, and documentation form one release contract. Each task
still leaves a focused component testable on its own.

- Create `crates/edgezero-adapter-fastly/src/runtime_descriptor.rs`: the shared
  descriptor key, strict duplicate-detecting parser, canonical serializer, value
  validation, runtime allowlist, and prior managed-alias discovery.
- Modify `crates/edgezero-adapter/src/registry.rs`: provider-neutral deployment
  ownership and non-secret manifest variable defaults.
- Modify `crates/edgezero-cli/src/lib.rs`: construct the typed deployment context.
- Modify `crates/edgezero-cli/src/adapter.rs`: arbitrate manifest-command versus
  adapter-managed deployment without checking provider names.
- Modify `crates/edgezero-adapter-fastly/src/lib.rs`: load the current service/version
  descriptor and propagate failures before app/store construction.
- Modify `crates/edgezero-adapter-fastly/src/request.rs`: update custom-entry-point
  guidance for the fallible runtime loader.
- Modify `crates/edgezero-adapter-fastly/src/cli.rs`: Fastly ownership, strict args,
  environment resolution, provider preflight, migration inventory, link planning,
  draft preparation, descriptor verification, and publication.
- Create `.github/actions/fastly-common/scripts/common.sh`: one shared alphanumeric
  Fastly service-ID validator for deploy, capture, healthcheck, and rollback actions.
- Modify `.github/actions/deploy-core/scripts/run-app-cli.sh`: preserve the fixed
  public runtime keys needed in the descriptor while retaining the credential scrub.
- Modify Fastly action scripts/YAML and `.github/actions/deploy-core/tests/*`: keep a
  valid emitted version when a later deployment operation fails and model the new
  single-store lifecycle.
- Modify the Fastly, CLI, deployment, adoption, and migration guides listed in Task 9.

Do not add a Tokio dependency, do not create another physical selector store, and do
not touch the user's untracked
`examples/app-demo/crates/app-demo-adapter-cloudflare/.dev.vars`.

### Task 1: Add provider-neutral deployment ownership

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs:44-74,308-358,710-845`
- Modify: `crates/edgezero-cli/src/lib.rs:164-219,587-776`
- Modify: `crates/edgezero-cli/src/adapter.rs:162-209,455-560`

- [ ] **Step 1: Write failing registry tests for the default deployment contract**

Add tests proving a default adapter selects `ManifestCommand` and a default
`AdapterDeployContext` contains no variable defaults:

```rust
#[test]
fn default_deploy_preflight_keeps_manifest_command() {
    let context = AdapterDeployContext::default();
    assert_eq!(
        FIRST.preflight_deploy(&context, &[]).unwrap(),
        DeployOwnership::ManifestCommand
    );
    assert!(context.variable_defaults.is_empty());
}
```

- [ ] **Step 2: Run the adapter test and verify it fails**

Run: `cargo test -p edgezero-adapter default_deploy_preflight_keeps_manifest_command`

Expected: FAIL because `DeployOwnership`, `preflight_deploy`, and
`variable_defaults` do not exist.

- [ ] **Step 3: Add the neutral ownership API and typed defaults**

Add `BTreeMap` to the existing collections import and define:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeployOwnership {
    #[default]
    ManifestCommand,
    AdapterManaged,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdapterDeployContext {
    pub adapter_manifest_path: Option<PathBuf>,
    pub service_id: Option<String>,
    pub staging: bool,
    pub stores: DeployStoreIds,
    pub variable_defaults: BTreeMap<String, String>,
}
```

Add this default hook to `Adapter` before `deploy`:

```rust
fn preflight_deploy(
    &self,
    _context: &AdapterDeployContext,
    _args: &[String],
) -> Result<DeployOwnership, String> {
    Ok(DeployOwnership::ManifestCommand)
}
```

- [ ] **Step 4: Run the adapter crate tests**

Run: `cargo test -p edgezero-adapter`

Expected: PASS.

- [ ] **Step 5: Write failing CLI dispatch tests**

In `edgezero-cli/src/lib.rs`, invert
`run_custom_deploy_with_stores_requires_registered_adapter_before_command` so the
unregistered manifest command succeeds and creates its marker. Add a small registered
recording adapter proving:

1. `AdapterManaged` bypasses a manifest deploy command;
2. a preflight error prevents that command from running;
3. adapter-applicable manifest variable defaults reach `AdapterDeployContext`; and
4. manifest secrets never appear in `variable_defaults`.

- [ ] **Step 6: Run the focused CLI tests and verify they fail**

Run: `cargo test -p edgezero-cli run_custom_deploy_with_stores -- --nocapture`

Run: `cargo test -p edgezero-cli deploy_preflight -- --nocapture`

Expected: FAIL because store declarations still force adapter registration and no
ownership hook participates in dispatch.

- [ ] **Step 7: Thread defaults and implement ownership arbitration**

In `run_deploy`, collect only applicable non-secret defaults:

```rust
let variable_defaults = manifest
    .as_ref()
    .map(|loader| loader.manifest().environment_for(&args.adapter))
    .into_iter()
    .flat_map(|environment| environment.variables)
    .filter_map(|binding| binding.value.map(|value| (binding.env, value)))
    .collect();
```

Set that field on `AdapterDeployContext`. In `adapter::deploy`:

1. look up the registered adapter without requiring it;
2. call `preflight_deploy` when registered;
3. treat an unregistered adapter as `ManifestCommand`;
4. run a present manifest command only for non-staging `ManifestCommand` ownership,
   then call an optional registered adapter's `finalize_deploy`;
5. call only `adapter.deploy` for `AdapterManaged`; and
6. when no manifest command exists, require and run the adapter, retaining the legacy
   finalizer only for that fallback path.

Remove the current `context.stores.is_empty()` registration rule. Do not add an
adapter-name comparison.

- [ ] **Step 8: Run the CLI tests**

Run: `cargo test -p edgezero-cli`

Expected: PASS, including the unchanged parent-environment-over-manifest-default
subprocess tests.

- [ ] **Step 9: Commit the neutral dispatch boundary**

```bash
git add crates/edgezero-adapter/src/registry.rs crates/edgezero-cli/src/lib.rs crates/edgezero-cli/src/adapter.rs
git commit -m "refactor(deploy): let adapters claim managed deployment"
```

### Task 2: Define the strict version descriptor contract

**Files:**

- Create: `crates/edgezero-adapter-fastly/src/runtime_descriptor.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs:1-45`
- Test: `crates/edgezero-adapter-fastly/src/runtime_descriptor.rs`

- [ ] **Step 1: Write descriptor key and round-trip tests**

Cover exact keys for two services and two versions, sorted canonical bytes, a valid
descriptor containing fixed runtime values plus Config/KV/Secret selectors, and
`managed_store_aliases()` discovering an old store absent from the new manifest.

```rust
assert_eq!(
    runtime_descriptor_key("SvcA1", 42),
    "EDGEZERO__SERVICES__SvcA1__VERSIONS__42__ENV_V1"
);
```

- [ ] **Step 2: Write malformed descriptor tests**

Add table tests for duplicate `format`, duplicate `entries`, duplicate entry names,
missing fields, unknown top-level fields, wrong types, format `2`, unknown runtime
keys, undeclared store IDs, blank/control store names or keys, invalid port, invalid
boolean, invalid logging level, and values that must never be echoed in diagnostics.
Add a policy-separation case whose descriptor is structurally valid but contains an
unknown entry shape: `validated_env` must reject it while `managed_store_aliases()`
ignores that shape and still returns any valid managed aliases beside it.

- [ ] **Step 3: Run the focused tests and verify they fail**

Run: `cargo test -p edgezero-adapter-fastly runtime_descriptor -- --nocapture`

Expected: FAIL because the module and symbols do not exist.

- [ ] **Step 4: Implement the wire type with duplicate-detecting visitors**

Create a private shared module gated for CLI, Fastly runtime, or tests. Its core API
must be:

```rust
pub(crate) const RUNTIME_DESCRIPTOR_FORMAT: u8 = 1;

pub(crate) struct RuntimeDescriptor {
    entries: BTreeMap<String, String>,
}

pub(crate) fn runtime_descriptor_key(service_id: &str, version: u64) -> String;

impl RuntimeDescriptor {
    pub(crate) fn from_entries(entries: BTreeMap<String, String>) -> Result<Self, RuntimeDescriptorError>;
    pub(crate) fn parse(raw: &str) -> Result<Self, RuntimeDescriptorError>;
    pub(crate) fn canonical_json(&self) -> Result<String, RuntimeDescriptorError>;
    pub(crate) fn validated_env(
        &self,
        stores: StoresMetadata,
    ) -> Result<EnvConfig, RuntimeDescriptorError>;
    pub(crate) fn managed_store_aliases(&self) -> Result<BTreeSet<String>, RuntimeDescriptorError>;
}
```

Implement custom serde `Visitor`s for both maps. Reject duplicate/unknown top-level
fields and duplicate entry names during deserialization; do not parse through
`serde_json::Value`. Serialize `{ "format": 1, "entries": ... }` from a
`BTreeMap` and reject bytes over `FASTLY_CONFIG_ENTRY_LIMIT`.

The runtime allowlist contains the fixed adapter/logging keys plus `__NAME` for each
declared Config/KV/Secret ID and `__KEY` for declared Config IDs. Validate all entries
before calling `EnvConfig::from_vars`. Redact raw values from errors.

- [ ] **Step 5: Implement deployment-side prior alias discovery**

Recognize only the exact key grammar
`EDGEZERO__STORES__<CONFIG|KV|SECRETS>__<NONEMPTY_ID>__NAME`. Validate the physical
name and return its value even when the logical ID is absent from current metadata.
Ignore unrelated fixed settings and unknown shapes; never use arbitrary descriptor
data as a deletion candidate. Keep structural parsing separate from those two consumer
policies so migration can inspect an older valid descriptor without weakening runtime
validation.

- [ ] **Step 6: Run descriptor tests and crate tests**

Run: `cargo test -p edgezero-adapter-fastly runtime_descriptor -- --nocapture`

Run: `cargo test -p edgezero-adapter-fastly`

Expected: PASS.

- [ ] **Step 7: Commit the shared wire format**

```bash
git add crates/edgezero-adapter-fastly/src/runtime_descriptor.rs crates/edgezero-adapter-fastly/src/lib.rs
git commit -m "feat(fastly): define versioned runtime descriptors"
```

### Task 3: Make Fastly runtime loading version-scoped and fallible

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/lib.rs:35-290,344-435`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs:180-215,310-335`
- Test: `crates/edgezero-adapter-fastly/src/runtime_descriptor.rs`

- [ ] **Step 1: Write pure lookup tests before using Fastly hostcalls**

Add an injected helper in `runtime_descriptor.rs` that accepts service ID, version,
store metadata, and a `FnMut(&str) -> Result<Option<String>, E>`. Test:

- two services sharing one map resolve distinct descriptors;
- old/new versions of one service resolve distinct values;
- a missing store or descriptor returns an empty `EnvConfig` only with no declared
  stores;
- missing data with any declared store fails;
- lookup errors and malformed descriptors always fail.

Capture the store-free fallback diagnostic and assert it names the missing
service/version descriptor key without including descriptor contents.

- [ ] **Step 2: Run focused tests and verify they fail**

Run: `cargo test -p edgezero-adapter-fastly descriptor_lookup -- --nocapture`

Expected: FAIL because runtime lookup still reads unscoped individual entries and is
infallible.

- [ ] **Step 3: Implement the pure lookup and typed error**

Define `RuntimeEnvConfigError` with redacted variants for store absence, lookup
failure, descriptor absence, and invalid descriptor. Implement
`std::error::Error + Send + Sync + 'static` through `thiserror`.

- [ ] **Step 4: Replace the Fastly runtime reader**

Change the public API to:

```rust
pub fn runtime_env_config(
    stores: StoresMetadata,
) -> Result<EnvConfig, RuntimeEnvConfigError>
```

Use `fastly::compute_runtime::service_id()` and `service_version()` to construct the
key. Open `edgezero_runtime_env` with `ConfigStore::try_open`; only
`OpenError::ConfigStoreDoesNotExist` may become the store-free fallback. Read through
`try_get`; every other open/lookup error propagates. Remove `runtime_env_vars`, the
unscoped per-key reads, and their obsolete tests.

- [ ] **Step 5: Propagate failure before constructing the app or registries**

In `run_app_with_request_extensions`, use:

```rust
let stores = A::stores();
let env = runtime_env_config(stores)?;
```

Update custom-entry-point examples in `request.rs` to use `?`. Keep `run_app`'s
existing `Result<fastly::Response, fastly::Error>` signature.

- [ ] **Step 6: Run Fastly runtime and target checks**

Run: `cargo test -p edgezero-adapter-fastly`

Run: `cargo check -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1`

Expected: PASS.

- [ ] **Step 7: Commit runtime lookup**

```bash
git add crates/edgezero-adapter-fastly/src/lib.rs crates/edgezero-adapter-fastly/src/request.rs crates/edgezero-adapter-fastly/src/runtime_descriptor.rs
git commit -m "feat(fastly): load runtime config by service version"
```

### Task 4: Enforce Fastly arguments, IDs, and environment precedence

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:133-221,417-484,3589-3823,4357-4450,4921-4938,5700-5825,5989-6035`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` context literals around
  `5029,9561,11217,11288,11340`

- [ ] **Step 1: Write exhaustive reserved-argument tests**

Table-test every forbidden spelling from the design:

```text
--service-id x, --service-id=x, -s x, -s=x, -sVALUE
--service-name x, --service-name=x
--version x, --version=x
--autoclone
--token x, --token=x, -t x, -t=x, -tVALUE
```

Prove the scan runs even for a store-free production manifest command.

- [ ] **Step 2: Write managed allowlist tests**

Accept only `--comment`, `--package`/`-p` in detached/inline/attached forms and the
documented global booleans. Reject unknown flags, positional arguments, missing/empty
values, and conflicting duplicate value flags. Ownership switches only when the full
managed state machine lands in Task 6, so this task must leave store-aware production
on its existing path.

- [ ] **Step 3: Write service-ID and environment-precedence tests**

Change underscore/hyphen cases to rejection and keep mixed ASCII alphanumerics. Test
that parent process values override `context.variable_defaults`, defaults fill absent
parents, and logical store IDs fill absent selectors. Test an invalid present value as
an error rather than an `EnvConfig` fallback.

Add path-level rejection tests for deploy, active-version capture, healthcheck,
rollback, provision service selection, and direct `Adapter::deploy` calls. Each test
must prove the provider CLI/API fake was not invoked with the invalid ID; direct helper
tests alone do not establish that every entry path calls the validator.

- [ ] **Step 4: Run focused tests and verify they fail**

Run: `cargo test -p edgezero-adapter-fastly --features cli deploy_arg -- --nocapture`

Run: `cargo test -p edgezero-adapter-fastly --features cli service_id -- --nocapture`

Run: `cargo test -p edgezero-adapter-fastly --features cli deploy_environment -- --nocapture`

Expected: FAIL under the current permissive/drop behavior.

- [ ] **Step 5: Implement exact argument parsing**

Replace `COMPUTE_UPDATE_*`, `StagedPassthrough`, and
`split_staged_passthrough` with:

```rust
struct ManagedDeployArgs {
    comment: Option<String>,
    package: Option<String>,
    globals: Vec<String>,
}

fn scan_reserved_deploy_args(args: &[String]) -> Result<(), String>;
fn parse_managed_deploy_args(args: &[String]) -> Result<ManagedDeployArgs, String>;
```

Parse token-by-token so detached values are consumed exactly once and short attached
forms cannot bypass the scan. Never silently drop an argument.

- [ ] **Step 6: Implement Fastly argument preflight and effective environment**

Override `preflight_deploy` to validate the effective service ID and reserved flags,
while retaining `ManifestCommand` ownership at this intermediate checkpoint. Staging
continues to reach the adapter because manifest deploy commands are production-only.
Expose and test the stricter managed parser, but do not route store-aware production
through it until Task 6 can prepare descriptors and links before activation.

Build `EnvConfig` from `context.variable_defaults` followed by `std::env::vars()` so
parent values win, but validate present raw values before applying defaults. Keep the
current finalizer intact at this checkpoint; Task 6 removes selector writes in the
same change that enables managed ownership.

- [ ] **Step 7: Apply the alphanumeric service-ID rule everywhere in Rust**

Change `validate_service_id` to nonempty ASCII alphanumeric only and route deploy,
active-version, healthcheck, rollback, provision selection, and direct adapter calls
through it. Update all fixtures that accidentally use punctuation.

- [ ] **Step 8: Run Fastly CLI and CLI integration tests**

Run: `cargo test -p edgezero-adapter-fastly --features cli`

Run: `cargo test -p edgezero-cli`

Expected: PASS.

- [ ] **Step 9: Commit Fastly deploy preflight**

```bash
git add crates/edgezero-adapter-fastly/src/cli.rs
git commit -m "fix(fastly): validate managed deploy inputs before dispatch"
```

### Task 5: Build a read-only Fastly deployment and migration plan

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:223-390,3502-4075,4514-4608,4982-5027,5061-5374`
- Modify: `crates/edgezero-adapter-fastly/src/runtime_descriptor.rs`
- Test: co-located tests in both files

- [ ] **Step 1: Write version-source parser tests**

Refactor version JSON parsing into records containing `number`, `active`, `locked`,
staging state, and environments. Test active selection plus first-deploy selection of
the unique highest unlocked/inactive/unstaged draft. Reject missing versions,
duplicate numbers, malformed fields, a locked/staged/deployed highest version, and
ambiguous drafts.

- [ ] **Step 2: Write pure link-plan tests**

Cover exact desired links, already-correct links, an alias collision with a different
resource ID, stale aliases from a previous version descriptor, a store removal/rename
whose old logical ID is absent from the new manifest, unrelated-link preservation,
shared-link preservation, and Config/KV/Secret isolation.

- [ ] **Step 3: Write legacy migration-inventory tests**

Cover PR #344 keys of the form
`EDGEZERO__SERVICES__<SERVICE_ID>__<CANONICAL_SUFFIX>`, current unscoped canonical
keys, and unscoped entries read from a legacy twin linked under
`edgezero_runtime_env`. Prove legacy values identify only already-linked candidate
aliases, never populate the new descriptor, and ambiguous candidates are preserved.
Also prove another service's scoped entries are ignored, and an unambiguous legacy
alias whose resolved resource ID matches the source link is removed when no longer
selected.

- [ ] **Step 4: Run focused tests and verify they fail**

Run: `cargo test -p edgezero-adapter-fastly --features cli deploy_plan -- --nocapture`

Run: `cargo test -p edgezero-adapter-fastly --features cli migration_inventory -- --nocapture`

Expected: FAIL because the plan types and exact migration rules do not exist.

- [ ] **Step 5: Add typed planning structures**

Keep provider I/O in `cli.rs` and add focused private types:

```rust
enum PublishTarget { Production, Staging }
enum EditableVersionSource {
    CloneActive { active: u64 },
    InitialDraft { snapshot: InitialDraftSnapshot },
}
struct DesiredResourceLink {
    logical_id: Option<String>,
    kind: ResourceKind,
    alias: String,
    selected_name: String,
    resource_id: String,
}
struct LinkReconciliation {
    delete_link_ids: Vec<String>,
    create: Vec<DesiredResourceLink>,
}
struct ManagedDeployPlan { /* resolved immutable preflight data */ }
```

Do not move unrelated provisioning/config-GC code out of `cli.rs`.

- [ ] **Step 6: Resolve the complete plan without mutations**

Before `compute update`, resolve and validate:

1. service ID, token, target, args, and effective environment;
2. canonical descriptor bytes and the 8,000-byte limit;
3. the one physical `edgezero_runtime_env` ID;
4. every selected Config/KV/Secret resource ID;
5. all service versions and the active version or existing initial draft;
6. the source version's links;
7. its exact prior descriptor, or legacy inventory when absent; and
8. the complete desired create/delete link plan.

For no-active first deployment, snapshot the exact existing initial draft's version
metadata, domains, backends, logging endpoints, settings, and links. Never create a
blank version. Fail before publication if no suitable initialized draft exists.

- [ ] **Step 7: Implement exact migration ownership**

Read scoped legacy entries only for the current service. Read unscoped entries from
the resource actually linked under `edgezero_runtime_env`, which covers a legacy
staging twin. Compare candidates with the source version's actual alias/resource-ID
pairs. Delete only unambiguous matches; preserve unknown or ambiguous links. Leave all
legacy entries and physical twins intact.

- [ ] **Step 8: Run plan and parser tests**

Run: `cargo test -p edgezero-adapter-fastly --features cli deploy_plan -- --nocapture`

Run: `cargo test -p edgezero-adapter-fastly --features cli migration_inventory -- --nocapture`

Expected: PASS.

- [ ] **Step 9: Commit provider preflight planning**

```bash
git add crates/edgezero-adapter-fastly/src/cli.rs crates/edgezero-adapter-fastly/src/runtime_descriptor.rs
git commit -m "feat(fastly): plan descriptors and resource links before deploy"
```

### Task 6: Prepare and publish one exact Fastly draft

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:4124-4255,4514-4816,4982-5374,9487-11580`
- Modify: `crates/edgezero-cli/src/lib.rs:704-776`
- Test: co-located stateful fake tests in `crates/edgezero-adapter-fastly/src/cli.rs`

- [ ] **Step 1: Replace twin-oriented fake expectations with a stateful fake**

Model service versions, clone inheritance, links per version, one runtime Config Store
holding descriptors for multiple services/versions, selected stores by kind,
create-only entries, readback, stage/activate, and injected failures for every
mutation boundary. Remove expectations for
`edgezero_runtime_env_staging_<service-id>`.

- [ ] **Step 2: Write production and staging success tests**

Prove:

- active production and staging both clone/update the active version;
- production activates while staging stages;
- equal and different environment store selections produce exact links;
- `edgezero_runtime_env` always links the one shared physical store;
- descriptor/link verification occurs before publish;
- old and new versions retain different descriptor bytes; and
- two services sharing the physical store never collide.

Also prove the completed Fastly `preflight_deploy` returns `AdapterManaged` for
staging or any declared store, returns `ManifestCommand` for store-free production,
and prevents a store-aware production manifest command from running.

Replace `run_deploy_reconciles_fastly_selectors_after_a_manifest_command` in
`edgezero-cli/src/lib.rs` with that store-aware bypass assertion. Keep the existing
store-free manifest-command forwarding test unchanged.

- [ ] **Step 3: Write first-deploy and failure-order tests**

Prove first deploy reuses the existing initial draft and preserves domains, backends,
logging endpoints, settings, and unrelated links. Reject missing, locked, staged, or
changed initial drafts. Inject lookup, update, comment, link delete/create, descriptor
create/readback, stage, and activation failures; assert no earlier active/staged
version is modified.

For every read-only preflight failure (version/store/descriptor/link lookup, parse, or
plan collision) and for local `compute build` failure, assert the fake operation log
contains zero provider mutations: no clone, package upload, comment, link delete/create,
descriptor create, stage, or activation.

- [ ] **Step 4: Write descriptor immutability and version-output tests**

Accept a retry when existing canonical bytes are identical. Reject different bytes
without overwrite. Prove `version=<N>` is emitted as soon as a draft is selected or
created and remains present when a later operation fails.

- [ ] **Step 5: Run focused tests and verify they fail**

Run: `cargo test -p edgezero-adapter-fastly --features cli managed_deploy -- --nocapture`

Expected: FAIL under the current post-activation/twin lifecycle.

- [ ] **Step 6: Enable managed ownership with the common state machine**

Change Fastly's `preflight_deploy` ownership result to `AdapterManaged` exactly when
`context.staging || !context.stores.is_empty()`. In the same implementation change,
route those calls into the complete state machine below; do not leave a checkpoint
where managed ownership reaches the legacy production or twin lifecycle.

Perform local build before remote draft mutation. For an active service, run
`fastly compute update --autoclone --version=active` with the exact validated package
and global args. For first deploy, revalidate the initial snapshot, emit its version,
and run `compute update --version=<N>` without autoclone. Capture command status and
combined output separately so a returned version can be logged before preserving a
nonzero status.

Apply the comment to that exact version. Re-list clone links and require them to match
the source inventory before reconciliation.

- [ ] **Step 7: Reconcile links on the unreachable draft**

Delete only planned stale managed aliases, create only planned desired links, reject
collisions, then re-list and require the final alias/resource-ID map for all managed
links. Preserve unrelated and intentionally shared links.

- [ ] **Step 8: Create and verify the immutable descriptor**

Read the exact descriptor key first. If absent, use
`fastly config-store-entry create --store-id=<id> --key=<key> --stdin` without
`--upsert`. If present, accept only exact canonical bytes. Read it back and require
byte equality. Never update or delete a version descriptor.

- [ ] **Step 9: Revalidate and publish**

Re-read draft state and final links. For an initial draft, compare protected
domains/backends/logging/settings with its preflight snapshot while excluding
EdgeZero's own package, comment, and planned link mutations. Call
`service-version stage` for staging or the Fastly activation endpoint for production.
Remove `relink_runtime_env_for_staging`, staging-twin creation/mirroring, and
post-activation selector reconciliation. Remove selector writes from
`finalize_deploy` in this same step; retain only store-free manifest/fallback version
capture.

- [ ] **Step 10: Run Fastly and CLI tests**

Run: `cargo test -p edgezero-adapter-fastly --features cli`

Run: `cargo test -p edgezero-cli`

Expected: PASS.

- [ ] **Step 11: Commit the atomic lifecycle**

```bash
git add crates/edgezero-adapter-fastly/src/cli.rs crates/edgezero-cli/src/lib.rs
git commit -m "fix(fastly): prepare selectors before publishing versions"
```

### Task 7: Preserve runtime inputs and failed-deploy version output in actions

**Files:**

- Create: `.github/actions/fastly-common/scripts/common.sh`
- Modify: `.github/actions/deploy-core/scripts/run-app-cli.sh:199-218`
- Modify: `.github/actions/deploy-fastly/scripts/validate.sh:21-37`
- Modify: `.github/actions/deploy-fastly/scripts/deploy.sh:22-80`
- Modify: `.github/actions/deploy-fastly/scripts/capture-previous.sh:39-65`
- Modify: `.github/actions/healthcheck-fastly/scripts/validate.sh:1-16`
- Modify: `.github/actions/healthcheck-fastly/scripts/healthcheck.sh:27-40`
- Modify: `.github/actions/rollback-fastly/scripts/validate.sh:1-16`
- Modify: `.github/actions/rollback-fastly/scripts/rollback.sh:22-35`
- Modify: `.github/actions/deploy-fastly/action.yml`
- Modify: `.github/actions/healthcheck-fastly/action.yml`
- Modify: `.github/actions/rollback-fastly/action.yml`
- Test: `.github/actions/deploy-core/tests/run.sh:403-456,647-705,2008-2233,2330-2408`

- [ ] **Step 1: Write failing shell contract tests**

Change underscore/hyphen service IDs from accepted to rejected in every lifecycle
wrapper and prove valid mixed alphanumeric IDs pass. Add tests that fixed public keys
(`EDGEZERO__ADAPTER__HOST`, `__PORT`, `EDGEZERO__LOGGING__ENDPOINT`, `__LEVEL`) and
canonical store selectors survive `run-app-cli.sh`, while arbitrary or malformed
`EDGEZERO__*` names remain scrubbed.

Add a fake deploy CLI that prints `version=42` then exits with a distinctive nonzero
status. Require `fastly-version=42` and the unchanged exit status. Malformed,
conflicting, or absent version lines on a failed command must emit no version and
preserve the original status.

- [ ] **Step 2: Run action tests and verify they fail**

Run: `bash .github/actions/deploy-core/tests/run.sh`

Expected: FAIL on current deploy ID acceptance, runtime-key scrub, and early failure
before version parsing.

- [ ] **Step 3: Add one shared Fastly service-ID helper**

Create `require_fastly_service_id` in `fastly-common/scripts/common.sh`, implemented
through the generic `require_input_matching` with `^[A-Za-z0-9]+$`. Source it from
deploy validation/execution, previous-version capture, healthcheck validation/execution,
and rollback validation/execution. Pass the ID into early composite validation steps so
bad IDs fail before artifact download.

- [ ] **Step 4: Preserve only the additional fixed runtime keys**

Extend `capture_public_runtime_env` with an exact allowlist for host, port, logging
endpoint, and logging level. Keep its canonical store-selector syntax check and private
input scrub unchanged.

- [ ] **Step 5: Parse a valid version before returning CLI failure**

Refactor the parsing in `deploy.sh` into a helper that classifies missing, malformed,
conflicting, or one distinct numeric value. After `run-app-cli.sh`, parse the log even
when `rc != 0`; append a valid version first, then call `fail_with "$rc"`. When `rc ==
0`, retain the existing fail-closed contract for every invalid output shape.

- [ ] **Step 6: Update action metadata**

Describe service IDs as alphanumeric and document that `fastly-version` may be
available after a later deployment failure. Do not weaken token or artifact boundaries.

- [ ] **Step 7: Run local action gates**

Run: `bash .github/actions/deploy-core/tests/run.sh`

Run: `shellcheck -e SC1091 .github/actions/*/scripts/*.sh .github/actions/deploy-core/tests/*.sh`

Expected: PASS.

- [ ] **Step 8: Commit action contracts**

```bash
git add .github/actions/fastly-common .github/actions/deploy-core/scripts/run-app-cli.sh .github/actions/deploy-fastly .github/actions/healthcheck-fastly .github/actions/rollback-fastly .github/actions/deploy-core/tests/run.sh
git commit -m "fix(actions): retain failed Fastly deploy versions"
```

### Task 8: Update lifecycle fakes and composite smoke coverage

**Files:**

- Modify: `.github/actions/deploy-core/tests/make-smoke-fixture.sh:126-206`
- Modify: `.github/actions/deploy-core/tests/make-fake-fastly-env.sh:1-284`
- Modify: `.github/actions/deploy-core/tests/assert-staged-calls.sh:1-110`
- Modify: `.github/actions/deploy-core/tests/assert-production-deploy.sh`
- Modify: `.github/actions/deploy-core/tests/assert-lost-version.sh`
- Modify: `.github/workflows/deploy-action.yml:403-501`

- [ ] **Step 1: Split store-free and store-aware smoke fixtures**

Add an explicit fixture mode for `[stores.config]`. Keep production credential-boundary,
cross-job, cache, and lost-version manifest-command smokes store-free. Opt config-push
and staged lifecycle fixtures into stores so the registered Fastly adapter owns those
deployments.

- [ ] **Step 2: Replace the fake staging twin with descriptor state**

Model one `ENVSEL1` store, descriptor entry create/readback, resource links per
version, clone inheritance, selected Config/KV/Secret resources, and stage/activate.
Retain legacy scoped/unscoped data only as migration input. Remove `STAGESEL1` and all
creation/linking of `edgezero_runtime_env_staging_dummyservice`.

- [ ] **Step 3: Rewrite staged call assertions**

Assert:

- no staging-twin store is created or linked;
- `edgezero_runtime_env -> ENVSEL1` remains;
- exact selected resources are linked;
- descriptor key equals
  `EDGEZERO__SERVICES__dummyservice__VERSIONS__42__ENV_V1`;
- canonical bytes include staging Config `__KEY` and selected store names;
- descriptor write/readback and final link verification precede stage; and
- `--comment`, update flags, and version threading remain correct.

- [ ] **Step 4: Run local smoke contracts**

Run: `bash .github/actions/deploy-core/tests/run.sh`

Run: `.github/actions/deploy-core/tests/check-action-pins.sh`

If installed, run: `actionlint -shellcheck='shellcheck -S warning'`

Expected: PASS. The composite jobs in `.github/workflows/deploy-action.yml` remain the
authoritative hosted smoke for action composition.

- [ ] **Step 5: Commit lifecycle fixtures**

```bash
git add .github/actions/deploy-core/tests .github/workflows/deploy-action.yml
git commit -m "test(actions): model version-scoped Fastly descriptors"
```

### Task 9: Align all user-facing deployment and migration documentation

**Files:**

- Modify: `docs/guide/deploy-github-actions.md`
- Modify: `docs/guide/adapters/fastly.md`
- Modify: `docs/guide/cli-reference.md`
- Modify: `docs/guide/manifest-store-migration.md`
- Modify: `docs/guide/blob-app-config-migration.md`
- Modify: `docs/guide/deploy-action-adoption.md`
- Modify: `.github/actions/deploy-core/tests/run.sh`

- [ ] **Step 1: Replace mutable selector-store explanations**

Document one physical `edgezero_runtime_env`, the internal
`EDGEZERO__SERVICES__<SERVICE_ID>__VERSIONS__<VERSION>__ENV_V1` key, canonical JSON,
prepare-before-publish ordering, exact store links, and fail-closed runtime behavior for
apps that declare stores. Remove staging-twin and one-service-owner guidance.

- [ ] **Step 2: Document the public environment contract and migration**

Keep canonical environment variable names with no service ID. Explain parent value >
manifest variable default > logical default. State that legacy service-scoped and
unscoped entries are inventory-only during deployment, old entries/twins remain for
rollback, and new runtimes never fall back to them. Preserve optional Secret Store
declarations and `__NAME` examples without including secret values.

- [ ] **Step 3: Correct the Trusted Server workflow example**

Show a preflight job that validates the requested domain and emits the exact GitHub
Environment name. The deploy job must use:

```yaml
needs: preflight
environment: ${{ needs.preflight.outputs.environment }}
```

Map `ts.example.com` to production and `staging.ts.example.com` to staging while
keeping `inputs.domain` as the actual Fastly/healthcheck hostname.

Add a persistent action/docs contract test in `deploy-core/tests/run.sh` that requires
the example to contain both
`environment: ${{ needs.preflight.outputs.environment }}` and a Fastly/healthcheck
`domain: ${{ inputs.domain }}` use, and rejects
`environment: ${{ inputs.domain }}`. This keeps the validated GitHub Environment name
distinct from the real hostname.

- [ ] **Step 4: Document dispatch, args, IDs, and recovery**

Describe provider-neutral ownership, unregistered manifest-command compatibility,
Fastly's reserved lifecycle flags, its managed allowlist, the common alphanumeric
service-ID rule, and recovery from a failed deploy using an emitted `fastly-version`.

- [ ] **Step 5: Remove obsolete manual and Viceroy examples**

Remove manual unscoped selector writes and staging-twin instructions. If retaining a
Viceroy example, seed the complete descriptor under an explicit local service/version
key rather than unscoped canonical entries.

- [ ] **Step 6: Run documentation gates**

Run: `npm run format` (working directory: `docs`)

Run: `npm run lint` (working directory: `docs`)

Run: `npm run build` (working directory: `docs`)

Run: `bash .github/actions/deploy-core/tests/run.sh`

Expected: PASS.

- [ ] **Step 7: Commit documentation**

```bash
git add docs/guide .github/actions/deploy-core/tests/run.sh
git commit -m "docs(fastly): explain version-scoped runtime selectors"
```

### Task 10: Run the full verification and review the final branch

**Files:**

- Verify: all modified files
- Preserve: `examples/app-demo/crates/app-demo-adapter-cloudflare/.dev.vars`

- [ ] **Step 1: Run formatting and focused action checks**

Run: `cargo fmt --all -- --check`

Run: `bash .github/actions/deploy-core/tests/run.sh`

Run: `shellcheck -e SC1091 .github/actions/*/scripts/*.sh .github/actions/deploy-core/tests/*.sh`

Expected: PASS.

- [ ] **Step 2: Run Rust workspace tests**

Run: `cargo test --workspace --all-targets`

Expected: PASS.

- [ ] **Step 3: Run lint and feature checks**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`

Run: `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`

Run: `cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly`

Expected: PASS.

- [ ] **Step 4: Run documentation checks**

Run: `npm run format` (working directory: `docs`)

Run: `npm run lint` (working directory: `docs`)

Run: `npm run build` (working directory: `docs`)

Expected: PASS.

- [ ] **Step 5: Compare the final diff with the approved design**

Use @superpowers:requesting-code-review. Verify every review finding is covered, no
provider-name branch entered the generic CLI, no descriptor is mutable, no old live or
staged version is changed by preparation, and no unrelated resource link is removed.

- [ ] **Step 6: Inspect repository state**

Run: `git status --short --branch`

Expected: only intended tracked changes/commits plus the untouched untracked
`.dev.vars` file.

- [ ] **Step 7: Update the issue and PR**

Rewrite the existing PR title/body around the final implementation, include the
problem, resulting production/staging behavior, migration rule, and validation
evidence. Push the reviewed commits, then monitor required checks and fix any failure
before reporting completion.
