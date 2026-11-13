//! Pull request details from the GitHub GraphQL API, through the `gh` CLI (so it reuses the
//! user's authentication). One query returns up to 100 PRs (fewer when GitHub answers 502 to a
//! big page) with their reviews, which is far cheaper than per-PR REST calls.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::io::Read;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One `gh` query at a time: repositories are dumped in parallel, and a burst of heavy queries
/// is what trips GitHub's secondary rate limit.
static GH_QUERY: Mutex<()> = Mutex::new(());

const QUERY: &str = "
query($owner: String!, $name: String!, $after: String, $first: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: MERGED, first: $first, after: $after,
                 orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number createdAt mergedAt updatedAt isDraft baseRefName
        additions deletions changedFiles
        author { login __typename }
        mergedBy { login __typename }
        comments(first: 30) {
          totalCount
          nodes { author { login __typename } createdAt }
        }
        labels(first: 10) { nodes { name } }
        reviews(first: 50) {
          nodes { author { login __typename } state submittedAt comments { totalCount } }
        }
        commits(last: 100) {
          totalCount
          nodes { commit { authoredDate committedDate additions deletions } }
        }
        timelineItems(
          itemTypes: [READY_FOR_REVIEW_EVENT, REVIEW_REQUESTED_EVENT, HEAD_REF_FORCE_PUSHED_EVENT]
          first: 100
        ) {
          nodes {
            __typename
            ... on ReadyForReviewEvent { createdAt }
            ... on ReviewRequestedEvent { createdAt }
            ... on HeadRefForcePushedEvent { createdAt }
          }
        }
      }
    }
  }
}";

/// One thing that happened on a PR, for drawing its timeline. `kind` is `opened`, `ready`,
/// `review_requested`, `review`, `comment`, `commit`, `force_push` or `merged`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub at: String,
    pub kind: String,
    /// `bot` or `human`, for reviews and comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The login (pseudonymized for people, as is for bots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub who: Option<String>,
    /// A review's state (`APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// A review's inline comments, or a commit's changed lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// The PR author's own review or comment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub author: bool,
}

