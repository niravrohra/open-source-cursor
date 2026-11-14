use crate::protocol::ChatMessage;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    error: Option<GeminiApiError>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiApiError {
    message: String,
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
        mut on_chunk: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str),
    {
        let url = format!("{GEMINI_BASE_URL}/{model}:streamGenerateContent?alt=sse");

        let system_instruction = system_prompt.map(|text| GeminiContent {
            role: None,
            parts: vec![GeminiPart {
                text: text.to_string(),
            }],
        });

        let contents = messages
            .iter()
            .map(|m| GeminiContent {
                role: Some(match m.role.as_str() {
                    "assistant" | "model" => "model".to_string(),
                    _ => "user".to_string(),
                }),
                parts: vec![GeminiPart {
                    text: m.content.clone(),
                }],
            })
            .collect();

        let body = GeminiRequest {
            system_instruction,
            contents,
            generation_config: Some(GenerationConfig { temperature }),
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
        let mut full_text = String::new();

        while let Some(item) = stream.next().await {
            let bytes = item.map_err(|e| format!("Stream read error: {e}"))?;
            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = sse_buffer.find('\n') {
                let line = sse_buffer[..pos].trim_end_matches('\r').to_string();
                sse_buffer.drain(..=pos);

                if let Some(json_str) = line.strip_prefix("data: ") {
                    if let Some(delta) = parse_sse_data(json_str)? {
                        full_text.push_str(&delta);
                        on_chunk(&delta);
                    }
                }
            }
        }

        if let Some(json_str) = sse_buffer.trim().strip_prefix("data: ") {
            if let Some(delta) = parse_sse_data(json_str)? {
                full_text.push_str(&delta);
                on_chunk(&delta);
            }
        }

        Ok(full_text)
    }
}

fn parse_sse_data(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }

    let chunk: GeminiStreamChunk =
        serde_json::from_str(trimmed).map_err(|e| format!("Invalid Gemini SSE JSON: {e}"))?;

    if let Some(err) = chunk.error {
        return Err(format!("Gemini stream error: {}", err.message));
    }

    let mut text = String::new();
    for candidate in chunk.candidates {
        if let Some(content) = candidate.content {
            for part in content.parts {
                text.push_str(&part.text);
            }
        }
    }

    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_sse_data;

    #[test]
    fn parses_gemini_sse_candidate_text() {
        let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"fn main() {}"}]}}]}"#;
        let parsed = parse_sse_data(raw).unwrap();
        assert_eq!(parsed.as_deref(), Some("fn main() {}"));
    }

    #[test]
    fn surfaces_gemini_sse_error() {
        let raw = r#"{"error":{"message":"API key not valid"}}"#;
        let err = parse_sse_data(raw).unwrap_err();
        assert!(err.contains("API key not valid"));
    }
}
