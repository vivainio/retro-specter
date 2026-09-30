//! AI usage per PR or per commit, read from commit messages only (any language).

use crate::ai::{self, Credit, Usage};
use crate::git::{Commit, Repo};
use crate::repos::RepoArgs;
use anyhow::Result;
use clap::ValueEnum;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Write};

/// What one row of the report is.
#[derive(Clone, Copy, ValueEnum, PartialEq)]
pub enum Unit {
    /// Merged PRs; the models of all commits in the PR are combined, most commits first.
    Pr,
    /// Individual commits (merge commits excluded).
    Commit,
}

/// What a row stands for.
#[derive(Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A merged PR (all its commits combined).
    Pr,
    /// A commit pushed straight to the branch, outside any PR.
    Direct,
    /// A single commit (`--by commit`).
    Commit,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Pr => "pr",
            Kind::Direct => "direct",
            Kind::Commit => "commit",
        }
    }
}

#[derive(Serialize)]
pub struct Row {
    pub kind: Kind,
    pub repo: String,
    /// The PR's merge / squash commit, or the commit itself.
    pub commit: String,
    pub pr: Option<u64>,
    pub author: String,
    pub date: String,
    pub subject: String,
    /// Commits in the PR (always 1 per commit).
    pub commits: u32,
    /// Lines added / removed, all file types.
    pub added: u64,
    pub removed: u64,
    pub ai: Usage,
}

/// One row per merged PR of `repo`, plus one per direct commit (`is_pr == false`).
pub fn by_pr(repo: &Repo, selected: &[(Commit, Option<u64>, bool)]) -> Result<Vec<Row>> {
    let name = repo.name();
    let empty = repo.empty_tree()?;
    selected
        .par_iter()
        .map(|(c, pr, is_pr)| {
            let own = format!("{}\n\n{}", c.subject, c.body);
            let base = c.parents.first();
            // Trailers live on the branch commits, not on the merge commit itself.
            let messages = match base {
                Some(base) if *is_pr && c.parents.len() > 1 => repo.branch_messages(base, &c.id)?,
                _ => Vec::new(),
            };
            let messages = if messages.is_empty() {
                vec![own]
            } else {
                messages
            };
            let (added, removed) = repo.numstat(base.unwrap_or(&empty), &c.id)?;
            Ok(Row {
                kind: if *is_pr { Kind::Pr } else { Kind::Direct },
                repo: name.clone(),
                commit: c.id.clone(),
                pr: *pr,
                author: c.author.clone(),
                date: c.date.clone(),
                subject: c.subject.clone(),
                commits: messages.len() as u32,
                added,
                removed,
                ai: ai::combine(messages.iter().map(String::as_str)),
            })
        })
        .collect()
}

/// One row per non-merge commit reachable from the requested revision.
pub fn by_commit(repo: &Repo, args: &RepoArgs) -> Result<Vec<Row>> {
    let name = repo.name();
    let log = repo.commit_log(
        &args.rev_for(repo),
        args.since_date().as_deref(),
        args.until.as_deref(),
        args.max_count,
    )?;
    Ok(log
        .into_iter()
        .map(|(c, added, removed)| {
            let ai = ai::detect(&format!("{}\n\n{}", c.subject, c.body));
            Row {
                kind: Kind::Commit,
                repo: name.clone(),
                pr: crate::pr::pr_number(&c),
                commit: c.id,
                author: c.author,
                date: c.date,
                subject: c.subject,
                commits: 1,
                added,
                removed,
                ai,
            }
        })
        .collect())
}

