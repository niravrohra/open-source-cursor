#![allow(dead_code)]

use reqwest::Client;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const EMBED_DIM: usize = 128;
const RRF_K: f64 = 60.0;
const CHUNK_TARGET_CHARS: usize = 1_200;
const MAX_CRAWL_PAGES_CAP: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResponse {
    pub query: String,
    pub synthesized_answer: String,
    pub citations: Vec<WebSearchHit>,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UrlMarkdownResult {
    pub url: String,
    pub title: String,
    pub markdown: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DocSourceSummary {
    pub name: String,
    pub root_url: String,
    pub pages_indexed: usize,
    pub chunks_indexed: usize,
    pub updated_at: u64,
    pub used_llms_txt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocChunkHit {
    pub source_name: String,
    pub url: String,
    pub title: String,
    pub section_heading: String,
    pub content: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpEnvPrompt {
    pub key: String,
    pub label: String,
    pub required: bool,
    pub secret: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpPreset {
    pub id: String,
    pub name: String,
    pub category: String,
    pub description: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<McpEnvPrompt>,
    pub docs_url: String,
}

fn rursor_home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".rursor")
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Validates an HTTP/HTTPS URL and blocks cloud metadata / SSRF targets.
pub fn validate_research_url(raw_url: &str, allow_localhost: bool) -> Result<reqwest::Url, String> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return Err("URL cannot be empty.".to_string());
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|e| format!("Invalid URL '{trimmed}': {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "Unsupported URL scheme '{other}'. Only http:// and https:// are allowed."
            ));
        }
    }

    let Some(host_raw) = parsed.host_str() else {
        return Err(format!("URL '{trimmed}' is missing a host."));
    };
    let host = host_raw.to_ascii_lowercase();

    // Always block cloud metadata endpoints regardless of `allow_localhost`.
    if host == "169.254.169.254"
        || host == "metadata.google.internal"
        || host == "metadata.goog"
        || host == "100.100.100.200"
        || host.starts_with("169.254.")
    {
        return Err(format!("Blocked restricted metadata host '{host}'."));
    }

    if !allow_localhost {
        let is_loopback = host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host == "[::1]"
            || host == "0.0.0.0"
            || host.starts_with("127.");
        if is_loopback {
            return Err(format!("Loopback host '{host}' is disabled for external web fetch."));
        }
    }

    Ok(parsed)
}

pub fn decode_html_entities(input: &str) -> String {
    let mut out = input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–");

    // Decode numeric decimal entities like &#123; and hex &#x7B;
    while let Some(start) = out.find("&#") {
        let rest = &out[start + 2..];
        let Some(semi_rel) = rest.find(';') else {
            break;
        };
        if semi_rel == 0 || semi_rel > 8 {
            break;
        }
        let code_str = &rest[..semi_rel];
        let parsed_char = if let Some(hex) = code_str.strip_prefix('x').or_else(|| code_str.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else {
            code_str.parse::<u32>().ok().and_then(char::from_u32)
        };
        let Some(ch) = parsed_char else {
            break;
        };
        let end = start + 2 + semi_rel + 1;
        out.replace_range(start..end, &ch.to_string());
    }
    out
}

fn strip_tag_block_case_insensitive(html: &str, tag: &str) -> String {
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}>");
    let mut current = html.to_string();
    loop {
        let lower = current.to_ascii_lowercase();
        let Some(start) = lower.find(&open_pat) else {
            break;
        };
        // Ensure boundary (`<script>` or `<script ...>`)
        let next_ch = lower[start + open_pat.len()..].chars().next();
        if !matches!(next_ch, Some('>') | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('/')) {
            break;
        }
        if let Some(rel_end) = lower[start..].find(&close_pat) {
            let end = start + rel_end + close_pat.len();
            current.replace_range(start..end, " ");
        } else if let Some(tag_close) = lower[start..].find('>') {
            current.replace_range(start..start + tag_close + 1, " ");
        } else {
            break;
        }
    }
    current
}

fn extract_html_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else {
        return String::new();
    };
    let Some(gt_rel) = lower[open..].find('>') else {
        return String::new();
    };
    let content_start = open + gt_rel + 1;
    let Some(close_rel) = lower[content_start..].find("</title>") else {
        return String::new();
    };
    let raw = &html[content_start..content_start + close_rel];
    decode_html_entities(&strip_remaining_tags(raw))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_remaining_tags(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for ch in fragment.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn extract_attr_value(tag_attrs: &str, attr_name: &str) -> Option<String> {
    let lower = tag_attrs.to_ascii_lowercase();
    let needle = format!("{attr_name}=");
    let pos = lower.find(&needle)?;
    let after = tag_attrs[pos + needle.len()..].trim_start();
    if let Some(stripped) = after.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].trim().to_string())
    } else if let Some(stripped) = after.strip_prefix('\'') {
        let end = stripped.find('\'')?;
        Some(stripped[..end].trim().to_string())
    } else {
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(after.len());
        Some(after[..end].trim().to_string())
    }
}

