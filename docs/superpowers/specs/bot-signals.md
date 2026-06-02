---
title: Bot Detection Signals Primitives
status: proposed
created: 2026-06-02
scope: fastly-cloudflare
---

# Bot Detection Signals Primitives

## Goal

Expose passive bot-related evidence from Fastly and Cloudflare so EdgeZero consumers can write their own bot policy.

The primitive should provide facts/signals only. EdgeZero should not decide whether a request is a bot, whether it should be blocked, or what thresholds a consumer should use.

## Non-goals

- No default bot policy.
- No `is_bot()` / `should_block()` helpers.
- No rate limiting / penaltybox abstraction in v1.
- No Turnstile / challenge helpers in v1.
- No Axum or Spin adapter population in v1.
- No fallback to spoofable forwarding headers for trusted `client_ip`.

## Core API

Add a provider-neutral `BotSignals` type in `edgezero-core`:

```rust
pub struct BotSignals {
    pub client_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub ja3: Option<String>,
    pub ja4: Option<String>,
    pub provider: Option<ProviderBotSignals>,
}

pub struct ProviderBotSignals {
    pub provider: BotProvider,
    pub analyzed: Option<bool>,
    pub score: Option<u8>,
    pub detected: Option<bool>,
    pub verified: Option<bool>,
    pub name: Option<String>,
    pub category: Option<String>,
    pub detection_ids: Vec<String>,
    pub js_detection_passed: Option<bool>,
    pub static_resource: Option<bool>,
    pub corporate_proxy: Option<bool>,
    pub ddos_detected: Option<bool>,
}

pub enum BotProvider {
    Fastly,
    Cloudflare,
}
```

Expose `BotSignals` directly as an extractor:

```rust
#[action]
async fn handler(signals: BotSignals) -> Result<Response, EdgeError> {
    // Consumer-owned bot policy.
}
```

Also expose request-context access:

```rust
impl RequestContext {
    pub fn bot_signals(&self) -> BotSignals;
}
```

Extraction should always succeed. If no adapter populated provider data, return a mostly empty/default `BotSignals` with simple request facts such as `user_agent` when available.

## Fastly adapter mapping

Populate `BotSignals` during `into_core_request` from trusted Fastly runtime APIs:

- `client_ip`: `Request::get_client_ip_addr()`
- `ja3`: `Request::get_tls_ja3_md5()` converted to lowercase hex
- `ja4`: `Request::get_tls_ja4()`
- `provider.provider`: `BotProvider::Fastly`
- `provider.analyzed`: `Some(Request::get_bot_analyzed())`
- `provider.detected`: `Some(Request::get_bot_detected())`
- `provider.verified`: `Request::get_bot_verified()`
- `provider.name`: `Request::get_bot_name().ok().flatten()`
- `provider.category`: `Request::get_bot_category().ok().flatten()`
- `provider.ddos_detected`: Fastly DDoS tag API

Do not include Fastly TLS protocol/cipher, original header order/count/fingerprint, or HTTP/2 fingerprint in v1.

## Cloudflare adapter mapping

Populate `BotSignals` during `into_core_request` before consuming the Workers request body, using `worker::Request::cf()` and `Cf::bot_management()`:

- `provider.provider`: `BotProvider::Cloudflare`
- `provider.score`: `botManagement.score`
- `provider.verified`: `botManagement.verifiedBot`
- `provider.static_resource`: `botManagement.staticResource`
- `ja3`: `botManagement.ja3Hash`
- `ja4`: `botManagement.ja4`
- `provider.js_detection_passed`: `botManagement.jsDetection.passed`
- `provider.detection_ids`: `botManagement.detectionIds`
- `provider.corporate_proxy`: `botManagement.corporateProxy`
- `provider.category`: `request.cf.verifiedBotCategory`

Do not include `signedAgent` in v1 because the current Rust Workers binding does not expose it cleanly.

## Consumer usage example

```rust
use edgezero_core::{action, BotSignals, EdgeError, Response, StatusCode};

#[action]
async fn protected(signals: BotSignals) -> Result<Response, EdgeError> {
    let provider = signals.provider.as_ref();

    let verified_bot = provider.and_then(|p| p.verified).unwrap_or(false);
    let low_cloudflare_score = provider
        .and_then(|p| p.score)
        .is_some_and(|score| score < 10);

    if low_cloudflare_score && !verified_bot {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body("blocked by app policy".into())
            .map_err(EdgeError::internal);
    }

    Response::ok("ok")
}
```

This example is illustrative only. EdgeZero should not provide this policy as a built-in default.

## Tests

Add focused tests for:

- `BotSignals` extractor returns inserted extension data.
- `RequestContext::bot_signals()` returns inserted extension data.
- `RequestContext::bot_signals()` / extractor return default/header-only data when no extension exists.
- Fastly adapter inserts `BotSignals` with client IP and provider fields when runtime APIs are available.
- Cloudflare adapter maps typed `request.cf.botManagement` fields when available.

## Verification

Initial implementation should run:

```sh
cargo test -p edgezero-core
cargo test -p edgezero-adapter-fastly
cargo test -p edgezero-adapter-cloudflare
cargo fmt --all -- --check
```

Before merging, also run the workspace checks required by `CLAUDE.md`.

## Research notes

### Fastly

Fastly Compute Rust exposes trusted request metadata relevant to bot policy: client IP, JA3/JA4, DDoS tagging, and Bot Management outputs including whether bot analysis ran, whether a bot was detected, bot name/category, and verified-bot status. Related but deferred primitives include `fastly::erl` edge rate limiting, `fastly::security::inspect` for NGWAF, ACLs, Config Store, KV Store, and Secret Store.

### Cloudflare

Cloudflare Workers exposes Bot Management data through `request.cf.botManagement` when Bot Management is enabled. The typed Rust surface includes score, verified bot, static resource, JA3 hash, JA4, JS detection result, detection IDs, and corporate proxy; `request.cf.verifiedBotCategory` is available separately. Related but deferred primitives include Workers KV, Durable Objects, Rate Limiting bindings, Turnstile Siteverify, and WAF custom rules.
