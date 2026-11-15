#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

const MAX_SNIPPET_CHARS: usize = 200;
const MAX_REPORTED_SPANS_IN_MESSAGE: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxErrorSpan {
    pub line: usize,
    pub column: usize,
    pub snippet: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxValidationResult {
    pub supported: bool,
    pub valid: bool,
    pub before_errors: usize,
    pub after_errors: usize,
    pub introduced_errors: Vec<SyntaxErrorSpan>,
    pub message: String,
}

enum GateLanguage {
    TreeSitter { lang: Language, label: &'static str },
    Json,
}

fn detect_gate_language(path: &str) -> Option<GateLanguage> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;

    match ext.as_str() {
        "rs" => Some(GateLanguage::TreeSitter {
            lang: tree_sitter_rust::LANGUAGE.into(),
            label: "Rust",
        }),
        "ts" | "mts" | "cts" => Some(GateLanguage::TreeSitter {
            lang: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            label: "TypeScript",
        }),
        "tsx" => Some(GateLanguage::TreeSitter {
            lang: tree_sitter_typescript::LANGUAGE_TSX.into(),
            label: "TSX",
        }),
        "js" | "jsx" | "mjs" | "cjs" => Some(GateLanguage::TreeSitter {
            lang: tree_sitter_javascript::LANGUAGE.into(),
            label: "JavaScript",
        }),
        "py" | "pyi" => Some(GateLanguage::TreeSitter {
            lang: tree_sitter_python::LANGUAGE.into(),
            label: "Python",
        }),
        "go" => Some(GateLanguage::TreeSitter {
            lang: tree_sitter_go::LANGUAGE.into(),
            label: "Go",
        }),
        "json" => Some(GateLanguage::Json),
        _ => None,
    }
}

pub fn is_syntax_gate_supported(path: &str) -> bool {
    detect_gate_language(path).is_some()
}

/// Validates a proposed edit (`before` -> `after`) for `path` before writing to disk.
///
/// - If `before` was clean (`before_errors == 0`, including new/empty files), `after_errors == 0` is required.
/// - If `before` already had syntax errors, `after` is valid iff `after_errors <= before_errors`
///   (disallowing edits that increase syntax error count while allowing partial repairs).
pub fn validate_edit_syntax(path: &str, before: &str, after: &str) -> SyntaxValidationResult {
    let Some(gate_lang) = detect_gate_language(path) else {
        return SyntaxValidationResult {
            supported: false,
            valid: true,
            before_errors: 0,
            after_errors: 0,
            introduced_errors: Vec::new(),
            message: String::new(),
        };
    };

    let lang_label = match &gate_lang {
        GateLanguage::TreeSitter { label, .. } => *label,
        GateLanguage::Json => "JSON",
    };

    let before_spans = if before.trim().is_empty() {
        Vec::new()
    } else {
        collect_syntax_errors(&gate_lang, before)
    };
    let after_spans = collect_syntax_errors(&gate_lang, after);

    let before_errors = before_spans.len();
    let after_errors = after_spans.len();
    let valid = after_errors == 0 || after_errors <= before_errors;

    let mut introduced_errors = diff_introduced_spans(&before_spans, &after_spans);
    if !valid && introduced_errors.is_empty() {
        introduced_errors = after_spans.clone();
    }

    let message = if valid {
        if after_errors == 0 {
            String::new()
        } else {
            format!(
                "{lang_label} syntax gate passed for `{path}` (pre-existing syntax errors did not increase: {before_errors} -> {after_errors})."
            )
        }
    } else {
        format_actionable_error_message(
            path,
            lang_label,
            before_errors,
            after_errors,
            &introduced_errors,
            &after_spans,
        )
    };

    SyntaxValidationResult {
        supported: true,
        valid,
        before_errors,
        after_errors,
        introduced_errors,
        message,
    }
}

fn collect_syntax_errors(gate_lang: &GateLanguage, source: &str) -> Vec<SyntaxErrorSpan> {
    match gate_lang {
        GateLanguage::TreeSitter { lang, .. } => collect_tree_sitter_errors(lang, source),
        GateLanguage::Json => collect_json_errors(source),
    }
}

fn collect_tree_sitter_errors(lang: &Language, source: &str) -> Vec<SyntaxErrorSpan> {
    if source.trim().is_empty() {
        return Vec::new();
    }

    let mut parser = Parser::new();
    if parser.set_language(lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let root = tree.root_node();
    if !root.has_error() {
        return Vec::new();
    }

    let lines: Vec<&str> = source
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let mut raw_spans = Vec::new();
    walk_cst_errors(root, source, &lines, &mut raw_spans);

    raw_spans.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.column.cmp(&b.column)));

    // Deduplicate spans landing on the exact same 1-based (line, column).
    let mut deduped: Vec<SyntaxErrorSpan> = Vec::with_capacity(raw_spans.len());
    for span in raw_spans {
        if let Some(prev) = deduped.last() {
            if prev.line == span.line && prev.column == span.column {
                continue;
            }
        }
        deduped.push(span);
    }

    deduped
}

