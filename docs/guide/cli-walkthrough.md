# CLI Walkthrough

This walkthrough takes a brand-new project from `edgezero new myapp` through every CLI
command you'll use day-to-day: `auth`, `provision`, `config validate`, `config push`,
`build`, `deploy`. It's a companion to the [CLI reference](./cli-reference), which
documents each command exhaustively — this page tells the story of how they fit
together.

The full command surface in your generated `myapp-cli`:

```bash
myapp-cli build       # cargo build for a target adapter
myapp-cli deploy      # push to production (per-adapter)
myapp-cli serve       # local dev server (per-adapter)
myapp-cli new         # scaffold another project
myapp-cli auth        # sign in / out / status against the platform CLI
myapp-cli provision   # create the platform resources backing your stores
myapp-cli config validate  # typed validate of edgezero.toml + myapp.toml
myapp-cli config push      # typed push of myapp.toml to the platform config store
```

The default `edgezero` binary exposes the same commands but runs the **raw** validate /
push paths because it has no typed app-config struct in scope. Downstream CLIs upgrade
to the typed paths so `validator` rules, `#[secret]` / `#[secret(store_ref)]` checks,
and Spin's flat-namespace collision check all run.

## 1. Scaffold

```bash
edgezero new myapp
cd myapp
```

You get a Cargo workspace with one core crate, one CLI crate, and one adapter crate per
target (axum, cloudflare, fastly, spin). The CLI crate (`crates/myapp-cli`) wires
`myapp_core::config::MyappConfig` into the typed `config validate` / `config push`
paths — that's the whole reason a downstream CLI exists.

Adapter discovery is link-time. The scaffolder includes every adapter that's compiled
into the `edgezero-cli` binary you ran `new` from.

## 2. Sign in

```bash
myapp-cli auth login --adapter cloudflare    # → wrangler login
myapp-cli auth login --adapter fastly        # → fastly profile create
myapp-cli auth login --adapter spin          # → spin cloud login
myapp-cli auth login --adapter axum          # → no-op (no remote auth)
```

EdgeZero stores no credentials of its own. `auth` delegates to whatever the adapter
declares — typically a shell-out to the platform's native CLI. Per-project overrides
live in `edgezero.toml`:

```toml
[adapters.cloudflare.commands]
auth-login  = "./scripts/cf-login.sh"
auth-status = "wrangler whoami --json"
```

## 3. Provision platform resources

`provision` has two modes:

- **`provision --local`** — synthesises each adapter's baseline
  manifest (Cloudflare `wrangler.toml`, Fastly `fastly.toml`, Spin
  `spin.toml` + `runtime-config.toml`, Axum `axum.toml`) and merges
  per-store `[[kv_namespaces]]` / `[local_server.*]` /
  `key_value_stores` bindings + writes line-oriented `.env` /
  `.dev.vars` / `.edgezero/.env` files. **No cloud shell-outs.** The
  scaffolder runs this for every selected adapter as part of
  `edgezero new`, so you rarely invoke it by hand for a first-run
  project.
- **`provision` (no `--local`)** — creates the backing platform
  resources for real: `wrangler kv namespace create` (Cloudflare),
  `fastly <kind>-store create` (Fastly), or the Spin-manifest edits
  described below.

All adapter manifests (`axum.toml`, `wrangler.toml`, `fastly.toml`,
`spin.toml`, `runtime-config.toml`) are gitignored — teammates
regenerate them via `<app>-cli provision --adapter <name> --local`
after cloning. The scaffold-time provision loop writes each on
`edgezero new`, and provision is the single source of truth for
each generated file (no scaffold `.hbs` template for any adapter
manifest).

Once you've declared store ids in `edgezero.toml`:

```toml
[stores.kv]
ids = ["sessions", "cache"]
default = "sessions"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
```

…`provision` creates the backing resources on whichever adapter you target:

```bash
myapp-cli provision --adapter cloudflare --dry-run
myapp-cli provision --adapter cloudflare

# Regenerate the local manifest + env files (no cloud calls) —
# what the scaffolder ran for you on `edgezero new`.
myapp-cli provision --adapter cloudflare --local
```

Per-adapter behaviour:

- **axum** — local-only. Prints one note per declared store id (KV is in-memory; config
  reads `.edgezero/local-config-<id>.json`; secrets read env vars).
- **cloudflare** — for each KV / config id, shells out to:

  ```bash
  wrangler kv namespace create <platform-name>
  ```

  where `<platform-name>` resolves from `EDGEZERO__STORES__<KIND>__<ID>__NAME`
  and falls back to the logical `<id>`. Parses the namespace id from stdout
  and appends `[[kv_namespaces]] binding = "<platform-name>", id = "<extracted>"`
  to `wrangler.toml`. Idempotent on the binding name. Secrets are runtime-managed
  via `wrangler secret put` — no-op here.

