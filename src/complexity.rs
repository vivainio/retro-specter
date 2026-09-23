//! Indentation-based complexity.
//!
//! Each logical line of code contributes its indentation level; a file's
//! complexity is the sum. Blank lines, comments, docstrings, preprocessor
//! directives and brace/bracket-only lines are ignored so that formatting
//! style does not dominate the score.

use serde::Serialize;

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
    /// Logical lines of code.
    pub lines: u32,
    /// Sum of indentation levels.
    pub total: u64,
    /// Deepest indentation level.
    pub max: u32,
}

impl Stats {
    fn push(&mut self, level: u32) {
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

/// Indentation level per line (index = line number - 1); `None` for non-code lines.
pub struct Levels(Vec<Option<u32>>);

impl Levels {
    pub fn analyze(src: &str, lang: Language, tab_width: usize) -> Levels {
        let src = src.strip_prefix('\u{feff}').unwrap_or(src);
        let mut block_comment = false;
        let mut py_string: Option<&'static str> = None;

        let widths: Vec<Option<usize>> = src
            .lines()
            .map(|line| {
                let code = match lang {
                    Language::CSharp => csharp_is_code(line, &mut block_comment),
                    Language::Python => python_is_code(line, &mut py_string),
                };
                code.then(|| indent_width(line, tab_width))
            })
            .collect();

        let unit = detect_unit(widths.iter().flatten().copied());
        Levels(
            widths
                .into_iter()
                .map(|w| w.map(|w| (w / unit) as u32))
                .collect(),
        )
    }

    pub fn stats(&self) -> Stats {
        let mut s = Stats::default();
        self.0.iter().flatten().for_each(|&l| s.push(l));
        s
    }

    /// Stats restricted to the given 1-based `(start, count)` line ranges.
    pub fn stats_in(&self, ranges: &[(u32, u32)]) -> Stats {
        let mut s = Stats::default();
        for &(start, count) in ranges {
            let from = (start.max(1) - 1) as usize;
            let to = (from + count as usize).min(self.0.len());
            if from < to {
                self.0[from..to].iter().flatten().for_each(|&l| s.push(l));
            }
        }
        s
    }
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

/// The most common positive indentation step between consecutive code lines.
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
        .max_by_key(|&d| (hist[d], d))
        .filter(|&d| hist[d] > 0)
        .unwrap_or(4)
}

fn is_punctuation_only(t: &str) -> bool {
    t.chars()
        .all(|c| matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';' | ',' | ':'))
}

fn csharp_is_code(line: &str, in_block: &mut bool) -> bool {
    let mut t = line.trim();
    if *in_block {
        match t.find("*/") {
            Some(i) => {
                *in_block = false;
                t = t[i + 2..].trim();
            }
            None => return false,
        }
    }
    while let Some(rest) = t.strip_prefix("/*") {
        match rest.find("*/") {
            Some(i) => t = rest[i + 2..].trim(),
            None => {
                *in_block = true;
                return false;
            }
        }
    }
    if t.is_empty() || t.starts_with("//") || t.starts_with('#') || is_punctuation_only(t) {
        return false;
    }
    // A block comment opened at the end of a code line.
    if let Some(i) = t.find("/*")
        && !t[i + 2..].contains("*/")
        && !t[..i].contains('"')
    {
        *in_block = true;
    }
    true
}

fn python_is_code(line: &str, in_string: &mut Option<&'static str>) -> bool {
    let t = line.trim();
    if let Some(delim) = *in_string {
        if t.matches(delim).count() % 2 == 1 {
            *in_string = None;
        }
        return false;
    }
    if t.is_empty() || t.starts_with('#') || is_punctuation_only(t) {
        return false;
    }
    let delim = match (t.find("\"\"\""), t.find("'''")) {
        (Some(a), Some(b)) if b < a => "'''",
        (Some(_), _) => "\"\"\"",
        (None, Some(_)) => "'''",
        (None, None) => return true,
    };
    if t.matches(delim).count() % 2 == 1 {
        *in_string = Some(delim);
    }
    // A line that *starts* with a triple-quoted string is a docstring, not logic.
    !t.trim_start_matches(['r', 'R', 'b', 'B', 'u', 'U', 'f', 'F'])
        .starts_with(delim)
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
        // def(0) if(1) for(2) print(3) return(1) 1,(2)
        assert_eq!(s.lines, 6);
        assert_eq!(s.total, 9);
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
}
