use crate::protocol::ChatMessage;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const OPENAI_CHAT_COMPLETIONS_URL: &str = "https://api.openai.com/v1/chat/completions";

const DEFAULT_GEMINI_FAST: &str = "gemini-2.5-flash";
const DEFAULT_GEMINI_LITE: &str = "gemini-2.5-flash-lite";
const DEFAULT_GEMINI_PRO: &str = "gemini-2.5-pro";
const DEFAULT_ANTHROPIC_FAST: &str = "claude-3-5-haiku-latest";
const DEFAULT_ANTHROPIC_DEEP: &str = "claude-sonnet-4-20250514";
const DEFAULT_OPENAI_FAST: &str = "gpt-4o-mini";
const DEFAULT_OPENAI_DEEP: &str = "o3-mini";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskKind {
    TabAutocomplete,
    InlineEdit,
    AgentFast,
    AgentDeep,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Gemini,
    LocalLlm,
    Anthropic,
    OpenAi,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderKeys {
    pub gemini: Option<String>,
    pub anthropic: Option<String>,
    pub openai: Option<String>,
}

impl ProviderKeys {
    pub fn has_gemini(&self) -> bool {
        self.gemini
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn has_anthropic(&self) -> bool {
        self.anthropic
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn has_openai(&self) -> bool {
        self.openai
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDecision {
    pub provider: ProviderKind,
    pub model: String,
    pub thinking_budget: Option<u32>,
    pub reason: String,
}

/// Routes a task to the optimal provider and model based on explicit model override,
/// task kind, prompt complexity, local LLM availability, and configured BYOK keys.
pub fn route_task(
    kind: TaskKind,
    prompt_hint: &str,
    requested_model: Option<&str>,
    local_backend: &str,
    local_model: &str,
    keys: &ProviderKeys,
) -> RouteDecision {
    let complex = is_complex_prompt(prompt_hint);
    let explicit = requested_model
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("auto"));

    if let Some(model) = explicit {
        let lower = model.to_ascii_lowercase();

        if lower.starts_with("claude-") {
            let thinking_budget = if kind == TaskKind::AgentDeep || complex {
                Some(8192)
            } else {
                None
            };
            return RouteDecision {
                provider: ProviderKind::Anthropic,
                model: model.to_string(),
                thinking_budget,
                reason: format!("Explicit Claude model '{model}' routed to Anthropic"),
            };
        }

        if is_openai_model_name(&lower) {
            return RouteDecision {
                provider: ProviderKind::OpenAi,
                model: model.to_string(),
                thinking_budget: None,
                reason: format!("Explicit OpenAI model '{model}' routed to OpenAI"),
            };
        }

        if lower == "local" || lower.starts_with("local:") {
            let resolved_local = model
                .strip_prefix("local:")
                .or_else(|| model.strip_prefix("LOCAL:"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| local_model.trim());
            let final_model = if resolved_local.is_empty() {
                "qwen2.5-coder:7b".to_string()
            } else {
                resolved_local.to_string()
            };
            return RouteDecision {
                provider: ProviderKind::LocalLlm,
                model: final_model,
                thinking_budget: None,
                reason: "Explicit local model routed to LocalLlm".to_string(),
            };
        }

        // Gemini or custom Gemini-compatible model
        if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
            let thinking_budget = if lower.contains("pro") && (kind == TaskKind::AgentDeep || complex)
            {
                Some(8192)
            } else {
                None
            };
            return RouteDecision {
                provider: ProviderKind::Gemini,
                model: model.to_string(),
                thinking_budget,
                reason: format!("Explicit model '{model}' routed to Gemini"),
            };
        }

        // Gemini key is missing, but another cloud BYOK key is configured -> fall back
        let deep_tier = kind == TaskKind::AgentDeep || complex || lower.contains("pro");
        return fallback_without_gemini(
            kind,
            deep_tier,
            keys,
            &format!("Gemini key absent for '{model}'; fell back to available BYOK provider"),
        );
    }

    // Automatic routing (requested_model is None or "auto")
    match kind {
        TaskKind::TabAutocomplete => {
            let backend = local_backend.trim().to_ascii_lowercase();
            let local_ready =
                !backend.is_empty() && backend != "gemini" && backend != "none" && !local_model.trim().is_empty();

            if local_ready {
                return RouteDecision {
                    provider: ProviderKind::LocalLlm,
                    model: local_model.trim().to_string(),
                    thinking_budget: None,
                    reason: format!("Tab autocomplete routed to local backend '{local_backend}'"),
                };
            }

            if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
                return RouteDecision {
                    provider: ProviderKind::Gemini,
                    model: DEFAULT_GEMINI_LITE.to_string(),
                    thinking_budget: None,
                    reason: "Tab autocomplete routed to low-latency Gemini Flash-Lite".to_string(),
                };
            }

            fallback_without_gemini(
                kind,
                false,
                keys,
                "Gemini key absent; Tab autocomplete fell back to available BYOK provider",
            )
        }
        TaskKind::Compaction => {
            if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
                RouteDecision {
                    provider: ProviderKind::Gemini,
                    model: DEFAULT_GEMINI_LITE.to_string(),
                    thinking_budget: None,
                    reason: "Context compaction routed to Gemini Flash-Lite".to_string(),
                }
            } else {
                fallback_without_gemini(
                    kind,
                    false,
                    keys,
                    "Gemini key absent; compaction fell back to available BYOK provider",
                )
            }
        }
        TaskKind::InlineEdit => {
            if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
                RouteDecision {
                    provider: ProviderKind::Gemini,
                    model: DEFAULT_GEMINI_FAST.to_string(),
                    thinking_budget: None,
                    reason: "Inline edit routed to fast Gemini Flash".to_string(),
                }
            } else {
                fallback_without_gemini(
                    kind,
                    false,
                    keys,
                    "Gemini key absent; inline edit fell back to available BYOK provider",
                )
            }
        }
        TaskKind::AgentFast => {
            if complex {
                if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
                    RouteDecision {
                        provider: ProviderKind::Gemini,
                        model: DEFAULT_GEMINI_PRO.to_string(),
                        thinking_budget: Some(8192),
                        reason: "Complex architecture/refactor prompt upgraded to Gemini Pro with thinking budget".to_string(),
                    }
                } else {
                    fallback_without_gemini(
                        kind,
                        true,
                        keys,
                        "Complex prompt with Gemini key absent; fell back to deep BYOK model",
                    )
                }
            } else if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
                RouteDecision {
                    provider: ProviderKind::Gemini,
                    model: DEFAULT_GEMINI_FAST.to_string(),
                    thinking_budget: None,
                    reason: "Standard agent task routed to fast Gemini Flash".to_string(),
                }
            } else {
                fallback_without_gemini(
                    kind,
                    false,
                    keys,
                    "Gemini key absent; fast agent task fell back to available BYOK provider",
                )
            }
        }
        TaskKind::AgentDeep => {
            let budget = if complex { 16384 } else { 8192 };
            if keys.has_gemini() || (!keys.has_anthropic() && !keys.has_openai()) {
                RouteDecision {
                    provider: ProviderKind::Gemini,
                    model: DEFAULT_GEMINI_PRO.to_string(),
                    thinking_budget: Some(budget),
                    reason: "Deep agent reasoning routed to Gemini Pro with thinking budget".to_string(),
                }
            } else {
                fallback_without_gemini(
                    kind,
                    true,
                    keys,
                    "Gemini key absent; deep agent task fell back to available BYOK provider",
                )
            }
        }
    }
}

