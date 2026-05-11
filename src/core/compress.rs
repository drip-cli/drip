//! Semantic compression — replaces long function bodies with a one-line
//! placeholder so a first-read of a 400-line file becomes ~40 lines of
//! signatures + a few stubs. `drip refresh <file>` recovers the full
//! content.
//!
//! No real parser (tree-sitter, syn) — they'd quadruple the binary
//! size. Instead, language-aware line scanning + brace balancing.
//! Degrades gracefully: false negatives mean uncompressed output,
//! never mangled output.
//!
//! Languages with first-class support: Python, Rust, JS/TS, Go, and
//! the C-family (Java, C, C++, C#, Kotlin, Swift, Scala, PHP) via a
//! shared brace heuristic that handles K&R (`signature {`) and Allman
//! (`signature` + lone `{` line).
//!
//! Anything else returns `None` and the file is sent uncompressed.
//! Opt-OUT via `DRIP_NO_COMPRESS=1`; raise the threshold with
//! `DRIP_COMPRESS_MIN_BYTES=N` (default 1024).

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    Rust,
    JavaScript,
    TypeScript,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Kotlin,
    Swift,
    Scala,
    Php,
    Generic,
}

#[derive(Debug, Clone)]
pub struct Compressed {
    pub text: String,
    pub functions_elided: usize,
    pub lines_elided: usize,
    pub original_lines: usize,
    /// Names of elided functions, in source order. Stored as JSON on
    /// the `reads` row so the post-edit hook can warn when an edit
    /// targets a body the agent never saw.
    pub elided_function_names: Vec<String>,
    /// Compressed-line → original-line mapping. One entry per line in
    /// `text`. For visible lines start == end. For elided-body stubs
    /// the range spans every hidden line; `symbol_name` is set.
    #[allow(dead_code)]
    pub source_map: SourceMap,
}

/// One row of the compressed→original line mapping. All line numbers
/// are 1-indexed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceMapEntry {
    pub compressed_line: usize,
    pub original_start: usize,
    pub original_end: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    #[serde(default)]
    pub elided: bool,
}

pub type SourceMap = Vec<SourceMapEntry>;

/// Bodies shorter than this stay inline — eliding a short function
/// would cost more tokens than the function itself. Override with
/// `DRIP_COMPRESS_MIN_BODY` (clamped to a hard floor of 4).
const DEFAULT_MIN_BODY_LINES: usize = 15;
const MIN_BODY_LINES_FLOOR: usize = 4;

pub fn min_body_lines() -> usize {
    std::env::var("DRIP_COMPRESS_MIN_BODY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MIN_BODY_LINES)
        .max(MIN_BODY_LINES_FLOOR)
}
/// Files smaller than this don't benefit from compression. Configurable
/// via `DRIP_COMPRESS_MIN_BYTES`.
const DEFAULT_MIN_BYTES: usize = 1024;

pub fn detect_language(path: &Path) -> Language {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "py" => Language::Python,
        "rs" => Language::Rust,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "ts" | "tsx" => Language::TypeScript,
        "go" => Language::Go,
        "java" => Language::Java,
        "c" | "h" => Language::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
        "cs" => Language::CSharp,
        "kt" | "kts" => Language::Kotlin,
        "swift" => Language::Swift,
        "scala" | "sc" => Language::Scala,
        "php" | "phtml" => Language::Php,
        _ => Language::Generic,
    }
}

pub fn min_bytes() -> usize {
    std::env::var("DRIP_COMPRESS_MIN_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MIN_BYTES)
}

pub fn enabled() -> bool {
    std::env::var("DRIP_NO_COMPRESS").as_deref() != Ok("1")
}

pub fn compress(content: &str, lang: Language) -> Option<Compressed> {
    if !enabled() || content.len() < min_bytes() {
        return None;
    }
    match lang {
        Language::Python => compress_python(content),
        Language::Rust => compress_brace(content, BraceFlavor::Rust),
        Language::JavaScript | Language::TypeScript => compress_brace(content, BraceFlavor::JsTs),
        Language::Go => compress_brace(content, BraceFlavor::Go),
        Language::Java
        | Language::C
        | Language::Cpp
        | Language::CSharp
        | Language::Kotlin
        | Language::Swift
        | Language::Scala
        | Language::Php => compress_brace(content, BraceFlavor::CFamily),
        Language::Generic => None,
    }
}

// -------- Python ---------------------------------------------------------

/// `def`, `async def`, `class` → indentation of the keyword.
fn python_block_start(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let rest = trimmed
        .strip_prefix("async def ")
        .or_else(|| trimmed.strip_prefix("def "))
        .or_else(|| trimmed.strip_prefix("class "))?;
    let first = rest.chars().next()?;
    if first.is_alphabetic() || first == '_' {
        Some(indent)
    } else {
        None
    }
}

fn leading_ws(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// Function/method name from a `def` / `async def` line. `None` for
/// `class` and unparseable input.
fn python_signature_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("async def ")
        .or_else(|| trimmed.strip_prefix("def "))?;
    let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_')?;
    Some(rest[..end].to_string())
}

fn compress_python(content: &str) -> Option<Compressed> {
    let lines: Vec<&str> = content.lines().collect();
    let original_lines = lines.len();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut source_map: SourceMap = Vec::with_capacity(lines.len());
    let mut funcs_elided = 0usize;
    let mut lines_elided = 0usize;
    let mut elided_names: Vec<String> = Vec::new();
    let mut i = 0usize;
    process_python_range(
        &lines,
        &mut i,
        None,
        &mut out,
        &mut source_map,
        &mut funcs_elided,
        &mut lines_elided,
        &mut elided_names,
    );
    if funcs_elided == 0 {
        return None;
    }
    let mut text = out.join("\n");
    if content.ends_with('\n') {
        text.push('\n');
    }
    Some(Compressed {
        text,
        functions_elided: funcs_elided,
        lines_elided,
        original_lines,
        elided_function_names: elided_names,
        source_map,
    })
}

/// Walk Python lines eliding only `def` / `async def` bodies. Classes
/// are descended into so method signatures, decorators, attributes and
/// docstrings stay visible. `enclosing_indent = Some(N)` scopes the
/// walk to a class body; the walk returns when a non-blank line
/// dedents to or past that boundary.
#[allow(clippy::too_many_arguments)]
fn process_python_range(
    lines: &[&str],
    i: &mut usize,
    enclosing_indent: Option<usize>,
    out: &mut Vec<String>,
    source_map: &mut SourceMap,
    funcs_elided: &mut usize,
    lines_elided: &mut usize,
    elided_names: &mut Vec<String>,
) {
    while *i < lines.len() {
        let line = lines[*i];
        if let Some(enc) = enclosing_indent {
            if !line.trim().is_empty() && leading_ws(line) <= enc {
                return;
            }
        }
        if let Some(indent) = python_block_start(line) {
            let is_class = line.trim_start().starts_with("class ");
            let sig_first = *i;
            out.push(line.to_string());
            push_visible(source_map, out.len(), sig_first + 1);
            let mut sig_end = *i;
            while !lines[sig_end].trim_end().ends_with(':') && sig_end + 1 < lines.len() {
                sig_end += 1;
                out.push(lines[sig_end].to_string());
                push_visible(source_map, out.len(), sig_end + 1);
            }
            *i = sig_end + 1;
            if is_class {
                process_python_range(
                    lines,
                    i,
                    Some(indent),
                    out,
                    source_map,
                    funcs_elided,
                    lines_elided,
                    elided_names,
                );
            } else {
                let fn_name = python_signature_name(line);
                let body_start = *i;
                while *i < lines.len() {
                    let cur = lines[*i];
                    if cur.trim().is_empty() {
                        *i += 1;
                        continue;
                    }
                    let cur_indent = leading_ws(cur);
                    if cur_indent <= indent {
                        break;
                    }
                    *i += 1;
                }
                let body_len = *i - body_start;
                if body_len >= min_body_lines() {
                    let pad = " ".repeat(indent + 4);
                    let body_first_orig = body_start + 1;
                    let body_last_orig = *i;
                    out.push(format!(
                        "{pad}...  # [DRIP-elided: original L{body_first_orig}-L{body_last_orig}, {body_len} lines | drip refresh to expand]"
                    ));
                    source_map.push(SourceMapEntry {
                        compressed_line: out.len(),
                        original_start: body_first_orig,
                        original_end: body_last_orig,
                        symbol_name: fn_name.clone(),
                        elided: true,
                    });
                    *funcs_elided += 1;
                    *lines_elided += body_len.saturating_sub(1);
                    if let Some(n) = fn_name {
                        elided_names.push(n);
                    }
                } else {
                    for (offset, j) in lines[body_start..*i].iter().enumerate() {
                        out.push((*j).to_string());
                        push_visible(source_map, out.len(), body_start + offset + 1);
                    }
                }
            }
        } else {
            out.push(line.to_string());
            push_visible(source_map, out.len(), *i + 1);
            *i += 1;
        }
    }
}

/// 1-to-1 mapping for a verbatim line. Caller passes `out.len()` AFTER
/// pushing.
fn push_visible(source_map: &mut SourceMap, compressed_line: usize, original_line: usize) {
    source_map.push(SourceMapEntry {
        compressed_line,
        original_start: original_line,
        original_end: original_line,
        symbol_name: None,
        elided: false,
    });
}

// -------- Brace-balanced langs (Rust, JS/TS, Go) -------------------------

#[derive(Debug, Clone, Copy)]
enum BraceFlavor {
    Rust,
    JsTs,
    Go,
    /// Java / C / C++ / C# / Kotlin / Swift / Scala / PHP — shared
    /// brace + comment + string syntax. Line ends with `{`, contains
    /// `(...)`, doesn't start with a control/structural keyword.
    CFamily,
}

/// Keywords that must NEVER be treated as function-open lines.
/// Eliding an `if {...}` would hide real logic.
const CONTROL_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "while",
    "for",
    "switch",
    "do",
    "try",
    "catch",
    "finally",
    "synchronized",
    "match",
    "loop",
    "unsafe",
];