/// Converts raw HTML into clean, structured Markdown while stripping scripts, styles, and nav chrome.
pub fn html_to_clean_markdown(html: &str) -> (String, String) {
    let title = extract_html_title(html);
    let mut cleaned = html.to_string();
    for noisy_tag in ["script", "style", "noscript", "svg", "nav", "footer", "header", "iframe"] {
        cleaned = strip_tag_block_case_insensitive(&cleaned, noisy_tag);
    }

    let mut out = String::with_capacity(cleaned.len());
    let mut idx = 0;
    let bytes = cleaned.as_bytes();
    let mut in_pre = false;
    let mut pending_link_href: Option<String> = None;
    let mut link_text_buf = String::new();

    while idx < bytes.len() {
        if bytes[idx] == b'<' {
            if let Some(rel_gt) = cleaned[idx..].find('>') {
                let raw_tag = &cleaned[idx + 1..idx + rel_gt];
                let trimmed_tag = raw_tag.trim();
                let is_closing = trimmed_tag.starts_with('/');
                let tag_body = if is_closing {
                    trimmed_tag[1..].trim()
                } else {
                    trimmed_tag.trim_end_matches('/').trim()
                };
                let tag_name_end = tag_body
                    .find(|c: char| c.is_whitespace())
                    .unwrap_or(tag_body.len());
                let tag_name = tag_body[..tag_name_end].to_ascii_lowercase();
                let tag_attrs = &tag_body[tag_name_end..];

                match (is_closing, tag_name.as_str()) {
                    (false, "h1") => out.push_str("\n\n# "),
                    (false, "h2") => out.push_str("\n\n## "),
                    (false, "h3") => out.push_str("\n\n### "),
                    (false, "h4") => out.push_str("\n\n#### "),
                    (false, "h5") => out.push_str("\n\n##### "),
                    (false, "h6") => out.push_str("\n\n###### "),
                    (true, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => out.push_str("\n\n"),
                    (false, "pre") => {
                        in_pre = true;
                        out.push_str("\n\n```\n");
                    }
                    (true, "pre") => {
                        in_pre = false;
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push_str("```\n\n");
                    }
                    (false, "code") if !in_pre => out.push('`'),
                    (true, "code") if !in_pre => out.push('`'),
                    (false, "li") => out.push_str("\n- "),
                    (true, "li") => out.push('\n'),
                    (false, "p" | "div" | "section" | "article" | "blockquote") => out.push_str("\n\n"),
                    (true, "p" | "div" | "section" | "article" | "blockquote") => out.push_str("\n\n"),
                    (false, "br") => out.push('\n'),
                    (false, "tr") => out.push('\n'),
                    (false, "td" | "th") => out.push_str(" | "),
                    (false, "a") if !in_pre => {
                        if let Some(href) = extract_attr_value(tag_attrs, "href") {
                            if !href.starts_with("javascript:") && !href.is_empty() {
                                pending_link_href = Some(href);
                                link_text_buf.clear();
                            }
                        }
                    }
                    (true, "a") if !in_pre => {
                        if let Some(href) = pending_link_href.take() {
                            let label = decode_html_entities(&link_text_buf)
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ");
                            link_text_buf.clear();
                            if label.is_empty() {
                                out.push_str(&href);
                            } else {
                                out.push_str(&format!("[{label}]({href})"));
                            }
                        }
                    }
                    _ => {}
                }

                idx += rel_gt + 1;
                continue;
            }
        }

        let next_lt = cleaned[idx..].find('<').map(|o| idx + o).unwrap_or(cleaned.len());
        let text_slice = &cleaned[idx..next_lt];
        if pending_link_href.is_some() {
            link_text_buf.push_str(text_slice);
        } else if in_pre {
            out.push_str(&decode_html_entities(text_slice));
        } else {
            let decoded = decode_html_entities(text_slice);
            let collapsed = decoded.replace(['\r', '\n', '\t'], " ");
            out.push_str(&collapsed);
        }
        idx = next_lt;
    }

    // Normalize excessive blank lines and line-leading whitespace outside code fences
    let mut normalized_lines: Vec<String> = Vec::new();
    let mut inside_fence = false;
    let mut consecutive_empty = 0usize;

    for raw_line in out.lines() {
        if raw_line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            consecutive_empty = 0;
            normalized_lines.push("```".to_string());
            continue;
        }
        if inside_fence {
            normalized_lines.push(raw_line.to_string());
        } else {
            let collapsed_spaces = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
            if collapsed_spaces.is_empty() {
                consecutive_empty += 1;
                if consecutive_empty <= 1 && !normalized_lines.is_empty() {
                    normalized_lines.push(String::new());
                }
            } else {
                consecutive_empty = 0;
                normalized_lines.push(collapsed_spaces);
            }
        }
    }

    let final_md = normalized_lines.join("\n").trim().to_string();
    (title, final_md)
}

/// Extracts same-host, same-prefix crawlable `<a href="...">` URLs from HTML.
pub fn extract_same_prefix_links(base_url: &reqwest::Url, prefix_path: &str, html: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let lower = html.to_ascii_lowercase();
    let mut search_from = 0usize;

    while let Some(rel_a) = lower[search_from..].find("<a ") {
        let start = search_from + rel_a;
        let Some(rel_gt) = lower[start..].find('>') else {
            break;
        };
        let tag_slice = &html[start..start + rel_gt];
        search_from = start + rel_gt + 1;

        let Some(href_raw) = extract_attr_value(tag_slice, "href") else {
            continue;
        };
        if href_raw.starts_with('#')
            || href_raw.starts_with("mailto:")
            || href_raw.starts_with("javascript:")
        {
            continue;
        }
        let Ok(mut joined) = base_url.join(&href_raw) else {
            continue;
        };
        joined.set_fragment(None);
        if joined.scheme() != base_url.scheme() || joined.host_str() != base_url.host_str() || joined.port() != base_url.port() {
            continue;
        }
        if !joined.path().starts_with(prefix_path) {
            continue;
        }
        // Skip binary assets
        let lower_path = joined.path().to_ascii_lowercase();
        if [".png", ".jpg", ".jpeg", ".gif", ".svg", ".pdf", ".zip", ".mp4", ".woff", ".css", ".js"]
            .iter()
            .any(|ext| lower_path.ends_with(ext))
        {
            continue;
        }
        let canonical = joined.to_string();
        if seen.insert(canonical.clone()) {
            results.push(canonical);
        }
    }

    results
}

