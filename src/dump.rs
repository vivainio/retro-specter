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
//!
//! No commit text is stored: neither subjects nor messages. Each record keeps only what was
//! derived from the message at dump time: the PR number, the merged branch name, and the AI
//! models and tools credited.

use crate::ai;
use crate::git::{Commit, Repo};
use crate::github::{self, PrInfo as GhInfo};
use crate::pr;
use crate::repos::RepoArgs;
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_merge: Option<String>,
    /// That PR's number, when one could be parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The merged branch's name, when the merge subject gives one (a pseudo-PR label for
    /// merges without a number).
    #[serde(default)]
    pub branch: Option<String>,
    pub author: String,
    /// Author email (`.mailmap` applied); absent when authors are pseudonymized.
    #[serde(default)]
    pub author_email: Option<String>,
    pub date: String,
    /// AI models (`Co-Authored-By:` trailers) and tools (`Generated with` lines) the commit
    /// message credits.
    #[serde(default)]
    pub ai_models: Vec<String>,
    #[serde(default)]
    pub ai_tools: Vec<String>,
    pub added: u64,
    pub removed: u64,
    /// Jira-style ticket keys in the merged / unmerged branch name or the commit message (or
    /// the PR's, for a commit inside a PR); only the keys are kept. Omitted when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tickets: Vec<String>,
    /// The commit's subject line; only with `--titles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Present (as `true`) only on a remote branch that is not merged into the walked
    /// revision; `branch` names it. Merged and direct commits omit the field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unmerged: bool,
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
    #[serde(default)]
    pub author_email: Option<String>,
    pub date: String,
    /// Credits in the merge commit's own message (for a squash, the commit's message).
    #[serde(default)]
    pub ai_models: Vec<String>,
    #[serde(default)]
    pub ai_tools: Vec<String>,
    /// The PR's net change against its first parent.
    pub added: u64,
    pub removed: u64,
    /// Number of `commit` records that belong to it.
    pub commits: u32,
    pub squash: bool,
    /// Jira-style ticket keys in the branch name or the merge / squash commit message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tickets: Vec<String>,
    /// The PR's title; only with `--titles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// GitHub's data for the PR; only with `--github`, and only for PRs it knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<GhInfo>,
}

/// Model and tool names credited by `c`'s message.
fn credits(c: &Commit) -> (Vec<String>, Vec<String>) {
    let u = ai::detect(&format!("{}\n\n{}", c.subject, c.body));
    let names = |l: Vec<ai::Credit>| l.into_iter().map(|c| c.name).collect();
    (names(u.models), names(u.tools))
}

fn commit_rec(
    name: &str,
    c: &Commit,
    pr_merge: Option<&str>,
    pr: Option<u64>,
    branch: Option<&str>,
    inherited: &[String],
    titles: bool,
) -> Record {
    let mut tickets = pr::tickets(branch, &[&c.subject, &c.body]);
    for t in inherited {
        if !tickets.contains(t) {
            tickets.push(t.clone());
        }
    }
    Record::Commit(CommitRec {
        repo: name.to_string(),
        sha: c.id.clone(),
        pr_merge: pr_merge.map(String::from),
        pr,
        branch: branch.map(String::from),
        author: c.author.clone(),
        author_email: Some(c.email.clone()),
        date: c.date.clone(),
        ai_models: credits(c).0,
        ai_tools: credits(c).1,
        added: c.added,
        removed: c.removed,
        tickets,
        subject: titles.then(|| c.subject.clone()),
        unmerged: false,
    })
}

/// Repositories whose `--github` query failed, so their PRs lack GitHub data.
static GITHUB_FAILED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Names of the repositories whose GitHub query failed during this run, sorted.
pub fn github_failures() -> Vec<String> {
    let mut v = GITHUB_FAILED.lock().unwrap().clone();
    v.sort();
    v
}

