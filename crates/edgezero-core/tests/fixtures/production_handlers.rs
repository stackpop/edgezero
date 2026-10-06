//! Shared native/WASM hosting fixture. No native runtime dependency belongs here.

use std::sync::Arc;

use edgezero_core::EdgeError;
use edgezero_core::action;
use edgezero_core::extractor::{AppConfig, State};
use serde::Deserialize;
use validator::Validate;

#[derive(Deserialize, Validate, edgezero_core::AppConfig)]
pub struct FixtureConfig {
    #[validate(length(min = 4_u64))]
    pub greeting: String,
    #[secret]
    #[validate(length(min = 8_u64))]
    pub token: String,
}

pub struct HostState {
    pub greeting: String,
}

#[action]
pub async fn state(State(state): State<Arc<HostState>>) -> Result<String, EdgeError> {
    Ok(state.greeting.clone())
}

#[action]
pub async fn typed(AppConfig(config): AppConfig<FixtureConfig>) -> Result<String, EdgeError> {
    Ok(config.greeting)
}