/// Container-opening keywords (class / namespace / module). Their
/// inner method signatures are what we want the agent to see, so we
/// don't elide the container itself.
const STRUCTURAL_KEYWORDS: &[&str] = &[
    "class",
    "struct",
    "interface",
    "enum",
    "trait",
    "object",
    "namespace",
    "module",
    "package",
    "extension",
    "protocol",
    "impl",
    "union",
    "record",
];

/// Visibility / declaration modifiers that prefix a type or method
/// declaration in C-family languages. Stripped before classifying the
/// real keyword, so `public class Foo(int x)` is recognised as a type
/// declaration (with a primary constructor) rather than mis-classified
/// as a method because `class` isn't the very first word.
///
/// Kotlin-specific modifiers (`data`, `open`, `value`, `inline`,
/// Modifiers stripped before classifying a C-family declaration.
/// Includes Kotlin's `data`/`open`/`value`/`inline`/`annotation`/
/// `companion`/`fun` so `data class Foo(val x)` etc. resolve as type
/// declarations rather than methods.
const CFAMILY_MODIFIERS: &[&str] = &[
    "public ",
    "private ",
    "protected ",
    "internal ",
    "sealed ",
    "abstract ",
    "static ",
    "partial ",
    "readonly ",
    "unsafe ",
    "virtual ",
    "override ",
    "async ",
    "extern ",
    "final ",
    "synchronized ",
    "native ",
    "default ",
    // Kotlin-specific
    "data ",
    "open ",
    "value ",
    "inline ",
    "annotation ",
    "companion ",
    "fun ",
];

/// Strip every leading modifier so the caller sees the keyword that
/// actually opens the construct.
fn strip_cfamily_modifiers(head: &str) -> &str {
    let mut rest = head;
    'outer: loop {
        for m in CFAMILY_MODIFIERS {
            if let Some(r) = rest.strip_prefix(m) {
                rest = r;
                continue 'outer;
            }
        }
        return rest;
    }
}

/// True iff `head` declares a type. Tolerates visibility modifiers
/// and the C# `record class` / `record struct` pair. Short-circuits
/// primary-constructor lines like `public class Foo(int x)` that look
/// method-like to the brace heuristic.
fn is_type_declaration(head: &str, flavor: BraceFlavor) -> bool {
    if starts_with_word(head, STRUCTURAL_KEYWORDS) {
        return true;
    }
    if matches!(flavor, BraceFlavor::CFamily) {
        let bare = strip_cfamily_modifiers(head);
        if starts_with_word(bare, STRUCTURAL_KEYWORDS) {
            return true;
        }
        if let Some(rest) = bare.strip_prefix("record ") {
            if starts_with_word(rest.trim_start(), &["class", "struct"]) {
                return true;
            }
        }
    }
    false
}

/// True if `line` ends a function/method declaration with an opening
/// brace. The `{` must be the LAST non-whitespace char so struct
/// literals like `let x = Foo {` don't match.
fn is_func_open_line(line: &str, flavor: BraceFlavor) -> bool {
    let stripped = strip_line_comment(line, flavor).trim_end();
    if !stripped.ends_with('{') {
        return false;
    }
    let head = stripped[..stripped.len() - 1].trim_start();
    if starts_with_word(head, CONTROL_KEYWORDS) || is_type_declaration(head, flavor) {
        return false;
    }
    match flavor {
        BraceFlavor::Rust => head.contains("fn ") && head.contains('('),
        BraceFlavor::JsTs => {
            head.contains("function ")
                || head.contains("=>")
                || (head.contains('(') && head.contains(')'))
        }
        BraceFlavor::Go => head.starts_with("func "),
        BraceFlavor::CFamily => head.contains('(') && head.contains(')'),
    }
}

/// True if `line` is a method/function signature whose opening `{`
/// lives on a separate line (Allman style — C# default, common in
/// C++/Java). Caller must confirm the next non-blank line is `{`.
fn is_func_signature_line_allman(line: &str, flavor: BraceFlavor) -> bool {
    // Allman is C-family-only; other flavors are overwhelmingly K&R.
    if !matches!(flavor, BraceFlavor::CFamily) {
        return false;
    }
    let stripped = strip_line_comment(line, flavor).trim_end();
    if stripped.is_empty() {
        return false;
    }
    if stripped.ends_with(';') || stripped.ends_with('{') || stripped.ends_with(',') {
        return false;
    }
    if !(stripped.contains('(') && stripped.contains(')')) {
        return false;
    }
    let head = stripped.trim_start();
    if starts_with_word(head, CONTROL_KEYWORDS) || is_type_declaration(head, flavor) {
        return false;
    }
    // Skip attribute / annotation lines like `[Route("api")]`,
    // `@Cacheable(value = "x")` — they have `(...)` but aren't methods.
    if head.starts_with('[') || head.starts_with('@') {
        return false;
    }
    true
}

fn skip_blank_lines(lines: &[&str], mut idx: usize) -> usize {
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    idx
}

/// Detect the *end* of a function-signature header. Three C-family
/// layouts: single-line K&R (`sig(args) {`), multi-line K&R (params
/// wrap, closing `) {` on a later line), and Allman (lone `{` on the
/// next non-blank line). Returns the index of the line ending with `{`.
fn detect_func_open(lines: &[&str], i: usize, flavor: BraceFlavor) -> Option<usize> {
    let line = lines[i];

    // Single-line K&R.
    if is_func_open_line(line, flavor) {
        return Some(i);
    }

    // Multi-line K&R: line ends with `{` and has more `)` than `(`.
    // Walk back tracking paren depth until we find the matching `(`.
    // That line must look like a real method declaration — otherwise
    // we'd elide value initialisers (`val x = build(\n …\n) { … }`)
    // or trailing-lambda blocks (`launch(\n …\n) { … }`).
    let stripped = strip_line_comment(line, flavor).trim_end();
    if let Some(head) = stripped.strip_suffix('{').map(str::trim_end) {
        let head_trim = head.trim_start();
        if !starts_with_word(head_trim, CONTROL_KEYWORDS) {
            let close_count = head.matches(')').count() as i32;
            let open_count = head.matches('(').count() as i32;
            let mut depth = close_count - open_count;
            if depth > 0 {
                let mut j = i;
                while j > 0 {
                    j -= 1;
                    let prev = strip_line_comment(lines[j], flavor);
                    depth += prev.matches(')').count() as i32;
                    depth -= prev.matches('(').count() as i32;
                    if depth <= 0 {
                        let prev_head = prev.trim_start();
                        if starts_with_word(prev_head, CONTROL_KEYWORDS)
                            || is_type_declaration(prev_head, flavor)
                        {
                            return None;
                        }
                        if !looks_like_method_signature_start(prev_head, flavor) {
                            return None;
                        }
                        return Some(i);
                    }
                }
            }
        }
    }

    // Allman.
    if is_func_signature_line_allman(line, flavor) {
        let brace_idx = skip_blank_lines(lines, i + 1);
        if brace_idx < lines.len() && lines[brace_idx].trim() == "{" {
            return Some(brace_idx);
        }
    }

    None
}

/// Conservative gate for the multi-line K&R case: does the line where
/// the `(` opens look like a method declaration? Anything starting
/// with `val`/`var`/`let`/`return`/etc., or with `=` before the `(`,
/// is rejected to avoid eliding trailing-lambda blocks as bodies.
fn looks_like_method_signature_start(head: &str, flavor: BraceFlavor) -> bool {
    match flavor {
        BraceFlavor::Rust => head.contains("fn "),
        BraceFlavor::Go => head.starts_with("func "),
        BraceFlavor::JsTs => true,
        BraceFlavor::CFamily => {
            const REJECT_PREFIXES: &[&str] = &[
                "val ",
                "var ",
                "let ",
                "const ",
                "return ",
                "throw ",
                "new ",
                "await ",
                "yield ",
                "co_await ",
                "co_yield ",
            ];
            if REJECT_PREFIXES.iter().any(|p| head.starts_with(p)) {
                return false;
            }
            // `=` before the first `(` ⇒ assignment-with-call.
            if let Some(paren_idx) = head.find('(') {
                if head[..paren_idx].contains('=') {
                    return false;
                }
            }
            true
        }
    }
}

/// True iff `head` starts with one of `keywords` followed by a space
/// or `(` — avoids matching `iframe` as `if`.
fn starts_with_word(head: &str, keywords: &[&str]) -> bool {
    for kw in keywords {
        if let Some(rest) = head.strip_prefix(kw) {
            match rest.chars().next() {
                Some(c) if c.is_whitespace() || c == '(' || c == '{' => return true,
                None => return true,
                _ => {}
            }
        }
    }
    false
}

/// True when DRIP shrinks Javadoc / KDoc / JSDoc blocks for `flavor`.
/// Rust's `///` line-doc style is left alone.
fn javadoc_compression_enabled(flavor: BraceFlavor) -> bool {
    if std::env::var("DRIP_COMPRESS_JAVADOC").as_deref() == Ok("0") {
        return false;
    }
    matches!(flavor, BraceFlavor::CFamily | BraceFlavor::JsTs)
}

/// One emitted Javadoc/KDoc/JSDoc line. `elided = true` is the
/// `[DRIP-javadoc-elided …]` marker; its range covers all dropped
/// lines.
#[derive(Debug, Clone)]
struct JdLine {
    text: String,
    orig_start: usize,
    orig_end: usize,
    elided: bool,
}