/// Fetches a single HTTP/HTTPS URL and converts it to clean LLM-ready Markdown.
pub async fn fetch_url_as_markdown(
    http: &Client,
    raw_url: &str,
    max_chars: usize,
    allow_localhost: bool,
) -> Result<UrlMarkdownResult, String> {
    let parsed = validate_research_url(raw_url, allow_localhost)?;
    let resp = http
        .get(parsed.clone())
        .header("User-Agent", "Rursor-Research-Engine/1.0")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch '{}': {e}", parsed))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {} while fetching '{}'", status.as_u16(), parsed));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed reading response body from '{}': {e}", parsed))?;

    let (title, mut markdown) = if content_type.contains("text/plain")
        || content_type.contains("text/markdown")
        || parsed.path().ends_with(".md")
        || parsed.path().ends_with(".txt")
    {
        let inferred_title = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_start_matches('#').trim().to_string())
            .unwrap_or_else(|| parsed.path().to_string());
        (inferred_title, body)
    } else {
        html_to_clean_markdown(&body)
    };

    let limit = max_chars.max(500);
    let mut truncated = false;
    if markdown.chars().count() > limit {
        markdown = markdown.chars().take(limit).collect::<String>();
        truncated = true;
    }

    Ok(UrlMarkdownResult {
        url: parsed.to_string(),
        title,
        markdown,
        truncated,
    })
}

/// Parses a Gemini `generateContent` JSON payload containing `groundingMetadata`.
pub fn parse_gemini_grounded_search_response(query: &str, payload: &Value) -> Result<WebSearchResponse, String> {
    let candidate = payload
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .ok_or_else(|| "Gemini search response contained no candidates.".to_string())?;

    let mut answer_parts = Vec::new();
    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for p in parts {
            if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                answer_parts.push(text);
            }
        }
    }
    let synthesized_answer = answer_parts.join("").trim().to_string();

    let mut citations: Vec<WebSearchHit> = Vec::new();
    let mut seen_urls = HashSet::new();
    if let Some(chunks) = candidate
        .get("groundingMetadata")
        .and_then(|gm| gm.get("groundingChunks"))
        .and_then(|gc| gc.as_array())
    {
        for chunk in chunks {
            if let Some(web) = chunk.get("web") {
                let url = web
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let title = web
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&url)
                    .trim()
                    .to_string();
                if !url.is_empty() && seen_urls.insert(url.clone()) {
                    citations.push(WebSearchHit {
                        title,
                        url,
                        snippet: String::new(),
                    });
                }
            }
        }
    }

    Ok(WebSearchResponse {
        query: query.to_string(),
        synthesized_answer,
        citations,
        backend: "gemini_google_search".to_string(),
    })
}

/// Performs live web search using Gemini Google Search Grounding (when `gemini_api_key` is set)
/// with automatic keyless fallback to DuckDuckGo HTML Lite + Wikipedia OpenSearch.
pub async fn web_search(
    http: &Client,
    query: &str,
    limit: usize,
    gemini_api_key: Option<&str>,
    model_override: Option<&str>,
) -> Result<WebSearchResponse, String> {
    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return Err("Search query cannot be empty.".to_string());
    }
    let capped_limit = limit.clamp(1, 15);

    if let Some(key) = gemini_api_key.filter(|k| !k.trim().is_empty()) {
        let model = model_override
            .filter(|m| !m.trim().is_empty() && m.starts_with("gemini"))
            .unwrap_or("gemini-2.5-flash");
        let endpoint = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}"
        );
        let body = json!({
            "contents": [{
                "role": "user",
                "parts": [{
                    "text": format!(
                        "Search the web and provide an accurate, up-to-date technical answer with key facts for: {trimmed_query}"
                    )
                }]
            }],
            "tools": [{ "google_search": {} }],
            "generationConfig": { "temperature": 0.1 }
        });

        if let Ok(resp) = http.post(&endpoint).json(&body).send().await {
            if resp.status().is_success() {
                if let Ok(val) = resp.json::<Value>().await {
                    if let Ok(mut parsed) = parse_gemini_grounded_search_response(trimmed_query, &val) {
                        if !parsed.synthesized_answer.is_empty() || !parsed.citations.is_empty() {
                            parsed.citations.truncate(capped_limit);
                            return Ok(parsed);
                        }
                    }
                }
            }
        }
    }

    // Keyless fallback: Wikipedia OpenSearch JSON API
    let wiki_resp = http
        .get("https://en.wikipedia.org/w/api.php")
        .query(&[
            ("action", "opensearch"),
            ("search", trimmed_query),
            ("limit", &capped_limit.to_string()),
            ("namespace", "0"),
            ("format", "json"),
        ])
        .header("User-Agent", "Rursor-Research-Engine/1.0")
        .send()
        .await
        .map_err(|e| format!("Web search fallback request failed: {e}"))?;

    let arr = wiki_resp
        .json::<Value>()
        .await
        .map_err(|e| format!("Failed parsing fallback search JSON: {e}"))?;

    let mut citations = Vec::new();
    if let Some(top) = arr.as_array() {
        let titles = top.get(1).and_then(|v| v.as_array());
        let descs = top.get(2).and_then(|v| v.as_array());
        let urls = top.get(3).and_then(|v| v.as_array());
        if let (Some(t_arr), Some(u_arr)) = (titles, urls) {
            for i in 0..t_arr.len().min(u_arr.len()).min(capped_limit) {
                let title = t_arr[i].as_str().unwrap_or("").to_string();
                let url = u_arr[i].as_str().unwrap_or("").to_string();
                let snippet = descs
                    .and_then(|d| d.get(i))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !url.is_empty() {
                    citations.push(WebSearchHit { title, url, snippet });
                }
            }
        }
    }

    let summary = if citations.is_empty() {
        format!("No keyless results found for '{trimmed_query}'. Configure a Gemini API key in Rursor Settings for full Google Search grounding.")
    } else {
        citations
            .iter()
            .map(|c| format!("- **{}** ({}): {}", c.title, c.url, c.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    };

    Ok(WebSearchResponse {
        query: trimmed_query.to_string(),
        synthesized_answer: summary,
        citations,
        backend: "wikipedia_opensearch_fallback".to_string(),
    })
}

// ============================================================================
// Persistent `@docs` Documentation Crawler & Hybrid SQLite FTS5 + Vector Index
// ============================================================================

fn compute_doc_embedding_128(text: &str) -> [f32; EMBED_DIM] {
    let mut vec = [0.0f32; EMBED_DIM];
    for raw_tok in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let tok = raw_tok.trim().to_ascii_lowercase();
        if tok.len() < 2 {
            continue;
        }
        let mut h: u64 = 0xcbf29ce484222325;
        for b in tok.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        let bucket = (h as usize) % EMBED_DIM;
        let sign = if (h >> 32) & 1 == 0 { 1.0f32 } else { -1.0f32 };
        vec[bucket] += sign;
    }
    let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 1e-6 {
        for v in &mut vec {
            *v /= norm;
        }
    }
    vec
}

