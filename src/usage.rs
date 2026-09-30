//! AI usage aggregated from a raw dump (see `dump`); never touches git.

use crate::ai::{self, Credit, Usage};
use crate::dump::{CommitRec, Record};
use clap::ValueEnum;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Write};

/// What one row of the report is.
#[derive(Clone, Copy, ValueEnum, PartialEq)]
pub enum Unit {
    /// Merged PRs (models of all their commits combined, most commits first) plus direct commits.
    Pr,
    /// Individual commits.
    Commit,
}

/// What a row stands for.
#[derive(Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A merged PR (all its commits combined).
    Pr,
    /// A commit outside any PR.
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
    /// Merged branch name: the label of a PR that has no number.
    pub branch: Option<String>,
    pub author: String,
    pub date: String,
    pub subject: String,
    /// Commits in the row (1 unless it's a PR).
    pub commits: u32,
    /// Lines added / removed, all file types. For a PR this is its net change against the
    /// target branch, so lines a later commit rewrote or reverted don't count twice.
    pub added: u64,
    pub removed: u64,
    /// Sum over the row's commits of their own lines (>= the net figures when commits
    /// undo each other's work). Equals `added` / `removed` for a single commit.
    pub churn_added: u64,
    pub churn_removed: u64,
    /// Lines credited to AI: the row's net lines scaled by the share of commit churn that
    /// AI-credited commits account for. Exact for single commits and for PRs where all or
    /// none of the commits credit AI; an estimate for mixed PRs.
    pub ai_added: u64,
    pub ai_removed: u64,
    /// Per-commit facts: which models / tools, and the raw lines of the credited commits.
    pub ai: Usage,
}

impl Row {
    /// `x`, a share of this row's commit churn, scaled onto the row's net lines.
    fn scale(&self, x: u64, churn: u64, net: u64) -> u64 {
        if churn == 0 || x >= churn {
            return if churn == 0 { 0 } else { net };
        }
        (x as u128 * net as u128 / churn as u128) as u64
    }

    fn scale_added(&self, x: u64) -> u64 {
        self.scale(x, self.churn_added, self.added)
    }

    fn scale_removed(&self, x: u64) -> u64 {
        self.scale(x, self.churn_removed, self.removed)
    }
}

