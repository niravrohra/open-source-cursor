use reqwest::Client;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command as TokioCommand;

const GEMINI_V1BETA_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_VEO_MODEL: &str = "veo-2.0-generate-001";
const DEFAULT_FLASH_IMAGE_MODEL: &str = "gemini-2.0-flash-exp-image-generation";
const PPTX_SLIDE_WIDTH_EMU: i64 = 12_192_000;
const PPTX_SLIDE_HEIGHT_EMU: i64 = 6_858_000;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

fn deserialize_string_or_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

fn default_video_width() -> u32 {
    1280
}

fn default_video_height() -> u32 {
    720
}

fn default_video_fps() -> u32 {
    24
}

fn default_scene_duration() -> f32 {
    3.5
}

// ============================================================================
// Public Specs & Artifacts
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlideMetric {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub label: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub value: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub delta: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlideSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub layout: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub subtitle: String,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub right_bullets: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<SlideMetric>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub speaker_notes: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentationSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub subtitle: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub author: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub theme: String,
    pub output_path: String,
    #[serde(default)]
    pub slides: Vec<SlideSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentationArtifact {
    pub pptx_path: String,
    pub html_deck_path: String,
    pub relative_pptx_path: String,
    pub relative_html_deck_path: String,
    pub slide_count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSceneSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub heading: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub subheading: String,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub code_snippet: Option<String>,
    #[serde(default)]
    pub narration: Option<String>,
    #[serde(default = "default_scene_duration")]
    pub duration_secs: f32,
    #[serde(default)]
    pub accent_hex: Option<String>,
}

impl Default for VideoSceneSpec {
    fn default() -> Self {
        Self {
            heading: String::new(),
            subheading: String::new(),
            bullets: Vec::new(),
            code_snippet: None,
            narration: None,
            duration_secs: default_scene_duration(),
            accent_hex: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStoryboardSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub title: String,
    pub output_path: String,
    #[serde(default = "default_video_width")]
    pub width: u32,
    #[serde(default = "default_video_height")]
    pub height: u32,
    #[serde(default = "default_video_fps")]
    pub fps: u32,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub scenes: Vec<VideoSceneSpec>,
}

impl Default for VideoStoryboardSpec {
    fn default() -> Self {
        Self {
            title: String::new(),
            output_path: "videos/explainer.mp4".to_string(),
            width: default_video_width(),
            height: default_video_height(),
            fps: default_video_fps(),
            voice: None,
            scenes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VeoVideoSpec {
    pub prompt: String,
    pub output_path: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub duration_secs: Option<u32>,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub max_wait_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoArtifact {
    pub video_path: String,
    pub relative_video_path: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub duration_secs: f32,
    pub scene_count: usize,
    pub has_voiceover: bool,
    pub engine: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAssetSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub prompt: String,
    pub output_path: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub svg_markup: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub accent_hex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAssetArtifact {
    pub image_path: String,
    pub relative_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub svg_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub engine: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    String(String),
    Number(f64),
    Formula(String),
}

impl Serialize for CellValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            CellValue::String(s) => {
                let mut map = serializer.serialize_map(Some(2))?;
                use serde::ser::SerializeMap;
                map.serialize_entry("type", "string")?;
                map.serialize_entry("value", s)?;
                map.end()
            }
            CellValue::Number(n) => {
                let mut map = serializer.serialize_map(Some(2))?;
                use serde::ser::SerializeMap;
                map.serialize_entry("type", "number")?;
                map.serialize_entry("value", n)?;
                map.end()
            }
            CellValue::Formula(f) => {
                let mut map = serializer.serialize_map(Some(2))?;
                use serde::ser::SerializeMap;
                map.serialize_entry("type", "formula")?;
                map.serialize_entry("value", f)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for CellValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CellVisitor;

        impl<'de> Visitor<'de> for CellVisitor {
            type Value = CellValue;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string, number, boolean, null, or typed cell object")
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(CellValue::Number(v as f64))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(CellValue::Number(v as f64))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(CellValue::Number(v))
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(CellValue::String(if v { "TRUE" } else { "FALSE" }.to_string()))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(CellValue::String(String::new()))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CellValue::String(String::new()))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                if v.trim_start().starts_with('=') {
                    Ok(CellValue::Formula(v.to_string()))
                } else {
                    Ok(CellValue::String(v.to_string()))
                }
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                if v.trim_start().starts_with('=') {
                    Ok(CellValue::Formula(v))
                } else {
                    Ok(CellValue::String(v))
                }
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut kind: Option<String> = None;
                let mut val: Option<Value> = None;
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    match k.as_str() {
                        "Formula" | "formula" => {
                            let s = v
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string());
                            return Ok(CellValue::Formula(s));
                        }
                        "Number" | "number" => {
                            let n = v
                                .as_f64()
                                .ok_or_else(|| de::Error::custom("expected f64 for Number cell"))?;
                            return Ok(CellValue::Number(n));
                        }
                        "String" | "string" => {
                            let s = v
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string());
                            return Ok(CellValue::String(s));
                        }
                        "type" | "kind" => {
                            kind = v.as_str().map(|s| s.to_ascii_lowercase());
                        }
                        "value" => {
                            val = Some(v);
                        }
                        _ => {}
                    }
                }

                match (kind.as_deref(), val) {
                    (Some("formula"), Some(v)) => Ok(CellValue::Formula(
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    )),
                    (Some("number"), Some(v)) => v
                        .as_f64()
                        .map(CellValue::Number)
                        .ok_or_else(|| de::Error::custom("invalid numeric cell value")),
                    (_, Some(Value::Number(n))) => n
                        .as_f64()
                        .map(CellValue::Number)
                        .ok_or_else(|| de::Error::custom("invalid numeric cell value")),
                    (_, Some(Value::String(s))) => {
                        if kind.as_deref() == Some("string") {
                            Ok(CellValue::String(s))
                        } else if s.trim_start().starts_with('=') {
                            Ok(CellValue::Formula(s))
                        } else {
                            Ok(CellValue::String(s))
                        }
                    }
                    (_, Some(other)) => Ok(CellValue::String(other.to_string())),
                    _ => Ok(CellValue::String(String::new())),
                }
            }
        }

        deserializer.deserialize_any(CellVisitor)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub name: String,
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<CellValue>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpreadsheetSpec {
    pub output_path: String,
    #[serde(default)]
    pub sheets: Vec<SheetSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpreadsheetArtifact {
    pub xlsx_path: String,
    pub relative_xlsx_path: String,
    pub sheet_count: usize,
    pub row_count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocTable {
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocSection {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub heading: String,
    #[serde(default)]
    pub paragraphs: Vec<String>,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub code_block: Option<String>,
    #[serde(default)]
    pub table: Option<DocTable>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSpec {
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub subtitle: String,
    pub output_path: String,
    #[serde(default)]
    pub sections: Vec<DocSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentArtifact {
    pub docx_path: String,
    pub relative_docx_path: String,
    pub section_count: usize,
    pub bytes: u64,
}

// ============================================================================
// Security: Strict Workspace Path Confinement & Temp Guard
// ============================================================================

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn create(prefix: &str) -> Result<Self, String> {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "rursor_{}_{}_{}_{}",
            prefix,
            std::process::id(),
            nanos,
            seq
        ));
        fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create staging directory '{}': {}", dir.display(), e))?;
        Ok(Self { path: dir })
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Resolves `raw_output_path` strictly inside `workspace_root`, rejecting `..` traversal,
/// null bytes, root escapes, and symlink escapes outside the workspace.
pub fn resolve_confined_output_path(
    workspace_root: &Path,
    raw_output_path: &str,
) -> Result<(PathBuf, String), String> {
    let trimmed = raw_output_path.trim();
    if trimmed.is_empty() {
        return Err("output_path cannot be empty".to_string());
    }
    if trimmed.contains('\0') {
        return Err("output_path contains invalid null byte".to_string());
    }

    if !workspace_root.exists() {
        fs::create_dir_all(workspace_root).map_err(|e| {
            format!(
                "failed to initialize workspace_root '{}': {}",
                workspace_root.display(),
                e
            )
        })?;
    }
    let canon_root = fs::canonicalize(workspace_root).map_err(|e| {
        format!(
            "failed to canonicalize workspace_root '{}': {}",
            workspace_root.display(),
            e
        )
    })?;

    let input_path = Path::new(trimmed);
    let mut clean_rel = PathBuf::new();

    if input_path.is_absolute() {
        // Reject any `..` in absolute path before checking prefix
        for comp in input_path.components() {
            if matches!(comp, Component::ParentDir) {
                return Err(format!(
                    "path traversal ('..') is forbidden in output_path '{}'",
                    trimmed
                ));
            }
        }
        // Must lie strictly under workspace_root (or canon_root)
        let stripped = input_path
            .strip_prefix(&canon_root)
            .or_else(|_| input_path.strip_prefix(workspace_root))
            .map_err(|_| {
                format!(
                    "output_path '{}' escapes workspace_root '{}'",
                    trimmed,
                    canon_root.display()
                )
            })?;
        for comp in stripped.components() {
            match comp {
                Component::Normal(seg) => clean_rel.push(seg),
                Component::CurDir => {}
                _ => {
                    return Err(format!(
                        "disallowed path component in output_path '{}'",
                        trimmed
                    ));
                }
            }
        }
    } else {
        for comp in input_path.components() {
            match comp {
                Component::Normal(seg) => clean_rel.push(seg),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(format!(
                        "path traversal ('..') is forbidden in output_path '{}'",
                        trimmed
                    ));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "root/prefix component is forbidden in relative output_path '{}'",
                        trimmed
                    ));
                }
            }
        }
    }

    if clean_rel.as_os_str().is_empty() || clean_rel.file_name().is_none() {
        return Err(format!(
            "output_path '{}' must specify a valid file name inside workspace",
            trimmed
        ));
    }

    // Walk component-by-component ensuring no pre-existing symlink points outside canon_root
    let mut cursor = canon_root.clone();
    if let Some(parent_rel) = clean_rel.parent() {
        for seg in parent_rel.components() {
            if let Component::Normal(name) = seg {
                cursor.push(name);
                if cursor.exists() {
                    let canon_step = fs::canonicalize(&cursor).map_err(|e| {
                        format!("failed to canonicalize '{}': {}", cursor.display(), e)
                    })?;
                    if !canon_step.starts_with(&canon_root) {
                        return Err(format!(
                            "symlink escape detected at '{}' outside workspace '{}'",
                            cursor.display(),
                            canon_root.display()
                        ));
                    }
                }
            }
        }
        fs::create_dir_all(&cursor)
            .map_err(|e| format!("failed to create parent directory '{}': {}", cursor.display(), e))?;
    }

    let canon_parent = fs::canonicalize(&cursor)
        .map_err(|e| format!("failed to canonicalize parent '{}': {}", cursor.display(), e))?;
    if !canon_parent.starts_with(&canon_root) {
        return Err(format!(
            "resolved parent '{}' escapes workspace_root '{}'",
            canon_parent.display(),
            canon_root.display()
        ));
    }

    let file_name = clean_rel
        .file_name()
        .ok_or_else(|| "missing output file name".to_string())?;
    let final_abs = canon_parent.join(file_name);
    if final_abs.exists() {
        if let Ok(canon_existing) = fs::canonicalize(&final_abs) {
            if !canon_existing.starts_with(&canon_root) {
                return Err(format!(
                    "target file symlink '{}' escapes workspace_root '{}'",
                    final_abs.display(),
                    canon_root.display()
                ));
            }
        }
    }

    let rel_display = clean_rel.to_string_lossy().replace('\\', "/");
    Ok((final_abs, rel_display))
}

// ============================================================================
// Escaping & RFC 4648 Base64 Utilities
// ============================================================================

/// Escapes XML 1.0 special characters (`&`, `<`, `>`, `"`, `'`) and strips illegal control chars.
pub fn xml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Escapes HTML5 text and attribute special characters.
pub fn html_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(B64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn base64_decode(raw: &str) -> Result<Vec<u8>, String> {
    let payload = match raw.find(";base64,") {
        Some(idx) => &raw[idx + 8..],
        None => raw,
    };

    let mut sextets = Vec::with_capacity(payload.len());
    for byte in payload.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => {
                return Err(format!(
                    "invalid base64 byte 0x{:02X} ('{}')",
                    byte, byte as char
                ));
            }
        };
        sextets.push(val);
    }

    if sextets.len() % 4 == 1 {
        return Err("invalid base64 payload length".to_string());
    }

    let mut out = Vec::with_capacity((sextets.len() * 3) / 4);
    for group in sextets.chunks(4) {
        let g0 = group[0] as u32;
        let g1 = group[1] as u32;
        out.push(((g0 << 2) | (g1 >> 4)) as u8);
        if group.len() > 2 {
            let g2 = group[2] as u32;
            out.push((((g1 & 0x0F) << 4) | (g2 >> 2)) as u8);
            if group.len() > 3 {
                let g3 = group[3] as u32;
                out.push((((g2 & 0x03) << 6) | g3) as u8);
            }
        }
    }
    Ok(out)
}

fn package_openxml_zip(entries: &[(String, String)], dest_zip: &Path) -> Result<u64, String> {
    let guard = TempDirGuard::create("openxml")?;
    for (rel_path, content) in entries {
        let entry_path = guard.path.join(rel_path);
        if let Some(parent) = entry_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create archive dir '{}': {}",
                    parent.display(),
                    e
                )
            })?;
        }
        fs::write(&entry_path, content.as_bytes()).map_err(|e| {
            format!(
                "failed to write archive entry '{}': {}",
                entry_path.display(),
                e
            )
        })?;
    }

    if dest_zip.exists() {
        fs::remove_file(dest_zip)
            .map_err(|e| format!("failed to replace existing '{}': {}", dest_zip.display(), e))?;
    }

    let output = StdCommand::new("/usr/bin/zip")
        .arg("-q")
        .arg("-r")
        .arg(dest_zip)
        .arg(".")
        .current_dir(&guard.path)
        .output()
        .map_err(|e| format!("failed to invoke /usr/bin/zip: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "/usr/bin/zip exited with status {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let meta = fs::metadata(dest_zip)
        .map_err(|e| format!("failed to stat '{}': {}", dest_zip.display(), e))?;
    Ok(meta.len())
}

// ============================================================================
// 1. PowerPoint (.pptx) + Interactive HTML5 Slide Deck Generator
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct DeckPalette {
    name: &'static str,
    bg: &'static str,
    card: &'static str,
    border: &'static str,
    fg: &'static str,
    muted: &'static str,
    accent: &'static str,
    accent_alt: &'static str,
    code_bg: &'static str,
    code_fg: &'static str,
    positive: &'static str,
    negative: &'static str,
}

fn resolve_deck_palette(theme: &str) -> DeckPalette {
    match theme.trim().to_ascii_lowercase().as_str() {
        "minimal" | "light" => DeckPalette {
            name: "Minimal",
            bg: "FFFFFF",
            card: "F4F6FA",
            border: "CBD5E1",
            fg: "0F172A",
            muted: "475569",
            accent: "4F46E5",
            accent_alt: "0D9488",
            code_bg: "0F172A",
            code_fg: "E2E8F0",
            positive: "059669",
            negative: "DC2626",
        },
        "executive" | "slate" => DeckPalette {
            name: "Executive",
            bg: "0F172A",
            card: "1E293B",
            border: "334155",
            fg: "F8FAFC",
            muted: "94A3B8",
            accent: "38BDF8",
            accent_alt: "34D399",
            code_bg: "090E1A",
            code_fg: "E2E8F0",
            positive: "34D399",
            negative: "F87171",
        },
        _ => DeckPalette {
            name: "Obsidian",
            bg: "0B0D12",
            card: "131822",
            border: "252E42",
            fg: "F3F5F9",
            muted: "9AA4B8",
            accent: "7C5CFF",
            accent_alt: "2AD8A7",
            code_bg: "080A0F",
            code_fg: "E6EDF3",
            positive: "2AD8A7",
            negative: "FF6B81",
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn pptx_rect_shape_xml(
    shape_id: u32,
    name: &str,
    x: i64,
    y: i64,
    cx: i64,
    cy: i64,
    geom: &str,
    fill_hex: Option<&str>,
    border_hex: Option<&str>,
    body_paragraphs_xml: &str,
    anchor: &str,
) -> String {
    let fill_xml = match fill_hex {
        Some(hex) => format!("<a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill>", hex),
        None => "<a:noFill/>".to_string(),
    };
    let line_xml = match border_hex {
        Some(hex) => format!(
            "<a:ln w=\"12700\"><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill></a:ln>",
            hex
        ),
        None => "<a:ln><a:noFill/></a:ln>".to_string(),
    };
    let av_lst = if geom == "roundRect" {
        "<a:avLst><a:gd name=\"adj\" fmla=\"val 6500\"/></a:avLst>"
    } else {
        "<a:avLst/>"
    };

    format!(
        "<p:sp>\
<p:nvSpPr>\
<p:cNvPr id=\"{shape_id}\" name=\"{name}\"/>\
<p:cNvSpPr txBox=\"1\"/>\
<p:nvPr/>\
</p:nvSpPr>\
<p:spPr>\
<a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
<a:prstGeom prst=\"{geom}\">{av_lst}</a:prstGeom>\
{fill_xml}\
{line_xml}\
</p:spPr>\
<p:txBody>\
<a:bodyPr wrap=\"square\" lIns=\"182880\" tIns=\"137160\" rIns=\"182880\" bIns=\"137160\" anchor=\"{anchor}\"/>\
<a:lstStyle/>\
{body_paragraphs_xml}\
</p:txBody>\
</p:sp>",
        shape_id = shape_id,
        name = xml_escape(name),
        x = x,
        y = y,
        cx = cx,
        cy = cy,
        geom = geom,
        av_lst = av_lst,
        fill_xml = fill_xml,
        line_xml = line_xml,
        anchor = anchor,
        body_paragraphs_xml = body_paragraphs_xml
    )
}

fn pptx_paragraph_xml(
    text: &str,
    font_size_hundredths_pt: u32,
    color_hex: &str,
    bold: bool,
    italic: bool,
    align: &str,
    font_face: &str,
    bullet_color_hex: Option<&str>,
    space_after_pts: u32,
) -> String {
    let b_attr = if bold { " b=\"1\"" } else { "" };
    let i_attr = if italic { " i=\"1\"" } else { "" };
    let spc_after = if space_after_pts > 0 {
        format!(
            "<a:spcAft><a:spcPts val=\"{}\"/></a:spcAft>",
            space_after_pts * 100
        )
    } else {
        String::new()
    };
    let (mar_attrs, bullet_xml) = match bullet_color_hex {
        Some(bhex) => (
            " marL=\"274320\" indent=\"-182880\"",
            format!(
                "<a:buClr><a:srgbClr val=\"{}\"/></a:buClr><a:buChar char=\"&#8226;\"/>",
                bhex
            ),
        ),
        None => ("", "<a:buNone/>".to_string()),
    };

    format!(
        "<a:p>\
<a:pPr algn=\"{align}\"{mar_attrs}>{spc_after}{bullet_xml}</a:pPr>\
<a:r>\
<a:rPr lang=\"en-US\" sz=\"{sz}\"{b_attr}{i_attr} dirty=\"0\">\
<a:solidFill><a:srgbClr val=\"{color_hex}\"/></a:solidFill>\
<a:latin typeface=\"{font_face}\"/>\
<a:cs typeface=\"{font_face}\"/>\
</a:rPr>\
<a:t>{escaped}</a:t>\
</a:r>\
</a:p>",
        align = align,
        mar_attrs = mar_attrs,
        spc_after = spc_after,
        bullet_xml = bullet_xml,
        sz = font_size_hundredths_pt,
        b_attr = b_attr,
        i_attr = i_attr,
        color_hex = color_hex,
        font_face = xml_escape(font_face),
        escaped = xml_escape(text)
    )
}

fn build_slide_xml(
    slide_idx_1_based: usize,
    total_slides: usize,
    deck_title: &str,
    author: &str,
    slide: &SlideSpec,
    pal: &DeckPalette,
) -> String {
    let layout = slide.layout.trim().to_ascii_lowercase();
    let mut shapes = String::with_capacity(4096);
    let mut next_id: u32 = 2;

    // Top accent bar across widescreen canvas
    shapes.push_str(&pptx_rect_shape_xml(
        next_id,
        "TopAccentBar",
        0,
        0,
        PPTX_SLIDE_WIDTH_EMU,
        68_580,
        "rect",
        Some(pal.accent),
        None,
        "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>",
        "t",
    ));
    next_id += 1;

    if layout == "title" {
        let mut hero_paras = String::new();
        hero_paras.push_str(&pptx_paragraph_xml(
            if slide.title.is_empty() {
                deck_title
            } else {
                &slide.title
            },
            4400,
            pal.fg,
            true,
            false,
            "l",
            "Helvetica Neue",
            None,
            16,
        ));
        if !slide.subtitle.is_empty() {
            hero_paras.push_str(&pptx_paragraph_xml(
                &slide.subtitle,
                2100,
                pal.muted,
                false,
                false,
                "l",
                "Helvetica Neue",
                None,
                14,
            ));
        }
        for b in &slide.bullets {
            hero_paras.push_str(&pptx_paragraph_xml(
                b,
                1600,
                pal.fg,
                false,
                false,
                "l",
                "Helvetica Neue",
                Some(pal.accent_alt),
                8,
            ));
        }

        shapes.push_str(&pptx_rect_shape_xml(
            next_id,
            "TitleHeroCard",
            914_400,
            1_371_600,
            10_363_200,
            3_962_400,
            "roundRect",
            Some(pal.card),
            Some(pal.border),
            &hero_paras,
            "ctr",
        ));
        next_id += 1;

        if !author.is_empty() {
            let meta_p = pptx_paragraph_xml(
                &format!("Prepared by {}  |  {} Theme", author, pal.name),
                1300,
                pal.accent_alt,
                true,
                false,
                "l",
                "Helvetica Neue",
                None,
                0,
            );
            shapes.push_str(&pptx_rect_shape_xml(
                next_id,
                "AuthorBadge",
                914_400,
                5_600_000,
                7_500_000,
                500_000,
                "rect",
                None,
                None,
                &meta_p,
                "ctr",
            ));
            next_id += 1;
        }
    } else {
        // Standard Slide Header
        let mut header_paras = pptx_paragraph_xml(
            if slide.title.is_empty() {
                deck_title
            } else {
                &slide.title
            },
            2800,
            pal.fg,
            true,
            false,
            "l",
            "Helvetica Neue",
            None,
            6,
        );
        if !slide.subtitle.is_empty() {
            header_paras.push_str(&pptx_paragraph_xml(
                &slide.subtitle,
                1500,
                pal.muted,
                false,
                false,
                "l",
                "Helvetica Neue",
                None,
                0,
            ));
        }
        shapes.push_str(&pptx_rect_shape_xml(
            next_id,
            "SlideHeader",
            731_520,
            365_760,
            10_728_960,
            1_188_720,
            "rect",
            None,
            None,
            &header_paras,
            "t",
        ));
        next_id += 1;

        match layout.as_str() {
            "two_column" => {
                let col_w = 5_181_600;
                let col_y = 1_737_360;
                let col_h = 4_389_120;

                let mut left_paras = String::new();
                if slide.bullets.is_empty() {
                    left_paras.push_str(&pptx_paragraph_xml(
                        "Key Points",
                        1800,
                        pal.muted,
                        false,
                        false,
                        "l",
                        "Helvetica Neue",
                        None,
                        8,
                    ));
                } else {
                    for item in &slide.bullets {
                        left_paras.push_str(&pptx_paragraph_xml(
                            item,
                            1700,
                            pal.fg,
                            false,
                            false,
                            "l",
                            "Helvetica Neue",
                            Some(pal.accent),
                            10,
                        ));
                    }
                }
                shapes.push_str(&pptx_rect_shape_xml(
                    next_id,
                    "LeftColumnCard",
                    731_520,
                    col_y,
                    col_w,
                    col_h,
                    "roundRect",
                    Some(pal.card),
                    Some(pal.border),
                    &left_paras,
                    "t",
                ));
                next_id += 1;

                let mut right_paras = String::new();
                if slide.right_bullets.is_empty() {
                    right_paras.push_str(&pptx_paragraph_xml(
                        "Supporting Details",
                        1800,
                        pal.muted,
                        false,
                        false,
                        "l",
                        "Helvetica Neue",
                        None,
                        8,
                    ));
                } else {
                    for item in &slide.right_bullets {
                        right_paras.push_str(&pptx_paragraph_xml(
                            item,
                            1700,
                            pal.fg,
                            false,
                            false,
                            "l",
                            "Helvetica Neue",
                            Some(pal.accent_alt),
                            10,
                        ));
                    }
                }
                shapes.push_str(&pptx_rect_shape_xml(
                    next_id,
                    "RightColumnCard",
                    6_278_880,
                    col_y,
                    col_w,
                    col_h,
                    "roundRect",
                    Some(pal.card),
                    Some(pal.border),
                    &right_paras,
                    "t",
                ));
                next_id += 1;
            }
            "metrics" => {
                let count = slide.metrics.len().clamp(1, 4);
                let gap = 243_840_i64;
                let total_w = 10_728_960_i64;
                let card_w = (total_w - gap * ((count as i64) - 1)) / (count as i64);
                let card_y = 1_760_000_i64;
                let card_h = if slide.bullets.is_empty() {
                    3_200_000_i64
                } else {
                    2_286_000_i64
                };

                for (i, metric) in slide.metrics.iter().take(4).enumerate() {
                    let x = 731_520 + (i as i64) * (card_w + gap);
                    let delta_trimmed = metric.delta.trim();
                    let delta_color = if delta_trimmed.starts_with('-') {
                        pal.negative
                    } else if delta_trimmed.starts_with('+') {
                        pal.positive
                    } else {
                        pal.accent_alt
                    };

                    let mut m_paras = pptx_paragraph_xml(
                        &metric.label,
                        1300,
                        pal.muted,
                        true,
                        false,
                        "l",
                        "Helvetica Neue",
                        None,
                        10,
                    );
                    m_paras.push_str(&pptx_paragraph_xml(
                        &metric.value,
                        3400,
                        pal.fg,
                        true,
                        false,
                        "l",
                        "Helvetica Neue",
                        None,
                        8,
                    ));
                    if !delta_trimmed.is_empty() {
                        m_paras.push_str(&pptx_paragraph_xml(
                            delta_trimmed,
                            1400,
                            delta_color,
                            true,
                            false,
                            "l",
                            "Helvetica Neue",
                            None,
                            0,
                        ));
                    }

                    shapes.push_str(&pptx_rect_shape_xml(
                        next_id,
                        &format!("MetricCard{}", i + 1),
                        x,
                        card_y,
                        card_w,
                        card_h,
                        "roundRect",
                        Some(pal.card),
                        Some(pal.border),
                        &m_paras,
                        "ctr",
                    ));
                    next_id += 1;
                }

                if !slide.bullets.is_empty() {
                    let mut b_paras = String::new();
                    for b in &slide.bullets {
                        b_paras.push_str(&pptx_paragraph_xml(
                            b,
                            1600,
                            pal.fg,
                            false,
                            false,
                            "l",
                            "Helvetica Neue",
                            Some(pal.accent),
                            8,
                        ));
                    }
                    shapes.push_str(&pptx_rect_shape_xml(
                        next_id,
                        "MetricsSummary",
                        731_520,
                        4_300_000,
                        10_728_960,
                        1_850_000,
                        "roundRect",
                        Some(pal.card),
                        Some(pal.border),
                        &b_paras,
                        "t",
                    ));
                    next_id += 1;
                }
            }
            "code" => {
                let code_raw = slide.code.as_deref().unwrap_or("// Code sample");
                let mut code_paras = String::new();
                for line in code_raw.lines().take(20) {
                    let display_line = if line.is_empty() { " " } else { line };
                    code_paras.push_str(&pptx_paragraph_xml(
                        display_line,
                        1250,
                        pal.code_fg,
                        false,
                        false,
                        "l",
                        "Menlo",
                        None,
                        2,
                    ));
                }
                if code_paras.is_empty() {
                    code_paras.push_str(&pptx_paragraph_xml(
                        " ",
                        1250,
                        pal.code_fg,
                        false,
                        false,
                        "l",
                        "Menlo",
                        None,
                        0,
                    ));
                }

                let (code_x, code_w) = if slide.bullets.is_empty() {
                    (731_520, 10_728_960)
                } else {
                    let mut note_paras = String::new();
                    for b in &slide.bullets {
                        note_paras.push_str(&pptx_paragraph_xml(
                            b,
                            1550,
                            pal.fg,
                            false,
                            false,
                            "l",
                            "Helvetica Neue",
                            Some(pal.accent),
                            10,
                        ));
                    }
                    shapes.push_str(&pptx_rect_shape_xml(
                        next_id,
                        "CodeAnnotations",
                        731_520,
                        1_737_360,
                        3_800_000,
                        4_389_120,
                        "roundRect",
                        Some(pal.card),
                        Some(pal.border),
                        &note_paras,
                        "t",
                    ));
                    next_id += 1;
                    (4_750_000, 6_710_480)
                };

                shapes.push_str(&pptx_rect_shape_xml(
                    next_id,
                    "CodeTerminalBox",
                    code_x,
                    1_737_360,
                    code_w,
                    4_389_120,
                    "roundRect",
                    Some(pal.code_bg),
                    Some(pal.accent),
                    &code_paras,
                    "t",
                ));
                next_id += 1;
            }
            "quote" => {
                let quote_body = if !slide.bullets.is_empty() {
                    slide.bullets[0].as_str()
                } else if !slide.subtitle.is_empty() {
                    slide.subtitle.as_str()
                } else {
                    slide.title.as_str()
                };
                let attribution = if slide.bullets.len() > 1 {
                    slide.bullets[1].as_str()
                } else if !slide.subtitle.is_empty() && !slide.bullets.is_empty() {
                    slide.subtitle.as_str()
                } else {
                    author
                };

                let mut q_paras = pptx_paragraph_xml(
                    &format!("\"{}\"", quote_body),
                    2400,
                    pal.fg,
                    false,
                    true,
                    "ctr",
                    "Helvetica Neue",
                    None,
                    16,
                );
                if !attribution.is_empty() {
                    q_paras.push_str(&pptx_paragraph_xml(
                        &format!("— {}", attribution),
                        1500,
                        pal.accent_alt,
                        true,
                        false,
                        "ctr",
                        "Helvetica Neue",
                        None,
                        0,
                    ));
                }
                shapes.push_str(&pptx_rect_shape_xml(
                    next_id,
                    "QuoteCard",
                    1_097_280,
                    1_828_800,
                    9_997_440,
                    3_840_480,
                    "roundRect",
                    Some(pal.card),
                    Some(pal.accent),
                    &q_paras,
                    "ctr",
                ));
                next_id += 1;
            }
            _ => {
                // Default: "bullets"
                let mut b_paras = String::new();
                if slide.bullets.is_empty() {
                    let fallback = if !slide.subtitle.is_empty() {
                        slide.subtitle.as_str()
                    } else {
                        slide.title.as_str()
                    };
                    b_paras.push_str(&pptx_paragraph_xml(
                        fallback,
                        1800,
                        pal.fg,
                        false,
                        false,
                        "l",
                        "Helvetica Neue",
                        Some(pal.accent),
                        10,
                    ));
                } else {
                    for b in &slide.bullets {
                        b_paras.push_str(&pptx_paragraph_xml(
                            b,
                            1800,
                            pal.fg,
                            false,
                            false,
                            "l",
                            "Helvetica Neue",
                            Some(pal.accent),
                            12,
                        ));
                    }
                }
                shapes.push_str(&pptx_rect_shape_xml(
                    next_id,
                    "BulletsCard",
                    731_520,
                    1_737_360,
                    10_728_960,
                    4_389_120,
                    "roundRect",
                    Some(pal.card),
                    Some(pal.border),
                    &b_paras,
                    "t",
                ));
                next_id += 1;
            }
        }
    }

    // Footer slide number indicator
    let footer_p = pptx_paragraph_xml(
        &format!(
            "{}  •  {} / {}",
            if deck_title.is_empty() {
                "Rursor Studio"
            } else {
                deck_title
            },
            slide_idx_1_based,
            total_slides
        ),
        1050,
        pal.muted,
        false,
        false,
        "r",
        "Helvetica Neue",
        None,
        0,
    );
    shapes.push_str(&pptx_rect_shape_xml(
        next_id,
        "SlideFooter",
        731_520,
        6_309_360,
        10_728_960,
        365_760,
        "rect",
        None,
        None,
        &footer_p,
        "ctr",
    ));

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sld xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld>\
<p:bg><p:bgPr><a:solidFill><a:srgbClr val=\"{bg}\"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>\
<p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
{shapes}\
</p:spTree>\
</p:cSld>\
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>\
</p:sld>",
        bg = pal.bg,
        shapes = shapes
    )
}

fn build_pptx_theme_xml(pal: &DeckPalette) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Rursor {name}\">\
<a:themeElements>\
<a:clrScheme name=\"Rursor {name}\">\
<a:dk1><a:srgbClr val=\"{bg}\"/></a:dk1>\
<a:lt1><a:srgbClr val=\"{fg}\"/></a:lt1>\
<a:dk2><a:srgbClr val=\"{card}\"/></a:dk2>\
<a:lt2><a:srgbClr val=\"{muted}\"/></a:lt2>\
<a:accent1><a:srgbClr val=\"{accent}\"/></a:accent1>\
<a:accent2><a:srgbClr val=\"{accent_alt}\"/></a:accent2>\
<a:accent3><a:srgbClr val=\"{positive}\"/></a:accent3>\
<a:accent4><a:srgbClr val=\"{negative}\"/></a:accent4>\
<a:accent5><a:srgbClr val=\"{border}\"/></a:accent5>\
<a:accent6><a:srgbClr val=\"{accent}\"/></a:accent6>\
<a:hlink><a:srgbClr val=\"{accent}\"/></a:hlink>\
<a:folHlink><a:srgbClr val=\"{accent_alt}\"/></a:folHlink>\
</a:clrScheme>\
<a:fontScheme name=\"Rursor Sans\">\
<a:majorFont><a:latin typeface=\"Helvetica Neue\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont>\
<a:minorFont><a:latin typeface=\"Helvetica Neue\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont>\
</a:fontScheme>\
<a:fmtScheme name=\"Rursor Formats\">\
<a:fillStyleLst>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
</a:fillStyleLst>\
<a:lnStyleLst>\
<a:ln w=\"9525\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>\
<a:ln w=\"19050\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>\
<a:ln w=\"25400\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:ln>\
</a:lnStyleLst>\
<a:effectStyleLst>\
<a:effectStyle><a:effectLst/></a:effectStyle>\
<a:effectStyle><a:effectLst/></a:effectStyle>\
<a:effectStyle><a:effectLst/></a:effectStyle>\
</a:effectStyleLst>\
<a:bgFillStyleLst>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
</a:bgFillStyleLst>\
</a:fmtScheme>\
</a:themeElements>\
<a:objectDefaults/>\
<a:extraClrSchemeLst/>\
</a:theme>",
        name = pal.name,
        bg = pal.bg,
        fg = pal.fg,
        card = pal.card,
        muted = pal.muted,
        accent = pal.accent,
        accent_alt = pal.accent_alt,
        positive = pal.positive,
        negative = pal.negative,
        border = pal.border
    )
}

fn build_companion_html_deck(
    spec: &PresentationSpec,
    slides: &[SlideSpec],
    pal: &DeckPalette,
) -> String {
    let deck_title = if spec.title.trim().is_empty() {
        "Rursor Presentation"
    } else {
        spec.title.trim()
    };

    let mut slides_markup = String::with_capacity(slides.len() * 1024);
    let mut notes_json_items = Vec::with_capacity(slides.len());

    for (idx, slide) in slides.iter().enumerate() {
        let layout = slide.layout.trim().to_ascii_lowercase();
        let active_cls = if idx == 0 { " active" } else { "" };
        notes_json_items.push(
            serde_json::to_string(slide.speaker_notes.as_deref().unwrap_or(""))
                .unwrap_or_else(|_| "\"\"".to_string()),
        );

        let body_html = match layout.as_str() {
            "title" => {
                let mut bullets_html = String::new();
                if !slide.bullets.is_empty() {
                    bullets_html.push_str("<ul class=\"hero-bullets\">");
                    for b in &slide.bullets {
                        bullets_html.push_str(&format!("<li>{}</li>", html_escape(b)));
                    }
                    bullets_html.push_str("</ul>");
                }
                let author_html = if !spec.author.trim().is_empty() {
                    format!(
                        "<div class=\"author-pill\">Prepared by {}</div>",
                        html_escape(spec.author.trim())
                    )
                } else {
                    String::new()
                };
                format!(
                    "<div class=\"hero-card\">\
<div class=\"eyebrow\">{theme} DECK</div>\
<h1>{title}</h1>\
{sub}\
{bullets}\
{author}\
</div>",
                    theme = html_escape(&pal.name.to_uppercase()),
                    title = html_escape(if slide.title.is_empty() {
                        deck_title
                    } else {
                        &slide.title
                    }),
                    sub = if slide.subtitle.is_empty() {
                        String::new()
                    } else {
                        format!("<p class=\"hero-sub\">{}</p>", html_escape(&slide.subtitle))
                    },
                    bullets = bullets_html,
                    author = author_html
                )
            }
            "two_column" => {
                let mut left_items = String::new();
                for b in &slide.bullets {
                    left_items.push_str(&format!("<li>{}</li>", html_escape(b)));
                }
                let mut right_items = String::new();
                for b in &slide.right_bullets {
                    right_items.push_str(&format!("<li>{}</li>", html_escape(b)));
                }
                format!(
                    "<header class=\"slide-hdr\"><h2>{title}</h2>{sub}</header>\
<div class=\"two-col\">\
<div class=\"card\"><ul>{left}</ul></div>\
<div class=\"card alt-bullets\"><ul>{right}</ul></div>\
</div>",
                    title = html_escape(&slide.title),
                    sub = if slide.subtitle.is_empty() {
                        String::new()
                    } else {
                        format!("<p>{}</p>", html_escape(&slide.subtitle))
                    },
                    left = left_items,
                    right = right_items
                )
            }
            "metrics" => {
                let mut cards = String::new();
                for m in &slide.metrics {
                    let d = m.delta.trim();
                    let delta_cls = if d.starts_with('-') {
                        "neg"
                    } else if d.starts_with('+') {
                        "pos"
                    } else {
                        "neu"
                    };
                    cards.push_str(&format!(
                        "<div class=\"kpi-card\">\
<div class=\"kpi-label\">{label}</div>\
<div class=\"kpi-value\">{value}</div>\
{delta}\
</div>",
                        label = html_escape(&m.label),
                        value = html_escape(&m.value),
                        delta = if d.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "<div class=\"kpi-delta {}\">{}</div>",
                                delta_cls,
                                html_escape(d)
                            )
                        }
                    ));
                }
                let mut extra = String::new();
                if !slide.bullets.is_empty() {
                    extra.push_str("<div class=\"card metrics-notes\"><ul>");
                    for b in &slide.bullets {
                        extra.push_str(&format!("<li>{}</li>", html_escape(b)));
                    }
                    extra.push_str("</ul></div>");
                }
                format!(
                    "<header class=\"slide-hdr\"><h2>{title}</h2>{sub}</header>\
<div class=\"kpi-grid\">{cards}</div>\
{extra}",
                    title = html_escape(&slide.title),
                    sub = if slide.subtitle.is_empty() {
                        String::new()
                    } else {
                        format!("<p>{}</p>", html_escape(&slide.subtitle))
                    },
                    cards = cards,
                    extra = extra
                )
            }
            "code" => {
                let code_str = slide.code.as_deref().unwrap_or("");
                let mut numbered = String::new();
                for (ln, line) in code_str.lines().enumerate() {
                    numbered.push_str(&format!(
                        "<div class=\"code-line\"><span class=\"ln\">{:>2}</span><span>{}</span></div>",
                        ln + 1,
                        html_escape(line)
                    ));
                }
                let side_notes = if slide.bullets.is_empty() {
                    String::new()
                } else {
                    let mut ul = String::from("<div class=\"card\"><ul>");
                    for b in &slide.bullets {
                        ul.push_str(&format!("<li>{}</li>", html_escape(b)));
                    }
                    ul.push_str("</ul></div>");
                    ul
                };
                let grid_cls = if side_notes.is_empty() {
                    "code-single"
                } else {
                    "code-split"
                };
                format!(
                    "<header class=\"slide-hdr\"><h2>{title}</h2>{sub}</header>\
<div class=\"{grid_cls}\">{side_notes}\
<div class=\"code-card\"><div class=\"code-dots\"><i></i><i></i><i></i></div><pre><code>{numbered}</code></pre></div>\
</div>",
                    title = html_escape(&slide.title),
                    sub = if slide.subtitle.is_empty() {
                        String::new()
                    } else {
                        format!("<p>{}</p>", html_escape(&slide.subtitle))
                    },
                    grid_cls = grid_cls,
                    side_notes = side_notes,
                    numbered = numbered
                )
            }
            "quote" => {
                let quote_txt = if !slide.bullets.is_empty() {
                    slide.bullets[0].as_str()
                } else if !slide.subtitle.is_empty() {
                    slide.subtitle.as_str()
                } else {
                    slide.title.as_str()
                };
                let attr_txt = if slide.bullets.len() > 1 {
                    slide.bullets[1].as_str()
                } else {
                    spec.author.as_str()
                };
                format!(
                    "<header class=\"slide-hdr\"><h2>{title}</h2></header>\
<blockquote class=\"quote-card\">\
<p>&ldquo;{quote}&rdquo;</p>\
{attr}\
</blockquote>",
                    title = html_escape(&slide.title),
                    quote = html_escape(quote_txt),
                    attr = if attr_txt.trim().is_empty() {
                        String::new()
                    } else {
                        format!("<footer>&mdash; {}</footer>", html_escape(attr_txt.trim()))
                    }
                )
            }
            _ => {
                let mut items = String::new();
                for b in &slide.bullets {
                    items.push_str(&format!("<li>{}</li>", html_escape(b)));
                }
                format!(
                    "<header class=\"slide-hdr\"><h2>{title}</h2>{sub}</header>\
<div class=\"card bullets-card\"><ul>{items}</ul></div>",
                    title = html_escape(&slide.title),
                    sub = if slide.subtitle.is_empty() {
                        String::new()
                    } else {
                        format!("<p>{}</p>", html_escape(&slide.subtitle))
                    },
                    items = items
                )
            }
        };

        slides_markup.push_str(&format!(
            "<section class=\"slide{active_cls}\" data-index=\"{idx}\">{body_html}</section>",
            active_cls = active_cls,
            idx = idx,
            body_html = body_html
        ));
    }

    format!(
        "<!DOCTYPE html>\n\
<html lang=\"en\">\n\
<head>\n\
<meta charset=\"UTF-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\
<title>{title}</title>\n\
<style>\n\
:root {{\n\
  --bg: #{bg};\n\
  --card: #{card};\n\
  --border: #{border};\n\
  --fg: #{fg};\n\
  --muted: #{muted};\n\
  --accent: #{accent};\n\
  --accent-alt: #{accent_alt};\n\
  --code-bg: #{code_bg};\n\
  --code-fg: #{code_fg};\n\
  --pos: #{pos};\n\
  --neg: #{neg};\n\
}}\n\
* {{ box-sizing: border-box; }}\n\
html, body {{ margin: 0; padding: 0; width: 100%; height: 100%; background: #06080d; color: var(--fg); font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Display', 'Inter', sans-serif; overflow: hidden; }}\n\
.deck-shell {{ display: flex; flex-direction: column; width: 100%; height: 100%; }}\n\
.stage-wrap {{ flex: 1; display: flex; align-items: center; justify-content: center; padding: 18px; overflow: hidden; }}\n\
.viewport {{ position: relative; width: min(100%, calc((100vh - 100px) * 16 / 9)); aspect-ratio: 16 / 9; background: var(--bg); border: 1px solid var(--border); border-radius: 14px; box-shadow: 0 24px 64px rgba(0,0,0,0.55); overflow: hidden; display: flex; flex-direction: column; }}\n\
.top-accent {{ height: 5px; width: 100%; background: linear-gradient(90deg, var(--accent), var(--accent-alt)); flex-shrink: 0; }}\n\
.slide {{ display: none; flex: 1; padding: 3.6% 5%; flex-direction: column; gap: 18px; overflow: auto; }}\n\
.slide.active {{ display: flex; animation: fadeSlide 180ms ease-out; }}\n\
@keyframes fadeSlide {{ from {{ opacity: 0; transform: translateY(6px); }} to {{ opacity: 1; transform: translateY(0); }} }}\n\
.hero-card {{ margin: auto 0; padding: 36px 42px; background: var(--card); border: 1px solid var(--border); border-radius: 14px; }}\n\
.eyebrow {{ font-size: 12px; font-weight: 700; letter-spacing: 0.14em; color: var(--accent); margin-bottom: 10px; }}\n\
h1 {{ margin: 0 0 12px; font-size: clamp(26px, 3.6vw, 48px); line-height: 1.12; }}\n\
.hero-sub {{ margin: 0 0 16px; font-size: clamp(15px, 1.8vw, 22px); color: var(--muted); }}\n\
.author-pill {{ display: inline-block; margin-top: 14px; padding: 6px 12px; border-radius: 999px; border: 1px solid var(--border); color: var(--accent-alt); font-size: 13px; font-weight: 600; }}\n\
.slide-hdr h2 {{ margin: 0; font-size: clamp(20px, 2.5vw, 34px); }}\n\
.slide-hdr p {{ margin: 6px 0 0; color: var(--muted); font-size: clamp(13px, 1.3vw, 17px); }}\n\
.card {{ background: var(--card); border: 1px solid var(--border); border-radius: 12px; padding: 22px 26px; flex: 1; overflow: auto; }}\n\
ul {{ margin: 0; padding-left: 20px; display: flex; flex-direction: column; gap: 11px; font-size: clamp(14px, 1.45vw, 19px); line-height: 1.45; }}\n\
li::marker {{ color: var(--accent); }}\n\
.alt-bullets li::marker {{ color: var(--accent-alt); }}\n\
.two-col {{ display: grid; grid-template-columns: 1fr 1fr; gap: 18px; flex: 1; min-height: 0; }}\n\
.kpi-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 16px; }}\n\
.kpi-card {{ background: var(--card); border: 1px solid var(--border); border-radius: 12px; padding: 20px; display: flex; flex-direction: column; gap: 8px; }}\n\
.kpi-label {{ font-size: 12px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); font-weight: 600; }}\n\
.kpi-value {{ font-size: clamp(24px, 2.8vw, 40px); font-weight: 800; }}\n\
.kpi-delta {{ font-size: 13px; font-weight: 700; }}\n\
.kpi-delta.pos {{ color: var(--pos); }}\n\
.kpi-delta.neg {{ color: var(--neg); }}\n\
.kpi-delta.neu {{ color: var(--accent-alt); }}\n\
.code-single {{ display: flex; flex: 1; min-height: 0; }}\n\
.code-split {{ display: grid; grid-template-columns: 38% 1fr; gap: 18px; flex: 1; min-height: 0; }}\n\
.code-card {{ background: var(--code-bg); color: var(--code-fg); border: 1px solid var(--accent); border-radius: 12px; padding: 14px 18px; flex: 1; overflow: auto; display: flex; flex-direction: column; }}\n\
.code-dots {{ display: flex; gap: 6px; margin-bottom: 10px; }}\n\
.code-dots i {{ width: 10px; height: 10px; border-radius: 50%; background: #ff5f56; display: inline-block; }}\n\
.code-dots i:nth-child(2) {{ background: #ffbd2e; }}\n\
.code-dots i:nth-child(3) {{ background: #27c93f; }}\n\
pre {{ margin: 0; font-family: 'SF Mono', Menlo, Consolas, monospace; font-size: clamp(11px, 1.15vw, 15px); line-height: 1.5; }}\n\
.code-line {{ display: flex; gap: 14px; white-space: pre; }}\n\
.ln {{ color: var(--muted); opacity: 0.6; user-select: none; }}\n\
.quote-card {{ margin: auto 0; padding: 32px 38px; background: var(--card); border-left: 4px solid var(--accent); border-radius: 12px; font-size: clamp(18px, 2.1vw, 28px); font-style: italic; line-height: 1.45; }}\n\
.quote-card footer {{ margin-top: 14px; font-size: 15px; font-style: normal; color: var(--accent-alt); font-weight: 600; }}\n\
.progress-track {{ height: 3px; background: rgba(127,127,127,0.2); width: 100%; }}\n\
.progress-fill {{ height: 100%; background: var(--accent); width: 0%; transition: width 160ms ease; }}\n\
.notes-drawer {{ display: none; background: #0f131c; color: #d8dee9; border-top: 1px solid #252e42; padding: 10px 18px; font-size: 13px; max-height: 130px; overflow-y: auto; }}\n\
.notes-drawer.open {{ display: block; }}\n\
.toolbar {{ display: flex; align-items: center; justify-content: space-between; padding: 8px 16px; background: #0b0d12; border-top: 1px solid #1e2636; color: #cbd5e1; font-size: 12px; }}\n\
.toolbar-btns {{ display: flex; align-items: center; gap: 8px; }}\n\
button {{ background: #161c28; color: #f1f5f9; border: 1px solid #2a344a; border-radius: 6px; padding: 5px 11px; font-size: 12px; cursor: pointer; }}\n\
button:hover {{ border-color: #7c5cff; }}\n\
</style>\n\
</head>\n\
<body>\n\
<div class=\"deck-shell\">\n\
  <div class=\"stage-wrap\">\n\
    <div class=\"viewport\" id=\"viewport\">\n\
      <div class=\"top-accent\"></div>\n\
      {slides_markup}\n\
      <div class=\"progress-track\"><div class=\"progress-fill\" id=\"progressFill\"></div></div>\n\
    </div>\n\
  </div>\n\
  <div class=\"notes-drawer\" id=\"notesDrawer\"></div>\n\
  <div class=\"toolbar\">\n\
    <div><strong>{title}</strong></div>\n\
    <div class=\"toolbar-btns\">\n\
      <button id=\"prevBtn\" title=\"Previous Slide (ArrowLeft)\">&larr; Prev</button>\n\
      <span id=\"slideCounter\">1 / {total}</span>\n\
      <button id=\"nextBtn\" title=\"Next Slide (ArrowRight / Space)\">Next &rarr;</button>\n\
      <button id=\"notesBtn\" title=\"Toggle Speaker Notes (N)\">Notes (N)</button>\n\
      <button id=\"fsBtn\" title=\"Toggle Fullscreen (F)\">Fullscreen (F)</button>\n\
    </div>\n\
  </div>\n\
</div>\n\
<script>\n\
(function() {{\n\
  const slides = Array.from(document.querySelectorAll('.slide'));\n\
  const notes = [{notes_json}];\n\
  const counterEl = document.getElementById('slideCounter');\n\
  const progressEl = document.getElementById('progressFill');\n\
  const notesEl = document.getElementById('notesDrawer');\n\
  let idx = 0;\n\
  function render() {{\n\
    slides.forEach((el, i) => el.classList.toggle('active', i === idx));\n\
    counterEl.textContent = (idx + 1) + ' / ' + slides.length;\n\
    progressEl.style.width = (((idx + 1) / Math.max(1, slides.length)) * 100).toFixed(1) + '%';\n\
    const noteText = notes[idx] || 'No speaker notes for this slide.';\n\
    notesEl.textContent = 'Speaker Notes: ' + noteText;\n\
  }}\n\
  function go(delta) {{\n\
    idx = Math.max(0, Math.min(slides.length - 1, idx + delta));\n\
    render();\n\
  }}\n\
  function toggleNotes() {{ notesEl.classList.toggle('open'); }}\n\
  function toggleFs() {{\n\
    if (!document.fullscreenElement) {{ document.documentElement.requestFullscreen?.(); }}\n\
    else {{ document.exitFullscreen?.(); }}\n\
  }}\n\
  document.getElementById('prevBtn').addEventListener('click', () => go(-1));\n\
  document.getElementById('nextBtn').addEventListener('click', () => go(1));\n\
  document.getElementById('notesBtn').addEventListener('click', toggleNotes);\n\
  document.getElementById('fsBtn').addEventListener('click', toggleFs);\n\
  window.addEventListener('keydown', (e) => {{\n\
    if (e.key === 'ArrowRight' || e.key === ' ' || e.key === 'PageDown') {{ e.preventDefault(); go(1); }}\n\
    else if (e.key === 'ArrowLeft' || e.key === 'PageUp') {{ e.preventDefault(); go(-1); }}\n\
    else if (e.key === 'Home') {{ idx = 0; render(); }}\n\
    else if (e.key === 'End') {{ idx = slides.length - 1; render(); }}\n\
    else if (e.key === 'n' || e.key === 'N') {{ toggleNotes(); }}\n\
    else if (e.key === 'f' || e.key === 'F') {{ toggleFs(); }}\n\
  }});\n\
  render();\n\
}})();\n\
</script>\n\
</body>\n\
</html>\n",
        title = html_escape(deck_title),
        bg = pal.bg,
        card = pal.card,
        border = pal.border,
        fg = pal.fg,
        muted = pal.muted,
        accent = pal.accent,
        accent_alt = pal.accent_alt,
        code_bg = pal.code_bg,
        code_fg = pal.code_fg,
        pos = pal.positive,
        neg = pal.negative,
        slides_markup = slides_markup,
        total = slides.len(),
        notes_json = notes_json_items.join(",")
    )
}

/// Generates a genuine OpenXML `.pptx` presentation (`16:9` widescreen `12192000 x 6858000` EMUs)
/// alongside a synchronized self-contained interactive `<stem>.deck.html` HTML5 slide deck.
pub fn create_presentation(
    workspace_root: &Path,
    spec: &PresentationSpec,
) -> Result<PresentationArtifact, String> {
    let normalized_out = if spec.output_path.trim().to_ascii_lowercase().ends_with(".pptx") {
        spec.output_path.trim().to_string()
    } else {
        format!("{}.pptx", spec.output_path.trim())
    };

    let (pptx_abs, rel_pptx) = resolve_confined_output_path(workspace_root, &normalized_out)?;
    let stem = pptx_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "invalid presentation file stem".to_string())?;
    let html_file_name = format!("{}.deck.html", stem);
    let html_abs = pptx_abs
        .parent()
        .ok_or_else(|| "missing presentation parent directory".to_string())?
        .join(&html_file_name);
    let rel_html = match Path::new(&rel_pptx).parent() {
        Some(p) if !p.as_os_str().is_empty() => {
            format!("{}/{}", p.to_string_lossy().replace('\\', "/"), html_file_name)
        }
        _ => html_file_name,
    };

    let pal = resolve_deck_palette(&spec.theme);
    let slides: Vec<SlideSpec> = if spec.slides.is_empty() {
        vec![SlideSpec {
            layout: "title".to_string(),
            title: if spec.title.is_empty() {
                "Untitled Presentation".to_string()
            } else {
                spec.title.clone()
            },
            subtitle: spec.subtitle.clone(),
            ..SlideSpec::default()
        }]
    } else {
        spec.slides.clone()
    };

    let slide_count = slides.len();
    let mut entries: Vec<(String, String)> = Vec::with_capacity(12 + slide_count * 4);

    // 1. [Content_Types].xml
    let mut overrides = String::from(
        "<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/>\
<Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/>\
<Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>",
    );
    for i in 1..=slide_count {
        overrides.push_str(&format!(
            "<Override PartName=\"/ppt/slides/slide{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>",
            i
        ));
        if slides[i - 1]
            .speaker_notes
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            overrides.push_str(&format!(
                "<Override PartName=\"/ppt/notesSlides/notesSlide{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml\"/>",
                i
            ));
        }
    }
    entries.push((
        "[Content_Types].xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
{overrides}\
</Types>"
        ),
    ));

    // 2. _rels/.rels
    entries.push((
        "_rels/.rels".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"ppt/presentation.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>\
</Relationships>"
            .to_string(),
    ));

    // 3. docProps/core.xml & docProps/app.xml
    entries.push((
        "docProps/core.xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:dcterms=\"http://purl.org/dc/terms/\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
<dc:title>{title}</dc:title>\
<dc:subject>{subtitle}</dc:subject>\
<dc:creator>{author}</dc:creator>\
</cp:coreProperties>",
            title = xml_escape(&spec.title),
            subtitle = xml_escape(&spec.subtitle),
            author = xml_escape(if spec.author.is_empty() {
                "Rursor Studio"
            } else {
                &spec.author
            }),
        ),
    ));
    entries.push((
        "docProps/app.xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\" \
xmlns:vt=\"http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes\">\
<Application>Rursor Media Studio</Application>\
<Slides>{}</Slides>\
<PresentationFormat>Widescreen</PresentationFormat>\
</Properties>",
            slide_count
        ),
    ));

    // 4. ppt/presentation.xml & ppt/_rels/presentation.xml.rels
    let mut sld_id_lst = String::new();
    let mut pres_rels = String::from(
        "<Relationship Id=\"rIdMaster1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>\
<Relationship Id=\"rIdTheme1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"theme/theme1.xml\"/>",
    );
    for i in 1..=slide_count {
        sld_id_lst.push_str(&format!(
            "<p:sldId id=\"{}\" r:id=\"rIdSlide{}\"/>",
            255 + i,
            i
        ));
        pres_rels.push_str(&format!(
            "<Relationship Id=\"rIdSlide{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{}.xml\"/>",
            i, i
        ));
    }

    entries.push((
        "ppt/presentation.xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:presentation xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" saveSubsetFonts=\"1\">\
<p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rIdMaster1\"/></p:sldMasterIdLst>\
<p:sldIdLst>{sld_id_lst}</p:sldIdLst>\
<p:sldSz cx=\"{cx}\" cy=\"{cy}\"/>\
<p:notesSz cx=\"6858000\" cy=\"9144000\"/>\
<p:defaultTextStyle><a:defPPr><a:defRPr lang=\"en-US\"/></a:defPPr></p:defaultTextStyle>\
</p:presentation>",
            sld_id_lst = sld_id_lst,
            cx = PPTX_SLIDE_WIDTH_EMU,
            cy = PPTX_SLIDE_HEIGHT_EMU
        ),
    ));
    entries.push((
        "ppt/_rels/presentation.xml.rels".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{pres_rels}</Relationships>"
        ),
    ));

    // 5. Slide Master, Slide Layout & Theme
    entries.push((
        "ppt/slideMasters/slideMaster1.xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldMaster xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld>\
<p:bg><p:bgPr><a:solidFill><a:srgbClr val=\"{bg}\"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>\
<p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
</p:spTree>\
</p:cSld>\
<p:clrMap bg1=\"dk1\" tx1=\"lt1\" bg2=\"dk2\" tx2=\"lt2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/>\
<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst>\
<p:txStyles>\
<p:titleStyle><a:lvl1pPr algn=\"l\"><a:defRPr sz=\"3600\" b=\"1\"/></a:lvl1pPr></p:titleStyle>\
<p:bodyStyle><a:lvl1pPr algn=\"l\"><a:defRPr sz=\"1800\"/></a:lvl1pPr></p:bodyStyle>\
<p:otherStyle><a:lvl1pPr algn=\"l\"><a:defRPr sz=\"1400\"/></a:lvl1pPr></p:otherStyle>\
</p:txStyles>\
</p:sldMaster>",
            bg = pal.bg
        ),
    ));
    entries.push((
        "ppt/slideMasters/_rels/slideMaster1.xml.rels".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"../theme/theme1.xml\"/>\
</Relationships>"
            .to_string(),
    ));
    entries.push((
        "ppt/slideLayouts/slideLayout1.xml".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldLayout xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" type=\"blank\" preserve=\"1\">\
<p:cSld name=\"Blank\">\
<p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
</p:spTree>\
</p:cSld>\
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>\
</p:sldLayout>"
            .to_string(),
    ));
    entries.push((
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/>\
</Relationships>"
            .to_string(),
    ));
    entries.push(("ppt/theme/theme1.xml".to_string(), build_pptx_theme_xml(&pal)));

    // 6. Individual Slides + Speaker Notes
    for (idx0, slide) in slides.iter().enumerate() {
        let i = idx0 + 1;
        let slide_xml = build_slide_xml(i, slide_count, &spec.title, &spec.author, slide, &pal);
        entries.push((format!("ppt/slides/slide{}.xml", i), slide_xml));

        let has_notes = slide
            .speaker_notes
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if has_notes {
            let notes_text = slide.speaker_notes.as_deref().unwrap_or("");
            entries.push((
                format!("ppt/slides/_rels/slide{}.xml.rels", i),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide\" Target=\"../notesSlides/notesSlide{}.xml\"/>\
</Relationships>",
                    i
                ),
            ));
            let notes_p = pptx_paragraph_xml(
                notes_text,
                1200,
                "1E293B",
                false,
                false,
                "l",
                "Helvetica Neue",
                None,
                6,
            );
            let notes_shape = pptx_rect_shape_xml(
                2,
                "SpeakerNotesBody",
                685_800,
                685_800,
                5_486_400,
                7_772_400,
                "rect",
                None,
                None,
                &notes_p,
                "t",
            );
            entries.push((
                format!("ppt/notesSlides/notesSlide{}.xml", i),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:notes xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
{notes_shape}\
</p:spTree></p:cSld>\
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>\
</p:notes>"
                ),
            ));
            entries.push((
                format!("ppt/notesSlides/_rels/notesSlide{}.xml.rels", i),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"../slides/slide{}.xml\"/>\
</Relationships>",
                    i
                ),
            ));
        } else {
            entries.push((
                format!("ppt/slides/_rels/slide{}.xml.rels", i),
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
</Relationships>"
                    .to_string(),
            ));
        }
    }

    let bytes = package_openxml_zip(&entries, &pptx_abs)?;

    let html_deck = build_companion_html_deck(spec, &slides, &pal);
    fs::write(&html_abs, html_deck.as_bytes()).map_err(|e| {
        format!(
            "failed to write companion HTML5 slide deck '{}': {}",
            html_abs.display(),
            e
        )
    })?;

    Ok(PresentationArtifact {
        pptx_path: pptx_abs.to_string_lossy().to_string(),
        html_deck_path: html_abs.to_string_lossy().to_string(),
        relative_pptx_path: rel_pptx,
        relative_html_deck_path: rel_html,
        slide_count,
        bytes,
    })
}


