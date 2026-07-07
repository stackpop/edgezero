# Migrating to the blob app-config

The blob app-config rewrite (spec §10) replaces per-leaf typed-config
storage with a single JSON envelope per `[stores.config]` key. The
runtime extractor swaps `#[secret]` field values for the resolved
secret automatically; handlers no longer call `config_store_default()`
or `secret_store.require_str(...)` by hand.

The cutover is **atomic**: there is no compatibility path. Pushing
once with the new CLI brings the store into the new shape.

## TL;DR

```sh
# Push your typed C as one envelope blob per environment.
<app-cli> config push --adapter <name>          # remote
<app-cli> config push --adapter <name> --local  # local emulator state

# Read it back, see the diff against your local TOML.
<app-cli> config diff --adapter <name>                  # default: unified diff
<app-cli> config diff --adapter <name> --format json    # for jq / CI gates
<app-cli> config diff --adapter <name> --exit-code      # CI-friendly: exit 1 if changes
```

In handlers, swap hand-managed reads for the new extractor:

```rust
// before
async fn greet(ctx: RequestContext) -> Result<Response, EdgeError> {
    let cfg: AppDemoConfig = ctx.config_store_default()?.get("app_config").await?;
    let api_token = ctx.secret_store_default()?.require_str(&cfg.api_token).await?;
    // ...
}

// after
use edgezero_core::extractor::AppConfig;

#[action]
async fn greet(AppConfig(cfg): AppConfig<AppDemoConfig>) -> Result<Response, EdgeError> {
    // cfg.api_token already holds the resolved secret value;
    // cfg.feature.new_checkout is typed as bool, etc.
}
```

## Why this change

The pre-blob model stored each leaf at a separate Config Store key
(`feature.new_checkout`, `service.timeout_ms`, `api_token`). Three
problems made that untenable as projects grew:

- **Drift detection was per-leaf.** Renaming a field in code left
  orphans in the store; finding them required a full key-by-key audit.
  The blob model embeds a SHA over the canonical-form `data`, so the
  whole config is one drift-detection unit.
- **Push wasn't atomic.** Pushing N leaves was N round trips; a
  failure mid-push left the store in a partially-updated state. The
  blob model writes ONE envelope per `[stores.config]` key.
- **Secret resolution was per-handler.** Every handler that read a
  `#[secret]` field had to remember to call `require_str`. The new
  `AppConfig<C>` extractor walks `C::SECRET_FIELDS` once and replaces
  each key NAME with the resolved value before handing `cfg` to the
  handler.

## What's in the blob

The pushed value is a single JSON envelope:

```json
{
  "version": 1,
  "generated_at": "2026-06-22T18:42:31Z",
  "sha256": "1f3a…",
  "data": {
    "api_token": "demo_api_token",
    "feature": { "new_checkout": false },
    "greeting": "hello",
    "service": { "timeout_ms": 1500 },
    "vault": "default"
  }
}
```

**`data`** carries every typed field VERBATIM — including `#[secret]`
fields. Per spec §3.3 Model A, the value at rest in a secret-bearing
field is the operator-supplied KEY NAME (`api_token = "demo_api_token"`,
`vault = "default"`); the runtime extractor reads those names from
`data` and swaps each one for the resolved secret value at request
time. **The blob never contains the resolved secret bytes.**

**`sha256`** covers `data` only in canonical form (sorted keys,
ryu-shortest floats, no trailing whitespace). `version` and
`generated_at` are NOT part of the hash. Two pushes ten minutes apart
with the same data produce blobs with identical `sha256` and different
`generated_at`; the skip-on-equal path catches the redundant write.

## Per-adapter mechanics

::: tip Adapter manifests are gitignored
Every mechanics section below refers to **your local `axum.toml` /
`wrangler.toml` / `fastly.toml` / `spin.toml` / `runtime-config.toml`**
— these are not committed; regenerate each via
`<app-cli> provision --adapter <name> --local` after cloning and
re-apply any operator edits locally. All five adapter manifests
follow the same gitignored-generated model — the scaffold-time
provision loop writes each on `edgezero new`, and the synthesiser
is the single source of truth.
:::

