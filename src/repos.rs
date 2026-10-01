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
    /// Revision to start walking back from. Default: each repository's remote default branch
    /// (origin/HEAD, e.g. origin/main), not whatever happens to be checked out. Pass `HEAD`
    /// for the checked-out branch.
    pub rev: Option<String>,

    /// Path to a git repository (repeatable; default: current directory).
    #[arg(short = 'C', long = "repo")]
    pub repos: Vec<PathBuf>,

    /// A repository to clone temporarily and analyze: `owner/repo` (GitHub over SSH) or any git
    /// URL (repeatable). Cloned without file contents into the system temp directory and
    /// deleted when the run ends. A clone is current, so `--fetch` skips it.
    #[arg(short = 'R', long = "remote", value_name = "REPO")]
    pub remotes: Vec<String>,

    /// Recursively find git checkouts under this directory (repeatable).
    #[arg(long, value_name = "DIR")]
    pub scan: Vec<PathBuf>,

    /// How many directory levels --scan descends.
    #[arg(long, default_value_t = 6)]
    pub scan_depth: usize,

    /// `git fetch` every repository first, so fresh history is analyzed. Also lets a repository
    /// without a local `origin/HEAD` ask the remote for its default branch.
    #[arg(long)]
    pub fetch: bool,

    /// `dump` only: also record commits that are on remote branches but not yet in the walked
    /// revision (unmerged work), tagged `"unmerged":true` with the branch name. With `--fetch`,
    /// branches already deleted on the remote are ignored. A branch that was squash-merged but
    /// not deleted still shows up, as its commits are not reachable from the target.
    #[arg(long)]
    pub unmerged: bool,

    /// `dump` only: also store the text that identifies the work: `title` on PR records and
    /// `subject` on commit records. Off by default, so dumps stay free of commit text.
    #[arg(long)]
    pub titles: bool,

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
        if dirs.is_empty() && self.scan.is_empty() && self.remotes.is_empty() {
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
        let mut repos = dedupe_worktrees(repos);
        if !self.remotes.is_empty() {
            let root = std::sync::Arc::new(git::TempRoot::new()?);
            // Cloning waits on the network, so use a wider pool than --jobs.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads((rayon::current_num_threads() * 2).max(8))
                .build()?;
            let cloned: Vec<Option<git::Repo>> = pool.install(|| {
                self.remotes
                    .par_iter()
                    .map(|spec| match git::Repo::clone_remote(spec, &root) {
                        Ok(r) => Some(r),
                        Err(e) => {
                            eprintln!("skipping {spec}: {e:#}");
                            None
                        }
                    })
                    .collect()
            });
            repos.extend(cloned.into_iter().flatten());
        }
        if self.fetch {
            // A repository that can't be fetched is stale, so it is left out of the run.
            // Fetching waits on the network, not the CPU, so use a wider pool than --jobs.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads((rayon::current_num_threads() * 4).max(16))
                .build()?;
            let fetched: Vec<bool> = pool.install(|| {
                repos
                    .par_iter()
                    .map(|r| match if r.is_temp() { Ok(()) } else { r.fetch() } {
                        Ok(()) => true,
                        Err(e) => {
                            eprintln!("skipping {}: fetch failed: {e:#}", r.name());
                            false
                        }
                    })
                    .collect()
            });
            let mut ok = fetched.into_iter();
            repos.retain(|_| ok.next().unwrap_or(true));
        }
        Ok(repos)
    }

    /// The revision to walk in `repo`: the one given, else the remote's default branch, falling
    /// back to the current branch's upstream and finally `HEAD` (e.g. no remote).
    pub fn rev_for(&self, repo: &git::Repo) -> String {
        match &self.rev {
            Some(rev) => rev.clone(),
            None => repo
                .default_branch(self.fetch)
                .or_else(|| repo.upstream())
                .unwrap_or_else(|| "HEAD".to_string()),
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

    /// Every first-parent commit of `repo` in the window, newest first, as
    /// `(commit, PR number, is a PR)`. Commits that aren't PRs were pushed straight to the
    /// branch, or are `git pull` sync merges (merging the branch's own upstream back in).
    fn classified(
        &self,
        repo: &git::Repo,
        stats: bool,
    ) -> Result<Vec<(git::Commit, Option<u64>, bool)>> {
        let rev = self.rev_for(repo);
        let commits = repo.first_parent_log(
            &rev,
            self.since_date().as_deref(),
            self.until.as_deref(),
            stats,
        )?;
        let trunk = repo.branch_name(&rev);
        Ok(commits
            .into_iter()
            .map(|c| {
                let n = pr::pr_number(&c);
                let sync = n.is_none()
                    && c.parents.len() > 1
                    && pr::merge_info(&c.subject)
                        .is_some_and(|m| m.pull || Some(&m.branch) == trunk.as_ref());
                let is_pr = !sync
                    && match self.mode {
                        Mode::All => true,
                        Mode::Merges => c.parents.len() > 1,
                        Mode::Prs => c.parents.len() > 1 || n.is_some(),
                    };
                (c, n, is_pr)
            })
            .collect())
    }

    /// All first-parent commits, PRs and not, with `-n` counting both together (for `dump`).
    pub fn select_with_direct(
        &self,
        repo: &git::Repo,
    ) -> Result<Vec<(git::Commit, Option<u64>, bool)>> {
        let mut all = self.classified(repo, true)?;
        all.truncate(self.max_count.unwrap_or(usize::MAX));
        Ok(all)
    }

    /// The PR-like commits of `repo`, newest first, with their PR numbers.
    pub fn select(&self, repo: &git::Repo) -> Result<Selected> {
        Ok(self
            .classified(repo, false)?
            .into_iter()
            .filter(|(_, _, is_pr)| *is_pr)
            .map(|(c, n, _)| (c, n))
            .take(self.max_count.unwrap_or(usize::MAX))
            .collect())
    }
}