// ============================================================================
// 2. Pure-Rust Anti-Aliased Rasterizer, PNG/BMP Codec & Video Studio
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct Rgba(u8, u8, u8, u8);

fn parse_hex_color(raw: &str, fallback: Rgba) -> Rgba {
    let hex = raw.trim().trim_start_matches('#');
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok();
            let g = u8::from_str_radix(&hex[2..4], 16).ok();
            let b = u8::from_str_radix(&hex[4..6], 16).ok();
            match (r, g, b) {
                (Some(r), Some(g), Some(b)) => Rgba(r, g, b, 255),
                _ => fallback,
            }
        }
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok().map(|v| v * 17);
            let g = u8::from_str_radix(&hex[1..2], 16).ok().map(|v| v * 17);
            let b = u8::from_str_radix(&hex[2..3], 16).ok().map(|v| v * 17);
            match (r, g, b) {
                (Some(r), Some(g), Some(b)) => Rgba(r, g, b, 255),
                _ => fallback,
            }
        }
        _ => fallback,
    }
}

struct RgbaCanvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RgbaCanvas {
    fn new(width: u32, height: u32, bg: Rgba) -> Self {
        let len = (width as usize) * (height as usize) * 4;
        let mut pixels = vec![0u8; len];
        for px in pixels.chunks_exact_mut(4) {
            px[0] = bg.0;
            px[1] = bg.1;
            px[2] = bg.2;
            px[3] = bg.3;
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    #[inline]
    fn blend_pixel(&mut self, x: i32, y: i32, src: Rgba) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 || src.3 == 0 {
            return;
        }
        let idx = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        if src.3 == 255 {
            self.pixels[idx] = src.0;
            self.pixels[idx + 1] = src.1;
            self.pixels[idx + 2] = src.2;
            self.pixels[idx + 3] = 255;
            return;
        }
        let a = src.3 as u32;
        let inv_a = 255 - a;
        self.pixels[idx] = (((src.0 as u32) * a + (self.pixels[idx] as u32) * inv_a) / 255) as u8;
        self.pixels[idx + 1] =
            (((src.1 as u32) * a + (self.pixels[idx + 1] as u32) * inv_a) / 255) as u8;
        self.pixels[idx + 2] =
            (((src.2 as u32) * a + (self.pixels[idx + 2] as u32) * inv_a) / 255) as u8;
        self.pixels[idx + 3] = 255;
    }

    fn fill_obsidian_backdrop(&mut self, accent: Rgba) {
        let w = self.width.max(1) as f32;
        let h = self.height.max(1) as f32;
        let glow_cx = w * 0.82;
        let glow_cy = h * 0.18;
        let glow_r = w.max(h) * 0.58;

        for y in 0..self.height {
            let ty = (y as f32) / h;
            let base_r = 11.0 + ty * 7.0;
            let base_g = 13.0 + ty * 9.0;
            let base_b = 18.0 + ty * 15.0;
            for x in 0..self.width {
                let dx = (x as f32) - glow_cx;
                let dy = (y as f32) - glow_cy;
                let dist = (dx * dx + dy * dy).sqrt();
                let glow = (1.0 - (dist / glow_r)).clamp(0.0, 1.0);
                let g2 = glow * glow * 0.22;
                let idx = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
                self.pixels[idx] = (base_r + (accent.0 as f32) * g2).clamp(0.0, 255.0) as u8;
                self.pixels[idx + 1] = (base_g + (accent.1 as f32) * g2).clamp(0.0, 255.0) as u8;
                self.pixels[idx + 2] = (base_b + (accent.2 as f32) * g2).clamp(0.0, 255.0) as u8;
                self.pixels[idx + 3] = 255;
            }
        }
    }

    /// Signed-distance-field anti-aliased rounded rectangle fill & optional border.
    #[allow(clippy::too_many_arguments)]
    fn draw_round_rect_aa(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        fill: Option<Rgba>,
        stroke: Option<(Rgba, f32)>,
    ) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = radius.clamp(0.0, (w * 0.5).min(h * 0.5));
        let half_w = w * 0.5;
        let half_h = h * 0.5;
        let cx = x + half_w;
        let cy = y + half_h;

        let min_x = (x - 2.0).floor().max(0.0) as i32;
        let max_x = (x + w + 2.0).ceil().min(self.width as f32) as i32;
        let min_y = (y - 2.0).floor().max(0.0) as i32;
        let max_y = (y + h + 2.0).ceil().min(self.height as f32) as i32;

        for py in min_y..max_y {
            let dy = ((py as f32 + 0.5) - cy).abs() - (half_h - r);
            for px in min_x..max_x {
                let dx = ((px as f32 + 0.5) - cx).abs() - (half_w - r);
                let out_dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
                let in_dist = dx.max(dy).min(0.0);
                let sdf = out_dist + in_dist - r;

                if let Some(fc) = fill {
                    let coverage = (0.5 - sdf).clamp(0.0, 1.0);
                    if coverage > 0.0 {
                        let a = ((fc.3 as f32) * coverage) as u8;
                        self.blend_pixel(px, py, Rgba(fc.0, fc.1, fc.2, a));
                    }
                }

                if let Some((sc, sw)) = stroke {
                    let half_sw = (sw * 0.5).max(0.5);
                    let border_cov = (half_sw + 0.5 - sdf.abs()).clamp(0.0, 1.0);
                    if border_cov > 0.0 {
                        let a = ((sc.3 as f32) * border_cov) as u8;
                        self.blend_pixel(px, py, Rgba(sc.0, sc.1, sc.2, a));
                    }
                }
            }
        }
    }

