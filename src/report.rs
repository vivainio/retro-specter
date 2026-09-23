//! Output formatting.

use crate::analyze::PrResult;
use std::collections::HashMap;
use std::io::{self, Write};

pub fn table(
    out: &mut impl Write,
    prs: &[PrResult],
    show_files: bool,
    top: usize,
) -> io::Result<()> {
    writeln!(
        out,
        "{:<10}  {:>6}  {:<9}  {:>5}  {:>6}  {:>7}  {:>7}  {:>7}  {:>4}  SUBJECT",
        "DATE", "PR", "COMMIT", "FILES", "+LINES", "+CPLX", "-CPLX", "ΔCPLX", "MAXD"
    )?;
    for pr in prs {
        writeln!(
            out,
            "{:<10}  {:>6}  {:<9}  {:>5}  {:>6}  {:>7}  {:>7}  {:>+7}  {:>4}  {}",
            pr.date.get(..10).unwrap_or(&pr.date),
            pr.pr.map(|n| format!("#{n}")).unwrap_or_default(),
            &pr.commit[..9.min(pr.commit.len())],
            pr.files.len(),
            pr.added.lines,
            pr.added.total,
            pr.removed.total,
            pr.delta,
            pr.added.max,
            truncate(&pr.subject, 60),
        )?;
        if show_files {
            for f in &pr.files {
                writeln!(
                    out,
                    "{:>36}  {:>6}  {:>7}  {:>7}  {:>+7}  {:>4}  {} {}",
                    "",
                    f.added.lines,
                    f.added.total,
                    f.removed.total,
                    f.delta,
                    f.added.max,
                    f.status,
                    f.path
                )?;
            }
        }
    }
    summary(out, prs, top)
}

fn summary(out: &mut impl Write, prs: &[PrResult], top: usize) -> io::Result<()> {
    if prs.is_empty() || top == 0 {
        return Ok(());
    }
    let added: u64 = prs.iter().map(|p| p.added.total).sum();
    let removed: u64 = prs.iter().map(|p| p.removed.total).sum();
    let delta: i64 = prs.iter().map(|p| p.delta).sum();
    writeln!(
        out,
        "\n{} PRs  |  complexity added {added}, removed {removed}, net {delta:+}",
        prs.len()
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
            &p.commit[..9.min(p.commit.len())],
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
    }
    let mut files: HashMap<&str, Hot> = HashMap::new();
    for p in prs {
        // `prs` is newest first, so the first `after` seen is the most recent size.
        for f in &p.files {
            let h = files.entry(f.path.as_str()).or_default();
            h.prs += 1;
            h.added += f.added.total;
            h.delta += f.delta;
            h.current.get_or_insert(f.after.total);
        }
    }
    let mut hot: Vec<_> = files.into_iter().collect();
    hot.sort_by_key(|(_, h)| std::cmp::Reverse(h.added));
    writeln!(out, "\nHotspot files (complexity added across PRs):")?;
    writeln!(
        out,
        "  {:>7}  {:>7}  {:>7}  {:>4}  PATH",
        "+CPLX", "ΔCPLX", "NOW", "PRS"
    )?;
    for (path, h) in hot.iter().take(top) {
        writeln!(
            out,
            "  {:>7}  {:>+7}  {:>7}  {:>4}  {path}",
            h.added,
            h.delta,
            h.current.unwrap_or(0),
            h.prs
        )?;
    }
    Ok(())
}

pub fn csv(out: &mut impl Write, prs: &[PrResult], per_file: bool) -> io::Result<()> {
    if per_file {
        writeln!(
            out,
            "date,pr,commit,path,status,language,lines_added,cplx_added,cplx_removed,max_depth_added,cplx_before,cplx_after,delta"
        )?;
        for p in prs {
            for f in &p.files {
                writeln!(
                    out,
                    "{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    p.date,
                    p.pr.map(|n| n.to_string()).unwrap_or_default(),
                    p.commit,
                    esc(&f.path),
                    f.status,
                    f.language.name(),
                    f.added.lines,
                    f.added.total,
                    f.removed.total,
                    f.added.max,
                    f.before.total,
                    f.after.total,
                    f.delta
                )?;
            }
        }
    } else {
        writeln!(
            out,
            "date,pr,commit,author,files,lines_added,cplx_added,cplx_removed,max_depth_added,cplx_before,cplx_after,delta,subject"
        )?;
        for p in prs {
            writeln!(
                out,
                "{},{},{},{},{},{},{},{},{},{},{},{},{}",
                p.date,
                p.pr.map(|n| n.to_string()).unwrap_or_default(),
                p.commit,
                esc(&p.author),
                p.files.len(),
                p.added.lines,
                p.added.total,
                p.removed.total,
                p.added.max,
                p.before.total,
                p.after.total,
                p.delta,
                esc(&p.subject)
            )?;
        }
    }
    Ok(())
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
