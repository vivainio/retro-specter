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
}
