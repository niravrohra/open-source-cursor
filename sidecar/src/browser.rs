#![allow(dead_code)]

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};

const RFC6455_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const WS_OPCODE_CONTINUATION: u8 = 0x0;
const WS_OPCODE_TEXT: u8 = 0x1;
const WS_OPCODE_BINARY: u8 = 0x2;
const WS_OPCODE_CLOSE: u8 = 0x8;
const WS_OPCODE_PING: u8 = 0x9;
const WS_OPCODE_PONG: u8 = 0xA;

const MAX_WS_FRAME_BYTES: usize = 128 * 1024 * 1024;
const MAX_DIAGNOSTIC_ENTRIES: usize = 200;
const MAX_TRACKED_NETWORK_REQUESTS: usize = 512;
const DEFAULT_VIEWPORT_WIDTH: u32 = 1280;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 800;
const DEFAULT_CDP_TIMEOUT_MS: u64 = 30_000;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_pseudo_random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let seq = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let mut x = nanos
        ^ seq.wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ pid.rotate_left(17);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn generate_mask_key() -> [u8; 4] {
    let r = next_pseudo_random_u64();
    [
        (r & 0xFF) as u8,
        ((r >> 8) & 0xFF) as u8,
        ((r >> 16) & 0xFF) as u8,
        ((r >> 24) & 0xFF) as u8,
    ]
}

fn generate_sec_websocket_key() -> String {
    let a = next_pseudo_random_u64().to_le_bytes();
    let b = next_pseudo_random_u64().to_le_bytes();
    let mut nonce = [0u8; 16];
    nonce[..8].copy_from_slice(&a);
    nonce[8..].copy_from_slice(&b);
    base64_encode(&nonce)
}

/// Pure-Rust FIPS 180-4 SHA-1 implementation for RFC 6455 `Sec-WebSocket-Accept` computation.
pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

/// Pure-Rust RFC 4648 standard Base64 encoder.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}

/// Pure-Rust RFC 4648 standard Base64 decoder (ignores ASCII whitespace).
pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(b: u8) -> Result<u8, String> {
        match b {
            b'A'..=b'Z' => Ok(b - b'A'),
            b'a'..=b'z' => Ok(b - b'a' + 26),
            b'0'..=b'9' => Ok(b - b'0' + 52),
            b'+' | b'-' => Ok(62),
            b'/' | b'_' => Ok(63),
            _ => Err(format!("Invalid base64 byte: 0x{b:02x}")),
        }
    }

    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if clean.is_empty() {
        return Ok(Vec::new());
    }
    if clean.len() % 4 != 0 {
        return Err(format!(
            "Invalid base64 length {} (must be a multiple of 4)",
            clean.len()
        ));
    }

    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    for quad in clean.chunks_exact(4) {
        let pad = if quad[2] == b'=' && quad[3] == b'=' {
            2
        } else if quad[3] == b'=' {
            1
        } else {
            0
        };
        let a = val(quad[0])? as u32;
        let b = val(quad[1])? as u32;
        let c = if pad >= 2 { 0 } else { val(quad[2])? as u32 };
        let d = if pad >= 1 { 0 } else { val(quad[3])? as u32 };
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((triple >> 16) & 0xFF) as u8);
        if pad < 2 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if pad < 1 {
            out.push((triple & 0xFF) as u8);
        }
    }
    Ok(out)
}

/// Computes RFC 6455 `Sec-WebSocket-Accept` from `Sec-WebSocket-Key`.
pub fn compute_websocket_accept(sec_websocket_key: &str) -> String {
    let mut combined = String::with_capacity(sec_websocket_key.len() + RFC6455_GUID.len());
    combined.push_str(sec_websocket_key.trim());
    combined.push_str(RFC6455_GUID);
    let digest = sha1_digest(combined.as_bytes());
    base64_encode(&digest)
}

// ============================================================================
// Data Models & JSON-RPC Payloads
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceHint {
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SomElement {
    pub id: u32,
    pub tag: String,
    #[serde(default, alias = "input_type")]
    pub input_type: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    pub label: String,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub href: Option<String>,
    pub selector: String,
    pub bounds: ElementBounds,
    #[serde(default, alias = "source_hint")]
    pub source_hint: Option<SourceHint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshot {
    pub url: String,
    pub title: String,
    #[serde(alias = "viewport_width")]
    pub viewport_width: u32,
    #[serde(alias = "viewport_height")]
    pub viewport_height: u32,
    #[serde(alias = "draw_som_badges")]
    pub draw_som_badges: bool,
    pub elements: Vec<SomElement>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "screenshot_base64")]
    pub screenshot_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "screenshot_path")]
    pub screenshot_path: Option<String>,
    #[serde(default, alias = "text_summary")]
    pub text_summary: String,
    #[serde(default, alias = "timestamp_ms")]
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateResult {
    pub url: String,
    pub title: String,
    pub status: u16,
    #[serde(alias = "viewport_width")]
    pub viewport_width: u32,
    #[serde(alias = "viewport_height")]
    pub viewport_height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserAction {
    Click {
        #[serde(default, alias = "somId")]
        som_id: Option<u32>,
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
    },
    Type {
        #[serde(default, alias = "somId")]
        som_id: Option<u32>,
        #[serde(default)]
        selector: Option<String>,
        text: String,
        #[serde(default, alias = "pressEnter")]
        press_enter: bool,
    },
    Scroll {
        #[serde(default, alias = "deltaX")]
        delta_x: f64,
        #[serde(default, alias = "deltaY")]
        delta_y: f64,
    },
    Evaluate {
        expression: String,
    },
    #[serde(alias = "clearOverlay")]
    ClearOverlay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserActionResult {
    pub ok: bool,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "resolved_x")]
    pub resolved_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "resolved_y")]
    pub resolved_y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConsoleEntry {
    pub level: String,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(alias = "timestamp_ms")]
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserNetworkError {
    #[serde(alias = "request_id")]
    pub request_id: String,
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(alias = "error_text")]
    pub error_text: String,
    #[serde(default, alias = "resource_type")]
    pub resource_type: Option<String>,
    #[serde(alias = "timestamp_ms")]
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDiagnostics {
    #[serde(default, alias = "console_logs")]
    pub console_logs: Vec<BrowserConsoleEntry>,
    #[serde(default, alias = "network_errors")]
    pub network_errors: Vec<BrowserNetworkError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfExportResult {
    #[serde(alias = "output_path")]
    pub output_path: String,
    #[serde(alias = "bytes_written")]
    pub bytes_written: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WalkthroughStep {
    Navigate {
        url: String,
        #[serde(default, alias = "viewportWidth")]
        viewport_width: Option<u32>,
        #[serde(default, alias = "viewportHeight")]
        viewport_height: Option<u32>,
    },
    Click {
        #[serde(default, alias = "somId")]
        som_id: Option<u32>,
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
    },
    Type {
        #[serde(default, alias = "somId")]
        som_id: Option<u32>,
        #[serde(default)]
        selector: Option<String>,
        text: String,
        #[serde(default, alias = "pressEnter")]
        press_enter: bool,
    },
    Scroll {
        #[serde(default, alias = "deltaX")]
        delta_x: f64,
        #[serde(default, alias = "deltaY")]
        delta_y: f64,
    },
    #[serde(alias = "waitMs", alias = "wait")]
    WaitMs {
        ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkthroughRecording {
    #[serde(alias = "output_mp4_path")]
    pub output_mp4_path: String,
    #[serde(alias = "frame_count")]
    pub frame_count: usize,
    #[serde(alias = "step_count")]
    pub step_count: usize,
    #[serde(alias = "duration_ms")]
    pub duration_ms: u64,
    #[serde(alias = "bytes_written")]
    pub bytes_written: usize,
    pub encoder: String,
}

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
}

// ============================================================================
// Strict URL Safety Validator
// ============================================================================

/// Validates a browser navigation URL:
/// - Allows `http://` and `https://` (including `localhost` and `127.0.0.1` for local dev servers).
/// - Blocks `file://`, `javascript:`, `data:`, `chrome://`, `devtools://`, `about:`, and
///   cloud metadata endpoints (`169.254.169.254`, link-local `169.254.0.0/16`, `metadata.google.internal`, etc.).
pub fn validate_browser_url(raw_url: &str) -> Result<String, String> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return Err("URL cannot be empty.".to_string());
    }
    if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("URL must not contain whitespace or control characters.".to_string());
    }

    let lower = trimmed.to_ascii_lowercase();
    for forbidden in [
        "file:",
        "javascript:",
        "vbscript:",
        "data:",
        "chrome:",
        "chrome-extension:",
        "devtools:",
        "about:",
        "blob:",
        "ws:",
        "wss:",
        "ftp:",
    ] {
        if lower.starts_with(forbidden) {
            return Err(format!("Blocked unsafe URL scheme in '{trimmed}'."));
        }
    }

    let (scheme, after_scheme) = if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if lower.starts_with("http://") {
        ("http", &trimmed[7..])
    } else if lower.starts_with("https://") {
        ("https", &trimmed[8..])
    } else {
        return Err(format!(
            "Unsupported URL scheme in '{trimmed}'. Only http:// and https:// are allowed."
        ));
    };

    let authority_end = after_scheme
        .find(|c| ['/', '?', '#'].contains(&c))
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let path_and_more = &after_scheme[authority_end..];

    if authority.is_empty() {
        return Err(format!("URL '{trimmed}' is missing a hostname."));
    }
    if authority.contains('@') {
        return Err("URLs with embedded userinfo credentials ('@') are not allowed.".to_string());
    }

    let (host, port_opt) = if authority.starts_with('[') {
        let close = authority
            .find(']')
            .ok_or_else(|| format!("Invalid IPv6 host in URL '{trimmed}'."))?;
        let ipv6_host = &authority[..=close];
        let rest = &authority[close + 1..];
        let port = if let Some(port_str) = rest.strip_prefix(':') {
            let p: u16 = port_str
                .parse()
                .map_err(|_| format!("Invalid port '{port_str}' in URL '{trimmed}'."))?;
            if p == 0 {
                return Err("Port 0 is not allowed.".to_string());
            }
            Some(p)
        } else if rest.is_empty() {
            None
        } else {
            return Err(format!("Invalid authority '{authority}' in URL '{trimmed}'."));
        };
        (ipv6_host, port)
    } else if let Some((h, port_str)) = authority.rsplit_once(':') {
        let p: u16 = port_str
            .parse()
            .map_err(|_| format!("Invalid port '{port_str}' in URL '{trimmed}'."))?;
        if p == 0 {
            return Err("Port 0 is not allowed.".to_string());
        }
        (h, Some(p))
    } else {
        (authority, None)
    };

    if host.is_empty() {
        return Err(format!("URL '{trimmed}' has an empty hostname."));
    }

    let host_lower = host.trim_end_matches('.').to_ascii_lowercase();

    // Block cloud metadata hostnames and link-local / IMDS IP addresses
    if matches!(
        host_lower.as_str(),
        "169.254.169.254"
            | "metadata"
            | "metadata.google.internal"
            | "metadata.goog"
            | "instance-data"
            | "100.100.100.200"
            | "[fd00:ec2::254]"
    ) || host_lower.starts_with("169.254.")
        || host_lower.contains("169.254.169.254")
        || host_lower.contains("a9fe:a9fe")
    {
        return Err(format!(
            "Blocked cloud metadata / link-local endpoint '{host}'."
        ));
    }

    if let Ok(ipv4) = host_lower.parse::<std::net::Ipv4Addr>() {
        let octets = ipv4.octets();
        if octets[0] == 169 && octets[1] == 254 {
            return Err(format!("Blocked link-local / metadata IP '{ipv4}'."));
        }
        if octets == [100, 100, 100, 200] {
            return Err(format!("Blocked cloud metadata IP '{ipv4}'."));
        }
    } else if let Ok(int_ip) = host_lower.parse::<u32>() {
        let octets = int_ip.to_be_bytes();
        if (octets[0] == 169 && octets[1] == 254) || octets == [100, 100, 100, 200] {
            return Err("Blocked integer-encoded cloud metadata IP.".to_string());
        }
    } else if let Some(hex_str) = host_lower.strip_prefix("0x") {
        if let Ok(hex_ip) = u32::from_str_radix(hex_str, 16) {
            let octets = hex_ip.to_be_bytes();
            if (octets[0] == 169 && octets[1] == 254) || octets == [100, 100, 100, 200] {
                return Err("Blocked hex-encoded cloud metadata IP.".to_string());
            }
        }
    }

    let normalized_path = if path_and_more.is_empty() {
        ""
    } else {
        path_and_more
    };
    let port_part = port_opt.map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("{scheme}://{host}{port_part}{normalized_path}"))
}

// ============================================================================
// Clean RFC 6455 Local WebSocket Client (`CdpWsConnection`)
// ============================================================================