fn cosine_sim_128(a: &[f32; EMBED_DIM], b_bytes: &[u8]) -> f32 {
    if b_bytes.len() != EMBED_DIM * 4 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    for (i, &av) in a.iter().enumerate() {
        let off = i * 4;
        let bv = f32::from_le_bytes([b_bytes[off], b_bytes[off + 1], b_bytes[off + 2], b_bytes[off + 3]]);
        dot += av * bv;
    }
    dot
}

fn encode_embedding_bytes(vec: &[f32; EMBED_DIM]) -> Vec<u8> {
    let mut out = Vec::with_capacity(EMBED_DIM * 4);
    for &v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn split_markdown_into_chunks(default_title: &str, markdown: &str) -> Vec<(String, String)> {
    let mut chunks: Vec<(String, String)> = Vec::new();
    let mut current_heading = default_title.to_string();
    let mut current_buf = String::new();

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') && trimmed.contains(' ') {
            if !current_buf.trim().is_empty() {
                chunks.push((current_heading.clone(), current_buf.trim().to_string()));
                current_buf.clear();
            }
            current_heading = trimmed.trim_start_matches('#').trim().to_string();
        }
        current_buf.push_str(line);
        current_buf.push('\n');

        if current_buf.len() >= CHUNK_TARGET_CHARS {
            chunks.push((current_heading.clone(), current_buf.trim().to_string()));
            current_buf.clear();
        }
    }
    if !current_buf.trim().is_empty() {
        chunks.push((current_heading, current_buf.trim().to_string()));
    }
    chunks
}

#[derive(Debug, Clone)]
pub struct DocsIndexStore {
    db_path: PathBuf,
}

impl Default for DocsIndexStore {
    fn default() -> Self {
        Self::default_store()
    }
}

impl DocsIndexStore {
    pub fn default_store() -> Self {
        Self {
            db_path: rursor_home_dir().join("docs-index").join("docs.sqlite"),
        }
    }