/// What GitHub knows about a merged PR that git does not. Review figures leave out the PR
/// author's own reviews (replies to comments) and only look at the first 50 reviews. `review_count`,
/// `approvals`, `changes_requested`, `first_review_at` and `approved_at` count every reviewer,
/// bots included; the `human_*` fields and `reviewers` count people only, and `bot_reviewers`
/// names the bots (an account GitHub types as `Bot`, such as an automated code reviewer).
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
    /// The PR was opened by a bot (dependabot, an automation).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub author_bot: bool,
    /// The merger is a bot (a merge queue, an automation): its name is kept as is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merged_by_bot: bool,
    pub review_count: u32,
    pub approvals: u32,
    pub changes_requested: u32,
    #[serde(default)]
    pub first_review_at: Option<String>,
    #[serde(default)]
    pub approved_at: Option<String>,
    /// Distinct human reviewers, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
    /// Distinct bot reviewers, sorted. Real names: a bot is a tool, not a person.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bot_reviewers: Vec<String>,
    /// What the bots did, for those who want to count them separately: reviews, the first one's
    /// time, approvals, changes requested and inline comments, summed over every bot.
    #[serde(default)]
    pub bot_review_count: u32,
    #[serde(default)]
    pub first_bot_review_at: Option<String>,
    #[serde(default)]
    pub bot_approvals: u32,
    #[serde(default)]
    pub bot_changes_requested: u32,
    #[serde(default)]
    pub bot_review_comments: u32,
    /// Reviews by people (the author's own and pending ones excluded).
    #[serde(default)]
    pub human_review_count: u32,
    #[serde(default)]
    pub human_approvals: u32,
    #[serde(default)]
    pub human_changes_requested: u32,
    #[serde(default)]
    pub first_human_review_at: Option<String>,
    #[serde(default)]
    pub human_approved_at: Option<String>,
    /// Inline review comments left by people, summed over their reviews.
    #[serde(default)]
    pub review_comments: u32,
    /// When the PR was last marked ready for review (it was opened as a draft); absent otherwise.
    #[serde(default)]
    pub ready_at: Option<String>,
    /// When a review was first requested from someone.
    #[serde(default)]
    pub first_review_requested_at: Option<String>,
    /// Commits in the PR, and the newest commit's committed time (a rebase moves it).
    #[serde(default)]
    pub commit_count: u32,
    #[serde(default)]
    pub last_commit_at: Option<String>,
    /// Rework: commits authored after the first human review, and the lines they change.
    /// Only set when a person reviewed the PR. Only the last 100 commits are read.
    #[serde(default)]
    pub commits_after_review: Option<u32>,
    #[serde(default)]
    pub lines_after_review: Option<u64>,
    /// Everything that happened on the PR, oldest first (at most 400 events). Commit times are
    /// committed dates, so a rebase moves them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<Event>,
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
    let host = if host.contains('.') {
        host
    } else {
        "github.com".into()
    };
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
        author_bot: n["author"]["__typename"].as_str() == Some("Bot"),
        merged_by_bot: n["mergedBy"]["__typename"].as_str() == Some("Bot"),
        ..PrInfo::default()
    };
    let earliest = |slot: &mut Option<String>, t: &str| {
        if slot.as_deref().is_none_or(|s| t < s) {
            *slot = Some(t.to_string());
        }
    };
    let mut bots = Vec::new();
    let mut events = vec![
        Event {
            at: p.created_at.clone(),
            kind: "opened".into(),
            ..Event::default()
        },
        Event {
            at: p.merged_at.clone(),
            kind: "merged".into(),
            ..Event::default()
        },
    ];
    let actor = |v: &Value| -> (Option<String>, Option<String>, bool) {
        let who = login(v);
        let by = if v["__typename"].as_str() == Some("Bot") {
            "bot"
        } else {
            "human"
        };
        let own = who.is_some() && who == author;
        (Some(by.to_string()), who, own)
    };
    for c in n["comments"]["nodes"].as_array().into_iter().flatten() {
        if let Some(at) = c["createdAt"].as_str() {
            let (by, who, own) = actor(&c["author"]);
            events.push(Event {
                at: at.into(),
                kind: "comment".into(),
                by,
                who,
                author: own,
                ..Event::default()
            });
        }
    }
    for r in n["reviews"]["nodes"].as_array().into_iter().flatten() {
        let who = login(&r["author"]);
        // Pending reviews have no submission time; the author's own are replies.
        let Some(at) = r["submittedAt"].as_str() else {
            continue;
        };
        let (by, who_, own) = actor(&r["author"]);
        events.push(Event {
            at: at.into(),
            kind: "review".into(),
            by,
            who: who_,
            state: r["state"].as_str().map(String::from),
            n: r["comments"]["totalCount"].as_u64().map(|c| c as u32),
            author: own,
        });
        if who.is_some() && who == author {
            continue;
        }
        let bot = r["author"]["__typename"].as_str() == Some("Bot");
        p.review_count += 1;
        earliest(&mut p.first_review_at, at);
        let state = r["state"].as_str();
        match state {
            Some("APPROVED") => {
                p.approvals += 1;
                earliest(&mut p.approved_at, at);
            }
            Some("CHANGES_REQUESTED") => p.changes_requested += 1,
            _ => {}
        }
        if bot {
            bots.extend(who);
            p.bot_review_count += 1;
            earliest(&mut p.first_bot_review_at, at);
            match state {
                Some("APPROVED") => p.bot_approvals += 1,
                Some("CHANGES_REQUESTED") => p.bot_changes_requested += 1,
                _ => {}
            }
            p.bot_review_comments += r["comments"]["totalCount"].as_u64().unwrap_or(0) as u32;
            continue;
        }
        p.human_review_count += 1;
        earliest(&mut p.first_human_review_at, at);
        match state {
            Some("APPROVED") => {
                p.human_approvals += 1;
                earliest(&mut p.human_approved_at, at);
            }
            Some("CHANGES_REQUESTED") => p.human_changes_requested += 1,
            _ => {}
        }
        p.review_comments += r["comments"]["totalCount"].as_u64().unwrap_or(0) as u32;
        reviewers.extend(who);
    }
    for sorted in [&mut reviewers, &mut bots] {
        sorted.sort();
        sorted.dedup();
    }
    p.reviewers = reviewers;
    p.bot_reviewers = bots;

    for e in n["timelineItems"]["nodes"].as_array().into_iter().flatten() {
        let Some(at) = e["createdAt"].as_str() else {
            continue;
        };
        let kind = match e["__typename"].as_str() {
            Some("ReadyForReviewEvent") => {
                if p.ready_at.as_deref().is_none_or(|s| at > s) {
                    p.ready_at = Some(at.to_string());
                }
                "ready"
            }
            Some("ReviewRequestedEvent") => {
                earliest(&mut p.first_review_requested_at, at);
                "review_requested"
            }
            Some("HeadRefForcePushedEvent") => "force_push",
            _ => continue,
        };
        events.push(Event {
            at: at.into(),
            kind: kind.into(),
            ..Event::default()
        });
    }

    p.commit_count = n["commits"]["totalCount"].as_u64().unwrap_or(0) as u32;
    let commits: Vec<&Value> = n["commits"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| &c["commit"])
        .collect();
    p.last_commit_at = commits
        .iter()
        .filter_map(|c| c["committedDate"].as_str())
        .max()
        .map(String::from);
    for c in &commits {
        if let Some(at) = c["committedDate"].as_str() {
            events.push(Event {
                at: at.into(),
                kind: "commit".into(),
                n: Some(
                    (c["additions"].as_u64().unwrap_or(0) + c["deletions"].as_u64().unwrap_or(0))
                        as u32,
                ),
                ..Event::default()
            });
        }
    }
    events.sort_by(|a, b| a.at.cmp(&b.at));
    events.truncate(400);
    p.timeline = events;
    if let Some(first) = p.first_human_review_at.as_deref() {
        let after: Vec<&&Value> = commits
            .iter()
            .filter(|c| c["authoredDate"].as_str().is_some_and(|d| d > first))
            .collect();
        p.commits_after_review = Some(after.len() as u32);
        p.lines_after_review = Some(
            after
                .iter()
                .map(|c| {
                    c["additions"].as_u64().unwrap_or(0) + c["deletions"].as_u64().unwrap_or(0)
                })
                .sum(),
        );
    }
    Some((n["number"].as_u64()?, p))
}

