//! Finding git checkouts under a directory tree.

use std::fs;
use std::path::{Path, PathBuf};

const SKIP_DIRS: &[&str] = &["node_modules", "target", "venv", "__pycache__"];

/// Every directory under `root` (including `root`) that holds a git checkout,
/// sorted by path. Doesn't descend into checkouts (so submodules and nested
/// vendored repos are ignored), hidden directories, or symlinks.
pub fn find_repos(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, max_depth, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    // `.git` is a directory normally, a file for worktrees and submodules.
    if dir.join(".git").exists() {
        found.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir()); // false for symlinks
        if is_dir && !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
            walk(&e.path(), depth - 1, found);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_checkouts_but_not_inside_them() {
        let root = std::env::temp_dir().join(format!("retro-specter-disc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in [
            "a/.git",
            "b/c/.git",
            "a/vendor/.git",
            "b/node_modules/x/.git",
            "d/e",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let found = find_repos(&root, 4);
        assert_eq!(found, vec![root.join("a"), root.join("b/c")]);
        assert_eq!(find_repos(&root, 1), vec![root.join("a")]);
        fs::remove_dir_all(&root).unwrap();
    }
}
