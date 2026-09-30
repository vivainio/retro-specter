//! The raw dump: one JSON object per line describing every PR and commit in a repository.
//!
//! Aggregators (`ai`, or your own scripts) work from this and never touch git, so a dump
//! can be produced once (per repo, as `<repo>.jsonl`) and analyzed many times.
//!
//! * `{"type":"pr", ...}`     a merged PR: a merge commit, or a squash / numbered commit.
//! * `{"type":"commit", ...}` a non-merge commit. `pr_merge` is the SHA of the PR record
//!   (merge or squash commit) that brought it in, `null` for a direct commit.
//!
//! A squash-merged PR yields both a `pr` record and a `commit` record for the same SHA.

use crate::git::{Commit, Repo};
use crate::pr;
use crate::repos::RepoArgs;
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Record {
    Commit(CommitRec),
    Pr(PrRec),
}

#[derive(Serialize, Deserialize)]
pub struct CommitRec {
    pub repo: String,
    pub sha: String,
    /// SHA of the PR record containing this commit; `None` for a direct commit.
    pub pr_merge: Option<String>,
    /// That PR's number, when one could be parsed.
    pub pr: Option<u64>,
    /// The merged branch's name, when the merge subject gives one (a pseudo-PR label for
    /// merges without a number).
    #[serde(default)]
    pub branch: Option<String>,
    pub author: String,
    pub date: String,
    pub subject: String,
    /// Full commit message (subject and body), so trailers can be re-parsed later.
    pub message: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Serialize, Deserialize)]
pub struct PrRec {
    pub repo: String,
    /// The merge commit, or the squash commit itself.
    pub sha: String,
    pub pr: Option<u64>,
    /// The merged branch's name, when the merge subject gives one.
    #[serde(default)]
    pub branch: Option<String>,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub message: String,
    /// The PR's net change against its first parent.
    pub added: u64,
    pub removed: u64,
    /// Number of `commit` records that belong to it.
    pub commits: u32,
    pub squash: bool,
}

fn message(c: &Commit) -> String {
    if c.body.is_empty() {
        c.subject.clone()
    } else {
        format!("{}\n\n{}", c.subject, c.body)
    }
}

fn commit_rec(
    name: &str,
    c: &Commit,
    pr_merge: Option<&str>,
    pr: Option<u64>,
    branch: Option<&str>,
) -> Record {
    Record::Commit(CommitRec {
        repo: name.to_string(),
        sha: c.id.clone(),
        pr_merge: pr_merge.map(String::from),
        pr,
        branch: branch.map(String::from),
        author: c.author.clone(),
        date: c.date.clone(),
        subject: c.subject.clone(),
        message: message(c),
        added: c.added,
        removed: c.removed,
    })
}

/// All records for `repo`, newest first; each PR is followed by its commits.
pub fn dump_repo(repo: &Repo, args: &RepoArgs) -> Result<Vec<Record>> {
    let name = repo.name();
    let items = args.select_with_direct(repo)?;
    let groups = items
        .par_iter()
        .map(|(c, pr, is_pr)| one(repo, &name, c, *pr, *is_pr))
        .collect::<Result<Vec<_>>>()?;
    Ok(groups.into_iter().flatten().collect())
}

fn one(repo: &Repo, name: &str, c: &Commit, pr: Option<u64>, is_pr: bool) -> Result<Vec<Record>> {
    let merge = c.parents.len() > 1;
    if !is_pr && !merge {
        return Ok(vec![commit_rec(name, c, None, None, None)]);
    }
    if !is_pr {
        // A sync merge (git pull): the merge itself is noise, the commits it brought in
        // are trunk commits made elsewhere.
        let base = &c.parents[0];
        let members = repo.commit_log(&format!("{base}..{}", c.id), None, None, None)?;
        return Ok(members
            .iter()
            .map(|m| commit_rec(name, m, None, None, None))
            .collect());
    }
    let branch = pr::merge_info(&c.subject).map(|m| m.branch);
    let pr_rec = |added, removed, commits, squash| {
        Record::Pr(PrRec {
            repo: name.to_string(),
            sha: c.id.clone(),
            pr,
            branch: branch.clone(),
            author: c.author.clone(),
            date: c.date.clone(),
            subject: c.subject.clone(),
            message: message(c),
            added,
            removed,
            commits,
            squash,
        })
    };
    if !merge {
        // Squash / rebase-numbered commit: the commit is the whole PR.
        return Ok(vec![
            pr_rec(c.added, c.removed, 1, true),
            commit_rec(name, c, Some(&c.id), pr, None),
        ]);
    }
    let base = &c.parents[0];
    let members = repo.commit_log(&format!("{base}..{}", c.id), None, None, None)?;
    let (added, removed) = repo.numstat(base, &c.id)?;
    let mut out = vec![pr_rec(added, removed, members.len() as u32, false)];
    out.extend(
        members
            .iter()
            .map(|m| commit_rec(name, m, Some(&c.id), pr, branch.as_deref())),
    );
    Ok(out)
}

pub fn write_lines(out: &mut impl Write, records: &[Record]) -> Result<()> {
    for r in records {
        serde_json::to_writer(&mut *out, r)?;
        writeln!(out)?;
    }
    Ok(())
}

/// Writes `records` to `dir/<file>` atomically, so a failed run never clobbers a good dump.
pub fn write_file(dir: &Path, file: &str, records: &[Record]) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{file}.tmp"));
    let mut w = BufWriter::new(fs::File::create(&tmp)?);
    write_lines(&mut w, records)?;
    w.flush()?;
    drop(w);
    fs::rename(&tmp, dir.join(file))?;
    Ok(())
}

/// A file name for `repo` that's unique among `used`: `name.jsonl`, or `name-<hash>.jsonl`
/// when two repositories share a directory name.
pub fn file_name(repo: &Repo, used: &mut HashSet<String>) -> String {
    let name = repo.name();
    let mut f = format!("{name}.jsonl");
    if !used.insert(f.clone()) {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        repo.dir()
            .canonicalize()
            .unwrap_or_else(|_| repo.dir().to_path_buf())
            .hash(&mut h);
        f = format!("{name}-{:08x}.jsonl", h.finish() as u32);
        used.insert(f.clone());
    }
    f
}

/// Reads records from dump files; directories contribute their `*.jsonl` files (sorted).
pub fn read(paths: &[std::path::PathBuf]) -> Result<Vec<Record>> {
    use std::io::{BufRead, BufReader};
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut found: Vec<_> = fs::read_dir(p)?
                .flatten()
                .map(|e| e.path())
                .filter(|f| f.extension().is_some_and(|x| x == "jsonl"))
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(p.clone());
        }
    }
    let mut records = Vec::new();
    let mut load = |name: &str, r: &mut dyn BufRead| -> Result<()> {
        for (i, line) in r.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            records.push(
                serde_json::from_str(&line)
                    .with_context(|| format!("{name}:{}: not a dump record", i + 1))?,
            );
        }
        Ok(())
    };
    if files.is_empty() {
        load("<stdin>", &mut std::io::stdin().lock())?;
    }
    for f in &files {
        let file = fs::File::open(f).with_context(|| format!("opening {}", f.display()))?;
        load(&f.display().to_string(), &mut BufReader::new(file))?;
    }
    Ok(records)
}
