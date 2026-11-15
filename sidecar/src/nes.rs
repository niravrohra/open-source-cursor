use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Predicted next edit or cross-file navigation target returned by the NES engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NesPrediction {
    /// One of `"edit"`, `"insert"`, `"jump"`, or `"none"`.
    pub kind: String,
    /// Workspace-relative (or target) file path for the predicted edit or jump.
    #[serde(alias = "target_path", default)]
    pub target_path: String,
    /// 1-based inclusive start line of the target region.
    #[serde(alias = "start_line", default = "default_one")]
    pub start_line: usize,
    /// 1-based inclusive end line of the target region.
    #[serde(alias = "end_line", default = "default_one")]
    pub end_line: usize,
    /// Replacement or inserted text (empty for pure `"jump"` or `"none"`).
    #[serde(default)]
    pub replacement: String,
    /// Short human-readable explanation of why this next edit or jump is suggested.
    #[serde(default)]
    pub reason: String,
    /// Optional 1-based line number to jump to when `kind == "jump"` or a follow-up jump exists.
    #[serde(alias = "jump_to_line", default)]
    pub jump_to_line: Option<usize>,
}

/// Candidate caller or reference site in another workspace file that may need an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedEditSite {
    /// Workspace-relative file path containing the reference.
    pub path: String,
    /// 1-based line number of the reference.
    pub line: usize,
    /// Trimmed single-line preview of the matching source line.
    pub preview: String,
}

fn default_one() -> usize {
    1
}

/// Builds the `(system_prompt, user_prompt)` pair for diff-based Next-Edit Prediction
/// and cross-file Tab-to-Jump inference.
///
/// `focus_start_line` is 1-based and corresponds to the first line of `focus_block`.
#[allow(clippy::too_many_arguments)]
pub fn build_nes_prompt(
    file_path: &str,
    language_id: &str,
    window_before: &str,
    focus_block: &str,
    window_after: &str,
    focus_start_line: usize,
    recent_edits: &[String],
    related_sites: &[RelatedEditSite],
) -> (String, String) {
    let system_prompt = "\
You are Rursor's Next-Edit Prediction (NES) and Tab-to-Jump engine.
Analyze the developer's current focus region, recent edits, and related cross-file reference sites, then predict the single highest-confidence next edit or navigation jump.

Respond with ONLY a single valid JSON object (no markdown fences, no commentary) using this exact schema:
{
  \"kind\": \"edit\" | \"insert\" | \"jump\" | \"none\",
  \"targetPath\": \"<relative file path>\",
  \"startLine\": <1-based inclusive integer>,
  \"endLine\": <1-based inclusive integer>,
  \"replacement\": \"<exact replacement or inserted code, no line numbers or fences>\",
  \"reason\": \"<concise 1-sentence reason>\",
  \"jumpToLine\": <1-based integer or null>
}

Rules:
- Use \"edit\" to replace lines [startLine..endLine] in targetPath with replacement, preserving indentation.
- Use \"insert\" to insert new code at startLine (with endLine = startLine) without deleting existing lines.
- Use \"jump\" when the next logical step is to navigate to another line or related file (set targetPath and jumpToLine; replacement may be empty or contain the suggested edit at that site).
- Use \"none\" when no confident next edit or jump is warranted.
- Line numbers are 1-based and inclusive.";

    let focus_line_count = focus_block.lines().count().max(1);
    let focus_end_line = focus_start_line.saturating_add(focus_line_count - 1);

    let mut recent_section = String::new();
    if !recent_edits.is_empty() {
        recent_section.push_str("\n<recent_edits>\n");
        for edit in recent_edits.iter().take(6) {
            if !edit.trim().is_empty() {
                recent_section.push_str("- ");
                recent_section.push_str(edit.trim());
                recent_section.push('\n');
            }
        }
        recent_section.push_str("</recent_edits>\n");
    }

    let mut related_section = String::new();
    if !related_sites.is_empty() {
        related_section.push_str("\n<related_edit_sites>\n");
        for site in related_sites {
            related_section.push_str(&format!(
                "- {}:{} | {}\n",
                site.path, site.line, site.preview
            ));
        }
        related_section.push_str("</related_edit_sites>\n");
    }

    let user_prompt = format!(
        "File: {file_path} ({language_id})\nFocus range: lines {focus_start_line}-{focus_end_line}\n{recent_section}{related_section}\n<window_before>\n{window_before}\n</window_before>\n\n<focus_block start_line=\"{focus_start_line}\" end_line=\"{focus_end_line}\">\n{focus_block}\n</focus_block>\n\n<window_after>\n{window_after}\n</window_after>"
    );

    (system_prompt.to_string(), user_prompt)
}