/// PRs asked for per query, and the smallest page worth trying. A repository whose PRs carry many
/// reviews can make a 100-PR response too large for GitHub, which then answers 502 every time.
const PAGE: u32 = 100;
const MIN_PAGE: u32 = 6;

/// Page size the next repository starts with. A repository that had to shrink its pages pulls this
/// down (never below `MIN_START`), so the rest of the run does not spend two slow failures per
/// repository finding out again that 100 PRs is too many for this organisation's PRs.
static START_PAGE: AtomicU32 = AtomicU32::new(PAGE);
const MIN_START: u32 = 25;

/// A `gh` call that takes longer is killed and counts as a transient failure: queries run one at
/// a time, so one hung call would otherwise stall every repository.
const GH_TIMEOUT: Duration = Duration::from_secs(180);

/// A repository still paging after this long gives up, so its GitHub data is left out and the
/// dump goes on (it is reported as incomplete) instead of one huge repository holding the run.
const REPO_BUDGET: Duration = Duration::from_secs(20 * 60);

/// Runs `cmd` to the end, or kills it after `timeout`. A killed command fails with `timed out` in
/// its stderr, which `is_transient` recognises.
fn output_within(mut cmd: Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Read both pipes while waiting, or a large response fills the pipe and the child blocks.
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap_or_default();
    let mut stderr = err.join().unwrap_or_default();
    if timed_out {
        stderr.extend_from_slice(
            format!("\ntimed out after {}s", timeout.as_secs()).as_bytes(),
        );
    }
    Ok(std::process::Output { status, stdout, stderr })
}

/// Page size to retry with after a failed query, or `None` when a smaller page won't help
/// (the error is not transient, or the page is already as small as it gets).
fn smaller_page(page: u32, err: &str) -> Option<u32> {
    (page > MIN_PAGE && is_transient(err) && !is_rate_limited(err))
        .then(|| (page / 2).max(MIN_PAGE))
}

