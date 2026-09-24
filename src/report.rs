//! Output formatting.

use crate::analyze::{Changes, PrResult};
use crate::complexity::Stats;
use std::collections::HashMap;
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Detail {
    Pr,
    File,
    Function,
}

pub fn table(out: &mut impl Write, prs: &[PrResult], detail: Detail, top: usize) -> io::Result<()> {
    writeln!(
        out,
        "{:<10}  {:>6}  {:<9}  {:>5}  {:>6}  {:>7}  {:>7}  {:>7}  {:>4}  {:>11}  {:>5}  SUBJECT",
        "DATE",
        "PR",
        "COMMIT",
        "FILES",
        "+LINES",
        "+CPLX",
        "-CPLX",
        "ΔCPLX",
        "MAXD",
        "FN +/-/~",
        "ΔCLS"
    )?;
    for pr in prs {
        writeln!(
            out,
            "{:<10}  {:>6}  {:<9}  {:>5}  {:>6}  {:>7}  {:>7}  {:>+7}  {:>4}  {:>11}  {:>+5}  {}",
            pr.date.get(..10).unwrap_or(&pr.date),
            pr.pr.map(|n| format!("#{n}")).unwrap_or_default(),
            short(&pr.commit),
            pr.files.len(),
            pr.added.lines,
            pr.added.total,
            pr.removed.total,
            pr.delta,
            pr.added.max,
            fn_changes(&pr.changes),
            pr.counts_after.classes as i64 - pr.counts_before.classes as i64,
            truncate(&pr.subject, 60),
        )?;
        if detail < Detail::File {
            continue;
        }
        for f in &pr.files {
            writeln!(
                out,
                "{:>36}  {:>6}  {:>7}  {:>7}  {:>+7}  {:>4}  {:>11}  {:>+5}  {} {}",
                "",
                f.added.lines,
                f.added.total,
                f.removed.total,
                f.delta,
                f.added.max,
                fn_changes(&f.changes),
                f.counts_after.classes as i64 - f.counts_before.classes as i64,
                f.status,
                f.path
            )?;
            if detail < Detail::Function {
                continue;
            }
            for (sym, classes) in [("+", &f.classes_added), ("-", &f.classes_removed)] {
                if !classes.is_empty() {
                    let label = if classes.len() == 1 {
                        "class"
                    } else {
                        "classes"
                    };
                    writeln!(
                        out,
                        "{:>40}{sym} {} {label}: {}",
                        "",
                        classes.len(),
                        truncate(&classes.join(", "), 100)
                    )?;
                }
            }
            for fc in &f.functions {
                let (sym, cplx) = match (fc.before, fc.after) {
                    (Some(b), Some(a)) => ("~", format!("{} → {}", fmt_fn(&b), fmt_fn(&a))),
                    (None, Some(a)) => ("+", fmt_fn(&a)),
                    (Some(b), None) => ("-", fmt_fn(&b)),
                    (None, None) => continue,
                };
                writeln!(out, "{:>40}{sym} {}  {cplx}", "", fc.name)?;
            }
        }
    }
    summary(out, prs, top)
}

fn fn_changes(c: &Changes) -> String {
    format!(
        "{}/{}/{}",
        c.functions_added, c.functions_removed, c.functions_modified
    )
}

/// `complexity (dMAX_DEPTH, LINES L)`
fn fmt_fn(s: &Stats) -> String {
    format!("{} (d{}, {}L)", s.total, s.max, s.lines)
}

