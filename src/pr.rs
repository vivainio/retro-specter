//! Recognizing merged pull requests from commit messages.

use crate::git::Commit;
use regex::Regex;
use std::sync::LazyLock;

static SUBJECT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^Merge pull request #(\d+)",     // GitHub merge commit
        r"^Merged PR (\d+)",               // Azure DevOps
        r"\(#(\d+)\)\s*$",                 // GitHub squash / rebase
        r"^Merge branch .* into .*!(\d+)", // GitLab (some templates)
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

static BODY_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"See merge request \S*!(\d+)").unwrap()); // GitLab

/// What a merge commit's subject says about the branch it merged.
#[derive(Debug, PartialEq)]
pub struct MergeInfo {
    pub branch: String,
    /// A `git pull` style merge of the branch's own upstream (`... of <url>`), not a unit of work.
    pub pull: bool,
}

static MERGE_PATTERNS: LazyLock<Vec<(Regex, bool)>> = LazyLock::new(|| {
    [
        // GitHub: Merge pull request #12 from owner/feature-x
        (r"^Merge pull request #\d+ from (?:[^/\s]+/)?(\S+)", false),
        // git pull: Merge branch 'master' of https://host/repo
        (r"^Merge branch '([^']+)' of \S+", true),
        // Merge remote-tracking branch 'origin/feature-x'
        (
            r"^Merge remote-tracking branch '(?:[^/']+/)?([^']+)'",
            false,
        ),
        // Merge branch 'feature-x' [into 'main']
        (r"^Merge branch '([^']+)'", false),
        // Merge feature-x into main   (unquoted ref, or "Merge feature-x: custom text")
        (r"^Merge (?:tag )?'?([\w./-]+)'?(?: into \S+|:|$)", false),
    ]
    .iter()
    .map(|(p, pull)| (Regex::new(p).unwrap(), *pull))
    .collect()
});

/// The branch a merge commit's subject says it merged, for merges without a PR number.
pub fn merge_info(subject: &str) -> Option<MergeInfo> {
    MERGE_PATTERNS.iter().find_map(|(re, pull)| {
        re.captures(subject).map(|c| MergeInfo {
            branch: c[1].to_string(),
            pull: *pull,
        })
    })
}

/// The PR / merge request number referenced by a commit message, if any.
pub fn pr_number(commit: &Commit) -> Option<u64> {
    SUBJECT_PATTERNS
        .iter()
        .find_map(|re| re.captures(&commit.subject))
        .or_else(|| BODY_PATTERN.captures(&commit.body))
        .and_then(|c| c[1].parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str) -> Commit {
        Commit {
            id: String::new(),
            parents: vec![],
            author: String::new(),
            date: String::new(),
            subject: subject.into(),
            body: body.into(),
            added: 0,
            removed: 0,
        }
    }

    #[test]
    fn detects_common_formats() {
        assert_eq!(
            pr_number(&commit("Merge pull request #42 from a/b", "")),
            Some(42)
        );
        assert_eq!(
            pr_number(&commit("Merged PR 1234: Fix thing", "")),
            Some(1234)
        );
        assert_eq!(pr_number(&commit("Fix thing (#77)", "")), Some(77));
        assert_eq!(
            pr_number(&commit(
                "Merge branch 'x' into 'main'",
                "See merge request grp/proj!9"
            )),
            Some(9)
        );
        assert_eq!(pr_number(&commit("Refs #12 in the middle", "")), None);
    }

    #[test]
    fn reads_merged_branch_names() {
        let b = |s| merge_info(s).map(|m| (m.branch, m.pull));
        assert_eq!(
            b("Merge pull request #4 from a/feat/x"),
            Some(("feat/x".into(), false))
        );
        assert_eq!(
            b("Merge branch 'master' of https://h/r"),
            Some(("master".into(), true))
        );
        assert_eq!(
            b("Merge branch 'wip' into main"),
            Some(("wip".into(), false))
        );
        assert_eq!(
            b("Merge remote-tracking branch 'origin/dev'"),
            Some(("dev".into(), false))
        );
        assert_eq!(
            b("Merge worktree/calm-1 into main"),
            Some(("worktree/calm-1".into(), false))
        );
        assert_eq!(
            b("Merge worktree/silver-2: rename x"),
            Some(("worktree/silver-2".into(), false))
        );
        assert_eq!(
            b("Merge pull request #4 from a/feat/x").map(|x| x.0),
            Some("feat/x".into())
        );
        assert_eq!(b("Merge changes from upstream"), None);
        assert_eq!(b("Fix (#3)"), None);
    }
}