/// GitHub's primary or secondary rate limit: a smaller page doesn't help, waiting does.
fn is_rate_limited(err: &str) -> bool {
    err.to_lowercase().contains("rate limit")
}

/// Runs `gh` with `args`, retrying up to `attempts` times with a growing pause when GitHub answers
/// with a transient server error (502/503/504) or the connection fails.
fn run_gh(args: &[String], owner: &str, name: &str, attempts: u32) -> Result<std::process::Output> {
    let mut attempt = 1;
    loop {
        let res = {
            let _one_at_a_time = GH_QUERY.lock().unwrap_or_else(|e| e.into_inner());
            let mut cmd = Command::new("gh");
            cmd.args(args).stdin(Stdio::null());
            output_within(cmd, GH_TIMEOUT)
                .context("failed to run gh (is the GitHub CLI installed?)")?
        };
        if res.status.success() {
            return Ok(res);
        }
        let err = String::from_utf8_lossy(&res.stderr).trim().to_string();
        if is_rate_limited(&err) && attempt < RATE_LIMIT_ATTEMPTS {
            // GitHub asks for a wait of minutes; a growing pause, with the other queries held back
            eprintln!(
                "{owner}/{name}: GitHub rate limit, waiting {}s",
                60 * attempt
            );
            std::thread::sleep(std::time::Duration::from_secs(60 * u64::from(attempt)));
            attempt += 1;
            continue;
        }
        if attempt < attempts && is_transient(&err) && !is_rate_limited(&err) {
            std::thread::sleep(std::time::Duration::from_secs(2u64 << attempt));
            attempt += 1;
            continue;
        }
        bail!("gh api failed for {owner}/{name}: {err}");
    }
}

const RATE_LIMIT_ATTEMPTS: u32 = 6;