### Axum

The push writes to `.edgezero/local-config-<id>.json` next to your
`edgezero.toml`. The file is a JSON map of `{ "<key>": "<envelope_json>" }`;
the dev server reads the envelope through `AxumConfigStore::get`. There
is no `--local` flag because Axum's push IS always local.

### Cloudflare

The push shells out to `wrangler kv bulk put --namespace-id=<id> --remote`
with one entry: `(<key>, <envelope_json>)`. With `--local`, the same
command runs against `.wrangler/state` instead.

The bundled `edgezero` binary calls `wrangler` from your shell; your
local `wrangler.toml` selects the namespace (it's not committed —
regenerate via `<app-cli> provision --adapter cloudflare --local`).

### Fastly

The push uses `fastly config-store-entry update --upsert --stdin`
to write the envelope as the value of one Config Store entry.

**Oversized envelopes** are handled automatically. Fastly's per-entry
limit is 8,000 characters. If your envelope fits, it's stored
directly. Otherwise the adapter:

1. Splits the envelope JSON into UTF-8-safe chunks (target 7,000
   bytes each).
2. Writes each chunk under a content-addressed key:
   `<KEY>.__edgezero_chunks.<envelope_sha256>.<index>`.
3. Writes a JSON root pointer at `<KEY>` LAST — `{ edgezero_kind:
"fastly_config_chunks", version: 1, envelope_sha256, envelope_len,
data_sha256, chunks: [...] }`.

Reads (runtime, `config diff`, `config push` skip-on-equal) detect the
pointer shape automatically and reassemble the envelope. The chunking
is invisible to `AppConfig<C>` and to operators in normal cases. If
the pointer JSON itself exceeds 8,000 characters (extremely large
configs), push hard-errors before any platform write and asks you
to restructure into multiple typed config structs.

`config push --adapter fastly --local` mirrors the same shape into
your local `fastly.toml`'s `[local_server.config_stores.<id>.contents]`
table (the file is not committed — regenerate via `<app-cli>
provision --adapter fastly --local`). Dotted chunk keys are written
as literal TOML keys (quoted strings), NOT nested dotted-path tables
— so the local store layout matches the remote store layout.

### Spin

`config push --adapter spin --local` writes SQLite-directly into
`<spin.toml dir>/.spin/sqlite_key_value.db`, using the vendored
`spin_key_value` schema, at `(store=<platform>, key=<key>)`.
Your local `spin.toml` + `runtime-config.toml` (which pin the label
routing) are not committed — regenerate via `<app-cli> provision
--adapter spin --local` after cloning.

Without `--local`, the push targets whatever the Spin runtime config
declares for the store. The adapter mirrors the writer's four-branch
dispatch on the read side too:

| `runtime-config.toml` backend                 | `config diff` behaviour                                                                        |
| --------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `--local` flag                                | SQLite-direct read                                                                             |
| Manifest deploy targets Fermyon Cloud         | Returns `Unsupported` — Spin Cloud's key-value CLI has no `get`; remote read-back is not in v1 |
| `type = "redis"` / `"azure_cosmos"` / unknown | Errors with a pointer at the backend's native CLI (`redis-cli GET <key>`, etc.)                |
| `type = "spin"` (default)                     | SQLite-direct read, honouring `path` override                                                  |

