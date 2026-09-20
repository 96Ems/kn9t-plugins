//! kn9t-anthropic — Anthropic Messages API provider plugin.
//!
//! R-ANTH-010..R-ANTH-040: Messages API, thinking verbatim, cache placement, usage partition.

mod client;
mod map;

use kn9t_plugin_sdk::{
    Plugin,
    ctx::ProviderCallCtx,
    traits::{PluginProvider, ProviderResult},
    wire::{ModelDecl, PriceDecl},
};
use serde_json::Value;

pub struct AnthropicProvider;

impl PluginProvider for AnthropicProvider {
    fn id(&self) -> &str { "anthropic" }

    fn models(&self) -> Vec<ModelDecl> {
        vec![
            ModelDecl {
                id: "claude-opus-4-5".into(),
                ctx_window: 200_000,
                price: Some(PriceDecl { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
            },
            ModelDecl {
                id: "claude-sonnet-4-5".into(),
                ctx_window: 200_000,
                price: Some(PriceDecl { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
            },
            ModelDecl {
                id: "claude-haiku-4-5".into(),
                ctx_window: 200_000,
                price: Some(PriceDecl { input: 0.8, output: 4.0, cache_read: 0.08, cache_write: 1.0 }),
            },
        ]
    }

    fn complete(&self, request: &Value, ctx: &ProviderCallCtx) -> ProviderResult {
        client::complete(request, ctx)
    }
}

fn main() {
    Plugin::new("kn9t-anthropic")
        .provider(AnthropicProvider)
        .run();
}
