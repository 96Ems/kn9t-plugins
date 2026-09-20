//! scrape tool — fetches and extracts content from a URL using Firecrawl.

use crate::http;
use kn9t_plugin_sdk::{
    ctx::ToolCallCtx,
    traits::{PluginTool, ToolOutput},
    wire::{DefaultPolicy, Effect, EffectKind, ToolPolicy, ToolSpec},
};
use serde::Deserialize;
use serde_json::{json, Value};

pub struct Scrape;

#[derive(Deserialize)]
struct ScrapeResponse {
    success: bool,
    data: Option<ScrapeData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ScrapeData {
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    metadata: Option<ScrapeMetadata>,
}

#[derive(Deserialize)]
struct ScrapeMetadata {
    #[serde(default)]
    title: Option<String>,
}

impl PluginTool for Scrape {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scrape".into(),
            description: "Fetch and extract content from a URL as markdown. \
                          Uses Firecrawl to handle JavaScript rendering and complex pages. \
                          Returns clean markdown suitable for LLM context.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to scrape."
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default: 20000)."
                    }
                },
                "required": ["url"]
            }),
            parallel_safe: true,
            hidden: false,
            effects: vec![Effect { field: "url".into(), kind: EffectKind::Network }],
            policy: ToolPolicy {
                pattern_field: Some("url".into()),
                default_policy: DefaultPolicy::Allow,
                builtin_allow: vec![],
                builtin_deny: vec![],
            },
        }
    }

    fn execute(&self, args: &Value, ctx: &ToolCallCtx) -> ToolOutput {
        let url = match args.get("url").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => return ToolOutput::error("missing 'url' argument"),
        };

        let max_chars = args.get("max_chars")
            .and_then(|m| m.as_u64())
            .unwrap_or(20000) as usize;

        if ctx.cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }

        ctx.progress.send(&format!("Scraping: {url}"));

        let body = json!({
            "url": url,
            "formats": ["markdown"]
        });

        let response = match http::agent().post("https://api.firecrawl.dev/v1/scrape")
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

        let result: ScrapeResponse = match response.into_json() {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("JSON parse error: {e}")),
        };

        if !result.success {
            return ToolOutput::error(result.error.unwrap_or_else(|| "unknown error".into()));
        }

        let data = match result.data {
            Some(d) => d,
            None => return ToolOutput::error("No content returned."),
        };

        let mut output = String::new();

        if let Some(meta) = &data.metadata {
            if let Some(title) = &meta.title {
                output.push_str(&format!("# {title}\n\n"));
            }
        }

        if let Some(markdown) = data.markdown {
            let content = if markdown.len() > max_chars {
                format!("{}...\n\n[Content truncated at {} chars]", &markdown[..max_chars], max_chars)
            } else {
                markdown
            };
            output.push_str(&content);
        } else {
            return ToolOutput::error("No markdown content extracted.");
        }

        ToolOutput::text(output)
    }
}