/// Safely parses a raw model response (plain JSON, fenced JSON, or prose-wrapped JSON)
/// into a validated [`NesPrediction`].
///
/// Returns `None` if no valid prediction JSON is found or if required invariants fail.
pub fn parse_nes_response(
    raw: &str,
    default_path: &str,
    default_start: usize,
    default_end: usize,
) -> Option<NesPrediction> {
    let candidate = extract_json_object(raw)?;
    let obj = serde_json::from_str::<serde_json::Value>(&candidate).ok()?;
    let map = obj.as_object()?;

    let kind_raw = map.get("kind")?.as_str()?.trim().to_ascii_lowercase();
    if !matches!(kind_raw.as_str(), "edit" | "insert" | "jump" | "none") {
        return None;
    }

    let target_path = map
        .get("targetPath")
        .or_else(|| map.get("target_path"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_path)
        .to_string();

    let parsed_start = map
        .get("startLine")
        .or_else(|| map.get("start_line"))
        .and_then(value_to_usize);
    let parsed_end = map
        .get("endLine")
        .or_else(|| map.get("end_line"))
        .and_then(value_to_usize);

    let normalized_default_start = default_start.max(1);
    let normalized_default_end = default_end.max(normalized_default_start);

    let start_line = parsed_start.unwrap_or(normalized_default_start).max(1);
    let mut end_line = match (parsed_start, parsed_end) {
        (_, Some(e)) => e.max(1),
        (Some(_), None) => start_line,
        (None, None) => normalized_default_end,
    };
    if end_line < start_line {
        end_line = start_line;
    }

    let has_replacement_key = map.contains_key("replacement");
    let replacement = map
        .get("replacement")
        .and_then(|v| v.as_str())
        .map(|s| s.replace("\r\n", "\n"))
        .unwrap_or_default();

    let reason = map
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let mut jump_to_line = map
        .get("jumpToLine")
        .or_else(|| map.get("jump_to_line"))
        .and_then(value_to_usize)
        .map(|n| n.max(1));

    if kind_raw == "insert" && replacement.is_empty() {
        return None;
    }
    if kind_raw == "edit" && !has_replacement_key {
        return None;
    }
    if kind_raw == "jump" && jump_to_line.is_none() {
        jump_to_line = Some(start_line);
    }

    Some(NesPrediction {
        kind: kind_raw,
        target_path,
        start_line,
        end_line,
        replacement,
        reason,
        jump_to_line,
    })
}

/// Scans `files` (`(rel_path, content)`) for callers or references related to `changed_hint`,
/// excluding `current_rel_path` and returning up to `limit` ranked [`RelatedEditSite`] hits.
pub fn find_related_edit_sites(
    changed_hint: &str,
    current_rel_path: &str,
    files: &[(String, String)],
    limit: usize,
) -> Vec<RelatedEditSite> {
    if limit == 0 || changed_hint.trim().is_empty() {
        return Vec::new();
    }

    let symbols = extract_candidate_symbols(changed_hint);
    if symbols.is_empty() {
        return Vec::new();
    }

    let normalized_current = normalize_rel_path(current_rel_path);
    let mut scored_hits: Vec<(usize, RelatedEditSite)> = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::new();

    for (path, content) in files {
        if normalize_rel_path(path) == normalized_current {
            continue;
        }

        for (idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let mut best_score = 0usize;
            for (symbol, base_weight) in &symbols {
                if contains_whole_word(line, symbol) {
                    let mut score = *base_weight;
                    if is_call_or_type_usage(line, symbol) {
                        score += 3;
                    }
                    if is_import_line(trimmed) {
                        score = score.saturating_sub(1);
                    }
                    if score > best_score {
                        best_score = score;
                    }
                }
            }

            if best_score > 0 {
                let line_no = idx + 1;
                if seen.insert((path.clone(), line_no)) {
                    scored_hits.push((
                        best_score,
                        RelatedEditSite {
                            path: path.clone(),
                            line: line_no,
                            preview: truncate_chars(trimmed, 160),
                        },
                    ));
                }
            }
        }
    }

    scored_hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.line.cmp(&b.1.line))
    });

    scored_hits.into_iter().take(limit).map(|(_, site)| site).collect()
}

