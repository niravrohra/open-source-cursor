use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct LocalLlmClient {
    http: Client,
}

impl LocalLlmClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    /// Completes a FIM prompt using either Ollama (`/api/generate`) or an OpenAI/MLX-compatible server (`/v1/chat/completions`).
    pub async fn complete_fim(
        &self,
        backend: &str,
        endpoint: &str,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, String> {
        let base = endpoint.trim_end_matches('/');
        if backend == "ollama" {
            let url = format!("{base}/api/generate");
            let body = json!({
                "model": model,
                "system": system_prompt,
                "prompt": user_prompt,
                "stream": false,
                "options": {
                    "temperature": 0.1,
                    "num_predict": 160
                }
            });
            let resp = self
                .http
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Ollama request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                return Err(format!("Ollama error ({status}): {txt}"));
            }
            let val: Value = resp
                .json()
                .await
                .map_err(|e| format!("Invalid Ollama response: {e}"))?;
            return Ok(val
                .get("response")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string());
        }

        // MLX / OpenAI-compatible server (e.g. mlx_lm.server, LM Studio, llama.cpp server)
        let url = if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        };
        let body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": user_prompt }
            ],
            "temperature": 0.1,
            "max_tokens": 160,
            "stream": false
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Local OpenAI/MLX request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(format!("Local server error ({status}): {txt}"));
        }
        let val: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid local OpenAI/MLX response: {e}"))?;
        let text = val
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(text.to_string())
    }
}