    fn draw_circle_aa(&mut self, cx: f32, cy: f32, radius: f32, fill: Rgba) {
        let min_x = (cx - radius - 2.0).floor().max(0.0) as i32;
        let max_x = (cx + radius + 2.0).ceil().min(self.width as f32) as i32;
        let min_y = (cy - radius - 2.0).floor().max(0.0) as i32;
        let max_y = (cy + radius + 2.0).ceil().min(self.height as f32) as i32;

        for py in min_y..max_y {
            let dy = (py as f32 + 0.5) - cy;
            for px in min_x..max_x {
                let dx = (px as f32 + 0.5) - cx;
                let sdf = (dx * dx + dy * dy).sqrt() - radius;
                let cov = (0.5 - sdf).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let a = ((fill.3 as f32) * cov) as u8;
                    self.blend_pixel(px, py, Rgba(fill.0, fill.1, fill.2, a));
                }
            }
        }
    }

    fn draw_line_aa(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, thickness: f32, color: Rgba) {
        let min_x = (x1.min(x2) - thickness - 2.0).floor().max(0.0) as i32;
        let max_x = (x1.max(x2) + thickness + 2.0)
            .ceil()
            .min(self.width as f32) as i32;
        let min_y = (y1.min(y2) - thickness - 2.0).floor().max(0.0) as i32;
        let max_y = (y1.max(y2) + thickness + 2.0)
            .ceil()
            .min(self.height as f32) as i32;
        let vx = x2 - x1;
        let vy = y2 - y1;
        let len_sq = (vx * vx + vy * vy).max(0.0001);
        let half_t = (thickness * 0.5).max(0.5);

        for py in min_y..max_y {
            let wy = (py as f32 + 0.5) - y1;
            for px in min_x..max_x {
                let wx = (px as f32 + 0.5) - x1;
                let t = ((wx * vx + wy * vy) / len_sq).clamp(0.0, 1.0);
                let proj_x = x1 + t * vx;
                let proj_y = y1 + t * vy;
                let dist = ((px as f32 + 0.5 - proj_x).powi(2)
                    + (py as f32 + 0.5 - proj_y).powi(2))
                .sqrt();
                let cov = (half_t + 0.5 - dist).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let a = ((color.3 as f32) * cov) as u8;
                    self.blend_pixel(px, py, Rgba(color.0, color.1, color.2, a));
                }
            }
        }
    }

    /// Renders smooth anti-aliased text with bilinear glyph interpolation and soft drop-shadow.
    fn draw_text_smooth(&mut self, x: i32, y: i32, text: &str, scale: f32, color: Rgba) {
        let scale = scale.max(1.0);
        let adv_x = (8.5 * scale).round() as i32;
        let glyph_px = (8.0 * scale).ceil() as i32;
        let mut pen_x = x;

        for ch in text.chars() {
            if ch == '\t' {
                pen_x += adv_x * 2;
                continue;
            }
            let glyph = ascii_glyph_8x8(ch);
            // Soft shadow pass + foreground pass
            for (dx_off, dy_off, pass_col) in [
                (
                    1,
                    (scale * 0.6).round() as i32,
                    Rgba(0, 0, 0, ((color.3 as u32) * 90 / 255) as u8),
                ),
                (0, 0, color),
            ] {
                for py in 0..glyph_px {
                    let gy = ((py as f32 + 0.5) / scale) - 0.5;
                    for px in 0..glyph_px {
                        let gx = ((px as f32 + 0.5) / scale) - 0.5;
                        let cov = sample_glyph_bilinear(&glyph, gx, gy);
                        if cov > 0.08 {
                            let smooth = ((cov - 0.18) / 0.58).clamp(0.0, 1.0);
                            if smooth > 0.0 {
                                let a = ((pass_col.3 as f32) * smooth) as u8;
                                self.blend_pixel(
                                    pen_x + px + dx_off,
                                    y + py + dy_off,
                                    Rgba(pass_col.0, pass_col.1, pass_col.2, a),
                                );
                            }
                        }
                    }
                }
            }
            pen_x += adv_x;
        }
    }

    fn draw_wrapped_text(
        &mut self,
        x: i32,
        y: i32,
        max_width_px: i32,
        text: &str,
        scale: f32,
        color: Rgba,
        max_lines: usize,
    ) -> i32 {
        let adv_x = (8.5 * scale).round().max(6.0) as i32;
        let max_chars = (max_width_px / adv_x).max(8) as usize;
        let line_h = (12.5 * scale).round() as i32;
        let lines = wrap_text_chars(text, max_chars, max_lines);
        let mut cur_y = y;
        for line in &lines {
            self.draw_text_smooth(x, cur_y, line, scale, color);
            cur_y += line_h;
        }
        cur_y
    }

    /// Writes a standard 24-bit uncompressed BMP buffer.
    fn to_bmp_bytes(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let row_stride = (w * 3).div_ceil(4) * 4;
        let pixel_data_size = row_stride * h;
        let file_size = 54 + pixel_data_size;

        let mut bmp = Vec::with_capacity(file_size);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());

        // BITMAPINFOHEADER (40 bytes)
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(self.width as i32).to_le_bytes());
        bmp.extend_from_slice(&(self.height as i32).to_le_bytes()); // bottom-up
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
        bmp.extend_from_slice(&2835i32.to_le_bytes());
        bmp.extend_from_slice(&2835i32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());

        let pad = row_stride - w * 3;
        for row_rev in (0..h).rev() {
            let row_start = row_rev * w * 4;
            for col in 0..w {
                let idx = row_start + col * 4;
                bmp.push(self.pixels[idx + 2]); // B
                bmp.push(self.pixels[idx + 1]); // G
                bmp.push(self.pixels[idx]); // R
            }
            for _ in 0..pad {
                bmp.push(0);
            }
        }
        bmp
    }

    /// Pure-Rust RFC 2083 PNG encoder (uncompressed zlib deflate blocks + Adler32 + CRC32).
    fn to_png_bytes_fallback(&self) -> Vec<u8> {
        let w = self.width;
        let h = self.height;
        let mut raw_scanlines = Vec::with_capacity(((w as usize) * 4 + 1) * (h as usize));
        for y in 0..(h as usize) {
            raw_scanlines.push(0u8); // Filter type 0 (None)
            let start = y * (w as usize) * 4;
            raw_scanlines.extend_from_slice(&self.pixels[start..start + (w as usize) * 4]);
        }

        // Build valid zlib stream with stored (BTYPE=00) blocks
        let mut zlib = Vec::with_capacity(raw_scanlines.len() + 64);
        zlib.push(0x78);
        zlib.push(0x01);
        let chunks: Vec<&[u8]> = if raw_scanlines.is_empty() {
            vec![&[]]
        } else {
            raw_scanlines.chunks(65_535).collect()
        };
        for (i, block) in chunks.iter().enumerate() {
            let is_last = i + 1 == chunks.len();
            zlib.push(if is_last { 0x01 } else { 0x00 });
            let len = block.len() as u16;
            zlib.extend_from_slice(&len.to_le_bytes());
            zlib.extend_from_slice(&(!len).to_le_bytes());
            zlib.extend_from_slice(block);
        }
        zlib.extend_from_slice(&adler32(&raw_scanlines).to_be_bytes());

        let mut png = Vec::with_capacity(zlib.len() + 64);
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");

        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.push(8); // bit depth
        ihdr.push(6); // RGBA
        ihdr.push(0);
        ihdr.push(0);
        ihdr.push(0);
        write_png_chunk(&mut png, b"IHDR", &ihdr);
        write_png_chunk(&mut png, b"IDAT", &zlib);
        write_png_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// Saves the canvas as a compressed PNG (using macOS `/usr/bin/sips` CoreGraphics compression
    /// when available, with automatic fallback to the built-in pure-Rust PNG encoder).
    fn save_png(&self, dest_png: &Path) -> Result<u64, String> {
        if let Ok(guard) = TempDirGuard::create("png") {
            let tmp_bmp = guard.path.join("frame.bmp");
            if fs::write(&tmp_bmp, self.to_bmp_bytes()).is_ok() && Path::new("/usr/bin/sips").exists()
            {
                if let Ok(out) = StdCommand::new("/usr/bin/sips")
                    .args(["-s", "format", "png"])
                    .arg(&tmp_bmp)
                    .arg("--out")
                    .arg(dest_png)
                    .output()
                {
                    if out.status.success() {
                        if let Ok(meta) = fs::metadata(dest_png) {
                            if meta.len() > 64 {
                                return Ok(meta.len());
                            }
                        }
                    }
                }
            }
        }

        let png_bytes = self.to_png_bytes_fallback();
        fs::write(dest_png, &png_bytes)
            .map_err(|e| format!("failed to write PNG '{}': {}", dest_png.display(), e))?;
        Ok(png_bytes.len() as u64)
    }
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32_ieee(chunk_type: &[u8; 4], data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in chunk_type.iter().chain(data.iter()) {
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

fn write_png_chunk(buf: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(chunk_type);
    buf.extend_from_slice(data);
    buf.extend_from_slice(&crc32_ieee(chunk_type, data).to_be_bytes());
}

fn wrap_text_chars(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            if current.is_empty() {
                if word.chars().count() > max_chars {
                    let truncated: String = word.chars().take(max_chars).collect();
                    current.push_str(&truncated);
                } else {
                    current.push_str(word);
                }
            } else if current.chars().count() + 1 + word.chars().count() <= max_chars {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                if lines.len() >= max_lines {
                    return lines;
                }
                current = word.chars().take(max_chars).collect();
            }
        }
        if !current.is_empty() {
            lines.push(current);
            if lines.len() >= max_lines {
                return lines;
            }
        }
    }
    lines
}

#[inline]
fn glyph_bit(glyph: &[u8; 8], gx: i32, gy: i32) -> f32 {
    if !(0..8).contains(&gx) || !(0..8).contains(&gy) {
        return 0.0;
    }
    let row = glyph[gy as usize];
    if (row & (0x80 >> gx)) != 0 {
        1.0
    } else {
        0.0
    }
}

#[inline]
fn sample_glyph_bilinear(glyph: &[u8; 8], gx: f32, gy: f32) -> f32 {
    let x0 = gx.floor() as i32;
    let y0 = gy.floor() as i32;
    let fx = gx - (x0 as f32);
    let fy = gy - (y0 as f32);

    let p00 = glyph_bit(glyph, x0, y0);
    let p10 = glyph_bit(glyph, x0 + 1, y0);
    let p01 = glyph_bit(glyph, x0, y0 + 1);
    let p11 = glyph_bit(glyph, x0 + 1, y0 + 1);

    let top = p00 * (1.0 - fx) + p10 * fx;
    let bot = p01 * (1.0 - fx) + p11 * fx;
    top * (1.0 - fy) + bot * fy
}

/// Complete 95-glyph printable ASCII 8x8 bitmap font (`0x20..=0x7E`, MSB = left pixel).
fn ascii_glyph_8x8(ch: char) -> [u8; 8] {
    match ch {
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '!' => [0x18, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x18, 0x00],
        '"' => [0x6C, 0x6C, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00],
        '#' => [0x6C, 0x6C, 0xFE, 0x6C, 0xFE, 0x6C, 0x6C, 0x00],
        '$' => [0x18, 0x3E, 0x60, 0x3C, 0x06, 0x7C, 0x18, 0x00],
        '%' => [0x00, 0xC6, 0xCC, 0x18, 0x30, 0x66, 0xC6, 0x00],
        '&' => [0x38, 0x6C, 0x38, 0x76, 0xDC, 0xCC, 0x76, 0x00],
        '\'' => [0x18, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00],
        '(' => [0x0C, 0x18, 0x30, 0x30, 0x30, 0x18, 0x0C, 0x00],
        ')' => [0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x18, 0x30, 0x00],
        '*' => [0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00],
        '+' => [0x00, 0x18, 0x18, 0x7E, 0x18, 0x18, 0x00, 0x00],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30],
        '-' | '—' | '–' => [0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
        '/' => [0x06, 0x0C, 0x18, 0x30, 0x60, 0xC0, 0x80, 0x00],
        '0' => [0x7C, 0xC6, 0xCE, 0xD6, 0xE6, 0xC6, 0x7C, 0x00],
        '1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        '2' => [0x7C, 0xC6, 0x06, 0x1C, 0x30, 0x66, 0xFE, 0x00],
        '3' => [0x7C, 0xC6, 0x06, 0x3C, 0x06, 0xC6, 0x7C, 0x00],
        '4' => [0x1C, 0x3C, 0x6C, 0xCC, 0xFE, 0x0C, 0x1E, 0x00],
        '5' => [0xFE, 0xC0, 0xFC, 0x06, 0x06, 0xC6, 0x7C, 0x00],
        '6' => [0x38, 0x60, 0xC0, 0xFC, 0xC6, 0xC6, 0x7C, 0x00],
        '7' => [0xFE, 0xC6, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
        '8' => [0x7C, 0xC6, 0xC6, 0x7C, 0xC6, 0xC6, 0x7C, 0x00],
        '9' => [0x7C, 0xC6, 0xC6, 0x7E, 0x06, 0x0C, 0x78, 0x00],
        ':' => [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00],
        ';' => [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x30],
        '<' => [0x0C, 0x18, 0x30, 0x60, 0x30, 0x18, 0x0C, 0x00],
        '=' => [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00],
        '>' => [0x30, 0x18, 0x0C, 0x06, 0x0C, 0x18, 0x30, 0x00],
        '?' => [0x7C, 0xC6, 0x0C, 0x18, 0x18, 0x00, 0x18, 0x00],
        '@' => [0x7C, 0xC6, 0xDE, 0xDE, 0xDE, 0xC0, 0x78, 0x00],
        'A' => [0x38, 0x6C, 0xC6, 0xFE, 0xC6, 0xC6, 0xC6, 0x00],
        'B' => [0xFC, 0x66, 0x66, 0x7C, 0x66, 0x66, 0xFC, 0x00],
        'C' => [0x3C, 0x66, 0xC0, 0xC0, 0xC0, 0x66, 0x3C, 0x00],
        'D' => [0xF8, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0xF8, 0x00],
        'E' => [0xFE, 0x62, 0x68, 0x78, 0x68, 0x62, 0xFE, 0x00],
        'F' => [0xFE, 0x62, 0x68, 0x78, 0x68, 0x60, 0xF0, 0x00],
        'G' => [0x3C, 0x66, 0xC0, 0xC0, 0xCE, 0x66, 0x3E, 0x00],
        'H' => [0xC6, 0xC6, 0xC6, 0xFE, 0xC6, 0xC6, 0xC6, 0x00],
        'I' => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'J' => [0x1E, 0x0C, 0x0C, 0x0C, 0xCC, 0xCC, 0x78, 0x00],
        'K' => [0xE6, 0x66, 0x6C, 0x78, 0x6C, 0x66, 0xE6, 0x00],
        'L' => [0xF0, 0x60, 0x60, 0x60, 0x62, 0x66, 0xFE, 0x00],
        'M' => [0xC6, 0xEE, 0xFE, 0xFE, 0xD6, 0xC6, 0xC6, 0x00],
        'N' => [0xC6, 0xE6, 0xF6, 0xDE, 0xCE, 0xC6, 0xC6, 0x00],
        'O' => [0x7C, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0x7C, 0x00],
        'P' => [0xFC, 0x66, 0x66, 0x7C, 0x60, 0x60, 0xF0, 0x00],
        'Q' => [0x7C, 0xC6, 0xC6, 0xC6, 0xD6, 0xDE, 0x7C, 0x06],
        'R' => [0xFC, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0xE6, 0x00],
        'S' => [0x7C, 0xC6, 0x60, 0x38, 0x0C, 0xC6, 0x7C, 0x00],
        'T' => [0x7E, 0x5A, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'U' => [0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0x7C, 0x00],
        'V' => [0xC6, 0xC6, 0xC6, 0xC6, 0xC6, 0x6C, 0x38, 0x00],
        'W' => [0xC6, 0xC6, 0xD6, 0xFE, 0xFE, 0xEE, 0xC6, 0x00],
        'X' => [0xC6, 0xC6, 0x6C, 0x38, 0x6C, 0xC6, 0xC6, 0x00],
        'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x3C, 0x00],
        'Z' => [0xFE, 0xC6, 0x8C, 0x18, 0x32, 0x66, 0xFE, 0x00],
        '[' => [0x3C, 0x30, 0x30, 0x30, 0x30, 0x30, 0x3C, 0x00],
        '\\' => [0xC0, 0x60, 0x30, 0x18, 0x0C, 0x06, 0x02, 0x00],
        ']' => [0x3C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x3C, 0x00],
        '^' => [0x10, 0x38, 0x6C, 0xC6, 0x00, 0x00, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF],
        '`' => [0x30, 0x18, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'a' => [0x00, 0x00, 0x78, 0x0C, 0x7C, 0xCC, 0x76, 0x00],
        'b' => [0xE0, 0x60, 0x7C, 0x66, 0x66, 0x66, 0xDC, 0x00],
        'c' => [0x00, 0x00, 0x7C, 0xC6, 0xC0, 0xC6, 0x7C, 0x00],
        'd' => [0x1C, 0x0C, 0x7C, 0xCC, 0xCC, 0xCC, 0x76, 0x00],
        'e' => [0x00, 0x00, 0x7C, 0xC6, 0xFE, 0xC0, 0x7C, 0x00],
        'f' => [0x3C, 0x66, 0x60, 0xF8, 0x60, 0x60, 0xF0, 0x00],
        'g' => [0x00, 0x00, 0x76, 0xCC, 0xCC, 0x7C, 0x0C, 0xF8],
        'h' => [0xE0, 0x60, 0x6C, 0x76, 0x66, 0x66, 0xE6, 0x00],
        'i' => [0x18, 0x00, 0x38, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'j' => [0x06, 0x00, 0x06, 0x06, 0x06, 0x66, 0x66, 0x3C],
        'k' => [0xE0, 0x60, 0x66, 0x6C, 0x78, 0x6C, 0xE6, 0x00],
        'l' => [0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'm' => [0x00, 0x00, 0xEC, 0xFE, 0xD6, 0xD6, 0xC6, 0x00],
        'n' => [0x00, 0x00, 0xDC, 0x66, 0x66, 0x66, 0x66, 0x00],
        'o' => [0x00, 0x00, 0x7C, 0xC6, 0xC6, 0xC6, 0x7C, 0x00],
        'p' => [0x00, 0x00, 0xDC, 0x66, 0x66, 0x7C, 0x60, 0xF0],
        'q' => [0x00, 0x00, 0x76, 0xCC, 0xCC, 0x7C, 0x0C, 0x1E],
        'r' => [0x00, 0x00, 0xDC, 0x76, 0x60, 0x60, 0xF0, 0x00],
        's' => [0x00, 0x00, 0x7E, 0xC0, 0x7C, 0x06, 0xFC, 0x00],
        't' => [0x30, 0x30, 0xFC, 0x30, 0x30, 0x36, 0x1C, 0x00],
        'u' => [0x00, 0x00, 0xCC, 0xCC, 0xCC, 0xCC, 0x76, 0x00],
        'v' => [0x00, 0x00, 0xC6, 0xC6, 0xC6, 0x6C, 0x38, 0x00],
        'w' => [0x00, 0x00, 0xC6, 0xD6, 0xFE, 0xFE, 0x6C, 0x00],
        'x' => [0x00, 0x00, 0xC6, 0x6C, 0x38, 0x6C, 0xC6, 0x00],
        'y' => [0x00, 0x00, 0xC6, 0xC6, 0xC6, 0x7E, 0x06, 0xFC],
        'z' => [0x00, 0x00, 0xFE, 0x8C, 0x18, 0x32, 0xFE, 0x00],
        '{' => [0x0E, 0x18, 0x18, 0x70, 0x18, 0x18, 0x0E, 0x00],
        '|' => [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00],
        '}' => [0x70, 0x18, 0x18, 0x0E, 0x18, 0x18, 0x70, 0x00],
        '~' => [0x76, 0xDC, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '•' | '·' => [0x00, 0x00, 0x3C, 0x3C, 0x3C, 0x3C, 0x00, 0x00],
        _ => [0x7E, 0x42, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00],
    }
}

fn render_video_scene_canvas(
    deck_title: &str,
    scene: &VideoSceneSpec,
    scene_idx: usize,
    total_scenes: usize,
    width: u32,
    height: u32,
    progress_ratio: f32,
) -> RgbaCanvas {
    let accent = scene
        .accent_hex
        .as_deref()
        .map(|h| parse_hex_color(h, Rgba(124, 92, 255, 255)))
        .unwrap_or(Rgba(124, 92, 255, 255));
    let accent_alt = Rgba(42, 216, 167, 255);
    let fg = Rgba(243, 245, 249, 255);
    let muted = Rgba(154, 164, 184, 255);
    let card_fill = Rgba(19, 24, 34, 238);
    let card_border = Rgba(37, 46, 66, 255);

    let mut canvas = RgbaCanvas::new(width, height, Rgba(11, 13, 18, 255));
    canvas.fill_obsidian_backdrop(accent);

    let scale_ref = (width as f32 / 1280.0).clamp(0.45, 2.0);
    let pad_x = (56.0 * scale_ref).round();
    let pad_y = (44.0 * scale_ref).round();
    let card_w = (width as f32) - pad_x * 2.0;
    let card_h = (height as f32) - pad_y * 2.0 - (20.0 * scale_ref);

    // Main Obsidian glass card
    canvas.draw_round_rect_aa(
        pad_x,
        pad_y,
        card_w,
        card_h,
        18.0 * scale_ref,
        Some(card_fill),
        Some((card_border, 1.5 * scale_ref)),
    );

    // Top accent bar inside card
    canvas.draw_round_rect_aa(
        pad_x + 24.0 * scale_ref,
        pad_y + 22.0 * scale_ref,
        88.0 * scale_ref,
        6.0 * scale_ref,
        3.0 * scale_ref,
        Some(accent),
        None,
    );

    // Scene badge pill
    let badge_text = format!("SCENE {:02} / {:02}", scene_idx + 1, total_scenes.max(1));
    canvas.draw_text_smooth(
        (pad_x + 24.0 * scale_ref) as i32,
        (pad_y + 38.0 * scale_ref) as i32,
        &badge_text,
        (1.4 * scale_ref).max(1.0),
        accent_alt,
    );

    if !deck_title.trim().is_empty() {
        canvas.draw_text_smooth(
            (pad_x + 210.0 * scale_ref) as i32,
            (pad_y + 38.0 * scale_ref) as i32,
            deck_title.trim(),
            (1.35 * scale_ref).max(1.0),
            muted,
        );
    }

    // Scene Heading & Subheading
    let inner_x = (pad_x + 32.0 * scale_ref) as i32;
    let inner_w = (card_w - 64.0 * scale_ref) as i32;
    let mut cursor_y = (pad_y + 74.0 * scale_ref) as i32;

    let heading_str = if scene.heading.trim().is_empty() {
        deck_title
    } else {
        scene.heading.trim()
    };
    cursor_y = canvas.draw_wrapped_text(
        inner_x,
        cursor_y,
        inner_w,
        heading_str,
        (3.1 * scale_ref).max(1.2),
        fg,
        2,
    );

    if !scene.subheading.trim().is_empty() {
        cursor_y += (6.0 * scale_ref) as i32;
        cursor_y = canvas.draw_wrapped_text(
            inner_x,
            cursor_y,
            inner_w,
            scene.subheading.trim(),
            (1.85 * scale_ref).max(1.0),
            muted,
            2,
        );
    }

    cursor_y += (18.0 * scale_ref) as i32;

    let has_code = scene
        .code_snippet
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let bullets_max_w = if has_code && !scene.bullets.is_empty() {
        ((inner_w as f32) * 0.46) as i32
    } else {
        inner_w
    };

    // Bullets
    let mut bullet_y = cursor_y;
    for bullet in scene.bullets.iter().take(5) {
        let dot_r = (4.5 * scale_ref).max(2.5);
        canvas.draw_circle_aa(
            (inner_x as f32) + dot_r + 2.0,
            (bullet_y as f32) + 8.0 * scale_ref,
            dot_r,
            accent,
        );
        bullet_y = canvas.draw_wrapped_text(
            inner_x + (22.0 * scale_ref) as i32,
            bullet_y,
            bullets_max_w - (24.0 * scale_ref) as i32,
            bullet,
            (1.75 * scale_ref).max(1.0),
            fg,
            2,
        );
        bullet_y += (10.0 * scale_ref) as i32;
    }

    // Code Terminal Box
    if let Some(code) = scene.code_snippet.as_deref().filter(|s| !s.trim().is_empty()) {
        let (code_x, code_y, code_w) = if !scene.bullets.is_empty() {
            (
                (inner_x as f32) + (inner_w as f32) * 0.49,
                cursor_y as f32,
                (inner_w as f32) * 0.51,
            )
        } else {
            (inner_x as f32, bullet_y as f32, inner_w as f32)
        };
        let remaining_h = ((pad_y + card_h - 26.0 * scale_ref) - code_y).max(80.0 * scale_ref);
        canvas.draw_round_rect_aa(
            code_x,
            code_y,
            code_w,
            remaining_h,
            12.0 * scale_ref,
            Some(Rgba(8, 10, 15, 250)),
            Some((accent, 1.4 * scale_ref)),
        );

        // macOS window control dots
        let dot_y = code_y + 15.0 * scale_ref;
        canvas.draw_circle_aa(
            code_x + 16.0 * scale_ref,
            dot_y,
            4.2 * scale_ref,
            Rgba(255, 95, 86, 255),
        );
        canvas.draw_circle_aa(
            code_x + 30.0 * scale_ref,
            dot_y,
            4.2 * scale_ref,
            Rgba(255, 189, 46, 255),
        );
        canvas.draw_circle_aa(
            code_x + 44.0 * scale_ref,
            dot_y,
            4.2 * scale_ref,
            Rgba(39, 201, 63, 255),
        );

        let code_scale = (1.35 * scale_ref).max(1.0);
        let line_step = (12.0 * code_scale).round() as i32;
        let max_code_chars =
            (((code_w - 28.0 * scale_ref) / (8.5 * code_scale)).floor() as usize).max(12);
        let mut cy_line = (code_y + 32.0 * scale_ref) as i32;
        let max_y_code = (code_y + remaining_h - 14.0 * scale_ref) as i32;

        for line in code.lines() {
            if cy_line + line_step > max_y_code {
                break;
            }
            let trimmed = line.trim_start();
            let line_col = if trimmed.starts_with("//") || trimmed.starts_with('#') {
                Rgba(122, 136, 160, 255)
            } else if trimmed.starts_with("fn ")
                || trimmed.starts_with("pub ")
                || trimmed.starts_with("async ")
                || trimmed.starts_with("export ")
                || trimmed.starts_with("const ")
            {
                accent_alt
            } else {
                Rgba(230, 237, 243, 255)
            };
            let clipped: String = line.chars().take(max_code_chars).collect();
            canvas.draw_text_smooth(
                (code_x + 14.0 * scale_ref) as i32,
                cy_line,
                &clipped,
                code_scale,
                line_col,
            );
            cy_line += line_step;
        }
    }

    // Bottom storyboard progress track + fill
    let bar_y = (height as f32) - (16.0 * scale_ref);
    let bar_h = (6.0 * scale_ref).max(3.0);
    canvas.draw_round_rect_aa(
        pad_x,
        bar_y,
        card_w,
        bar_h,
        bar_h * 0.5,
        Some(Rgba(37, 46, 66, 200)),
        None,
    );
    let fill_w = (card_w * progress_ratio.clamp(0.03, 1.0)).max(bar_h);
    canvas.draw_round_rect_aa(
        pad_x,
        bar_y,
        fill_w,
        bar_h,
        bar_h * 0.5,
        Some(accent),
        None,
    );

    canvas
}

fn resolve_ffmpeg_binary() -> Result<PathBuf, String> {
    let candidates = [
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "/usr/bin/ffmpeg",
    ];
    for c in candidates {
        let p = Path::new(c);
        if p.exists() {
            return Ok(p.to_path_buf());
        }
    }
    if let Ok(out) = StdCommand::new("which").arg("ffmpeg").output() {
        if out.status.success() {
            let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !found.is_empty() && Path::new(&found).exists() {
                return Ok(PathBuf::from(found));
            }
        }
    }
    Err("ffmpeg binary not found at /opt/homebrew/bin/ffmpeg or on PATH".to_string())
}

fn write_silent_pcm_wav(dest_wav: &Path, sample_rate: u32, duration_secs: f32) -> Result<(), String> {
    let num_samples = ((sample_rate as f32) * duration_secs.max(0.1)).round() as u32;
    let data_bytes = num_samples * 2;
    let riff_size = 36 + data_bytes;

    let mut wav = Vec::with_capacity((44 + data_bytes) as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // Mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav.resize((44 + data_bytes) as usize, 0u8);

    fs::write(dest_wav, &wav)
        .map_err(|e| format!("failed to write PCM WAV '{}': {}", dest_wav.display(), e))
}

async fn try_synthesize_scene_voiceover(
    ffmpeg_bin: &Path,
    staging_dir: &Path,
    scene_idx: usize,
    narration: &str,
    voice: Option<&str>,
    target_duration_secs: f32,
) -> (PathBuf, f32, bool) {
    let sample_rate = 22_050u32;
    let wav_out = staging_dir.join(format!("scene_audio_{}.wav", scene_idx));
    let min_dur = target_duration_secs.max(0.4);

    let voice_disabled = voice
        .map(|v| {
            let low = v.trim().to_ascii_lowercase();
            low == "none" || low == "silent" || low == "off"
        })
        .unwrap_or(false);

    if !narration.trim().is_empty() && !voice_disabled && Path::new("/usr/bin/say").exists() {
        let aiff_path = staging_dir.join(format!("scene_tts_{}.aiff", scene_idx));
        let mut cmd = TokioCommand::new("/usr/bin/say");
        if let Some(v) = voice.map(str::trim).filter(|s| !s.is_empty()) {
            cmd.arg("-v").arg(v);
        }
        cmd.arg("-o").arg(&aiff_path).arg(narration.trim());

        if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
            // Note: In headless macOS sandboxes without CoreAudio/speechsynthesisd, `/usr/bin/say`
            // exits 0 and writes a 4096-byte header-only AIFF (`Duration: N/A`). Require > 8192 bytes.
            let valid_aiff = out.status.success()
                && fs::metadata(&aiff_path)
                    .map(|m| m.len() > 8_192)
                    .unwrap_or(false);

            if valid_aiff {
                let norm = TokioCommand::new(ffmpeg_bin)
                    .args(["-y", "-i"])
                    .arg(&aiff_path)
                    .args([
                        "-af",
                        &format!("apad=whole_dur={:.3}", min_dur),
                        "-ar",
                        &sample_rate.to_string(),
                        "-ac",
                        "1",
                        "-c:a",
                        "pcm_s16le",
                    ])
                    .arg(&wav_out)
                    .output()
                    .await;

                if let Ok(norm_out) = norm {
                    if norm_out.status.success() {
                        if let Ok(meta) = fs::metadata(&wav_out) {
                            if meta.len() > 44 {
                                let pcm_bytes = meta.len().saturating_sub(44);
                                let actual_dur =
                                    ((pcm_bytes as f32) / ((sample_rate * 2) as f32)).max(min_dur);
                                return (wav_out, actual_dur, true);
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = write_silent_pcm_wav(&wav_out, sample_rate, min_dur);
    (wav_out, min_dur, false)
}

/// Renders a multi-scene programmatic explainer `.mp4` video on Apple Silicon using
/// anti-aliased typography/cards, optional macOS `/usr/bin/say` neural TTS voiceover
/// (with graceful headless fallback), and H.264 `yuv420p` `+faststart` encoding via FFmpeg.
pub async fn render_explainer_video(
    workspace_root: &Path,
    spec: &VideoStoryboardSpec,
) -> Result<VideoArtifact, String> {
    let normalized_out = if spec.output_path.trim().to_ascii_lowercase().ends_with(".mp4") {
        spec.output_path.trim().to_string()
    } else {
        format!("{}.mp4", spec.output_path.trim())
    };
    let (mp4_abs, rel_mp4) = resolve_confined_output_path(workspace_root, &normalized_out)?;
    let ffmpeg_bin = resolve_ffmpeg_binary()?;

    // Ensure even dimensions for H.264 yuv420p compatibility
    let width = (spec.width.clamp(160, 3840) & !1).max(160);
    let height = (spec.height.clamp(120, 2160) & !1).max(120);
    let fps = spec.fps.clamp(10, 60);

    let scenes: Vec<VideoSceneSpec> = if spec.scenes.is_empty() {
        vec![VideoSceneSpec {
            heading: if spec.title.is_empty() {
                "Rursor Explainer".to_string()
            } else {
                spec.title.clone()
            },
            subheading: "Generated locally by Rursor Media Studio".to_string(),
            duration_secs: 2.0,
            ..VideoSceneSpec::default()
        }]
    } else {
        spec.scenes.clone()
    };

    let guard = TempDirGuard::create("video")?;
    let total_scenes = scenes.len();

    let mut frame_entries: Vec<(PathBuf, f32)> = Vec::with_capacity(total_scenes * 2);
    let mut audio_wavs: Vec<PathBuf> = Vec::with_capacity(total_scenes);
    let mut total_duration = 0.0_f32;
    let mut any_voiceover = false;

    for (idx, scene) in scenes.iter().enumerate() {
        let base_dur = if scene.duration_secs.is_finite() && scene.duration_secs > 0.2 {
            scene.duration_secs
        } else {
            2.5
        };

        let narration_text = scene.narration.as_deref().unwrap_or("");
        let (wav_path, effective_dur, used_tts) = try_synthesize_scene_voiceover(
            &ffmpeg_bin,
            &guard.path,
            idx,
            narration_text,
            spec.voice.as_deref(),
            base_dur,
        )
        .await;
        if used_tts {
            any_voiceover = true;
        }
        audio_wavs.push(wav_path);

        // Render two smooth timeline progress steps per scene so the progress bar animates
        let half_dur = (effective_dur * 0.5).max(0.15);
        let ratio_a = ((idx as f32) + 0.5) / (total_scenes as f32);
        let ratio_b = ((idx + 1) as f32) / (total_scenes as f32);

        let title_clone = spec.title.clone();
        let scene_clone_a = scene.clone();
        let png_a = guard.path.join(format!("scene_{}_a.png", idx));
        let png_a_out = png_a.clone();
        tokio::task::spawn_blocking(move || {
            let canvas = render_video_scene_canvas(
                &title_clone,
                &scene_clone_a,
                idx,
                total_scenes,
                width,
                height,
                ratio_a,
            );
            canvas.save_png(&png_a_out)
        })
        .await
        .map_err(|e| format!("frame raster task failed: {}", e))??;

        let title_clone_b = spec.title.clone();
        let scene_clone_b = scene.clone();
        let png_b = guard.path.join(format!("scene_{}_b.png", idx));
        let png_b_out = png_b.clone();
        tokio::task::spawn_blocking(move || {
            let canvas = render_video_scene_canvas(
                &title_clone_b,
                &scene_clone_b,
                idx,
                total_scenes,
                width,
                height,
                ratio_b,
            );
            canvas.save_png(&png_b_out)
        })
        .await
        .map_err(|e| format!("frame raster task failed: {}", e))??;

        frame_entries.push((png_a, half_dur));
        frame_entries.push((png_b, (effective_dur - half_dur).max(0.15)));
        total_duration += effective_dur;
    }

    // Build ffconcat manifest for frames
    let mut video_concat = String::from("ffconcat version 1.0\n");
    for (frame_path, dur) in &frame_entries {
        let escaped = frame_path.to_string_lossy().replace('\'', "'\\''");
        video_concat.push_str(&format!("file '{}'\nduration {:.4}\n", escaped, dur));
    }
    if let Some((last_frame, _)) = frame_entries.last() {
        let escaped = last_frame.to_string_lossy().replace('\'', "'\\''");
        video_concat.push_str(&format!("file '{}'\n", escaped));
    }
    let video_concat_path = guard.path.join("frames.ffconcat");
    fs::write(&video_concat_path, video_concat.as_bytes())
        .map_err(|e| format!("failed to write ffconcat manifest: {}", e))?;

    // Build ffconcat manifest for per-scene WAV audio tracks
    let mut audio_concat = String::from("ffconcat version 1.0\n");
    for wav in &audio_wavs {
        let escaped = wav.to_string_lossy().replace('\'', "'\\''");
        audio_concat.push_str(&format!("file '{}'\n", escaped));
    }
    let audio_concat_path = guard.path.join("audio.ffconcat");
    fs::write(&audio_concat_path, audio_concat.as_bytes())
        .map_err(|e| format!("failed to write audio ffconcat: {}", e))?;

    let encode_out = TokioCommand::new(&ffmpeg_bin)
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&video_concat_path)
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&audio_concat_path)
        .args([
            "-vf",
            &format!("fps={},scale={}:{},format=yuv420p", fps, width, height),
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "21",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-shortest",
            "-movflags",
            "+faststart",
        ])
        .arg(&mp4_abs)
        .output()
        .await
        .map_err(|e| format!("failed to execute ffmpeg: {}", e))?;

    if !encode_out.status.success() {
        let stderr = String::from_utf8_lossy(&encode_out.stderr);
        return Err(format!(
            "ffmpeg H.264 encode failed ({}): {}",
            encode_out.status,
            stderr.chars().rev().take(600).collect::<String>().chars().rev().collect::<String>()
        ));
    }

    let meta = fs::metadata(&mp4_abs)
        .map_err(|e| format!("failed to stat output video '{}': {}", mp4_abs.display(), e))?;

    Ok(VideoArtifact {
        video_path: mp4_abs.to_string_lossy().to_string(),
        relative_video_path: rel_mp4,
        width,
        height,
        fps,
        duration_secs: total_duration,
        scene_count: total_scenes,
        has_voiceover: any_voiceover,
        engine: "apple-silicon-ffmpeg".to_string(),
        bytes: meta.len(),
    })
}

fn extract_veo_video_source(op_json: &Value) -> Option<(Option<String>, Option<String>)> {
    let response = op_json.get("response").unwrap_or(op_json);

    // Shape 1: response.generateVideoResponse.generatedSamples[0].video.{uri, bytesBase64Encoded}
    if let Some(video) = response
        .pointer("/generateVideoResponse/generatedSamples/0/video")
        .or_else(|| response.pointer("/generatedSamples/0/video"))
    {
        let uri = video.get("uri").and_then(Value::as_str).map(str::to_string);
        let b64 = video
            .get("bytesBase64Encoded")
            .or_else(|| video.get("data"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if uri.is_some() || b64.is_some() {
            return Some((uri, b64));
        }
    }

    // Shape 2: response.videos[0].{uri, bytesBase64Encoded}
    if let Some(v0) = response.pointer("/videos/0") {
        let uri = v0.get("uri").and_then(Value::as_str).map(str::to_string);
        let b64 = v0
            .get("bytesBase64Encoded")
            .and_then(Value::as_str)
            .map(str::to_string);
        if uri.is_some() || b64.is_some() {
            return Some((uri, b64));
        }
    }

    // Shape 3: response.predictions[0].bytesBase64Encoded
    if let Some(p0) = response.pointer("/predictions/0") {
        let uri = p0.get("uri").and_then(Value::as_str).map(str::to_string);
        let b64 = p0
            .get("bytesBase64Encoded")
            .and_then(Value::as_str)
            .map(str::to_string);
        if uri.is_some() || b64.is_some() {
            return Some((uri, b64));
        }
    }

    None
}

/// Calls Cloud Gemini Veo (`models/{veo_model}:predictLongRunning`, default `veo-2.0-generate-001`),
/// polls `operations/{name}` until completion, and writes the generated `.mp4` into `output_path`.
pub async fn generate_veo_video(
    http: &Client,
    api_key: &str,
    workspace_root: &Path,
    spec: &VeoVideoSpec,
) -> Result<VideoArtifact, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err("Gemini API key is required for Cloud Veo video generation".to_string());
    }
    if spec.prompt.trim().is_empty() {
        return Err("Veo video prompt cannot be empty".to_string());
    }

    let normalized_out = if spec.output_path.trim().to_ascii_lowercase().ends_with(".mp4") {
        spec.output_path.trim().to_string()
    } else {
        format!("{}.mp4", spec.output_path.trim())
    };
    let (mp4_abs, rel_mp4) = resolve_confined_output_path(workspace_root, &normalized_out)?;

    let raw_model = spec
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_VEO_MODEL);
    let model_id = raw_model.strip_prefix("models/").unwrap_or(raw_model);
    let aspect_ratio = spec
        .aspect_ratio
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("16:9");
    let duration_secs = spec.duration_secs.unwrap_or(6).clamp(5, 8);

    let start_url = format!(
        "{}/models/{}:predictLongRunning",
        GEMINI_V1BETA_URL, model_id
    );
    let req_body = json!({
        "instances": [{ "prompt": spec.prompt.trim() }],
        "parameters": {
            "aspectRatio": aspect_ratio,
            "sampleCount": 1,
            "durationSeconds": duration_secs,
        }
    });

    let start_resp = http
        .post(&start_url)
        .header("x-goog-api-key", key)
        .json(&req_body)
        .send()
        .await
        .map_err(|e| format!("Veo predictLongRunning request failed: {}", e))?;

    let start_status = start_resp.status();
    let start_json: Value = start_resp
        .json()
        .await
        .map_err(|e| format!("failed to parse Veo operation response: {}", e))?;

    if !start_status.is_success() {
        let msg = start_json
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Veo API returned an error");
        return Err(format!("Veo API error ({}): {}", start_status, msg));
    }

    let op_name = start_json
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "Veo predictLongRunning did not return an operation 'name'".to_string())?;

    let poll_path = if op_name.contains('/') {
        op_name.trim_start_matches('/').to_string()
    } else {
        format!("operations/{}", op_name)
    };
    let poll_url = format!("{}/{}", GEMINI_V1BETA_URL, poll_path);

    let poll_interval = Duration::from_millis(spec.poll_interval_ms.unwrap_or(4_000).max(500));
    let max_wait = Duration::from_secs(spec.max_wait_secs.unwrap_or(300).max(10));
    let started = std::time::Instant::now();

    let final_op = if start_json.get("done").and_then(Value::as_bool) == Some(true) {
        start_json
    } else {
        loop {
            if started.elapsed() > max_wait {
                return Err(format!(
                    "Veo operation '{}' timed out after {}s",
                    op_name,
                    max_wait.as_secs()
                ));
            }
            tokio::time::sleep(poll_interval).await;

            let poll_resp = http
                .get(&poll_url)
                .header("x-goog-api-key", key)
                .send()
                .await
                .map_err(|e| format!("failed to poll Veo operation '{}': {}", op_name, e))?;

            let poll_status = poll_resp.status();
            let op_val: Value = poll_resp
                .json()
                .await
                .map_err(|e| format!("invalid JSON while polling Veo operation: {}", e))?;

            if !poll_status.is_success() {
                let msg = op_val
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("polling failed");
                return Err(format!("Veo poll error ({}): {}", poll_status, msg));
            }

            if let Some(err_obj) = op_val.get("error") {
                return Err(format!("Veo generation failed: {}", err_obj));
            }

            if op_val.get("done").and_then(Value::as_bool) == Some(true) {
                break op_val;
            }
        }
    };

    let (video_uri, video_b64) = extract_veo_video_source(&final_op).ok_or_else(|| {
        "Veo completed operation did not contain generated video URI or base64 payload".to_string()
    })?;

    let video_bytes = if let Some(b64) = video_b64 {
        base64_decode(&b64)?
    } else if let Some(uri) = video_uri {
        let dl = http
            .get(&uri)
            .header("x-goog-api-key", key)
            .send()
            .await
            .map_err(|e| format!("failed to download Veo MP4 from URI: {}", e))?;
        if !dl.status().is_success() {
            return Err(format!("Veo video download failed with HTTP {}", dl.status()));
        }
        dl.bytes()
            .await
            .map_err(|e| format!("failed reading Veo MP4 stream: {}", e))?
            .to_vec()
    } else {
        return Err("empty Veo video payload".to_string());
    };

    fs::write(&mp4_abs, &video_bytes)
        .map_err(|e| format!("failed to save Veo MP4 '{}': {}", mp4_abs.display(), e))?;

    let (w, h) = if aspect_ratio == "9:16" {
        (720, 1280)
    } else {
        (1280, 720)
    };

    Ok(VideoArtifact {
        video_path: mp4_abs.to_string_lossy().to_string(),
        relative_video_path: rel_mp4,
        width: w,
        height: h,
        fps: 24,
        duration_secs: duration_secs as f32,
        scene_count: 1,
        has_voiceover: false,
        engine: model_id.to_string(),
        bytes: video_bytes.len() as u64,
    })
}

// ============================================================================
// 3. Image & Asset Studio (Cloud Gemini Imagen / Flash Image + Local SVG/Card -> PNG)
// ============================================================================

fn extract_svg_attr<'a>(tag: &'a str, attr_name: &str) -> Option<&'a str> {
    let mut search_from = 0;
    while let Some(rel_pos) = tag[search_from..].find(attr_name) {
        let pos = search_from + rel_pos;
        let prev_ok = pos == 0
            || tag.as_bytes()[pos - 1].is_ascii_whitespace()
            || tag.as_bytes()[pos - 1] == b'<';
        let after_attr = pos + attr_name.len();
        if prev_ok && after_attr < tag.len() {
            let rest = tag[after_attr..].trim_start();
            if let Some(after_eq) = rest.strip_prefix('=') {
                let val_part = after_eq.trim_start();
                if let Some(quote) = val_part.chars().next() {
                    if quote == '"' || quote == '\'' {
                        let inner = &val_part[1..];
                        if let Some(end_q) = inner.find(quote) {
                            return Some(&inner[..end_q]);
                        }
                    }
                }
            }
        }
        search_from = after_attr;
    }
    None
}

fn parse_svg_f32(tag: &str, attr: &str, default_val: f32) -> f32 {
    extract_svg_attr(tag, attr)
        .map(|v| v.trim().trim_end_matches("px"))
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default_val)
}

fn unescape_basic_xml(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Renders lightweight `<svg>` markup (`<rect>`, `<circle>`, `<line>`, `<text>`) onto `RgbaCanvas`.
fn render_svg_markup_to_canvas(svg: &str, fallback_w: u32, fallback_h: u32) -> RgbaCanvas {
    let mut width = fallback_w;
    let mut height = fallback_h;
    if let Some(svg_idx) = svg.find("<svg") {
        if let Some(end_tag) = svg[svg_idx..].find('>') {
            let open_tag = &svg[svg_idx..svg_idx + end_tag + 1];
            width = (parse_svg_f32(open_tag, "width", fallback_w as f32) as u32).clamp(64, 3840);
            height = (parse_svg_f32(open_tag, "height", fallback_h as f32) as u32).clamp(64, 2160);
        }
    }

    let mut canvas = RgbaCanvas::new(width, height, Rgba(11, 13, 18, 255));
    let mut cursor = 0usize;

    while let Some(lt_rel) = svg[cursor..].find('<') {
        let lt = cursor + lt_rel;
        let Some(gt_rel) = svg[lt..].find('>') else {
            break;
        };
        let gt = lt + gt_rel;
        let tag = &svg[lt..=gt];
        let tag_trim = tag.trim_start_matches('<').trim_start();

        if tag_trim.starts_with("rect") {
            let x = parse_svg_f32(tag, "x", 0.0);
            let y = parse_svg_f32(tag, "y", 0.0);
            let w = parse_svg_f32(tag, "width", width as f32);
            let h = parse_svg_f32(tag, "height", height as f32);
            let rx = parse_svg_f32(tag, "rx", 0.0);
            let fill = match extract_svg_attr(tag, "fill") {
                Some("none") | Some("transparent") => None,
                Some(col) => Some(parse_hex_color(col, Rgba(19, 24, 34, 255))),
                None => Some(Rgba(19, 24, 34, 255)),
            };
            let stroke = extract_svg_attr(tag, "stroke")
                .filter(|s| *s != "none")
                .map(|col| {
                    let sw = parse_svg_f32(tag, "stroke-width", 1.5);
                    (parse_hex_color(col, Rgba(124, 92, 255, 255)), sw)
                });
            canvas.draw_round_rect_aa(x, y, w, h, rx, fill, stroke);
        } else if tag_trim.starts_with("circle") {
            let cx = parse_svg_f32(tag, "cx", 0.0);
            let cy = parse_svg_f32(tag, "cy", 0.0);
            let r = parse_svg_f32(tag, "r", 8.0);
            let fill = extract_svg_attr(tag, "fill")
                .map(|c| parse_hex_color(c, Rgba(124, 92, 255, 255)))
                .unwrap_or(Rgba(124, 92, 255, 255));
            canvas.draw_circle_aa(cx, cy, r, fill);
        } else if tag_trim.starts_with("line") {
            let x1 = parse_svg_f32(tag, "x1", 0.0);
            let y1 = parse_svg_f32(tag, "y1", 0.0);
            let x2 = parse_svg_f32(tag, "x2", 0.0);
            let y2 = parse_svg_f32(tag, "y2", 0.0);
            let sw = parse_svg_f32(tag, "stroke-width", 2.0);
            let col = extract_svg_attr(tag, "stroke")
                .map(|c| parse_hex_color(c, Rgba(124, 92, 255, 255)))
                .unwrap_or(Rgba(124, 92, 255, 255));
            canvas.draw_line_aa(x1, y1, x2, y2, sw, col);
        } else if tag_trim.starts_with("text") && !tag.ends_with("/>") {
            let tx = parse_svg_f32(tag, "x", 24.0) as i32;
            let ty = parse_svg_f32(tag, "y", 32.0) as i32;
            let font_sz = parse_svg_f32(tag, "font-size", 18.0);
            let scale = (font_sz / 9.0).clamp(1.0, 5.0);
            let col = extract_svg_attr(tag, "fill")
                .map(|c| parse_hex_color(c, Rgba(243, 245, 249, 255)))
                .unwrap_or(Rgba(243, 245, 249, 255));
            if let Some(close_rel) = svg[gt + 1..].find("</text>") {
                let inner_raw = &svg[gt + 1..gt + 1 + close_rel];
                let clean = unescape_basic_xml(inner_raw.trim());
                let baseline_adjusted_y = ty - ((6.5 * scale).round() as i32);
                canvas.draw_text_smooth(tx, baseline_adjusted_y, &clean, scale, col);
                cursor = gt + 1 + close_rel + 7;
                continue;
            }
        }

        cursor = gt + 1;
    }

    canvas
}

fn render_diagram_card_canvas(spec: &ImageAssetSpec, width: u32, height: u32) -> RgbaCanvas {
    let scene = VideoSceneSpec {
        heading: if spec.prompt.trim().is_empty() {
            "Rursor Architecture Asset".to_string()
        } else {
            spec.prompt.clone()
        },
        subheading: spec
            .subtitle
            .clone()
            .unwrap_or_else(|| "Rursor Studio Visual Asset".to_string()),
        bullets: if spec.bullets.is_empty() {
            vec![
                "Native OpenXML + Media Pipeline".to_string(),
                "Deterministic Apple Silicon Raster Engine".to_string(),
            ]
        } else {
            spec.bullets.clone()
        },
        code_snippet: None,
        narration: None,
        duration_secs: 1.0,
        accent_hex: spec.accent_hex.clone(),
    };
    render_video_scene_canvas("RURSOR ASSET STUDIO", &scene, 0, 1, width, height, 1.0)
}

async fn generate_cloud_gemini_image(
    http: &Client,
    api_key: &str,
    model_id: &str,
    prompt: &str,
) -> Result<(Vec<u8>, String), String> {
    let clean_model = model_id.strip_prefix("models/").unwrap_or(model_id);

    if clean_model.starts_with("imagen-") {
        let url = format!("{}/models/{}:predict", GEMINI_V1BETA_URL, clean_model);
        let body = json!({
            "instances": [{ "prompt": prompt }],
            "parameters": {
                "sampleCount": 1,
                "outputOptions": { "mimeType": "image/png" }
            }
        });
        let resp = http
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Imagen predict request failed: {}", e))?;
        let status = resp.status();
        let val: Value = resp
            .json()
            .await
            .map_err(|e| format!("invalid Imagen JSON response: {}", e))?;
        if !status.is_success() {
            let msg = val
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Imagen API error");
            return Err(format!("Imagen error ({}): {}", status, msg));
        }
        let b64 = val
            .pointer("/predictions/0/bytesBase64Encoded")
            .and_then(Value::as_str)
            .ok_or_else(|| "Imagen response missing bytesBase64Encoded".to_string())?;
        let mime = val
            .pointer("/predictions/0/mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();
        return Ok((base64_decode(b64)?, mime));
    }

    // Gemini 2.0 Flash Image Generation (`:generateContent` with IMAGE modality)
    let url = format!("{}/models/{}:generateContent", GEMINI_V1BETA_URL, clean_model);
    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": prompt }]
        }],
        "generationConfig": {
            "responseModalities": ["TEXT", "IMAGE"]
        }
    });

    let resp = http
        .post(&url)
        .header("x-goog-api-key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Gemini image generation request failed: {}", e))?;
    let status = resp.status();
    let val: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid Gemini image JSON response: {}", e))?;
    if !status.is_success() {
        let msg = val
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Gemini image generation failed");
        return Err(format!("Gemini image error ({}): {}", status, msg));
    }

    if let Some(parts) = val
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            let inline = part.get("inlineData").or_else(|| part.get("inline_data"));
            if let Some(inl) = inline {
                if let Some(b64) = inl.get("data").and_then(Value::as_str) {
                    let mime = inl
                        .get("mimeType")
                        .or_else(|| inl.get("mime_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("image/png")
                        .to_string();
                    return Ok((base64_decode(b64)?, mime));
                }
            }
        }
    }

    Err("Gemini response did not include inlineData image bytes".to_string())
}

/// Generates a PNG image asset via either Cloud Gemini (`gemini-2.0-flash-exp-image-generation` /
/// `imagen-3.0-generate-002`) OR deterministic Local SVG / Diagram Card -> PNG rasterization.
pub async fn generate_image_asset(
    http: &Client,
    api_key: Option<&str>,
    workspace_root: &Path,
    spec: &ImageAssetSpec,
) -> Result<ImageAssetArtifact, String> {
    let normalized_out = if spec.output_path.trim().to_ascii_lowercase().ends_with(".png") {
        spec.output_path.trim().to_string()
    } else {
        format!("{}.png", spec.output_path.trim())
    };
    let (png_abs, rel_png) = resolve_confined_output_path(workspace_root, &normalized_out)?;

    let width = spec.width.unwrap_or(1280).clamp(64, 3840);
    let height = spec.height.unwrap_or(720).clamp(64, 2160);
    let mode = spec
        .mode
        .as_deref()
        .unwrap_or("auto")
        .trim()
        .to_ascii_lowercase();

    let has_svg = spec
        .svg_markup
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    if mode != "local" && !has_svg {
        if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
            let model_id = spec
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_FLASH_IMAGE_MODEL);
            match generate_cloud_gemini_image(http, key, model_id, &spec.prompt).await {
                Ok((img_bytes, mime)) => {
                    fs::write(&png_abs, &img_bytes).map_err(|e| {
                        format!("failed to write cloud image '{}': {}", png_abs.display(), e)
                    })?;
                    return Ok(ImageAssetArtifact {
                        image_path: png_abs.to_string_lossy().to_string(),
                        relative_image_path: rel_png,
                        svg_path: None,
                        width,
                        height,
                        mime_type: mime,
                        engine: model_id.to_string(),
                        bytes: img_bytes.len() as u64,
                    });
                }
                Err(err) if mode == "cloud" => return Err(err),
                Err(_) => {
                    // In "auto" mode, fall through gracefully to local diagram card renderer
                }
            }
        } else if mode == "cloud" {
            return Err("Gemini API key is required when mode == 'cloud'".to_string());
        }
    }

    // Local SVG or Diagram Card -> PNG
    let mut companion_svg_path: Option<String> = None;
    let canvas = if let Some(svg) = spec.svg_markup.as_deref().filter(|s| !s.trim().is_empty()) {
        let stem = png_abs
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("diagram");
        if let Some(parent) = png_abs.parent() {
            let svg_abs = parent.join(format!("{}.svg", stem));
            if fs::write(&svg_abs, svg.as_bytes()).is_ok() {
                companion_svg_path = Some(svg_abs.to_string_lossy().to_string());
            }
        }
        render_svg_markup_to_canvas(svg, width, height)
    } else {
        render_diagram_card_canvas(spec, width, height)
    };

    let out_w = canvas.width;
    let out_h = canvas.height;
    let png_abs_clone = png_abs.clone();
    let bytes = tokio::task::spawn_blocking(move || canvas.save_png(&png_abs_clone))
        .await
        .map_err(|e| format!("PNG encode task failed: {}", e))??;

    Ok(ImageAssetArtifact {
        image_path: png_abs.to_string_lossy().to_string(),
        relative_image_path: rel_png,
        svg_path: companion_svg_path,
        width: out_w,
        height: out_h,
        mime_type: "image/png".to_string(),
        engine: if has_svg {
            "local-svg-rasterizer".to_string()
        } else {
            "local-diagram-card".to_string()
        },
        bytes,
    })
}


// ============================================================================
// 4. Excel (.xlsx) & Word (.docx) OpenXML Generators
// ============================================================================

fn excel_column_letters(mut col_zero_based: usize) -> String {
    let mut chars = Vec::new();
    loop {
        let rem = col_zero_based % 26;
        chars.push((b'A' + rem as u8) as char);
        if col_zero_based < 26 {
            break;
        }
        col_zero_based = (col_zero_based / 26) - 1;
    }
    chars.into_iter().rev().collect()
}

fn sanitize_excel_sheet_name(raw: &str, fallback_idx: usize) -> String {
    let filtered: String = raw
        .chars()
        .map(|c| match c {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '-',
            other if (other as u32) < 0x20 => ' ',
            other => other,
        })
        .collect::<String>()
        .trim()
        .chars()
        .take(31)
        .collect();
    if filtered.is_empty() {
        format!("Sheet{}", fallback_idx)
    } else {
        filtered
    }
}

fn build_xlsx_worksheet_xml(sheet: &SheetSpec) -> (String, usize) {
    let max_cols = sheet
        .rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(sheet.headers.len());

    let mut cols_xml = String::new();
    if max_cols > 0 {
        cols_xml.push_str("<cols>");
        for col_idx in 0..max_cols {
            let mut max_len = sheet
                .headers
                .get(col_idx)
                .map(|h| h.chars().count())
                .unwrap_or(10);
            for row in &sheet.rows {
                if let Some(cell) = row.get(col_idx) {
                    let clen = match cell {
                        CellValue::String(s) => s.chars().count(),
                        CellValue::Number(n) => format!("{}", n).len(),
                        CellValue::Formula(_) => 14,
                    };
                    if clen > max_len {
                        max_len = clen;
                    }
                }
            }
            let width = ((max_len as f32) * 1.18 + 4.0).clamp(12.0, 48.0);
            let idx1 = col_idx + 1;
            cols_xml.push_str(&format!(
                "<col min=\"{idx1}\" max=\"{idx1}\" width=\"{width:.1}\" customWidth=\"1\"/>"
            ));
        }
        cols_xml.push_str("</cols>");
    }

    let mut sheet_data = String::from("<sheetData>");
    let mut row_num = 1usize;

    if !sheet.headers.is_empty() {
        sheet_data.push_str(&format!("<row r=\"{}\" ht=\"22\" customHeight=\"1\">", row_num));
        for (col_idx, header) in sheet.headers.iter().enumerate() {
            let cell_ref = format!("{}{}", excel_column_letters(col_idx), row_num);
            sheet_data.push_str(&format!(
                "<c r=\"{cell_ref}\" s=\"1\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{val}</t></is></c>",
                cell_ref = cell_ref,
                val = xml_escape(header)
            ));
        }
        sheet_data.push_str("</row>");
        row_num += 1;
    }

    for row in &sheet.rows {
        sheet_data.push_str(&format!("<row r=\"{}\">", row_num));
        for (col_idx, cell) in row.iter().enumerate() {
            let cell_ref = format!("{}{}", excel_column_letters(col_idx), row_num);
            match cell {
                CellValue::String(s) => {
                    sheet_data.push_str(&format!(
                        "<c r=\"{cell_ref}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{val}</t></is></c>",
                        cell_ref = cell_ref,
                        val = xml_escape(s)
                    ));
                }
                CellValue::Number(n) => {
                    if n.is_finite() {
                        sheet_data.push_str(&format!(
                            "<c r=\"{cell_ref}\"><v>{n}</v></c>",
                            cell_ref = cell_ref,
                            n = n
                        ));
                    } else {
                        sheet_data.push_str(&format!(
                            "<c r=\"{cell_ref}\" t=\"inlineStr\"><is><t>{val}</t></is></c>",
                            cell_ref = cell_ref,
                            val = xml_escape(&n.to_string())
                        ));
                    }
                }
                CellValue::Formula(raw_f) => {
                    let trimmed = raw_f.trim();
                    let formula_expr = trimmed.strip_prefix('=').unwrap_or(trimmed);
                    sheet_data.push_str(&format!(
                        "<c r=\"{cell_ref}\"><f>{expr}</f></c>",
                        cell_ref = cell_ref,
                        expr = xml_escape(formula_expr)
                    ));
                }
            }
        }
        sheet_data.push_str("</row>");
        row_num += 1;
    }
    sheet_data.push_str("</sheetData>");

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
{cols_xml}\
{sheet_data}\
</worksheet>"
    );
    (xml, sheet.rows.len())
}

/// Generates a genuine OpenXML `.xlsx` workbook supporting `String`, `Number(f64)`, and `Formula(String)` cells.
pub fn create_spreadsheet(
    workspace_root: &Path,
    spec: &SpreadsheetSpec,
) -> Result<SpreadsheetArtifact, String> {
    let normalized_out = if spec.output_path.trim().to_ascii_lowercase().ends_with(".xlsx") {
        spec.output_path.trim().to_string()
    } else {
        format!("{}.xlsx", spec.output_path.trim())
    };
    let (xlsx_abs, rel_xlsx) = resolve_confined_output_path(workspace_root, &normalized_out)?;

    let sheets: Vec<SheetSpec> = if spec.sheets.is_empty() {
        vec![SheetSpec {
            name: "Sheet1".to_string(),
            headers: vec!["Metric".to_string(), "Value".to_string()],
            rows: Vec::new(),
        }]
    } else {
        spec.sheets.clone()
    };

    let sheet_count = sheets.len();
    let mut entries: Vec<(String, String)> = Vec::with_capacity(8 + sheet_count);

    // 1. [Content_Types].xml
    let mut overrides = String::from(
        "<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>",
    );
    for i in 1..=sheet_count {
        overrides.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
            i
        ));
    }
    entries.push((
        "[Content_Types].xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
{overrides}\
</Types>"
        ),
    ));

    // 2. _rels/.rels
    entries.push((
        "_rels/.rels".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>\
</Relationships>"
            .to_string(),
    ));

    // 3. docProps
    entries.push((
        "docProps/core.xml".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
<dc:creator>Rursor Media Studio</dc:creator>\
</cp:coreProperties>"
            .to_string(),
    ));
    entries.push((
        "docProps/app.xml".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\
<Application>Rursor Media Studio</Application>\
</Properties>"
            .to_string(),
    ));

    // 4. xl/workbook.xml & xl/_rels/workbook.xml.rels
    let mut sheets_tag = String::from("<sheets>");
    let mut wb_rels = String::from(
        "<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
    );
    let mut used_names = Vec::with_capacity(sheet_count);
    let mut total_rows = 0usize;

    for (idx0, sheet) in sheets.iter().enumerate() {
        let idx1 = idx0 + 1;
        let mut sname = sanitize_excel_sheet_name(&sheet.name, idx1);
        if used_names.iter().any(|n: &String| n.eq_ignore_ascii_case(&sname)) {
            let suffix = format!("_{}", idx1);
            let keep = 31usize.saturating_sub(suffix.len());
            sname = format!("{}{}", sname.chars().take(keep).collect::<String>(), suffix);
        }
        used_names.push(sname.clone());

        sheets_tag.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rIdSheet{}\"/>",
            xml_escape(&sname),
            idx1,
            idx1
        ));
        wb_rels.push_str(&format!(
            "<Relationship Id=\"rIdSheet{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",
            idx1, idx1
        ));

        let (ws_xml, rows_in_sheet) = build_xlsx_worksheet_xml(sheet);
        total_rows += rows_in_sheet;
        entries.push((format!("xl/worksheets/sheet{}.xml", idx1), ws_xml));
    }
    sheets_tag.push_str("</sheets>");

    entries.push((
        "xl/workbook.xml".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
{sheets_tag}\
<calcPr calcId=\"191029\" fullCalcOnLoad=\"1\"/>\
</workbook>"
        ),
    ));
    entries.push((
        "xl/_rels/workbook.xml.rels".to_string(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{wb_rels}</Relationships>"
        ),
    ));

    // 5. xl/styles.xml (0 = Normal cell, 1 = Styled Obsidian Header cell)
    entries.push((
        "xl/styles.xml".to_string(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
<fonts count=\"2\">\
<font><sz val=\"11\"/><color rgb=\"FF0F172A\"/><name val=\"Calibri\"/></font>\
<font><b/><sz val=\"11\"/><color rgb=\"FFF8FAFC\"/><name val=\"Calibri\"/></font>\
</fonts>\
<fills count=\"3\">\
<fill><patternFill patternType=\"none\"/></fill>\
<fill><patternFill patternType=\"gray125\"/></fill>\
<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF131822\"/><bgColor indexed=\"64\"/></patternFill></fill>\
</fills>\
<borders count=\"2\">\
<border><left/><right/><top/><bottom/><diagonal/></border>\
<border>\
<left style=\"thin\"><color rgb=\"FF334155\"/></left>\
<right style=\"thin\"><color rgb=\"FF334155\"/></right>\
<top style=\"thin\"><color rgb=\"FF334155\"/></top>\
<bottom style=\"medium\"><color rgb=\"FF7C5CFF\"/></bottom>\
<diagonal/>\
</border>\
</borders>\
<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
<cellXfs count=\"2\">\
<xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\
<xf numFmtId=\"0\" fontId=\"1\" fillId=\"2\" borderId=\"1\" xfId=\"0\" applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\" applyAlignment=\"1\"><alignment vertical=\"center\"/></xf>\
</cellXfs>\
</styleSheet>"
            .to_string(),
    ));

    let bytes = package_openxml_zip(&entries, &xlsx_abs)?;

    Ok(SpreadsheetArtifact {
        xlsx_path: xlsx_abs.to_string_lossy().to_string(),
        relative_xlsx_path: rel_xlsx,
        sheet_count,
        row_count: total_rows,
        bytes,
    })
}

