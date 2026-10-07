//! Pull request details from the GitHub GraphQL API, through the `gh` CLI (so it reuses the
//! user's authentication). One query returns up to 100 PRs with their reviews, which is far
//! cheaper than per-PR REST calls.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::{Command, Stdio};

const QUERY: &str = "
query($owner: String!, $name: String!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: MERGED, first: 100, after: $after,
                 orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number createdAt mergedAt updatedAt isDraft baseRefName
        additions deletions changedFiles
        author { login }
        mergedBy { login }
        comments { totalCount }
        labels(first: 10) { nodes { name } }
        reviews(first: 20) { nodes { author { login } state submittedAt } }
      }
    }
  }
}";

/// What GitHub knows about a merged PR that git does not. Review figures leave out the PR
/// author's own reviews (replies to comments) and only look at the first 20 reviews.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PrInfo {
    pub created_at: String,
    pub merged_at: String,
    pub is_draft: bool,
    pub base_ref: String,
    /// GitHub's own line and file counts for the PR.
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// Conversation comments (not review comments).
    pub comments: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default)]
    pub merged_by: Option<String>,
    pub review_count: u32,
    pub approvals: u32,
    pub changes_requested: u32,
    #[serde(default)]
    pub first_review_at: Option<String>,
    #[serde(default)]
    pub approved_at: Option<String>,
    /// Distinct reviewers, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
}