/// If `lines[start]` opens `/**`, compress the block: keep the summary
/// (first 2 non-blank, non-tag lines) and `@param`/`@return`/`@throws`/
/// `@since`/`@deprecated`/`@exception` tags verbatim; replace the rest
/// with a single `[DRIP-javadoc-elided …]` marker. Returns
/// `(closing_idx, rewritten_lines, lines_elided)` or `None` when the
/// block isn't worth compressing.
fn try_compress_javadoc(
    lines: &[&str],
    start: usize,
    flavor: BraceFlavor,
) -> Option<(usize, Vec<JdLine>, usize)> {
    if !javadoc_compression_enabled(flavor) {
        return None;
    }
    let opener = lines[start].trim_start();
    if !opener.starts_with("/**") {
        return None;
    }
    if opener.contains("*/") {
        return None;
    }
    let mut end = start + 1;
    while end < lines.len() {
        if lines[end].trim_end().ends_with("*/") {
            break;
        }
        end += 1;
    }
    if end == lines.len() {
        return None;
    }
    let block_len = end - start + 1;
    if block_len < 6 {
        return None;
    }

    let mut kept: Vec<usize> = Vec::with_capacity(block_len);
    let mut hit_tag = false;
    let mut summary_kept = 0usize;
    for (idx, line) in lines.iter().enumerate().take(end + 1).skip(start) {
        let trimmed = line
            .trim_start()
            .trim_start_matches('*')
            .trim_start_matches(' ')
            .trim_end();
        if idx == start || idx == end {
            kept.push(idx);
            continue;
        }
        if let Some(tag) = trimmed.split_whitespace().next() {
            if tag.starts_with('@') {
                if matches!(
                    tag,
                    "@param" | "@return" | "@throws" | "@since" | "@deprecated" | "@exception"
                ) {
                    kept.push(idx);
                    hit_tag = true;
                }
                continue;
            }
        }
        if trimmed.is_empty() {
            continue;
        }
        if !hit_tag && summary_kept < 2 {
            kept.push(idx);
            summary_kept += 1;
        }
    }
    let elided = block_len - kept.len();
    if elided < 2 {
        return None;
    }

    let indent = leading_ws(lines[start]);
    let pad = " ".repeat(indent);
    let mut out: Vec<JdLine> = Vec::with_capacity(kept.len() + 1);
    let last = *kept.last().unwrap();
    let kept_set: std::collections::HashSet<usize> = kept.iter().copied().collect();
    let drop_start = (start..=end)
        .find(|i| !kept_set.contains(i))
        .unwrap_or(start);
    let drop_end = (start..=end)
        .rev()
        .find(|i| !kept_set.contains(i))
        .unwrap_or(end);
    for idx in &kept {
        if *idx == last {
            out.push(JdLine {
                text: format!(
                    "{pad} * [DRIP-javadoc-elided: original L{}-L{}, {} lines | drip refresh for full]",
                    drop_start + 1,
                    drop_end + 1,
                    elided,
                ),
                orig_start: drop_start + 1,
                orig_end: drop_end + 1,
                elided: true,
            });
        }
        out.push(JdLine {
            text: lines[*idx].to_string(),
            orig_start: *idx + 1,
            orig_end: *idx + 1,
            elided: false,
        });
    }
    Some((end, out, elided))
}

fn strip_line_comment(line: &str, _flavor: BraceFlavor) -> &str {
    // Every brace flavor we support uses `//` as line comment.
    if let Some(idx) = find_outside_strings(line, "//") {
        &line[..idx]
    } else {
        line
    }
}

