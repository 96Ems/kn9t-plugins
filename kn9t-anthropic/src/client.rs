//! R-ANTH-010..R-ANTH-040 — Anthropic Messages API HTTP client.

use crate::map;
use kn9t_plugin_sdk::{ctx::ProviderCallCtx, traits::ProviderResult, wire::Usage, SseReader};
use serde_json::{json, Value};

// ── config ────────────────────────────────────────────────────────────────────

struct AnthConfig {
    api_key: String,
    endpoint: String,
    version_header: String,
    /// Conversation-derived header: `(name, value)`. Both halves must be present.
    session_header: Option<(String, String)>,
}

impl AnthConfig {
    fn from_request(req: &Value) -> Result<Self, String> {
        let api_key = req.get("config")
            .and_then(|c| c.get("api_key"))
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| "missing api_key / ANTHROPIC_API_KEY".to_string())?;

        // Pointing at a gateway is per-deployment, so it is read from the environment the
        // host injects ([provider.X.env]) -- the request payload's `config` is kept first
        // for hosts that populate it.
        let endpoint = req.get("config")
            .and_then(|c| c.get("endpoint"))
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .or_else(|| non_empty_env("ANTHROPIC_BASE_URL"))
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());

        let version_header = req.get("config")
            .and_then(|c| c.get("anthropic_version"))
            .and_then(|v| v.as_str())
            .unwrap_or("2023-06-01")
            .to_string();

        // Gateways that route by conversation (OpenCode Go) reject a request without this
        // header, and its value is per conversation -- so the name is deployment config
        // and the value arrives on the request.
        let session_header = match (non_empty_env("ANTHROPIC_SESSION_HEADER"), session_of(req)) {
            (Some(name), Some(value)) => Some((name, value)),
            _ => None,
        };

        Ok(AnthConfig { api_key, endpoint, version_header, session_header })
    }
}

/// Env var that is present and not blank.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn session_of(req: &Value) -> Option<String> {
    req.get("session")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn complete(request: &Value, ctx: &ProviderCallCtx) -> ProviderResult {
    let cfg = match AnthConfig::from_request(request) {
        Ok(c) => c,
        Err(e) => return ProviderResult::error(e),
    };
    if ctx.cancel.is_cancelled() {
        return ProviderResult::error("cancelled");
    }

    let body = map::build_body(request);
    let body_str = match serde_json::to_string(&body) {
        Ok(s) => s,
        Err(e) => return ProviderResult::error(format!("serialise: {e}")),
    };

    // Debug: dump request body if ANTHROPIC_DEBUG=1
    if std::env::var("ANTHROPIC_DEBUG").as_deref() == Ok("1") {
        eprintln!("[kn9t-anthropic] request body:\n{}", 
            serde_json::to_string_pretty(&body).unwrap_or_else(|_| body_str.clone()));
    }

    let url = format!("{}/v1/messages", cfg.endpoint);
    let mut request = ureq::post(&url)
        .set("x-api-key", &cfg.api_key)
        .set("anthropic-version", &cfg.version_header)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    if let Some((name, value)) = &cfg.session_header {
        request = request.set(name, value);
    }
    let resp = match request.send_string(&body_str) {
        Ok(r) => r,
        Err(ureq::Error::Status(status, resp)) => {
            let body_text = resp.into_string().unwrap_or_default();
            return ProviderResult::error(format!("HTTP {status}: {body_text}"));
        }
        Err(e) => return ProviderResult::error(format!("connect: {e}")),
    };

    parse_stream(resp.into_reader(), ctx)
}

// ── SSE stream parser (Anthropic streaming events) ────────────────────────────

fn parse_stream<R: std::io::Read>(reader: R, ctx: &ProviderCallCtx) -> ProviderResult {
    let sse = SseReader::new(reader);

    // Anthropic SSE events: content_block_start, content_block_delta,
    // content_block_stop, message_delta (usage + stop), message_stop
    let mut stop_reason = String::new();
    let mut usage_val: Option<Value> = None;

    // Track current content block index and type for delta routing
    let mut block_idx: u32 = 0;
    let mut block_type = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();

    for event_result in sse {
        if ctx.cancel.is_cancelled() {
            return ProviderResult::error("cancelled");
        }
        let ev = match event_result {
            Ok(e) => e,
            Err(e) => return ProviderResult::error(e),
        };

        let data: Value = match serde_json::from_str(ev.data.lines().next().unwrap_or("")) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match ev.event.as_str() {
            "content_block_start" => {
                let idx = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                block_idx = idx;
                let block = data.get("content_block").unwrap_or(&Value::Null);
                block_type = block.get("type").and_then(|t| t.as_str())
                    .unwrap_or("").to_string();

                match block_type.as_str() {
                    "tool_use" => {
                        current_tool_id = block.get("id").and_then(|i| i.as_str())
                            .unwrap_or("").to_string();
                        current_tool_name = block.get("name").and_then(|n| n.as_str())
                            .unwrap_or("").to_string();
                        ctx.chunk.tool_use_start(&current_tool_id, &current_tool_name, "");
                    }
                    _ => {}
                }
            }

            "content_block_delta" => {
                let delta = data.get("delta").unwrap_or(&Value::Null);
                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            ctx.chunk.text_delta(text);
                        }
                    }
                    "thinking_delta" => {
                        let thinking = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                        // thinking_delta may not have a signature yet
                        ctx.chunk.thinking_delta(thinking, "");
                    }
                    "signature_delta" => {
                        // R-ANTH-020: signature arrives as a delta — emit as thinking signature
                        let sig = delta.get("signature").and_then(|s| s.as_str()).unwrap_or("");
                        ctx.chunk.thinking_delta("", sig);
                    }
                    "input_json_delta" => {
                        // Tool argument streaming (Anthropic)
                        let args = delta.get("partial_json").and_then(|j| j.as_str()).unwrap_or("");
                        if !args.is_empty() {
                            ctx.chunk.tool_use_delta(&current_tool_id, args);
                        }
                    }
                    _ => {}
                }
                let _ = block_idx; // suppress unused warning
            }

            "message_delta" => {
                if let Some(u) = data.get("usage") {
                    usage_val = Some(u.clone());
                }
                if let Some(sr) = data.get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    stop_reason = match sr {
                        "tool_use" => "TOOL_CALL".to_string(),
                        "max_tokens" => "LENGTH".to_string(),
                        _ => "STOP".to_string(),
                    };
                }
            }

            "message_stop" => break,

            "error" => {
                if let Some(err) = data.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    return ProviderResult::error(err);
                }
                return ProviderResult::error("unknown Anthropic error");
            }

            _ => {}
        }
    }

    let (input, output, cache_read, cache_write) = usage_val
        .as_ref()
        .map(|u| map::decode_usage(u))
        .unwrap_or((0, 0, 0, 0));

    ProviderResult {
        stop: if stop_reason.is_empty() { "STOP".to_string() } else { stop_reason },
        usage: Usage {
            input: input as u64,
            output: output as u64,
            cache_read: cache_read as u64,
            cache_write: cache_write as u64,
        },
        cost_usd: None,
        error: None,
    }
}
