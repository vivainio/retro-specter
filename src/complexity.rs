//! Indentation-based complexity.
//!
//! Each logical statement contributes its indentation level; a file's
//! complexity is the sum. Blank lines, comments, docstrings, preprocessor
//! directives and brace/bracket-only lines are ignored, and continuation lines
//! (wrapped arguments, method chains, initializers) are folded into the
//! statement they continue, so that formatting style does not affect the score.

use crate::structure::{self, Decl, Structure};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    CSharp,
    Python,
}

impl Language {
    pub const EXTENSIONS: &[(&str, Language)] = &[
        ("cs", Language::CSharp),
        ("py", Language::Python),
        ("pyi", Language::Python),
    ];

    pub fn name(self) -> &'static str {
        match self {
            Language::CSharp => "csharp",
            Language::Python => "python",
        }
    }

    pub fn from_path(path: &str) -> Option<Language> {
        let (_, ext) = path.rsplit_once('.')?;
        Self::EXTENSIONS
            .iter()
            .find(|(e, _)| e.eq_ignore_ascii_case(ext))
            .map(|&(_, lang)| lang)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Stats {
    /// Logical statements.
    pub lines: u32,
    /// Sum of indentation levels.
    pub total: u64,
    /// Deepest indentation level.
    pub max: u32,
}

impl Stats {
    pub(crate) fn push(&mut self, level: u32) {
        self.lines += 1;
        self.total += level as u64;
        self.max = self.max.max(level);
    }

    pub fn merge(&mut self, other: &Stats) {
        self.lines += other.lines;
        self.total += other.total;
        self.max = self.max.max(other.max);
    }
}

/// A logical statement: one code line plus any continuation lines.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stmt {
    /// 0-based first / last physical line.
    pub first: u32,
    pub last: u32,
    pub level: u32,
    /// Hash of the statement text with all whitespace removed, used to
    /// recognize statements that were only reformatted.
    pub key: u64,
}

impl Stmt {
    /// Whether any 1-based `(start, count)` range overlaps this statement.
    fn overlaps(&self, ranges: &[(u32, u32)]) -> bool {
        ranges
            .iter()
            .any(|&(s, c)| c > 0 && s <= self.last + 1 && s + c > self.first + 1)
    }
}

pub struct Levels {
    stmts: Vec<Stmt>,
    /// Function / class declarations by statement index.
    decls: Vec<(usize, Decl)>,
}

/// Complexity of the statements a diff really changed.
pub struct Churn {
    pub added: Stats,
    pub removed: Stats,
    /// 1-based `(start, count)` line ranges of the changed statements.
    pub added_ranges: Vec<(u32, u32)>,
    pub removed_ranges: Vec<(u32, u32)>,
}