fn credits(list: &[Credit], counts: bool) -> String {
    list.iter()
        .map(|c| {
            if counts {
                format!("{} ({})", c.name, c.commits)
            } else {
                c.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn table(out: &mut impl Write, rows: &[Row], unit: Unit, detail: bool) -> io::Result<()> {
    if detail {
        for r in rows {
            let ai = [
                credits(&r.ai.models, unit == Unit::Pr),
                credits(&r.ai.tools, false),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
            writeln!(
                out,
                "{:<20}  {:<10}  {:>6}  {:<9}  {:>7}  {:>7}  {:<40}  {}",
                trunc(&r.repo, 20),
                r.date.get(..10).unwrap_or(&r.date),
                match (r.kind, r.pr) {
                    (Kind::Direct, _) => "direct".into(),
                    (_, Some(n)) => format!("#{n}"),
                    _ => String::new(),
                },
                &r.commit[..9.min(r.commit.len())],
                r.added,
                r.removed,
                trunc(&ai, 40),
                trunc(&r.subject, 60)
            )?;
        }
        writeln!(out)?;
    }
    let (direct, prs): (Vec<&Row>, Vec<&Row>) = rows.iter().partition(|r| r.kind == Kind::Direct);
    if direct.is_empty() {
        return summary(out, &prs, unit == Unit::Pr, "PRS");
    }
    if !prs.is_empty() {
        writeln!(out, "== Pull requests ==")?;
        summary(out, &prs, true, "PRS")?;
        writeln!(out)?;
    }
    writeln!(out, "== Direct commits (outside any PR) ==")?;
    summary(out, &direct, false, "COMMITS")
}

#[derive(Default)]
struct Agg {
    label: String,
    units: usize,
    commits: u64,
    added: u64,
    removed: u64,
}

/// `pr_mode`: rows are PRs, so also show how many commits each entry accounts for.
fn summary(out: &mut impl Write, rows: &[&Row], pr_mode: bool, noun: &str) -> io::Result<()> {
    if rows.is_empty() {
        return writeln!(out, "nothing found");
    }
    let pct = |n: usize, d: usize| 100.0 * n as f64 / d as f64;
    let total_commits: u64 = rows.iter().map(|r| r.commits as u64).sum();
    let ai_commits: u64 = rows.iter().map(|r| r.ai.ai_commits as u64).sum();
    let ai_rows = rows.iter().filter(|r| !r.ai.is_empty()).count();
    write!(
        out,
        "{ai_rows} of {} {} credit AI ({:.0}%)",
        rows.len(),
        noun.to_lowercase(),
        pct(ai_rows, rows.len())
    )?;
    if pr_mode {
        write!(
            out,
            "; {ai_commits} of {total_commits} commits ({:.0}%)",
            pct(ai_commits as usize, total_commits as usize)
        )?;
    }
    writeln!(
        out,
        "\n(an entry crediting several models counts under each)"
    )?;

    let commits_col = |a: &Agg| {
        if pr_mode {
            format!("  {:>7}", a.commits)
        } else {
            String::new()
        }
    };
    let head = |out: &mut dyn Write, title: &str| {
        writeln!(
            out,
            "\n{:<30}  {:>5}  {:>5}{}  {:>9}  {:>9}",
            title,
            noun,
            "%",
            if pr_mode {
                format!("  {:>7}", "COMMITS")
            } else {
                String::new()
            },
            "+LINES",
            "-LINES"
        )
    };
    let line = |out: &mut dyn Write, a: &Agg| {
        writeln!(
            out,
            "{:<30}  {:>5}  {:>4.0}%{}  {:>9}  {:>9}",
            trunc(&a.label, 30),
            a.units,
            pct(a.units, rows.len()),
            commits_col(a),
            a.added,
            a.removed
        )
    };

    let by_credit = |pick: fn(&Usage) -> &Vec<Credit>| {
        // Keyed case-insensitively, shown with the first spelling seen.
        let mut m: HashMap<String, Agg> = HashMap::new();
        for r in rows {
            for c in pick(&r.ai) {
                let a = m.entry(c.name.to_lowercase()).or_default();
                if a.label.is_empty() {
                    a.label = c.name.clone();
                }
                a.units += 1;
                a.commits += c.commits as u64;
                a.added += r.added;
                a.removed += r.removed;
            }
        }
        let mut v: Vec<_> = m.into_values().collect();
        v.sort_by(|a, b| b.units.cmp(&a.units).then_with(|| a.label.cmp(&b.label)));
        v
    };
    for (title, list) in [
        ("MODEL (Co-Authored-By)", by_credit(|u| &u.models)),
        ("TOOL (Generated with)", by_credit(|u| &u.tools)),
    ] {
        if list.is_empty() {
            continue;
        }
        head(out, title)?;
        for a in &list {
            line(out, a)?;
        }
    }

    let mut any = Agg {
        label: "(any AI credit)".into(),
        ..Agg::default()
    };
    let mut none = Agg {
        label: "(no attribution)".into(),
        ..Agg::default()
    };
    for r in rows {
        let a = if r.ai.is_empty() { &mut none } else { &mut any };
        a.units += 1;
        a.commits += (r.commits - r.ai.ai_commits) as u64 * r.ai.is_empty() as u64
            + r.ai.ai_commits as u64 * !r.ai.is_empty() as u64;
        a.added += r.added;
        a.removed += r.removed;
    }
    head(out, "TOTAL")?;
    line(out, &any)?;
    line(out, &none)?;

    let mut repos: Vec<(Agg, usize)> = Vec::new(); // (everything, entries crediting AI)
    for r in rows {
        let i = repos
            .iter()
            .position(|x| x.0.label == r.repo)
            .unwrap_or_else(|| {
                repos.push((
                    Agg {
                        label: r.repo.clone(),
                        ..Agg::default()
                    },
                    0,
                ));
                repos.len() - 1
            });
        let (a, ai) = &mut repos[i];
        a.units += 1;
        a.commits += r.commits as u64;
        a.added += r.added;
        a.removed += r.removed;
        *ai += !r.ai.is_empty() as usize;
    }
    if repos.len() > 1 {
        repos.sort_by(|a, b| {
            b.0.units
                .cmp(&a.0.units)
                .then_with(|| a.0.label.cmp(&b.0.label))
        });
        writeln!(
            out,
            "\n{:<30}  {:>5}  {:>5}  {:>5}  {:>9}  {:>9}",
            "REPO", noun, "AI", "AI%", "+LINES", "-LINES"
        )?;
        for (a, ai) in &repos {
            writeln!(
                out,
                "{:<30}  {:>5}  {:>5}  {:>4.0}%  {:>9}  {:>9}",
                trunc(&a.label, 30),
                a.units,
                ai,
                pct(*ai, a.units),
                a.added,
                a.removed
            )?;
        }
    }
    Ok(())
}

/// Models / tools are listed most commits first, `;`-separated, with parallel count columns.
pub fn csv(out: &mut impl Write, rows: &[Row]) -> io::Result<()> {
    writeln!(
        out,
        "kind,repo,date,pr,commit,author,commits,ai_commits,lines_added,lines_removed,ai_models,ai_model_commits,ai_tools,ai_tool_commits,subject"
    )?;
    let names = |l: &[Credit]| {
        l.iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(";")
    };
    let counts = |l: &[Credit]| {
        l.iter()
            .map(|c| c.commits.to_string())
            .collect::<Vec<_>>()
            .join(";")
    };
    for r in rows {
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.kind.name(),
            esc(&r.repo),
            r.date,
            r.pr.map(|n| n.to_string()).unwrap_or_default(),
            r.commit,
            esc(&r.author),
            r.commits,
            r.ai.ai_commits,
            r.added,
            r.removed,
            esc(&names(&r.ai.models)),
            counts(&r.ai.models),
            esc(&names(&r.ai.tools)),
            counts(&r.ai.tools),
            esc(&r.subject)
        )?;
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

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}
