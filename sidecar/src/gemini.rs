use crate::protocol::ChatMessage;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_V1BETA_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-004";
const DEFAULT_COMPACTION_MODEL: &str = "gemini-2.5-flash";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Value>,
    contents: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_content: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThinkingConfig {
    thinking_budget: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<ThinkingConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct GeminiTurnOptions {
    pub thinking_budget: Option<u32>,
    pub cached_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    error: Option<GeminiApiError>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiCandidateContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidateContent {
    #[serde(default)]
    parts: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct GeminiApiError {
    message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GeminiFunctionCall {
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Default)]
pub struct GeminiTurnOutput {
    pub text: String,
    pub function_calls: Vec<GeminiFunctionCall>,
    pub raw_model_parts: Vec<Value>,
}

#[derive(Debug, Default, PartialEq)]
struct ParsedChunk {
    text: String,
    function_calls: Vec<GeminiFunctionCall>,
    raw_parts: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct GeminiClient {
    http: Client,
}

impl Default for GeminiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiClient {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
        }
    }

    /// Streams text deltas from the Gemini Cloud API (`streamGenerateContent?alt=sse`).
    pub async fn stream_chat<F>(
        &self,
        api_key: &str,
        model: &str,
        system_prompt: Option<&str>,
        messages: &[ChatMessage],
        temperature: f32,
        max_output_tokens: Option<u32>,
        mut on_chunk: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str),
    {
        let contents: Vec<Value> = messages
            .iter()
            .map(chat_message_to_gemini_content)
            .collect();

        let output = self
            .stream_turn(
                api_key,
                model,
                system_prompt,
                &contents,
                None,
                temperature,
                max_output_tokens,
                &mut on_chunk,
            )
            .await?;

        Ok(output.text)
    }

    /// Streams a single Gemini turn supporting raw conversation contents and function calling.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_turn<F>(
        &self,
        api_key: &str,
        model: &str,
        system_prompt: Option<&str>,
        contents: &[Value],
        tools: Option<Vec<Value>>,
        temperature: f32,
        max_output_tokens: Option<u32>,
        on_chunk: F,
    ) -> Result<GeminiTurnOutput, String>
    where
        F: FnMut(&str),
    {
        self.stream_turn_with_options(
            api_key,
            model,
            system_prompt,
            contents,
            tools,
            temperature,
            max_output_tokens,
            GeminiTurnOptions::default(),
            on_chunk,
        )
        .await
    }

    /// Streams a single Gemini turn with optional thinking budget and cached context reference.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_turn_with_options<F>(
        &self,
        api_key: &str,
        model: &str,
        system_prompt: Option<&str>,
        contents: &[Value],
        tools: Option<Vec<Value>>,
        temperature: f32,
        max_output_tokens: Option<u32>,
        options: GeminiTurnOptions,
        mut on_chunk: F,
    ) -> Result<GeminiTurnOutput, String>
    where
        F: FnMut(&str),
    {
        let url = format!("{GEMINI_BASE_URL}/{model}:streamGenerateContent?alt=sse");
        let body = build_gemini_request(
            system_prompt,
            contents,
            tools,
            temperature,
            max_output_tokens,
            options,
        );

        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Gemini request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Gemini API error ({status}): {err_body}"));
        }

        let mut stream = resp.bytes_stream();
        let mut sse_buffer = String::new();
        let mut output = GeminiTurnOutput::default();

        while let Some(item) = stream.next().await {
            let bytes = item.map_err(|e| format!("Stream read error: {e}"))?;
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = sse_buffer.find('\n') {
                let line = sse_buffer[..pos].trim_end_matches('\r').to_string();
                sse_buffer.drain(..=pos);

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Some(parsed) = parse_sse_data(json_str)? {
                        apply_parsed_chunk(parsed, &mut output, &mut on_chunk);
                    }
                }
            }
        }

        if let Some(json_str) = sse_buffer.trim().strip_prefix("data: ") {
            if let Some(parsed) = parse_sse_data(json_str)? {
                apply_parsed_chunk(parsed, &mut output, &mut on_chunk);
            }
        }

        Ok(output)
    }

    /// Creates a Gemini Context Cache (`v1beta/cachedContents`) for static workspace/system context
    /// and returns the resulting resource name (e.g. `cachedContents/...`).
    pub async fn create_cached_content(
        &self,
        api_key: &str,
        model: &str,
        system_prompt: Option<&str>,
        static_context: &str,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        let url = format!("{GEMINI_V1BETA_URL}/cachedContents");
        let qualified_model = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        };

        let mut payload = json!({
            "model": qualified_model,
            "contents": [{
                "role": "user",
                "parts": [{ "text": static_context }]
            }],
            "ttl": format!("{ttl_seconds}s")
        });

        if let Some(sys) = system_prompt {
            if !sys.trim().is_empty() {
                payload["systemInstruction"] = json!({
                    "parts": [{ "text": sys }]
                });
            }
        }

        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Gemini cachedContents request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Gemini cachedContents API error ({status}): {err_body}"));
        }

        let val: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid cachedContents response JSON: {e}"))?;
        val.get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Missing 'name' in cachedContents response: {val}"))
    }

    /// Compacts older conversation turns into a concise summary message via Gemini Flash
    /// while preserving the most recent `keep_recent` messages verbatim.
    pub async fn compact_messages(
        &self,
        api_key: &str,
        model: &str,
        messages: &[ChatMessage],
        keep_recent: usize,
    ) -> Result<Vec<ChatMessage>, String> {
        if messages.len() <= keep_recent || messages.len() <= 2 {
            return Ok(messages.to_vec());
        }

        let split_idx = messages.len().saturating_sub(keep_recent);
        if split_idx == 0 {
            return Ok(messages.to_vec());
        }

        let older = &messages[..split_idx];
        let recent = &messages[split_idx..];

        let mut transcript = String::new();
        for (idx, msg) in older.iter().enumerate() {
            transcript.push_str(&format!(
                "[Turn {} - {}]:\n{}\n\n",
                idx + 1,
                msg.role,
                msg.content
            ));
        }

        let compaction_model = if model.trim().is_empty() {
            DEFAULT_COMPACTION_MODEL
        } else {
            model
        };

        let prompt_messages = vec![ChatMessage {
            role: "user".to_string(),
            content: format!(
                "Summarize the following earlier conversation turns concisely. Preserve all key technical decisions, file paths, symbol names, commands, errors, and open action items.\n\n{transcript}"
            ),
            images: Vec::new(),
        }];

        let summary = self
            .stream_chat(
                api_key,
                compaction_model,
                Some("You are a precise technical conversation compactor. Output a structured, factual summary only."),
                &prompt_messages,
                0.1,
                Some(2048),
                |_| {},
            )
            .await?;

        let mut compacted = Vec::with_capacity(1 + recent.len());
        compacted.push(ChatMessage {
            role: "user".to_string(),
            content: format!("[Compacted Earlier Conversation Summary]\n{}", summary.trim()),
            images: Vec::new(),
        });
        compacted.extend_from_slice(recent);
        Ok(compacted)
    }

    /// Generates a dense embedding vector using `text-embedding-004:embedContent`.
    pub async fn embed_text(&self, api_key: &str, text: &str) -> Result<Vec<f32>, String> {
        let url = format!("{GEMINI_BASE_URL}/{DEFAULT_EMBEDDING_MODEL}:embedContent");
        let payload = json!({
            "model": format!("models/{DEFAULT_EMBEDDING_MODEL}"),
            "content": {
                "parts": [{ "text": text }]
            }
        });

        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Gemini embedContent request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Gemini embedContent API error ({status}): {err_body}"));
        }

        let val: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid embedContent response JSON: {e}"))?;
        let values = val
            .get("embedding")
            .and_then(|e| e.get("values"))
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("Missing embedding.values in response: {val}"))?;

        let mut out = Vec::with_capacity(values.len());
        for item in values {
            let num = item
                .as_f64()
                .ok_or_else(|| format!("Non-numeric embedding value in response: {item}"))?;
            out.push(num as f32);
        }
        Ok(out)
    }
}

