//! What the git panel knows about a repository: which directories are repos, and what
//! `git status --porcelain=v2 --branch` says about one. Pure, so it's what the tests cover.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq)]
pub struct Status {
    /// The branch name, or the short commit id when HEAD is detached.
    pub branch: String,
    pub detached: bool,
    /// Commits to push and to pull; None when the branch has no upstream.
    pub ahead_behind: Option<(u32, u32)>,
    pub untracked: bool,
    pub staged: bool,
    pub modified: bool,
    pub renamed: bool,
    pub deleted: bool,
    pub unmerged: bool,
}

impl Status {
    /// Whether anything is uncommitted.
    pub fn dirty(&self) -> bool {
        self.untracked || self.staged || self.modified || self.renamed || self.deleted || self.unmerged
    }
}

pub fn parse(text: &str) -> Status {
    let mut s = Status::default();
    let mut oid = "";
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            oid = rest;
        } else if let Some(rest) = line.strip_prefix("# branch.head ") {
            if rest == "(detached)" {
                s.detached = true;
            } else {
                s.branch = rest.to_owned();
            }
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // "+A -B"; only present when the branch has an upstream that exists.
            let mut counts = rest.split(' ');
            let ahead = counts.next().and_then(|a| a.strip_prefix('+')?.parse().ok());
            let behind = counts.next().and_then(|b| b.strip_prefix('-')?.parse().ok());
            if let (Some(ahead), Some(behind)) = (ahead, behind) {
                s.ahead_behind = Some((ahead, behind));
            }
        } else if line.starts_with("? ") {
            s.untracked = true;
        } else if line.starts_with("u ") {
            s.unmerged = true;
        } else if let Some(rest) = line.strip_prefix("1 ").or_else(|| line.strip_prefix("2 ")) {
            let mut xy = rest.chars();
            if let (Some(x), Some(y)) = (xy.next(), xy.next()) {
                flag(&mut s, x, y);
            }
        }
    }
    if s.detached {
        s.branch = oid.chars().take(7).collect();
    }
    s
}

/// zhimmer's rules for one XY pair (its lib/prompt.zsh), with v2's `.` where v1 has a space.
/// Unmerged pairs come as `u` lines in v2, so they never reach here.
fn flag(s: &mut Status, x: char, y: char) {
    if (x == 'A' && matches!(y, '.' | 'M' | 'D')) || (x == 'M' && matches!(y, '.' | 'M' | 'D')) {
        s.staged = true;
    }
    if matches!(x, '.' | 'M' | 'A' | 'R' | 'C') && y == 'M' {
        s.modified = true;
    }
    if x == 'R' && matches!(y, '.' | 'M' | 'D') {
        s.renamed = true;
    }
    if (matches!(x, '.' | 'M' | 'A' | 'R' | 'C' | 'D') && y == 'D') || (x == 'D' && matches!(y, '.' | 'M')) {
        s.deleted = true;
    }
}

/// The git repositories under `root`, down to `depth` levels (1 is its direct subdirectories),
/// sorted case-insensitively like lazi's listings. A repo is a directory with a `.git` entry (a
/// directory, or a file for worktrees and submodules). The search doesn't go into a repo, so
/// submodules and tools cloned inside one stay out, and skips hidden directories, where
/// `~/.local` and plugin managers keep clones nobody works in. Err only when `root` itself
/// can't be read.
pub fn discover(root: &Path, depth: usize) -> io::Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    let mut todo = vec![(fs::read_dir(root)?, depth)];
    while let Some((entries, depth)) = todo.pop() {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().as_bytes().starts_with(b".") {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.join(".git").exists() {
                repos.push(path);
            } else if depth > 1
                && let Ok(entries) = fs::read_dir(&path)
            {
                todo.push((entries, depth - 1));
            }
        }
    }
    repos.sort_by_cached_key(|path| path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_lowercase());
    Ok(repos)
}