/// Parses a local `ws://127.0.0.1:<port>/<path>` or `ws://localhost:<port>/<path>` URL.
pub fn parse_local_ws_url(ws_url: &str) -> Result<(String, u16, String), String> {
    let trimmed = ws_url.trim();
    let rest = trimmed
        .strip_prefix("ws://")
        .ok_or_else(|| format!("Expected ws:// URL, got '{trimmed}'"))?;

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    let (host, port_str) = if authority.starts_with('[') {
        let close = authority
            .find(']')
            .ok_or_else(|| format!("Malformed IPv6 WebSocket authority '{authority}'"))?;
        let h = &authority[1..close];
        let p = authority[close + 1..]
            .strip_prefix(':')
            .ok_or_else(|| format!("Missing port in WebSocket URL '{trimmed}'"))?;
        (h, p)
    } else {
        authority
            .rsplit_once(':')
            .ok_or_else(|| format!("Missing port in WebSocket URL '{trimmed}'"))?
    };

    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return Err(format!(
            "Refusing non-local CDP WebSocket host '{host}'. Only loopback hosts are allowed."
        ));
    }

    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("Invalid port '{port_str}' in WebSocket URL '{trimmed}'"))?;
    if port == 0 {
        return Err("WebSocket port cannot be 0.".to_string());
    }

    let connect_host = if host == "localhost" {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    };

    Ok((connect_host, port, path.to_string()))
}

/// Encodes a client-to-server RFC 6455 masked WebSocket frame (`FIN = 1`, `MASK = 1`).
pub fn encode_ws_client_frame(opcode: u8, payload: &[u8], mask_key: [u8; 4]) -> Vec<u8> {
    let len = payload.len();
    let header_len = if len <= 125 {
        2
    } else if len <= 65_535 {
        4
    } else {
        10
    };
    let mut frame = Vec::with_capacity(header_len + 4 + len);
    frame.push(0x80 | (opcode & 0x0F));

    if len <= 125 {
        frame.push(0x80 | (len as u8));
    } else if len <= 65_535 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }

    frame.extend_from_slice(&mask_key);
    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask_key[i & 3]);
    }
    frame
}

/// Encodes an unmasked server-to-client RFC 6455 WebSocket frame (used by mock tests & helpers).
pub fn encode_ws_server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let header_len = if len <= 125 {
        2
    } else if len <= 65_535 {
        4
    } else {
        10
    };
    let mut frame = Vec::with_capacity(header_len + len);
    frame.push(0x80 | (opcode & 0x0F));
    if len <= 125 {
        frame.push(len as u8);
    } else if len <= 65_535 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

struct WsStreamReader<R> {
    stream: R,
    buf: Vec<u8>,
    cursor: usize,
}

impl<R: AsyncRead + Unpin> WsStreamReader<R> {
    fn new(stream: R, leftover: Vec<u8>) -> Self {
        Self {
            stream,
            buf: leftover,
            cursor: 0,
        }
    }

    async fn read_exact_bytes(&mut self, count: usize) -> Result<Vec<u8>, String> {
        let mut scratch = [0u8; 32_768];
        while self.buf.len().saturating_sub(self.cursor) < count {
            let n = self
                .stream
                .read(&mut scratch)
                .await
                .map_err(|e| format!("WebSocket read error: {e}"))?;
            if n == 0 {
                return Err("WebSocket connection closed by peer.".to_string());
            }
            self.buf.extend_from_slice(&scratch[..n]);
        }

        let end = self.cursor + count;
        let out = self.buf[self.cursor..end].to_vec();
        self.cursor = end;

        if self.cursor >= 65_536 && self.cursor * 2 >= self.buf.len() {
            self.buf.drain(..self.cursor);
            self.cursor = 0;
        }
        Ok(out)
    }

    async fn read_raw_frame(&mut self) -> Result<(bool, u8, Vec<u8>), String> {
        let hdr = self.read_exact_bytes(2).await?;
        let fin = (hdr[0] & 0x80) != 0;
        let opcode = hdr[0] & 0x0F;
        let masked = (hdr[1] & 0x80) != 0;
        let len7 = (hdr[1] & 0x7F) as usize;

        let payload_len: usize = if len7 == 126 {
            let ext = self.read_exact_bytes(2).await?;
            u16::from_be_bytes([ext[0], ext[1]]) as usize
        } else if len7 == 127 {
            let ext = self.read_exact_bytes(8).await?;
            let raw = u64::from_be_bytes([
                ext[0], ext[1], ext[2], ext[3], ext[4], ext[5], ext[6], ext[7],
            ]);
            if raw > MAX_WS_FRAME_BYTES as u64 {
                return Err(format!("WebSocket frame exceeds safety cap ({raw} bytes)."));
            }
            raw as usize
        } else {
            len7
        };

        let mask_key = if masked {
            let m = self.read_exact_bytes(4).await?;
            Some([m[0], m[1], m[2], m[3]])
        } else {
            None
        };

        let mut payload = if payload_len > 0 {
            self.read_exact_bytes(payload_len).await?
        } else {
            Vec::new()
        };

        if let Some(mask) = mask_key {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }

        Ok((fin, opcode, payload))
    }
}

#[derive(Default)]
struct DiagnosticsRingBuffer {
    console_logs: VecDeque<BrowserConsoleEntry>,
    network_errors: VecDeque<BrowserNetworkError>,
    request_meta: HashMap<String, (String, String, Option<String>)>,
    request_order: VecDeque<String>,
}

impl DiagnosticsRingBuffer {
    fn push_console(&mut self, entry: BrowserConsoleEntry) {
        if self.console_logs.len() >= MAX_DIAGNOSTIC_ENTRIES {
            self.console_logs.pop_front();
        }
        self.console_logs.push_back(entry);
    }

    fn push_network_error(&mut self, entry: BrowserNetworkError) {
        if self.network_errors.len() >= MAX_DIAGNOSTIC_ENTRIES {
            self.network_errors.pop_front();
        }
        self.network_errors.push_back(entry);
    }

    fn record_request(
        &mut self,
        request_id: String,
        url: String,
        method: String,
        resource_type: Option<String>,
    ) {
        if !self.request_meta.contains_key(&request_id) {
            if self.request_order.len() >= MAX_TRACKED_NETWORK_REQUESTS {
                if let Some(evicted) = self.request_order.pop_front() {
                    self.request_meta.remove(&evicted);
                }
            }
            self.request_order.push_back(request_id.clone());
        }
        self.request_meta
            .insert(request_id, (url, method, resource_type));
    }