Spin Cloud's `Unsupported` means `config diff --adapter spin` cannot
show you the remote state. `config push --adapter spin --yes` writes
unconditionally; without `--yes`, the push prompts on a TTY or exits
non-zero on a non-TTY (per spec §8.3's four-branch UX).

## Operator runbook

### First push of a new project

1. Provision the backing stores. Each adapter has its own; `edgezero
new` already ran the `--local` form for you when scaffolding, so
   the cloud form is only needed the first time you point at real
   platform resources:

   ```sh
   # Local form — regenerates the adapter manifest + env files.
   <app-cli> provision --adapter axum --local
   <app-cli> provision --adapter cloudflare --local
   <app-cli> provision --adapter fastly --local
   <app-cli> provision --adapter spin --local

   # Cloud form — creates real platform resources.
   <app-cli> provision --adapter axum         # no-op (file-based)
   <app-cli> provision --adapter cloudflare   # wrangler kv namespace create
   <app-cli> provision --adapter fastly       # fastly config-store create
   <app-cli> provision --adapter spin         # edits your local spin.toml in place
   ```

2. Pre-populate `[stores.secrets]`. Push doesn't write secret values
   into the config store — your handlers will fail with
   `ConfigOutOfDate` at runtime if the operator-supplied key name
   doesn't resolve.

   ```sh
   # Cloudflare (per spec §10.2)
   wrangler secret put demo_api_token --binding APP_SECRETS

   # Fastly
   fastly secret-store-entry create --store-id=<id> --name=demo_api_token --value=<value>

   # Spin local
   echo demo_api_token=<value> >> .env

   # Axum local
   EDGEZERO_SECRET_demo_api_token=<value> cargo run -p <app-cli> -- serve --adapter axum
   ```

3. Push the typed config:

   ```sh
   <app-cli> config push --adapter <name>
   ```

   The CLI prints an inline unified diff against the current remote
   state. `--yes` (`-y`) skips the consent prompt; `--no-diff`
   suppresses the render; `--dry-run` shows the diff and exits without
   writing.

### Per-environment key override

Spec 5.4 + 12.7: a single `<app-name>.toml` covers dev / staging /
production. To swap which blob the runtime reads:

```sh
# Push BOTH variants. Each lands at its own key.
<app-cli> config push --adapter <name> --key app_config
<app-cli> config push --adapter <name> --key app_config_staging
```

The override variable is `EDGEZERO__STORES__CONFIG__<ID>__KEY` --
double-underscore separators, upper-case `<ID>`. The runtime extractor
packs `default_key` into the `ConfigStoreBinding` at adapter init.
**Where you set the override depends on the platform's variable
mechanism.**

| Adapter        | Where to set `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY`                                                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Axum**       | Process env: `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging <app-cli> serve --adapter axum`                                                                   |
| **Cloudflare** | `.dev.vars` (local) or `wrangler.toml` `[vars]` (deployed) -- wrangler surfaces it to `env.var(...)` in the worker                                                           |
| **Spin**       | `[application.variables]` in `spin.toml` (defaulted) plus `SPIN_VARIABLE_EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging spin up` for a per-invocation override |
| **Fastly**     | A dedicated `edgezero_runtime_env` Config Store (Compute@Edge has no process env). See below.                                                                                |

#### Fastly specifically

Compute@Edge has no `std::env`, so EdgeZero reads runtime overrides
from a Fastly Config Store named `edgezero_runtime_env`. The store is
created automatically by `edgezero provision --adapter fastly`. After
provisioning:

```sh
# Look up the platform store id (matches by name).
fastly config-store list --json | jq -r '.[] | select(.name=="edgezero_runtime_env") | .id'

# Set the override.
fastly config-store-entry update \
  --store-id=<STORE-ID> \
  --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY \
  --value=app_config_staging \
  --upsert
```

Locally (Viceroy), the store lives in fastly.toml's
`[local_server.config_stores.edgezero_runtime_env]` block. If the
store is missing at runtime, EdgeZero logs a one-line warning to
Fastly logs (`Fastly Config Store 'edgezero_runtime_env' not found;
EDGEZERO__* runtime overrides will use baked-in defaults`) and falls
back to the binding's default id -- so the runtime keeps serving, but
your per-environment override is silently inactive until you provision.

### Drift detection in CI

```sh
# Exit non-zero if the deployed remote differs from your local TOML.
<app-cli> config diff --adapter <name> --exit-code --format json
```

`config diff`'s exit codes per spec Q10:

- `0` — no changes (or `--exit-code` not set).
- `1` — changes with `--exit-code` set.
- `2` — adapter / config / read error.

Pair with `--format json` for machine-readable output:

```json
{
  "local_sha256": "1f3a…",
  "remote_sha256": "a472…",
  "added": { "vault": "default" },
  "removed": {},
  "changed": {
    "feature.new_checkout": { "from": false, "to": true },
    "service.timeout_ms": { "from": 1500, "to": 2000 }
  }
}
```

### Cleanup of orphan per-leaf keys

After the cutover, your store may still hold the pre-blob per-leaf
entries (`feature.new_checkout`, `service.timeout_ms`, etc.) that
nothing reads. The blob model leaves them inert — they're not
referenced — but they consume store quota.

| Adapter    | Cleanup command                                                                                                                                                                                                                              |
| ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Axum       | `rm .edgezero/local-config-*.json` (the blob push writes a fresh file)                                                                                                                                                                       |
| Cloudflare | `wrangler kv bulk delete <tempfile.json> --namespace-id=<id> --remote` with the orphan keys listed                                                                                                                                           |
| Fastly     | `fastly config-store-entry delete --store-id=<id> --key=<orphan-key>` per key                                                                                                                                                                |
| Spin local | `sqlite3 .spin/sqlite_key_value.db "DELETE FROM spin_key_value WHERE store='<id>' AND key NOT IN ('app_config', 'app_config_staging', ...)"` -- preserve every key your runtime might select via `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY` |

Listing the orphans before deletion:

```sh
# Cloudflare
wrangler kv key list --namespace-id=<id> --remote | jq -r '.[].name'

# Fastly
fastly config-store-entry list --store-id=<id> --json | jq -r '.[].key'

# Spin
sqlite3 .spin/sqlite_key_value.db "SELECT key FROM spin_key_value WHERE store='<id>'"
```

A future `config gc --adapter <name>` will automate this; v1 is
manual on the rationale that orphan cleanup is best done with operator
oversight.

### Fastly chunk-pointer hygiene

If your envelope ever pushed as chunked, then later shrinks back
under 8,000 characters, the old chunks remain in the store unreferenced
(the root pointer is the only active commit record). To find them:

```sh
fastly config-store-entry list --store-id=<id> --json \
  | jq -r '.[].key | select(contains(".__edgezero_chunks."))'
```

They're safe to delete via the same per-key delete command above.
A future `config gc` will sweep them automatically.

## Troubleshooting

### `ConfigOutOfDate` at runtime

The runtime read either:

- couldn't find the blob key (push hasn't run for the deployed code
  revision),