- **fastly** — for each id, shells out to:

  ```bash
  fastly <kind>-store create --name=<platform-name>
  ```

  using the same `<platform-name>` resolution, then appends the
  `[setup.<kind>_stores.<platform-name>]` block to `fastly.toml`.
  Idempotent on the `[setup.*]` block presence. Local-mode Viceroy
  state (`[local_server.<kind>_stores.<platform-name>]`) is owned by
  `provision --local`; the cloud form doesn't touch it.

- **spin** — pure `spin.toml` editing (no shell-out — Spin KV stores are runtime-resolved
  by the Fermyon stack). For each KV id AND each `[stores.config]` id (both KV-backed
  at runtime since the KV-config migration), appends the platform-resolved label to
  the resolved `[component.<component>].key_value_stores = [...]` array. Secrets stay
  manual — see [§5 Spin manual secret declarations](#5-spin-manual-secret-declarations).

If your `spin.toml` declares more than one `[component.*]`, set
`[adapters.spin.adapter].component = "<id>"` in `edgezero.toml` so `provision` knows
which component receives the labels.

## 4. Validate

Before pushing config, validate the manifest + typed app-config against each adapter's
contract:

```bash
myapp-cli config validate --strict
```

This runs:

- TOML / schema checks on `edgezero.toml` and `myapp.toml`.
- Typed deserialise into `MyappConfig` + `validator::Validate::validate()`.
- `#[secret]` field presence + non-empty + `[stores.secrets]` declared.
- `#[secret(store_ref)]` value is one of `[stores.secrets].ids`.
- Spin `[component.*]` discovery + within-`#[secret]` flat-namespace
  collision check — if `spin` is in your declared adapter set. (Spin
  config keys live in KV and accept arbitrary UTF-8; only secret
  values still share the variable namespace.)
- `--strict` adds capability-aware completeness (rejects e.g. multi-id
  `[stores.secrets]` when Spin is targeted, since Spin is
  Single-capable for secrets).

The default `edgezero` binary runs the same checks except the typed ones (it has no
`MyappConfig` to deserialise into). Use the typed flow for the strongest signal.

## 5. Push config

```bash
myapp-cli config push --adapter axum --dry-run
myapp-cli config push --adapter axum
```

Typed push runs the strict pre-flight validation, serialises `MyappConfig` via
`serde_json`, **strips every `#[secret]` and `#[secret(store_ref)]` top-level field**
(runtime store ids and secret values both belong out of the config-store payload),
flattens nested structs into dotted keys (`service.timeout_ms`), JSON-encodes arrays as
single string values, and pushes per-adapter:

- **axum** — writes the flat `string -> string` JSON object to
  `.edgezero/local-config-<id>.json` (the same file `AxumConfigStore` reads back at
  runtime).
- **cloudflare** — reads the namespace id from `wrangler.toml` (matched by binding =
  `<platform-name>`, resolved from `EDGEZERO__STORES__CONFIG__<ID>__NAME` or the
  logical `<id>`; errors with "did you run `provision`?" if absent), writes the
  entries to a temp file in wrangler's bulk format, then runs:

  ```bash
  wrangler kv bulk put <tempfile> --namespace-id=<id>
  ```

- **fastly** — resolves the platform config-store id on demand via
  `fastly config-store list --json` (matched by `name = <platform-name>`, resolved
  the same way), then per entry:

  ```bash
  fastly config-store-entry create --store-id=<id> --key=<k> --value=<v>
  ```

- **spin** — reads `runtime-config.toml` (next to `spin.toml` by
  default; override with `--runtime-config <path>`) to dispatch
  per-backend. Decision order:
  1. `--local` forces SQLite-direct against
     `<spin.toml dir>/.spin/sqlite_key_value.db`. Non-`default` labels
     still require a `[key_value_store.<label>]` stanza in
     runtime-config.toml — without it, the dispatcher refuses the
     push and tells you the exact stanza to add, since the file you'd
     write would be unreadable from a running `spin up`.
  2. If the manifest's `[adapters.spin.commands].deploy` shells to
     `spin deploy` / `spin cloud deploy`, push batches entries into
     `spin cloud key-value set --app <APP> --label <LABEL>
KEY=VALUE [KEY=VALUE …]` invocations (one shellout per
     ≤96 KiB argv chunk, ≥1000 entries per invocation). `<APP>`
     comes from `[application].name` in spin.toml; `<LABEL>` is the
     env-resolved platform label per Fermyon's
     [app-scoped label model](https://developer.fermyon.com/cloud/linking-applications-to-resources-using-labels).
     Pre-link the label to a cloud KV store with
     `spin cloud link key-value` (or the dashboard) before the
     first push; authenticate first via `spin cloud login`.
  3. Otherwise dispatch on `runtime-config.toml`'s
     `[key_value_store.<label>].type`: `type = "spin"` → SQLite-direct
     write (stanza required for non-`default` labels); `type =
"redis"` / `azure_cosmos` / unknown → clear error pointing at
     the backend's native CLI (e.g. `redis-cli -u <url>
SET <key> <value>`).
  4. Default: SQLite-direct at Spin's `.spin/sqlite_key_value.db`,
     but ONLY for the `default` label (Spin auto-provides). Other
     labels require a stanza per point 1.

  No internet-facing endpoint is involved on the EdgeZero side: the
  SQLite writer opens the file directly via `rusqlite` (using Spin's
  exact `spin_key_value` schema, vendored from upstream + drift-tested
  at build time), and the cloud writer shells out to the official
  Fermyon plugin.

### Spin manual secret declarations

`config push` never writes secret variables — `#[secret]` fields are stripped before
push, and a `#[secret(store_ref)]` field's runtime key is code-local (e.g.
`ctx.secret_store(&cfg.vault)?.require_str("active")`), so the CLI cannot infer it.
Declare them manually in `spin.toml`:

```toml
[variables]
api_token = { required = true, secret = true }  # the #[secret] field

[component.myapp.variables]
api_token = "{{ api_token }}"
```

Then set the value at run time via `SPIN_VARIABLE_API_TOKEN=<value>` or
`spin up --env API_TOKEN=<value>`.

## 6. Env-var overlay

Every key in `myapp.toml` can be overridden at load time by an
`<APP_NAME>__…__<KEY>` environment variable, where `<APP_NAME>` is the
manifest's `[app].name` uppercased with `-` → `_`. For an app named `myapp`
the prefix is `MYAPP__`; for `my-app` it would be `MY_APP__`. Dotted config
keys are joined with `__`. The overlay applies to both `config validate`
and `config push` so the values you see match the runtime:

```bash
# myapp.toml: service.timeout_ms = 1500
MYAPP__SERVICE__TIMEOUT_MS=5000 myapp-cli config push --adapter axum
# .edgezero/local-config-app_config.json now has "service.timeout_ms": "5000"
```

Pass `--no-env` to skip the overlay (useful when CI builds want the on-disk
values verbatim). Setting the lowercase / source-form spelling
(`myapp__...`) is silently ignored at runtime — the prefix must be the
normalised form.

## 7. Build + deploy

```bash
myapp-cli build  --adapter cloudflare
myapp-cli deploy --adapter cloudflare
```

`build` runs the compiled `[adapters.<name>.commands].build` (or falls back to the
adapter's built-in builder). `deploy` does the same for the deploy command. Native
`axum` has no remote deploy — use standard container/binary deployment instead.

## 8. The full loop in one go

For a Cloudflare-targeted project:

```bash
edgezero new myapp && cd myapp
myapp-cli auth login --adapter cloudflare
myapp-cli provision --adapter cloudflare
myapp-cli config validate --strict
myapp-cli config push --adapter cloudflare
myapp-cli build --adapter cloudflare
myapp-cli deploy --adapter cloudflare
```

For Spin (which has the most manual setup because of secret variables):

```bash
edgezero new myapp && cd myapp
# Add manual secret declarations to crates/myapp-adapter-spin/spin.toml first
# (see "Spin manual secret declarations" above)
myapp-cli auth login --adapter spin
myapp-cli provision --adapter spin
myapp-cli config validate --strict
myapp-cli config push --adapter spin
myapp-cli build --adapter spin
SPIN_VARIABLE_API_TOKEN=<your_token> myapp-cli deploy --adapter spin
```

For local dev (axum), the flow is simpler — no auth, no provision, just push + serve:

```bash
myapp-cli config push --adapter axum
myapp-cli serve --adapter axum
```

## Migrating from the pre-rewrite manifest

If you're upgrading a project from the pre-Stage-2 manifest schema (`[stores.kv] name =
"..."`, `[stores.config.defaults]`, `[adapters.<name>.stores.*]`), see
[the migration guide](./manifest-store-migration). The pre-rewrite fields are now a
hard load error — every project must migrate.

## Next Steps

- [CLI reference](./cli-reference) — every flag, exit code, and per-adapter behaviour.
- [Configuration](./configuration) — `edgezero.toml` schema in detail.
- [Manifest store migration](./manifest-store-migration) — pre- → post-rewrite mapping.
