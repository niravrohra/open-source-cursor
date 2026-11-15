use crate::protocol::ChatMessage;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
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

pub struct GeminiClient {
    http: Client,
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
        mut on_chunk: F,
    ) -> Result<GeminiTurnOutput, String>
    where
        F: FnMut(&str),
    {
        let url = format!("{GEMINI_BASE_URL}/{model}:streamGenerateContent?alt=sse");

        let system_instruction = system_prompt.map(|text| {
            json!({
                "parts": [{ "text": text }]
            })
        });

        let body = GeminiRequest {
            system_instruction,
            contents: contents.to_vec(),
            tools,
            generation_config: Some(GenerationConfig {
                temperature,
                max_output_tokens,
            }),
        };

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
}