    fn ingest_cdp_event(&mut self, method: &str, params: &Value) {
        match method {
            "Runtime.consoleAPICalled" => {
                let level = params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                    .to_string();
                let mut parts = Vec::new();
                if let Some(args) = params.get("args").and_then(Value::as_array) {
                    for arg in args {
                        if let Some(s) = arg.get("value").and_then(Value::as_str) {
                            parts.push(s.to_string());
                        } else if let Some(v) = arg.get("value") {
                            if !v.is_null() {
                                parts.push(v.to_string());
                            } else if let Some(desc) =
                                arg.get("description").and_then(Value::as_str)
                            {
                                parts.push(desc.to_string());
                            } else {
                                parts.push("null".to_string());
                            }
                        } else if let Some(desc) = arg.get("description").and_then(Value::as_str) {
                            parts.push(desc.to_string());
                        }
                    }
                }
                let text = if parts.is_empty() {
                    params
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                } else {
                    parts.join(" ")
                };

                let top_frame = params
                    .get("stackTrace")
                    .and_then(|st| st.get("callFrames"))
                    .and_then(Value::as_array)
                    .and_then(|frames| frames.first());
                let url = top_frame
                    .and_then(|f| f.get("url"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
                let line = top_frame
                    .and_then(|f| f.get("lineNumber"))
                    .and_then(Value::as_u64)
                    .map(|l| (l as u32).saturating_add(1));

                self.push_console(BrowserConsoleEntry {
                    level,
                    text,
                    url,
                    line,
                    timestamp_ms: now_epoch_ms(),
                });
            }
            "Runtime.exceptionThrown" => {
                let details = params.get("exceptionDetails").unwrap_or(params);
                let base_text = details
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("Uncaught exception");
                let desc = details
                    .get("exception")
                    .and_then(|ex| ex.get("description"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let text = if desc.is_empty() || desc == base_text {
                    base_text.to_string()
                } else {
                    format!("{base_text}: {desc}")
                };
                let url = details
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
                let line = details
                    .get("lineNumber")
                    .and_then(Value::as_u64)
                    .map(|l| (l as u32).saturating_add(1));

                self.push_console(BrowserConsoleEntry {
                    level: "exception".to_string(),
                    text,
                    url,
                    line,
                    timestamp_ms: now_epoch_ms(),
                });
            }
            "Network.requestWillBeSent" => {
                if let Some(req_id) = params.get("requestId").and_then(Value::as_str) {
                    let req = params.get("request");
                    let url = req
                        .and_then(|r| r.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let http_method = req
                        .and_then(|r| r.get("method"))
                        .and_then(Value::as_str)
                        .unwrap_or("GET")
                        .to_string();
                    let resource_type = params
                        .get("type")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    self.record_request(req_id.to_string(), url, http_method, resource_type);
                }
            }
            "Network.loadingFailed" => {
                let req_id = params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let meta = self.request_meta.get(&req_id).cloned();
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| meta.as_ref().map(|m| m.0.clone()))
                    .unwrap_or_default();
                let http_method = meta
                    .as_ref()
                    .map(|m| m.1.clone())
                    .unwrap_or_else(|| "GET".to_string());
                let resource_type = params
                    .get("type")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| meta.and_then(|m| m.2));
                let error_text = params
                    .get("errorText")
                    .and_then(Value::as_str)
                    .unwrap_or("Network loading failed")
                    .to_string();

                self.push_network_error(BrowserNetworkError {
                    request_id: req_id,
                    url,
                    method: http_method,
                    status: None,
                    error_text,
                    resource_type,
                    timestamp_ms: now_epoch_ms(),
                });
            }
            "Network.responseReceived" => {
                let response = params.get("response").unwrap_or(params);
                let status = response
                    .get("status")
                    .and_then(Value::as_u64)
                    .unwrap_or(200) as u16;
                if status >= 400 {
                    let req_id = params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let meta = self.request_meta.get(&req_id).cloned();
                    let url = response
                        .get("url")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .or_else(|| meta.as_ref().map(|m| m.0.clone()))
                        .unwrap_or_default();
                    let http_method = meta
                        .as_ref()
                        .map(|m| m.1.clone())
                        .unwrap_or_else(|| "GET".to_string());
                    let status_text = response
                        .get("statusText")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let error_text = if status_text.is_empty() {
                        format!("HTTP {status}")
                    } else {
                        format!("HTTP {status} {status_text}")
                    };
                    let resource_type = params
                        .get("type")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .or_else(|| meta.and_then(|m| m.2));

                    self.push_network_error(BrowserNetworkError {
                        request_id: req_id,
                        url,
                        method: http_method,
                        status: Some(status),
                        error_text,
                        resource_type,
                        timestamp_ms: now_epoch_ms(),
                    });
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> BrowserDiagnostics {
        BrowserDiagnostics {
            console_logs: self.console_logs.iter().cloned().collect(),
            network_errors: self.network_errors.iter().cloned().collect(),
        }
    }

    fn clear(&mut self) {
        self.console_logs.clear();
        self.network_errors.clear();
    }
}

/// Multiplexed RFC 6455 WebSocket client for Chrome DevTools Protocol (`ws://127.0.0.1:<port>/...`).
pub struct CdpWsConnection {
    ws_url: String,
    next_id: AtomicU64,
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    diagnostics: Arc<Mutex<DiagnosticsRingBuffer>>,
    event_tx: broadcast::Sender<CdpEvent>,
}

impl CdpWsConnection {
    pub async fn connect(ws_url: &str) -> Result<Self, String> {
        let (host, port, _path) = parse_local_ws_url(ws_url)?;
        let addr = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };

        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("Failed to connect to CDP WebSocket {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        Self::connect_stream(ws_url, stream).await
    }

    pub async fn connect_stream<S>(ws_url: &str, mut stream: S) -> Result<Self, String>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (host, port, path) = parse_local_ws_url(ws_url)?;
        let sec_key = generate_sec_websocket_key();
        let expected_accept = compute_websocket_accept(&sec_key);
        let handshake_req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {sec_key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );

        stream
            .write_all(handshake_req.as_bytes())
            .await
            .map_err(|e| format!("Failed to send WebSocket upgrade request: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("Failed to flush WebSocket upgrade request: {e}"))?;

        let mut resp_buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 2048];
        let header_end = loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| format!("Failed reading WebSocket handshake response: {e}"))?;
            if n == 0 {
                return Err("EOF before WebSocket HTTP 101 handshake completed.".to_string());
            }
            resp_buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = resp_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if resp_buf.len() > 32_768 {
                return Err("WebSocket handshake headers exceeded 32KB.".to_string());
            }
        };

        let header_str = String::from_utf8_lossy(&resp_buf[..header_end]).to_string();
        let leftover = resp_buf[header_end..].to_vec();

        let mut lines = header_str.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 101") && !status_line.starts_with("HTTP/1.0 101") {
            return Err(format!(
                "WebSocket upgrade rejected by server: '{status_line}'"
            ));
        }

        let mut accept_header: Option<String> = None;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("sec-websocket-accept") {
                    accept_header = Some(v.trim().to_string());
                }
            }
        }
        match accept_header {
            Some(actual) if actual == expected_accept => {}
            Some(actual) => {
                return Err(format!(
                    "Sec-WebSocket-Accept mismatch: expected '{expected_accept}', got '{actual}'"
                ));
            }
            None => {
                return Err(
                    "Missing Sec-WebSocket-Accept header in HTTP 101 response.".to_string(),
                );
            }
        }

        let (read_half, mut write_half) = tokio::io::split(stream);
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics = Arc::new(Mutex::new(DiagnosticsRingBuffer::default()));
        let (event_tx, _) = broadcast::channel::<CdpEvent>(256);

        tokio::spawn(async move {
            while let Some(frame_bytes) = write_rx.recv().await {
                if write_half.write_all(&frame_bytes).await.is_err() {
                    break;
                }
                if write_half.flush().await.is_err() {
                    break;
                }
            }
        });

        let pending_reader = Arc::clone(&pending);
        let diag_reader = Arc::clone(&diagnostics);
        let write_tx_pong = write_tx.clone();
        let event_tx_reader = event_tx.clone();

        tokio::spawn(async move {
            let mut reader = WsStreamReader::new(read_half, leftover);
            let mut frag_opcode: Option<u8> = None;
            let mut frag_buf: Vec<u8> = Vec::new();

            loop {
                let (fin, opcode, payload) = match reader.read_raw_frame().await {
                    Ok(f) => f,
                    Err(err) => {
                        let mut map = pending_reader.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(Err(err.clone()));
                        }
                        break;
                    }
                };

                match opcode {
                    WS_OPCODE_PING => {
                        let pong =
                            encode_ws_client_frame(WS_OPCODE_PONG, &payload, generate_mask_key());
                        let _ = write_tx_pong.send(pong);
                    }
                    WS_OPCODE_PONG => {}
                    WS_OPCODE_CLOSE => {
                        let close_reply =
                            encode_ws_client_frame(WS_OPCODE_CLOSE, &[], generate_mask_key());
                        let _ = write_tx_pong.send(close_reply);
                        let mut map = pending_reader.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(Err("CDP WebSocket closed by server.".to_string()));
                        }
                        break;
                    }
                    WS_OPCODE_TEXT | WS_OPCODE_BINARY => {
                        if fin && frag_opcode.is_none() {
                            Self::dispatch_incoming_message(
                                &payload,
                                &pending_reader,
                                &diag_reader,
                                &event_tx_reader,
                            )
                            .await;
                        } else {
                            frag_opcode = Some(opcode);
                            frag_buf.clear();
                            frag_buf.extend_from_slice(&payload);
                        }
                    }
                    WS_OPCODE_CONTINUATION => {
                        if frag_opcode.is_some() {
                            frag_buf.extend_from_slice(&payload);
                            if frag_buf.len() > MAX_WS_FRAME_BYTES {
                                frag_opcode = None;
                                frag_buf.clear();
                                continue;
                            }
                            if fin {
                                let completed = std::mem::take(&mut frag_buf);
                                frag_opcode = None;
                                Self::dispatch_incoming_message(
                                    &completed,
                                    &pending_reader,
                                    &diag_reader,
                                    &event_tx_reader,
                                )
                                .await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            ws_url: ws_url.to_string(),
            next_id: AtomicU64::new(1),
            write_tx,
            pending,
            diagnostics,
            event_tx,
        })
    }

    async fn dispatch_incoming_message(
        raw: &[u8],
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
        diagnostics: &Arc<Mutex<DiagnosticsRingBuffer>>,
        event_tx: &broadcast::Sender<CdpEvent>,
    ) {
        let parsed: Value = match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(_) => return,
        };

        if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
            let waiter = pending.lock().await.remove(&id);
            if let Some(tx) = waiter {
                if let Some(err_obj) = parsed.get("error") {
                    let msg = err_obj
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("CDP command error");
                    let data = err_obj
                        .get("data")
                        .and_then(Value::as_str)
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    let _ = tx.send(Err(format!("{msg}{data}")));
                } else {
                    let result = parsed.get("result").cloned().unwrap_or_else(|| json!({}));
                    let _ = tx.send(Ok(result));
                }
            }
            return;
        }

        if let Some(method) = parsed.get("method").and_then(Value::as_str) {
            let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
            diagnostics.lock().await.ingest_cdp_event(method, &params);
            let _ = event_tx.send(CdpEvent {
                method: method.to_string(),
                params,
            });
        }
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<CdpEvent> {
        self.event_tx.subscribe()
    }

    pub async fn send_command(&self, method: &str, params: Value) -> Result<Value, String> {
        self.send_command_with_timeout(
            method,
            params,
            Duration::from_millis(DEFAULT_CDP_TIMEOUT_MS),
        )
        .await
    }

    pub async fn send_command_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = serde_json::to_vec(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|e| format!("Failed to serialize CDP request: {e}"))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let frame = encode_ws_client_frame(WS_OPCODE_TEXT, &payload, generate_mask_key());
        if self.write_tx.send(frame).is_err() {
            self.pending.lock().await.remove(&id);
            return Err("CDP WebSocket writer is disconnected.".to_string());
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(format!("CDP request '{method}' canceled on socket close.")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!(
                    "CDP request '{method}' timed out after {}ms.",
                    timeout.as_millis()
                ))
            }
        }
    }

    pub async fn send_ping(&self, payload: &[u8]) -> Result<(), String> {
        let frame = encode_ws_client_frame(WS_OPCODE_PING, payload, generate_mask_key());
        self.write_tx
            .send(frame)
            .map_err(|_| "CDP WebSocket writer is disconnected.".to_string())
    }

    pub async fn diagnostics(&self) -> BrowserDiagnostics {
        self.diagnostics.lock().await.snapshot()
    }

    pub async fn clear_diagnostics(&self) {
        self.diagnostics.lock().await.clear();
    }

    pub fn is_open(&self) -> bool {
        !self.write_tx.is_closed()
    }

    pub async fn close(&self) -> Result<(), String> {
        let frame = encode_ws_client_frame(WS_OPCODE_CLOSE, &[], generate_mask_key());
        let _ = self.write_tx.send(frame);
        Ok(())
    }
}

// ============================================================================
// Chromium & FFmpeg Discovery + Pure-Rust PNG/PPM & MP4 Encoding
// ============================================================================

/// Discovers an installed macOS Chromium-family browser binary (`CHROME_PATH`, Chrome, Brave, Edge, Chromium).
pub fn discover_chromium_binary() -> Result<PathBuf, String> {
    if let Ok(env_path) = std::env::var("CHROME_PATH") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
        PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ];

    if let Ok(home) = std::env::var("HOME") {
        let user_apps = PathBuf::from(home).join("Applications");
        candidates.push(user_apps.join("Google Chrome.app/Contents/MacOS/Google Chrome"));
        candidates.push(user_apps.join("Brave Browser.app/Contents/MacOS/Brave Browser"));
        candidates.push(user_apps.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
        candidates.push(user_apps.join("Chromium.app/Contents/MacOS/Chromium"));
    }

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(
        "No Chromium browser found. Install Google Chrome, Brave, Edge, or Chromium, or set CHROME_PATH."
            .to_string(),
    )
}

/// Discovers `ffmpeg` binary (`FFMPEG_PATH`, `/opt/homebrew/bin/ffmpeg`, `/usr/local/bin/ffmpeg`, or `PATH`).
pub fn discover_ffmpeg_binary() -> Result<PathBuf, String> {
    if let Ok(env_path) = std::env::var("FFMPEG_PATH") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    for fixed in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"] {
        let p = PathBuf::from(fixed);
        if p.is_file() {
            return Ok(p);
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("ffmpeg");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err("ffmpeg binary not found at /opt/homebrew/bin/ffmpeg or on PATH.".to_string())
}

fn default_browser_profile_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".rursor").join("browser-profile");
        }
    }
    std::env::temp_dir().join("rursor-browser-profile")
}

/// Parses `DevTools listening on ws://127.0.0.1:<port>/...` from a Chromium stderr line.
pub fn parse_devtools_ws_from_stderr_line(line: &str) -> Option<(u16, String)> {
    let marker = "DevTools listening on ";
    let idx = line.find(marker)?;
    let ws_url = line[idx + marker.len()..].trim();
    let (_, port, _) = parse_local_ws_url(ws_url).ok()?;
    Some((port, ws_url.to_string()))
}

