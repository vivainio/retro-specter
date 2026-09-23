//! Thin wrapper around the `git` CLI.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub struct Repo {
    dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    /// 1-based `(start, count)` ranges of removed lines in the old file.
    pub removed: Vec<(u32, u32)>,
    /// 1-based `(start, count)` ranges of added lines in the new file.
    pub added: Vec<(u32, u32)>,
}

impl Repo {
    pub fn open(dir: &Path) -> Result<Repo> {
        let repo = Repo {
            dir: dir.to_path_buf(),
        };
        repo.run(&["rev-parse", "--git-dir"])
            .with_context(|| format!("{} is not a git repository", dir.display()))?;
        Ok(repo)
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(&self.dir)
            .args(["-c", "core.quotePath=false"]);
        c
    }

    fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<String> {
        let out = self
            .cmd()
            .args(args)
            .stderr(Stdio::piped())
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.iter()
                    .map(|a| a.as_ref().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The empty tree id (differs between SHA-1 and SHA-256 repositories).
    pub fn empty_tree(&self) -> Result<String> {
        let mut child = self
            .cmd()
            .args(["hash-object", "-t", "tree", "--stdin"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out)?;
        child.wait()?;
        Ok(out.trim().to_string())
    }

    /// Commits on the first-parent chain of `rev`, newest first.
    pub fn first_parent_log(
        &self,
        rev: &str,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<Commit>> {
        let mut args = vec![
            "log".to_string(),
            "--first-parent".into(),
            "--no-color".into(),
            "--format=%H%x1f%P%x1f%an%x1f%aI%x1f%s%x1f%b%x1e".into(),
        ];
        if let Some(s) = since {
            args.push(format!("--since={s}"));
        }
        if let Some(u) = until {
            args.push(format!("--until={u}"));
        }
        args.push(rev.to_string());
        args.push("--".into());

        let out = self.run(&args)?;
        Ok(out
            .split('\x1e')
            .filter_map(|rec| {
                let mut f = rec.trim_start_matches('\n').split('\x1f');
                let id = f.next().filter(|s| !s.is_empty())?.to_string();
                Some(Commit {
                    id,
                    parents: f
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .map(String::from)
                        .collect(),
                    author: f.next().unwrap_or("").to_string(),
                    date: f.next().unwrap_or("").to_string(),
                    subject: f.next().unwrap_or("").to_string(),
                    body: f.next().unwrap_or("").trim().to_string(),
                })
            })
            .collect())
    }

    /// Zero-context diff between two revisions, restricted to `pathspecs`.
    pub fn diff(&self, from: &str, to: &str, pathspecs: &[String]) -> Result<Vec<FileDiff>> {
        let mut args: Vec<String> = [
            "diff",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            from,
            to,
            "--",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend_from_slice(pathspecs);
        Ok(parse_diff(&self.run(&args)?))
    }

    pub fn cat_file(&self) -> Result<CatFile> {
        let mut child = self
            .cmd()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to start git cat-file")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(CatFile {
            child,
            stdin,
            stdout,
        })
    }
}

fn parse_diff(out: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // Remaining (old, new) lines of the current hunk; content lines may look
    // like headers (e.g. a removed "-- x" line), so they must be counted.
    let mut pending = (0u32, 0u32);

    for line in out.lines() {
        if pending != (0, 0) {
            match line.as_bytes().first() {
                Some(b'-') => pending.0 = pending.0.saturating_sub(1),
                Some(b'+') => pending.1 = pending.1.saturating_sub(1),
                _ => {}
            }
            continue;
        }
        if line.starts_with("diff --git ") {
            files.push(FileDiff::default());
        } else if let Some(p) = line.strip_prefix("--- ") {
            if let Some(f) = files.last_mut() {
                f.old_path = header_path(p, "a/");
            }
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if let Some(f) = files.last_mut() {
                f.new_path = header_path(p, "b/");
            }
        } else if let Some(h) = line.strip_prefix("@@ ")
            && let (Some(f), Some((old, new))) = (files.last_mut(), parse_hunk(h))
        {
            if old.1 > 0 {
                f.removed.push(old);
            }
            if new.1 > 0 {
                f.added.push(new);
            }
            pending = (old.1, new.1);
        }
    }
    // Pure renames, mode changes and binary files have no ---/+++ headers.
    files.retain(|f| f.old_path.is_some() || f.new_path.is_some());
    files
}

fn header_path(p: &str, prefix: &str) -> Option<String> {
    let p = p.trim_end_matches('\t');
    if p == "/dev/null" {
        return None;
    }
    let p = p
        .strip_prefix('"')
        .and_then(|p| p.strip_suffix('"'))
        .unwrap_or(p);
    Some(p.strip_prefix(prefix).unwrap_or(p).to_string())
}

/// Parses `-a[,b] +c[,d] @@ ...` into `((a, b), (c, d))`.
fn parse_hunk(h: &str) -> Option<((u32, u32), (u32, u32))> {
    let mut parts = h.split_whitespace();
    let range = |s: &str| -> Option<(u32, u32)> {
        let (start, count) = s.split_once(',').unwrap_or((s, "1"));
        Some((start.parse().ok()?, count.parse().ok()?))
    };
    let old = range(parts.next()?.strip_prefix('-')?)?;
    let new = range(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

/// A persistent `git cat-file --batch` process for fast blob reads.
pub struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    /// Reads `rev:path`; returns `None` if the object is missing or larger than `max_bytes`.
    pub fn read(&mut self, rev: &str, path: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        writeln!(self.stdin, "{rev}:{path}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() != 3 {
            return Ok(None); // "<name> missing" / "<name> ambiguous"
        }
        let size: usize = fields[2]
            .parse()
            .with_context(|| format!("bad cat-file header: {header}"))?;
        let mut buf = vec![0u8; size + 1]; // content + trailing LF
        self.stdout.read_exact(&mut buf)?;
        buf.pop();
        Ok((fields[1] == "blob" && size <= max_bytes).then_some(buf))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zero_context_diff() {
        let out = "\
diff --git a/x.py b/x.py
index 1..2 100644
--- a/x.py
+++ b/x.py
@@ -3 +3,2 @@ def f():
--- tricky removed line
+++ tricky added line
+second
@@ -10,2 +11,0 @@
-a
-b
diff --git a/new.cs b/new.cs
new file mode 100644
--- /dev/null
+++ b/new.cs
@@ -0,0 +1,3 @@
+a
+b
+c
";
        let d = parse_diff(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].old_path.as_deref(), Some("x.py"));
        assert_eq!(d[0].removed, vec![(3, 1), (10, 2)]);
        assert_eq!(d[0].added, vec![(3, 2)]);
        assert_eq!(d[1].old_path, None);
        assert_eq!(d[1].new_path.as_deref(), Some("new.cs"));
        assert_eq!(d[1].added, vec![(1, 3)]);
    }
}
