use std::sync::Arc;

use anyedge_core::Request;
use worker::{Context, Env};

/// Provider-specific context stored alongside each request to expose Worker APIs.
#[derive(Clone, Debug)]
pub struct CloudflareRequestContext {
    env: Arc<Env>,
    ctx: Arc<Context>,
}

impl CloudflareRequestContext {
    pub fn insert(request: &mut Request, env: Env, ctx: Context) {
        request.extensions_mut().insert(Self {
            env: Arc::new(env),
            ctx: Arc::new(ctx),
        });
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    pub fn ctx(&self) -> &Context {
        &self.ctx
    }

    pub fn get(request: &Request) -> Option<&CloudflareRequestContext> {
        request.extensions().get::<CloudflareRequestContext>()
    }
}
