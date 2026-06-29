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
/// the value at rest is the secret-store KEY NAME — Model A per
/// spec 3.3), `vault` (`#[secret(store_ref)]`, the value at rest
/// is the secret-store ID; the runtime extractor's `secret_walk`
/// reads both verbatim from the blob and resolves `api_token`
/// against the secret store).
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
    // Tests run in non-TTY CI; bypass the interactive consent gate.
    args.yes = true;
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
fn config_push_axum_writes_local_config_json_with_secret_key_names() {
    // The typed push writes a blob envelope under the logical store id
    // key. Per spec 3.3 (Model A) the envelope's `data` carries every
    // field VERBATIM — including `#[secret]` (`api_token`, the
    // secret-store key NAME) and `#[secret(store_ref)]` (`vault`, the
    // secret-store id). The runtime `secret_walk` reads those names
    // at request time and swaps each for the resolved value.
    use edgezero_core::blob_envelope::{BlobEnvelope, ENVELOPE_VERSION_V1};

    let (dir, manifest) = write_app_demo_project("axum");
    edgezero_cli::run_config_push_typed::<AppDemoConfig>(&push_args(&manifest, "axum", false))
        .expect("typed axum push succeeds");

    // The file is `{ "app_config": "<envelope_json_string>" }`.
    let written = dir.path().join(".edgezero/local-config-app_config.json");
    let raw = fs::read_to_string(&written).expect("axum push wrote the local-config file");
    let outer: serde_json::Value = serde_json::from_str(&raw).expect("valid outer JSON");
    let envelope_str = outer["app_config"]
        .as_str()
        .expect("outer key must be the logical store id `app_config`");
    let envelope: BlobEnvelope =
        serde_json::from_str(envelope_str).expect("envelope JSON must parse");

    // Envelope integrity.
    envelope.verify().expect("envelope SHA must verify");
    assert_eq!(envelope.version, ENVELOPE_VERSION_V1);

    // Non-secret fields survive in `data` (nested, not flattened).
    let data = &envelope.data;
    assert_eq!(data["greeting"], "hello from app-demo");
    assert_eq!(data["feature"]["new_checkout"], false);
    assert_eq!(data["service"]["timeout_ms"], 1500_i64);

    // Model A: secret-bearing fields persist their KEY NAMES at rest.
    // The runtime extractor's `secret_walk` reads each name from the
    // blob and resolves it against the configured secret store.
    assert_eq!(
        data["api_token"], "demo_api_token",
        "`#[secret]` field must persist its key NAME at rest: {data}"
    );
    assert_eq!(
        data["vault"], "default",
        "`#[secret(store_ref)]` field must persist its store id at rest: {data}"
    );
}

