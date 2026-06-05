//! Integration tests — drive `edgezero-cli`'s typed flows through
//! `AppDemoConfig`, the downstream-CLI surface this example exists
//! to exercise.
//!
//! These tests construct an app-demo-shaped manifest + config in a
//! tempdir rather than pointing at the in-repo `examples/app-demo/`
//! files, so a writeback test never corrupts the example checked
//! into git. The env-overlay path is already covered by a unit test
//! in `app-demo-core/src/config.rs`.

#![cfg(test)]

use app_demo_core::config::AppDemoConfig;
use edgezero_cli::args::{ConfigPushArgs, ConfigValidateArgs};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// `AppDemoConfig`-shaped TOML — exercises every field the macro
/// emits: `greeting` (plain), `feature.new_checkout` (nested),
/// `service.timeout_ms` (nested numeric), `api_token` (`#[secret]`,
/// must be stripped from push payloads), `vault`
/// (`#[secret(store_ref)]`, must also be stripped — it names a
/// runtime store id, not a payload value).
const APP_DEMO_CONFIG: &str = r#"
api_token = "demo_api_token"
greeting = "hello from app-demo"
vault = "default"

[feature]
new_checkout = false

[service]
timeout_ms = 1500
"#;

/// Minimal `edgezero.toml` with axum + spin adapters, a single
/// config store id, and a secrets section so the typed validator's
/// `#[secret]` checks pass. We don't include cloudflare/fastly
/// because the push tests don't dispatch to them, and the spin
/// section needs its own `spin.toml` companion (written per-test).
fn manifest_for_adapter(adapter: &str) -> String {
    let adapter_block = match adapter {
        "axum" => {
            r#"[adapters.axum.adapter]
crate = "crates/app-demo-adapter-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#
        }
        "spin" => {
            r#"[adapters.spin.adapter]
crate = "crates/app-demo-adapter-spin"
manifest = "spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#
        }
        other => panic!("unsupported adapter in fixture: {other}"),
    };
    format!(
        r#"
[app]
name = "app-demo"
entry = "crates/app-demo-core"

{adapter_block}
[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#
    )
}

fn write_app_demo_project(adapter: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest_path = dir.path().join("edgezero.toml");
    fs::write(&manifest_path, manifest_for_adapter(adapter)).expect("write manifest");
    fs::write(dir.path().join("app-demo.toml"), APP_DEMO_CONFIG).expect("write app config");
    if adapter == "spin" {
        // The spin push needs a single-component spin.toml the
        // resolver can locate. A bare scaffold is enough — the
        // adapter only cares about [component.*] discovery and
        // never reads source/wasm paths during push.
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"app-demo\"\nversion = \"0.1.0\"\n[component.app-demo]\nsource = \"app_demo.wasm\"\n",
        )
        .expect("write spin.toml");
    }
    (dir, manifest_path)
}

// `ConfigValidateArgs` / `ConfigPushArgs` are `#[non_exhaustive]`,
// so an out-of-crate test can't use the struct-literal form.
// `Default::default()` + field assignment is the supported path.
fn validate_args(manifest: &Path, strict: bool) -> ConfigValidateArgs {
    let mut args = ConfigValidateArgs::default();
    args.manifest = manifest.to_path_buf();
    args.no_env = true; // isolate from any ambient APP_DEMO__* env vars
    args.strict = strict;
    args
}

fn push_args(manifest: &Path, adapter: &str, dry_run: bool) -> ConfigPushArgs {
    let mut args = ConfigPushArgs::default();
    args.adapter = adapter.to_owned();
    args.manifest = manifest.to_path_buf();
    args.no_env = true;
    args.dry_run = dry_run;
    args
}

#[test]
fn config_validate_strict_passes_against_app_demo_config() {
    // Typed validator runs the raw checks (manifest schema, store
    // declarations) plus the typed `#[secret]` / store-ref
    // checks. `--strict` adds capability-aware completeness. The
    // fixture is the shape `app-demo` ships with — this test
    // catches any drift between AppDemoConfig and the validator
    // contract.
    let (_dir, manifest) = write_app_demo_project("axum");
    edgezero_cli::run_config_validate_typed::<AppDemoConfig>(&validate_args(&manifest, true))
        .expect("typed --strict validate must pass against the demo shape");
}

#[test]
fn config_push_axum_writes_local_config_json_without_secrets() {
    // The typed push must strip BOTH `#[secret]` (`api_token`)
    // and `#[secret(store_ref)]` (`vault`) before writing —
    // runtime store ids and secret values both belong out of
    // the config-store payload.
    let (dir, manifest) = write_app_demo_project("axum");
    edgezero_cli::run_config_push_typed::<AppDemoConfig>(&push_args(&manifest, "axum", false))
        .expect("typed axum push succeeds");

    let written = dir.path().join(".edgezero/local-config-app_config.json");
    let raw = fs::read_to_string(&written).expect("axum push wrote the local-config file");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");

    assert_eq!(parsed["greeting"], "hello from app-demo");
    assert_eq!(parsed["feature.new_checkout"], "false");
    assert_eq!(parsed["service.timeout_ms"], "1500");
    assert!(
        parsed.get("api_token").is_none(),
        "`#[secret]` field must be stripped from axum push: {parsed}"
    );
    assert!(
        parsed.get("vault").is_none(),
        "`#[secret(store_ref)]` field must be stripped from axum push: {parsed}"
    );
}