fn fallback_without_gemini(
    kind: TaskKind,
    deep_tier: bool,
    keys: &ProviderKeys,
    reason_prefix: &str,
) -> RouteDecision {
    if keys.has_anthropic() {
        let model = if deep_tier {
            DEFAULT_ANTHROPIC_DEEP
        } else {
            DEFAULT_ANTHROPIC_FAST
        };
        let thinking_budget = if deep_tier && kind != TaskKind::TabAutocomplete {
            Some(8192)
        } else {
            None
        };
        return RouteDecision {
            provider: ProviderKind::Anthropic,
            model: model.to_string(),
            thinking_budget,
            reason: format!("{reason_prefix} (Anthropic)"),
        };
    }

    if keys.has_openai() {
        let model = if deep_tier {
            DEFAULT_OPENAI_DEEP
        } else {
            DEFAULT_OPENAI_FAST
        };
        return RouteDecision {
            provider: ProviderKind::OpenAi,
            model: model.to_string(),
            thinking_budget: None,
            reason: format!("{reason_prefix} (OpenAI)"),
        };
    }

    RouteDecision {
        provider: ProviderKind::Gemini,
        model: if deep_tier {
            DEFAULT_GEMINI_PRO.to_string()
        } else {
            DEFAULT_GEMINI_FAST.to_string()
        },
        thinking_budget: if deep_tier { Some(8192) } else { None },
        reason: "No provider keys configured; defaulting to Gemini".to_string(),
    }
}

