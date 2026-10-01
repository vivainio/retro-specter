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

/// Words that look like a Jira key (`UTF-8`, `SHA-256`) but are not tickets.
const NOT_PROJECTS: &[&str] = &[
    "UTF", "SHA", "ISO", "RFC", "CVE", "AES", "TLS", "SSL", "MD", "EN", "BR", "PR", "WIP", "FIX",
    "BUG", "FEATURE", "ISSUE", "HOTFIX", "RELEASE", "V",
];

static TICKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^A-Za-z0-9])([A-Za-z][A-Za-z0-9]*)-(\d+)\b").unwrap());

/// Jira-style ticket keys (`FOO-123`) mentioned in `branch` or in `texts` (subject, body, ...),
/// each once, in order of appearance with the branch first. A key in a branch name may be any
/// case (`bt-19325` is `BT-19325`); in text it must be upper case, so `utf-8` and `issue-88`
/// are not tickets. Common non-ticket look-alikes (`UTF-8`, `SHA-256`) are ignored.
pub fn tickets(branch: Option<&str>, texts: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let sources = branch.map(|b| (b, true)).into_iter().chain(texts.iter().map(|t| (*t, false)));
    for (text, is_branch) in sources {
        for c in TICKET.captures_iter(text) {
            let project = &c[1];
            let keyed = project.len() >= 2
                && (is_branch || project.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit()));
            let project = project.to_ascii_uppercase();
            if keyed && !NOT_PROJECTS.contains(&project.as_str()) {
                let key = format!("{project}-{}", &c[2]);
                if !found.contains(&key) {
                    found.push(key);
                }
            }
        }
    }
    found
}

/// The PR's title as far as the merge commit tells it: a GitHub merge commit keeps the title in
/// the first line of its body, Azure DevOps prefixes the subject (`Merged PR 7: Title`), and a
/// squash commit's subject is the title plus a trailing `(#N)`. Anything else is the subject.
pub fn title(commit: &Commit) -> String {
    let subject = commit.subject.trim();
    if subject.starts_with("Merge pull request #") {
        if let Some(line) = commit.body.lines().map(str::trim).find(|l| !l.is_empty()) {
            return line.to_string();
        }
    }
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Merged PR \d+:\s*").unwrap());
    static SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\(#\d+\)\s*$").unwrap());
    let t = PREFIX.replace(subject, "");
    SUFFIX.replace(&t, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str) -> Commit {
        Commit {
            id: String::new(),
            parents: vec![],
            author: String::new(),
            email: String::new(),
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
    fn extracts_titles() {
        let t = |s, b| title(&commit(s, b));
        assert_eq!(t("Merge pull request #4 from a/b", "\nFix the thing\n"), "Fix the thing");
        assert_eq!(t("Merge pull request #4 from a/b", ""), "Merge pull request #4 from a/b");
        assert_eq!(t("Merged PR 12: Add x", ""), "Add x");
        assert_eq!(t("Fix the thing (#77)", "details"), "Fix the thing");
        assert_eq!(t("Plain commit", ""), "Plain commit");
    }

    #[test]
    fn finds_tickets_in_branch_and_text() {
        assert_eq!(tickets(Some("features/AC-2518-efs"), &[]), ["AC-2518"]);
        assert_eq!(tickets(Some("bt-19325"), &[]), ["BT-19325"]);
        assert_eq!(
            tickets(Some("bt-1-x"), &["BT-19391: seller BT-33, BT-1", "see FOO-7"]),
            ["BT-1", "BT-19391", "BT-33", "FOO-7"]
        );
        // text needs upper case; look-alikes and plain issue numbers are not tickets
        assert!(tickets(Some("issue-88-parity"), &["use utf-8, SHA-256 and UTF-8"]).is_empty());
        assert!(tickets(None, &["fix v-2 and x-1"]).is_empty());
        assert_eq!(tickets(None, &["A-1,B-2"]), Vec::<String>::new());
        assert_eq!(tickets(None, &["AB-1,CD-2"]), ["AB-1", "CD-2"]);
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