fn walk_cst_errors(
    node: Node,
    source: &str,
    lines: &[&str],
    out: &mut Vec<SyntaxErrorSpan>,
) {
    if !node.has_error() {
        return;
    }

    if node.is_missing() {
        let pos = node.start_position();
        let line = pos.row + 1;
        let column = byte_col_to_one_based_char_col(lines, pos.row, pos.column);
        let snippet = extract_offending_line(lines, pos.row);
        let expected = node.kind();
        out.push(SyntaxErrorSpan {
            line,
            column,
            snippet,
            kind: format!("missing `{expected}`"),
        });
        return;
    }

    if node.is_error() {
        if has_nested_error_child(node) {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_cst_errors(child, source, lines, out);
            }
            return;
        }

        let pos = node.start_position();
        let line = pos.row + 1;
        let column = byte_col_to_one_based_char_col(lines, pos.row, pos.column);
        let snippet = extract_offending_line(lines, pos.row);
        let offending_tok = source
            .get(node.byte_range())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        let kind = if offending_tok.is_empty() {
            "syntax_error".to_string()
        } else {
            let compact: String = offending_tok.chars().take(32).collect();
            format!("syntax_error near `{compact}`")
        };

        out.push(SyntaxErrorSpan {
            line,
            column,
            snippet,
            kind,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_cst_errors(child, source, lines, out);
    }
}

fn has_nested_error_child(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_error() || (child.has_error() && subtree_contains_error_node(child)) {
            return true;
        }
    }
    false
}

fn subtree_contains_error_node(node: Node) -> bool {
    if node.is_error() {
        return true;
    }
    if !node.has_error() {
        return false;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if subtree_contains_error_node(child) {
            return true;
        }
    }
    false
}

fn collect_json_errors(source: &str) -> Vec<SyntaxErrorSpan> {
    match serde_json::from_str::<serde_json::Value>(source) {
        Ok(_) => Vec::new(),
        Err(err) => {
            let lines: Vec<&str> = source
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .collect();
            let line = err.line().max(1);
            let column = err.column().max(1);
            let snippet = extract_offending_line(&lines, line - 1);
            let raw_msg = err.to_string();
            let clean_reason = raw_msg
                .rsplit_once(" at line ")
                .map(|(prefix, _)| prefix)
                .unwrap_or(&raw_msg);
            vec![SyntaxErrorSpan {
                line,
                column,
                snippet,
                kind: format!("json_syntax_error: {clean_reason}"),
            }]
        }
    }
}

fn byte_col_to_one_based_char_col(lines: &[&str], row: usize, byte_col: usize) -> usize {
    if let Some(line) = lines.get(row) {
        if let Some(prefix) = line.get(..byte_col) {
            return prefix.chars().count() + 1;
        }
    }
    byte_col + 1
}