fn crc32_png(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn adler32_zlib(bytes: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in bytes {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let crc = crc32_png(&out[crc_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Pure-Rust valid RGB PNG encoder (stored deflate blocks) for zero-crate frame generation & tests.
pub fn encode_uncompressed_rgb_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(3))
        .ok_or_else(|| "PNG dimensions overflow.".to_string())?;
    if rgb.len() != expected {
        return Err(format!(
            "RGB buffer length {} does not match {}x{}x3 ({expected})",
            rgb.len(),
            width,
            height
        ));
    }

    let row_bytes = (width as usize) * 3;
    let mut raw_scanlines = Vec::with_capacity((row_bytes + 1) * (height as usize));
    for row in 0..(height as usize) {
        raw_scanlines.push(0x00); // Filter type 0 (None)
        let start = row * row_bytes;
        raw_scanlines.extend_from_slice(&rgb[start..start + row_bytes]);
    }

    let mut zlib = Vec::with_capacity(raw_scanlines.len() + 64);
    zlib.push(0x78);
    zlib.push(0x01);

    let chunks: Vec<&[u8]> = if raw_scanlines.is_empty() {
        vec![&[]]
    } else {
        raw_scanlines.chunks(65_535).collect()
    };
    for (idx, block) in chunks.iter().enumerate() {
        let is_final = idx + 1 == chunks.len();
        zlib.push(if is_final { 0x01 } else { 0x00 });
        let len = block.len() as u16;
        let nlen = !len;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&nlen.to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32_zlib(&raw_scanlines).to_be_bytes());

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // 8-bit
    ihdr[9] = 2; // RGB color type
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;
    write_png_chunk(&mut png, b"IHDR", &ihdr);
    write_png_chunk(&mut png, b"IDAT", &zlib);
    write_png_chunk(&mut png, b"IEND", &[]);
    Ok(png)
}

/// Pure-Rust binary PPM (`P6`) encoder for synthetic frame pipelines.
pub fn encode_binary_ppm(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let expected = (width as usize) * (height as usize) * 3;
    if rgb.len() != expected {
        return Err(format!(
            "PPM RGB buffer length {} does not match {}x{}x3 ({expected})",
            rgb.len(),
            width,
            height
        ));
    }
    let header = format!("P6\n{width} {height}\n255\n");
    let mut out = Vec::with_capacity(header.len() + rgb.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(rgb);
    Ok(out)
}

/// Encodes sequential `frame_0001.<ext>` frames in `frames_dir` into a smooth `1280x800` `.mp4` video
/// using `/opt/homebrew/bin/ffmpeg` (`libx264` with automatic fallback to `h264_videotoolbox`).
pub async fn encode_frames_to_mp4(
    frames_dir: &Path,
    frame_extension: &str,
    fps: u32,
    output_mp4_path: &Path,
) -> Result<(usize, String), String> {
    let ffmpeg = discover_ffmpeg_binary()?;
    if let Some(parent) = output_mp4_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create MP4 output directory: {e}"))?;
        }
    }

    let ext = frame_extension.trim_start_matches('.');
    let input_pattern = frames_dir.join(format!("frame_%04d.{ext}"));
    let vf_filter = "scale=1280:800:force_original_aspect_ratio=decrease,pad=1280:800:(ow-iw)/2:(oh-ih)/2:color=0x0B0D12,format=yuv420p";
    let fps_str = fps.max(1).to_string();

    let encoders = ["libx264", "h264_videotoolbox"];
    let mut last_err = String::new();

    for encoder in encoders {
        let mut cmd = Command::new(&ffmpeg);
        cmd.arg("-y")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-framerate")
            .arg(&fps_str)
            .arg("-start_number")
            .arg("1")
            .arg("-i")
            .arg(&input_pattern)
            .arg("-vf")
            .arg(vf_filter)
            .arg("-c:v")
            .arg(encoder);

        if encoder == "libx264" {
            cmd.arg("-preset").arg("ultrafast").arg("-crf").arg("23");
        }

        cmd.arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-movflags")
            .arg("+faststart")
            .arg(output_mp4_path);

        let output = cmd
            .output()
            .await
            .map_err(|e| format!("Failed to spawn ffmpeg ({}): {e}", ffmpeg.display()))?;

        if output.status.success() {
            let meta = fs::metadata(output_mp4_path)
                .map_err(|e| format!("ffmpeg succeeded but output MP4 missing: {e}"))?;
            if meta.len() >= 16 {
                return Ok((meta.len() as usize, encoder.to_string()));
            }
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }

    Err(format!(
        "ffmpeg failed to encode MP4 walkthrough: {last_err}"
    ))
}

// ============================================================================
// Set-of-Mark (`[1] [2] [3]`) & Click Ripple Injected JavaScript Helpers
// ============================================================================

fn build_som_snapshot_script(draw_som_badges: bool) -> String {
    let draw_flag = if draw_som_badges { "true" } else { "false" };
    format!(
        r#"(() => {{
  const DRAW_BADGES = {draw_flag};
  const MAX_ELEMENTS = 120;

  const prevOverlay = document.getElementById('__rursor_som_overlay__');
  if (prevOverlay) prevOverlay.remove();

  document.querySelectorAll('[data-rursor-som]').forEach((el) => {{
    el.removeAttribute('data-rursor-som');
  }});

  function parseFileLineAttr(raw, explicitLine) {{
    if (!raw || typeof raw !== 'string') return null;
    const trimmed = raw.trim();
    if (!trimmed) return null;
    if (explicitLine) {{
      const parsedExplicit = parseInt(String(explicitLine), 10);
      if (Number.isFinite(parsedExplicit) && parsedExplicit > 0) {{
        return {{ file: trimmed, line: parsedExplicit }};
      }}
    }}
    const match = trimmed.match(/^(.+?):(\d+)(?::\d+)?$/);
    if (match) {{
      const lineNum = parseInt(match[2], 10);
      if (Number.isFinite(lineNum) && lineNum > 0) {{
        return {{ file: match[1], line: lineNum }};
      }}
    }}
    return null;
  }}

  function extractSourceHint(el) {{
    let cur = el;
    for (let depth = 0; cur && depth < 5; depth++, cur = cur.parentElement) {{
      if (cur.getAttribute) {{
        const lineAttr =
          cur.getAttribute('data-inspector-line') ||
          cur.getAttribute('data-source-line') ||
          cur.getAttribute('data-line');
        const srcAttr =
          cur.getAttribute('data-source') ||
          cur.getAttribute('data-source-file') ||
          cur.getAttribute('data-v-inspector') ||
          cur.getAttribute('data-vite-inspector') ||
          cur.getAttribute('data-locatorjs');
        const fromAttr = parseFileLineAttr(srcAttr, lineAttr);
        if (fromAttr) return fromAttr;
      }}

      const keys = Object.keys(cur);
      for (let i = 0; i < keys.length; i++) {{
        const k = keys[i];
        if (k.startsWith('__reactFiber$') || k.startsWith('__reactInternalInstance$')) {{
          let fiber = cur[k];
          for (let fDepth = 0; fiber && fDepth < 6; fDepth++, fiber = fiber.return) {{
            const dbg = fiber._debugSource || (fiber._debugOwner && fiber._debugOwner._debugSource);
            if (dbg && dbg.fileName && dbg.lineNumber) {{
              return {{
                file: String(dbg.fileName),
                line: Math.max(1, Number(dbg.lineNumber) || 1),
              }};
            }}
          }}
        }}
      }}
    }}
    return null;
  }}

  function cssEscapeIdent(str) {{
    if (typeof CSS !== 'undefined' && CSS.escape) return CSS.escape(str);
    return String(str).replace(/([^a-zA-Z0-9_-])/g, '\\$1');
  }}

  function buildSelector(el, somId) {{
    if (el.id && typeof el.id === 'string' && el.id.trim()) {{
      return '#' + cssEscapeIdent(el.id.trim());
    }}
    const testId =
      el.getAttribute('data-testid') ||
      el.getAttribute('data-test') ||
      el.getAttribute('data-cy');
    if (testId) {{
      return `[data-testid="${{String(testId).replace(/"/g, '\\"')}}"]`;
    }}
    const tag = el.tagName.toLowerCase();
    const nameAttr = el.getAttribute('name');
    if (nameAttr) {{
      const candidate = `${{tag}}[name="${{String(nameAttr).replace(/"/g, '\\"')}}"]`;
      if (document.querySelectorAll(candidate).length === 1) return candidate;
    }}
    const parts = [];
    let node = el;
    for (let depth = 0; node && node.nodeType === 1 && depth < 4; depth++) {{
      const t = node.tagName.toLowerCase();
      if (node.id && node.id.trim()) {{
        parts.unshift('#' + cssEscapeIdent(node.id.trim()));
        break;
      }}
      let nth = 1;
      let sib = node.previousElementSibling;
      while (sib) {{
        if (sib.tagName === node.tagName) nth++;
        sib = sib.previousElementSibling;
      }}
      parts.unshift(`${{t}}:nth-of-type(${{nth}})`);
      node = node.parentElement;
    }}
    return parts.join(' > ') || `[data-rursor-som="${{somId}}"]`;
  }}

  const selectorQuery = [
    'a',
    'button',
    'input',
    'textarea',
    'select',
    'summary',
    '[role="button"]',
    '[role="link"]',
    '[role="tab"]',
    '[contenteditable="true"]',
  ].join(', ');

  const candidates = Array.from(document.querySelectorAll(selectorQuery));
  const vw = window.innerWidth || document.documentElement.clientWidth || 1280;
  const vh = window.innerHeight || document.documentElement.clientHeight || 800;

  const elements = [];
  const seen = new Set();

  for (let i = 0; i < candidates.length && elements.length < MAX_ELEMENTS; i++) {{
    const el = candidates[i];
    if (!el || seen.has(el)) continue;
    seen.add(el);

    const style = window.getComputedStyle(el);
    if (
      style.display === 'none' ||
      style.visibility === 'hidden' ||
      parseFloat(style.opacity || '1') <= 0.02
    ) {{
      continue;
    }}

    const rect = el.getBoundingClientRect();
    if (rect.width < 4 || rect.height < 4) continue;
    if (rect.bottom <= 0 || rect.right <= 0 || rect.top >= vh || rect.left >= vw) continue;

    const id = elements.length + 1;
    el.setAttribute('data-rursor-som', String(id));

    const tag = el.tagName.toLowerCase();
    const inputType =
      tag === 'input' || tag === 'button'
        ? (el.getAttribute('type') || el.type || null)
        : null;
    const role = el.getAttribute('role') || null;
    const placeholder = el.getAttribute('placeholder') || null;
    const rawHref = tag === 'a' ? (el.getAttribute('href') || el.href || null) : null;

    const rawLabel =
      el.getAttribute('aria-label') ||
      el.innerText ||
      (typeof el.value === 'string' ? el.value : '') ||
      el.getAttribute('title') ||
      el.getAttribute('alt') ||
      placeholder ||
      el.getAttribute('name') ||
      tag;
    const label = String(rawLabel).replace(/\s+/g, ' ').trim().slice(0, 140);

    elements.push({{
      id,
      tag,
      inputType: inputType ? String(inputType) : null,
      role: role ? String(role) : null,
      label,
      placeholder: placeholder ? String(placeholder) : null,
      href: rawHref ? String(rawHref) : null,
      selector: buildSelector(el, id),
      bounds: {{
        x: Math.round(rect.left * 10) / 10,
        y: Math.round(rect.top * 10) / 10,
        width: Math.round(rect.width * 10) / 10,
        height: Math.round(rect.height * 10) / 10,
      }},
      sourceHint: extractSourceHint(el),
    }});
  }}

  if (DRAW_BADGES && document.body) {{
    const overlay = document.createElement('div');
    overlay.id = '__rursor_som_overlay__';
    overlay.style.cssText =
      'position:fixed;inset:0;pointer-events:none;z-index:2147483647;overflow:hidden;';

    for (const item of elements) {{
      const box = document.createElement('div');
      box.style.cssText = [
        'position:fixed',
        `left:${{item.bounds.x}}px`,
        `top:${{item.bounds.y}}px`,
        `width:${{item.bounds.width}}px`,
        `height:${{item.bounds.height}}px`,
        'border:1.5px solid rgba(124,92,255,0.88)',
        'background:rgba(124,92,255,0.06)',
        'border-radius:4px',
        'box-sizing:border-box',
        'pointer-events:none',
      ].join(';');

      const badge = document.createElement('span');
      badge.textContent = `[${{item.id}}]`;
      const badgeTop = item.bounds.y >= 18 ? -17 : 1;
      badge.style.cssText = [
        'position:absolute',
        'left:-1px',
        `top:${{badgeTop}}px`,
        'background:#7C5CFF',
        'color:#FFFFFF',
        'font:700 10px/1.15 -apple-system,BlinkMacSystemFont,ui-monospace,monospace',
        'padding:1.5px 4px',
        'border-radius:3px',
        'box-shadow:0 2px 6px rgba(11,13,18,0.75)',
        'border:1px solid rgba(255,255,255,0.26)',
        'white-space:nowrap',
        'pointer-events:none',
      ].join(';');

      box.appendChild(badge);
      overlay.appendChild(box);
    }}
    document.body.appendChild(overlay);
  }}

  const bodyText = document.body ? String(document.body.innerText || '') : '';
  const textSummary = bodyText.replace(/\s+/g, ' ').trim().slice(0, 6000);

  return {{
    url: String(window.location.href || ''),
    title: String(document.title || ''),
    viewportWidth: vw,
    viewportHeight: vh,
    elements,
    textSummary,
  }};
}})()"#
    )
}

fn build_resolve_target_script(som_id: Option<u32>, selector: Option<&str>, focus: bool) -> String {
    let som_json = som_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "null".to_string());
    let sel_json = selector
        .and_then(|s| serde_json::to_string(s).ok())
        .unwrap_or_else(|| "null".to_string());
    let do_focus = if focus { "true" } else { "false" };

    format!(
        r#"(() => {{
  const somId = {som_json};
  const sel = {sel_json};
  let el = null;
  if (somId !== null) {{
    el = document.querySelector(`[data-rursor-som="${{somId}}"]`);
  }}
  if (!el && sel) {{
    el = document.querySelector(sel);
  }}
  if (!el) {{
    return {{ found: false }};
  }}
  el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }});
  const rect = el.getBoundingClientRect();
  if ({do_focus}) {{
    if (typeof el.focus === 'function') el.focus();
  }}
  return {{
    found: true,
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2,
    tag: el.tagName.toLowerCase(),
  }};
}})()"#
    )
}

fn build_click_ripple_script(x: f64, y: f64) -> String {
    format!(
        r#"(() => {{
  const prev = document.getElementById('__rursor_click_ripple__');
  if (prev) prev.remove();
  if (!document.body) return false;
  const ripple = document.createElement('div');
  ripple.id = '__rursor_click_ripple__';
  ripple.style.cssText = [
    'position:fixed',
    `left:${{{x} - 22}}px`,
    `top:${{{y} - 22}}px`,
    'width:44px',
    'height:44px',
    'border-radius:999px',
    'border:2.5px solid #7C5CFF',
    'background:radial-gradient(circle, rgba(124,92,255,0.55) 0%, rgba(124,92,255,0.18) 55%, rgba(124,92,255,0) 100%)',
    'box-shadow:0 0 18px rgba(124,92,255,0.9), 0 0 36px rgba(124,92,255,0.45)',
    'pointer-events:none',
    'z-index:2147483647',
  ].join(';');
  const dot = document.createElement('div');
  dot.style.cssText =
    'position:absolute;left:16px;top:16px;width:12px;height:12px;border-radius:999px;background:#7C5CFF;border:2px solid #FFFFFF;box-sizing:border-box;';
  ripple.appendChild(dot);
  document.body.appendChild(ripple);
  return true;
}})()"#
    )
}

// ============================================================================
// Managed Chromium Process & Session (`BrowserSession` + `BrowserSessionManager`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct BrowserLaunchConfig {
    pub headless: bool,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub user_data_dir: PathBuf,
    pub chrome_binary: Option<PathBuf>,
    pub artifacts_dir: Option<PathBuf>,
}

impl Default for BrowserLaunchConfig {
    fn default() -> Self {
        Self {
            headless: true,
            viewport_width: DEFAULT_VIEWPORT_WIDTH,
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            user_data_dir: default_browser_profile_dir(),
            chrome_binary: None,
            artifacts_dir: None,
        }
    }
}

pub struct BrowserSession {
    child: Child,
    debug_port: u16,
    cdp: CdpWsConnection,
    viewport_width: u32,
    viewport_height: u32,
    artifacts_dir: Option<PathBuf>,
}

impl BrowserSession {
    pub async fn launch(config: &BrowserLaunchConfig) -> Result<Self, String> {
        let chrome_bin = match &config.chrome_binary {
            Some(p) => p.clone(),
            None => discover_chromium_binary()?,
        };

        match Self::spawn_with_profile(&chrome_bin, config, &config.user_data_dir).await {
            Ok(session) => Ok(session),
            Err(primary_err) => {
                // Retry once with an isolated profile directory if the shared profile is locked
                let fallback_profile = std::env::temp_dir().join(format!(
                    "rursor-browser-profile-{}-{}",
                    std::process::id(),
                    now_epoch_ms()
                ));
                Self::spawn_with_profile(&chrome_bin, config, &fallback_profile)
                    .await
                    .map_err(|_| primary_err)
            }
        }
    }

