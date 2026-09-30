//! Choosing repositories and the PRs to look at; shared by all commands.

use crate::{discover, git, pr};
use anyhow::Result;
use clap::{Args, ValueEnum};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Copy, ValueEnum, PartialEq)]
pub enum Mode {
    /// Merge commits, plus commits whose message references a PR (squash merges).
    Prs,
    /// Only merge commits.
    Merges,
    /// Every commit on the first-parent chain.
    All,
}

#[derive(Args)]
pub struct RepoArgs {
    /// Revision to start walking back from.
    #[arg(default_value = "HEAD")]
    pub rev: String,

    /// Path to a git repository (repeatable; default: current directory).
    #[arg(short = 'C', long = "repo")]
    pub repos: Vec<PathBuf>,

    /// Recursively find git checkouts under this directory (repeatable).
    #[arg(long, value_name = "DIR")]
    pub scan: Vec<PathBuf>,

    /// How many directory levels --scan descends.
    #[arg(long, default_value_t = 6)]
    pub scan_depth: usize,

    /// `git fetch` every repository first. A default `HEAD` then becomes the current
    /// branch's upstream (e.g. origin/main) when it has one, so fresh history is analyzed.
    #[arg(long)]
    pub fetch: bool,

    /// Maximum number of PRs per repository (newest first).
    #[arg(short = 'n', long)]
    pub max_count: Option<usize>,

    /// Only PRs merged after this date (anything `git log --since` accepts).
    #[arg(long)]
    pub since: Option<String>,

    /// Only PRs from the last N months.
    #[arg(long, value_name = "N", conflicts_with_all = ["since", "days"])]
    pub months: Option<u32>,

    /// Only PRs from the last N days.
    #[arg(long, value_name = "N", conflicts_with = "since")]
    pub days: Option<u32>,

    /// Only PRs merged before this date.
    #[arg(long)]
    pub until: Option<String>,

    /// Which first-parent commits count as PRs.
    #[arg(long, value_enum, default_value_t = Mode::Prs)]
    pub mode: Mode,
}

/// Keeps one checkout per repository: linked worktrees of a repository that is already in the
/// set are dropped (preferring the main checkout), since they share the same history.
fn dedupe_worktrees(repos: Vec<git::Repo>) -> Vec<git::Repo> {
    let mut kept: Vec<(Option<std::path::PathBuf>, bool, git::Repo)> = Vec::new();
    for r in repos {
        let (common, linked) = match r.identity() {
            Some((c, l)) => (Some(c), l),
            None => (None, false),
        };
        match kept.iter_mut().find(|k| k.0.is_some() && k.0 == common) {
            Some(k) if k.1 && !linked => {
                eprintln!(
                    "skipping worktree {}: same repository as {}",
                    k.2.dir().display(),
                    r.dir().display()
                );
                *k = (common, linked, r);
            }
            Some(k) => eprintln!(
                "skipping worktree {}: same repository as {}",
                r.dir().display(),
                k.2.dir().display()
            ),
            None => kept.push((common, linked, r)),
        }
    }
    kept.into_iter().map(|k| k.2).collect()
}

pub type Selected = Vec<(git::Commit, Option<u64>)>;

impl RepoArgs {
    /// Resolves `-C` and `--scan` into opened, de-duplicated repositories, fetching if asked.
    pub fn open_repos(&self) -> Result<Vec<git::Repo>> {
        let mut dirs = self.repos.clone();
        for root in &self.scan {
            let found = discover::find_repos(root, self.scan_depth);
            eprintln!(
                "found {} repositories under {}",
                found.len(),
                root.display()
            );
            dirs.extend(found);
        }
        if dirs.is_empty() && self.scan.is_empty() {
            dirs.push(PathBuf::from("."));
        }
        let mut seen = HashSet::new();
        dirs.retain(|d| seen.insert(d.canonicalize().unwrap_or_else(|_| d.clone())));

        let mut repos = Vec::new();
        for d in &dirs {
            match git::Repo::open(d) {
                Ok(r) => repos.push(r),
                // With several repositories, one bad path shouldn't sink the whole run.
                Err(e) if dirs.len() > 1 => eprintln!("skipping {}: {e:#}", d.display()),
                Err(e) => return Err(e),
            }
        }
        let repos = dedupe_worktrees(repos);
        if self.fetch {
            repos.par_iter().for_each(|r| {
                if let Err(e) = r.fetch() {
                    eprintln!("warning: fetch failed for {}: {e:#}", r.name());
                }
            });
        }
        Ok(repos)
    }

    /// The revision to walk in `repo`: with --fetch, a default `HEAD` becomes the upstream.
    pub fn rev_for(&self, repo: &git::Repo) -> String {
        match (self.fetch, self.rev.as_str()) {
            (true, "HEAD") => repo.upstream().unwrap_or_else(|| self.rev.clone()),
            _ => self.rev.clone(),
        }
    }

    /// The lower date bound from --months / --days / --since.
    pub fn since_date(&self) -> Option<String> {
        match (self.months, self.days) {
            (Some(m), _) => Some(format!("{m} months ago")),
            (_, Some(d)) => Some(format!("{d} days ago")),
            _ => self.since.clone(),
        }
    }

    /// Like `select`, but also returns the first-parent commits that aren't PRs (pushed straight
    /// to the branch), flagged `false`. `-n` counts both together. With `--mode all` every
    /// commit counts as a PR.
    pub fn select_with_direct(
        &self,
        repo: &git::Repo,
    ) -> Result<Vec<(git::Commit, Option<u64>, bool)>> {
        let commits = repo.first_parent_log(
            &self.rev_for(repo),
            self.since_date().as_deref(),
            self.until.as_deref(),
        )?;
        Ok(commits
            .into_iter()
            .map(|c| {
                let n = pr::pr_number(&c);
                let is_pr = match self.mode {
                    Mode::All => true,
                    Mode::Merges => c.parents.len() > 1,
                    Mode::Prs => c.parents.len() > 1 || n.is_some(),
                };
                (c, n, is_pr)
            })
            .take(self.max_count.unwrap_or(usize::MAX))
            .collect())
    }

    /// The PR-like commits of `repo`, newest first, with their PR numbers.
    pub fn select(&self, repo: &git::Repo) -> Result<Selected> {
        let rev = self.rev_for(repo);
        let since = self.since_date();
        let commits = repo.first_parent_log(&rev, since.as_deref(), self.until.as_deref())?;
        Ok(commits
            .into_iter()
            .map(|c| {
                let n = pr::pr_number(&c);
                (c, n)
            })
            .filter(|(c, n)| match self.mode {
                Mode::All => true,
                Mode::Merges => c.parents.len() > 1,
                Mode::Prs => c.parents.len() > 1 || n.is_some(),
            })
            .take(self.max_count.unwrap_or(usize::MAX))
            .collect())
    }
}