impl Levels {
    pub fn analyze(src: &str, lang: Language, tab_width: usize) -> Levels {
        struct Raw {
            first: usize,
            last: usize,
            width: usize,
            /// 1 for the inline body of a one-line `if x: y`, which counts one level deeper.
            extra: u32,
            /// Inside a C# block: (index of the statement owning the block,
            /// width of the block's first statement).
            frame: Option<(usize, usize)>,
            text: String,
        }
        let src = src.strip_prefix('\u{feff}').unwrap_or(src);
        let mut block_comment = false;
        let mut py_string: Option<&'static str> = None;
        let mut tracker = Tracker::new(lang);
        let mut raw: Vec<Raw> = Vec::new();
        let mut decls = Vec::new();

        for (i, line) in src.lines().enumerate() {
            let kind = match lang {
                Language::CSharp => csharp_classify(line, &mut block_comment),
                Language::Python => python_classify(line, &mut py_string),
            };
            match kind {
                LineKind::Skip(rest) => tracker.feed(rest),
                LineKind::Punct(t) => {
                    // Part of the statement if it closes a continuation
                    // (`);`) or opens one (`{` of an initializer).
                    let before = tracker.continues(t);
                    tracker.anchor = raw.len().checked_sub(1);
                    tracker.feed(t);
                    if (before || tracker.in_expression())
                        && let Some(r) = raw.last_mut()
                    {
                        r.last = i;
                        r.text.push_str(t);
                    }
                }
                LineKind::Code(t) => {
                    let width = indent_width(line, tab_width);
                    // Closing braces leading the line (`} else {`) end their
                    // blocks before the rest of the line is placed.
                    let t = match lang {
                        Language::CSharp => {
                            let rest = t.trim_start_matches(['}', ' ', '\t']);
                            tracker.feed(&t[..t.len() - rest.len()]);
                            rest
                        }
                        Language::Python => t,
                    };
                    let mut cont = tracker.continues(t);
                    // Continuation lines never dedent past their statement;
                    // if one does, bracket tracking went wrong. Recover.
                    if cont && raw.last().is_none_or(|r| width < r.width) {
                        tracker.reset();
                        cont = false;
                    }
                    match raw.last_mut() {
                        Some(r) if cont => {
                            r.last = i;
                            r.text.push_str(t);
                        }
                        _ => {
                            if let Some(d) = structure::detect(lang, t) {
                                decls.push((raw.len(), d));
                            }
                            // `if (x) y;` counts like its two-line form.
                            let (head, body) = match inline_body(lang, t) {
                                Some(k) => (&t[..k], Some(&t[k..])),
                                None => (t, None),
                            };
                            let frame = tracker
                                .frames
                                .last_mut()
                                .map(|f| (f.anchor, *f.first_width.get_or_insert(width)));
                            raw.push(Raw {
                                first: i,
                                last: i,
                                width,
                                extra: 0,
                                frame,
                                text: head.to_string(),
                            });
                            if let Some(body) = body {
                                raw.push(Raw {
                                    first: i,
                                    last: i,
                                    width,
                                    extra: 1,
                                    frame,
                                    text: body.to_string(),
                                });
                            }
                        }
                    }
                    tracker.anchor = raw.len().checked_sub(1);
                    tracker.feed(t);
                }
            }
        }

        let unit = detect_unit(raw.iter().filter(|r| r.extra == 0).map(|r| r.width));
        let mut stmts: Vec<Stmt> = Vec::with_capacity(raw.len());
        for r in raw {
            // A C# block nests one level below the statement owning it;
            // indentation only matters relative to the block's first statement.
            let level = match r.frame {
                Some((anchor, first)) => {
                    stmts[anchor].level + 1 + (r.width.saturating_sub(first) / unit) as u32
                }
                None => (r.width / unit) as u32,
            } + r.extra;
            stmts.push(Stmt {
                first: r.first as u32,
                last: r.last as u32,
                level,
                key: stmt_key(&r.text),
            });
        }
        Levels { stmts, decls }
    }

    pub fn structure(&self) -> Structure {
        structure::build(&self.stmts, &self.decls)
    }

    pub fn stats(&self) -> Stats {
        let mut s = Stats::default();
        self.stmts.iter().for_each(|st| s.push(st.level));
        s
    }

    /// Stats of statements touching the given 1-based `(start, count)` line ranges.
    #[cfg(test)]
    pub fn stats_in(&self, ranges: &[(u32, u32)]) -> Stats {
        let mut s = Stats::default();
        self.touched(ranges).for_each(|st| s.push(st.level));
        s
    }

    fn touched<'a>(&'a self, ranges: &'a [(u32, u32)]) -> impl Iterator<Item = &'a Stmt> {
        self.stmts.iter().filter(move |s| s.overlaps(ranges))
    }
}

/// Statements touched by a diff, minus pairs of removed / added statements
/// that are identical apart from whitespace at the same nesting level.
pub fn churn(
    old: Option<&Levels>,
    removed: &[(u32, u32)],
    new: Option<&Levels>,
    added: &[(u32, u32)],
) -> Churn {
    let old_t: Vec<&Stmt> = old
        .map(|l| l.touched(removed).collect())
        .unwrap_or_default();
    let new_t: Vec<&Stmt> = new.map(|l| l.touched(added).collect()).unwrap_or_default();

    let count = |v: &[&Stmt]| {
        let mut m: HashMap<(u64, u32), usize> = HashMap::new();
        v.iter()
            .for_each(|s| *m.entry((s.key, s.level)).or_default() += 1);
        m
    };
    let (old_c, new_c) = (count(&old_t), count(&new_t));
    let matched: HashMap<(u64, u32), usize> = old_c
        .iter()
        .filter_map(|(k, &n)| new_c.get(k).map(|&m| (*k, n.min(m))))
        .collect();

    let leftover = |v: &[&Stmt]| {
        let mut skip = matched.clone();
        let mut stats = Stats::default();
        let mut ranges = Vec::new();
        for s in v {
            if let Some(n) = skip.get_mut(&(s.key, s.level))
                && *n > 0
            {
                *n -= 1;
                continue;
            }
            stats.push(s.level);
            ranges.push((s.first + 1, s.last - s.first + 1));
        }
        (stats, ranges)
    };
    let (added, added_ranges) = leftover(&new_t);
    let (removed, removed_ranges) = leftover(&old_t);
    Churn {
        added,
        removed,
        added_ranges,
        removed_ranges,
    }
}

