//! Function and class detection from declaration lines plus indentation.
//!
//! A declaration at level `L` owns every following code line indented deeper
//! than `L`. Function complexity is measured relative to the function body, so
//! a flat function scores 0 regardless of how deeply its class is nested.

use crate::complexity::{Language, Stats, Stmt};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decl {
    Function(String),
    Class(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct Function {
    /// Qualified name, e.g. `Outer.Inner.Method`.
    pub name: String,
    /// 1-based first / last line (declaration through last body line).
    pub start: u32,
    pub end: u32,
    /// Body stats, levels relative to the body's own indentation.
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Default)]
pub struct Structure {
    pub functions: Vec<Function>,
    /// Qualified class / type names.
    pub classes: Vec<String>,
}

static PY_FUNC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:async\s+)?def\s+(\w+)").unwrap());
static PY_CLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^class\s+(\w+)").unwrap());

static CS_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\[[^\]]*\]\s*)*(?:\w+\s+)*?(?:class|struct|interface|enum|record)\s+(?:(?:class|struct)\s+)?(\w+)")
        .unwrap()
});
/// `[modifiers] ReturnType Name[<T>](` or `[modifiers] Name(` (constructors).
static CS_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\[[^\]]*\]\s*)*[\w<>\[\],.?\s]*?(\w+)\s*(?:<[\w\s,<>]*>)?\s*\(").unwrap()
});

static CS_AFTER_TUPLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\??\s*(\w+)\s*(?:<[\w\s,<>]*>)?\s*\(").unwrap());

const CS_MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "async",
    "virtual",
    "override",
    "abstract",
    "sealed",
    "unsafe",
    "extern",
    "partial",
    "readonly",
];