fn value_to_usize(val: &serde_json::Value) -> Option<usize> {
    if let Some(n) = val.as_u64() {
        return usize::try_from(n).ok();
    }
    if let Some(n) = val.as_i64() {
        return if n >= 0 { usize::try_from(n).ok() } else { None };
    }
    if let Some(s) = val.as_str() {
        return s.trim().parse::<usize>().ok();
    }
    None
}

fn extract_json_object(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let stripped = strip_outer_markdown_fence(trimmed);
    if let Some(slice) = find_balanced_braces(stripped) {
        return Some(slice.to_string());
    }
    find_balanced_braces(trimmed).map(str::to_string)
}

fn strip_outer_markdown_fence(input: &str) -> &str {
    let trimmed = input.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let after_open = match trimmed.find('\n') {
        Some(idx) => &trimmed[idx + 1..],
        None => return trimmed,
    };
    let end = after_open.rfind("```").unwrap_or(after_open.len());
    after_open[..end].trim()
}

fn find_balanced_braces(input: &str) -> Option<&str> {
    let mut start: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                if start.is_some() {
                    in_string = true;
                }
            }
            '{' => {
                if start.is_none() {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if let Some(s) = start {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(&input[s..=idx]);
                    }
                }
            }
            _ => {}
        }
    }

    None
}

fn normalize_rel_path(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn extract_candidate_symbols(changed_hint: &str) -> Vec<(String, usize)> {
    // Strip leading "path:line -> " hint prefix when present so file path segments
    // don't pollute symbol extraction.
    let hint_body = match changed_hint.split_once("->") {
        Some((prefix, rest)) if prefix.contains(':') && !prefix.contains('(') => rest.trim(),
        _ => changed_hint.trim(),
    };

    let bytes = hint_body.as_bytes();
    let mut weights: HashMap<String, usize> = HashMap::new();
    let mut prev_ident: Option<String> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let ident = &hint_body[start..i];
            if ident.len() >= 3 && !is_noise_keyword(ident) {
                let next_non_ws = hint_body[i..].trim_start().as_bytes().first().copied();
                let mut weight = 2usize;

                if matches!(
                    prev_ident.as_deref(),
                    Some(
                        "fn" | "def"
                            | "function"
                            | "class"
                            | "struct"
                            | "enum"
                            | "trait"
                            | "interface"
                            | "type"
                            | "impl"
                            | "const"
                    )
                ) {
                    weight += 6;
                }

                if next_non_ws == Some(b'(') {
                    weight += 4;
                } else if next_non_ws == Some(b'<') || next_non_ws == Some(b':') {
                    weight += 2;
                }

                if ident.contains('_') || has_camel_boundary(ident) {
                    weight += 2;
                }

                let entry = weights.entry(ident.to_string()).or_insert(0);
                if weight > *entry {
                    *entry = weight;
                }
            }
            prev_ident = Some(ident.to_string());
        } else {
            i += 1;
        }
    }

    let mut ranked: Vec<(String, usize)> = weights.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(6);
    ranked
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn has_camel_boundary(ident: &str) -> bool {
    let mut saw_lower = false;
    for b in ident.bytes() {
        if b.is_ascii_lowercase() {
            saw_lower = true;
        } else if b.is_ascii_uppercase() && saw_lower {
            return true;
        }
    }
    false
}

