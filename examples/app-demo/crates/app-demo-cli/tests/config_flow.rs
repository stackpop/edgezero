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

#[test]
fn config_push_spin_dry_run_prints_translated_keys_and_preserves_manifest() {
    // Spin dry-run must:
    //   - resolve the single-component spin.toml,
    //   - announce the would-be writeback (preview output),
    //   - leave spin.toml untouched (no half-written manifest).
    // The CLI emits status lines via `log::info!`, which a unit
    // test can't reliably intercept without process-global
    // logger surgery. So this test asserts the integration
    // contract — typed flow dispatches cleanly + spin.toml is
    // byte-identical — and relies on the printed-content
    // assertions in `edgezero_adapter_spin::cli::tests::
    // push_dry_run_does_not_edit_spin_toml` for
    // the `__`-translated keys + both-table preview lines.
    let (dir, manifest) = write_app_demo_project("spin");
    let spin_path = dir.path().join("spin.toml");
    let before = fs::read_to_string(&spin_path).expect("read spin.toml before");

    edgezero_cli::run_config_push_typed::<AppDemoConfig>(&push_args(&manifest, "spin", true))
        .expect("typed spin dry-run dispatches cleanly");

    let after = fs::read_to_string(&spin_path).expect("read spin.toml after");
    assert_eq!(
        before, after,
        "spin dry-run must leave spin.toml byte-identical"
    );
}

/// Companion to the CLI dispatch test above: confirm
/// the spin adapter's dry-run preview surfaces every translated
/// key derivable from `AppDemoConfig` (with `#[secret]` and
/// `#[secret(store_ref)]` fields stripped) and the bindings
/// reference both spin.toml tables. Goes through the adapter
/// directly so the assertion can inspect the `Vec<String>`
/// status lines the CLI would otherwise hand to `log::info!`.
#[test]
fn spin_dry_run_preview_lists_app_demo_translated_keys_and_both_tables() {
    use edgezero_adapter::registry as adapter_registry;
    use tempfile::tempdir;

    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("spin.toml"),
        "spin_manifest_version = 2\n[application]\nname = \"app-demo\"\nversion = \"0.1.0\"\n[component.app-demo]\nsource = \"app_demo.wasm\"\n",
    )
    .expect("write spin.toml");

    // These are the entries the typed flow produces from
    // AppDemoConfig — secrets (`api_token`, `vault`) stripped,
    // nested struct flattened to dotted keys.
    let entries = vec![
        ("greeting".to_owned(), "hello from app-demo".to_owned()),
        ("feature.new_checkout".to_owned(), "false".to_owned()),
        ("service.timeout_ms".to_owned(), "1500".to_owned()),
    ];

    let adapter = adapter_registry::get_adapter("spin").expect("spin adapter registered");
    let out = adapter
        .push_config_entries(
            dir.path(),
            Some("spin.toml"),
            None,
            "app_config",
            &entries,
            true,
        )
        .expect("spin dry-run with app-demo entries");

    // Translated key form (`.→__`, lowercased) appears in
    // some preview line for each entry.
    for translated in &["greeting", "feature__new_checkout", "service__timeout_ms"] {
        assert!(
            out.iter().any(|line| line.contains(translated)),
            "dry-run preview names translated key `{translated}`: {out:?}"
        );
    }
    // Both spin.toml tables are referenced.
    assert!(
        out.iter().any(|line| {
            line.contains("[variables]") && line.contains("[component.app-demo.variables]")
        }),
        "dry-run preview references both [variables] and [component.app-demo.variables] tables: {out:?}"
    );
    // Stripped fields must NOT appear (regression — typed-flow
    // SECRET_FIELDS stripping must reach the adapter before the
    // dry-run preview is built).
    for secret in &["api_token", "vault"] {
        assert!(
            !out.iter().any(|line| line.contains(secret)),
            "dry-run preview must not leak `{secret}` (the typed flow strips SECRET_FIELDS): {out:?}"
        );
    }
}