fn build_gemini_request(
    system_prompt: Option<&str>,
    contents: &[Value],
    tools: Option<Vec<Value>>,
    temperature: f32,
    max_output_tokens: Option<u32>,
    options: GeminiTurnOptions,
) -> GeminiRequest {
    let system_instruction = system_prompt.map(|text| {
        json!({
            "parts": [{ "text": text }]
        })
    });

    let thinking_config = options
        .thinking_budget
        .map(|thinking_budget| ThinkingConfig { thinking_budget });

    GeminiRequest {
        system_instruction,
        contents: contents.to_vec(),
        tools,
        generation_config: Some(GenerationConfig {
            temperature,
            max_output_tokens,
            thinking_config,
        }),
        cached_content: options.cached_content,
    }
}

/// Estimates the total token count for a slice of chat messages using a ~4 chars/token heuristic
/// plus per-message framing and image attachment overhead.
pub fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|msg| {
            let text_tokens = (msg.content.chars().count() + 3) / 4;
            let image_tokens = msg.images.len() * 258;
            4 + text_tokens + image_tokens
        })
        .sum()
}

fn apply_parsed_chunk<F: FnMut(&str)>(
    parsed: ParsedChunk,
    output: &mut GeminiTurnOutput,
    on_chunk: &mut F,
) {
    if !parsed.text.is_empty() {
        output.text.push_str(&parsed.text);
        on_chunk(&parsed.text);
    }
    output.function_calls.extend(parsed.function_calls);
    output.raw_model_parts.extend(parsed.raw_parts);
}

