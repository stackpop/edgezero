#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::http::Request;
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::key_value_store::KvHandle;
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::secret_store::SecretHandle;

#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
pub(crate) fn insert_store_handles(
    request: &mut Request,
    kv_handle: Option<KvHandle>,
    secret_handle: Option<SecretHandle>,
) {
    if let Some(handle) = kv_handle {
        request.extensions_mut().insert(handle);
    }

    if let Some(handle) = secret_handle {
        request.extensions_mut().insert(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::request_builder;
    use edgezero_core::key_value_store::NoopKvStore;
    use edgezero_core::secret_store::{NoopSecretStore, SecretHandle};
    use std::sync::Arc;

    #[test]
    fn insert_store_handles_adds_present_handles() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");
        let kv_handle = KvHandle::new(Arc::new(NoopKvStore));
        let secret_handle = SecretHandle::new(Arc::new(NoopSecretStore));

        insert_store_handles(
            &mut request,
            Some(kv_handle.clone()),
            Some(secret_handle.clone()),
        );

        assert!(request.extensions().get::<KvHandle>().is_some());
        assert!(request.extensions().get::<SecretHandle>().is_some());
    }

    #[test]
    fn insert_store_handles_skips_absent_handles() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        insert_store_handles(&mut request, None, None);

        assert!(request.extensions().get::<KvHandle>().is_none());
        assert!(request.extensions().get::<SecretHandle>().is_none());
    }
}