fn stmt_key(text: &str) -> u64 {
    let norm: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    // `} else {` / `else` and `{ G(); }` / `G();` differ only in brace style.
    let norm = norm.trim_matches(['{', '}']);
    let mut h = DefaultHasher::new();
    norm.hash(&mut h);
    h.finish()
}

fn indent_width(line: &str, tab_width: usize) -> usize {
    let mut w = 0;
    for c in line.chars() {
        match c {
            ' ' => w += 1,
            '\t' => w = (w / tab_width + 1) * tab_width,
            _ => break,
        }
    }
    w
}

/// The most common positive indentation step between consecutive statements.
fn detect_unit(widths: impl Iterator<Item = usize>) -> usize {
    let mut hist = [0u32; 9];
    let mut prev = 0;
    for w in widths {
        if w > prev && w - prev <= 8 {
            hist[w - prev] += 1;
        }
        prev = w;
    }
    (1..=8)
        .max_by_key(|&d| (hist[d], std::cmp::Reverse(d)))
        .filter(|&d| hist[d] > 0)
        .unwrap_or(4)
}

enum LineKind<'a> {
    /// Blank, comment or docstring line. Carries any code after a closing
    /// `*/` or `"""` so that its brackets are still tracked.
    Skip(&'a str),
    /// Only braces / brackets: not a statement, but brackets are tracked.
    Punct(&'a str),
    Code(&'a str),
}

fn is_punctuation_only(t: &str) -> bool {
    t.chars()
        .all(|c| matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';' | ',' | ':'))
}

fn csharp_classify<'a>(line: &'a str, in_block: &mut bool) -> LineKind<'a> {
    let mut t = line.trim();
    if *in_block {
        match t.find("*/") {
            Some(i) => {
                *in_block = false;
                t = t[i + 2..].trim();
            }
            None => return LineKind::Skip(""),
        }
    }
    while let Some(rest) = t.strip_prefix("/*") {
        match rest.find("*/") {
            Some(i) => t = rest[i + 2..].trim(),
            None => {
                *in_block = true;
                return LineKind::Skip("");
            }
        }
    }
    if t.is_empty() || t.starts_with("//") || t.starts_with('#') {
        return LineKind::Skip("");
    }
    if is_punctuation_only(t) {
        return LineKind::Punct(t);
    }
    // A block comment opened at the end of a code line.
    if let Some(i) = t.find("/*")
        && !t[i + 2..].contains("*/")
        && !t[..i].contains('"')
    {
        *in_block = true;
    }
    LineKind::Code(t)
}

fn python_classify<'a>(line: &'a str, in_string: &mut Option<&'static str>) -> LineKind<'a> {
    let t = line.trim();
    if let Some(delim) = *in_string {
        if t.matches(delim).count() % 2 == 1 {
            *in_string = None;
            return LineKind::Skip(&t[t.rfind(delim).unwrap() + 3..]);
        }
        return LineKind::Skip("");
    }
    if t.is_empty() || t.starts_with('#') {
        return LineKind::Skip("");
    }
    if is_punctuation_only(t) {
        return LineKind::Punct(t);
    }
    let delim = match (t.find("\"\"\""), t.find("'''")) {
        (Some(a), Some(b)) if b < a => "'''",
        (Some(_), _) => "\"\"\"",
        (None, Some(_)) => "'''",
        (None, None) => return LineKind::Code(t),
    };
    if t.matches(delim).count() % 2 == 1 {
        *in_string = Some(delim);
    }
    // A line that *starts* with a triple-quoted string is a docstring, not logic.
    if t.trim_start_matches(['r', 'R', 'b', 'B', 'u', 'U', 'f', 'F'])
        .starts_with(delim)
    {
        LineKind::Skip("")
    } else {
        LineKind::Code(t)
    }
}