    async fn spawn_with_profile(
        chrome_bin: &Path,
        config: &BrowserLaunchConfig,
        profile_dir: &Path,
    ) -> Result<Self, String> {
        fs::create_dir_all(profile_dir)
            .map_err(|e| format!("Failed to create browser profile dir '{}': {e}", profile_dir.display()))?;

        let mut cmd = Command::new(chrome_bin);
        if config.headless {
            cmd.arg("--headless=new");
        }
        cmd.arg("--remote-debugging-port=0")
            .arg("--remote-allow-origins=*")
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg(format!(
                "--window-size={},{}",
                config.viewport_width, config.viewport_height
            ))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-default-apps")
            .arg("--hide-scrollbars")
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to launch Chromium '{}': {e}", chrome_bin.display()))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture Chromium stderr.".to_string())?;

        let (port_tx, port_rx) = oneshot::channel::<Result<(u16, String), String>>();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut recent_err = Vec::new();
            let mut sender = Some(port_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some((port, ws_url)) = parse_devtools_ws_from_stderr_line(&line) {
                    if let Some(tx) = sender.take() {
                        let _ = tx.send(Ok((port, ws_url)));
                    }
                    // Continue draining stderr silently so child never blocks on a full pipe
                } else if sender.is_some() && recent_err.len() < 8 && !line.trim().is_empty() {
                    recent_err.push(line);
                }
            }
            if let Some(tx) = sender.take() {
                let detail = if recent_err.is_empty() {
                    "Chromium exited before printing DevTools WebSocket URL.".to_string()
                } else {
                    format!(
                        "Chromium exited before DevTools started: {}",
                        recent_err.join(" | ")
                    )
                };
                let _ = tx.send(Err(detail));
            }
        });

        let (debug_port, _browser_ws) = tokio::time::timeout(Duration::from_secs(15), port_rx)
            .await
            .map_err(|_| "Timed out waiting 15s for Chromium DevTools port.".to_string())?
            .map_err(|_| "Chromium stderr listener closed unexpectedly.".to_string())??;

        let page_ws_url = Self::resolve_page_target_ws(debug_port).await?;
        let cdp = CdpWsConnection::connect(&page_ws_url).await?;

        cdp.send_command("Page.enable", json!({})).await?;
        cdp.send_command("Runtime.enable", json!({})).await?;
        cdp.send_command("DOM.enable", json!({})).await?;
        cdp.send_command("Network.enable", json!({})).await?;
        cdp.send_command(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": config.viewport_width,
                "height": config.viewport_height,
                "deviceScaleFactor": 1,
                "mobile": config.viewport_width <= 480,
            }),
        )
        .await?;

        Ok(Self {
            child,
            debug_port,
            cdp,
            viewport_width: config.viewport_width,
            viewport_height: config.viewport_height,
            artifacts_dir: config.artifacts_dir.clone(),
        })
    }

    async fn resolve_page_target_ws(port: u16) -> Result<String, String> {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        let list_url = format!("http://127.0.0.1:{port}/json/list");
        for _ in 0..20 {
            if let Ok(resp) = http.get(&list_url).send().await {
                if let Ok(targets) = resp.json::<Vec<Value>>().await {
                    for target in &targets {
                        let is_page = target
                            .get("type")
                            .and_then(Value::as_str)
                            .map(|t| t == "page")
                            .unwrap_or(false);
                        if is_page {
                            if let Some(ws) =
                                target.get("webSocketDebuggerUrl").and_then(Value::as_str)
                            {
                                return Ok(ws.to_string());
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        }

        let new_url = format!("http://127.0.0.1:{port}/json/new?about:blank");
        let resp = match http.put(&new_url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => http
                .get(&new_url)
                .send()
                .await
                .map_err(|e| format!("Failed to open CDP page target via /json/new: {e}"))?,
        };
        let target: Value = resp
            .json()
            .await
            .map_err(|e| format!("Invalid JSON from /json/new: {e}"))?;
        target
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| "Missing webSocketDebuggerUrl in /json/new response.".to_string())
    }

    /// Creates a `BrowserSession` backed by an already-connected `CdpWsConnection` (for tests or external targets).
    pub fn from_cdp_connection(
        cdp: CdpWsConnection,
        child: Child,
        debug_port: u16,
        viewport_width: u32,
        viewport_height: u32,
        artifacts_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            child,
            debug_port,
            cdp,
            viewport_width,
            viewport_height,
            artifacts_dir,
        }
    }

    pub fn is_healthy(&mut self) -> bool {
        if !self.cdp.is_open() {
            return false;
        }
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn debug_port(&self) -> u16 {
        self.debug_port
    }

    pub async fn navigate(
        &mut self,
        url: &str,
        viewport_width: u32,
        viewport_height: u32,
    ) -> Result<NavigateResult, String> {
        let safe_url = validate_browser_url(url)?;
        let vw = viewport_width.clamp(240, 3840);
        let vh = viewport_height.clamp(240, 3840);
        self.viewport_width = vw;
        self.viewport_height = vh;

        self.cdp
            .send_command(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": vw,
                    "height": vh,
                    "deviceScaleFactor": 1,
                    "mobile": vw <= 480,
                }),
            )
            .await?;

        let mut events = self.cdp.subscribe_events();
        let nav_resp = self
            .cdp
            .send_command("Page.navigate", json!({ "url": safe_url }))
            .await?;

        if let Some(err_text) = nav_resp
            .get("errorText")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Err(format!("Navigation to '{safe_url}' failed: {err_text}"));
        }

        let mut doc_status: u16 = 200;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let step_wait = remaining.min(Duration::from_millis(400));
            match tokio::time::timeout(step_wait, events.recv()).await {
                Ok(Ok(ev)) => {
                    if ev.method == "Network.responseReceived" {
                        let is_doc = ev
                            .params
                            .get("type")
                            .and_then(Value::as_str)
                            .map(|t| t.eq_ignore_ascii_case("Document"))
                            .unwrap_or(false);
                        if is_doc {
                            if let Some(st) = ev
                                .params
                                .get("response")
                                .and_then(|r| r.get("status"))
                                .and_then(Value::as_u64)
                            {
                                doc_status = st as u16;
                            }
                        }
                    } else if ev.method == "Page.loadEventFired"
                        || ev.method == "Page.frameStoppedLoading"
                    {
                        break;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {
                    if let Ok(state_val) = self
                        .evaluate_js_value("document.readyState", false)
                        .await
                    {
                        if state_val.as_str() == Some("complete") {
                            break;
                        }
                    }
                }
            }
        }

        // Brief network/render settle for SPA hydration
        tokio::time::sleep(Duration::from_millis(120)).await;

        let meta = self
            .evaluate_js_value(
                "({ url: String(window.location.href || ''), title: String(document.title || '') })",
                false,
            )
            .await
            .unwrap_or_else(|_| json!({ "url": safe_url, "title": "" }));

        let final_url = meta
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&safe_url)
            .to_string();
        let title = meta
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        Ok(NavigateResult {
            url: final_url,
            title,
            status: doc_status,
            viewport_width: vw,
            viewport_height: vh,
        })
    }

    async fn evaluate_js_value(&self, expression: &str, await_promise: bool) -> Result<Value, String> {
        let resp = self
            .cdp
            .send_command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": await_promise,
                }),
            )
            .await?;

        if let Some(exc) = resp.get("exceptionDetails") {
            let text = exc
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("JS evaluation threw an exception");
            let desc = exc
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(if desc.is_empty() {
                text.to_string()
            } else {
                format!("{text}: {desc}")
            });
        }

        Ok(resp
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn snapshot(
        &self,
        draw_som_badges: bool,
        include_screenshot: bool,
    ) -> Result<BrowserSnapshot, String> {
        let script = build_som_snapshot_script(draw_som_badges);
        let raw = self.evaluate_js_value(&script, false).await?;

        let url = raw
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = raw
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let viewport_width = raw
            .get("viewportWidth")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(self.viewport_width);
        let viewport_height = raw
            .get("viewportHeight")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(self.viewport_height);
        let text_summary = raw
            .get("textSummary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let elements: Vec<SomElement> = raw
            .get("elements")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();

        let ts = now_epoch_ms();
        let (screenshot_base64, screenshot_path) = if include_screenshot {
            let b64 = self.capture_png_base64().await?;
            let saved_path = if let Some(base_dir) = &self.artifacts_dir {
                let shots_dir = base_dir.join("screenshots");
                if fs::create_dir_all(&shots_dir).is_ok() {
                    if let Ok(png_bytes) = base64_decode(&b64) {
                        let file_path = shots_dir.join(format!("snapshot_{ts}.png"));
                        if fs::write(&file_path, png_bytes).is_ok() {
                            Some(file_path.to_string_lossy().to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            (Some(b64), saved_path)
        } else {
            (None, None)
        };

        Ok(BrowserSnapshot {
            url,
            title,
            viewport_width,
            viewport_height,
            draw_som_badges,
            elements,
            screenshot_base64,
            screenshot_path,
            text_summary,
            timestamp_ms: ts,
        })
    }

    pub async fn capture_png_base64(&self) -> Result<String, String> {
        let shot = self
            .cdp
            .send_command("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        shot.get("data")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| "Page.captureScreenshot did not return base64 PNG data.".to_string())
    }

    async fn resolve_coordinates(
        &self,
        som_id: Option<u32>,
        selector: Option<&str>,
        x: Option<f64>,
        y: Option<f64>,
        focus: bool,
    ) -> Result<(f64, f64), String> {
        if let (Some(px), Some(py)) = (x, y) {
            return Ok((px, py));
        }
        if som_id.is_none() && selector.is_none() {
            return Err(
                "Target coordinates (x, y), som_id, or CSS selector must be provided.".to_string(),
            );
        }
        let script = build_resolve_target_script(som_id, selector, focus);
        let res = self.evaluate_js_value(&script, false).await?;
        let found = res.get("found").and_then(Value::as_bool).unwrap_or(false);
        if !found {
            return Err(format!(
                "Could not locate target element (som_id={som_id:?}, selector={selector:?})."
            ));
        }
        let cx = res
            .get("x")
            .and_then(Value::as_f64)
            .ok_or_else(|| "Resolved element missing x coordinate.".to_string())?;
        let cy = res
            .get("y")
            .and_then(Value::as_f64)
            .ok_or_else(|| "Resolved element missing y coordinate.".to_string())?;
        Ok((cx, cy))
    }

    async fn dispatch_left_click(&self, x: f64, y: f64) -> Result<(), String> {
        self.cdp
            .send_command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": x,
                    "y": y,
                }),
            )
            .await?;
        self.cdp
            .send_command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 1,
                    "clickCount": 1,
                }),
            )
            .await?;
        self.cdp
            .send_command(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 0,
                    "clickCount": 1,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn act(&self, action: BrowserAction) -> Result<BrowserActionResult, String> {
        let mut resolved_x = None;
        let mut resolved_y = None;
        let mut eval_value = None;
        let action_name: String;

        match action {
            BrowserAction::Click {
                som_id,
                selector,
                x,
                y,
            } => {
                action_name = "click".to_string();
                let (cx, cy) = self
                    .resolve_coordinates(som_id, selector.as_deref(), x, y, false)
                    .await?;
                self.dispatch_left_click(cx, cy).await?;
                resolved_x = Some(cx);
                resolved_y = Some(cy);
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            BrowserAction::Type {
                som_id,
                selector,
                text,
                press_enter,
            } => {
                action_name = "type".to_string();
                if som_id.is_some() || selector.is_some() {
                    let (cx, cy) = self
                        .resolve_coordinates(som_id, selector.as_deref(), None, None, true)
                        .await?;
                    self.dispatch_left_click(cx, cy).await?;
                    resolved_x = Some(cx);
                    resolved_y = Some(cy);
                }
                self.cdp
                    .send_command("Input.insertText", json!({ "text": text }))
                    .await?;
                if press_enter {
                    self.cdp
                        .send_command(
                            "Input.dispatchKeyEvent",
                            json!({
                                "type": "keyDown",
                                "key": "Enter",
                                "code": "Enter",
                                "windowsVirtualKeyCode": 13,
                                "nativeVirtualKeyCode": 13,
                                "text": "\r",
                                "unmodifiedText": "\r",
                            }),
                        )
                        .await?;
                    self.cdp
                        .send_command(
                            "Input.dispatchKeyEvent",
                            json!({
                                "type": "keyUp",
                                "key": "Enter",
                                "code": "Enter",
                                "windowsVirtualKeyCode": 13,
                                "nativeVirtualKeyCode": 13,
                            }),
                        )
                        .await?;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            BrowserAction::Scroll { delta_x, delta_y } => {
                action_name = "scroll".to_string();
                let cx = (self.viewport_width as f64) / 2.0;
                let cy = (self.viewport_height as f64) / 2.0;
                let wheel_res = self
                    .cdp
                    .send_command(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": "mouseWheel",
                            "x": cx,
                            "y": cy,
                            "deltaX": delta_x,
                            "deltaY": delta_y,
                        }),
                    )
                    .await;
                if wheel_res.is_err() {
                    let _ = self
                        .evaluate_js_value(
                            &format!("window.scrollBy({delta_x}, {delta_y})"),
                            false,
                        )
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
            BrowserAction::Evaluate { expression } => {
                action_name = "evaluate".to_string();
                let v = self.evaluate_js_value(&expression, true).await?;
                eval_value = Some(v);
            }
            BrowserAction::ClearOverlay => {
                action_name = "clear_overlay".to_string();
                let _ = self
                    .evaluate_js_value(
                        "(() => { const el = document.getElementById('__rursor_som_overlay__'); if (el) el.remove(); return true; })()",
                        false,
                    )
                    .await?;
            }
        }

        let meta = self
            .evaluate_js_value(
                "({ url: String(window.location.href || ''), title: String(document.title || '') })",
                false,
            )
            .await
            .unwrap_or_else(|_| json!({ "url": "", "title": "" }));

        Ok(BrowserActionResult {
            ok: true,
            action: action_name,
            resolved_x,
            resolved_y,
            value: eval_value,
            url: meta
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: meta
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    pub async fn diagnostics(&self) -> BrowserDiagnostics {
        self.cdp.diagnostics().await
    }

    pub async fn clear_diagnostics(&self) {
        self.cdp.clear_diagnostics().await;
    }

    pub async fn export_pdf(&self, output_path: &Path) -> Result<PdfExportResult, String> {
        let resp = self
            .cdp
            .send_command(
                "Page.printToPDF",
                json!({
                    "printBackground": true,
                    "preferCSSPageSize": true,
                }),
            )
            .await?;
        let b64 = resp
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| "Page.printToPDF did not return base64 PDF data.".to_string())?;
        let pdf_bytes = base64_decode(b64)?;

        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating PDF output directory: {e}"))?;
            }
        }
        fs::write(output_path, &pdf_bytes)
            .map_err(|e| format!("Failed writing PDF to '{}': {e}", output_path.display()))?;

        Ok(PdfExportResult {
            output_path: output_path.to_string_lossy().to_string(),
            bytes_written: pdf_bytes.len(),
        })
    }

    async fn capture_repeated_frames(
        &self,
        staging_dir: &Path,
        frame_counter: &mut usize,
        repeats: usize,
    ) -> Result<(), String> {
        let b64 = self.capture_png_base64().await?;
        let png_bytes = base64_decode(&b64)?;
        for _ in 0..repeats.max(1) {
            *frame_counter += 1;
            let frame_path = staging_dir.join(format!("frame_{:04}.png", *frame_counter));
            fs::write(&frame_path, &png_bytes)
                .map_err(|e| format!("Failed writing walkthrough frame: {e}"))?;
        }
        Ok(())
    }

    pub async fn record_walkthrough(
        &mut self,
        steps: Vec<WalkthroughStep>,
        output_mp4_path: &Path,
    ) -> Result<WalkthroughRecording, String> {
        let started = Instant::now();
        let fps: u32 = 6;
        let staging_dir = std::env::temp_dir().join(format!(
            "rursor_walkthrough_frames_{}_{}",
            std::process::id(),
            now_epoch_ms()
        ));
        fs::create_dir_all(&staging_dir)
            .map_err(|e| format!("Failed creating walkthrough staging dir: {e}"))?;

        let mut frame_counter: usize = 0;
        let step_count = steps.len();

        let step_result: Result<(), String> = async {
            if steps.is_empty() {
                self.capture_repeated_frames(&staging_dir, &mut frame_counter, 6)
                    .await?;
            }

            for step in steps {
                match step {
                    WalkthroughStep::Navigate {
                        url,
                        viewport_width,
                        viewport_height,
                    } => {
                        let vw = viewport_width.unwrap_or(DEFAULT_VIEWPORT_WIDTH);
                        let vh = viewport_height.unwrap_or(DEFAULT_VIEWPORT_HEIGHT);
                        self.navigate(&url, vw, vh).await?;
                        self.capture_repeated_frames(&staging_dir, &mut frame_counter, 4)
                            .await?;
                    }
                    WalkthroughStep::Click {
                        som_id,
                        selector,
                        x,
                        y,
                    } => {
                        let (cx, cy) = self
                            .resolve_coordinates(som_id, selector.as_deref(), x, y, false)
                            .await?;
                        let _ = self
                            .evaluate_js_value(&build_click_ripple_script(cx, cy), false)
                            .await;
                        self.capture_repeated_frames(&staging_dir, &mut frame_counter, 2)
                            .await?;
                        let _ = self
                            .evaluate_js_value(
                                "(() => { const r = document.getElementById('__rursor_click_ripple__'); if (r) r.remove(); return true; })()",
                                false,
                            )
                            .await;
                        self.dispatch_left_click(cx, cy).await?;
                        tokio::time::sleep(Duration::from_millis(160)).await;
                        self.capture_repeated_frames(&staging_dir, &mut frame_counter, 4)
                            .await?;
                    }
                    WalkthroughStep::Type {
                        som_id,
                        selector,
                        text,
                        press_enter,
                    } => {
                        self.act(BrowserAction::Type {
                            som_id,
                            selector,
                            text,
                            press_enter,
                        })
                        .await?;
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        self.capture_repeated_frames(&staging_dir, &mut frame_counter, 3)
                            .await?;
                    }
                    WalkthroughStep::Scroll { delta_x, delta_y } => {
                        self.act(BrowserAction::Scroll { delta_x, delta_y }).await?;
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        self.capture_repeated_frames(&staging_dir, &mut frame_counter, 3)
                            .await?;
                    }
                    WalkthroughStep::WaitMs { ms } => {
                        let clamped = ms.min(15_000);
                        tokio::time::sleep(Duration::from_millis(clamped)).await;
                        let hold_frames = ((clamped / 180) as usize).clamp(1, 24);
                        self.capture_repeated_frames(&staging_dir, &mut frame_counter, hold_frames)
                            .await?;
                    }
                }
            }
            Ok(())
        }
        .await;

        if let Err(err) = step_result {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(err);
        }

        let encode_res =
            encode_frames_to_mp4(&staging_dir, "png", fps, output_mp4_path).await;
        let _ = fs::remove_dir_all(&staging_dir);
        let (bytes_written, encoder) = encode_res?;

        Ok(WalkthroughRecording {
            output_mp4_path: output_mp4_path.to_string_lossy().to_string(),
            frame_count: frame_counter,
            step_count,
            duration_ms: started.elapsed().as_millis() as u64,
            bytes_written,
            encoder,
        })
    }

    pub async fn shutdown(&mut self) {
        let _ = self.cdp.close().await;
        let _ = self.child.kill().await;
    }
}

/// Thread-safe manager that lazily launches and supervises a local Chromium CDP session.
pub struct BrowserSessionManager {
    config: RwLock<BrowserLaunchConfig>,
    session: Mutex<Option<BrowserSession>>,
}

impl Default for BrowserSessionManager {
    fn default() -> Self {
        Self::new(BrowserLaunchConfig::default())
    }
}

impl BrowserSessionManager {
    pub fn new(config: BrowserLaunchConfig) -> Self {
        Self {
            config: RwLock::new(config),
            session: Mutex::new(None),
        }
    }

    pub async fn set_workspace_root(&self, workspace_root: Option<PathBuf>) {
        let mut cfg = self.config.write().await;
        cfg.artifacts_dir = workspace_root.map(|root| root.join(".rursor").join("artifacts"));
    }

    pub async fn set_headless(&self, headless: bool) {
        let mut cfg = self.config.write().await;
        if cfg.headless != headless {
            cfg.headless = headless;
            drop(cfg);
            self.close().await;
        }
    }

    async fn ensure_session<'a>(
        &'a self,
        guard: &'a mut Option<BrowserSession>,
    ) -> Result<&'a mut BrowserSession, String> {
        let needs_relaunch = match guard.as_mut() {
            Some(sess) => !sess.is_healthy(),
            None => true,
        };
        if needs_relaunch {
            if let Some(mut stale) = guard.take() {
                stale.shutdown().await;
            }
            let cfg = self.config.read().await.clone();
            let fresh = BrowserSession::launch(&cfg).await?;
            *guard = Some(fresh);
        }
        guard
            .as_mut()
            .ok_or_else(|| "Browser session failed to initialize.".to_string())
    }

    pub async fn navigate(
        &self,
        url: &str,
        viewport_width: u32,
        viewport_height: u32,
    ) -> Result<NavigateResult, String> {
        let mut guard = self.session.lock().await;
        let sess = self.ensure_session(&mut guard).await?;
        sess.navigate(url, viewport_width, viewport_height).await
    }

    pub async fn snapshot(
        &self,
        draw_som_badges: bool,
        include_screenshot: bool,
    ) -> Result<BrowserSnapshot, String> {
        let mut guard = self.session.lock().await;
        let sess = self.ensure_session(&mut guard).await?;
        sess.snapshot(draw_som_badges, include_screenshot).await
    }

    pub async fn act(&self, action: BrowserAction) -> Result<BrowserActionResult, String> {
        let mut guard = self.session.lock().await;
        let sess = self.ensure_session(&mut guard).await?;
        sess.act(action).await
    }

    pub async fn diagnostics(&self) -> BrowserDiagnostics {
        let guard = self.session.lock().await;
        match guard.as_ref() {
            Some(sess) => sess.diagnostics().await,
            None => BrowserDiagnostics::default(),
        }
    }

    pub async fn clear_diagnostics(&self) {
        let guard = self.session.lock().await;
        if let Some(sess) = guard.as_ref() {
            sess.clear_diagnostics().await;
        }
    }

    pub async fn export_pdf(&self, output_path: &Path) -> Result<PdfExportResult, String> {
        let mut guard = self.session.lock().await;
        let sess = self.ensure_session(&mut guard).await?;
        sess.export_pdf(output_path).await
    }

    pub async fn record_walkthrough(
        &self,
        steps: Vec<WalkthroughStep>,
        output_mp4_path: &Path,
    ) -> Result<WalkthroughRecording, String> {
        let mut guard = self.session.lock().await;
        let sess = self.ensure_session(&mut guard).await?;
        sess.record_walkthrough(steps, output_mp4_path).await
    }

    pub async fn close(&self) {
        let mut guard = self.session.lock().await;
        if let Some(mut sess) = guard.take() {
            sess.shutdown().await;
        }
    }

    /// Unified JSON-RPC dispatcher for `browser_*` methods from `sidecar/src/main.rs` & `BrowserStudioPanel`.
    pub async fn handle_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "browser_navigate" => {
                if let Some(headless) = params.get("headless").and_then(Value::as_bool) {
                    self.set_headless(headless).await;
                }
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "browser_navigate requires 'url'.".to_string())?;
                let vw = params
                    .get("viewportWidth")
                    .or_else(|| params.get("viewport_width"))
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or(DEFAULT_VIEWPORT_WIDTH);
                let vh = params
                    .get("viewportHeight")
                    .or_else(|| params.get("viewport_height"))
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or(DEFAULT_VIEWPORT_HEIGHT);
                let res = self.navigate(url, vw, vh).await?;
                serde_json::to_value(res).map_err(|e| e.to_string())
            }
            "browser_snapshot" => {
                let draw_som = params
                    .get("drawSomBadges")
                    .or_else(|| params.get("draw_som_badges"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let include_shot = params
                    .get("includeScreenshot")
                    .or_else(|| params.get("include_screenshot"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let snap = self.snapshot(draw_som, include_shot).await?;
                serde_json::to_value(snap).map_err(|e| e.to_string())
            }
            "browser_act" => {
                let action_val = params.get("action").cloned().unwrap_or(params);
                let action: BrowserAction = serde_json::from_value(action_val)
                    .map_err(|e| format!("Invalid BrowserAction payload: {e}"))?;
                let res = self.act(action).await?;
                serde_json::to_value(res).map_err(|e| e.to_string())
            }
            "browser_diagnostics" => {
                let clear = params
                    .get("clear")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let diag = self.diagnostics().await;
                if clear {
                    self.clear_diagnostics().await;
                }
                serde_json::to_value(diag).map_err(|e| e.to_string())
            }
            "browser_clear_diagnostics" => {
                self.clear_diagnostics().await;
                Ok(json!({ "cleared": true }))
            }
            "browser_export_pdf" => {
                let out_str = params
                    .get("outputPath")
                    .or_else(|| params.get("output_path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "browser_export_pdf requires 'outputPath'.".to_string())?;
                let res = self.export_pdf(Path::new(out_str)).await?;
                serde_json::to_value(res).map_err(|e| e.to_string())
            }
            "browser_record_walkthrough" => {
                let steps_val = params
                    .get("steps")
                    .cloned()
                    .ok_or_else(|| "browser_record_walkthrough requires 'steps'.".to_string())?;
                let steps: Vec<WalkthroughStep> = serde_json::from_value(steps_val)
                    .map_err(|e| format!("Invalid walkthrough steps: {e}"))?;
                let out_str = params
                    .get("outputMp4Path")
                    .or_else(|| params.get("output_mp4_path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "browser_record_walkthrough requires 'outputMp4Path'.".to_string()
                    })?;
                let res = self.record_walkthrough(steps, Path::new(out_str)).await?;
                serde_json::to_value(res).map_err(|e| e.to_string())
            }
            "browser_close" => {
                self.close().await;
                Ok(json!({ "closed": true }))
            }
            other => Err(format!("Unknown browser RPC method: '{other}'")),
        }
    }
}

// ============================================================================
// Unit Tests (`browser::tests`)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    trait MockIo: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> MockIo for T {}
    type BoxMockIo = Box<dyn MockIo>;

    /// Spawns an in-process mock CDP server using `tokio::net::TcpListener` + `CdpWsConnection::connect`
    /// when loopback TCP is permitted, or seamlessly falls back to `tokio::io::duplex` +
    /// `CdpWsConnection::connect_stream` inside Seatbelt `deny network-outbound` sandboxes.
    async fn connect_mock_cdp_with<F, Fut>(
        host: &str,
        path: &str,
        server_fn: F,
    ) -> (CdpWsConnection, tokio::task::JoinHandle<()>, u16)
    where
        F: FnOnce(BoxMockIo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Ok(listener) = TcpListener::bind("127.0.0.1:0").await {
            let port = listener.local_addr().unwrap().port();
            if let Ok(probe_client) = TcpStream::connect(("127.0.0.1", port)).await {
                let _ = probe_client.set_nodelay(true);
                let (srv_tcp, _) = listener.accept().await.expect("accept mock tcp");
                let server_handle = tokio::spawn(async move {
                    server_fn(Box::new(srv_tcp)).await;
                });
                let ws_url = format!("ws://{host}:{port}{path}");
                let conn = CdpWsConnection::connect_stream(&ws_url, probe_client)
                    .await
                    .expect("connect mock CDP over TcpListener");
                return (conn, server_handle, port);
            }
        }

        let port = 9222u16;
        let (client_io, server_io) = tokio::io::duplex(512 * 1024);
        let server_handle = tokio::spawn(async move {
            server_fn(Box::new(server_io)).await;
        });
        let ws_url = format!("ws://{host}:{port}{path}");
        let conn = CdpWsConnection::connect_stream(&ws_url, client_io)
            .await
            .expect("connect mock CDP over duplex stream");
        (conn, server_handle, port)
    }

    /// Helper: reads one masked client-to-server WebSocket frame from a mock server socket.
    async fn read_masked_client_frame<S: AsyncRead + Unpin + ?Sized>(
        stream: &mut S,
    ) -> Result<(u8, Vec<u8>), String> {
        let mut hdr = [0u8; 2];
        stream
            .read_exact(&mut hdr)
            .await
            .map_err(|e| format!("mock server read hdr: {e}"))?;
        let opcode = hdr[0] & 0x0F;
        let masked = (hdr[1] & 0x80) != 0;
        assert!(masked, "RFC 6455 client frames must be masked");
        let len7 = (hdr[1] & 0x7F) as usize;

        let payload_len = if len7 == 126 {
            let mut ext = [0u8; 2];
            stream
                .read_exact(&mut ext)
                .await
                .map_err(|e| format!("mock server ext16: {e}"))?;
            u16::from_be_bytes(ext) as usize
        } else if len7 == 127 {
            let mut ext = [0u8; 8];
            stream
                .read_exact(&mut ext)
                .await
                .map_err(|e| format!("mock server ext64: {e}"))?;
            u64::from_be_bytes(ext) as usize
        } else {
            len7
        };

        let mut mask = [0u8; 4];
        stream
            .read_exact(&mut mask)
            .await
            .map_err(|e| format!("mock server mask: {e}"))?;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            stream
                .read_exact(&mut payload)
                .await
                .map_err(|e| format!("mock server payload: {e}"))?;
        }
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
        Ok((opcode, payload))
    }

    async fn complete_mock_ws_handshake<S: AsyncRead + AsyncWrite + Unpin + ?Sized>(
        stream: &mut S,
    ) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let hdr_end = loop {
            let n = stream.read(&mut tmp).await.expect("read upgrade request");
            assert!(n > 0, "unexpected EOF in upgrade");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let req_text = String::from_utf8_lossy(&buf[..hdr_end]).to_string();
        let mut ws_key = String::new();
        for line in req_text.split("\r\n") {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("sec-websocket-key") {
                    ws_key = v.trim().to_string();
                }
            }
        }
        assert!(!ws_key.is_empty(), "missing Sec-WebSocket-Key");
        let accept = compute_websocket_accept(&ws_key);
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        stream
            .write_all(resp.as_bytes())
            .await
            .expect("write 101 response");
        stream.flush().await.expect("flush 101 response");
        req_text
    }

    #[test]
    fn rfc6455_sha1_and_base64_accept_vector() {
        // Official RFC 6455 Section 1.3 handshake vector
        let accept = compute_websocket_accept("dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");

        // FIPS 180-4 SHA-1 vectors ("abc" & multi-block >64 bytes)
        let abc_digest = sha1_digest(b"abc");
        assert_eq!(
            abc_digest,
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78,
                0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
        let multi_block =
            sha1_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            multi_block,
            [
                0x84, 0x98, 0x3e, 0x44, 0x1c, 0x3b, 0xd2, 0x6e, 0xba, 0xae, 0x4a, 0xa1, 0xf9,
                0x51, 0x29, 0xe5, 0xe5, 0x46, 0x70, 0xf1
            ]
        );

        // Base64 round-trip for 0, 1, 2, 3, and 257 bytes
        for len in [0usize, 1, 2, 3, 4, 31, 257] {
            let raw: Vec<u8> = (0..len).map(|i| ((i * 73 + 19) & 0xFF) as u8).collect();
            let encoded = base64_encode(&raw);
            let decoded = base64_decode(&encoded).expect("base64 decode");
            assert_eq!(decoded, raw);
        }
    }

    #[test]
    fn ws_client_frame_encoding_covers_7bit_16bit_and_64bit_lengths() {
        let mask = [0x12, 0x34, 0x56, 0x78];

        // 7-bit payload (<= 125)
        let small = b"hello cdp";
        let f_small = encode_ws_client_frame(WS_OPCODE_TEXT, small, mask);
        assert_eq!(f_small[0], 0x81);
        assert_eq!(f_small[1], 0x80 | (small.len() as u8));
        assert_eq!(&f_small[2..6], &mask);
        let unmasked_small: Vec<u8> = f_small[6..]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i & 3])
            .collect();
        assert_eq!(unmasked_small, small);

        // 16-bit payload (126..=65535)
        let med = vec![b'A'; 1024];
        let f_med = encode_ws_client_frame(WS_OPCODE_TEXT, &med, mask);
        assert_eq!(f_med[0], 0x81);
        assert_eq!(f_med[1], 0x80 | 126);
        assert_eq!(u16::from_be_bytes([f_med[2], f_med[3]]), 1024);
        assert_eq!(f_med.len(), 4 + 4 + 1024);

        // 64-bit payload (> 65535)
        let large = vec![b'Z'; 90_000];
        let f_large = encode_ws_client_frame(WS_OPCODE_TEXT, &large, mask);
        assert_eq!(f_large[0], 0x81);
        assert_eq!(f_large[1], 0x80 | 127);
        let decoded_len = u64::from_be_bytes([
            f_large[2], f_large[3], f_large[4], f_large[5], f_large[6], f_large[7], f_large[8],
            f_large[9],
        ]);
        assert_eq!(decoded_len, 90_000);
        assert_eq!(f_large.len(), 10 + 4 + 90_000);
    }

    #[tokio::test]
    async fn cdp_ws_connection_roundtrip_large_payload_ping_pong_and_events() {
        let big_b64 = "A".repeat(160_000); // >64KB extended 64-bit WebSocket frame!
        let big_b64_expected = big_b64.clone();

        let (conn, server_task, _port) = connect_mock_cdp_with(
            "127.0.0.1",
            "/devtools/page/mock-1",
            move |mut sock| async move {
                let req_headers = complete_mock_ws_handshake(&mut *sock).await;
                assert!(req_headers.starts_with("GET /devtools/page/mock-1 HTTP/1.1"));

                // Send a server Ping (0x9) first to verify automatic client masked Pong (0xA)
                let ping_frame = encode_ws_server_frame(WS_OPCODE_PING, b"rursor-ping");
                sock.write_all(&ping_frame).await.unwrap();

                // Send asynchronous CDP console, exception, and network events
                let events = [
                    json!({
                        "method": "Runtime.consoleAPICalled",
                        "params": {
                            "type": "error",
                            "args": [{ "type": "string", "value": "Hydration mismatch in <Hero />" }],
                            "stackTrace": { "callFrames": [{ "url": "http://localhost:3000/src/Hero.tsx", "lineNumber": 41 }] }
                        }
                    }),
                    json!({
                        "method": "Runtime.exceptionThrown",
                        "params": {
                            "exceptionDetails": {
                                "text": "Uncaught TypeError",
                                "url": "http://localhost:3000/src/App.tsx",
                                "lineNumber": 18,
                                "exception": { "description": "Cannot read properties of undefined (reading 'map')" }
                            }
                        }
                    }),
                    json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "req-1",
                            "type": "Fetch",
                            "request": { "url": "http://localhost:3000/api/items", "method": "POST" }
                        }
                    }),
                    json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "req-1",
                            "type": "Fetch",
                            "response": { "url": "http://localhost:3000/api/items", "status": 502, "statusText": "Bad Gateway" }
                        }
                    }),
                    json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "req-ok",
                            "type": "Script",
                            "request": { "url": "http://localhost:3000/main.js", "method": "GET" }
                        }
                    }),
                    json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "req-ok",
                            "type": "Script",
                            "response": { "url": "http://localhost:3000/main.js", "status": 200, "statusText": "OK" }
                        }
                    }),
                    json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "req-2",
                            "type": "XHR",
                            "request": { "url": "http://localhost:3000/api/stream", "method": "GET" }
                        }
                    }),
                    json!({
                        "method": "Network.loadingFailed",
                        "params": {
                            "requestId": "req-2",
                            "type": "XHR",
                            "errorText": "net::ERR_CONNECTION_REFUSED"
                        }
                    }),
                ];
                for ev in events {
                    let bytes = serde_json::to_vec(&ev).unwrap();
                    sock.write_all(&encode_ws_server_frame(WS_OPCODE_TEXT, &bytes))
                        .await
                        .unwrap();
                }
                sock.flush().await.unwrap();

                // Read automatic Pong + two concurrent client commands, reply out-of-order with >64KB payload
                let mut saw_pong = false;
                let mut cmd_ids = Vec::new();
                while !saw_pong || cmd_ids.len() < 2 {
                    let (opcode, payload) = read_masked_client_frame(&mut *sock).await.unwrap();
                    if opcode == WS_OPCODE_PONG {
                        assert_eq!(payload, b"rursor-ping");
                        saw_pong = true;
                    } else if opcode == WS_OPCODE_TEXT {
                        let req: Value = serde_json::from_slice(&payload).unwrap();
                        let id = req.get("id").and_then(Value::as_u64).unwrap();
                        let method = req
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap()
                            .to_string();
                        cmd_ids.push((id, method));
                    }
                }

                // Reply in reverse order to verify ID correlation
                for (id, method) in cmd_ids.into_iter().rev() {
                    let resp = if method == "Page.captureScreenshot" {
                        json!({ "id": id, "result": { "data": big_b64 } })
                    } else {
                        json!({ "id": id, "result": { "frameId": "main-frame" } })
                    };
                    let raw = serde_json::to_vec(&resp).unwrap();
                    sock.write_all(&encode_ws_server_frame(WS_OPCODE_TEXT, &raw))
                        .await
                        .unwrap();
                }
                sock.flush().await.unwrap();
            },
        )
        .await;

        let (nav_res, shot_res) = tokio::join!(
            conn.send_command("Page.navigate", json!({ "url": "http://localhost:3000" })),
            conn.send_command("Page.captureScreenshot", json!({ "format": "png" }))
        );

        assert_eq!(
            nav_res.unwrap().get("frameId").and_then(Value::as_str),
            Some("main-frame")
        );
        let shot_data = shot_res
            .unwrap()
            .get("data")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert_eq!(shot_data.len(), 160_000);
        assert_eq!(shot_data, big_b64_expected);

        let diag = conn.diagnostics().await;
        assert_eq!(diag.console_logs.len(), 2);
        assert_eq!(diag.console_logs[0].level, "error");
        assert!(diag.console_logs[0].text.contains("Hydration mismatch"));
        assert_eq!(diag.console_logs[0].line, Some(42));
        assert_eq!(diag.console_logs[1].level, "exception");
        assert!(diag.console_logs[1].text.contains("Cannot read properties"));

        assert_eq!(diag.network_errors.len(), 2);
        assert_eq!(diag.network_errors[0].status, Some(502));
        assert_eq!(diag.network_errors[0].method, "POST");
        assert_eq!(diag.network_errors[0].url, "http://localhost:3000/api/items");
        assert_eq!(diag.network_errors[1].status, None);
        assert_eq!(
            diag.network_errors[1].error_text,
            "net::ERR_CONNECTION_REFUSED"
        );
        assert_eq!(
            diag.network_errors[1].url,
            "http://localhost:3000/api/stream"
        );

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn cdp_ws_handles_fragmented_multi_frame_messages() {
        let (conn, server, _port) = connect_mock_cdp_with(
            "localhost",
            "/devtools/page/frag",
            |mut sock| async move {
                complete_mock_ws_handshake(&mut *sock).await;

                let (op, payload) = read_masked_client_frame(&mut *sock).await.unwrap();
                assert_eq!(op, WS_OPCODE_TEXT);
                let req: Value = serde_json::from_slice(&payload).unwrap();
                let id = req.get("id").and_then(Value::as_u64).unwrap();

                let full_json =
                    serde_json::to_vec(&json!({ "id": id, "result": { "fragmented": "ok-12345" } }))
                        .unwrap();
                let mid = full_json.len() / 2;

                // Fragment 1: FIN=0, opcode=TEXT
                let mut f1 = vec![WS_OPCODE_TEXT, mid as u8];
                f1.extend_from_slice(&full_json[..mid]);
                sock.write_all(&f1).await.unwrap();

                // Interleaved Ping mid-fragmentation (valid per RFC 6455 Section 5.4)
                sock.write_all(&encode_ws_server_frame(WS_OPCODE_PING, b"frag"))
                    .await
                    .unwrap();

                // Fragment 2: FIN=1, opcode=CONTINUATION
                let rem = &full_json[mid..];
                let mut f2 = vec![0x80 | WS_OPCODE_CONTINUATION, rem.len() as u8];
                f2.extend_from_slice(rem);
                sock.write_all(&f2).await.unwrap();
                sock.flush().await.unwrap();

                let (pong_op, pong_payload) = read_masked_client_frame(&mut *sock).await.unwrap();
                assert_eq!(pong_op, WS_OPCODE_PONG);
                assert_eq!(pong_payload, b"frag");
            },
        )
        .await;

        let res = conn.send_command("Runtime.enable", json!({})).await.unwrap();
        assert_eq!(
            res.get("fragmented").and_then(Value::as_str),
            Some("ok-12345")
        );
        server.await.unwrap();
    }

    #[test]
    fn url_validator_allows_http_https_and_localhost_dev_servers() {
        assert_eq!(
            validate_browser_url("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert_eq!(
            validate_browser_url("  http://127.0.0.1:5173/dashboard?tab=metrics#top ").unwrap(),
            "http://127.0.0.1:5173/dashboard?tab=metrics#top"
        );
        assert_eq!(
            validate_browser_url("https://example.com/docs/getting-started").unwrap(),
            "https://example.com/docs/getting-started"
        );
        assert_eq!(
            validate_browser_url("http://[::1]:8080/health").unwrap(),
            "http://[::1]:8080/health"
        );
    }

    #[test]
    fn url_validator_blocks_unsafe_schemes_and_cloud_metadata() {
        for blocked in [
            "",
            "   ",
            "file:///etc/passwd",
            "FILE:///Users/test/.ssh/id_rsa",
            "javascript:alert(1)",
            "JavaScript:fetch('http://evil')",
            "data:text/html,<script>alert(1)</script>",
            "chrome://settings",
            "devtools://devtools/bundled/inspector.html",
            "about:blank",
            "ws://127.0.0.1:9222/devtools",
            "http://169.254.169.254/latest/meta-data/",
            "https://169.254.169.254/computeMetadata/v1/",
            "http://169.254.170.2/v2/credentials",
            "http://2852039166/latest/meta-data/",
            "http://0xa9fea9fe/latest",
            "http://[::ffff:169.254.169.254]/latest",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://100.100.100.200/latest/meta-data/",
            "http://user:pass@localhost:3000/",
        ] {
            assert!(
                validate_browser_url(blocked).is_err(),
                "Expected URL '{blocked}' to be rejected"
            );
        }
    }

    #[test]
    fn parses_devtools_stderr_and_rejects_remote_ws_hosts() {
        let line = "DevTools listening on ws://127.0.0.1:54921/devtools/browser/2f1a9c0e-1234";
        let (port, ws) = parse_devtools_ws_from_stderr_line(line).expect("parse stderr line");
        assert_eq!(port, 54921);
        assert_eq!(
            ws,
            "ws://127.0.0.1:54921/devtools/browser/2f1a9c0e-1234"
        );

        assert!(parse_local_ws_url("ws://192.168.1.50:9222/devtools/page/1").is_err());
        assert!(parse_local_ws_url("wss://127.0.0.1:9222/devtools/page/1").is_err());
    }

    #[test]
    fn discovers_macos_chromium_and_ffmpeg_binaries() {
        let chrome = discover_chromium_binary().expect("discover macOS Chromium");
        assert!(chrome.is_file(), "Chromium binary should exist on disk");

        let ffmpeg = discover_ffmpeg_binary().expect("discover ffmpeg");
        assert!(ffmpeg.is_file(), "ffmpeg binary should exist on disk");
    }

    #[tokio::test]
    async fn encodes_synthetic_png_and_ppm_walkthrough_frames_to_valid_mp4() {
        let base_tmp = std::env::temp_dir().join(format!(
            "rursor_mp4_test_{}_{}",
            std::process::id(),
            now_epoch_ms()
        ));
        let png_dir = base_tmp.join("png_frames");
        let ppm_dir = base_tmp.join("ppm_frames");
        fs::create_dir_all(&png_dir).unwrap();
        fs::create_dir_all(&ppm_dir).unwrap();

        let width = 160u32;
        let height = 100u32;
        for idx in 1..=8u32 {
            let mut rgb = vec![0u8; (width * height * 3) as usize];
            for y in 0..height {
                for x in 0..width {
                    let off = ((y * width + x) * 3) as usize;
                    // Obsidian canvas (#0B0D12) with animated Rursor violet (#7C5CFF) bar
                    let highlight = x >= idx * 12 && x <= idx * 12 + 28 && y >= 30 && y <= 70;
                    if highlight {
                        rgb[off] = 0x7C;
                        rgb[off + 1] = 0x5C;
                        rgb[off + 2] = 0xFF;
                    } else {
                        rgb[off] = 0x0B;
                        rgb[off + 1] = 0x0D;
                        rgb[off + 2] = 0x12;
                    }
                }
            }
            let png_bytes = encode_uncompressed_rgb_png(width, height, &rgb).unwrap();
            assert_eq!(&png_bytes[..8], b"\x89PNG\r\n\x1a\n");
            fs::write(png_dir.join(format!("frame_{idx:04}.png")), &png_bytes).unwrap();

            let ppm_bytes = encode_binary_ppm(width, height, &rgb).unwrap();
            fs::write(ppm_dir.join(format!("frame_{idx:04}.ppm")), &ppm_bytes).unwrap();
        }

        let png_mp4 = base_tmp.join("walkthrough_from_png.mp4");
        let (png_mp4_len, encoder_used) =
            encode_frames_to_mp4(&png_dir, "png", 6, &png_mp4)
                .await
                .expect("encode PNG frames to MP4");
        assert!(png_mp4_len > 256);
        assert!(!encoder_used.is_empty());
        let mp4_bytes = fs::read(&png_mp4).unwrap();
        assert_eq!(
            &mp4_bytes[4..8],
            b"ftyp",
            "MP4 from PNG frames must start with ISO BMFF ftyp box"
        );

        let ppm_mp4 = base_tmp.join("walkthrough_from_ppm.mp4");
        let (ppm_mp4_len, _) = encode_frames_to_mp4(&ppm_dir, "ppm", 6, &ppm_mp4)
            .await
            .expect("encode PPM frames to MP4");
        assert!(ppm_mp4_len > 256);
        let ppm_mp4_bytes = fs::read(&ppm_mp4).unwrap();
        assert_eq!(
            &ppm_mp4_bytes[4..8],
            b"ftyp",
            "MP4 from PPM frames must start with ISO BMFF ftyp box"
        );

        let _ = fs::remove_dir_all(&base_tmp);
    }

    #[tokio::test]
    async fn mock_browser_session_end_to_end_snapshot_act_pdf_and_walkthrough_mp4() {
        // Pre-build a valid synthetic PNG screenshot for Page.captureScreenshot
        let rgb = vec![0x18u8; 64 * 40 * 3];
        let sample_png = encode_uncompressed_rgb_png(64, 40, &rgb).unwrap();
        let sample_png_b64 = base64_encode(&sample_png);
        let sample_pdf_b64 = base64_encode(b"%PDF-1.7\n1 0 obj<<>>endobj\n%%EOF\n");

        let (cdp, server, port) = connect_mock_cdp_with(
            "127.0.0.1",
            "/devtools/page/e2e",
            move |mut sock| async move {
                complete_mock_ws_handshake(&mut *sock).await;

                while let Ok((opcode, payload)) = read_masked_client_frame(&mut *sock).await {
                    if opcode == WS_OPCODE_CLOSE {
                        break;
                    }
                    if opcode != WS_OPCODE_TEXT {
                        continue;
                    }
                    let req: Value = serde_json::from_slice(&payload).unwrap();
                    let id = req.get("id").and_then(Value::as_u64).unwrap();
                    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

                    let result = match method {
                        "Page.navigate" => {
                            let ev = serde_json::to_vec(&json!({
                                "method": "Page.loadEventFired",
                                "params": { "timestamp": 1.0 }
                            }))
                            .unwrap();
                            sock.write_all(&encode_ws_server_frame(WS_OPCODE_TEXT, &ev))
                                .await
                                .unwrap();
                            json!({ "frameId": "root" })
                        }
                        "Page.captureScreenshot" => json!({ "data": sample_png_b64 }),
                        "Page.printToPDF" => json!({ "data": sample_pdf_b64 }),
                        "Runtime.evaluate" => {
                            let expr = params
                                .get("expression")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if expr.contains("MAX_ELEMENTS = 120") {
                                json!({
                                    "result": {
                                        "value": {
                                            "url": "http://localhost:3000/login",
                                            "title": "Rursor Demo Login",
                                            "viewportWidth": 1280,
                                            "viewportHeight": 800,
                                            "textSummary": "Sign in to Rursor Studio",
                                            "elements": [{
                                                "id": 1,
                                                "tag": "button",
                                                "inputType": "submit",
                                                "role": "button",
                                                "label": "Sign In",
                                                "placeholder": null,
                                                "href": null,
                                                "selector": "#sign-in-btn",
                                                "bounds": { "x": 480.0, "y": 320.0, "width": 160.0, "height": 44.0 },
                                                "sourceHint": { "file": "src/components/LoginButton.tsx", "line": 27 }
                                            }]
                                        }
                                    }
                                })
                            } else if expr.contains("scrollIntoView") {
                                json!({
                                    "result": {
                                        "value": { "found": true, "x": 560.0, "y": 342.0, "tag": "button" }
                                    }
                                })
                            } else {
                                json!({
                                    "result": {
                                        "value": { "url": "http://localhost:3000/login", "title": "Rursor Demo Login" }
                                    }
                                })
                            }
                        }
                        _ => json!({}),
                    };

                    let reply = serde_json::to_vec(&json!({ "id": id, "result": result })).unwrap();
                    sock.write_all(&encode_ws_server_frame(WS_OPCODE_TEXT, &reply))
                        .await
                        .unwrap();
                    sock.flush().await.unwrap();
                }
            },
        )
        .await;
        let dummy_child = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn dummy child");

        let tmp_dir = std::env::temp_dir().join(format!(
            "rursor_session_e2e_{}_{}",
            std::process::id(),
            now_epoch_ms()
        ));
        let mut session = BrowserSession::from_cdp_connection(
            cdp,
            dummy_child,
            port,
            1280,
            800,
            Some(tmp_dir.join("artifacts")),
        );

        let snap = session.snapshot(true, true).await.expect("snapshot");
        assert_eq!(snap.title, "Rursor Demo Login");
        assert_eq!(snap.elements.len(), 1);
        assert_eq!(snap.elements[0].id, 1);
        assert_eq!(
            snap.elements[0].source_hint,
            Some(SourceHint {
                file: "src/components/LoginButton.tsx".to_string(),
                line: 27
            })
        );
        assert!(snap.screenshot_path.is_some());

        let click_res = session
            .act(BrowserAction::Click {
                som_id: Some(1),
                selector: None,
                x: None,
                y: None,
            })
            .await
            .expect("act click");
        assert_eq!(click_res.resolved_x, Some(560.0));
        assert_eq!(click_res.resolved_y, Some(342.0));

        let pdf_path = tmp_dir.join("out.pdf");
        let pdf_res = session.export_pdf(&pdf_path).await.expect("export pdf");
        assert!(pdf_res.bytes_written > 10);
        assert!(fs::read(&pdf_path).unwrap().starts_with(b"%PDF-1.7"));

        let mp4_path = tmp_dir.join("walkthrough.mp4");
        let rec = session
            .record_walkthrough(
                vec![
                    WalkthroughStep::Navigate {
                        url: "http://localhost:3000/login".to_string(),
                        viewport_width: Some(1280),
                        viewport_height: Some(800),
                    },
                    WalkthroughStep::Click {
                        som_id: Some(1),
                        selector: None,
                        x: None,
                        y: None,
                    },
                    WalkthroughStep::WaitMs { ms: 50 },
                ],
                &mp4_path,
            )
            .await
            .expect("record walkthrough");
        assert!(rec.frame_count >= 10);
        let mp4_header = fs::read(&mp4_path).unwrap();
        assert_eq!(&mp4_header[4..8], b"ftyp");

        session.shutdown().await;
        server.await.unwrap();
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