fn docx_paragraph_xml(text: &str, style_id: Option<&str>) -> String {
    let p_pr = match style_id {
        Some(sid) => format!("<w:pPr><w:pStyle w:val=\"{}\"/></w:pPr>", xml_escape(sid)),
        None => String::new(),
    };
    format!(
        "<w:p>{p_pr}<w:r><w:t xml:space=\"preserve\">{txt}</w:t></w:r></w:p>",
        p_pr = p_pr,
        txt = xml_escape(text)
    )
}

fn docx_bullet_xml(text: &str) -> String {
    format!(
        "<w:p>\
<w:pPr><w:numPr><w:ilvl w:val=\"0\"/><w:numId w:val=\"1\"/></w:numPr><w:spacing w:after=\"100\"/></w:pPr>\
<w:r><w:t xml:space=\"preserve\">{txt}</w:t></w:r>\
</w:p>",
        txt = xml_escape(text)
    )
}

fn docx_code_block_xml(code: &str) -> String {
    let mut runs = String::new();
    for (idx, line) in code.lines().enumerate() {
        if idx > 0 {
            runs.push_str("<w:r><w:br/></w:r>");
        }
        runs.push_str(&format!(
            "<w:r><w:rPr><w:rFonts w:ascii=\"Menlo\" w:hAnsi=\"Menlo\" w:cs=\"Menlo\"/><w:sz w:val=\"19\"/><w:color w:val=\"0F172A\"/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>",
            xml_escape(line)
        ));
    }
    format!(
        "<w:p>\
<w:pPr>\
<w:pBdr>\
<w:top w:val=\"single\" w:sz=\"6\" w:space=\"6\" w:color=\"CBD5E1\"/>\
<w:left w:val=\"single\" w:sz=\"18\" w:space=\"8\" w:color=\"7C5CFF\"/>\
<w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"6\" w:color=\"CBD5E1\"/>\
<w:right w:val=\"single\" w:sz=\"6\" w:space=\"6\" w:color=\"CBD5E1\"/>\
</w:pBdr>\
<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F1F5F9\"/>\
<w:spacing w:before=\"120\" w:after=\"160\" w:line=\"260\" w:lineRule=\"auto\"/>\
</w:pPr>\
{runs}\
</w:p>"
    )
}