fn is_transient(err: &str) -> bool {
    [
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connection reset",
        "EOF",
        // gh's message for a response cut off mid-body
        "unexpected end of JSON",
    ]
    .iter()
    .any(|m| err.contains(m))
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
    let mut page = START_PAGE.load(Ordering::Relaxed);
    let started = Instant::now();
    loop {
        if started.elapsed() > REPO_BUDGET {
            bail!(
                "gave up on {owner}/{name} after {} min of paging ({} PRs read so far)",
                REPO_BUDGET.as_secs() / 60,
                out.len()
            );
        }
        let mut args = vec![
            "api".to_string(),
            "graphql".into(),
            "--hostname".into(),
            host.into(),
            "-f".into(),
            format!("query={QUERY}"),
            "-F".into(),
            format!("owner={owner}"),
            "-F".into(),
            format!("name={name}"),
            "-F".into(),
            format!("first={page}"),
        ];
        if let Some(a) = &after {
            args.extend(["-F".into(), format!("after={a}")]);
        }
        // Fewer retries per size while there is a smaller size to fall back to; the cursor stays
        // valid at any page size, so the same page is simply asked for in smaller pieces.
        let attempts = if page > MIN_PAGE { 2 } else { 4 };
        let res = match run_gh(&args, owner, name, attempts) {
            Ok(res) => res,
            Err(e) => match smaller_page(page, &e.to_string()) {
                Some(smaller) => {
                    eprintln!(
                        "{owner}/{name}: GitHub failed at {page} PRs per query, trying {smaller}"
                    );
                    page = smaller;
                    START_PAGE.fetch_min(smaller.max(MIN_START), Ordering::Relaxed);
                    continue;
                }
                None => return Err(e),
            },
        };
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

    #[cfg(unix)]
    #[test]
    fn a_hung_command_is_killed_and_reads_as_transient() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let t = Instant::now();
        let res = output_within(cmd, Duration::from_millis(200)).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(!res.status.success());
        assert!(is_transient(&String::from_utf8_lossy(&res.stderr)));
    }

    #[cfg(unix)]
    #[test]
    fn a_large_output_does_not_block_the_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 3000000 /dev/zero"]);
        let res = output_within(cmd, Duration::from_secs(20)).unwrap();
        assert!(res.status.success());
        assert_eq!(res.stdout.len(), 3_000_000);
    }

    #[test]
    fn rate_limits_wait_instead_of_shrinking() {
        let e = "gh: You have exceeded a secondary rate limit. Please wait a few minutes";
        assert!(is_rate_limited(e));
        assert_eq!(smaller_page(100, e), None);
        assert!(!is_rate_limited("gh: HTTP 502"));
    }

    #[test]
    fn page_shrinks_on_transient_errors_only() {
        assert_eq!(smaller_page(100, "gh: HTTP 502"), Some(50));
        assert_eq!(smaller_page(50, "unexpected EOF"), Some(25));
        assert_eq!(smaller_page(100, "unexpected end of JSON input"), Some(50));
        assert_eq!(smaller_page(MIN_PAGE + 1, "HTTP 504"), Some(MIN_PAGE));
        assert_eq!(smaller_page(MIN_PAGE, "HTTP 502"), None);
        assert_eq!(smaller_page(100, "gh: HTTP 401 bad credentials"), None);
    }

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
    fn bots_and_rework_are_split_out() {
        let n = json!({
            "number": 9, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
            "author": {"login": "a"}, "mergedBy": {"login": "m"},
            "reviews": {"nodes": [
                {"author": {"login": "copilot", "__typename": "Bot"}, "state": "COMMENTED",
                 "submittedAt": "2026-01-01T00:05:00Z", "comments": {"totalCount": 4}},
                {"author": {"login": "r1", "__typename": "User"}, "state": "CHANGES_REQUESTED",
                 "submittedAt": "2026-01-02T00:00:00Z", "comments": {"totalCount": 2}},
                {"author": {"login": "r1", "__typename": "User"}, "state": "APPROVED",
                 "submittedAt": "2026-01-04T00:00:00Z", "comments": {"totalCount": 0}},
            ]},
            "commits": {"totalCount": 3, "nodes": [
                {"commit": {"authoredDate": "2026-01-01T00:00:00Z", "committedDate": "2026-01-01T00:00:00Z", "additions": 50, "deletions": 0}},
                {"commit": {"authoredDate": "2026-01-03T00:00:00Z", "committedDate": "2026-01-03T00:00:00Z", "additions": 7, "deletions": 3}},
                {"commit": {"authoredDate": "2026-01-01T00:00:00Z", "committedDate": "2026-01-04T00:00:00Z", "additions": 1, "deletions": 1}},
            ]},
            "timelineItems": {"nodes": [
                {"__typename": "ReadyForReviewEvent", "createdAt": "2026-01-01T00:03:00Z"},
                {"__typename": "ReviewRequestedEvent", "createdAt": "2026-01-01T00:04:00Z"},
                {"__typename": "ReviewRequestedEvent", "createdAt": "2026-01-02T00:00:00Z"},
            ]},
        });
        let (_, p) = info(&n).unwrap();
        // every reviewer
        assert_eq!(
            (p.review_count, p.first_review_at.as_deref()),
            (3, Some("2026-01-01T00:05:00Z"))
        );
        // people only
        assert_eq!(p.reviewers, ["r1"]);
        assert_eq!(p.bot_reviewers, ["copilot"]);
        assert_eq!(
            (p.bot_review_count, p.bot_review_comments, p.bot_approvals),
            (1, 4, 0)
        );
        assert_eq!(
            p.first_bot_review_at.as_deref(),
            Some("2026-01-01T00:05:00Z")
        );
        assert_eq!(
            (
                p.human_review_count,
                p.human_approvals,
                p.human_changes_requested
            ),
            (2, 1, 1)
        );
        assert_eq!(
            p.first_human_review_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        assert_eq!(p.human_approved_at.as_deref(), Some("2026-01-04T00:00:00Z"));
        assert_eq!(p.review_comments, 2);
        assert_eq!(p.ready_at.as_deref(), Some("2026-01-01T00:03:00Z"));
        assert_eq!(
            p.first_review_requested_at.as_deref(),
            Some("2026-01-01T00:04:00Z")
        );
        // one commit authored after the first human review (the rebased one keeps its old date)
        assert_eq!(
            (p.commit_count, p.commits_after_review, p.lines_after_review),
            (3, Some(1), Some(10))
        );
        assert_eq!(p.last_commit_at.as_deref(), Some("2026-01-04T00:00:00Z"));
    }

    #[test]
    fn timeline_orders_events_and_marks_actors() {
        let n = json!({
            "number": 9, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
            "author": {"login": "a", "__typename": "User"},
            "comments": {"totalCount": 2, "nodes": [
                {"author": {"login": "a", "__typename": "User"}, "createdAt": "2026-01-02T12:00:00Z"},
                {"author": {"login": "ci", "__typename": "Bot"}, "createdAt": "2026-01-01T00:02:00Z"},
            ]},
            "reviews": {"nodes": [
                {"author": {"login": "copilot", "__typename": "Bot"}, "state": "COMMENTED",
                 "submittedAt": "2026-01-01T00:05:00Z", "comments": {"totalCount": 4}},
                {"author": {"login": "r1", "__typename": "User"}, "state": "CHANGES_REQUESTED",
                 "submittedAt": "2026-01-02T00:00:00Z", "comments": {"totalCount": 2}},
            ]},
            "commits": {"totalCount": 1, "nodes": [
                {"commit": {"authoredDate": "2026-01-02T06:00:00Z", "committedDate": "2026-01-02T06:00:00Z", "additions": 7, "deletions": 3}},
            ]},
            "timelineItems": {"nodes": [
                {"__typename": "HeadRefForcePushedEvent", "createdAt": "2026-01-03T00:00:00Z"},
                {"__typename": "ReviewRequestedEvent", "createdAt": "2026-01-01T00:04:00Z"},
            ]},
        });
        let (_, p) = info(&n).unwrap();
        let seq: Vec<(&str, Option<&str>, Option<&str>)> = p
            .timeline
            .iter()
            .map(|e| (e.kind.as_str(), e.by.as_deref(), e.who.as_deref()))
            .collect();
        assert_eq!(
            seq,
            [
                ("opened", None, None),
                ("comment", Some("bot"), Some("ci")),
                ("review_requested", None, None),
                ("review", Some("bot"), Some("copilot")),
                ("review", Some("human"), Some("r1")),
                ("commit", None, None),
                ("comment", Some("human"), Some("a")),
                ("force_push", None, None),
                ("merged", None, None),
            ]
        );
        let own: Vec<bool> = p.timeline.iter().map(|e| e.author).collect();
        assert_eq!(own.iter().filter(|b| **b).count(), 1); // the author's own comment
        let review = p
            .timeline
            .iter()
            .find(|e| e.who.as_deref() == Some("r1"))
            .unwrap();
        assert_eq!(
            (review.state.as_deref(), review.n),
            (Some("CHANGES_REQUESTED"), Some(2))
        );
        assert_eq!(
            p.timeline.iter().find(|e| e.kind == "commit").unwrap().n,
            Some(10)
        );
    }

    #[test]
    fn rework_is_unset_without_a_human_review() {
        let n = json!({
            "number": 1, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": "2026-01-02T00:00:00Z",
            "author": {"login": "a"},
            "reviews": {"nodes": [{"author": {"login": "bot", "__typename": "Bot"}, "state": "COMMENTED", "submittedAt": "2026-01-01T00:01:00Z"}]},
            "commits": {"totalCount": 1, "nodes": [{"commit": {"authoredDate": "2026-01-01T00:00:00Z", "committedDate": "2026-01-01T00:00:00Z", "additions": 1, "deletions": 0}}]},
        });
        let (_, p) = info(&n).unwrap();
        assert_eq!((p.review_count, p.human_review_count), (1, 0));
        assert_eq!(
            (
                p.commits_after_review,
                p.lines_after_review,
                p.first_human_review_at
            ),
            (None, None, None)
        );
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
        assert_eq!(
            (p.review_count, p.approvals, p.changes_requested),
            (3, 2, 1)
        );
        assert_eq!(p.first_review_at.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(p.approved_at.as_deref(), Some("2026-01-02T12:00:00Z"));
        assert_eq!(p.reviewers, ["r1", "r2"]);
        assert_eq!(
            (p.merged_by.as_deref(), p.labels.as_slice()),
            (Some("m"), &["bug".to_string()][..])
        );
    }
}