fn is_openai_model_name(lower: &str) -> bool {
    lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

/// Detects complex architecture, refactoring, security, or multi-step prompts.
pub fn is_complex_prompt(prompt_hint: &str) -> bool {
    let lower = prompt_hint.to_ascii_lowercase();
    let complexity_keywords = [
        "architect",
        "architecture",
        "refactor",
        "redesign",
        "migrate",
        "migration",
        "distributed",
        "concurrency",
        "security audit",
        "multi-file",
        "system design",
        "root cause",
        "deep dive",
        "rewrite",
    ];
    if complexity_keywords.iter().any(|kw| lower.contains(kw)) {
        return true;
    }
    prompt_hint.chars().count() > 600
}

/// Streams a chat completion from Anthropic's `/v1/messages` SSE endpoint.
#[allow(clippy::too_many_arguments)]
pub async fn stream_anthropic_chat<F>(
    http: &Client,
    api_key: &str,
    model: &str,
    system_prompt: Option<&str>,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: Option<u32>,
    thinking_budget: Option<u32>,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    stream_anthropic_chat_with_url(
        http,
        ANTHROPIC_MESSAGES_URL,
        api_key,
        model,
        system_prompt,
        messages,
        temperature,
        max_tokens,
        thinking_budget,
        &mut on_chunk,
    )
    .await
}

/// Streams a chat completion from a configurable Anthropic-compatible `/v1/messages` URL.
#[allow(clippy::too_many_arguments)]
pub async fn stream_anthropic_chat_with_url<F>(
    http: &Client,
    url: &str,
    api_key: &str,
    model: &str,
    system_prompt: Option<&str>,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: Option<u32>,
    thinking_budget: Option<u32>,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    let converted_messages: Vec<Value> = messages
        .iter()
        .map(chat_message_to_anthropic_message)
        .collect();

    let requested_max = max_tokens.unwrap_or(4096);
    let mut body = json!({
        "model": model,
        "messages": converted_messages,
        "stream": true,
    });

    if let Some(sys) = system_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        body["system"] = json!(sys);
    }

    if let Some(budget) = thinking_budget.filter(|b| *b >= 1024) {
        let effective_max = requested_max.max(budget + 1024);
        body["max_tokens"] = json!(effective_max);
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": budget
        });
    } else {
        body["max_tokens"] = json!(requested_max);
        body["temperature"] = json!(temperature);
    }

    let resp = http
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Anthropic request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!("Anthropic API error ({status}): {err_body}"));
    }

    consume_sse_response(resp, parse_anthropic_sse_data, &mut on_chunk).await
}