/// All records for `repo`, newest first; each PR is followed by its commits. With
/// `--unmerged`, commits of unmerged remote branches follow.
///
/// `cached` holds GitHub data from an earlier dump, by PR number; only PRs missing from it are
/// queried.
pub fn dump_repo(
    repo: &Repo,
    args: &RepoArgs,
    cached: &HashMap<u64, GhInfo>,
) -> Result<Vec<Record>> {
    let name = repo.name();
    let items = args.select_with_direct(repo)?;
    let groups = items
        .par_iter()
        .map(|(c, pr, is_pr)| one(repo, &name, c, *pr, *is_pr, args.titles))
        .collect::<Result<Vec<_>>>()?;
    let mut records: Vec<Record> = groups.into_iter().flatten().collect();
    if args.unmerged {
        records.extend(unmerged_commits(repo, &name, args)?);
    }
    if args.github {
        // GitHub being unreachable shouldn't cost the git side of the dump.
        // Merged PRs don't change much, so only those without cached data are queried.
        let missing: Vec<&str> = records
            .iter()
            .filter_map(|r| match r {
                Record::Pr(p) if p.pr.is_some_and(|n| !cached.contains_key(&n)) => {
                    Some(p.date.as_str())
                }
                _ => None,
            })
            .collect();
        let fetched = match missing.iter().min() {
            None => Ok(HashMap::new()),
            Some(oldest) => github_info(repo, oldest),
        };
        match fetched {
            Ok(info) => {
                for r in &mut records {
                    if let Record::Pr(p) = r {
                        p.github = p.pr.and_then(|n| info.get(&n).or(cached.get(&n)).cloned());
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "warning: {name}: GitHub query failed, PRs left without new github data: {e:#}"
                );
                GITHUB_FAILED.lock().unwrap().push(name.clone());
                for r in &mut records {
                    if let Record::Pr(p) = r {
                        p.github = p.pr.and_then(|n| cached.get(&n).cloned());
                    }
                }
            }
        }
    }
    Ok(records)
}

/// GitHub's details for the merged PRs of `repo`'s `origin` merged since `oldest` (a PR's date).
fn github_info(repo: &Repo, oldest: &str) -> Result<HashMap<u64, GhInfo>> {
    let url = repo.origin_url().context("no origin remote")?;
    let (host, owner, name) =
        github::parse_remote(&url).with_context(|| format!("{url} is not a GitHub remote"))?;
    // A day of slack covers the date's UTC offset.
    let since = github::iso(repo.since_timestamp(oldest)? - 86_400);
    github::fetch(&host, &owner, &name, Some(&since))
}

/// Commits on remote branches that the walked revision doesn't contain. A commit reachable
/// from several branches is reported once, for the first branch (by name) that has it.
fn unmerged_commits(repo: &Repo, name: &str, args: &RepoArgs) -> Result<Vec<Record>> {
    let Some(since) = args.since_date() else {
        anyhow::bail!("--unmerged needs a time window: pass --since, --months or --days");
    };
    let target = args.rev_for(repo);
    let live = if args.fetch {
        repo.live_remote_branches().ok()
    } else {
        None
    };
    let mut branches = repo.remote_branches_since(&since)?;
    branches.sort();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for b in branches {
        let short = b.strip_prefix("origin/").unwrap_or(&b);
        if b == target || live.as_ref().is_some_and(|l| !l.contains(short)) {
            continue;
        }
        let log = repo.commit_log(
            &format!("{target}..{b}"),
            Some(&since),
            args.until.as_deref(),
            None,
        )?;
        for c in log.iter().filter(|c| seen.insert(c.id.clone())) {
            let Record::Commit(mut rec) =
                commit_rec(name, c, None, None, Some(short), &[], args.titles)
            else {
                unreachable!()
            };
            rec.unmerged = true;
            out.push(Record::Commit(rec));
        }
    }
    Ok(out)
}

fn one(
    repo: &Repo,
    name: &str,
    c: &Commit,
    pr: Option<u64>,
    is_pr: bool,
    titles: bool,
) -> Result<Vec<Record>> {
    let merge = c.parents.len() > 1;
    if !is_pr && !merge {
        return Ok(vec![commit_rec(name, c, None, None, None, &[], titles)]);
    }
    if !is_pr {
        // A sync merge (git pull): the merge itself is noise, the commits it brought in
        // are trunk commits made elsewhere.
        let base = &c.parents[0];
        let members = repo.commit_log(&format!("{base}..{}", c.id), None, None, None)?;
        return Ok(members
            .iter()
            .map(|m| commit_rec(name, m, None, None, None, &[], titles))
            .collect());
    }
    let branch = pr::merge_info(&c.subject).map(|m| m.branch);
    let tickets = pr::tickets(branch.as_deref(), &[&c.subject, &c.body]);
    let pr_rec = |added, removed, commits, squash| {
        Record::Pr(PrRec {
            repo: name.to_string(),
            sha: c.id.clone(),
            pr,
            branch: branch.clone(),
            author: c.author.clone(),
            author_email: Some(c.email.clone()),
            date: c.date.clone(),
            ai_models: credits(c).0,
            ai_tools: credits(c).1,
            added,
            removed,
            commits,
            squash,
            tickets: tickets.clone(),
            title: titles.then(|| pr::title(c)),
            github: None,
        })
    };
    if !merge {
        // Squash / rebase-numbered commit: the commit is the whole PR.
        return Ok(vec![
            pr_rec(c.added, c.removed, 1, true),
            commit_rec(name, c, Some(&c.id), pr, None, &[], titles),
        ]);
    }
    let base = &c.parents[0];
    let members = repo.commit_log(&format!("{base}..{}", c.id), None, None, None)?;
    let (added, removed) = repo.numstat(base, &c.id)?;
    let mut out = vec![pr_rec(added, removed, members.len() as u32, false)];
    out.extend(members.iter().map(|m| {
        commit_rec(
            name,
            m,
            Some(&c.id),
            pr,
            branch.as_deref(),
            &tickets,
            titles,
        )
    }));
    Ok(out)
}

/// Identity key: the lowercased email, or the lowercased name when there is none.
fn identity(name: &str, email: &str) -> String {
    if email.is_empty() { name } else { email }.to_lowercase()
}

const ADJECTIVES: &[&str] = &[
    "amber", "azure", "bold", "brave", "bright", "calm", "clever", "coral", "cosmic", "crisp",
    "dapper", "eager", "fancy", "fuzzy", "gentle", "golden", "happy", "humble", "icy", "ivory",
    "jolly", "keen", "lively", "lucky", "maple", "mellow", "merry", "misty", "mossy", "nimble",
    "noble", "olive", "peppy", "plucky", "polite", "proud", "quick", "quiet", "rapid", "rosy",
    "rustic", "sandy", "shiny", "silent", "silver", "sleek", "snowy", "solar", "spry", "steady",
    "sturdy", "sunny", "swift", "tawny", "teal", "tidy", "velvet", "vivid", "warm", "wild",
    "windy", "witty", "young", "zesty",
];

const ANIMALS: &[&str] = &[
    "badger", "beaver", "bison", "camel", "crane", "eagle", "falcon", "ferret", "finch", "fox",
    "gecko", "hedgehog", "heron", "ibis", "jackal", "koala", "lark", "lemur", "lynx", "marmot",
    "mole", "moose", "newt", "ocelot", "orca", "osprey", "otter", "owl", "panda", "parrot",
    "pelican", "penguin", "pika", "plover", "puffin", "quail", "rabbit", "raven", "robin",
    "salmon", "seal", "shrew", "skink", "snipe", "sparrow", "squid", "stoat", "stork", "swan",
    "tapir", "tiger", "toucan", "trout", "turtle", "walrus", "weasel", "wombat", "wren", "yak",
    "zebra", "bobcat", "cricket", "dolphin", "magpie",
];

/// `n` distinct pseudonyms like `amber-otter`, in an order shuffled by `seed`. Past the number
/// of adjective-animal pairs, names repeat with a `-2`, `-3`, ... suffix.
pub fn pseudonym_names(seed: u64, n: usize) -> Vec<String> {
    let total = ADJECTIVES.len() * ANIMALS.len();
    // Fisher-Yates over all pairs, driven by splitmix64 so it needs no dependency.
    let mut state = seed;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut order: Vec<usize> = (0..total).collect();
    for i in (1..total).rev() {
        order.swap(i, (next() % (i as u64 + 1)) as usize);
    }
    (0..n)
        .map(|i| {
            let p = order[i % total];
            let name = format!(
                "{}-{}",
                ADJECTIVES[p / ANIMALS.len()],
                ANIMALS[p % ANIMALS.len()]
            );
            match i / total {
                0 => name,
                round => format!("{name}-{}", round + 1),
            }
        })
        .collect()
}

/// Replaces every author in `groups` (one group per repository dumped in this run) with a
/// mnemonic pseudonym such as `amber-otter`, handed out in order of first appearance in time
/// (ties by identity), and drops the email. All groups share one assignment, so a person is the
/// same name in every file of the run. Nothing is stored: the names are shuffled by a per-run
/// `seed`, so different runs give different names.
pub fn pseudonymize(groups: &mut [Vec<Record>], seed: u64) {
    fn who(r: &mut Record) -> (&str, &mut String, &mut Option<String>) {
        match r {
            Record::Commit(c) => (&c.date, &mut c.author, &mut c.author_email),
            Record::Pr(p) => (&p.date, &mut p.author, &mut p.author_email),
        }
    }
    let mut first: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for r in groups.iter_mut().flatten() {
        let (date, author, email) = who(r);
        let (date, key) = (
            date.to_string(),
            identity(author, email.as_deref().unwrap_or("")),
        );
        let e = first.entry(key).or_insert_with(|| date.clone());
        if date < *e {
            *e = date;
        }
    }
    let mut order: Vec<(String, String)> = first.into_iter().map(|(k, d)| (d, k)).collect();
    order.sort();
    // GitHub logins (merger, reviewers) get names after the authors', so the two never clash.
    // They are not linked to the authors: a login is not a git identity.
    let mut logins: Vec<(String, String)> = Vec::new();
    for r in groups.iter().flatten() {
        if let Record::Pr(PrRec {
            date,
            github: Some(g),
            ..
        }) = r
        {
            logins.extend(
                g.merged_by
                    .iter()
                    .chain(&g.reviewers)
                    .map(|l| (date.clone(), l.clone())),
            );
        }
    }
    logins.sort();
    logins.dedup_by(|b, a| a.1 == b.1);
    let mut names = pseudonym_names(seed, order.len() + logins.len());
    let login_names: std::collections::HashMap<String, String> = logins
        .into_iter()
        .map(|(_, l)| l)
        .zip(names.split_off(order.len()))
        .collect();
    let label: std::collections::HashMap<String, String> = order
        .into_iter()
        .zip(names)
        .map(|((_, k), name)| (k, name))
        .collect();

    for r in groups.iter_mut().flatten() {
        let (_, author, email) = who(r);
        *author = label[&identity(author, email.as_deref().unwrap_or(""))].clone();
        *email = None;
        if let Record::Pr(PrRec {
            github: Some(g), ..
        }) = r
        {
            g.merged_by = g.merged_by.take().map(|l| login_names[&l].clone());
            g.reviewers = g.reviewers.iter().map(|l| login_names[l].clone()).collect();
            g.reviewers.sort();
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, date: &str, name: &str, email: &str) -> Record {
        Record::Commit(CommitRec {
            repo: "r".into(),
            sha: sha.into(),
            pr_merge: None,
            pr: None,
            branch: None,
            author: name.into(),
            author_email: Some(email.into()),
            date: date.into(),
            ai_models: vec![],
            ai_tools: vec![],
            added: 0,
            removed: 0,
            tickets: vec![],
            subject: None,
            unmerged: false,
        })
    }

    #[test]
    fn pseudonym_names_are_distinct_mnemonic_and_seeded() {
        let n = ADJECTIVES.len() * ANIMALS.len();
        let names = pseudonym_names(7, n + 3);
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            n + 3
        );
        assert!(names[0].chars().all(|c| c.is_ascii_lowercase() || c == '-'));
        assert!(names[n].ends_with("-2"));
        assert_eq!(names, pseudonym_names(7, n + 3));
        assert_ne!(pseudonym_names(7, 5), pseudonym_names(8, 5));
        // no word can look like an AI vendor to the credit detector
        assert!(names.iter().all(|x| !ai::is_ai_identity(x, "")));
    }

    #[test]
    fn pseudonyms_are_shared_across_groups() {
        let mut g = vec![
            vec![commit("1", "2026-02-01", "Bob B", "bob@x.com")],
            vec![
                commit("2", "2026-01-01", "Ann Alias", "ann@x.com"),
                commit("3", "2026-03-01", "Bob B", "BOB@x.com"),
            ],
        ];
        pseudonymize(&mut g, 7);
        let names = pseudonym_names(7, 2);
        let c = |r: &Record| match r {
            Record::Commit(c) => (c.author.clone(), c.author_email.clone()),
            _ => unreachable!(),
        };
        // Ann's earliest commit is first; the same email in any case is the same person
        assert_eq!(c(&g[1][0]), (names[0].clone(), None));
        assert_eq!(c(&g[0][0]), (names[1].clone(), None));
        assert_eq!(c(&g[1][1]).0, names[1]);
    }
}

/// GitHub data already in the dump file at `path`, by PR number; empty when there is none.
pub fn cached_github(path: &Path) -> HashMap<u64, GhInfo> {
    let Ok(file) = fs::File::open(path) else {
        return HashMap::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<Record>(&l).ok())
        .filter_map(|r| match r {
            Record::Pr(PrRec {
                pr: Some(n),
                github: Some(g),
                ..
            }) => Some((n, g)),
            _ => None,
        })
        .collect()
}