/// Marker for a C# `{` that opens an expression (initializer, switch
/// expression) rather than a block of statements.
const EXPR_BRACE: u8 = b'e';

/// C# lines starting with these continue the previous statement.
const CS_CONT_START: &[&str] = &[".", "?", ":", "&&", "||", "+ ", "=>", "where "];
/// C# lines following a line ending with these continue its statement
/// (`=` also covers `==`, `+=` and friends).
const CS_CONT_END: &[&str] = &["=", "=>", "&&", "||", "+", "?", "."];

static CS_NEW_EXPR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[=(,\[?:]|=>|\breturn)\s*new\b").unwrap());

/// Whether `s` ends inside a `new ...` expression (`= new Foo(a)`), not one
/// already closed by an enclosing paren (`using (var r = new R(s))`).
fn ends_in_new_expression(s: &str) -> bool {
    let Some(m) = CS_NEW_EXPR.find_iter(s).last() else {
        return false;
    };
    let tail = &s[m.end()..];
    let mut depth = 0i32;
    for c in tail.chars() {
        match c {
            ';' | '{' | '}' => return false,
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// A C# `{ ... }` statement block being scanned.
struct Frame {
    /// Bracket stack depth of the body's `{`.
    depth: usize,
    /// Index of the statement owning the block.
    anchor: usize,
    /// Width of the body's first statement, once seen.
    first_width: Option<usize>,
}

/// Tracks open brackets across lines to recognize continuation lines.
struct Tracker {
    lang: Language,
    stack: Vec<u8>,
    /// Code part of the previous non-blank, non-comment line.
    prev_end: String,
    frames: Vec<Frame>,
    /// Statement containing the line currently being fed.
    anchor: Option<usize>,
}

impl Tracker {
    fn new(lang: Language) -> Tracker {
        Tracker {
            lang,
            stack: Vec::new(),
            prev_end: String::new(),
            frames: Vec::new(),
            anchor: None,
        }
    }

    fn reset(&mut self) {
        self.stack.clear();
        self.prev_end.clear();
        self.frames.clear();
    }

    /// Whether code line `t` continues the previous statement.
    fn continues(&self, t: &str) -> bool {
        match self.lang {
            Language::Python => self.in_expression() || self.prev_end.ends_with('\\'),
            Language::CSharp => {
                self.in_expression()
                    || CS_CONT_START.iter().any(|p| t.starts_with(p))
                    || CS_CONT_END.iter().any(|p| self.prev_end.ends_with(p))
            }
        }
    }

    /// Whether the innermost open bracket makes following lines continuations.
    fn in_expression(&self) -> bool {
        match self.lang {
            Language::Python => !self.stack.is_empty(),
            Language::CSharp => matches!(self.stack.last(), Some(b'(' | b'[' | &EXPR_BRACE)),
        }
    }

    fn feed(&mut self, t: &str) {
        if t.trim().is_empty() {
            return;
        }
        let code_len = self.scan(t);
        let code = t[..code_len].trim_end();
        if self.lang == Language::CSharp && code.ends_with(';') {
            // A statement ended; drop any parens left open by a scanning miss.
            while matches!(self.stack.last(), Some(b'(' | b'[')) {
                self.stack.pop();
            }
        }
        self.prev_end = code.to_string();
    }

    /// Updates the bracket stack; returns the length of the code before any comment.
    fn scan(&mut self, t: &str) -> usize {
        let b = t.as_bytes();
        let cs = self.lang == Language::CSharp;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'/' if cs && b.get(i + 1) == Some(&b'/') => return i,
                b'/' if cs && b.get(i + 1) == Some(&b'*') => match t[i + 2..].find("*/") {
                    Some(p) => {
                        i += p + 4;
                        continue;
                    }
                    None => return i,
                },
                b'#' if !cs => return i,
                b'"' | b'\'' => {
                    i = skip_string(b, i, cs);
                    continue;
                }
                c @ (b'(' | b'[') => self.stack.push(c),
                b'{' => {
                    let (before, prev) = (&t[..i], self.prev_end.as_str());
                    let lambda = cs && opens_lambda_block(before, prev);
                    let nested = cs && self.in_expression();
                    let expr =
                        cs && (nested && !lambda || !nested && is_expression_brace(before, prev));
                    // Every C# block nests one level below the statement owning
                    // it, whatever the brace style or argument-list indentation.
                    if cs
                        && !expr
                        && let Some(anchor) = self.anchor
                    {
                        self.frames.push(Frame {
                            depth: self.stack.len(),
                            anchor,
                            first_width: None,
                        });
                    }
                    self.stack.push(if expr { EXPR_BRACE } else { b'{' });
                }
                b')' => self.close(|c| c == b'('),
                b']' => self.close(|c| c == b'['),
                b'}' => self.close(|c| c == b'{' || c == EXPR_BRACE),
                _ => {}
            }
            i += 1;
        }
        b.len()
    }

    fn close(&mut self, opener: impl Fn(u8) -> bool) {
        if let Some(p) = self.stack.iter().rposition(|&c| opener(c)) {
            self.stack.truncate(p);
            self.frames.retain(|f| f.depth < p);
        }
    }
}

const CS_COMPOUND: &[&str] = &["if", "while", "for", "foreach", "using", "lock", "fixed"];
const PY_COMPOUND: &[&str] = &[
    "if", "elif", "else", "for", "while", "with", "try", "except", "finally", "def", "class",
    "async",
];

/// For a compound statement with its body on the same line (`if (x) y;`,
/// `if x: y`), the byte index where the body starts.
fn inline_body(lang: Language, t: &str) -> Option<usize> {
    let word_end = t
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(t.len());
    let k = match lang {
        Language::CSharp => {
            let lead = t.len() - t.trim_start_matches(['}', ' ', '\t']).len();
            let rest = &t[lead..];
            let rest_word = rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            match &rest[..rest_word] {
                "else" => {
                    let after = rest[4..].trim_start();
                    if after.starts_with("if")
                        && !after[2..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
                    {
                        lead + (rest.len() - after.len()) + close_paren_after(&after[2..])? + 2
                    } else {
                        lead + 4
                    }
                }
                w if CS_COMPOUND.contains(&w) => {
                    lead + rest_word + close_paren_after(&rest[rest_word..])?
                }
                _ => return None,
            }
        }
        Language::Python => {
            if !PY_COMPOUND.contains(&&t[..word_end]) {
                return None;
            }
            top_level_colon(t)? + 1
        }
    };
    let body = t[k..].trim();
    let body = body
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .unwrap_or(body)
        .trim();
    let empty =
        body.is_empty() || body.starts_with('{') || body.starts_with("//") || body.starts_with('#');
    (!empty).then_some(k)
}

/// Index just past the `)` matching the first `(` in `s`, which must come
/// before anything else but whitespace.
fn close_paren_after(s: &str) -> Option<usize> {
    let open = s.len() - s.trim_start().len();
    if !s[open..].starts_with('(') {
        return None;
    }
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                i = skip_string(b, i, true);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index of the first `:` outside brackets and strings (not `:=`).
fn top_level_colon(t: &str) -> Option<usize> {
    let b = t.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                i = skip_string(b, i, false);
                continue;
            }
            b'#' => return None,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b':' if depth == 0 && b.get(i + 1) != Some(&b'=') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Returns the index just past the string literal starting at `b[i]`.
fn skip_string(b: &[u8], i: usize, cs: bool) -> usize {
    let q = b[i];
    if b[i..].starts_with(&[q, q, q]) {
        return b[i + 3..]
            .windows(3)
            .position(|w| w == [q, q, q])
            .map_or(b.len(), |p| i + 3 + p + 3);
    }
    let verbatim = cs
        && q == b'"'
        && i > 0
        && (b[i - 1] == b'@' || (i > 1 && b[i - 1] == b'$' && b[i - 2] == b'@'));
    let mut j = i + 1;
    while j < b.len() {
        if !verbatim && b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == q {
            if verbatim && b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// Text before a `{` on its line, or the previous line if the brace starts the line.
fn brace_context<'a>(before: &'a str, prev_end: &'a str) -> &'a str {
    let s = before.trim_end();
    if s.is_empty() { prev_end } else { s }
}

/// Whether a C# `{` opens a lambda / anonymous-method body.
fn opens_lambda_block(before: &str, prev_end: &str) -> bool {
    let s = brace_context(before, prev_end);
    s.ends_with("=>") || s.contains("delegate")
}

/// Whether a C# `{` preceded by `before` (or the previous line) opens an
/// initializer / switch expression rather than a statement block.
fn is_expression_brace(before: &str, prev_end: &str) -> bool {
    let s = brace_context(before, prev_end);
    if s.ends_with("=>") {
        return false; // lambda body
    }
    s.ends_with(['=', '(', ',', '['])
        || s.ends_with("switch")
        || s.ends_with("return")
        || ends_in_new_expression(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_levels() {
        let src = "\
def f(x):
    \"\"\"Docstring
    spanning lines.
    \"\"\"
    # comment
    if x:
        for i in x:
            print(i)
    return [
        1,
    ]
";
        let s = Levels::analyze(src, Language::Python, 4).stats();
        // def(0) if(1) for(2) print(3) return(1); `1,` continues `return [`
        assert_eq!(s.lines, 5);
        assert_eq!(s.total, 7);
        assert_eq!(s.max, 3);
    }

    #[test]
    fn csharp_levels_two_space() {
        let src = "\
namespace A
{
  /* block
     comment */
  class B
  {
    // comment
    void M()
    {
      if (x)
      {
        Do();
      }
    }
  }
}
";
        let s = Levels::analyze(src, Language::CSharp, 4).stats();
        // namespace(0) class(1) void(2) if(3) Do(4)
        assert_eq!(s.lines, 5);
        assert_eq!(s.total, 10);
        assert_eq!(s.max, 4);
    }

    #[test]
    fn tabs_and_ranges() {
        let src = "a\n\tb\n\t\tc\n";
        let lv = Levels::analyze(src, Language::CSharp, 4);
        assert_eq!(lv.stats().total, 3);
        assert_eq!(lv.stats_in(&[(2, 2)]).total, 3);
        assert_eq!(lv.stats_in(&[(3, 5)]).total, 2);
    }

    fn cs(src: &str) -> Levels {
        Levels::analyze(src, Language::CSharp, 4)
    }

    #[test]
    fn csharp_wrapping_is_neutral() {
        let one_line = "\
class C
{
    void M()
    {
        var arg = cmd.Arg.Replace(\"a\", f).Replace(\"b\", n.ToString());
        public static string T = \"long ( string\";
        if (x) {
            Do(1, 2);
        } else {
            Other();
        }
        var p = new Person { Name = \"x\", Age = 3 };
    }
}
";
        let wrapped = "\
class C
{
    void M()
    {
        var arg = cmd.Arg
            .Replace(\"a\", f)
            .Replace(\"b\", n.ToString());
        public static string T =
            \"long ( string\";
        if (x)
        {
            Do(
                1,
                2
            );
        }
        else
        {
            Other();
        }
        var p = new Person
        {
            Name = \"x\",
            Age = 3,
        };
    }
}
";
        let (a, b) = (cs(one_line), cs(wrapped));
        let (sa, sb) = (a.stats(), b.stats());
        assert_eq!((sa.lines, sa.total), (sb.lines, sb.total));

        // Diffing the two reports no real change except the object
        // initializer's trailing comma, which is part of the statement text.
        let all = |l: &Levels| vec![(1, l.stmts.last().unwrap().last + 1)];
        let ch = churn(Some(&a), &all(&a), Some(&b), &all(&b));
        assert_eq!((ch.added.lines, ch.removed.lines), (1, 1));
    }

    #[test]
    fn csharp_lambda_blocks_are_statements() {
        let src = "\
class C
{
    void M()
    {
        items.Where(x =>
        {
            if (x.Ok)
            {
                return true;
            }
            return false;
        });
    }
}
";
        // class(0) void(1) items(2) if(3) return(4) return(3)
        let s = cs(src).stats();
        assert_eq!((s.lines, s.total), (6, 13));
    }

    #[test]
    fn python_brackets_and_backslash() {
        let src = "\
x = foo(
    a,
    b)
y = 1 + \\
    2
s = \"\"\"
text (
\"\"\".strip()
z = 3
";
        let s = Levels::analyze(src, Language::Python, 4).stats();
        assert_eq!(s.lines, 4); // x, y, s, z
        assert_eq!(s.total, 0);
    }

    #[test]
    fn inline_bodies_count_like_expanded_form() {
        let py_one = "for x in y: go(x)\nif a: b()\nelse: c()\nx = {1: 2}\n";
        let py_two = "for x in y:\n    go(x)\nif a:\n    b()\nelse:\n    c()\nx = {1: 2}\n";
        let py = |s| Levels::analyze(s, Language::Python, 4).stats();
        assert_eq!(
            (py(py_one).lines, py(py_one).total),
            (py(py_two).lines, py(py_two).total)
        );
        assert_eq!(py(py_two).total, 3);

        let cs_one =
            "if (e != null) T = e.M;\nelse if (f(a)) { G(); }\nelse H();\nif (x) {\n    Y();\n}\n";
        let cs_two = "if (e != null)\n    T = e.M;\nelse if (f(a))\n{\n    G();\n}\nelse\n    H();\nif (x)\n{\n    Y();\n}\n";
        let (a, b) = (cs(cs_one), cs(cs_two));
        assert_eq!(
            (a.stats().lines, a.stats().total),
            (b.stats().lines, b.stats().total)
        );
        assert_eq!(b.stats().total, 4);
        let all = |l: &Levels| vec![(1, l.stmts.last().unwrap().last + 1)];
        let ch = churn(Some(&a), &all(&a), Some(&b), &all(&b));
        assert_eq!((ch.added.lines, ch.removed.lines), (0, 0));
    }

    #[test]
    fn nested_collection_initializer() {
        let src = "\
var d = new Dictionary<string, int>()
{
    {
        \"a\",
        1
    },
    { \"b\", 2 },
};
list.ForEach(x =>
{
    Use(x);
});
";
        // `var d ...` is one statement; `list.ForEach` and `Use` are statements.
        let s = cs(src).stats();
        assert_eq!((s.lines, s.total), (3, 1));
    }

    #[test]
    fn lambda_body_nests_from_its_statement() {
        let compact = "\
void M()
{
    Task.Delay(100).ContinueWith((t) =>
    {
        this.Invoke(new Action(() =>
        {
            Render();
        }));
    });
}
";
        let wrapped = "\
void M()
{
    Task.Delay(100)
        .ContinueWith(
            (t) =>
            {
                this.Invoke(
                    new Action(() =>
                    {
                        Render();
                    })
                );
            }
        );
}
";
        let (a, b) = (cs(compact).stats(), cs(wrapped).stats());
        // void(0) Task(1) this.Invoke(2) Render(3)
        assert_eq!((a.lines, a.total), (4, 6));
        assert_eq!((b.lines, b.total), (4, 6));
    }

    #[test]
    fn brace_style_is_neutral() {
        let gnu = "\
switch (key)
{
    case Keys.A:
        {
            Go();
            break;
        }
}
";
        let allman = "\
switch (key)
{
case Keys.A:
{
    Go();
    break;
}
}
";
        let (a, b) = (cs(gnu).stats(), cs(allman).stats());
        // switch(0) case(1) Go(2) break(2)
        assert_eq!((a.lines, a.total), (4, 5));
        assert_eq!((b.lines, b.total), (4, 5));
    }

    #[test]
    fn using_block_after_new_is_a_block() {
        let src = "\
using (TextReader reader = new StreamReader(script))
{
    var res = Eval(reader);
    if (res.Error != null) Tutorial = res.Error.Message;
}
var p = new Person(1)
{
    Name = \"x\",
};
";
        // using(0) var(1) if(1) body(2) p(0)
        let s = cs(src).stats();
        assert_eq!((s.lines, s.total), (5, 4));
    }

    #[test]
    fn reindent_still_counts() {
        let before = "def f():\n    a()\n";
        let after = "def f():\n    if x:\n        a()\n";
        let (o, n) = (
            Levels::analyze(before, Language::Python, 4),
            Levels::analyze(after, Language::Python, 4),
        );
        let ch = churn(Some(&o), &[(2, 1)], Some(&n), &[(2, 2)]);
        assert_eq!((ch.added.total, ch.removed.total), (3, 1));
    }
}