/// Length of the parenthesized group at the start of `s`.
fn tuple_end(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

const CS_NOT_A_DECL: &[&str] = &[
    "if",
    "else",
    "for",
    "foreach",
    "while",
    "do",
    "switch",
    "case",
    "using",
    "lock",
    "fixed",
    "catch",
    "return",
    "throw",
    "yield",
    "await",
    "new",
    "when",
    "nameof",
    "typeof",
    "sizeof",
    "default",
    "checked",
    "unchecked",
    "base",
    "this",
    "var",
    "goto",
    "in",
    "is",
    "as",
    "out",
    "ref",
    "from",
    "select",
    "where",
];

/// Recognizes a declaration on a (trimmed) logical code line.
pub fn detect(lang: Language, t: &str) -> Option<Decl> {
    match lang {
        Language::Python => {
            if let Some(c) = PY_FUNC.captures(t) {
                return Some(Decl::Function(c[1].to_string()));
            }
            PY_CLASS.captures(t).map(|c| Decl::Class(c[1].to_string()))
        }
        Language::CSharp => {
            let first = t.split(|c: char| !c.is_alphanumeric() && c != '_').next()?;
            if CS_NOT_A_DECL.contains(&first) {
                return None;
            }
            if let Some(c) = CS_TYPE.captures(t) {
                return Some(Decl::Class(c[1].to_string()));
            }
            let head = &t[..t.find('(').unwrap_or(t.len())];
            if head.contains('=') || (t.ends_with(';') && !t.contains("=>")) {
                return None;
            }
            let c = CS_METHOD.captures(t)?;
            let name = &c[1];
            if CS_MODIFIERS.contains(&name) {
                // Tuple return type: `static (int a, int b) Name(`
                let tuple = c.get(0)?.end() - 1;
                let after = tuple + tuple_end(&t[tuple..])?;
                return CS_AFTER_TUPLE
                    .captures(&t[after..])
                    .filter(|c| !CS_NOT_A_DECL.contains(&&c[1]))
                    .map(|c| Decl::Function(c[1].to_string()));
            }
            // Needs a return type or modifier before the name (rules out plain calls).
            let before_name = t[..c.get(1)?.start()].trim_end();
            let typed = before_name
                .ends_with(|c: char| c.is_alphanumeric() || matches!(c, '_' | '>' | ']' | '?'));
            if !typed || CS_NOT_A_DECL.contains(&name) {
                return None;
            }
            Some(Decl::Function(name.to_string()))
        }
    }
}

/// Builds the structure from statements and declarations.
pub(crate) fn build(stmts: &[Stmt], decls: &[(usize, Decl)]) -> Structure {
    struct Open {
        level: u32,
        name: String,
        func: Option<usize>, // index into `functions`
    }
    let mut out = Structure::default();
    let mut stack: Vec<Open> = Vec::new();
    let mut decls = decls.iter().peekable();
    let mut last_line = 0u32;

    let close = |stack: &mut Vec<Open>, out: &mut Structure, level: Option<u32>, end: u32| {
        while stack
            .last()
            .is_some_and(|o| level.is_none_or(|l| o.level >= l))
        {
            if let Some(i) = stack.pop().unwrap().func {
                out.functions[i].end = end;
            }
        }
    };

    for (i, st) in stmts.iter().enumerate() {
        let level = st.level;
        close(&mut stack, &mut out, Some(level), last_line);
        last_line = st.last + 1;

        if let Some((_, decl)) = decls.next_if(|(di, _)| *di == i) {
            let (name, is_func) = match decl {
                Decl::Function(n) => (n, true),
                Decl::Class(n) => (n, false),
            };
            let qual = stack
                .iter()
                .map(|o| o.name.as_str())
                .chain([name.as_str()])
                .collect::<Vec<_>>()
                .join(".");
            let func = if is_func {
                out.functions.push(Function {
                    name: qual,
                    start: st.first + 1,
                    end: st.last + 1,
                    stats: Stats::default(),
                });
                Some(out.functions.len() - 1)
            } else {
                out.classes.push(qual);
                None
            };
            stack.push(Open {
                level,
                name: name.clone(),
                func,
            });
        } else if let Some(o) = stack.iter().rev().find(|o| o.func.is_some()) {
            let rel = level.saturating_sub(o.level + 1);
            out.functions[o.func.unwrap()].stats.push(rel);
        }
    }
    close(&mut stack, &mut out, None, last_line);
    out
}

#[derive(Serialize)]
pub struct FnChange {
    pub name: String,
    /// `added`, `removed` or `modified`.
    pub change: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Stats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Stats>,
}

fn touched(f: &Function, ranges: &[(u32, u32)]) -> bool {
    ranges.iter().any(|&(s, c)| s <= f.end && s + c > f.start)
}

/// Pairs functions by qualified name (overloads in order) and classifies changes.
pub fn diff_functions(
    old: &[Function],
    new: &[Function],
    removed: &[(u32, u32)],
    added: &[(u32, u32)],
) -> Vec<FnChange> {
    let mut old_by_name: HashMap<&str, Vec<&Function>> = HashMap::new();
    for f in old.iter().rev() {
        old_by_name.entry(&f.name).or_default().push(f);
    }
    let mut changes = Vec::new();
    for n in new {
        match old_by_name.get_mut(n.name.as_str()).and_then(|v| v.pop()) {
            Some(o) => {
                if touched(n, added) || touched(o, removed) {
                    changes.push(FnChange {
                        name: n.name.clone(),
                        change: "modified",
                        before: Some(o.stats),
                        after: Some(n.stats),
                    });
                }
            }
            None => changes.push(FnChange {
                name: n.name.clone(),
                change: "added",
                before: None,
                after: Some(n.stats),
            }),
        }
    }
    let mut gone: Vec<&Function> = old_by_name.into_values().flatten().collect();
    gone.sort_by_key(|f| f.start);
    changes.extend(gone.into_iter().map(|o| FnChange {
        name: o.name.clone(),
        change: "removed",
        before: Some(o.stats),
        after: None,
    }));
    changes
}

/// Multiset difference `a - b` of names.
pub fn names_missing_from(a: &[String], b: &[String]) -> Vec<String> {
    let mut counts: HashMap<&str, i32> = HashMap::new();
    for n in b {
        *counts.entry(n).or_default() += 1;
    }
    a.iter()
        .filter(|n| {
            let c = counts.entry(n).or_default();
            *c -= 1;
            *c < 0
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::complexity::Levels;

    #[test]
    fn csharp_decl_detection() {
        let f = |t| detect(Language::CSharp, t);
        let func = |n: &str| Some(Decl::Function(n.into()));
        assert_eq!(
            f("public async Task<int> RunAsync(int x)"),
            func("RunAsync")
        );
        assert_eq!(f("public Svc(ILogger log) : base(log)"), func("Svc"));
        assert_eq!(
            f("private static T Get<T>(string k) where T : class"),
            func("Get")
        );
        assert_eq!(f("public int Double(int x) => x * 2;"), func("Double"));
        assert_eq!(f("int Local(int y)"), func("Local"));
        assert_eq!(
            f("public void Opt(int x = 0, string s = \"a\")"),
            func("Opt")
        );
        assert_eq!(f("[HttpGet] public IActionResult Get(int id)"), func("Get"));
        assert_eq!(f("public static int[] Many(int n)"), func("Many"));
        assert_eq!(
            f("public static (string, int) LookupFileAtLine(string line)"),
            func("LookupFileAtLine")
        );
        assert_eq!(
            f("private async (int a, int? b)? Pair<T>(T x)"),
            func("Pair")
        );
        assert_eq!(
            f("public sealed partial class Foo : Bar"),
            Some(Decl::Class("Foo".into()))
        );
        assert_eq!(
            f("public record struct Point(int X, int Y);"),
            Some(Decl::Class("Point".into()))
        );
        assert_eq!(
            f("[Serializable] internal enum Kind"),
            Some(Decl::Class("Kind".into()))
        );
        for not in [
            "if (x)",
            "else if (x > 0)",
            "foreach (var i in xs)",
            "return Foo(x);",
            "await DoAsync(a => b);",
            "var x = Foo(1);",
            "Foo(a,",
            "logger.Info(",
            "using (var s = Open())",
            "catch (Exception e)",
            "void Abstract();",
            "throw new ArgumentException(",
            "[Route(\"api/x\")]",
            "return record with { X = 1 };",
        ] {
            assert_eq!(f(not), None, "{not}");
        }
    }

    #[test]
    fn python_structure() {
        let src = "\
class A:
    def f(self, x):
        if x:
            return 1
        return 2

    async def g(self):
        def inner():
            pass
        return inner

def top():
    pass
";
        let lv = Levels::analyze(src, Language::Python, 4);
        let s = lv.structure();
        assert_eq!(s.classes, vec!["A"]);
        let names: Vec<_> = s.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A.f", "A.g", "A.g.inner", "top"]);
        let f = &s.functions[0];
        assert_eq!((f.start, f.end), (2, 5));
        // if(0) return(1) return(0)
        assert_eq!((f.stats.lines, f.stats.total, f.stats.max), (3, 1, 1));
        assert_eq!(s.functions[1].stats.lines, 1); // `return inner`; inner's body is its own
        assert_eq!((s.functions[3].start, s.functions[3].end), (12, 13));
    }

    #[test]
    fn csharp_structure_is_relative_to_method() {
        let src = "\
namespace N
{
    public class C
    {
        public int M(int x)
        {
            if (x > 0)
            {
                return x;
            }
            return 0;
        }
    }
}
";
        let s = Levels::analyze(src, Language::CSharp, 4).structure();
        assert_eq!(s.classes, vec!["C"]); // namespaces are not classes
        assert_eq!(s.functions.len(), 1);
        assert_eq!(s.functions[0].name, "C.M");
        assert_eq!(s.functions[0].stats.total, 1);
    }

    #[test]
    fn csharp_realistic_file() {
        let src = r#"
using System.Linq;

namespace Shop.Orders;

[ApiController]
[Route("api/[controller]")]
public sealed class OrdersController : ControllerBase
{
    private readonly IOrderService _svc;
    public int Count { get; private set; }
    public string Name
    {
        get { return _name; }
        set { _name = value ?? throw new ArgumentNullException(nameof(value)); }
    }

    public OrdersController(IOrderService svc) => _svc = svc;

    [HttpGet("{id}")]
    public async Task<ActionResult<OrderDto>> Get(int id, CancellationToken ct = default)
    {
        var order = await _svc.FindAsync(id, ct);
        if (order is null)
        {
            return NotFound();
        }
        var lines = order.Lines
            .Where(l => l.Qty > 0)
            .Select(l => new LineDto(l.Sku, l.Qty))
            .ToList();
        return order.Status switch
        {
            Status.Open => Ok(Map(order, lines)),
            _ => Conflict(),
        };

        static OrderDto Map(Order o, List<LineDto> lines)
        {
            return new OrderDto(o.Id, lines);
        }
    }

    protected override void Dispose(bool disposing)
    {
        base.Dispose(disposing);
    }

    private record LineDto(string Sku, int Qty);
}
"#;
        let s = Levels::analyze(src, Language::CSharp, 4).structure();
        let names: Vec<_> = s.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "OrdersController.OrdersController",
                "OrdersController.Get",
                "OrdersController.Get.Map",
                "OrdersController.Dispose"
            ]
        );
        assert_eq!(s.classes, ["OrdersController", "OrdersController.LineDto"]);
    }

    #[test]
    fn function_diff() {
        let func = |name: &str, start, end, total| Function {
            name: name.into(),
            start,
            end,
            stats: Stats {
                lines: 1,
                total,
                max: 0,
            },
        };
        let old = [
            func("a", 1, 3, 1),
            func("b", 4, 6, 2),
            func("gone", 7, 9, 0),
        ];
        let new = [func("a", 1, 3, 1), func("b", 4, 7, 5), func("new", 8, 9, 0)];
        let ch = diff_functions(&old, &new, &[], &[(5, 1)]);
        let got: Vec<_> = ch.iter().map(|c| (c.name.as_str(), c.change)).collect();
        assert_eq!(
            got,
            [("b", "modified"), ("new", "added"), ("gone", "removed")]
        );
    }
}
