//! End-to-end proof that nested and array `#[secret]` fields resolve
//! through the runtime secret walk of the `AppConfig<C>` extractor.
//!
//! Unlike `app_config_derive.rs` (which only reflects over the metadata
//! `secret_fields()` emits), this test drives the WHOLE chain a downstream
//! app hits at request time: a `BlobEnvelope` holding secret-store KEY NAMES
//! is deserialized through `AppConfig::<C>::from_request`, whose `secret_walk`
//! resolves every nested / array / named-store leaf against a live
//! `InMemorySecretStore` before the struct is materialised. The assertions
//! read the RESOLVED values off the deserialized config.

#![cfg(test)]

use async_trait::async_trait;
use edgezero_core::blob_envelope::BlobEnvelope;
use edgezero_core::body::Body;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
use edgezero_core::context::RequestContext;
use edgezero_core::extractor::{AppConfig as AppConfigExtractor, FromRequest as _};
use edgezero_core::http::{Method, request_builder};
use edgezero_core::params::PathParams;
use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, SecretRegistry, StoreRegistry,
};
use futures::executor::block_on;
use std::collections::BTreeMap;
use std::sync::Arc;
use validator::Validate as _;

// --- fixture config (nested objects + `Vec<_>` array + named store) --------

// A 2-level `KeyInDefault` nested leaf: `datadome.server_side_key`.
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct DataDome {
    #[secret]
    server_side_key: String,
}

// One array element carrying a `KeyInDefault` secret: `partners[*].api_key`.
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct Partner {
    #[secret]
    api_key: String,
}

// The root config, exercising every reachable secret shape at once.
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[app_config(nested)]
    #[validate(nested)]
    datadome: DataDome,
    #[app_config(nested)]
    #[validate(nested)]
    partners: Vec<Partner>,
    #[app_config(nested)]
    #[validate(nested)]
    vaulted: Vaulted,
}

// A nested `KeyInNamedStore` leaf whose `store_ref` sibling (`vault`) lives in
// the SAME inner struct — the innermost-parent scoping rule.
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct Vaulted {
    #[secret(store_ref = "vault")]
    token: String,
    #[secret(store_ref)]
    vault: String,
}

// --- wiring helpers ---------------------------------------------------------

// A minimal `ConfigStore` that returns one fixed blob-envelope string,
// mirroring the hand-written stores in `extractor.rs`'s own tests.
struct BlobStore(String);

#[async_trait(?Send)]
impl ConfigStore for BlobStore {
    async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
        Ok(Some(self.0.clone()))
    }
}

// Build a `RequestContext` wired to a config store holding `envelope` plus a
// two-store secret registry: the default store and a `named` store, so both
// the `KeyInDefault` leaves and the `KeyInNamedStore` leaf resolve.
fn ctx_with_stores(envelope: String) -> RequestContext {
    let binding = ConfigStoreBinding {
        handle: ConfigStoreHandle::new(Arc::new(BlobStore(envelope))),
        default_key: "app_config".to_owned(),
    };
    let config_registry: ConfigRegistry =
        StoreRegistry::single_id("app_config".to_owned(), binding);

    // Default store resolves the `KeyInDefault` leaves (nested + array).
    let default_store = InMemorySecretStore::new([
        ("default/dd_key".to_owned(), "DD"),
        ("default/p0_key".to_owned(), "P0"),
        ("default/p1_key".to_owned(), "P1"),
    ]);
    let default_bound = BoundSecretStore::new(
        SecretHandle::new(Arc::new(default_store)),
        "default".to_owned(),
    );

    // Named store resolves the `KeyInNamedStore` leaf via its `vault` sibling.
    let named_store = InMemorySecretStore::new([("named/tok_key".to_owned(), "TOK")]);
    let named_bound =
        BoundSecretStore::new(SecretHandle::new(Arc::new(named_store)), "named".to_owned());

    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    by_id.insert("default".to_owned(), default_bound);
    by_id.insert("named".to_owned(), named_bound);
    let secret_registry: SecretRegistry = StoreRegistry::new(by_id, "default".to_owned());

    let mut request = request_builder()
        .method(Method::GET)
        .uri("/config")
        .body(Body::empty())
        .expect("build request");
    request.extensions_mut().insert(config_registry);
    request.extensions_mut().insert(secret_registry);
    RequestContext::new(request, PathParams::default())
}

// The blob at rest holds secret-store KEY NAMES (Model A), not resolved
// values — exactly what `config push` persists.
fn envelope_with_key_names() -> String {
    let data = serde_json::json!({
        "datadome": { "server_side_key": "dd_key" },
        "partners": [ { "api_key": "p0_key" }, { "api_key": "p1_key" } ],
        "vaulted": { "token": "tok_key", "vault": "named" }
    });
    let envelope = BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned());
    serde_json::to_string(&envelope).expect("serialise envelope")
}

// --- the end-to-end assertions ---------------------------------------------

#[test]
fn nested_and_named_store_secrets_resolve_through_extractor() {
    let ctx = ctx_with_stores(envelope_with_key_names());
    let AppConfigExtractor(cfg) =
        block_on(AppConfigExtractor::<Settings>::from_request(&ctx)).expect("extraction succeeds");

    // Nested `KeyInDefault`: `datadome.server_side_key` -> default store.
    assert_eq!(cfg.datadome.server_side_key, "DD");

    // Nested `KeyInNamedStore`: `vaulted.token` -> the store named by its
    // sibling `vaulted.vault` ("named"). The `store_ref` sibling is left
    // verbatim (it names a store, not a secret).
    assert_eq!(cfg.vaulted.token, "TOK");
    assert_eq!(cfg.vaulted.vault, "named");
}

#[test]
fn array_element_secrets_resolve_per_index() {
    let ctx = ctx_with_stores(envelope_with_key_names());
    let AppConfigExtractor(cfg) =
        block_on(AppConfigExtractor::<Settings>::from_request(&ctx)).expect("extraction succeeds");

    // Each `partners[n].api_key` resolves independently against the default
    // store — proving the `ArrayEach` runtime walk.
    assert_eq!(cfg.partners.len(), 2);
    assert_eq!(cfg.partners[0].api_key, "P0");
    assert_eq!(cfg.partners[1].api_key, "P1");
}