fn docx_table_xml(table: &DocTable) -> String {
    let col_count = table
        .rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(table.headers.len())
        .max(1);
    let col_w_dxa = (9_360 / col_count).max(900);

    let mut grid = String::from("<w:tblGrid>");
    for _ in 0..col_count {
        grid.push_str(&format!("<w:gridCol w:w=\"{}\"/>", col_w_dxa));
    }
    grid.push_str("</w:tblGrid>");

    let mut rows_xml = String::new();
    if !table.headers.is_empty() {
        rows_xml.push_str("<w:tr>");
        for col_idx in 0..col_count {
            let text = table.headers.get(col_idx).map(String::as_str).unwrap_or("");
            rows_xml.push_str(&format!(
                "<w:tc>\
<w:tcPr><w:tcW w:w=\"{w}\" w:type=\"dxa\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"131822\"/></w:tcPr>\
<w:p><w:r><w:rPr><w:b/><w:color w:val=\"F8FAFC\"/></w:rPr><w:t xml:space=\"preserve\">{txt}</w:t></w:r></w:p>\
</w:tc>",
                w = col_w_dxa,
                txt = xml_escape(text)
            ));
        }
        rows_xml.push_str("</w:tr>");
    }

    for row in &table.rows {
        rows_xml.push_str("<w:tr>");
        for col_idx in 0..col_count {
            let text = row.get(col_idx).map(String::as_str).unwrap_or("");
            rows_xml.push_str(&format!(
                "<w:tc>\
<w:tcPr><w:tcW w:w=\"{w}\" w:type=\"dxa\"/></w:tcPr>\
<w:p><w:r><w:t xml:space=\"preserve\">{txt}</w:t></w:r></w:p>\
</w:tc>",
                w = col_w_dxa,
                txt = xml_escape(text)
            ));
        }
        rows_xml.push_str("</w:tr>");
    }

    format!(
        "<w:tbl>\
<w:tblPr>\
<w:tblW w:w=\"9360\" w:type=\"dxa\"/>\
<w:tblBorders>\
<w:top w:val=\"single\" w:sz=\"6\" w:space=\"0\" w:color=\"CBD5E1\"/>\
<w:left w:val=\"single\" w:sz=\"6\" w:space=\"0\" w:color=\"CBD5E1\"/>\
<w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"0\" w:color=\"CBD5E1\"/>\
<w:right w:val=\"single\" w:sz=\"6\" w:space=\"0\" w:color=\"CBD5E1\"/>\
<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"E2E8F0\"/>\
<w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"E2E8F0\"/>\
</w:tblBorders>\
</w:tblPr>\
{grid}\
{rows_xml}\
</w:tbl>"
    )
}

