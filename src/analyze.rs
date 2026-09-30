//! Per-PR complexity analysis.

use crate::complexity::{self, Language, Levels, Stats};
use crate::git::{CatFile, Commit, Repo};
use crate::structure::{self, FnChange, Structure};
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

#[derive(Clone, Copy, Default, Serialize)]
pub struct Counts {
    pub functions: u32,
    pub classes: u32,
}

impl Counts {
    fn of(s: &Structure) -> Counts {
        Counts {
            functions: s.functions.len() as u32,
            classes: s.classes.len() as u32,
        }
    }

    fn merge(&mut self, o: &Counts) {
        self.functions += o.functions;
        self.classes += o.classes;
    }
}

/// How many functions / classes a PR added, removed or modified.
#[derive(Clone, Copy, Default, Serialize)]
pub struct Changes {
    pub functions_added: u32,
    pub functions_removed: u32,
    pub functions_modified: u32,
    pub classes_added: u32,
    pub classes_removed: u32,
}

impl Changes {
    pub fn merge(&mut self, o: &Changes) {
        self.functions_added += o.functions_added;
        self.functions_removed += o.functions_removed;
        self.functions_modified += o.functions_modified;
        self.classes_added += o.classes_added;
        self.classes_removed += o.classes_removed;
    }
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
    /// Statements added / removed by the PR, ignoring pure reformatting.
    pub added: Stats,
    pub removed: Stats,
    /// `after.total - before.total`
    pub delta: i64,
    pub counts_before: Counts,
    pub counts_after: Counts,
    pub changes: Changes,
    /// Functions touched by the PR, with body complexity before / after.
    pub functions: Vec<FnChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub classes_added: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub classes_removed: Vec<String>,
}

#[derive(Serialize)]
pub struct PrResult {
    pub repo: String,
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
    pub counts_before: Counts,
    pub counts_after: Counts,
    pub changes: Changes,
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
        repo: repo.name(),
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
        counts_before: Counts::default(),
        counts_after: Counts::default(),
        changes: Changes::default(),
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

        let before = old.as_ref().map(Levels::stats).unwrap_or_default();
        let after = new.as_ref().map(Levels::stats).unwrap_or_default();
        let old_s = old.as_ref().map(Levels::structure).unwrap_or_default();
        let new_s = new.as_ref().map(Levels::structure).unwrap_or_default();
        // Only statements that changed beyond whitespace count as touched.
        let churn = complexity::churn(old.as_ref(), &d.removed, new.as_ref(), &d.added);
        let functions = structure::diff_functions(
            &old_s.functions,
            &new_s.functions,
            &churn.removed_ranges,
            &churn.added_ranges,
        );
        let classes_added = structure::names_missing_from(&new_s.classes, &old_s.classes);
        let classes_removed = structure::names_missing_from(&old_s.classes, &new_s.classes);
        let count = |kind| functions.iter().filter(|f| f.change == kind).count() as u32;
        let changes = Changes {
            functions_added: count("added"),
            functions_removed: count("removed"),
            functions_modified: count("modified"),
            classes_added: classes_added.len() as u32,
            classes_removed: classes_removed.len() as u32,
        };
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
            added: churn.added,
            removed: churn.removed,
            delta: after.total as i64 - before.total as i64,
            counts_before: Counts::of(&old_s),
            counts_after: Counts::of(&new_s),
            changes,
            functions,
            classes_added,
            classes_removed,
        };

        result.before.merge(&file.before);
        result.after.merge(&file.after);
        result.added.merge(&file.added);
        result.removed.merge(&file.removed);
        result.delta += file.delta;
        result.counts_before.merge(&file.counts_before);
        result.counts_after.merge(&file.counts_after);
        result.changes.merge(&file.changes);
        result.files.push(file);
    }
    Ok(result)
}