/// Turns dump records into report rows, in dump order.
pub fn rows(records: &[Record], unit: Unit) -> Vec<Row> {
    let commit_row = |c: &CommitRec, kind| Row {
        churn_added: c.added,
        churn_removed: c.removed,
        ai_added: if ai::detect(&c.message).is_empty() {
            0
        } else {
            c.added
        },
        ai_removed: if ai::detect(&c.message).is_empty() {
            0
        } else {
            c.removed
        },
        kind,
        repo: c.repo.clone(),
        commit: c.sha.clone(),
        pr: c.pr,
        branch: c.branch.clone(),
        author: c.author.clone(),
        date: c.date.clone(),
        subject: c.subject.clone(),
        commits: 1,
        added: c.added,
        removed: c.removed,
        ai: ai::combine([(c.message.as_str(), c.added, c.removed)]),
    };
    if unit == Unit::Commit {
        return records
            .iter()
            .filter_map(|r| match r {
                Record::Commit(c) => Some(commit_row(c, Kind::Commit)),
                Record::Pr(_) => None,
            })
            .collect();
    }

    let mut members: HashMap<(&str, &str), Vec<&CommitRec>> = HashMap::new();
    for r in records {
        if let Record::Commit(c) = r
            && let Some(m) = &c.pr_merge
        {
            members.entry((&c.repo, m)).or_default().push(c);
        }
    }
    records
        .iter()
        .filter_map(|r| match r {
            Record::Commit(c) if c.pr_merge.is_none() => Some(commit_row(c, Kind::Direct)),
            Record::Commit(_) => None,
            Record::Pr(p) => {
                let ms = members
                    .get(&(p.repo.as_str(), p.sha.as_str()))
                    .map_or(&[][..], Vec::as_slice);
                // An empty merge has no commits of its own; its message is all there is.
                let ai = if ms.is_empty() {
                    ai::combine([(p.message.as_str(), 0, 0)])
                } else {
                    ai::combine(ms.iter().map(|c| (c.message.as_str(), c.added, c.removed)))
                };
                let churn_added: u64 = ms.iter().map(|c| c.added).sum();
                let churn_removed: u64 = ms.iter().map(|c| c.removed).sum();
                let mut row = Row {
                    churn_added,
                    churn_removed,
                    ai_added: 0,
                    ai_removed: 0,
                    kind: Kind::Pr,
                    repo: p.repo.clone(),
                    commit: p.sha.clone(),
                    pr: p.pr,
                    branch: p.branch.clone(),
                    author: p.author.clone(),
                    date: p.date.clone(),
                    subject: p.subject.clone(),
                    commits: ms.len() as u32,
                    added: p.added,
                    removed: p.removed,
                    ai,
                };
                row.ai_added = row.scale_added(row.ai.ai_added);
                row.ai_removed = row.scale_removed(row.ai.ai_removed);
                Some(row)
            }
        })
        .collect()
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
                "{:<20}  {:<10}  {:>14}  {:<9}  {:>7}  {:>7}  {:>7}  {:<40}  {}",
                trunc(&r.repo, 20),
                r.date.get(..10).unwrap_or(&r.date),
                match (r.kind, r.pr, &r.branch) {
                    (Kind::Direct, ..) => "direct".into(),
                    (_, Some(n), _) => format!("#{n}"),
                    (_, None, Some(b)) => trunc(b, 14),
                    _ => String::new(),
                },
                &r.commit[..9.min(r.commit.len())],
                r.added,
                r.removed,
                r.ai_added,
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
/// Line counts are net; see `Row::ai_added` for how AI lines are attributed.
fn summary(out: &mut impl Write, rows: &[&Row], pr_mode: bool, noun: &str) -> io::Result<()> {
    if rows.is_empty() {
        return writeln!(out, "nothing found");
    }
    let pct = |n: u64, d: u64| 100.0 * n as f64 / d.max(1) as f64;
    let total_commits: u64 = rows.iter().map(|r| r.commits as u64).sum();
    let ai_commits: u64 = rows.iter().map(|r| r.ai.ai_commits as u64).sum();
    let ai_rows = rows.iter().filter(|r| !r.ai.is_empty()).count() as u64;
    write!(
        out,
        "{ai_rows} of {} {} include AI-credited commits ({:.0}%)",
        rows.len(),
        noun.to_lowercase(),
        pct(ai_rows, rows.len() as u64)
    )?;
    if pr_mode {
        write!(
            out,
            "; {ai_commits} of {total_commits} commits ({:.0}%)",
            pct(ai_commits, total_commits)
        )?;
    }
    writeln!(
        out,
        "\n(lines are PR net lines; for PRs mixing AI and other commits the AI share is estimated from commit churn; an entry crediting several models counts under each)"
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
            pct(a.units as u64, rows.len() as u64),
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
                a.added += r.scale_added(c.added);
                a.removed += r.scale_removed(c.removed);
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

    // Commit-level split: each row counts the entries containing such commits.
    let mut ai = Agg {
        label: "(AI-credited commits)".into(),
        ..Agg::default()
    };
    let mut human = Agg {
        label: "(other commits)".into(),
        ..Agg::default()
    };
    for r in rows {
        let other = (r.commits - r.ai.ai_commits) as u64;
        if r.ai.ai_commits > 0 {
            ai.units += 1;
            ai.commits += r.ai.ai_commits as u64;
            ai.added += r.ai_added;
            ai.removed += r.ai_removed;
        }
        if other > 0 {
            human.units += 1;
            human.commits += other;
            human.added += r.added - r.ai_added;
            human.removed += r.removed - r.ai_removed;
        }
    }
    head(out, "TOTAL")?;
    line(out, &ai)?;
    line(out, &human)?;

    // (everything, entries with AI, AI lines added / removed)
    let mut repos: Vec<(Agg, u64, u64, u64)> = Vec::new();
    for r in rows {
        let i = repos
            .iter()
            .position(|x| x.0.label == r.repo)
            .unwrap_or_else(|| {
                let a = Agg {
                    label: r.repo.clone(),
                    ..Agg::default()
                };
                repos.push((a, 0, 0, 0));
                repos.len() - 1
            });
        let (a, n_ai, ai_add, ai_rem) = &mut repos[i];
        a.units += 1;
        a.added += r.added;
        a.removed += r.removed;
        *n_ai += !r.ai.is_empty() as u64;
        *ai_add += r.ai_added;
        *ai_rem += r.ai_removed;
    }
    if repos.len() > 1 {
        repos.sort_by(|a, b| {
            b.0.units
                .cmp(&a.0.units)
                .then_with(|| a.0.label.cmp(&b.0.label))
        });
        writeln!(
            out,
            "\n{:<30}  {:>5}  {:>5}  {:>5}  {:>9}  {:>9}  {:>9}  {:>9}",
            "REPO", noun, "AI", "AI%", "+LINES", "-LINES", "AI +LINES", "AI -LINES"
        )?;
        for (a, n_ai, ai_add, ai_rem) in &repos {
            writeln!(
                out,
                "{:<30}  {:>5}  {:>5}  {:>4.0}%  {:>9}  {:>9}  {:>9}  {:>9}",
                trunc(&a.label, 30),
                a.units,
                n_ai,
                pct(*n_ai, a.units as u64),
                a.added,
                a.removed,
                ai_add,
                ai_rem
            )?;
        }
    }
    Ok(())
}

/// Models / tools are listed most commits first, `;`-separated, with parallel count columns.
pub fn csv(out: &mut impl Write, rows: &[Row]) -> io::Result<()> {
    writeln!(
        out,
        "kind,repo,date,pr,branch,commit,author,commits,ai_commits,lines_added,lines_removed,churn_added,churn_removed,ai_lines_added,ai_lines_removed,ai_models,ai_model_commits,ai_tools,ai_tool_commits,subject"
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
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.kind.name(),
            esc(&r.repo),
            r.date,
            r.pr.map(|n| n.to_string()).unwrap_or_default(),
            esc(r.branch.as_deref().unwrap_or_default()),
            r.commit,
            esc(&r.author),
            r.commits,
            r.ai.ai_commits,
            r.added,
            r.removed,
            r.churn_added,
            r.churn_removed,
            r.ai_added,
            r.ai_removed,
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