/// Generates a genuine OpenXML `.docx` document with styled headings, paragraphs,
/// bulleted lists, shaded monospace code blocks, and formatted tables.
pub fn create_document(
    workspace_root: &Path,
    spec: &DocumentSpec,
) -> Result<DocumentArtifact, String> {
    let normalized_out = if spec.output_path.trim().to_ascii_lowercase().ends_with(".docx") {
        spec.output_path.trim().to_string()
    } else {
        format!("{}.docx", spec.output_path.trim())
    };
    let (docx_abs, rel_docx) = resolve_confined_output_path(workspace_root, &normalized_out)?;

    let mut body_xml = String::with_capacity(4096);
    if !spec.title.trim().is_empty() {
        body_xml.push_str(&docx_paragraph_xml(spec.title.trim(), Some("Title")));
    }
    if !spec.subtitle.trim().is_empty() {
        body_xml.push_str(&docx_paragraph_xml(spec.subtitle.trim(), Some("Subtitle")));
    }

    for section in &spec.sections {
        if !section.heading.trim().is_empty() {
            body_xml.push_str(&docx_paragraph_xml(section.heading.trim(), Some("Heading1")));
        }
        for p in &section.paragraphs {
            body_xml.push_str(&docx_paragraph_xml(p, Some("Normal")));
        }
        for b in &section.bullets {
            body_xml.push_str(&docx_bullet_xml(b));
        }
        if let Some(code) = section.code_block.as_deref().filter(|s| !s.is_empty()) {
            body_xml.push_str(&docx_code_block_xml(code));
        }
        if let Some(tbl) = &section.table {
            body_xml.push_str(&docx_table_xml(tbl));
            body_xml.push_str("<w:p/>");
        }
    }

    body_xml.push_str(
        "<w:sectPr>\
<w:pgSz w:w=\"12240\" w:h=\"15840\"/>\
<w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/>\
</w:sectPr>",
    );

    let entries = vec![
        (
            "[Content_Types].xml".to_string(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\
</Types>"
                .to_string(),
        ),
        (
            "_rels/.rels".to_string(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>\
</Relationships>"
                .to_string(),
        ),
        (
            "docProps/core.xml".to_string(),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
<dc:title>{}</dc:title>\
<dc:creator>Rursor Media Studio</dc:creator>\
</cp:coreProperties>",
                xml_escape(&spec.title)
            ),
        ),
        (
            "docProps/app.xml".to_string(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\
<Application>Rursor Media Studio</Application>\
</Properties>"
                .to_string(),
        ),
        (
            "word/_rels/document.xml.rels".to_string(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
<Relationship Id=\"rIdNumbering\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>\
</Relationships>"
                .to_string(),
        ),
        (
            "word/styles.xml".to_string(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\">\
<w:name w:val=\"Normal\"/>\
<w:pPr><w:spacing w:after=\"140\" w:line=\"276\" w:lineRule=\"auto\"/></w:pPr>\
<w:rPr><w:rFonts w:ascii=\"Helvetica Neue\" w:hAnsi=\"Helvetica Neue\"/><w:sz w:val=\"22\"/><w:color w:val=\"0F172A\"/></w:rPr>\
</w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Title\">\
<w:name w:val=\"Title\"/>\
<w:pPr><w:spacing w:before=\"0\" w:after=\"80\"/></w:pPr>\
<w:rPr><w:b/><w:sz w:val=\"52\"/><w:color w:val=\"0B0D12\"/></w:rPr>\
</w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Subtitle\">\
<w:name w:val=\"Subtitle\"/>\
<w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"12\" w:space=\"10\" w:color=\"7C5CFF\"/></w:pBdr><w:spacing w:after=\"280\"/></w:pPr>\
<w:rPr><w:sz w:val=\"26\"/><w:color w:val=\"475569\"/></w:rPr>\
</w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\">\
<w:name w:val=\"heading 1\"/>\
<w:pPr><w:spacing w:before=\"280\" w:after=\"120\"/></w:pPr>\
<w:rPr><w:b/><w:sz w:val=\"32\"/><w:color w:val=\"1E1B4B\"/></w:rPr>\
</w:style>\
</w:styles>"
                .to_string(),
        ),
        (
            "word/numbering.xml".to_string(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:abstractNum w:abstractNumId=\"0\">\
<w:lvl w:ilvl=\"0\">\
<w:start w:val=\"1\"/>\
<w:numFmt w:val=\"bullet\"/>\
<w:lvlText w:val=\"&#8226;\"/>\
<w:lvlJc w:val=\"left\"/>\
<w:pPr><w:ind w:left=\"540\" w:hanging=\"280\"/></w:pPr>\
</w:lvl>\
</w:abstractNum>\
<w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num>\
</w:numbering>"
                .to_string(),
        ),
        (
            "word/document.xml".to_string(),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<w:body>{body_xml}</w:body>\
</w:document>"
            ),
        ),
    ];

    let bytes = package_openxml_zip(&entries, &docx_abs)?;

    Ok(DocumentArtifact {
        docx_path: docx_abs.to_string_lossy().to_string(),
        relative_docx_path: rel_docx,
        section_count: spec.sections.len(),
        bytes,
    })
}

// ============================================================================
// 5. Turnkey JSON-RPC Dispatcher Helper for Sidecar Integration
// ============================================================================

fn unwrap_spec_payload<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, String> {
    let target = params.get("spec").unwrap_or(params);
    serde_json::from_value::<T>(target.clone())
        .map_err(|e| format!("invalid media studio spec payload: {}", e))
}

/// Optional drop-in JSON-RPC router for `sidecar/src/main.rs`.
/// Returns `Some(Result<Value, String>)` when `method` is one of the 6 Media Studio RPC methods.
pub async fn dispatch_media_studio_rpc(
    method: &str,
    params: &Value,
    workspace_root: &Path,
    http: &Client,
    api_key: Option<&str>,
) -> Option<Result<Value, String>> {
    match method {
        "create_presentation" => Some((|| {
            let spec: PresentationSpec = unwrap_spec_payload(params)?;
            let root = workspace_root.to_path_buf();
            let artifact = create_presentation(&root, &spec)?;
            serde_json::to_value(artifact).map_err(|e| e.to_string())
        })()),
        "render_explainer_video" => Some(
            async {
                let spec: VideoStoryboardSpec = unwrap_spec_payload(params)?;
                let artifact = render_explainer_video(workspace_root, &spec).await?;
                serde_json::to_value(artifact).map_err(|e| e.to_string())
            }
            .await,
        ),
        "generate_veo_video" => Some(
            async {
                let spec: VeoVideoSpec = unwrap_spec_payload(params)?;
                let key = api_key
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "Gemini API key is not configured".to_string())?;
                let artifact = generate_veo_video(http, key, workspace_root, &spec).await?;
                serde_json::to_value(artifact).map_err(|e| e.to_string())
            }
            .await,
        ),
        "generate_image_asset" => Some(
            async {
                let spec: ImageAssetSpec = unwrap_spec_payload(params)?;
                let artifact = generate_image_asset(http, api_key, workspace_root, &spec).await?;
                serde_json::to_value(artifact).map_err(|e| e.to_string())
            }
            .await,
        ),
        "create_spreadsheet" => Some((|| {
            let spec: SpreadsheetSpec = unwrap_spec_payload(params)?;
            let artifact = create_spreadsheet(workspace_root, &spec)?;
            serde_json::to_value(artifact).map_err(|e| e.to_string())
        })()),
        "create_document" => Some((|| {
            let spec: DocumentSpec = unwrap_spec_payload(params)?;
            let artifact = create_document(workspace_root, &spec)?;
            serde_json::to_value(artifact).map_err(|e| e.to_string())
        })()),
        _ => None,
    }
}

// ============================================================================
// 6. Comprehensive Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_unzip_test_passes(archive_path: &Path) {
        let out = StdCommand::new("/usr/bin/unzip")
            .arg("-t")
            .arg(archive_path)
            .output()
            .expect("failed to execute /usr/bin/unzip -t");
        assert!(
            out.status.success(),
            "unzip -t failed on '{}': {}",
            archive_path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn list_archive_entries(archive_path: &Path) -> Vec<String> {
        let out = StdCommand::new("/usr/bin/unzip")
            .arg("-Z1")
            .arg(archive_path)
            .output()
            .expect("failed to execute /usr/bin/unzip -Z1");
        assert!(out.status.success(), "unzip -Z1 failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn read_archive_entry(archive_path: &Path, entry_name: &str) -> String {
        // Info-ZIP treats `[` and `]` as glob character classes unless escaped as `\[` / `\]`
        let escaped_pattern = entry_name.replace('[', "\\[").replace(']', "\\]");
        let out = StdCommand::new("/usr/bin/unzip")
            .arg("-p")
            .arg(archive_path)
            .arg(&escaped_pattern)
            .output()
            .expect("failed to execute /usr/bin/unzip -p");
        assert!(
            out.status.success(),
            "missing or unreadable entry '{}' in '{}': {}",
            entry_name,
            archive_path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("archive entry is not valid UTF-8")
    }

    #[test]
    fn test_pptx_and_html_deck_generation_with_xml_escaping() {
        let ws = TempDirGuard::create("test_pptx").unwrap();
        let spec = PresentationSpec {
            title: "Rursor & <Obsidian> \"Architecture\"".to_string(),
            subtitle: "Sub-millisecond AST & Multimodal Studio".to_string(),
            author: "Core Systems <Team>".to_string(),
            theme: "obsidian".to_string(),
            output_path: "decks/architecture.pptx".to_string(),
            slides: vec![
                SlideSpec {
                    layout: "title".to_string(),
                    title: "Wave C: Media & \"OpenXML\" <Engine>".to_string(),
                    subtitle: "100% Native Rust & TypeScript".to_string(),
                    bullets: vec!["Zero external Cargo deps & fast <zip>".to_string()],
                    speaker_notes: Some("Welcome note with & < > \" chars".to_string()),
                    ..SlideSpec::default()
                },
                SlideSpec {
                    layout: "metrics".to_string(),
                    title: "Latency & Throughput KPIs".to_string(),
                    subtitle: "P99 < 5ms & Throughput > 10k/s".to_string(),
                    metrics: vec![
                        SlideMetric {
                            label: "Cold Start".to_string(),
                            value: "4.2ms".to_string(),
                            delta: "-62% faster".to_string(),
                        },
                        SlideMetric {
                            label: "Tree-Sitter Gate".to_string(),
                            value: "1.1ms".to_string(),
                            delta: "+99.9% valid".to_string(),
                        },
                    ],
                    bullets: vec!["Measured on Apple M3 Max & M4 Pro".to_string()],
                    ..SlideSpec::default()
                },
                SlideSpec {
                    layout: "code".to_string(),
                    title: "Rust Sidecar Snippet".to_string(),
                    subtitle: "Safe Confinement & OpenXML".to_string(),
                    bullets: vec!["Rejects ../ traversal & escapes <tags>".to_string()],
                    code: Some(
                        "pub fn check(x: i32, y: i32) -> bool {\n    x < y && y > 0 && \"ok\" != \"err\"\n}"
                            .to_string(),
                    ),
                    ..SlideSpec::default()
                },
            ],
        };

        let artifact = create_presentation(&ws.path, &spec).expect("create_presentation failed");
        assert_eq!(artifact.slide_count, 3);
        assert!(artifact.bytes > 1_000);

        let pptx_path = PathBuf::from(&artifact.pptx_path);
        let html_path = PathBuf::from(&artifact.html_deck_path);
        assert!(pptx_path.exists(), "expected .pptx to exist");
        assert!(html_path.exists(), "expected .deck.html to exist");
        assert!(html_path.to_string_lossy().ends_with("architecture.deck.html"));

        assert_unzip_test_passes(&pptx_path);

        let entries = list_archive_entries(&pptx_path);
        assert!(entries.iter().any(|e| e == "[Content_Types].xml"));
        assert!(entries.iter().any(|e| e == "_rels/.rels"));
        assert!(entries.iter().any(|e| e == "ppt/presentation.xml"));
        assert!(entries.iter().any(|e| e == "ppt/slides/slide1.xml"));
        assert!(entries.iter().any(|e| e == "ppt/slides/slide2.xml"));
        assert!(entries.iter().any(|e| e == "ppt/slides/slide3.xml"));

        let content_types = read_archive_entry(&pptx_path, "[Content_Types].xml");
        assert!(content_types.contains("/ppt/slides/slide1.xml"));
        assert!(content_types.contains("/ppt/slides/slide2.xml"));
        assert!(content_types.contains("/ppt/slides/slide3.xml"));

        let s1 = read_archive_entry(&pptx_path, "ppt/slides/slide1.xml");
        let s2 = read_archive_entry(&pptx_path, "ppt/slides/slide2.xml");
        let s3 = read_archive_entry(&pptx_path, "ppt/slides/slide3.xml");

        assert!(s1.contains("0B0D12"), "Obsidian background #0B0D12 missing");
        assert!(
            s1.contains("Wave C: Media &amp; &quot;OpenXML&quot; &lt;Engine&gt;"),
            "XML special characters were not properly escaped in slide1.xml"
        );
        assert!(s2.contains("4.2ms"));
        assert!(s2.contains("P99 &lt; 5ms &amp; Throughput &gt; 10k/s"));
        assert!(s3.contains("x &lt; y &amp;&amp; y &gt; 0 &amp;&amp; &quot;ok&quot;"));

        let html = fs::read_to_string(&html_path).unwrap();
        assert!(html.contains("ArrowRight"));
        assert!(html.contains("Fullscreen (F)"));
        assert!(html.contains("Notes (N)"));
        assert!(html.contains("4.2ms"));
    }

    #[test]
    fn test_pptx_minimal_and_two_column_quote_layouts() {
        let ws = TempDirGuard::create("test_pptx_min").unwrap();
        let spec = PresentationSpec {
            title: "Minimal Theme Deck".to_string(),
            subtitle: "Clean White Canvas".to_string(),
            author: "Design Studio".to_string(),
            theme: "minimal".to_string(),
            output_path: "minimal_deck.pptx".to_string(),
            slides: vec![
                SlideSpec {
                    layout: "two_column".to_string(),
                    title: "Before vs After".to_string(),
                    bullets: vec!["Legacy Cloud Only".to_string()],
                    right_bullets: vec!["Hybrid Local + Cloud Studio".to_string()],
                    ..SlideSpec::default()
                },
                SlideSpec {
                    layout: "quote".to_string(),
                    title: "Design Principle".to_string(),
                    bullets: vec![
                        "Simplicity is prerequisite for reliability.".to_string(),
                        "Edsger W. Dijkstra".to_string(),
                    ],
                    ..SlideSpec::default()
                },
            ],
        };

        let art = create_presentation(&ws.path, &spec).unwrap();
        let pptx_path = PathBuf::from(&art.pptx_path);
        assert_unzip_test_passes(&pptx_path);
        let s1 = read_archive_entry(&pptx_path, "ppt/slides/slide1.xml");
        assert!(s1.contains("FFFFFF"), "Minimal background #FFFFFF missing");
        assert!(s1.contains("Hybrid Local + Cloud Studio"));
    }

    #[test]
    fn test_xlsx_spreadsheet_generation_with_numbers_and_sum_formula() {
        let ws = TempDirGuard::create("test_xlsx").unwrap();
        let spec = SpreadsheetSpec {
            output_path: "finance/q4_metrics.xlsx".to_string(),
            sheets: vec![SheetSpec {
                name: "Q4 Revenue & Costs".to_string(),
                headers: vec!["Region & Segment".to_string(), "ARR ($K)".to_string()],
                rows: vec![
                    vec![
                        CellValue::String("North America <Enterprise>".to_string()),
                        CellValue::Number(420.5),
                    ],
                    vec![
                        CellValue::String("EMEA & APAC".to_string()),
                        CellValue::Number(315.25),
                    ],
                    vec![
                        CellValue::String("Total ARR".to_string()),
                        CellValue::Formula("=SUM(B2:B3)".to_string()),
                    ],
                ],
            }],
        };

        let art = create_spreadsheet(&ws.path, &spec).expect("create_spreadsheet failed");
        assert_eq!(art.sheet_count, 1);
        assert_eq!(art.row_count, 3);

        let xlsx_path = PathBuf::from(&art.xlsx_path);
        assert!(xlsx_path.exists());
        assert_unzip_test_passes(&xlsx_path);

        let sheet1 = read_archive_entry(&xlsx_path, "xl/worksheets/sheet1.xml");
        assert!(
            sheet1.contains("<f>SUM(B2:B3)</f>"),
            "expected <f>SUM(B2:B3)</f> in sheet1.xml, got: {}",
            sheet1
        );
        assert!(sheet1.contains("<v>420.5</v>"));
        assert!(sheet1.contains("North America &lt;Enterprise&gt;"));
    }

    #[test]
    fn test_cell_value_flexible_json_deserialization() {
        let raw_json = json!({
            "outputPath": "data/flex.xlsx",
            "sheets": [{
                "name": "Pipeline",
                "headers": ["Item", "Count"],
                "rows": [
                    ["Requests", 1280],
                    [{"type": "string", "value": "Total"}, "=SUM(B2:B2)"]
                ]
            }]
        });
        let spec: SpreadsheetSpec = serde_json::from_value(raw_json).unwrap();
        assert_eq!(spec.sheets[0].rows[0][1], CellValue::Number(1280.0));
        assert_eq!(
            spec.sheets[0].rows[1][1],
            CellValue::Formula("=SUM(B2:B2)".to_string())
        );
    }

    #[test]
    fn test_docx_generation_with_headings_bullets_code_and_table() {
        let ws = TempDirGuard::create("test_docx").unwrap();
        let spec = DocumentSpec {
            title: "Rursor RFC-042: Media & OpenXML Studio".to_string(),
            subtitle: "Architecture & Security Review".to_string(),
            output_path: "docs/rfc_042.docx".to_string(),
            sections: vec![DocSection {
                heading: "1. Security & Path Confinement".to_string(),
                paragraphs: vec![
                    "All output artifacts are strictly confined inside workspace_root & validated."
                        .to_string(),
                ],
                bullets: vec![
                    "Rejects '../' parent traversal".to_string(),
                    "Escapes XML entities (& < > \")".to_string(),
                ],
                code_block: Some("let (abs, rel) = resolve_confined_output_path(root, out)?;".to_string()),
                table: Some(DocTable {
                    headers: vec!["Format".to_string(), "Validator".to_string()],
                    rows: vec![
                        vec![".pptx".to_string(), "/usr/bin/unzip -t".to_string()],
                        vec![".mp4".to_string(), "ISO BMFF ftyp".to_string()],
                    ],
                }),
            }],
        };

        let art = create_document(&ws.path, &spec).expect("create_document failed");
        assert_eq!(art.section_count, 1);
        let docx_path = PathBuf::from(&art.docx_path);
        assert!(docx_path.exists());
        assert_unzip_test_passes(&docx_path);

        let doc_xml = read_archive_entry(&docx_path, "word/document.xml");
        assert!(doc_xml.starts_with("<?xml version=\"1.0\""));
        assert!(doc_xml.ends_with("</w:document>"));
        assert!(doc_xml.contains("1. Security &amp; Path Confinement"));
        assert!(doc_xml.contains("<w:tbl>"));
        assert!(doc_xml.contains("ISO BMFF ftyp"));
        assert!(doc_xml.contains("resolve_confined_output_path"));
    }

    #[tokio::test]
    async fn test_local_explainer_mp4_video_generation_ftyp_header() {
        let ws = TempDirGuard::create("test_mp4").unwrap();
        let spec = VideoStoryboardSpec {
            title: "Rursor Architecture Walkthrough".to_string(),
            output_path: "videos/walkthrough.mp4".to_string(),
            width: 640,
            height: 360,
            fps: 24,
            voice: Some("none".to_string()),
            scenes: vec![
                VideoSceneSpec {
                    heading: "Sub-2ms Syntax Gate".to_string(),
                    subheading: "Tree-sitter AST Verification in Rust".to_string(),
                    bullets: vec![
                        "Blocks broken edits before touching disk".to_string(),
                        "Supports Rust, TypeScript, Python, Go".to_string(),
                    ],
                    code_snippet: Some("pub fn validate(src: &str) -> bool {\n    true\n}".to_string()),
                    narration: Some("Rursor validates every edit in under two milliseconds.".to_string()),
                    duration_secs: 0.8,
                    accent_hex: Some("#7C5CFF".to_string()),
                },
                VideoSceneSpec {
                    heading: "Multimodal Studio".to_string(),
                    subheading: "Native Decks, Spreadsheets & Video".to_string(),
                    bullets: vec!["H.264 yuv420p MP4 with faststart".to_string()],
                    code_snippet: None,
                    narration: None,
                    duration_secs: 0.8,
                    accent_hex: Some("#2AD8A7".to_string()),
                },
            ],
        };

        let art = render_explainer_video(&ws.path, &spec)
            .await
            .expect("render_explainer_video failed");
        assert_eq!(art.scene_count, 2);
        assert_eq!(art.width, 640);
        assert_eq!(art.height, 360);
        assert!(art.bytes > 1_000, "expected non-trivial MP4 size, got {}", art.bytes);

        let mp4_bytes = fs::read(&art.video_path).expect("failed to read generated MP4");
        assert!(mp4_bytes.len() >= 16);
        assert_eq!(
            &mp4_bytes[4..8],
            b"ftyp",
            "expected valid MP4 ISO BMFF 'ftyp' box header at bytes 4..8"
        );
    }

    #[tokio::test]
    async fn test_local_svg_and_diagram_card_png_generation() {
        let ws = TempDirGuard::create("test_png").unwrap();
        let http = Client::new();

        let svg_spec = ImageAssetSpec {
            prompt: "Architecture Flow".to_string(),
            output_path: "assets/flow_diagram.png".to_string(),
            mode: Some("local".to_string()),
            width: Some(480),
            height: Some(270),
            svg_markup: Some(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"480\" height=\"270\">\
                <rect x=\"0\" y=\"0\" width=\"480\" height=\"270\" fill=\"#0B0D12\"/>\
                <rect x=\"30\" y=\"60\" width=\"170\" height=\"120\" rx=\"14\" fill=\"#131822\" stroke=\"#7C5CFF\" stroke-width=\"2\"/>\
                <line x1=\"200\" y1=\"120\" x2=\"280\" y2=\"120\" stroke=\"#2AD8A7\" stroke-width=\"3\"/>\
                <rect x=\"280\" y=\"60\" width=\"170\" height=\"120\" rx=\"14\" fill=\"#131822\" stroke=\"#2AD8A7\" stroke-width=\"2\"/>\
                <text x=\"52\" y=\"125\" font-size=\"18\" fill=\"#F3F5F9\">Extension</text>\
                <text x=\"312\" y=\"125\" font-size=\"18\" fill=\"#F3F5F9\">Sidecar</text>\
                </svg>"
                    .to_string(),
            ),
            ..ImageAssetSpec::default()
        };

        let art = generate_image_asset(&http, None, &ws.path, &svg_spec)
            .await
            .expect("SVG->PNG generation failed");
        let png_bytes = fs::read(&art.image_path).unwrap();
        assert_eq!(&png_bytes[0..8], b"\x89PNG\r\n\x1a\n");
        assert!(art.svg_path.is_some());
    }

    #[test]
    fn test_path_traversal_rejected_across_all_generators() {
        let ws = TempDirGuard::create("test_confinement").unwrap();

        let bad_paths = [
            "../outside.pptx",
            "decks/../../outside.pptx",
            "/etc/outside.pptx",
            "",
        ];

        for bad in bad_paths {
            let err = resolve_confined_output_path(&ws.path, bad);
            assert!(
                err.is_err(),
                "expected path '{}' to be rejected by workspace confinement",
                bad
            );
        }

        let pptx_err = create_presentation(
            &ws.path,
            &PresentationSpec {
                title: "Escape".to_string(),
                output_path: "../outside.pptx".to_string(),
                ..PresentationSpec::default()
            },
        );
        assert!(pptx_err.is_err());

        let xlsx_err = create_spreadsheet(
            &ws.path,
            &SpreadsheetSpec {
                output_path: "../outside.xlsx".to_string(),
                sheets: vec![],
            },
        );
        assert!(xlsx_err.is_err());

        let docx_err = create_document(
            &ws.path,
            &DocumentSpec {
                title: "Escape".to_string(),
                output_path: "../../outside.docx".to_string(),
                ..DocumentSpec::default()
            },
        );
        assert!(docx_err.is_err());
    }

    #[test]
    fn test_base64_roundtrip_and_data_uri_decoding() {
        let sample = b"\x00\x01\x02Rursor Media Studio \xFF\xFE\x89PNG";
        let encoded = base64_encode(sample);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, sample);

        let with_prefix = format!("data:image/png;base64,\n  {}\n", encoded);
        let decoded_uri = base64_decode(&with_prefix).unwrap();
        assert_eq!(decoded_uri, sample);
    }

    #[test]
    fn test_excel_column_letters_conversion() {
        assert_eq!(excel_column_letters(0), "A");
        assert_eq!(excel_column_letters(25), "Z");
        assert_eq!(excel_column_letters(26), "AA");
        assert_eq!(excel_column_letters(27), "AB");
        assert_eq!(excel_column_letters(51), "AZ");
        assert_eq!(excel_column_letters(52), "BA");
    }

    #[tokio::test]
    async fn test_rpc_dispatcher_roundtrip() {
        let ws = TempDirGuard::create("test_rpc").unwrap();
        let http = Client::new();
        let params = json!({
            "spec": {
                "outputPath": "reports/summary.xlsx",
                "sheets": [{
                    "name": "Summary",
                    "headers": ["Metric", "Count"],
                    "rows": [["Decks", 4], ["Total", "=SUM(B2:B2)"]]
                }]
            }
        });
        let res = dispatch_media_studio_rpc("create_spreadsheet", &params, &ws.path, &http, None)
            .await
            .expect("expected method to match")
            .expect("expected ok spreadsheet result");
        assert_eq!(res["sheetCount"], 1);
        assert!(Path::new(res["xlsxPath"].as_str().unwrap()).exists());
    }
}