fn summary(out: &mut impl Write, prs: &[PrResult], top: usize) -> io::Result<()> {
    if prs.is_empty() || top == 0 {
        return Ok(());
    }
    let added: u64 = prs.iter().map(|p| p.added.total).sum();
    let removed: u64 = prs.iter().map(|p| p.removed.total).sum();
    let delta: i64 = prs.iter().map(|p| p.delta).sum();
    let mut ch = Changes::default();
    prs.iter().for_each(|p| ch.merge(&p.changes));
    writeln!(
        out,
        "\n{} PRs  |  complexity added {added}, removed {removed}, net {delta:+}",
        prs.len()
    )?;
    writeln!(
        out,
        "         |  functions +{} -{} ~{}, classes +{} -{}",
        ch.functions_added,
        ch.functions_removed,
        ch.functions_modified,
        ch.classes_added,
        ch.classes_removed
    )?;

    let mut by_added: Vec<&PrResult> = prs.iter().collect();
    by_added.sort_by_key(|p| std::cmp::Reverse(p.added.total));
    writeln!(out, "\nTop PRs by complexity added:")?;
    for p in by_added.iter().take(top).filter(|p| p.added.total > 0) {
        writeln!(
            out,
            "  {:>7}  {:>6}  {:<9}  {}",
            p.added.total,
            p.pr.map(|n| format!("#{n}")).unwrap_or_default(),
            short(&p.commit),
            truncate(&p.subject, 70)
        )?;
    }

    // Hotspots: files that accumulated the most complexity across PRs.
    #[derive(Default)]
    struct Hot {
        prs: u32,
        added: u64,
        delta: i64,
        current: Option<u64>,
        functions: Option<u32>,
    }
    // Functions whose body complexity grew the most.
    #[derive(Default)]
    struct HotFn {
        prs: u32,
        growth: i64,
        current: Option<Option<Stats>>,
    }
    let mut files: HashMap<&str, Hot> = HashMap::new();
    let mut funcs: HashMap<(&str, &str), HotFn> = HashMap::new();
    for p in prs {
        // `prs` is newest first, so the first value seen is the most recent.
        for f in &p.files {
            let h = files.entry(f.path.as_str()).or_default();
            h.prs += 1;
            h.added += f.added.total;
            h.delta += f.delta;
            h.current.get_or_insert(f.after.total);
            h.functions.get_or_insert(f.counts_after.functions);
            for fc in &f.functions {
                let h = funcs.entry((&f.path, &fc.name)).or_default();
                h.prs += 1;
                h.growth += fc.after.map_or(0, |s| s.total as i64)
                    - fc.before.map_or(0, |s| s.total as i64);
                h.current.get_or_insert(fc.after);
            }
        }
    }

    let mut hot: Vec<_> = files.into_iter().collect();
    hot.sort_by_key(|(_, h)| std::cmp::Reverse(h.added));
    writeln!(out, "\nHotspot files (complexity added across PRs):")?;
    writeln!(
        out,
        "  {:>7}  {:>7}  {:>7}  {:>5}  {:>4}  PATH",
        "+CPLX", "ΔCPLX", "NOW", "FUNCS", "PRS"
    )?;
    for (path, h) in hot.iter().take(top) {
        writeln!(
            out,
            "  {:>7}  {:>+7}  {:>7}  {:>5}  {:>4}  {path}",
            h.added,
            h.delta,
            h.current.unwrap_or(0),
            h.functions.unwrap_or(0),
            h.prs
        )?;
    }

    let mut hot_fns: Vec<_> = funcs.into_iter().filter(|(_, h)| h.growth > 0).collect();
    hot_fns.sort_by_key(|(k, h)| (std::cmp::Reverse(h.growth), *k));
    if !hot_fns.is_empty() {
        writeln!(out, "\nFunctions that grew the most:")?;
        writeln!(
            out,
            "  {:>7}  {:>7}  {:>5}  {:>4}  FUNCTION",
            "ΔCPLX", "NOW", "DEPTH", "PRS"
        )?;
        for ((path, name), h) in hot_fns.iter().take(top) {
            let now = h.current.flatten();
            writeln!(
                out,
                "  {:>+7}  {:>7}  {:>5}  {:>4}  {name}  ({path})",
                h.growth,
                now.map_or("gone".into(), |s| s.total.to_string()),
                now.map_or(String::new(), |s| s.max.to_string()),
                h.prs
            )?;
        }
    }
    Ok(())
}

pub fn csv(out: &mut impl Write, prs: &[PrResult], detail: Detail) -> io::Result<()> {
    const COUNTS: &str = "functions_before,functions_after,classes_before,classes_after,functions_added,functions_removed,functions_modified,classes_added,classes_removed";
    let pr_cols = |p: &PrResult| {
        format!(
            "{},{},{}",
            p.date,
            p.pr.map(|n| n.to_string()).unwrap_or_default(),
            p.commit
        )
    };
    let counts = |b: &crate::analyze::Counts, a: &crate::analyze::Counts, c: &Changes| {
        format!(
            "{},{},{},{},{},{},{},{},{}",
            b.functions,
            a.functions,
            b.classes,
            a.classes,
            c.functions_added,
            c.functions_removed,
            c.functions_modified,
            c.classes_added,
            c.classes_removed
        )
    };
    let stat = |s: Option<Stats>| {
        s.map_or(",,".into(), |s| {
            format!("{},{},{}", s.lines, s.total, s.max)
        })
    };

    match detail {
        Detail::Function => {
            writeln!(
                out,
                "date,pr,commit,path,language,function,change,lines_before,cplx_before,depth_before,lines_after,cplx_after,depth_after"
            )?;
            for p in prs {
                for f in &p.files {
                    for fc in &f.functions {
                        writeln!(
                            out,
                            "{},{},{},{},{},{},{}",
                            pr_cols(p),
                            esc(&f.path),
                            f.language.name(),
                            esc(&fc.name),
                            fc.change,
                            stat(fc.before),
                            stat(fc.after)
                        )?;
                    }
                }
            }
        }
        Detail::File => {
            writeln!(
                out,
                "date,pr,commit,path,status,language,lines_added,cplx_added,cplx_removed,max_depth_added,cplx_before,cplx_after,delta,{COUNTS}"
            )?;
            for p in prs {
                for f in &p.files {
                    writeln!(
                        out,
                        "{},{},{},{},{},{},{},{},{},{},{},{}",
                        pr_cols(p),
                        esc(&f.path),
                        f.status,
                        f.language.name(),
                        f.added.lines,
                        f.added.total,
                        f.removed.total,
                        f.added.max,
                        f.before.total,
                        f.after.total,
                        f.delta,
                        counts(&f.counts_before, &f.counts_after, &f.changes)
                    )?;
                }
            }
        }
        Detail::Pr => {
            writeln!(
                out,
                "date,pr,commit,author,files,lines_added,cplx_added,cplx_removed,max_depth_added,cplx_before,cplx_after,delta,{COUNTS},subject"
            )?;
            for p in prs {
                writeln!(
                    out,
                    "{},{},{},{},{},{},{},{},{},{},{},{}",
                    pr_cols(p),
                    esc(&p.author),
                    p.files.len(),
                    p.added.lines,
                    p.added.total,
                    p.removed.total,
                    p.added.max,
                    p.before.total,
                    p.after.total,
                    p.delta,
                    counts(&p.counts_before, &p.counts_after, &p.changes),
                    esc(&p.subject)
                )?;
            }
        }
    }
    Ok(())
}

fn short(commit: &str) -> &str {
    &commit[..9.min(commit.len())]
}

fn esc(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}