/// The directories to watch so a commit, checkout or reset in `repo` is seen at once: `.git`
/// (git renames the index and HEAD into place there) and every directory of branch refs,
/// since a branch like `feat/x` lives in `refs/heads/feat/` and inotify isn't recursive.
pub fn watch_dirs(repo: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![repo.join(".git")];
    let mut todo = vec![repo.join(".git/refs/heads")];
    while let Some(dir) = todo.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            todo.extend(entries.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
            dirs.push(dir);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_branch_with_upstream() {
        let s = parse("# branch.oid 0123456789abcdef\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n");
        assert_eq!(s, Status { branch: "main".into(), ahead_behind: Some((0, 0)), ..Status::default() });
        assert!(!s.dirty());
    }

    #[test]
    fn ahead_and_behind_are_counted() {
        let s = parse("# branch.head main\n# branch.upstream origin/main\n# branch.ab +3 -2\n");
        assert_eq!(s.ahead_behind, Some((3, 2)));
    }

    #[test]
    fn no_upstream_has_no_counts() {
        let s = parse("# branch.oid 0123456789abcdef\n# branch.head feature\n");
        assert_eq!(s.ahead_behind, None);
        assert_eq!(s.branch, "feature");
    }

    #[test]
    fn detached_head_shows_the_short_commit() {
        let s = parse("# branch.oid 0123456789abcdef\n# branch.head (detached)\n");
        assert!(s.detached);
        assert_eq!(s.branch, "0123456");
    }

    #[test]
    fn each_local_change_sets_its_flag() {
        let s = parse(concat!(
            "# branch.head main\n",
            "1 M. N... 100644 100644 100644 aaa bbb staged.rs\n",
            "1 .M N... 100644 100644 100644 aaa bbb modified.rs\n",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\told.rs\n",
            "1 .D N... 100644 100644 000000 aaa bbb deleted.rs\n",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs\n",
            "? untracked.rs\n",
        ));
        assert!(s.staged && s.modified && s.renamed && s.deleted && s.unmerged && s.untracked);
        assert!(s.dirty());
    }

    #[test]
    fn a_staged_edit_is_not_also_a_worktree_edit() {
        // v2 writes "." for an unchanged side; it must not count as a change.
        let s = parse("# branch.head main\n1 M. N... 100644 100644 100644 aaa bbb f.rs\n");
        assert!(s.staged);
        assert!(!s.modified && !s.deleted && !s.renamed);
    }

    #[test]
    fn discover_finds_repos_directly_under_the_root() {
        let root = std::env::temp_dir().join(format!("shop-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["beta/.git", "Alpha/.git", "notes", ".hidden/.git", "deep/inner/.git"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        // A worktree or submodule has a .git file instead of a directory.
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::write(root.join("wt/.git"), "gitdir: elsewhere").unwrap();
        let found = discover(&root, 1);
        let _ = fs::remove_dir_all(&root);
        let names: Vec<String> = found.unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["Alpha", "beta", "wt"]);
    }

    #[test]
    fn watch_dirs_cover_branches_with_slashes() {
        let repo = std::env::temp_dir().join(format!("shop-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        fs::create_dir_all(repo.join(".git/refs/heads/feat/deep")).unwrap();
        fs::create_dir_all(repo.join(".git/refs/heads/fix")).unwrap();
        fs::write(repo.join(".git/refs/heads/main"), "").unwrap();
        let mut dirs = watch_dirs(&repo);
        let _ = fs::remove_dir_all(&repo);
        dirs.sort();
        let heads = repo.join(".git/refs/heads");
        assert_eq!(dirs, [repo.join(".git"), heads.clone(), heads.join("feat"), heads.join("feat/deep"), heads.join("fix")]);
    }

    /// Like a home directory: repos one and two levels down are found, a repo's own nested
    /// repos (submodules, cloned tools) and anything deeper or hidden are not.
    #[test]
    fn discover_searches_down_to_the_depth() {
        let root = std::env::temp_dir().join(format!("shop-depth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in [
            "dotfiles/.git",
            "Projects/lazi/.git",
            "Pictures/bgs/.git",
            "Projects/website/.git",
            "Projects/website/themes/ananke/.git",
            "notes/htb/EASY/cve/.git",
            "notes/Tools/payloads/.git",
            ".claude/plugins/x/.git",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let found = discover(&root, 2);
        let _ = fs::remove_dir_all(&root);
        let found: Vec<String> = found.unwrap().iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(found, ["dotfiles", "Pictures/bgs", "Projects/lazi", "Projects/website"]);
    }

    #[test]
    fn discover_fails_on_a_missing_root() {
        assert!(discover(Path::new("/nonexistent/shop-root"), 2).is_err());
    }
}