    pub fn with_db_path(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    fn open_conn(&self) -> Result<Connection, String> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed creating docs-index dir '{}': {e}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)
            .map_err(|e| format!("Failed opening docs SQLite '{}': {e}", self.db_path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS doc_sources (
                name TEXT PRIMARY KEY,
                root_url TEXT NOT NULL,
                pages_indexed INTEGER NOT NULL,
                chunks_indexed INTEGER NOT NULL,
                used_llms_txt INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS doc_chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_name TEXT NOT NULL,
                url TEXT NOT NULL,
                title TEXT NOT NULL,
                section_heading TEXT NOT NULL,
                content TEXT NOT NULL,
                embedding BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_doc_chunks_source ON doc_chunks(source_name);
            CREATE VIRTUAL TABLE IF NOT EXISTS doc_chunks_fts USING fts5(
                chunk_id UNINDEXED,
                source_name,
                title,
                section_heading,
                content
            );",
        )
        .map_err(|e| format!("Failed initializing docs SQLite schema: {e}"))?;
        Ok(conn)
    }

    pub async fn crawl_and_index(
        &self,
        http: &Client,
        name: &str,
        start_url: &str,
        max_pages: usize,
        allow_localhost: bool,
    ) -> Result<DocSourceSummary, String> {
        let client = http.clone();
        self.crawl_with_fetcher(name, start_url, max_pages, allow_localhost, move |url| {
            let c = client.clone();
            async move {
                let resp = c
                    .get(&url)
                    .header("User-Agent", "Rursor-Research-Engine/1.0")
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("HTTP {}", resp.status().as_u16()));
                }
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("text/html")
                    .to_ascii_lowercase();
                let body = resp.text().await.map_err(|e| e.to_string())?;
                Ok((content_type, body))
            }
        })
        .await
    }

    pub async fn crawl_with_fetcher<F, Fut>(
        &self,
        name: &str,
        start_url: &str,
        max_pages: usize,
        allow_localhost: bool,
        mut fetcher: F,
    ) -> Result<DocSourceSummary, String>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<(String, String), String>>,
    {
        let clean_name = name.trim();
        if clean_name.is_empty() {
            return Err("Documentation source name cannot be empty.".to_string());
        }
        let start_parsed = validate_research_url(start_url, allow_localhost)?;
        let capped_pages = max_pages.clamp(1, MAX_CRAWL_PAGES_CAP);

        let mut pages: Vec<(String, String, String)> = Vec::new();
        let mut used_llms_txt = false;

        // 1. Fast path: check `/llms-full.txt` or `/llms.txt` if `start_url` is a root/docs root or direct `.txt`
        if start_parsed.path().ends_with("llms.txt") || start_parsed.path().ends_with("llms-full.txt") {
            if let Ok((_ct, body)) = fetcher(start_parsed.to_string()).await {
                if !body.trim().is_empty() {
                    used_llms_txt = true;
                    pages.push((start_parsed.to_string(), clean_name.to_string(), body));
                }
            }
        } else {
            for candidate_suffix in ["/llms-full.txt", "/llms.txt"] {
                if let Ok(cand_url) = start_parsed.join(candidate_suffix) {
                    if let Ok((ct, text)) = fetcher(cand_url.to_string()).await {
                        let is_plain = ct.contains("text/plain") || ct.contains("text/markdown");
                        if is_plain && text.trim().len() >= 80 && !text.trim_start().starts_with("<!DOCTYPE") {
                            used_llms_txt = true;
                            pages.push((cand_url.to_string(), clean_name.to_string(), text));
                            break;
                        }
                    }
                }
            }
        }

        // 2. Standard BFS same-prefix HTML crawl
        if pages.is_empty() {
            let prefix_path = {
                let p = start_parsed.path();
                if p.is_empty() || p == "/" {
                    "/".to_string()
                } else if p.ends_with('/') {
                    p.to_string()
                } else if let Some(idx) = p.rfind('/') {
                    if idx == 0 {
                        "/".to_string()
                    } else {
                        p[..=idx].to_string()
                    }
                } else {
                    "/".to_string()
                }
            };

            let mut queue: VecDeque<String> = VecDeque::new();
            let mut visited: HashSet<String> = HashSet::new();
            let mut start_clean = start_parsed.clone();
            start_clean.set_fragment(None);
            queue.push_back(start_clean.to_string());
            visited.insert(start_clean.to_string());

            while let Some(next_url) = queue.pop_front() {
                if pages.len() >= capped_pages {
                    break;
                }
                let Ok(url_obj) = validate_research_url(&next_url, allow_localhost) else {
                    continue;
                };
                let Ok((_ct, raw_body)) = fetcher(url_obj.to_string()).await else {
                    continue;
                };
                let (title, md) = html_to_clean_markdown(&raw_body);
                if !md.trim().is_empty() {
                    let effective_title = if title.is_empty() {
                        clean_name.to_string()
                    } else {
                        title
                    };
                    pages.push((url_obj.to_string(), effective_title, md));
                }

                if pages.len() < capped_pages {
                    for link in extract_same_prefix_links(&url_obj, &prefix_path, &raw_body) {
                        if visited.len() < capped_pages * 3 && visited.insert(link.clone()) {
                            queue.push_back(link);
                        }
                    }
                }
            }
        }

        if pages.is_empty() {
            return Err(format!("Could not extract any readable documentation pages from '{start_url}'."));
        }

        self.index_pages(clean_name, start_parsed.as_str(), &pages, used_llms_txt)
    }

    pub fn index_pages(
        &self,
        name: &str,
        root_url: &str,
        pages: &[(String, String, String)],
        used_llms_txt: bool,
    ) -> Result<DocSourceSummary, String> {
        let clean_name = name.trim();
        if clean_name.is_empty() {
            return Err("Documentation source name cannot be empty.".to_string());
        }
        if pages.is_empty() {
            return Err("Pages list cannot be empty.".to_string());
        }

        let mut conn = self.open_conn()?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("Failed starting docs SQLite transaction: {e}"))?;

        tx.execute("DELETE FROM doc_chunks WHERE source_name = ?1", params![clean_name])
            .map_err(|e| format!("Failed clearing previous chunks: {e}"))?;
        tx.execute("DELETE FROM doc_chunks_fts WHERE source_name = ?1", params![clean_name])
            .map_err(|e| format!("Failed clearing previous FTS chunks: {e}"))?;

        let mut chunks_indexed = 0usize;
        for (page_url, page_title, page_md) in pages {
            for (heading, chunk_body) in split_markdown_into_chunks(page_title, page_md) {
                let emb = compute_doc_embedding_128(&format!("{page_title} {heading} {chunk_body}"));
                let emb_bytes = encode_embedding_bytes(&emb);
                tx.execute(
                    "INSERT INTO doc_chunks (source_name, url, title, section_heading, content, embedding)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![clean_name, page_url, page_title, heading, chunk_body, emb_bytes],
                )
                .map_err(|e| format!("Failed inserting doc chunk: {e}"))?;
                let chunk_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO doc_chunks_fts (chunk_id, source_name, title, section_heading, content)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![chunk_id, clean_name, page_title, heading, chunk_body],
                )
                .map_err(|e| format!("Failed inserting FTS doc chunk: {e}"))?;
                chunks_indexed += 1;
            }
        }

        let updated_at = now_epoch_secs();
        tx.execute(
            "INSERT INTO doc_sources (name, root_url, pages_indexed, chunks_indexed, used_llms_txt, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET
               root_url = excluded.root_url,
               pages_indexed = excluded.pages_indexed,
               chunks_indexed = excluded.chunks_indexed,
               used_llms_txt = excluded.used_llms_txt,
               updated_at = excluded.updated_at",
            params![
                clean_name,
                root_url,
                pages.len() as i64,
                chunks_indexed as i64,
                if used_llms_txt { 1i64 } else { 0i64 },
                updated_at as i64
            ],
        )
        .map_err(|e| format!("Failed saving doc_sources record: {e}"))?;

        tx.commit()
            .map_err(|e| format!("Failed committing docs index transaction: {e}"))?;

        Ok(DocSourceSummary {
            name: clean_name.to_string(),
            root_url: root_url.to_string(),
            pages_indexed: pages.len(),
            chunks_indexed,
            updated_at,
            used_llms_txt,
        })
    }

    pub fn search_docs(
        &self,
        query: &str,
        source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DocChunkHit>, String> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let capped_limit = limit.clamp(1, 25);
        let conn = self.open_conn()?;

        let fts_tokens: Vec<String> = trimmed
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 2)
            .map(|t| format!("\"{t}\""))
            .collect();

        let mut rrf_scores: HashMap<i64, f64> = HashMap::new();
        if !fts_tokens.is_empty() {
            let fts_expr = fts_tokens.join(" OR ");
            let mut stmt = conn
                .prepare(
                    "SELECT CAST(chunk_id AS INTEGER), bm25(doc_chunks_fts)
                     FROM doc_chunks_fts
                     WHERE doc_chunks_fts MATCH ?1
                       AND (?2 IS NULL OR source_name = ?2)
                     ORDER BY bm25(doc_chunks_fts)
                     LIMIT 40",
                )
                .map_err(|e| format!("Failed preparing docs FTS query: {e}"))?;
            let rows = stmt
                .query_map(params![fts_expr, source_filter], |row| row.get::<_, i64>(0))
                .map_err(|e| format!("Failed executing docs FTS query: {e}"))?;
            for (rank_idx, id_res) in rows.flatten().enumerate() {
                *rrf_scores.entry(id_res).or_insert(0.0) += 1.5 / (RRF_K + (rank_idx + 1) as f64);
            }
        }

        let query_emb = compute_doc_embedding_128(trimmed);
        let mut all_stmt = conn
            .prepare(
                "SELECT id, source_name, url, title, section_heading, content, embedding
                 FROM doc_chunks
                 WHERE (?1 IS NULL OR source_name = ?1)",
            )
            .map_err(|e| format!("Failed preparing docs vector scan: {e}"))?;

        #[allow(clippy::type_complexity)]
        let mut records: HashMap<i64, (String, String, String, String, String)> = HashMap::new();
        let mut vec_scored: Vec<(i64, f32)> = Vec::new();

        let iter = all_stmt
            .query_map(params![source_filter], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                ))
            })
            .map_err(|e| format!("Failed scanning doc_chunks: {e}"))?;

        for row in iter.flatten() {
            let (id, src, url, title, heading, content, emb_blob) = row;
            let sim = cosine_sim_128(&query_emb, &emb_blob);
            if sim > 0.01 {
                vec_scored.push((id, sim));
            }
            records.insert(id, (src, url, title, heading, content));
        }

        vec_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank_idx, (id, _)) in vec_scored.into_iter().take(40).enumerate() {
            *rrf_scores.entry(id).or_insert(0.0) += 1.0 / (RRF_K + (rank_idx + 1) as f64);
        }

        let mut ranked: Vec<(i64, f64)> = rrf_scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut hits = Vec::new();
        for (id, score) in ranked.into_iter().take(capped_limit) {
            if let Some((source_name, url, title, section_heading, content)) = records.remove(&id) {
                hits.push(DocChunkHit {
                    source_name,
                    url,
                    title,
                    section_heading,
                    content,
                    score,
                });
            }
        }
        Ok(hits)
    }

    pub fn list_sources(&self) -> Result<Vec<DocSourceSummary>, String> {
        let conn = self.open_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT name, root_url, pages_indexed, chunks_indexed, used_llms_txt, updated_at
                 FROM doc_sources
                 ORDER BY updated_at DESC, name ASC",
            )
            .map_err(|e| format!("Failed listing doc sources: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DocSourceSummary {
                    name: row.get(0)?,
                    root_url: row.get(1)?,
                    pages_indexed: row.get::<_, i64>(2)?.max(0) as usize,
                    chunks_indexed: row.get::<_, i64>(3)?.max(0) as usize,
                    used_llms_txt: row.get::<_, i64>(4)? != 0,
                    updated_at: row.get::<_, i64>(5)?.max(0) as u64,
                })
            })
            .map_err(|e| format!("Failed reading doc sources: {e}"))?;
        Ok(rows.flatten().collect())
    }

    pub fn remove_source(&self, name: &str) -> Result<bool, String> {
        let clean = name.trim();
        let mut conn = self.open_conn()?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("Failed starting remove_source transaction: {e}"))?;
        tx.execute("DELETE FROM doc_chunks WHERE source_name = ?1", params![clean])
            .map_err(|e| format!("Failed deleting doc chunks: {e}"))?;
        tx.execute("DELETE FROM doc_chunks_fts WHERE source_name = ?1", params![clean])
            .map_err(|e| format!("Failed deleting doc FTS chunks: {e}"))?;
        let removed = tx
            .execute("DELETE FROM doc_sources WHERE name = ?1", params![clean])
            .map_err(|e| format!("Failed deleting doc_sources entry: {e}"))?;
        tx.commit()
            .map_err(|e| format!("Failed committing remove_source: {e}"))?;
        Ok(removed > 0)
    }
}