#[test]
fn config_push_axum_round_trip_serves_pushed_value_via_handler() {
    // The spec-intent half of "config push --adapter axum writes
    // the file AND a running demo server returns greeting on
    // /config/greeting". We skip the HTTP transport (axum's own
    // contract tests cover that) and verify the data contract that
    // actually matters for app-demo: the JSON `config push` writes
    // is exactly the payload `AxumConfigStore` reads back, and the
    // demo's
    // `config_get` handler dispatched against that store
    // surfaces the value. A full subprocess-server lifecycle
    // (ephemeral port + readiness + RAII teardown) would add
    // significant complexity for the same end-to-end coverage.
    use app_demo_core::handlers::config_get;
    use edgezero_adapter_axum::config_store::AxumConfigStore;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::ConfigStoreHandle;
    use edgezero_core::context::RequestContext;
    use edgezero_core::http::{request_builder, Method, StatusCode};
    use edgezero_core::params::PathParams;
    use edgezero_core::store_registry::{ConfigRegistry, StoreRegistry};
    use futures::executor::block_on;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    let (dir, manifest) = write_app_demo_project("axum");
    edgezero_cli::run_config_push_typed::<AppDemoConfig>(&push_args(&manifest, "axum", false))
        .expect("typed axum push succeeds");

    // Load the JSON the push just wrote via the SAME loader the
    // axum runtime uses — this is the contract test: file format
    // must match the reader's expectations.
    let local_config_path = dir.path().join(".edgezero/local-config-app_config.json");
    let store = AxumConfigStore::from_path(&local_config_path).expect("AxumConfigStore loads");
    let handle = ConfigStoreHandle::new(Arc::new(store));
    let by_id: BTreeMap<String, ConfigStoreHandle> =
        [("app_config".to_owned(), handle)].into_iter().collect();
    let registry: ConfigRegistry = StoreRegistry::new(by_id, "app_config".to_owned());

    // Build a /config/greeting request and dispatch the demo's
    // config_get handler — same dispatch path the wasm router
    // would invoke at runtime.
    let mut request = request_builder()
        .method(Method::GET)
        .uri("/config/greeting")
        .body(Body::empty())
        .expect("build request");
    request.extensions_mut().insert(registry);
    let mut params = HashMap::new();
    params.insert("name".to_owned(), "greeting".to_owned());
    let ctx = RequestContext::new(request, PathParams::new(params));

    let response = block_on(config_get(ctx)).expect("config_get handler ok");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().into_bytes().expect("buffered");
    assert_eq!(
        body.as_ref(),
        b"hello from app-demo",
        "handler must serve the value `config push` wrote"
    );
}

/// Spin push round-trip against the SQLite-direct backend: the
/// typed flow drives `config push --adapter spin` against a temp
/// project whose `runtime-config.toml` selects `type = "spin"` for
/// `app_config`. After the push, opening the resulting
/// `.spin/sqlite_key_value.db` via `rusqlite` MUST return the
/// flattened, secret-stripped entries — same shape Spin's runtime
/// would read on the next request.
#[test]
fn config_push_spin_writes_sqlite_round_tripped_via_rusqlite() {
    let (dir, manifest) = write_app_demo_project("spin");

    // Drop in a `runtime-config.toml` next to the spin manifest so
    // the per-backend dispatcher knows `app_config` is SQLite-backed.
    // `write_app_demo_project` writes spin.toml at the same root, so
    // both files live next to each other.
    let spin_manifest_dir = dir.path();
    fs::write(
        spin_manifest_dir.join("runtime-config.toml"),
        "[key_value_store.app_config]\ntype = \"spin\"\n",
    )
    .expect("write runtime-config.toml");

    edgezero_cli::run_config_push_typed::<AppDemoConfig>(&push_args(&manifest, "spin", false))
        .expect("typed spin push dispatches cleanly");

    // The dispatcher resolves the SQLite path to
    // `<spin_manifest_dir>/.spin/sqlite_key_value.db`. Open it and
    // pull the entries Spin's runtime would see at request time.
    let db_path = spin_manifest_dir.join(".spin/sqlite_key_value.db");
    assert!(
        db_path.exists(),
        "push must create the SQLite file at {}",
        db_path.display()
    );
    let connection = rusqlite::Connection::open(&db_path).expect("open written sqlite");
    let entries: BTreeMap<String, String> = connection
        .prepare("SELECT key, value FROM spin_key_value WHERE store = ?1")
        .expect("prepare")
        .query_map(rusqlite::params!["app_config"], |row| {
            let key: String = row.get(0)?;
            let value: Vec<u8> = row.get(1)?;
            Ok((
                key,
                String::from_utf8(value).expect("value is utf-8 from typed push"),
            ))
        })
        .expect("query_map")
        .collect::<Result<_, _>>()
        .expect("rows");

    assert_eq!(
        entries.get("greeting").map(String::as_str),
        Some("hello from app-demo"),
        "greeting landed verbatim: {entries:?}"
    );
    // Nested fields flatten with dotted keys.
    assert_eq!(
        entries.get("service.timeout_ms").map(String::as_str),
        Some("1500"),
        "nested fields land as `<table>.<field>`: {entries:?}"
    );
    // Boolean / non-string scalars become their TOML / JSON
    // representation (typed flow uses serde_json::to_string for the
    // flattened values).
    assert_eq!(
        entries.get("feature.new_checkout").map(String::as_str),
        Some("false"),
        "boolean flattens to its string repr: {entries:?}"
    );

    // SECRET stripping: `#[secret]` field (api_token) and
    // `#[secret(store_ref)]` field (vault) MUST NOT appear in the
    // pushed entries.
    assert!(
        !entries.contains_key("api_token"),
        "#[secret] field must be stripped from push payload: {entries:?}"
    );
    assert!(
        !entries.contains_key("vault"),
        "#[secret(store_ref)] field must be stripped from push payload: {entries:?}"
    );
}