fn is_noise_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "pub"
            | "use"
            | "mod"
            | "let"
            | "mut"
            | "var"
            | "val"
            | "const"
            | "static"
            | "def"
            | "function"
            | "class"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "interface"
            | "type"
            | "return"
            | "async"
            | "await"
            | "import"
            | "export"
            | "from"
            | "for"
            | "while"
            | "loop"
            | "match"
            | "switch"
            | "case"
            | "else"
            | "self"
            | "Self"
            | "this"
            | "super"
            | "true"
            | "false"
            | "null"
            | "none"
            | "None"
            | "Some"
            | "Result"
            | "Option"
            | "String"
            | "Vec"
            | "str"
            | "bool"
            | "usize"
            | "isize"
            | "u32"
            | "u64"
            | "i32"
            | "i64"
            | "f64"
            | "string"
            | "number"
            | "boolean"
            | "void"
            | "any"
            | "unknown"
            | "Promise"
            | "new"
            | "where"
            | "crate"
    )
}

fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let start = search_from + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_ident_continue(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_ident_continue(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        search_from = start + needle.len();
    }
    false
}

fn is_call_or_type_usage(line: &str, symbol: &str) -> bool {
    line.contains(&format!("{symbol}("))
        || line.contains(&format!(".{symbol}("))
        || line.contains(&format!("::{symbol}"))
        || line.contains(&format!("{symbol}<"))
}