// ============================================================================
// Curated 1-Click MCP Server Registry
// ============================================================================

pub fn builtin_mcp_catalog() -> Vec<McpPreset> {
    vec![
        McpPreset {
            id: "github".to_string(),
            name: "GitHub Official MCP".to_string(),
            category: "Developer Tools".to_string(),
            description: "Manage PRs, issues, branches, code search, and commits directly from Rursor.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-github".to_string()],
            env_keys: vec![McpEnvPrompt {
                key: "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                label: "GitHub Personal Access Token (repo/read:org)".to_string(),
                required: true,
                secret: true,
            }],
            docs_url: "https://github.com/modelcontextprotocol/servers".to_string(),
        },
        McpPreset {
            id: "postgres".to_string(),
            name: "PostgreSQL Schema & Query".to_string(),
            category: "Databases".to_string(),
            description: "Inspect PostgreSQL tables, schemas, indexes, and run read-only SQL queries.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-postgres".to_string()],
            env_keys: vec![McpEnvPrompt {
                key: "POSTGRES_URL".to_string(),
                label: "PostgreSQL Connection URI (postgresql://user:pass@localhost:5432/db)".to_string(),
                required: true,
                secret: true,
            }],
            docs_url: "https://github.com/modelcontextprotocol/servers".to_string(),
        },
        McpPreset {
            id: "playwright".to_string(),
            name: "Playwright Browser Automation".to_string(),
            category: "Browser & QA".to_string(),
            description: "Full Playwright accessibility-tree browser automation MCP server.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@playwright/mcp@latest".to_string()],
            env_keys: vec![],
            docs_url: "https://github.com/microsoft/playwright-mcp".to_string(),
        },
        McpPreset {
            id: "sqlite".to_string(),
            name: "SQLite Local Database".to_string(),
            category: "Databases".to_string(),
            description: "Query local SQLite databases and generate business/data insights.".to_string(),
            command: "uvx".to_string(),
            args: vec!["mcp-server-sqlite".to_string(), "--db-path".to_string(), "./data.db".to_string()],
            env_keys: vec![],
            docs_url: "https://github.com/modelcontextprotocol/servers".to_string(),
        },
        McpPreset {
            id: "filesystem".to_string(),
            name: "Scoped Filesystem Bridge".to_string(),
            category: "System".to_string(),
            description: "Expose controlled external directories safely to the Agent.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string(), ".".to_string()],
            env_keys: vec![],
            docs_url: "https://github.com/modelcontextprotocol/servers".to_string(),
        },
        McpPreset {
            id: "linear".to_string(),
            name: "Linear Issues & Cycles".to_string(),
            category: "Project Management".to_string(),
            description: "Read/update Linear tickets, projects, and sprint cycles.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "mcp-remote".to_string(), "https://mcp.linear.app/sse".to_string()],
            env_keys: vec![],
            docs_url: "https://linear.app/docs/mcp".to_string(),
        },
        McpPreset {
            id: "fetch".to_string(),
            name: "MCP Web Fetch".to_string(),
            category: "Web & Docs".to_string(),
            description: "Fetch web URLs and convert HTML pages to clean Markdown.".to_string(),
            command: "uvx".to_string(),
            args: vec!["mcp-server-fetch".to_string()],
            env_keys: vec![],
            docs_url: "https://github.com/modelcontextprotocol/servers".to_string(),
        },
        McpPreset {
            id: "memory".to_string(),
            name: "Knowledge Graph Memory".to_string(),
            category: "Memory".to_string(),
            description: "Persistent entity-relation knowledge graph across projects.".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-memory".to_string()],
            env_keys: vec![],
            docs_url: "https://github.com/modelcontextprotocol/servers".to_string(),
        },
    ]
}