pub fn chat_message_to_gemini_content(m: &ChatMessage) -> Value {
    let role = match m.role.as_str() {
        "assistant" | "model" => "model",
        _ => "user",
    };
    let mut parts = Vec::new();
    for img in &m.images {
        parts.push(json!({
            "inlineData": {
                "mimeType": img.mime_type,
                "data": img.data
            }
        }));
    }
    if !m.content.is_empty() || parts.is_empty() {
        parts.push(json!({ "text": m.content }));
    }
    json!({
        "role": role,
        "parts": parts
    })
}

fn parse_sse_data(raw: &str) -> Result<Option<ParsedChunk>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }

    let chunk: GeminiStreamChunk =
        serde_json::from_str(trimmed).map_err(|e| format!("Invalid Gemini SSE JSON: {e}"))?;

    if let Some(err) = chunk.error {
        return Err(format!("Gemini stream error: {}", err.message));
    }

    let mut out = ParsedChunk::default();
    for candidate in chunk.candidates {
        if let Some(content) = candidate.content {
            for part in content.parts {
                let is_thought = part
                    .get("thought")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !is_thought {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        out.text.push_str(t);
                    }
                }

                if let Some(fc) = part.get("functionCall") {
                    if let Some(name) = fc.get("name").and_then(|v| v.as_str()) {
                        let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                        out.function_calls.push(GeminiFunctionCall {
                            name: name.to_string(),
                            args,
                        });
                    }
                }

                out.raw_parts.push(part);
            }
        }
    }

    if out.text.is_empty() && out.function_calls.is_empty() && out.raw_parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gemini_sse_candidate_text() {
        let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"fn main() {}"}]}}]}"#;
        let parsed = parse_sse_data(raw).unwrap().unwrap();
        assert_eq!(parsed.text, "fn main() {}");
        assert!(parsed.function_calls.is_empty());
    }

    #[test]
    fn filters_thought_text_and_extracts_function_call() {
        let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"thinking...","thought":true},{"functionCall":{"name":"read_file","args":{"path":"src/main.rs"}},"thoughtSignature":"sig123"}]}}]}"#;
        let parsed = parse_sse_data(raw).unwrap().unwrap();
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.function_calls.len(), 1);
        assert_eq!(parsed.function_calls[0].name, "read_file");
        assert_eq!(parsed.function_calls[0].args["path"], "src/main.rs");
        assert_eq!(parsed.raw_parts.len(), 2);
        assert_eq!(parsed.raw_parts[1]["thoughtSignature"], "sig123");
    }

    #[test]
    fn surfaces_gemini_sse_error() {
        let raw = r#"{"error":{"message":"API key not valid"}}"#;
        let err = parse_sse_data(raw).unwrap_err();
        assert!(err.contains("API key not valid"));
    }

    #[test]
    fn estimates_tokens_for_messages_and_images() {
        use crate::protocol::ImageAttachment;

        let empty: Vec<ChatMessage> = Vec::new();
        assert_eq!(estimate_messages_tokens(&empty), 0);

        let msgs = vec![
            ChatMessage {
                role: "user".into(),
                content: "12345678".into(), // 8 chars -> 2 tokens + 4 overhead = 6
                images: vec![],
            },
            ChatMessage {
                role: "assistant".into(),
                content: "abcd".into(), // 4 chars -> 1 token + 4 overhead + 258 image = 263
                images: vec![ImageAttachment {
                    mime_type: "image/png".into(),
                    data: "AAAA".into(),
                    name: None,
                }],
            },
        ];
        assert_eq!(estimate_messages_tokens(&msgs), 6 + 263);
    }

    #[test]
    fn serializes_request_with_thinking_config_and_cached_content() {
        let req = build_gemini_request(
            None,
            &[json!({ "role": "user", "parts": [{ "text": "hi" }] })],
            None,
            0.3,
            Some(4096),
            GeminiTurnOptions {
                thinking_budget: Some(2048),
                cached_content: Some("cachedContents/ws-ctx-1".to_string()),
            },
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["cachedContent"], "cachedContents/ws-ctx-1");
        assert_eq!(
            v["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            2048
        );
        assert_eq!(v["generationConfig"]["maxOutputTokens"], 4096);
        assert!(v.get("systemInstruction").is_none());

        let default_req = build_gemini_request(
            Some("Be helpful"),
            &[json!({ "role": "user", "parts": [{ "text": "hi" }] })],
            None,
            0.2,
            None,
            GeminiTurnOptions::default(),
        );
        let default_v = serde_json::to_value(&default_req).unwrap();
        assert!(default_v.get("cachedContent").is_none());
        assert!(default_v["generationConfig"].get("thinkingConfig").is_none());
        assert_eq!(
            default_v["systemInstruction"]["parts"][0]["text"],
            "Be helpful"
        );
    }
}