/// Streams a chat completion from OpenAI's `/v1/chat/completions` SSE endpoint.
#[allow(clippy::too_many_arguments)]
pub async fn stream_openai_chat<F>(
    http: &Client,
    api_key: &str,
    model: &str,
    system_prompt: Option<&str>,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: Option<u32>,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    stream_openai_chat_with_url(
        http,
        OPENAI_CHAT_COMPLETIONS_URL,
        api_key,
        model,
        system_prompt,
        messages,
        temperature,
        max_tokens,
        &mut on_chunk,
    )
    .await
}

/// Streams a chat completion from a configurable OpenAI-compatible `/v1/chat/completions` URL.
#[allow(clippy::too_many_arguments)]
pub async fn stream_openai_chat_with_url<F>(
    http: &Client,
    url: &str,
    api_key: &str,
    model: &str,
    system_prompt: Option<&str>,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: Option<u32>,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    let mut converted_messages = Vec::new();
    if let Some(sys) = system_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        converted_messages.push(json!({
            "role": "system",
            "content": sys
        }));
    }
    converted_messages.extend(messages.iter().map(chat_message_to_openai_message));

    let lower_model = model.to_ascii_lowercase();
    let is_reasoning_model =
        lower_model.starts_with("o1") || lower_model.starts_with("o3") || lower_model.starts_with("o4");

    let mut body = json!({
        "model": model,
        "messages": converted_messages,
        "stream": true
    });

    if !is_reasoning_model {
        body["temperature"] = json!(temperature);
        if let Some(max) = max_tokens {
            body["max_tokens"] = json!(max);
        }
    } else if let Some(max) = max_tokens {
        body["max_completion_tokens"] = json!(max);
    }

    let resp = http
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OpenAI request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!("OpenAI API error ({status}): {err_body}"));
    }

    consume_sse_response(resp, parse_openai_sse_data, &mut on_chunk).await
}

async fn consume_sse_response<P, F>(
    resp: reqwest::Response,
    parse_fn: P,
    on_chunk: &mut F,
) -> Result<String, String>
where
    P: Fn(&str) -> Result<Option<String>, String>,
    F: FnMut(&str),
{
    let mut stream = resp.bytes_stream();
    let mut sse_buffer = String::new();
    let mut full_text = String::new();

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| format!("Stream read error: {e}"))?;
        sse_buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = sse_buffer.find('\n') {
            let line = sse_buffer[..pos].trim_end_matches('\r').to_string();
            sse_buffer.drain(..=pos);

            if let Some(json_str) = line.strip_prefix("data:") {
                if let Some(delta) = parse_fn(json_str)? {
                    if !delta.is_empty() {
                        full_text.push_str(&delta);
                        on_chunk(&delta);
                    }
                }
            }
        }
    }

    if let Some(json_str) = sse_buffer.trim().strip_prefix("data:") {
        if let Some(delta) = parse_fn(json_str)? {
            if !delta.is_empty() {
                full_text.push_str(&delta);
                on_chunk(&delta);
            }
        }
    }

    Ok(full_text)
}

fn chat_message_to_anthropic_message(m: &ChatMessage) -> Value {
    let role = match m.role.as_str() {
        "assistant" | "model" => "assistant",
        _ => "user",
    };

    if m.images.is_empty() {
        return json!({
            "role": role,
            "content": m.content
        });
    }

    let mut blocks = Vec::new();
    for img in &m.images {
        blocks.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": img.mime_type,
                "data": img.data
            }
        }));
    }
    if !m.content.is_empty() {
        blocks.push(json!({
            "type": "text",
            "text": m.content
        }));
    }
    json!({
        "role": role,
        "content": blocks
    })
}

fn chat_message_to_openai_message(m: &ChatMessage) -> Value {
    let role = match m.role.as_str() {
        "assistant" | "model" => "assistant",
        "system" => "system",
        _ => "user",
    };

    if m.images.is_empty() {
        return json!({
            "role": role,
            "content": m.content
        });
    }

    let mut parts = Vec::new();
    if !m.content.is_empty() {
        parts.push(json!({
            "type": "text",
            "text": m.content
        }));
    }
    for img in &m.images {
        parts.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", img.mime_type, img.data)
            }
        }));
    }
    json!({
        "role": role,
        "content": parts
    })
}