/// Safely merges an MCP server preset into `.rursor/mcp.json` (or `~/.rursor/mcp.json`),
/// preserving all existing `mcpServers` entries.
pub fn install_mcp_preset(
    target_mcp_json_path: &Path,
    preset_id: &str,
    mut env_values: HashMap<String, String>,
    extra_args: Vec<String>,
) -> Result<Value, String> {
    let preset = builtin_mcp_catalog()
        .into_iter()
        .find(|p| p.id == preset_id)
        .ok_or_else(|| format!("Unknown MCP preset '{preset_id}'."))?;

    let mut root_obj: Map<String, Value> = if target_mcp_json_path.exists() {
        let raw = fs::read_to_string(target_mcp_json_path)
            .map_err(|e| format!("Failed reading '{}': {e}", target_mcp_json_path.display()))?;
        if raw.trim().is_empty() {
            Map::new()
        } else {
            serde_json::from_str::<Value>(&raw)
                .map_err(|e| format!("Invalid JSON in '{}': {e}", target_mcp_json_path.display()))?
                .as_object()
                .cloned()
                .ok_or_else(|| "Root of mcp.json must be a JSON object.".to_string())?
        }
    } else {
        Map::new()
    };

    let mut args = preset.args.clone();
    // Special convenience for `postgres`: append `POSTGRES_URL` as CLI arg if provided
    if preset.id == "postgres" {
        if let Some(pg_url) = env_values.remove("POSTGRES_URL") {
            if !pg_url.trim().is_empty() {
                args.push(pg_url);
            }
        }
    }
    args.extend(extra_args);

    let mut server_entry = Map::new();
    server_entry.insert("command".to_string(), Value::String(preset.command));
    server_entry.insert(
        "args".to_string(),
        Value::Array(args.into_iter().map(Value::String).collect()),
    );
    if !env_values.is_empty() {
        let env_map: Map<String, Value> = env_values
            .into_iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        if !env_map.is_empty() {
            server_entry.insert("env".to_string(), Value::Object(env_map));
        }
    }

    let servers_val = root_obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers_obj) = servers_val.as_object_mut() else {
        return Err("`mcpServers` field in mcp.json is not an object.".to_string());
    };
    let entry_value = Value::Object(server_entry);
    servers_obj.insert(preset.id.clone(), entry_value.clone());

    if let Some(parent) = target_mcp_json_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating '{}': {e}", parent.display()))?;
    }
    let serialized = serde_json::to_string_pretty(&Value::Object(root_obj))
        .map_err(|e| format!("Failed serializing mcp.json: {e}"))?;
    fs::write(target_mcp_json_path, format!("{serialized}\n"))
        .map_err(|e| format!("Failed writing '{}': {e}", target_mcp_json_path.display()))?;

    Ok(json!({
        "presetId": preset.id,
        "configPath": target_mcp_json_path.to_string_lossy(),
        "entry": entry_value
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html_to_clean_markdown_and_strips_scripts_and_nav() {
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <title>Tokio &amp; Async Rust</title>
                <script>window.evil = true;</script>
                <style>body { color: red; }</style>
            </head>
            <body>
                <nav><a href="/home">Skip Nav</a></nav>
                <main>
                    <h1>Getting Started</h1>
                    <p>Use <code>tokio::spawn</code> to launch tasks &lt;fast&gt;.</p>
                    <ul>
                        <li>Zero-cost futures</li>
                        <li>Read <a href="https://tokio.rs/docs">Official Docs</a></li>
                    </ul>
                    <pre><code>async fn main() {
    println!("hello");
}</code></pre>
                </main>
                <footer>Copyright 2026</footer>
            </body>
            </html>
        "#;

        let (title, md) = html_to_clean_markdown(html);
        assert_eq!(title, "Tokio & Async Rust");
        assert!(!md.contains("window.evil"));
        assert!(!md.contains("Skip Nav"));
        assert!(!md.contains("Copyright 2026"));
        assert!(md.contains("# Getting Started"));
        assert!(md.contains("`tokio::spawn`"));
        assert!(md.contains("<fast>"));
        assert!(md.contains("[Official Docs](https://tokio.rs/docs)"));
        assert!(md.contains("```\nasync fn main()"));
    }

    #[test]
    fn blocks_ssrf_metadata_and_loopback_by_default() {
        assert!(validate_research_url("https://docs.rs/tokio/latest", false).is_ok());
        assert!(validate_research_url("http://169.254.169.254/latest/meta-data/", true).is_err());
        assert!(validate_research_url("http://metadata.google.internal/computeMetadata/v1/", true).is_err());
        assert!(validate_research_url("file:///etc/passwd", true).is_err());
        assert!(validate_research_url("http://127.0.0.1:8080/docs", false).is_err());
        assert!(validate_research_url("http://127.0.0.1:8080/docs", true).is_ok());
    }

    #[test]
    fn parses_gemini_grounding_metadata_citations() {
        let sample = json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "Rust 2024 stabilizes async closures." }]
                },
                "groundingMetadata": {
                    "groundingChunks": [
                        { "web": { "uri": "https://blog.rust-lang.org/2025/02/rust-1.85/", "title": "Rust Blog" } },
                        { "web": { "uri": "https://doc.rust-lang.org/edition-guide/", "title": "Edition Guide" } }
                    ]
                }
            }]
        });

        let parsed = parse_gemini_grounded_search_response("rust async closures", &sample).unwrap();
        assert_eq!(parsed.backend, "gemini_google_search");
        assert_eq!(parsed.synthesized_answer, "Rust 2024 stabilizes async closures.");
        assert_eq!(parsed.citations.len(), 2);
        assert_eq!(parsed.citations[0].title, "Rust Blog");
    }

    #[tokio::test]
    async fn crawls_mock_docs_site_and_performs_hybrid_rrf_search() {
        let tmp = std::env::temp_dir().join(format!("rursor_docs_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let store = DocsIndexStore::with_db_path(tmp.join("docs.sqlite"));

        let summary = store
            .crawl_with_fetcher(
                "drizzle",
                "https://orm.drizzle.team/docs/index.html",
                10,
                false,
                |url| async move {
                    match url.as_str() {
                        "https://orm.drizzle.team/docs/index.html" => Ok((
                            "text/html".to_string(),
                            r#"<html><head><title>Drizzle Guide</title></head><body>
                                <h1>Drizzle ORM Overview</h1>
                                <p>Drizzle provides type-safe SQL schema definitions.</p>
                                <a href="/docs/migrations.html">Migrations</a>
                                <a href="/other/outside.html">Outside Prefix</a>
                            </body></html>"#
                                .to_string(),
                        )),
                        "https://orm.drizzle.team/docs/migrations.html" => Ok((
                            "text/html".to_string(),
                            r#"<html><head><title>Drizzle Kit Migrations</title></head><body>
                                <h2>Push and Generate</h2>
                                <p>Run <code>npx drizzle-kit push</code> to sync PostgreSQL tables.</p>
                            </body></html>"#
                                .to_string(),
                        )),
                        _ => Err("404 Not Found".to_string()),
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(summary.pages_indexed, 2);
        assert!(summary.chunks_indexed >= 2);
        assert!(!summary.used_llms_txt);

        let hits = store
            .search_docs("drizzle-kit push PostgreSQL migrations", Some("drizzle"), 5)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].content.contains("drizzle-kit push"));

        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "drizzle");

        assert!(store.remove_source("drizzle").unwrap());
        assert!(store.list_sources().unwrap().is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn crawls_llms_txt_fast_path_when_available() {
        let tmp = std::env::temp_dir().join(format!("rursor_llms_txt_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let store = DocsIndexStore::with_db_path(tmp.join("docs.sqlite"));

        let summary = store
            .crawl_with_fetcher("hono", "https://hono.dev/", 5, false, |url| async move {
                if url == "https://hono.dev/llms-full.txt" {
                    Ok((
                        "text/plain".to_string(),
                        "# Hono Web Framework\n\n## Routing\nUse `app.get('/hello', c => c.text('hi'))` on Cloudflare Workers and Bun with ultra-low overhead."
                            .to_string(),
                    ))
                } else {
                    Err("404".to_string())
                }
            })
            .await
            .unwrap();

        assert!(summary.used_llms_txt);
        assert_eq!(summary.pages_indexed, 1);
        let hits = store.search_docs("Cloudflare Workers routing app.get", None, 3).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].content.contains("app.get"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn installs_mcp_presets_preserving_existing_servers() {
        let tmp = std::env::temp_dir().join(format!("rursor_mcp_preset_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let mcp_path = tmp.join(".rursor").join("mcp.json");
        fs::create_dir_all(mcp_path.parent().unwrap()).unwrap();
        fs::write(
            &mcp_path,
            r#"{"mcpServers":{"custom":{"command":"node","args":["srv.js"]}}}"#,
        )
        .unwrap();

        let mut env = HashMap::new();
        env.insert("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), "ghp_test123".to_string());
        install_mcp_preset(&mcp_path, "github", env, vec![]).unwrap();

        let parsed: Value = serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        let servers = parsed.get("mcpServers").and_then(|v| v.as_object()).unwrap();
        assert!(servers.contains_key("custom"));
        assert!(servers.contains_key("github"));
        assert_eq!(
            servers["github"]["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"].as_str(),
            Some("ghp_test123")
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
