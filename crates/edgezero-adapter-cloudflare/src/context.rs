use std::sync::Arc;

use edgezero_core::http::Request;
use worker::{Context, Env};

/// Adapter-specific context stored alongside each request to expose Worker APIs.
#[derive(Clone, Debug)]
pub struct CloudflareRequestContext {
    ctx: Arc<Context>,
    env: Arc<Env>,
}

impl CloudflareRequestContext {
    #[inline]
    #[must_use]
    pub fn ctx(&self) -> &Context {
        &self.ctx
    }

    #[inline]
    #[must_use]
    pub fn env(&self) -> &Env {
        &self.env
    }

    #[inline]
    #[must_use]
    pub fn get(request: &Request) -> Option<&Self> {
        request.extensions().get::<Self>()
    }

    #[inline]
    pub fn insert(request: &mut Request, env: Env, ctx: Context) {
        request.extensions_mut().insert(Self {
            ctx: Arc::new(ctx),
            env: Arc::new(env),
        });
    }
}