/// Find `needle` in `hay`, ignoring occurrences inside string literals.
/// Tracks `"…"`, `'…'`, and (for JS) `` `…` `` minimally — escapes count.
fn find_outside_strings(hay: &str, needle: &str) -> Option<usize> {
    let bytes = hay.as_bytes();
    let n = bytes.len();
    let nlen = needle.len();
    let mut i = 0;
    let mut state: Option<u8> = None; // active string delimiter, if any
    while i < n {
        let c = bytes[i];
        match state {
            Some(d) => {
                if c == b'\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if c == d {
                    state = None;
                }
                i += 1;
            }
            None => {
                if c == b'"' || c == b'\'' || c == b'`' {
                    state = Some(c);
                    i += 1;
                    continue;
                }
                if i + nlen <= n && &bytes[i..i + nlen] == needle.as_bytes() {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// Find the line index of the `}` matching the `{` that opened the
/// scope just before `start`. Counts braces outside strings/comments.
fn find_matching_brace_end(lines: &[&str], start: usize, flavor: BraceFlavor) -> Option<usize> {
    let mut depth: i32 = 1;
    let mut in_block_comment = false;
    let mut i = start;
    while i < lines.len() {
        let mut line = lines[i].to_string();
        if !in_block_comment {
            line = strip_line_comment(&line, flavor).to_string();
        }
        let bytes = line.as_bytes();
        let n = bytes.len();
        let mut j = 0;
        let mut state: Option<u8> = None;
        while j < n {
            let c = bytes[j];
            if in_block_comment {
                if c == b'*' && j + 1 < n && bytes[j + 1] == b'/' {
                    in_block_comment = false;
                    j += 2;
                    continue;
                }
                j += 1;
                continue;
            }
            if state.is_none() && c == b'/' && j + 1 < n && bytes[j + 1] == b'*' {
                in_block_comment = true;
                j += 2;
                continue;
            }
            match state {
                Some(d) => {
                    if c == b'\\' && j + 1 < n {
                        j += 2;
                        continue;
                    }
                    if c == d {
                        state = None;
                    }
                    j += 1;
                    continue;
                }
                None => {
                    if c == b'"' || c == b'\'' || c == b'`' {
                        state = Some(c);
                        j += 1;
                        continue;
                    }
                    if c == b'{' {
                        depth += 1;
                    } else if c == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    j += 1;
                }
            }
        }
        i += 1;
    }
    None
}

fn compress_brace(content: &str, flavor: BraceFlavor) -> Option<Compressed> {
    let lines: Vec<&str> = content.lines().collect();
    let original_lines = lines.len();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut source_map: SourceMap = Vec::with_capacity(lines.len());
    let mut funcs_elided = 0;
    let mut lines_elided = 0;
    let mut elided_names: Vec<String> = Vec::new();
    let mut javadoc_blocks_compressed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Javadoc / KDoc / JSDoc compression — runs first so a long
        // doc block above a short method still pays off even when the
        // body itself is too small to elide.
        if let Some((jd_end, jd_out, jd_elided)) = try_compress_javadoc(&lines, i, flavor) {
            for jd_line in jd_out {
                out.push(jd_line.text);
                source_map.push(SourceMapEntry {
                    compressed_line: out.len(),
                    original_start: jd_line.orig_start,
                    original_end: jd_line.orig_end,
                    symbol_name: None,
                    elided: jd_line.elided,
                });
            }
            lines_elided += jd_elided;
            javadoc_blocks_compressed += 1;
            i = jd_end + 1;
            continue;
        }

        // For multi-line K&R, the parameter-list lines between the `(`
        // opener and this closing `{` have already been pushed by
        // earlier iterations — they're part of the signature header.
        let sig_end_line = match detect_func_open(&lines, i, flavor) {
            Some(end) => end,
            None => {
                out.push(line.to_string());
                push_visible(&mut source_map, out.len(), i + 1);
                i += 1;
                continue;
            }
        };
        let body_start = sig_end_line + 1;

        // Signature start through `{` line, including continuations
        // and a lone Allman brace.
        for (offset, header_line) in lines[i..=sig_end_line].iter().enumerate() {
            out.push((*header_line).to_string());
            push_visible(&mut source_map, out.len(), i + offset + 1);
        }
        match find_matching_brace_end(&lines, body_start, flavor) {
            Some(end) => {
                let body_len = end - body_start;
                if body_len >= min_body_lines() {
                    let indent = (body_start..end)
                        .map(|j| lines[j])
                        .find(|l| !l.trim().is_empty())
                        .map(leading_ws)
                        .unwrap_or(4);
                    let pad = " ".repeat(indent);
                    let body_first_orig = body_start + 1;
                    let body_last_orig = end;
                    let fn_name = brace_signature_name(line, flavor);
                    out.push(format!(
                        "{pad}/* [DRIP-elided: original L{}-L{}, {} lines | drip refresh to expand] */",
                        body_first_orig, body_last_orig, body_len,
                    ));
                    source_map.push(SourceMapEntry {
                        compressed_line: out.len(),
                        original_start: body_first_orig,
                        original_end: body_last_orig,
                        symbol_name: fn_name.clone(),
                        elided: true,
                    });
                    out.push(lines[end].to_string());
                    push_visible(&mut source_map, out.len(), end + 1);
                    funcs_elided += 1;
                    lines_elided += body_len;
                    if let Some(n) = fn_name {
                        elided_names.push(n);
                    }
                    i = end + 1;
                    continue;
                }
                for (offset, j) in lines[body_start..=end].iter().enumerate() {
                    out.push((*j).to_string());
                    push_visible(&mut source_map, out.len(), body_start + offset + 1);
                }
                i = end + 1;
            }
            None => {
                // Unbalanced braces — bail safely (header already pushed).
                i = sig_end_line + 1;
            }
        }
    }
    if funcs_elided == 0 && javadoc_blocks_compressed == 0 {
        return None;
    }
    let mut text = out.join("\n");
    if content.ends_with('\n') {
        text.push('\n');
    }
    Some(Compressed {
        text,
        functions_elided: funcs_elided,
        lines_elided,
        original_lines,
        elided_function_names: elided_names,
        source_map,
    })
}

/// Function/method name on a brace-flavor open line.
fn brace_signature_name(line: &str, flavor: BraceFlavor) -> Option<String> {
    let stripped = strip_line_comment(line, flavor).trim_end();
    let head = if let Some(h) = stripped.strip_suffix('{') {
        h.trim()
    } else {
        stripped.trim()
    };
    match flavor {
        BraceFlavor::Rust => {
            let after = head.find("fn ").map(|i| &head[i + 3..])?;
            let end = after.find(|c: char| !c.is_alphanumeric() && c != '_')?;
            Some(after[..end].to_string())
        }
        BraceFlavor::JsTs => {
            if let Some(after) = head
                .find("function ")
                .map(|i| &head[i + "function ".len()..])
            {
                let end = after
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(after.len());
                if end > 0 {
                    return Some(after[..end].to_string());
                }
            }
            // Arrow / method shorthand: last identifier before `(`.
            let paren = head.find('(')?;
            let pre = head[..paren].trim_end();
            let last_word_start = pre
                .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|i| i + 1)
                .unwrap_or(0);
            let n = pre[last_word_start..].to_string();
            if n.is_empty() {
                None
            } else {
                Some(n)
            }
        }
        BraceFlavor::Go => {
            let after = head.strip_prefix("func ")?;
            let after = if after.starts_with('(') {
                let close = after.find(')')?;
                after[close + 1..].trim_start()
            } else {
                after
            };
            let end = after.find(|c: char| !c.is_alphanumeric() && c != '_')?;
            Some(after[..end].to_string())
        }
        BraceFlavor::CFamily => {
            // `type name(args)` — last identifier before `(`.
            let paren = head.find('(')?;
            let pre = head[..paren].trim_end();
            let last_word_start = pre
                .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|i| i + 1)
                .unwrap_or(0);
            let n = pre[last_word_start..].to_string();
            if n.is_empty() {
                None
            } else {
                Some(n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Compress tests mutate global env vars; serialise to avoid
    /// races between parallel tests' `set_var`/`remove_var`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn long_python_fn(name: &str) -> String {
        format!(
            "def {name}(x, y):\n    \
             a = x + y\n    \
             b = a * 2\n    \
             c = b - 1\n    \
             d = c ** 2\n    \
             return d\n"
        )
    }

    #[test]
    fn python_compresses_long_function() {
        let src = format!("import os\n\n{}", long_python_fn("foo")) + "\nprint('bye')\n";
        // Big enough to clear the min_bytes default? probably not. Bypass:
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let c = compress(&src, Language::Python).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        assert!(c.text.contains("DRIP-elided"));
        assert!(c.text.contains("def foo("));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_source_map_records_function_signature_visible_and_body_elided() {
        // Source layout (1-indexed):
        //   1: import os
        //   2: <blank>
        //   3: def foo(x, y):
        //   4-8: body lines
        //   9: <blank>
        //  10: print('bye')
        // Expected source map after compression:
        //   compressed L1 → orig L1 (visible)
        //   compressed L2 → orig L2 (visible)
        //   compressed L3 → orig L3 (signature, visible)
        //   compressed L4 → orig L4-L8 (elided body, symbol="foo")
        //   compressed L5 → orig L9 (visible)
        //   compressed L6 → orig L10 (visible)
        let src = format!("import os\n\n{}", long_python_fn("foo")) + "\nprint('bye')\n";
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let c = compress(&src, Language::Python).expect("compression should fire");
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");

        let by_compressed: std::collections::HashMap<usize, &SourceMapEntry> = c
            .source_map
            .iter()
            .map(|e| (e.compressed_line, e))
            .collect();
        assert!(
            !c.source_map.is_empty(),
            "source_map should be populated when compression fires",
        );
        // The signature line (`def foo(...)`) must appear as a 1-to-1
        // visible mapping at original line 3.
        let sig_entry = by_compressed
            .values()
            .find(|e| !e.elided && e.original_start == 3)
            .expect("signature entry");
        assert_eq!(sig_entry.original_start, sig_entry.original_end);
        // The elided stub MUST be present, MUST be flagged elided,
        // MUST carry the function name, MUST cover the body range.
        // The Python body walker consumes trailing blank lines as
        // part of the body (it can't tell where the function stops
        // and the next top-level whitespace begins until it sees a
        // non-indented non-blank line), so the elided range may
        // include the L9 blank that separates `foo` from `print`.
        let elided_entry = c
            .source_map
            .iter()
            .find(|e| e.elided)
            .expect("elided entry expected");
        assert_eq!(elided_entry.symbol_name.as_deref(), Some("foo"));
        assert!(
            elided_entry.original_start == 4 && (8..=9).contains(&elided_entry.original_end),
            "elided range {}-{} should start at L4 and end at L8 or L9",
            elided_entry.original_start,
            elided_entry.original_end,
        );
        // The stub text in `c.text` must include the new "original L{}-L{}" notice.
        assert!(
            c.text.contains(&format!(
                "original L{}-L{}",
                elided_entry.original_start, elided_entry.original_end
            )),
            "stub should embed the original line range, got text:\n{}",
            c.text
        );
    }

    #[test]
    fn brace_source_map_covers_body_range() {
        // 12-line Rust function — long enough to trigger elision.
        let body: String = (1..=12)
            .map(|i| format!("    let v_{i} = {i};\n"))
            .collect();
        let src = format!("fn outer() -> i32 {{\n{body}    42\n}}\n");
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let c = compress(&src, Language::Rust).expect("compression");
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");

        let elided = c
            .source_map
            .iter()
            .find(|e| e.elided)
            .expect("elided entry");
        assert_eq!(elided.symbol_name.as_deref(), Some("outer"));
        // Body lines are L2..=L14 (after the signature on L1 and
        // before the closing brace).
        assert!(
            elided.original_start == 2 && elided.original_end >= 13,
            "expected body span starting at L2, got {}-{}",
            elided.original_start,
            elided.original_end,
        );
        assert!(c.text.contains(&format!(
            "original L{}-L{}",
            elided.original_start, elided.original_end
        )));
    }

    #[test]
    fn source_map_has_one_entry_per_compressed_line() {
        // Invariant: every line of `Compressed::text` is represented
        // by exactly one source-map entry. Without this, callers
        // that scan by `compressed_line` (the `drip source-map
        // --line N` lookup, the pre-edit guard's overlap check) can
        // silently miss lines and return wrong answers.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        // Mix of long bodies (elided) + short helper (inlined) so the
        // map exercises both visible-line and elided-stub branches.
        let mut src = String::from("import os\n\n");
        src.push_str(&long_python_fn("big_one"));
        src.push('\n');
        src.push_str("def tiny():\n    return 1\n");
        src.push('\n');
        src.push_str(&long_python_fn("big_two"));
        let c = compress(&src, Language::Python).expect("compression");
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");

        let n_lines = c.text.lines().count();
        assert_eq!(
            c.source_map.len(),
            n_lines,
            "source_map length must equal compressed-text line count: \
             map={} lines={}\n--- compressed text ---\n{}\n--- source map ---\n{:#?}",
            c.source_map.len(),
            n_lines,
            c.text,
            c.source_map,
        );

        // Compressed-line numbers must cover 1..=n_lines without gaps.
        let mut seen: Vec<usize> = c.source_map.iter().map(|e| e.compressed_line).collect();
        seen.sort_unstable();
        let expected: Vec<usize> = (1..=n_lines).collect();
        assert_eq!(seen, expected, "compressed_line values must cover 1..=N");
    }

    #[test]
    fn javadoc_collapse_records_elided_range_in_source_map() {
        // A long /** Javadoc */ block above a function gets compressed
        // to a one-line stub. The source map must record the original
        // span so `drip source-map --line N` can answer "L4 of the
        // compressed view → original L2-L7" instead of the identity
        // mapping the line scanner would otherwise emit.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "package x;\n\
                   /**\n\
                    * Long javadoc block describing things in detail.\n\
                    * Continues for several lines so the collapser fires.\n\
                    * Includes @param annotations and notes.\n\
                    * Plus a final line of context.\n\
                    */\n\
                   public void worker() {\n\
                       int total = 0;\n\
                   }\n";
        let c = compress(src, Language::Java).expect("javadoc compression");
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
        let elided: Vec<_> = c.source_map.iter().filter(|e| e.elided).collect();
        assert!(
            !elided.is_empty(),
            "expected at least one elided source-map entry from javadoc collapse, got map:\n{:#?}",
            c.source_map
        );
        // At least one elided entry should span multiple original lines
        // (the javadoc block) — the function-body elision separately
        // emits its own entry, which is fine; we just need ONE that
        // covers the comment span.
        let multi_line_count = elided
            .iter()
            .filter(|e| e.original_end > e.original_start)
            .count();
        assert!(
            multi_line_count >= 1,
            "expected at least one multi-line elided entry (the javadoc block), got: {:#?}",
            elided
        );
    }

    #[test]
    fn python_short_body_kept_inline() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "def tiny():\n    return 1\n";
        assert!(compress(src, Language::Python).is_none());
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_class_methods_elided_individually_pricing_engine() {
        // Class bodies must be descended into; only def/async-def
        // bodies get elided — never the class itself.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
from decimal import Decimal
from typing import List

class PricingEngine:
    \"\"\"Moteur de tarification.\"\"\"

    DEFAULT_TAX = 0.20

    def __init__(self, registry: dict) -> None:
        self._registry = registry
        self._currency = \"EUR\"
        self._items = []
        self._tax = self.DEFAULT_TAX

    @property
    def currency(self) -> str:
        return self._currency

    @staticmethod
    def validate(amount: Decimal) -> bool:
        if amount < 0:
            return False
        if amount > 1000000:
            return False
        return True

    def compute(self, items: List[dict]) -> Decimal:
        total = Decimal(0)
        for item in items:
            price = Decimal(str(item[\"price\"]))
            qty = item.get(\"qty\", 1)
            line = price * qty
            total += line
        tax_amount = total * Decimal(str(self._tax))
        total += tax_amount
        rounded = total.quantize(Decimal(\"0.01\"))
        self._items.extend(items)
        self._last_total = rounded
        return rounded
";
        let c = compress(src, Language::Python).expect("expected compression");
        let t = &c.text;

        // Class signature visible.
        assert!(
            t.contains("class PricingEngine:"),
            "class header missing: {t}"
        );
        // Class docstring + attribute preserved.
        assert!(
            t.contains("\"\"\"Moteur de tarification.\"\"\""),
            "docstring missing: {t}"
        );
        assert!(
            t.contains("DEFAULT_TAX = 0.20"),
            "class attribute missing: {t}"
        );

        // Every method signature visible.
        assert!(t.contains("def __init__(self, registry: dict) -> None:"));
        assert!(t.contains("def currency(self) -> str:"));
        assert!(t.contains("def validate(amount: Decimal) -> bool:"));
        assert!(t.contains("def compute(self, items: List[dict]) -> Decimal:"));

        // Decorators preserved verbatim with correct indent.
        assert!(t.contains("    @property"), "@property missing: {t}");
        assert!(
            t.contains("    @staticmethod"),
            "@staticmethod missing: {t}"
        );

        // Short body (1 line) kept inline.
        assert!(
            t.contains("return self._currency"),
            "single-line body should stay inline: {t}"
        );

        // Long bodies elided. Three methods qualify (__init__: 4 lines,
        // validate: 5 lines, compute: 11 lines). currency is 1 line.
        assert_eq!(c.functions_elided, 3, "got: {t}");

        // The class body itself MUST NOT be elided as one block.
        // Concretely: the elision pad after `class PricingEngine:` is 4
        // spaces (method indent + 4), not 4 spaces aligned at the class
        // level — so ensure no top-level `    ...  # [DRIP-elided` line
        // appears immediately after the class header.
        let lines: Vec<&str> = t.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if line.starts_with("class PricingEngine") {
                // Look at the next few non-blank lines: none of them
                // should be a class-level elision line.
                let mut k = idx + 1;
                while k < lines.len() && lines[k].trim().is_empty() {
                    k += 1;
                }
                if let Some(next) = lines.get(k) {
                    assert!(
                        !next.trim_start().starts_with("...  # [DRIP-elided")
                            || next.starts_with("        "),
                        "class body was elided wholesale: {next:?}"
                    );
                }
            }
        }

        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_class_with_only_short_methods_returns_none() {
        // No def has a long body → nothing to elide → compress returns
        // None. We don't elide the class itself.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Tiny:
    def a(self):
        return 1

    def b(self):
        return 2
";
        assert!(compress(src, Language::Python).is_none());
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_async_method_inside_class_elided() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Worker:
    async def run(self, payload):
        result = await self.fetch(payload)
        await self.persist(result)
        await self.notify(result)
        return result
";
        let c = compress(src, Language::Python).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        assert!(c.text.contains("class Worker:"));
        assert!(c.text.contains("async def run(self, payload):"));
        assert!(c.text.contains("DRIP-elided"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_nested_class_methods_visible() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Outer:
    class Inner:
        def deep(self):
            a = 1
            b = 2
            c = 3
            d = 4
            return a + b + c + d

    def shallow(self):
        x = 1
        y = 2
        z = 3
        w = 4
        return x + y + z + w
";
        let c = compress(src, Language::Python).expect("expected compression");
        assert_eq!(c.functions_elided, 2, "got: {}", c.text);
        assert!(c.text.contains("class Outer:"));
        assert!(c.text.contains("    class Inner:"));
        assert!(c.text.contains("def deep(self):"));
        assert!(c.text.contains("def shallow(self):"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_multiline_method_signature_preserved() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class API:
    def request(
        self,
        method: str,
        url: str,
        headers: dict,
    ) -> dict:
        conn = self._open()
        resp = conn.send(method, url, headers)
        body = resp.read()
        return self._parse(body)
";
        let c = compress(src, Language::Python).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        // Multi-line signature emitted in full.
        assert!(c.text.contains("def request("));
        assert!(c.text.contains("method: str,"));
        assert!(c.text.contains("headers: dict,"));
        assert!(c.text.contains(") -> dict:"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn python_top_level_function_alongside_class() {
        // Mixed: top-level def + class with methods. Both kinds of long
        // bodies should be elided independently.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
def helper(x):
    a = x + 1
    b = a * 2
    c = b - 3
    d = c ** 2
    return d

class Service:
    def run(self):
        a = 1
        b = 2
        c = 3
        d = 4
        return a + b + c + d
";
        let c = compress(src, Language::Python).expect("expected compression");
        assert_eq!(c.functions_elided, 2);
        assert!(c.text.contains("def helper(x):"));
        assert!(c.text.contains("class Service:"));
        assert!(c.text.contains("def run(self):"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn rust_compresses_function_bodies() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
pub fn foo(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 1;
    let d = c.pow(2);
    d
}

pub fn bar() {
    println!(\"hi\");
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got: {}", c.text);
        assert!(c.text.contains("pub fn foo"));
        assert!(c.text.contains("DRIP-elided"));
        // Short fn `bar` stayed inline.
        assert!(c.text.contains("println!"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn js_arrow_function_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
const handler = (req, res) => {
    const body = req.body;
    const tag = body.tag;
    const value = body.value;
    res.send({ ok: true, tag, value });
    return value;
};
";
        let c = compress(src, Language::JavaScript).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        assert!(c.text.contains("(req, res) =>"));
        assert!(c.text.contains("DRIP-elided"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn no_compression_when_disabled() {
        std::env::set_var("DRIP_NO_COMPRESS", "1");
        let src = "def foo():\n    a = 1\n    b = 2\n    c = 3\n    d = 4\n    return d\n";
        assert!(compress(src, Language::Python).is_none());
        std::env::remove_var("DRIP_NO_COMPRESS");
    }

    #[test]
    fn brace_inside_string_does_not_break_balancing() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        // The literal '}' inside a string would close an unguarded
        // counter and truncate the body. Verify we handle it.
        let src = "\
fn foo() {
    let s = \"} not real\";
    let t = \"{ also fake\";
    let u = 1;
    let v = 2;
    println!(\"{}\", u + v);
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        // Make sure we didn't truncate inside the string.
        assert!(c.text.contains("DRIP-elided"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn detect_language_by_extension() {
        assert_eq!(detect_language(Path::new("a.py")), Language::Python);
        assert_eq!(detect_language(Path::new("b.rs")), Language::Rust);
        assert_eq!(detect_language(Path::new("c.tsx")), Language::TypeScript);
        assert_eq!(detect_language(Path::new("d.go")), Language::Go);
        assert_eq!(detect_language(Path::new("e.txt")), Language::Generic);
        // New language coverage
        assert_eq!(detect_language(Path::new("Foo.java")), Language::Java);
        assert_eq!(detect_language(Path::new("foo.c")), Language::C);
        assert_eq!(detect_language(Path::new("foo.h")), Language::C);
        assert_eq!(detect_language(Path::new("foo.cpp")), Language::Cpp);
        assert_eq!(detect_language(Path::new("foo.hpp")), Language::Cpp);
        assert_eq!(detect_language(Path::new("Foo.cs")), Language::CSharp);
        assert_eq!(detect_language(Path::new("Foo.kt")), Language::Kotlin);
        assert_eq!(detect_language(Path::new("Foo.swift")), Language::Swift);
        assert_eq!(detect_language(Path::new("Foo.scala")), Language::Scala);
        assert_eq!(detect_language(Path::new("foo.php")), Language::Php);
        // Case-insensitive
        assert_eq!(detect_language(Path::new("FOO.JAVA")), Language::Java);
    }

    #[test]
    fn java_compresses_methods_but_keeps_class_signatures_visible() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
package com.example;

public class UserService {
    public User findById(long id) {
        if (id <= 0) {
            throw new IllegalArgumentException(\"bad id\");
        }
        User user = repo.fetch(id);
        if (user == null) throw new NotFound();
        return user;
    }

    public void delete(long id) {
        repo.remove(id);
    }
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got: {}", c.text);
        // Class declaration must remain visible — eliding it would hide
        // the entire method list, defeating the purpose.
        assert!(c.text.contains("public class UserService {"));
        // Long method elided.
        assert!(c.text.contains("public User findById"));
        assert!(c.text.contains("DRIP-elided"));
        // Short method kept inline.
        assert!(c.text.contains("repo.remove(id);"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn control_flow_blocks_are_never_elided() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public int compute(int n) {
    if (n < 0) {
        n = -n;
        n = n * 2;
        n = n + 1;
        n = n - 1;
    }
    return n;
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        // The OUTER method body is elided (≥4 lines). The INNER `if`
        // would be inside that elided body, so we shouldn't see its
        // contents in the output.
        assert!(c.text.contains("DRIP-elided"));
        // But the if block's contents should NOT appear separately —
        // verify we didn't accidentally collapse just the `if {}`.
        let elision_count = c.text.matches("DRIP-elided").count();
        assert_eq!(
            elision_count, 1,
            "should elide the method body, NOT the inner if-block separately: {}",
            c.text
        );
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn cpp_compresses_function_with_namespace_qualified_name() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
#include <vector>

void Foo::bar(int x) {
    std::vector<int> v;
    v.push_back(x);
    v.push_back(x + 1);
    v.push_back(x + 2);
    process(v);
}
";
        let c = compress(src, Language::Cpp).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        assert!(c.text.contains("void Foo::bar(int x) {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_func_keyword_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Repo {
    suspend fun fetchAll(): List<User> {
        val raw = api.list()
        val parsed = raw.map { it.toUser() }
        val filtered = parsed.filter { it.active }
        return filtered.sortedBy { it.id }
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got: {}", c.text);
        assert!(c.text.contains("class Repo {"));
        assert!(c.text.contains("suspend fun fetchAll"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn swift_func_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
import Foundation

class Service {
    func fetchUsers(completion: @escaping ([User]) -> Void) {
        let url = URL(string: \"https://api.example.com\")!
        let task = session.dataTask(with: url) { data, _, error in
            guard let data = data, error == nil else { return }
            let users = try? decoder.decode([User].self, from: data)
            completion(users ?? [])
        }
        task.resume()
    }
}
";
        let c = compress(src, Language::Swift).expect("expected compression");
        assert!(c.functions_elided >= 1, "got: {}", c.text);
        assert!(c.text.contains("class Service {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn php_function_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "<?php
class UserController {
    public function show(int $id) {
        $user = $this->repo->find($id);
        if (!$user) {
            return response()->json(['error' => 'not found'], 404);
        }
        return response()->json($user);
    }
}
";
        let c = compress(src, Language::Php).expect("expected compression");
        assert_eq!(c.functions_elided, 1);
        assert!(c.text.contains("class UserController {"));
        assert!(c.text.contains("public function show"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn csharp_method_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
namespace MyApp.Services {
    public class UserService {
        public async Task<User> GetUserAsync(long id) {
            if (id <= 0) throw new ArgumentException(\"bad id\");
            var user = await _repo.FetchAsync(id);
            if (user == null) throw new NotFoundException();
            _logger.LogInformation(\"Fetched user {Id}\", id);
            return user;
        }
    }
}
";
        let c = compress(src, Language::CSharp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got: {}", c.text);
        assert!(c.text.contains("public class UserService {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn csharp_allman_method_compresses() {
        // Allman brace style is the C# default — `{` lives on its own
        // line below the signature. The original brace heuristic only
        // matched K&R, so every Allman-styled C# method slipped past.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class OrderService
{
    public async Task<Order?> GetByIdAsync(Guid id, CancellationToken ct)
    {
        _logger.LogDebug(\"Fetching {OrderId}\", id);
        var q = _db.Orders.AsNoTracking();
        q = q.Include(o => o.Items);
        q = q.Where(o => o.Id == id);
        return await q.FirstOrDefaultAsync(ct);
    }
}
";
        let c = compress(src, Language::CSharp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got: {}", c.text);
        assert!(c.text.contains("public async Task<Order?> GetByIdAsync"));
        // The lone Allman `{` line stays in the output; only the body
        // between it and the matching `}` is elided.
        assert!(c.text.contains("DRIP-elided"));
        // Class header and closing brace also preserved.
        assert!(c.text.contains("public class OrderService"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn csharp_allman_attributes_stay_with_signature() {
        // Attributes like [Authorize] / [HttpGet] live on lines above
        // the signature. They are not signatures themselves and must be
        // preserved verbatim — never elided as part of a body.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class UsersController
{
    [HttpGet(\"/users/{id}\")]
    [Authorize(Roles = \"admin\")]
    public async Task<IActionResult> Get(long id)
    {
        var u = await _svc.LookupAsync(id);
        if (u is null) return NotFound();
        var dto = _mapper.Map<UserDto>(u);
        _logger.LogInformation(\"served {Id}\", id);
        return Ok(dto);
    }
}
";
        let c = compress(src, Language::CSharp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("[HttpGet(\"/users/{id}\")]"));
        assert!(c.text.contains("[Authorize(Roles = \"admin\")]"));
        assert!(c.text.contains("public async Task<IActionResult> Get"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn csharp_primary_constructor_class_is_not_elided_as_method() {
        // `public class Foo(int x)` is a C# 12 primary-constructor
        // class declaration. The brace-balanced body is the CLASS body
        // — eliding it would erase every inner method signature, which
        // is exactly the structural information we want to preserve.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class CartTotals(decimal subtotal, decimal tax, decimal shipping)
{
    public decimal Subtotal { get; } = subtotal;
    public decimal Tax { get; } = tax;
    public decimal Shipping { get; } = shipping;
    public decimal Grand => Subtotal + Tax + Shipping;
    public bool IsTaxFree => Tax == 0m;
}
";
        // Either no compression at all, or specifically: the inner
        // method bodies are not elided (and the class body certainly
        // isn't elided as if it were a method).
        let result = compress(src, Language::CSharp);
        if let Some(c) = result {
            assert_eq!(
                c.functions_elided, 0,
                "primary-constructor class was incorrectly elided as a method: {}",
                c.text
            );
        }
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn csharp_record_with_semicolon_is_left_alone() {
        // Positional records that end in `;` have no body. The
        // compressor must leave them entirely alone (no signature
        // misclassification, no spurious elision attempt).
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public record CreateOrderLine(string Sku, int Quantity, decimal UnitPrice);
public record class Money(decimal Amount, string Currency);

public class Holder
{
    public void Use(CreateOrderLine line)
    {
        var s = line.Sku;
        var q = line.Quantity;
        var p = line.UnitPrice;
        Console.WriteLine($\"{s} x{q} @ {p}\");
        Console.WriteLine($\"total = {q * p}\");
    }
}
";
        let c = compress(src, Language::CSharp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        // Both record lines untouched.
        assert!(c.text.contains(
            "public record CreateOrderLine(string Sku, int Quantity, decimal UnitPrice);"
        ));
        assert!(c
            .text
            .contains("public record class Money(decimal Amount, string Currency);"));
        // The actual method's body got elided.
        assert!(c.text.contains("public void Use(CreateOrderLine line)"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn csharp_allman_with_generic_constraints_compresses() {
        // C# generics often carry a `where T : ...` constraint clause
        // after the parameter list. Allman places `{` on the next line
        // after the constraint — the signature line ends with
        // `class` / `struct` / `new()`, not `)`.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class Repo
{
    public T LoadOr<T>(Guid id, T fallback) where T : class, new()
    {
        var raw = _store.Read(id);
        if (raw is null) return fallback;
        var typed = _serializer.Deserialize<T>(raw);
        if (typed is null) return fallback;
        return typed;
    }
}
";
        let c = compress(src, Language::CSharp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("where T : class, new()"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_data_class_is_not_elided_as_method() {
        // `data class` is a Kotlin keyword. The class body holds
        // val/var declarations that the agent must see — eliding it
        // (because `data class Foo(val x: Int) {` ends with `{` and
        // contains `(...)`) would erase exactly the structural info
        // we care about.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
data class User(val id: UUID, val email: String, val displayName: String) {
    val isEmpty: Boolean = email.isEmpty()
    val isCorporate: Boolean = email.endsWith(\"@corp.example\")
    val displayLabel: String = if (isEmpty) \"(no email)\" else displayName
    fun isLikelyBot(): Boolean {
        val lower = email.lowercase()
        val patterns = listOf(\"bot\", \"noreply\", \"daemon\", \"automation\")
        return patterns.any { lower.contains(it) }
    }
}
";
        let result = compress(src, Language::Kotlin);
        // The data class itself must not be elided as a method body.
        // The inner method `isLikelyBot` may or may not be elided
        // depending on body length — that's fine — but the class
        // header must remain visible either way.
        if let Some(c) = &result {
            assert!(c.text.contains("data class User"));
            assert!(c.text.contains("val isEmpty"));
            assert!(c.text.contains("val isCorporate"));
        }
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_open_class_is_not_elided_as_method() {
        // `open class` allows subclassing — same risk as `data class`.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
open class Repo(private val db: Database) {
    open suspend fun load(id: UUID): Entity? {
        val raw = db.read(id)
        if (raw == null) return null
        val parsed = parse(raw)
        if (parsed == null) return null
        return validate(parsed)
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert!(c.text.contains("open class Repo"));
        // The inner method *can* be elided, but the class header stays.
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_value_class_is_not_elided_as_method() {
        // `value class` (formerly `inline class`) is a singleton
        // wrapper around a primitive — body holds member functions
        // that must stay visible.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
@JvmInline
value class Cents(val amount: Long) {
    operator fun plus(other: Cents): Cents = Cents(amount + other.amount)
    operator fun minus(other: Cents): Cents = Cents(amount - other.amount)
    fun toDollars(): Double = amount / 100.0
    override fun toString(): String = \"$\" + (amount / 100.0)
}
";
        let result = compress(src, Language::Kotlin);
        if let Some(c) = &result {
            assert!(c.text.contains("value class Cents"));
            assert!(c.text.contains("operator fun plus"));
            assert!(c.text.contains("operator fun minus"));
        }
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_fun_interface_is_not_elided_as_method() {
        // `fun interface` is Kotlin's SAM (Single Abstract Method)
        // interface keyword. The `fun` here is a *modifier* on the
        // interface, not the start of a function declaration —
        // eliding the interface body would erase the abstract method
        // signature.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
fun interface Predicate<T> {
    fun test(item: T): Boolean
}

class Holder {
    fun filter(items: List<Int>, p: Predicate<Int>): List<Int> {
        val out = mutableListOf<Int>()
        for (it in items) {
            if (p.test(it)) out.add(it)
        }
        return out
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert!(c.text.contains("fun interface Predicate<T>"));
        assert!(c.text.contains("fun test(item: T): Boolean"));
        // The implementation method `filter` *can* be elided.
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_multi_line_signature_compresses() {
        // Long parameter lists wrap across several lines per the
        // Kotlin style guide. The closing `): Type {` line carries the
        // body-opening brace but doesn't contain `(`, so the
        // single-line K&R heuristic misses it. The multi-line K&R
        // path detects this by walking back to the line that opened
        // the unmatched `(`.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Service {
    suspend fun processOrder(
        orderId: UUID,
        customerId: UUID,
        amount: Money,
        currency: String,
    ): OrderResult {
        validate(orderId)
        val ctx = lookup(customerId)
        val converted = convert(amount, currency)
        val saved = persist(orderId, ctx, converted)
        publish(saved)
        return OrderResult.success(saved)
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        // Param lines must remain in the output verbatim.
        assert!(c.text.contains("orderId: UUID,"));
        assert!(c.text.contains("currency: String,"));
        assert!(c.text.contains("): OrderResult {"));
        // And of course the body got the placeholder.
        assert!(c.text.contains("DRIP-elided"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_value_initializer_with_trailing_lambda_is_not_elided() {
        // The lookback heuristic for multi-line K&R must not
        // mis-classify `val x = build(...) { ... }` as a method
        // declaration. The `=` before the `(` is the giveaway.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Loader {
    fun setup(): Config {
        val cfg = configure(
            host,
            port,
            tls,
        ) {
            timeout = 30
            retries = 5
            backoff = 2.0
            useGzip = true
            useHttp2 = true
        }
        return cfg
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        // We expect the *outer* `setup()` method to be elided
        // (its body is long enough), but the inner `configure(...) {
        // ... }` block must NOT be detected as a separate method,
        // i.e. we should see exactly one elision, not two.
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_extension_fun_with_generics_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
fun <T> Flow<T>.bufferedPaged(pageSize: Int): Flow<List<T>> {
    val buffer = ArrayList<T>(pageSize)
    collect { value ->
        buffer += value
        if (buffer.size >= pageSize) {
            emit(buffer.toList())
            buffer.clear()
        }
    }
    return buffer
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("fun <T> Flow<T>.bufferedPaged"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_companion_object_fun_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Repo {
    companion object {
        fun fromConfig(config: Config): Repo {
            val client = HttpClient(config.host, config.port)
            val cache = LruCache<String, Any>(config.cacheSize)
            val mapper = ObjectMapper().apply { registerModule(KotlinModule()) }
            val timeoutMs = config.timeoutSeconds * 1000L
            val maxRetries = config.maxRetries.coerceAtLeast(1)
            return Repo(client, cache, mapper, timeoutMs, maxRetries)
        }
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("fun fromConfig"));
        assert!(c.text.contains("companion object"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_inline_reified_fun_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
inline fun <reified T> deserialize(data: String, mapper: ObjectMapper): T {
    val cleaned = data.trim().removePrefix(\"\\u0000\")
    if (cleaned.isEmpty()) {
        throw IllegalArgumentException(\"empty payload\")
    }
    val tree = mapper.readTree(cleaned)
    val node = tree.path(\"data\").takeIf { !it.isMissingNode } ?: tree
    return mapper.treeToValue(node, T::class.java)
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("inline fun <reified T> deserialize"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_visibility_modifier_fun_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Cache {
    private fun extractIdReflectively(entity: Any): UUID? {
        val cls = entity::class.java
        val field = cls.declaredFields.firstOrNull { it.name == \"id\" } ?: return null
        field.isAccessible = true
        return field.get(entity) as? UUID
    }
    internal fun buildQuery(filter: Filter): Query {
        val base = Query.from(\"items\")
        val withFilter = base.where(filter.toPredicate())
        val withSort = withFilter.orderBy(\"createdAt\")
        val limited = withSort.limit(filter.maxResults.coerceAtMost(1000))
        return limited
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert_eq!(c.functions_elided, 2, "got:\n{}", c.text);
        assert!(c.text.contains("private fun extractIdReflectively"));
        assert!(c.text.contains("internal fun buildQuery"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn kotlin_anonymous_object_override_fun_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Service {
    val cache = object : Cache<String, User> {
        override fun get(key: String): User? {
            val raw = backing.lookup(key) ?: return null
            val parsed = mapper.readValue<User>(raw)
            metrics.incrementHit(key)
            log.debug(\"cache hit for {}\", key)
            log.trace(\"value = {}\", parsed)
            return parsed
        }
    }
}
";
        let c = compress(src, Language::Kotlin).expect("expected compression");
        assert!(c.functions_elided >= 1, "got:\n{}", c.text);
        assert!(c.text.contains("override fun get"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn java_long_javadoc_is_compressed() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class Repo {
    /**
     * Finds a user by their unique identifier.
     *
     * <p>This method is cached and will return a stale result for up to
     * {@code TTL_SECONDS} seconds. Callers requiring strong consistency
     * must invalidate the cache explicitly via {@link #evictCache(UUID)}.
     *
     * <p>The lookup goes through the L1 entity cache first, then falls
     * back to the L2 region cache, then finally to the database.
     *
     * @param id the user identifier (non-null)
     * @return Optional containing the user, or empty if not found
     * @throws DataAccessException on database failure
     */
    public Optional<User> findById(UUID id) {
        Objects.requireNonNull(id);
        User cached = cache.get(id);
        if (cached != null) {
            return Optional.of(cached);
        }
        User fresh = entityManager.find(User.class, id);
        if (fresh != null) {
            cache.put(id, fresh);
        }
        return Optional.ofNullable(fresh);
    }
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        assert!(c.text.contains("[DRIP-javadoc-elided"), "got:\n{}", c.text);
        // Tags must survive verbatim.
        assert!(c.text.contains("@param id"));
        assert!(c.text.contains("@return"));
        assert!(c.text.contains("@throws"));
        // Summary must survive (first line).
        assert!(c.text.contains("Finds a user by their unique identifier"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn java_short_javadoc_is_kept_full() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class Repo {
    /**
     * Counts every active user.
     * @return total active count
     */
    public long count() {
        long total = entityManager.createQuery(\"SELECT COUNT(u) FROM User u\", Long.class)
                .getSingleResult();
        if (total < 0) {
            throw new IllegalStateException(\"negative count\");
        }
        if (total > Integer.MAX_VALUE) {
            log.warn(\"user count exceeds int range: {}\", total);
        }
        return total;
    }
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        // 4-line Javadoc must NOT be touched.
        assert!(!c.text.contains("[DRIP-javadoc-elided"), "got:\n{}", c.text);
        assert!(c.text.contains("Counts every active user"));
        assert!(c.text.contains("@return total active count"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn javadoc_compression_can_be_disabled_via_env() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        std::env::set_var("DRIP_COMPRESS_JAVADOC", "0");
        let src = "\
public class Repo {
    /**
     * Long doc.
     *
     * <p>Paragraph one explaining things at length.
     * <p>Paragraph two with even more details.
     * <p>Paragraph three for good measure.
     *
     * @param id the id
     * @return result
     */
    public User findById(UUID id) {
        return entityManager.find(User.class, id);
    }
}
";
        let c = compress(src, Language::Java);
        // With Javadoc compression off and a tiny body, there's nothing
        // left to elide → compress returns None.
        assert!(c.is_none() || !c.unwrap().text.contains("[DRIP-javadoc-elided"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
        std::env::remove_var("DRIP_COMPRESS_JAVADOC");
    }

    #[test]
    fn java_multi_line_signature_compresses() {
        // Same multi-line K&R behaviour for Java, where wrapping
        // long parameter lists is just as common.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class Reporter {
    public void emitMetric(
            String name,
            long value,
            Map<String, String> tags,
            Instant when) {
        Metric m = new Metric();
        m.setName(name);
        m.setValue(value);
        m.setTags(tags);
        m.setTimestamp(when);
        sink.write(m);
    }
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("Map<String, String> tags,"));
        assert!(c.text.contains("Instant when) {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- Rust audit ----------------------------------------------------

    #[test]
    fn rust_async_fn_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
pub async fn fetch_all(client: &Client) -> Result<Vec<Item>, Error> {
    let resp = client.get(\"/items\").send().await?;
    let parsed: Vec<Item> = resp.json().await?;
    let filtered: Vec<Item> = parsed.into_iter().filter(|x| x.active).collect();
    let sorted = sort_by_priority(filtered);
    Ok(sorted)
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("pub async fn fetch_all"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn rust_impl_block_methods_elided_individually() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
impl Repo {
    pub fn new() -> Self {
        let inner = Vec::new();
        let cap = 16;
        let count = 0;
        Self { inner, cap, count }
    }

    pub fn push(&mut self, item: Item) -> usize {
        self.inner.push(item);
        let len = self.inner.len();
        self.count = len;
        let idx = len - 1;
        idx
    }
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 2, "got:\n{}", c.text);
        assert!(c.text.contains("impl Repo {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn rust_trait_with_default_body_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
pub trait Validator {
    fn name(&self) -> &str;

    fn validate(&self, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err(\"empty\".into());
        }
        if value.len() > 256 {
            return Err(\"too long\".into());
        }
        Ok(())
    }
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("pub trait Validator"));
        assert!(c.text.contains("fn name(&self) -> &str;"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn rust_multi_line_signature_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
pub fn build_session(
    user_id: Uuid,
    ip: IpAddr,
    user_agent: String,
    config: &SessionConfig,
) -> Result<Session, Error> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at = now + config.absolute_timeout;
    let session = Session::new(id, user_id, ip, user_agent, now, expires_at);
    Ok(session)
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("user_id: Uuid,"));
        assert!(c.text.contains(") -> Result<Session, Error> {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn rust_struct_literal_is_not_a_function_open() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
pub fn make_thing() -> Thing {
    let x = 1;
    let y = 2;
    let z = 3;
    Thing { x, y, z, label: build_label(x, y, z) }
}
";
        let c = compress(src, Language::Rust).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- JavaScript / TypeScript audit -----------------------------------

    #[test]
    fn js_async_function_declaration_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
async function fetchAndStore(url, store) {
    const resp = await fetch(url);
    const json = await resp.json();
    const sanitised = sanitise(json);
    const persisted = await store.write(sanitised);
    return persisted;
}
";
        let c = compress(src, Language::JavaScript).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn js_generator_function_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
function* range(start, end, step) {
    let value = start;
    while (value < end) {
        yield value;
        const next = value + step;
        value = next;
    }
}
";
        let c = compress(src, Language::JavaScript).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn ts_class_methods_with_modifiers_compress() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
export class Service {
    public async fetch<T>(url: string): Promise<T> {
        const resp = await this.http.get(url);
        const text = await resp.text();
        const parsed = JSON.parse(text) as T;
        const cached = await this.cache.set(url, parsed);
        return cached;
    }

    private retry<T>(fn: () => Promise<T>, attempts: number): Promise<T> {
        const cap = Math.min(attempts, 5);
        const factor = 1.7;
        const out = this.runWithBackoff(fn, cap, factor);
        const wrapped = this.attachAbortLogic(out);
        return wrapped;
    }
}
";
        let c = compress(src, Language::TypeScript).expect("expected compression");
        assert_eq!(c.functions_elided, 2, "got:\n{}", c.text);
        assert!(c.text.contains("public async fetch<T>"));
        assert!(c.text.contains("private retry<T>"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn ts_type_guard_signature_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
export function isUser(value: unknown): value is User {
    if (value === null) return false;
    if (typeof value !== \"object\") return false;
    const v = value as Record<string, unknown>;
    if (typeof v.id !== \"string\") return false;
    if (typeof v.email !== \"string\") return false;
    return true;
}
";
        let c = compress(src, Language::TypeScript).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("isUser(value: unknown): value is User"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn ts_export_class_is_not_elided() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
export class Calculator {
    add(a: number, b: number): number {
        const x = a + 0;
        const y = b + 0;
        const z = x + y;
        const out = z;
        return out;
    }
}
";
        let c = compress(src, Language::TypeScript).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("export class Calculator"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- Go audit --------------------------------------------------------

    #[test]
    fn go_top_level_function_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
package handlers

func ProcessOrder(ctx context.Context, order *Order) error {
    if order == nil {
        return ErrNilOrder
    }
    if err := validate(order); err != nil {
        return err
    }
    if err := persist(ctx, order); err != nil {
        return err
    }
    return nil
}
";
        let c = compress(src, Language::Go).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("func ProcessOrder"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn go_method_receiver_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
func (s *Server) HandleCreate(w http.ResponseWriter, r *http.Request) {
    var req CreateRequest
    if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
        http.Error(w, err.Error(), http.StatusBadRequest)
        return
    }
    user, err := s.store.Create(r.Context(), &req)
    if err != nil {
        http.Error(w, err.Error(), http.StatusInternalServerError)
        return
    }
    w.WriteHeader(http.StatusCreated)
    json.NewEncoder(w).Encode(user)
}
";
        let c = compress(src, Language::Go).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("func (s *Server) HandleCreate"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn go_multi_line_signature_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
func ListUsers(
    ctx context.Context,
    filter *UserFilter,
    page int,
    pageSize int,
) ([]*User, int, error) {
    items, err := store.Find(ctx, filter, page, pageSize)
    if err != nil {
        return nil, 0, fmt.Errorf(\"list users: %w\", err)
    }
    total, err := store.Count(ctx, filter)
    if err != nil {
        return nil, 0, fmt.Errorf(\"count users: %w\", err)
    }
    return items, total, nil
}
";
        let c = compress(src, Language::Go).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("ctx context.Context,"));
        assert!(c.text.contains(") ([]*User, int, error) {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn go_interface_methods_stay_visible() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
type Store interface {
    Get(ctx context.Context, id string) (*Item, error)
    List(ctx context.Context, q string) ([]*Item, error)
    Save(ctx context.Context, item *Item) error
    Delete(ctx context.Context, id string) error
}

func RunSync(ctx context.Context, store Store) error {
    items, err := store.List(ctx, \"\")
    if err != nil {
        return err
    }
    for _, item := range items {
        if err := store.Save(ctx, item); err != nil {
            return err
        }
    }
    return nil
}
";
        let c = compress(src, Language::Go).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("type Store interface"));
        assert!(c
            .text
            .contains("Get(ctx context.Context, id string) (*Item, error)"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- C audit ---------------------------------------------------------

    #[test]
    fn c_function_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
#include <stddef.h>
#include <string.h>

static size_t parse_header(const char *buf, size_t len, header_t *out) {
    if (buf == NULL || out == NULL) return 0;
    if (len < HEADER_MIN_LEN) return 0;
    out->magic = read_u32_le(buf);
    out->version = read_u16_le(buf + 4);
    out->payload_len = read_u32_le(buf + 8);
    return HEADER_FIXED_LEN + out->payload_len;
}
";
        let c = compress(src, Language::C).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("static size_t parse_header"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn c_multi_line_function_signature_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
int compress_block(
        const uint8_t *src,
        size_t src_len,
        uint8_t *dst,
        size_t dst_cap,
        size_t *dst_len) {
    if (src == NULL || dst == NULL || dst_len == NULL) return -1;
    if (src_len == 0) {
        *dst_len = 0;
        return 0;
    }
    int rc = lz_compress(src, src_len, dst, dst_cap, dst_len);
    if (rc < 0) return rc;
    return 0;
}
";
        let c = compress(src, Language::C).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("size_t *dst_len) {"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn c_function_pointer_assignment_is_not_elided() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
int (*global_handler)(int) = NULL;

void register_handler(int (*fp)(int)) {
    if (fp == NULL) return;
    global_handler = fp;
    on_handler_registered();
    audit_log(\"handler set\");
    g_handler_count += 1;
}
";
        let c = compress(src, Language::C).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- C++ extra audit -------------------------------------------------

    #[test]
    fn cpp_template_method_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
template <typename T>
std::optional<T> Cache::get(const std::string &key) const {
    auto it = entries_.find(key);
    if (it == entries_.end()) return std::nullopt;
    if (is_expired(it->second.stored_at)) return std::nullopt;
    auto raw = it->second.bytes;
    auto parsed = deserialize<T>(raw);
    return parsed;
}
";
        let c = compress(src, Language::Cpp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("std::optional<T> Cache::get"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn cpp_constructor_with_member_init_list_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
Connection::Connection(const Endpoint &ep, std::chrono::seconds timeout)
    : endpoint_(ep), timeout_(timeout), state_(State::Connecting) {
    socket_.bind(ep);
    socket_.set_timeout(timeout);
    handshake_.begin();
    state_machine_.start();
    metrics_.connections_initiated += 1;
}
";
        let c = compress(src, Language::Cpp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("Connection::Connection"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- Swift audit -----------------------------------------------------

    #[test]
    fn swift_method_with_throws_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
struct Loader {
    func load(from url: URL) async throws -> [Item] {
        let (data, response) = try await session.data(from: url)
        guard let http = response as? HTTPURLResponse else {
            throw LoaderError.notHTTP
        }
        guard http.statusCode == 200 else {
            throw LoaderError.status(http.statusCode)
        }
        let items = try decoder.decode([Item].self, from: data)
        return items
    }
}
";
        let c = compress(src, Language::Swift).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("func load(from url: URL) async throws"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn swift_init_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Cache {
    init(capacity: Int, ttl: TimeInterval, evictionPolicy: EvictionPolicy) {
        self.capacity = capacity
        self.ttl = ttl
        self.evictionPolicy = evictionPolicy
        self.storage = [:]
        self.accessOrder = []
        self.hits = 0
    }
}
";
        let c = compress(src, Language::Swift).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("init(capacity: Int, ttl: TimeInterval"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn swift_extension_methods_compress() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
extension String {
    func sanitised() -> String {
        let trimmed = self.trimmingCharacters(in: .whitespaces)
        let lowered = trimmed.lowercased()
        let folded = lowered.folding(options: .diacriticInsensitive, locale: .current)
        let stripped = folded.replacingOccurrences(of: \"_\", with: \" \")
        return stripped
    }
}
";
        let c = compress(src, Language::Swift).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("extension String {"));
        assert!(c.text.contains("func sanitised()"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- Scala audit -----------------------------------------------------

    #[test]
    fn scala_def_method_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class UserService(repo: UserRepo) {
    def createUser(email: String, name: String): Either[Error, User] = {
        if (email.isEmpty) return Left(Error.EmailRequired)
        if (!email.contains(\"@\")) return Left(Error.InvalidEmail)
        val normalised = email.trim.toLowerCase
        val user = User(UUID.randomUUID(), normalised, name, Instant.now())
        val saved = repo.insert(user)
        Right(saved)
    }
}
";
        let c = compress(src, Language::Scala).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("class UserService"));
        assert!(c.text.contains("def createUser"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn scala_case_class_is_not_elided() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
case class Money(amount: BigDecimal, currency: String) {
    def +(other: Money): Money = {
        require(currency == other.currency, \"currency mismatch\")
        val total = amount + other.amount
        val out = Money(total, currency)
        val rounded = out.rounded
        rounded
    }
}
";
        let c = compress(src, Language::Scala).expect("expected compression");
        assert!(c.text.contains("case class Money"));
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn scala_object_methods_compress() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
object PriceFormatter {
    def format(amount: BigDecimal, currency: String, locale: Locale): String = {
        val rounded = amount.setScale(2, BigDecimal.RoundingMode.HALF_EVEN)
        val symbol = currencySymbol(currency, locale)
        val parts = rounded.toString.split(\"\\\\.\")
        val whole = groupDigits(parts(0), locale)
        val decimal = parts.lift(1).getOrElse(\"00\")
        symbol + whole + decimalSep(locale) + decimal
    }
}
";
        let c = compress(src, Language::Scala).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("object PriceFormatter"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- PHP audit -------------------------------------------------------

    #[test]
    fn php_class_method_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
<?php

class Mailer {
    public function send(string $to, string $subject, string $body): bool {
        if (!filter_var($to, FILTER_VALIDATE_EMAIL)) {
            return false;
        }
        $headers = $this->buildHeaders($to, $subject);
        $envelope = $this->buildEnvelope($to, $subject, $body);
        $result = $this->transport->deliver($envelope, $headers);
        $this->logger->info('sent', ['to' => $to, 'ok' => $result]);
        return $result;
    }
}
";
        let c = compress(src, Language::Php).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("public function send"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn php_static_method_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
<?php

class Tools {
    public static function normalize(string $value): string {
        $trimmed = trim($value);
        $lowered = strtolower($trimmed);
        $folded = iconv('UTF-8', 'ASCII//TRANSLIT', $lowered);
        $cleaned = preg_replace('/[^a-z0-9]+/', '-', $folded);
        $stripped = trim($cleaned ?? '', '-');
        return $stripped;
    }
}
";
        let c = compress(src, Language::Php).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("public static function normalize"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    // ---- Java extra audit ------------------------------------------------

    #[test]
    fn java_annotated_method_with_throws_compresses() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public class Handler {
    @Override
    @Transactional(readOnly = false)
    public Result process(Input input) throws ValidationException, IOException {
        if (input == null) {
            throw new ValidationException(\"null input\");
        }
        Result r = new Result();
        r.setStartedAt(Instant.now());
        r.setInput(input);
        r.setStatus(Status.PROCESSED);
        return r;
    }
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("@Override"));
        assert!(c.text.contains("@Transactional(readOnly = false)"));
        assert!(c.text.contains("public Result process"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn java_record_with_body_keeps_inner_methods_visible() {
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
public record Range(int low, int high) {
    public Range {
        if (low > high) throw new IllegalArgumentException(\"low > high\");
    }

    public int span() {
        int diff = high - low;
        int abs = Math.abs(diff);
        int normalised = abs;
        int adjusted = normalised + 0;
        return adjusted;
    }
}
";
        let c = compress(src, Language::Java).expect("expected compression");
        assert!(c.text.contains("public record Range"));
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c.text.contains("public int span()"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }

    #[test]
    fn cpp_allman_method_compresses() {
        // Allman is also common in C++. Same compressor pathway.
        let _g = lock_env();
        std::env::set_var("DRIP_COMPRESS_MIN_BYTES", "0");
        std::env::set_var("DRIP_COMPRESS_MIN_BODY", "4");
        let src = "\
class Parser
{
    int parse_value(std::string_view in) const
    {
        if (in.empty()) return -1;
        auto t = next_token(in);
        if (!t) return -1;
        process(t);
        finalize();
        return 0;
    }
};
";
        let c = compress(src, Language::Cpp).expect("expected compression");
        assert_eq!(c.functions_elided, 1, "got:\n{}", c.text);
        assert!(c
            .text
            .contains("int parse_value(std::string_view in) const"));
        std::env::remove_var("DRIP_COMPRESS_MIN_BYTES");
    }
}