/// `(host, owner, name)` of a GitHub remote URL (`git@host:o/n.git`, `https://host/o/n`,
/// `ssh://git@host/o/n.git`).
pub fn parse_remote(url: &str) -> Option<(String, String, String)> {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = match url.split_once("://") {
        Some((_, r)) => r.to_string(),
        None => url.replacen(':', "/", 1),
    };
    let rest = rest.rsplit_once('@').map_or(rest.as_str(), |(_, r)| r);
    let mut parts = rest.split('/');
    let host = parts.next()?.split(':').next()?.to_string();
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || name.is_empty() || !host.contains("github") {
        return None;
    }
    // A host without a dot is an SSH config alias; the API lives at github.com.
    let host = if host.contains('.') { host } else { "github.com".into() };
    Some((host, owner.to_string(), name.to_string()))
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time (UTC).
pub fn iso(ts: i64) -> String {
    let (days, secs) = (ts.div_euclid(86_400), ts.rem_euclid(86_400));
    // days -> civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn login(v: &Value) -> Option<String> {
    v["login"].as_str().map(String::from)
}

fn info(n: &Value) -> Option<(u64, PrInfo)> {
    let author = login(&n["author"]);
    let mut reviewers = Vec::new();
    let mut p = PrInfo {
        created_at: n["createdAt"].as_str()?.to_string(),
        merged_at: n["mergedAt"].as_str()?.to_string(),
        is_draft: n["isDraft"].as_bool().unwrap_or(false),
        base_ref: n["baseRefName"].as_str().unwrap_or_default().to_string(),
        additions: n["additions"].as_u64().unwrap_or(0),
        deletions: n["deletions"].as_u64().unwrap_or(0),
        changed_files: n["changedFiles"].as_u64().unwrap_or(0),
        comments: n["comments"]["totalCount"].as_u64().unwrap_or(0),
        labels: n["labels"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(String::from))
            .collect(),
        merged_by: login(&n["mergedBy"]),
        ..PrInfo::default()
    };
    let earliest = |slot: &mut Option<String>, t: &str| {
        if slot.as_deref().is_none_or(|s| t < s) {
            *slot = Some(t.to_string());
        }
    };
    for r in n["reviews"]["nodes"].as_array().into_iter().flatten() {
        let who = login(&r["author"]);
        // Pending reviews have no submission time; the author's own are replies.
        let Some(at) = r["submittedAt"].as_str() else {
            continue;
        };
        if who.is_some() && who == author {
            continue;
        }
        p.review_count += 1;
        earliest(&mut p.first_review_at, at);
        match r["state"].as_str() {
            Some("APPROVED") => {
                p.approvals += 1;
                earliest(&mut p.approved_at, at);
            }
            Some("CHANGES_REQUESTED") => p.changes_requested += 1,
            _ => {}
        }
        reviewers.extend(who);
    }
    reviewers.sort();
    reviewers.dedup();
    p.reviewers = reviewers;
    Some((n["number"].as_u64()?, p))
}

/// Merged PRs of `owner/name` updated since `since` (an ISO time; all of them when `None`),
/// by number. A merged PR is last updated at or after its merge, so every PR merged in the
/// window is found.
pub fn fetch(
    host: &str,
    owner: &str,
    name: &str,
    since: Option<&str>,
) -> Result<HashMap<u64, PrInfo>> {
    let mut out = HashMap::new();
    let mut after: Option<String> = None;
    loop {
        let mut cmd = Command::new("gh");
        cmd.args(["api", "graphql", "--hostname", host])
            .args(["-f", &format!("query={QUERY}")])
            .args(["-F", &format!("owner={owner}"), "-F", &format!("name={name}")]);
        if let Some(a) = &after {
            cmd.args(["-F", &format!("after={a}")]);
        }
        let res = cmd
            .stdin(Stdio::null())
            .output()
            .context("failed to run gh (is the GitHub CLI installed?)")?;
        if !res.status.success() {
            bail!(
                "gh api failed for {owner}/{name}: {}",
                String::from_utf8_lossy(&res.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&res.stdout).context("unreadable gh output")?;
        let prs = &v["data"]["repository"]["pullRequests"];
        let Some(nodes) = prs["nodes"].as_array() else {
            bail!("no pull requests in the response for {owner}/{name}: {v}");
        };
        out.extend(nodes.iter().filter_map(info));
        let stale = nodes
            .last()
            .and_then(|n| n["updatedAt"].as_str())
            .zip(since)
            .is_some_and(|(u, s)| u < s);
        match prs["pageInfo"]["endCursor"].as_str() {
            Some(c) if prs["pageInfo"]["hasNextPage"].as_bool() == Some(true) && !stale => {
                after = Some(c.to_string())
            }
            _ => return Ok(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remotes() {
        let want = Some(("github.com".into(), "o".into(), "n".into()));
        for u in [
            "git@github.com:o/n.git",
            "https://github.com/o/n",
            "https://user@github.com/o/n.git/",
            "ssh://git@github.com:22/o/n.git",
        ] {
            assert_eq!(parse_remote(u), want, "{u}");
        }
        assert_eq!(parse_remote("git@github-public:o/n.git"), want);
        assert_eq!(parse_remote("https://dev.azure.com/o/p/_git/n"), None);
        assert_eq!(parse_remote("/some/path"), None);
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn reviews_ignore_the_author_and_pending() {
        let n = json!({
            "number": 7, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": "2026-01-03T00:00:00Z",
            "isDraft": false, "baseRefName": "main", "additions": 5, "deletions": 1,
            "changedFiles": 2, "author": {"login": "a"}, "mergedBy": {"login": "m"},
            "comments": {"totalCount": 3}, "labels": {"nodes": [{"name": "bug"}]},
            "reviews": {"nodes": [
                {"author": {"login": "a"}, "state": "COMMENTED", "submittedAt": "2026-01-01T01:00:00Z"},
                {"author": {"login": "r2"}, "state": "CHANGES_REQUESTED", "submittedAt": "2026-01-02T00:00:00Z"},
                {"author": {"login": "r1"}, "state": "APPROVED", "submittedAt": "2026-01-02T12:00:00Z"},
                {"author": {"login": "r2"}, "state": "APPROVED", "submittedAt": "2026-01-03T00:00:00Z"},
                {"author": {"login": "r3"}, "state": "PENDING", "submittedAt": null},
            ]},
        });
        let (num, p) = info(&n).unwrap();
        assert_eq!(num, 7);
        assert_eq!((p.review_count, p.approvals, p.changes_requested), (3, 2, 1));
        assert_eq!(p.first_review_at.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(p.approved_at.as_deref(), Some("2026-01-02T12:00:00Z"));
        assert_eq!(p.reviewers, ["r1", "r2"]);
        assert_eq!((p.merged_by.as_deref(), p.labels.as_slice()), (Some("m"), &["bug".to_string()][..]));
    }
}