fn extract_offending_line(lines: &[&str], zero_based_row: usize) -> String {
    if lines.is_empty() {
        return String::new();
    }

    if let Some(direct) = lines.get(zero_based_row) {
        let trimmed = direct.trim();
        if !trimmed.is_empty() {
            return truncate_chars(trimmed, MAX_SNIPPET_CHARS);
        }
    }

    let search_from = zero_based_row.min(lines.len().saturating_sub(1));
    for idx in (0..=search_from).rev() {
        let trimmed = lines[idx].trim();
        if !trimmed.is_empty() {
            return format!("<after> {}", truncate_chars(trimmed, MAX_SNIPPET_CHARS));
        }
    }

    String::new()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

fn diff_introduced_spans(
    before_spans: &[SyntaxErrorSpan],
    after_spans: &[SyntaxErrorSpan],
) -> Vec<SyntaxErrorSpan> {
    if after_spans.is_empty() {
        return Vec::new();
    }
    if before_spans.is_empty() {
        return after_spans.to_vec();
    }

    let mut prior_counts: HashMap<(&str, &str), usize> = HashMap::new();
    for b in before_spans {
        *prior_counts
            .entry((b.kind.as_str(), b.snippet.as_str()))
            .or_insert(0) += 1;
    }

    let mut introduced = Vec::new();
    for a in after_spans {
        let key = (a.kind.as_str(), a.snippet.as_str());
        if let Some(remaining) = prior_counts.get_mut(&key) {
            if *remaining > 0 {
                *remaining -= 1;
                continue;
            }
        }
        introduced.push(a.clone());
    }
    introduced
}

fn format_actionable_error_message(
    path: &str,
    lang_label: &str,
    before_errors: usize,
    after_errors: usize,
    introduced_errors: &[SyntaxErrorSpan],
    all_after_errors: &[SyntaxErrorSpan],
) -> String {
    let display_spans = if introduced_errors.is_empty() {
        all_after_errors
    } else {
        introduced_errors
    };

    let mut out = format!(
        "Syntax gate rejected edit to `{path}` ({lang_label} syntax errors increased from {before_errors} to {after_errors}). The file was NOT modified on disk.\nOffending syntax error(s):\n"
    );

    for span in display_spans.iter().take(MAX_REPORTED_SPANS_IN_MESSAGE) {
        if span.snippet.is_empty() {
            out.push_str(&format!(
                "  - Line {}, Col {} [{}]\n",
                span.line, span.column, span.kind
            ));
        } else {
            out.push_str(&format!(
                "  - Line {}, Col {} [{}]: `{}`\n",
                span.line, span.column, span.kind, span.snippet
            ));
        }
    }

    if display_spans.len() > MAX_REPORTED_SPANS_IN_MESSAGE {
        out.push_str(&format!(
            "  ... and {} more syntax error(s).\n",
            display_spans.len() - MAX_REPORTED_SPANS_IN_MESSAGE
        ));
    }

    out.push_str(
        "Self-repair instruction: fix unbalanced delimiters (`{`, `}`, `(`, `)`, `[`, `]`), missing semicolons/colons, or malformed statements at the reported line/column and retry the edit.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_rust_ts_tsx_js_python_go_and_json() {
        let rust_res = validate_edit_syntax(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            r#"
pub struct Calculator {
    offset: i32,
}

impl Calculator {
    pub fn compute(&self, a: i32, b: i32) -> i32 {
        a + b + self.offset
    }
}
"#,
        );
        assert!(rust_res.supported);
        assert!(rust_res.valid, "unexpected rust msg: {}", rust_res.message);
        assert_eq!(rust_res.before_errors, 0);
        assert_eq!(rust_res.after_errors, 0);
        assert!(rust_res.introduced_errors.is_empty());

        let ts_res = validate_edit_syntax(
            "src/service.ts",
            "",
            r#"
export interface AuthSession {
    userId: string;
    expiresAt: number;
}

export function isSessionActive(session: AuthSession, now: number): boolean {
    return session.expiresAt > now;
}
"#,
        );
        assert!(ts_res.supported);
        assert!(ts_res.valid, "unexpected ts msg: {}", ts_res.message);
        assert_eq!(ts_res.after_errors, 0);

        let tsx_res = validate_edit_syntax(
            "src/Button.tsx",
            "",
            r#"
export function Button({ label }: { label: string }) {
    return <button className="btn">{label}</button>;
}
"#,
        );
        assert!(tsx_res.supported);
        assert!(tsx_res.valid, "unexpected tsx msg: {}", tsx_res.message);

        let jsx_res = validate_edit_syntax(
            "src/Card.jsx",
            "",
            "export const Card = ({ title }) => <section><h2>{title}</h2></section>;\n",
        );
        assert!(jsx_res.supported);
        assert!(jsx_res.valid, "unexpected jsx msg: {}", jsx_res.message);

        let mjs_res = validate_edit_syntax(
            "scripts/build.mjs",
            "",
            "export async function main() { return 42; }\n",
        );
        assert!(mjs_res.supported);
        assert!(mjs_res.valid);

        let py_res = validate_edit_syntax(
            "app/worker.py",
            "",
            r#"
class TaskRunner:
    def __init__(self, concurrency: int) -> None:
        self.concurrency = concurrency

    def run(self, items: list[str]) -> int:
        return len(items)
"#,
        );
        assert!(py_res.supported);
        assert!(py_res.valid, "unexpected py msg: {}", py_res.message);
        assert_eq!(py_res.after_errors, 0);

        let go_res = validate_edit_syntax(
            "internal/server/http.go",
            "",
            r#"
package server

type Router struct {
    prefix string
}

func NewRouter(prefix string) *Router {
    return &Router{prefix: prefix}
}
"#,
        );
        assert!(go_res.supported);
        assert!(go_res.valid, "unexpected go msg: {}", go_res.message);
        assert_eq!(go_res.after_errors, 0);

        let json_res = validate_edit_syntax(
            "package.json",
            "",
            r#"{"name": "rursor", "version": "0.1.0", "private": true}"#,
        );
        assert!(json_res.supported);
        assert!(json_res.valid, "unexpected json msg: {}", json_res.message);
        assert_eq!(json_res.after_errors, 0);
    }

    #[test]
    fn rejects_broken_rust_with_actionable_span() {
        let before = "pub fn compute(x: i32) -> i32 {\n    x * 2\n}\n";
        let after = "pub fn compute(x: i32) -> i32 {\n    let broken = ;\n    x * 2\n\n";

        let res = validate_edit_syntax("src/math.rs", before, after);
        assert!(res.supported);
        assert!(!res.valid);
        assert_eq!(res.before_errors, 0);
        assert!(res.after_errors >= 1);
        assert!(!res.introduced_errors.is_empty());
        assert!(res.introduced_errors.iter().any(|s| s.line == 2));
        assert!(res.message.contains("src/math.rs"));
        assert!(res.message.contains("Line 2"));
        assert!(res.message.contains("Self-repair instruction"));
    }

    #[test]
    fn rejects_broken_typescript_and_tsx() {
        let before = "export function greet(name: string): string {\n  return `hi ${name}`;\n}\n";
        let after = "export function greet(name: string): string {\n  const x: = 123;\n  return `hi ${name}`;\n";

        let res = validate_edit_syntax("src/greet.ts", before, after);
        assert!(res.supported);
        assert!(!res.valid);
        assert_eq!(res.before_errors, 0);
        assert!(res.after_errors >= 1);
        assert!(!res.introduced_errors.is_empty());
        assert!(res.message.contains("src/greet.ts"));

        let tsx_broken = validate_edit_syntax(
            "src/App.tsx",
            "export const App = () => <div>ok</div>;\n",
            "export const App = () => <div><span>unclosed</div>;\n",
        );
        assert!(tsx_broken.supported);
        assert!(!tsx_broken.valid);
        assert!(tsx_broken.after_errors >= 1);
    }

    #[test]
    fn rejects_broken_json_including_new_file_and_trailing_comma() {
        let new_broken = validate_edit_syntax(
            "tsconfig.json",
            "",
            "{\n  \"compilerOptions\": {\n    \"strict\": true,\n  }\n}",
        );
        assert!(new_broken.supported);
        assert!(!new_broken.valid);
        assert_eq!(new_broken.before_errors, 0);
        assert_eq!(new_broken.after_errors, 1);
        assert_eq!(new_broken.introduced_errors.len(), 1);
        assert_eq!(new_broken.introduced_errors[0].line, 4);
        assert!(new_broken.message.contains("tsconfig.json"));
        assert!(new_broken.message.contains("Line 4"));
    }

    #[test]
    fn allows_non_increasing_errors_on_already_broken_file_and_blocks_increases() {
        let already_broken = "fn first() {\n    let a = ;\n}\n\nfn second() -> i32 {\n    10\n}\n";
        let same_error_edit = "fn first() {\n    let a = ;\n}\n\nfn second() -> i32 {\n    42\n}\n";
        let worse_edit = "fn first() {\n    let a = ;\n}\n\nfn second() -> i32 {\n    let b = ;\n    42\n}\n";
        let repaired_edit = "fn first() {\n    let a = 1;\n}\n\nfn second() -> i32 {\n    42\n}\n";

        let keep_res = validate_edit_syntax("src/broken.rs", already_broken, same_error_edit);
        assert!(keep_res.valid);
        assert_eq!(keep_res.before_errors, keep_res.after_errors);

        let worse_res = validate_edit_syntax("src/broken.rs", already_broken, worse_edit);
        assert!(!worse_res.valid);
        assert!(worse_res.after_errors > worse_res.before_errors);
        assert!(worse_res
            .introduced_errors
            .iter()
            .any(|s| s.snippet.contains("let b =")));

        let fixed_res = validate_edit_syntax("src/broken.rs", already_broken, repaired_edit);
        assert!(fixed_res.valid);
        assert_eq!(fixed_res.after_errors, 0);
    }

    #[test]
    fn rejects_broken_python_go_and_javascript() {
        let py_broken = validate_edit_syntax(
            "app/handler.py",
            "def process(items: list[str]) -> int:\n    return len(items)\n",
            "def process(items: list[str]) -> int\n    return len(items)\n",
        );
        assert!(py_broken.supported);
        assert!(!py_broken.valid);
        assert!(py_broken.after_errors >= 1);
        assert!(!py_broken.introduced_errors.is_empty());

        let go_broken = validate_edit_syntax(
            "cmd/main.go",
            "package main\n\nfunc main() {}\n",
            "package main\n\nfunc main( {\n}\n",
        );
        assert!(go_broken.supported);
        assert!(!go_broken.valid);
        assert!(go_broken.after_errors >= 1);

        let js_broken = validate_edit_syntax(
            "src/util.mjs",
            "export function sum(a, b) { return a + b; }\n",
            "export function sum(a, b) { const arr = [a, b; return arr; }\n",
        );
        assert!(js_broken.supported);
        assert!(!js_broken.valid);
        assert!(js_broken.after_errors >= 1);
    }

    #[test]
    fn unsupported_extensions_pass_through_cleanly() {
        let res = validate_edit_syntax("docs/architecture.md", "# Old", "# New {{{ unclosed");
        assert!(!res.supported);
        assert!(res.valid);
        assert_eq!(res.before_errors, 0);
        assert_eq!(res.after_errors, 0);
        assert!(res.introduced_errors.is_empty());
    }
}