#[test]
fn config_typed_handler_deserialises_blob_envelope_to_greeting() {
    // Verify the blob-model contract end-to-end: the `config_typed`
    // handler uses `AppConfig<AppDemoConfig>` to read the envelope,
    // perform the secret walk (resolving `api_token`), and return
    // `cfg.greeting`. The blob is hand-constructed to include the
    // `api_token` key name — Model A stores the secret KEY NAME in
    // the blob, and the extractor resolves it via the secret store.
    use app_demo_core::handlers::config_typed;
    use edgezero_adapter_axum::config_store::AxumConfigStore;
    use edgezero_core::blob_envelope::BlobEnvelope;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::ConfigStoreHandle;
    use edgezero_core::context::RequestContext;
    use edgezero_core::http::{request_builder, Method, StatusCode};
    use edgezero_core::params::PathParams;
    use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
    use edgezero_core::store_registry::{
        BoundSecretStore, ConfigRegistry, ConfigStoreBinding, SecretRegistry, StoreRegistry,
    };
    use futures::executor::block_on;
    use std::sync::Arc;

    // Hand-construct a blob envelope with api_token = "demo_api_token"
    // (the key name, not the secret value). AxumConfigStore stores it
    // under key "app_config" — the binding's default_key.
    let data = serde_json::json!({
        "api_token": "demo_api_token",
        "greeting": "hello from app-demo",
        "vault": "default",
        "feature": { "new_checkout": false },
        "service": { "timeout_ms": 1500_u32 }
    });
    let envelope = BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned());
    let blob_str = serde_json::to_string(&envelope).expect("envelope JSON");
    let store = AxumConfigStore::from_map([("app_config".to_owned(), blob_str)]);

    let config_registry: ConfigRegistry = StoreRegistry::single_id(
        "app_config".to_owned(),
        ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(store)),
            default_key: "app_config".to_owned(),
        },
    );

    // The secret walk resolves `api_token = "demo_api_token"` via the
    // default secret store. InMemorySecretStore keys are `"<store>/<key>"`.
    let provider =
        InMemorySecretStore::new([("default/demo_api_token".to_owned(), "resolved-api-token")]);
    let secret_registry: SecretRegistry = StoreRegistry::single_id(
        "default".to_owned(),
        BoundSecretStore::new(SecretHandle::new(Arc::new(provider)), "default".to_owned()),
    );

    let mut request = request_builder()
        .method(Method::GET)
        .uri("/config/typed")
        .body(Body::empty())
        .expect("build request");
    request.extensions_mut().insert(config_registry);
    request.extensions_mut().insert(secret_registry);
    let ctx = RequestContext::new(request, PathParams::default());

    let response = block_on(config_typed(ctx)).expect("config_typed handler ok");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().into_bytes().expect("buffered");
    assert_eq!(
        body.as_ref(),
        b"hello from app-demo",
        "AppConfig extractor must surface cfg.greeting from the blob envelope"
    );
}

/// Spin push round-trip against the SQLite-direct backend: the
/// typed flow drives `config push --adapter spin` against a temp
/// project whose `runtime-config.toml` selects `type = "spin"` for
/// `app_config`. After the push, opening the resulting
/// `.spin/sqlite_key_value.db` via `rusqlite` MUST return one blob-
/// envelope row whose `data` carries every typed field VERBATIM —
/// non-secret fields plus the secret-bearing `api_token` (key NAME)
/// and `vault` (store id) — same shape Spin's runtime reads on the
/// next request before `secret_walk` resolves the secrets.
#[test]
fn config_push_spin_writes_sqlite_round_tripped_via_rusqlite() {
    // The typed push writes a single blob-envelope row under key
    // `"app_config"` (the logical store id). Pull the raw value string.
    use edgezero_core::blob_envelope::{BlobEnvelope, ENVELOPE_VERSION_V1};

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
    let envelope_str: String = connection
        .query_row(
            "SELECT value FROM spin_key_value WHERE store = ?1 AND key = ?2",
            rusqlite::params!["app_config", "app_config"],
            |row| {
                let bytes: Vec<u8> = row.get(0)?;
                Ok(String::from_utf8(bytes).expect("value is utf-8"))
            },
        )
        .expect("blob row must exist after typed push");

    let envelope: BlobEnvelope =
        serde_json::from_str(&envelope_str).expect("envelope JSON must parse");

    // Envelope integrity.
    envelope.verify().expect("envelope SHA must verify");
    assert_eq!(envelope.version, ENVELOPE_VERSION_V1);

    // Non-secret data survives (nested, not flattened).
    let data = &envelope.data;
    assert_eq!(
        data["greeting"], "hello from app-demo",
        "greeting in envelope.data: {data}"
    );
    assert_eq!(
        data["service"]["timeout_ms"], 1500_i64,
        "nested numeric in envelope.data: {data}"
    );
    assert_eq!(
        data["feature"]["new_checkout"], false,
        "boolean in envelope.data: {data}"
    );

    // Model A (spec 3.3): the secret-bearing fields persist their
    // KEY NAMES at rest. `secret_walk` resolves them at request time.
    assert_eq!(
        data["api_token"], "demo_api_token",
        "`#[secret]` field must persist its key NAME at rest: {data}"
    );
    assert_eq!(
        data["vault"], "default",
        "`#[secret(store_ref)]` field must persist its store id at rest: {data}"
    );
}