/// Parses a single `data:` payload from Anthropic's `/v1/messages` SSE stream.
/// Extracts `text_delta` chunks, filters `thinking_delta`, and surfaces API error events.
pub fn parse_anthropic_sse_data(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }

    let val: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("Invalid Anthropic SSE JSON: {e}"))?;

    let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "error" => {
            let msg = val
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Anthropic stream error");
            Err(format!("Anthropic stream error: {msg}"))
        }
        "content_block_delta" => {
            let delta_type = val
                .pointer("/delta/type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if delta_type == "text_delta" {
                let text = val
                    .pointer("/delta/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if text.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(text.to_string()))
                }
            } else {
                // e.g. thinking_delta or input_json_delta: do not emit as visible answer text
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// Parses a single `data:` payload from OpenAI's `/v1/chat/completions` SSE stream.
/// Extracts `choices[0].delta.content` and surfaces API error payloads.
pub fn parse_openai_sse_data(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }

    let val: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("Invalid OpenAI SSE JSON: {e}"))?;

    if let Some(err_msg) = val.pointer("/error/message").and_then(|v| v.as_str()) {
        return Err(format!("OpenAI stream error: {err_msg}"));
    }

    let Some(choices) = val.get("choices").and_then(|v| v.as_array()) else {
        return Ok(None);
    };

    let mut out = String::new();
    for choice in choices {
        if let Some(content) = choice.pointer("/delta/content").and_then(|v| v.as_str()) {
            out.push_str(content);
        }
    }

    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_keys() -> ProviderKeys {
        ProviderKeys {
            gemini: Some("gem-key".into()),
            anthropic: Some("ant-key".into()),
            openai: Some("oai-key".into()),
        }
    }

    #[test]
    fn routes_explicit_claude_and_openai_models() {
        let keys = all_keys();

        let claude = route_task(
            TaskKind::AgentFast,
            "Explain this fn",
            Some("claude-3-5-sonnet-latest"),
            "gemini",
            "",
            &keys,
        );
        assert_eq!(claude.provider, ProviderKind::Anthropic);
        assert_eq!(claude.model, "claude-3-5-sonnet-latest");
        assert_eq!(claude.thinking_budget, None);

        let gpt = route_task(
            TaskKind::InlineEdit,
            "Rename variable",
            Some("gpt-4o"),
            "gemini",
            "",
            &keys,
        );
        assert_eq!(gpt.provider, ProviderKind::OpenAi);
        assert_eq!(gpt.model, "gpt-4o");

        let o3 = route_task(
            TaskKind::AgentDeep,
            "Solve race condition",
            Some("o3-mini"),
            "gemini",
            "",
            &keys,
        );
        assert_eq!(o3.provider, ProviderKind::OpenAi);
        assert_eq!(o3.model, "o3-mini");

        let o4 = route_task(
            TaskKind::AgentFast,
            "Check types",
            Some("o4-mini"),
            "gemini",
            "",
            &keys,
        );
        assert_eq!(o4.provider, ProviderKind::OpenAi);
        assert_eq!(o4.model, "o4-mini");
    }

    #[test]
    fn routes_by_task_kind_and_prompt_complexity() {
        let keys = all_keys();

        // Complex architecture/refactor prompt -> gemini-2.5-pro with thinking budget
        let complex = route_task(
            TaskKind::AgentFast,
            "Refactor and architect the sidecar RPC layer for multi-provider streaming",
            Some("auto"),
            "gemini",
            "",
            &keys,
        );
        assert_eq!(complex.provider, ProviderKind::Gemini);
        assert_eq!(complex.model, "gemini-2.5-pro");
        assert!(complex.thinking_budget.unwrap_or(0) >= 8192);

        // Fast inline edit -> gemini-2.5-flash without thinking budget
        let fast = route_task(
            TaskKind::InlineEdit,
            "Add null check",
            None,
            "gemini",
            "",
            &keys,
        );
        assert_eq!(fast.provider, ProviderKind::Gemini);
        assert_eq!(fast.model, "gemini-2.5-flash");
        assert_eq!(fast.thinking_budget, None);

        // Tab autocomplete with local backend -> LocalLlm
        let tab_local = route_task(
            TaskKind::TabAutocomplete,
            "",
            None,
            "ollama",
            "qwen2.5-coder:7b",
            &keys,
        );
        assert_eq!(tab_local.provider, ProviderKind::LocalLlm);
        assert_eq!(tab_local.model, "qwen2.5-coder:7b");

        // Tab autocomplete without local backend -> gemini-2.5-flash-lite
        let tab_cloud = route_task(
            TaskKind::TabAutocomplete,
            "",
            None,
            "gemini",
            "",
            &keys,
        );
        assert_eq!(tab_cloud.provider, ProviderKind::Gemini);
        assert_eq!(tab_cloud.model, "gemini-2.5-flash-lite");

        // Compaction -> gemini-2.5-flash-lite
        let compact = route_task(TaskKind::Compaction, "summarize", None, "gemini", "", &keys);
        assert_eq!(compact.provider, ProviderKind::Gemini);
        assert_eq!(compact.model, "gemini-2.5-flash-lite");
    }

    #[test]
    fn falls_back_when_gemini_key_absent() {
        let anthropic_only = ProviderKeys {
            gemini: None,
            anthropic: Some("sk-ant-123".into()),
            openai: None,
        };
        let decision = route_task(
            TaskKind::AgentDeep,
            "Design new state machine",
            None,
            "gemini",
            "",
            &anthropic_only,
        );
        assert_eq!(decision.provider, ProviderKind::Anthropic);
        assert_eq!(decision.model, DEFAULT_ANTHROPIC_DEEP);
        assert!(decision.thinking_budget.is_some());

        let openai_only = ProviderKeys {
            gemini: Some("   ".into()),
            anthropic: None,
            openai: Some("sk-oai-123".into()),
        };
        let decision_oai = route_task(
            TaskKind::InlineEdit,
            "Format helper",
            Some("gemini-2.5-flash"),
            "gemini",
            "",
            &openai_only,
        );
        assert_eq!(decision_oai.provider, ProviderKind::OpenAi);
        assert_eq!(decision_oai.model, DEFAULT_OPENAI_FAST);
    }

    #[test]
    fn parses_anthropic_sse_chunks() {
        let start = r#"{"type":"message_start","message":{"id":"msg_1"}}"#;
        assert_eq!(parse_anthropic_sse_data(start).unwrap(), None);

        let thinking = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        assert_eq!(parse_anthropic_sse_data(thinking).unwrap(), None);

        let text = r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello, world!"}}"#;
        assert_eq!(
            parse_anthropic_sse_data(text).unwrap(),
            Some("Hello, world!".to_string())
        );

        let err = r#"{"type":"error","error":{"type":"overloaded_error","message":"Rate limit exceeded"}}"#;
        let err_res = parse_anthropic_sse_data(err).unwrap_err();
        assert!(err_res.contains("Rate limit exceeded"));
    }

    #[test]
    fn parses_openai_sse_chunks() {
        let role_only = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        assert_eq!(parse_openai_sse_data(role_only).unwrap(), None);

        let content = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"fn test() {}"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_openai_sse_data(content).unwrap(),
            Some("fn test() {}".to_string())
        );

        assert_eq!(parse_openai_sse_data("[DONE]").unwrap(), None);

        let err = r#"{"error":{"message":"Invalid API key","type":"invalid_request_error"}}"#;
        assert!(parse_openai_sse_data(err)
            .unwrap_err()
            .contains("Invalid API key"));
    }
}
