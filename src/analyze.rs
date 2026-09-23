//! Per-PR complexity analysis.

use crate::complexity::{Language, Levels, Stats};
use crate::git::{CatFile, Commit, Repo};
use anyhow::Result;
use serde::Serialize;

pub struct Options {
    pub tab_width: usize,
    pub max_file_bytes: usize,
    /// Only analyze files under these path prefixes (empty = all).
    pub paths: Vec<String>,
    /// Git pathspecs selecting supported languages and exclusions.
    pub pathspecs: Vec<String>,
    pub empty_tree: String,
}

#[derive(Serialize)]
pub struct FileResult {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub language: Language,
    pub status: &'static str,
    /// Whole file before / after the PR.
    pub before: Stats,
    pub after: Stats,
    /// Only the lines added / removed by the PR.
    pub added: Stats,
    pub removed: Stats,
    /// `after.total - before.total`
    pub delta: i64,
}

#[derive(Serialize)]
pub struct PrResult {
    pub commit: String,
    pub pr: Option<u64>,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub before: Stats,
    pub after: Stats,
    pub added: Stats,
    pub removed: Stats,
    pub delta: i64,
    /// Files skipped because they exceeded the size limit.
    pub skipped_files: u32,
    pub files: Vec<FileResult>,
}

pub fn analyze_commit(
    repo: &Repo,
    cat: &mut CatFile,
    commit: &Commit,
    pr: Option<u64>,
    opts: &Options,
) -> Result<PrResult> {
    // For merges the first parent is the target branch, so this is the PR's net change.
    let base = commit.parents.first().unwrap_or(&opts.empty_tree);
    let diffs = repo.diff(base, &commit.id, &opts.pathspecs)?;

    let mut result = PrResult {
        commit: commit.id.clone(),
        pr,
        author: commit.author.clone(),
        date: commit.date.clone(),
        subject: commit.subject.clone(),
        before: Stats::default(),
        after: Stats::default(),
        added: Stats::default(),
        removed: Stats::default(),
        delta: 0,
        skipped_files: 0,
        files: Vec::new(),
    };

    for d in diffs {
        let path = d.new_path.clone().or_else(|| d.old_path.clone()).unwrap();
        let Some(lang) = Language::from_path(&path) else {
            continue;
        };
        if !opts.paths.is_empty() && !opts.paths.iter().any(|p| path.starts_with(p.as_str())) {
            continue;
        }

        let mut load = |rev: &str, p: &Option<String>| -> Result<Option<Option<Levels>>> {
            let Some(p) = p else { return Ok(Some(None)) };
            Ok(cat.read(rev, p, opts.max_file_bytes)?.map(|bytes| {
                Some(Levels::analyze(
                    &String::from_utf8_lossy(&bytes),
                    lang,
                    opts.tab_width,
                ))
            }))
        };
        let (Some(old), Some(new)) = (load(base, &d.old_path)?, load(&commit.id, &d.new_path)?)
        else {
            result.skipped_files += 1;
            continue;
        };

        let stats = |lv: &Option<Levels>, ranges: Option<&[(u32, u32)]>| {
            lv.as_ref()
                .map(|lv| ranges.map_or_else(|| lv.stats(), |r| lv.stats_in(r)))
                .unwrap_or_default()
        };
        let before = stats(&old, None);
        let after = stats(&new, None);
        let file = FileResult {
            status: match (&d.old_path, &d.new_path) {
                (None, _) => "A",
                (_, None) => "D",
                (Some(o), Some(n)) if o != n => "R",
                _ => "M",
            },
            old_path: d.old_path.filter(|o| *o != path),
            path,
            language: lang,
            before,
            after,
            added: stats(&new, Some(&d.added)),
            removed: stats(&old, Some(&d.removed)),
            delta: after.total as i64 - before.total as i64,
        };

        result.before.merge(&file.before);
        result.after.merge(&file.after);
        result.added.merge(&file.added);
        result.removed.merge(&file.removed);
        result.delta += file.delta;
        result.files.push(file);
    }
    Ok(result)
}
