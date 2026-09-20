//! websearch tool — searches the web using Firecrawl's keyless API.

use crate::http;
use kn9t_plugin_sdk::{
    ctx::ToolCallCtx,
    traits::{PluginTool, ToolOutput},
    wire::{DefaultPolicy, Effect, EffectKind, ToolPolicy, ToolSpec},
};
use serde::Deserialize;
use serde_json::{json, Value};

pub struct WebSearch;

#[derive(Deserialize)]
struct SearchResponse {
    success: bool,
    data: Option<SearchData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct SearchData {
    #[serde(default)]
    web: Vec<WebResult>,
}

#[derive(Deserialize)]
struct WebResult {
    url: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
}

impl PluginTool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "websearch".into(),
            description: "Search the web using Firecrawl. Returns URLs, titles, and descriptions. \
                          Use for finding current information, research, or discovering relevant pages. \
                          Keyless API with daily rate limits.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query string."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 5, max: 10)."
                    }
                },
                "required": ["query"]
            }),
            parallel_safe: true,
            hidden: false,
            effects: vec![Effect { field: "query".into(), kind: EffectKind::Network }],
            policy: ToolPolicy {
                pattern_field: Some("query".into()),
                default_policy: DefaultPolicy::Allow,
                builtin_allow: vec![],
                builtin_deny: vec![],
            },
        }
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let query = match args.get("query").and_then(|q| q.as_str()) {
            Some(q) => q,
            None => return ToolOutput::error("missing 'query' argument"),
        };

        let limit = args.get("limit")
            .and_then(|l| l.as_u64())
            .unwrap_or(5)
            .min(10) as usize;

        if ctx.cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }

        ctx.progress.send(&format!("Searching: {query}"));

        let body = json!({
            "query": query,
            "limit": limit
        });

        let response = match http::agent().post("https://api.firecrawl.dev/v2/search")
            .set("Content-Type", "application/json")
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(429, _)) => {
                return ToolOutput::error("Rate limit exceeded. Firecrawl keyless API has daily limits per IP.");
            }
            Err(e) => {
                return ToolOutput::error(format!("HTTP error: {e}"));
            }
        };

        let result: SearchResponse = match response.into_json() {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("JSON parse error: {e}")),
        };

        if !result.success {
            return ToolOutput::error(result.error.unwrap_or_else(|| "unknown error".into()));
        }

        let data = match result.data {
            Some(d) => d,
            None => return ToolOutput::text("No results found."),
        };

        if data.web.is_empty() {
            return ToolOutput::text("No results found.");
        }

        let mut output = String::new();
        for (i, r) in data.web.iter().enumerate() {
            output.push_str(&format!("{}. **{}**\n", i + 1, r.title));
            output.push_str(&format!("   {}\n", r.url));
            if let Some(desc) = &r.description {
                let truncated = if desc.len() > 500 {
                    format!("{}...", &desc[..500])
                } else {
                    desc.clone()
                };
                output.push_str(&format!("   {}\n", truncated));
            }
            output.push('\n');
        }

        ToolOutput::text(output.trim_end())
    }
}