- found a blob whose `data` shape doesn't match the deployed `C`
  (push ran for a different code revision), or
- couldn't resolve a `#[secret]` field's key name against the
  secret store (the operator-supplied key isn't pre-provisioned).

`Retry-After: 60` accompanies the response so callers know to back off.
Run `<app-cli> config push --adapter <name>` for the deployed code
revision to fix the first two cases; provision the missing secret to
fix the third.

### `config push` reports "this command requires a typed app-config struct"

You ran `edgezero config push` from the BUNDLED binary instead of your
project's typed downstream CLI. The bundled binary intentionally cannot
push (it has no typed `C` in scope). Run `<your-app>-cli config push`
instead — `edgezero new` generates this CLI for you.

### Local fastly.toml writer wipes old chunks on re-push

The local writer wholesale-replaces the per-store contents block on
every push so stale entries don't accumulate during dev work. This is
intentional (and STRONGER than the remote behaviour, where orphan
chunks remain inert). The runtime correctness property holds either
way: a read after push B reconstructs envelope B, not A.

## Reference

- Spec: [`docs/superpowers/specs/2026-06-16-blob-app-config.md`](https://github.com/stackpop/edgezero)
- Implementation plan: [`docs/superpowers/plans/2026-06-17-blob-app-config.md`](https://github.com/stackpop/edgezero)
- Extractor source: `crates/edgezero-core/src/extractor.rs`
- CLI push entry point: `crates/edgezero-cli/src/config.rs::run_config_push_typed`
- CLI diff entry point: `crates/edgezero-cli/src/diff.rs::run_config_diff_typed`
- Fastly chunk-pointer helper: `crates/edgezero-adapter-fastly/src/chunked_config.rs`