fn is_import_line(trimmed: &str) -> bool {
    trimmed.starts_with("import ") || trimmed.starts_with("use ") || trimmed.starts_with("from ")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut count = 0usize;
    for (idx, _) in s.char_indices() {
        if count == max_chars {
            return format!("{}...", &s[..idx]);
        }
        count += 1;
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_nes_prompt_includes_context_and_related_sites() {
        let recent = vec!["src/auth.rs:10 -> fn verify_token(token: &str, strict: bool)".to_string()];
        let related = vec![RelatedEditSite {
            path: "src/routes.rs".to_string(),
            line: 42,
            preview: "let claims = verify_token(&header)?;".to_string(),
        }];

        let (sys, user) = build_nes_prompt(
            "src/auth.rs",
            "rust",
            "use crate::Claims;",
            "pub fn verify_token(token: &str, strict: bool) -> Result<Claims, AuthError> {\n    todo!()\n}",
            "pub fn refresh() {}",
            10,
            &recent,
            &related,
        );

        assert!(sys.contains("\"kind\""));
        assert!(sys.contains("\"jumpToLine\""));
        assert!(user.contains("File: src/auth.rs (rust)"));
        assert!(user.contains("lines 10-12"));
        assert!(user.contains("src/routes.rs:42 | let claims = verify_token(&header)?;"));
        assert!(user.contains("pub fn verify_token(token: &str, strict: bool)"));
        assert!(user.contains("<window_before>"));
        assert!(user.contains("<window_after>"));
    }

    #[test]
    fn parse_nes_response_parses_plain_and_fenced_json() {
        let fenced = r#"
Here is the next edit:
```json
{
  "kind": "edit",
  "targetPath": "src/lib.rs",
  "startLine": 14,
  "endLine": 16,
  "replacement": "let ok = validate(&input, true)?;\nreturn Ok(ok);",
  "reason": "Pass new strict flag to validate()",
  "jumpToLine": null
}
```
"#;

        let pred = parse_nes_response(fenced, "src/fallback.rs", 1, 1).expect("should parse");
        assert_eq!(pred.kind, "edit");
        assert_eq!(pred.target_path, "src/lib.rs");
        assert_eq!(pred.start_line, 14);
        assert_eq!(pred.end_line, 16);
        assert_eq!(
            pred.replacement,
            "let ok = validate(&input, true)?;\nreturn Ok(ok);"
        );
        assert_eq!(pred.reason, "Pass new strict flag to validate()");
        assert_eq!(pred.jump_to_line, None);
    }

    #[test]
    fn parse_nes_response_handles_braces_inside_strings_and_snake_case() {
        let raw = r#"{
            "kind": "edit",
            "target_path": "",
            "start_line": 22,
            "replacement": "if ready { return Ok(()); }",
            "reason": "Wrap in guard block"
        }"#;

        let pred = parse_nes_response(raw, "src/main.rs", 5, 8).expect("should parse");
        assert_eq!(pred.target_path, "src/main.rs");
        assert_eq!(pred.start_line, 22);
        assert_eq!(pred.end_line, 22);
        assert_eq!(pred.replacement, "if ready { return Ok(()); }");
    }

    #[test]
    fn parse_nes_response_defaults_jump_to_line_and_rejects_invalid() {
        let jump_json = r#"{"kind": "jump", "targetPath": "src/caller.ts", "startLine": 88, "reason": "Update caller"}"#;
        let jump = parse_nes_response(jump_json, "src/def.ts", 10, 12).expect("jump parses");
        assert_eq!(jump.kind, "jump");
        assert_eq!(jump.target_path, "src/caller.ts");
        assert_eq!(jump.jump_to_line, Some(88));

        assert_eq!(parse_nes_response("not json", "a.rs", 1, 1), None);
        assert_eq!(
            parse_nes_response(r#"{"kind": "bogus", "replacement": "x"}"#, "a.rs", 1, 1),
            None
        );
        assert_eq!(
            parse_nes_response(r#"{"kind": "insert", "replacement": ""}"#, "a.rs", 1, 1),
            None
        );
        assert_eq!(
            parse_nes_response(r#"{"kind": "edit", "startLine": 3}"#, "a.rs", 1, 1),
            None
        );

        let none_pred =
            parse_nes_response(r#"{"kind": "none", "reason": "No edit needed"}"#, "a.rs", 4, 4)
                .expect("none kind should parse");
        assert_eq!(none_pred.kind, "none");
        assert_eq!(none_pred.start_line, 4);

        let delete_pred = parse_nes_response(
            r#"{"kind": "edit", "startLine": 5, "endLine": 6, "replacement": ""}"#,
            "a.rs",
            1,
            1,
        )
        .expect("explicit empty replacement edit should parse");
        assert_eq!(delete_pred.replacement, "");
    }

    #[test]
    fn find_related_edit_sites_detects_cross_file_callers_and_ignores_partial_words() {
        let files = vec![
            (
                "src/auth.rs".to_string(),
                "pub fn verify_token(token: &str, strict: bool) {}\n".to_string(),
            ),
            (
                "src/routes.rs".to_string(),
                "use crate::auth::verify_token;\n\nfn handle() {\n    let user = verify_token(&tok);\n}\n"
                    .to_string(),
            ),
            (
                "src/other.rs".to_string(),
                "fn helper() {\n    let x = verify_token_v2(&tok);\n}\n".to_string(),
            ),
            (
                "src/tests.rs".to_string(),
                "fn test_auth() {\n    verify_token(\"abc\");\n}\n".to_string(),
            ),
        ];

        let hits = find_related_edit_sites(
            "src/auth.rs:1 -> pub fn verify_token(token: &str, strict: bool)",
            "src/auth.rs",
            &files,
            5,
        );

        assert!(!hits.iter().any(|h| h.path == "src/auth.rs"));
        assert!(!hits.iter().any(|h| h.path == "src/other.rs"));
        assert_eq!(hits.len(), 3);
        // Call sites should rank above the import line
        assert_eq!(hits[0].line, 4);
        assert_eq!(hits[0].path, "src/routes.rs");
        assert_eq!(hits[1].path, "src/tests.rs");
        assert_eq!(hits[1].line, 2);
        assert_eq!(hits[2].path, "src/routes.rs");
        assert_eq!(hits[2].line, 1);
    }

    #[test]
    fn find_related_edit_sites_respects_limit_and_utf8_truncation() {
        let long_line = format!("call_target(\"{}\");", "é".repeat(200));
        let files = vec![
            ("a.ts".to_string(), long_line),
            ("b.ts".to_string(), "call_target('b');".to_string()),
        ];

        let hits = find_related_edit_sites("function call_target(x: string)", "def.ts", &files, 1);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].preview.ends_with("..."));
        assert!(hits[0].preview.chars().count() <= 163);
    }
}
