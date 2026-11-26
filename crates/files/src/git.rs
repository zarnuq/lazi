//! Git status signs for the current directory, like yazi's git plugin: one `git status` over the
//! whole repository on a worker, each directory showing the most pressing state beneath it.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config;

/// What git says about a path, in yazi's order: a directory shows the last of its children's.
/// Ignored paths never pass up to their directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Ignored,
    /// Unmerged.
    Updated,
    Deleted,
    Added,
    Staged,
    Unstaged,
    Untracked,
}

/// One run of `git status`, keyed by paths relative to the repository's top.
#[derive(Debug, Default)]
pub struct Status {
    states: HashMap<PathBuf, State>,
    /// Ignored and untracked directories, which git lists whole, so everything inside takes
    /// their state.
    whole: Vec<(PathBuf, State)>,
}

impl Status {
    /// Reads `git status --porcelain -z --no-renames` output.
    pub fn parse(out: &[u8]) -> Self {
        let mut status = Self::default();
        let mut changed = Vec::new();
        for entry in out.split(|&b| b == 0) {
            let (Some(xy), Some(path)) = (entry.get(..2), entry.get(3..)) else { continue };
            let Some(state) = state(xy) else { continue };
            let is_dir = path.ends_with(b"/");
            let path = PathBuf::from(OsStr::from_bytes(path.strip_suffix(b"/").unwrap_or(path)));
            if is_dir && matches!(state, State::Ignored | State::Untracked) {
                status.whole.push((path.clone(), state));
            }
            if state != State::Ignored {
                changed.push((path.clone(), state));
            }
            status.states.insert(path, state);
        }
        for (path, state) in changed {
            for dir in path.ancestors().skip(1).filter(|d| !d.as_os_str().is_empty()) {
                let shown = status.states.entry(dir.to_path_buf()).or_insert(state);
                *shown = (*shown).max(state);
            }
        }
        status
    }

    /// `path` is relative to the repository's top; None is clean.
    pub fn state(&self, path: &Path) -> Option<State> {
        self.whole.iter().find(|(dir, _)| path.starts_with(dir)).map(|&(_, state)| state).or_else(|| self.states.get(path).copied())
    }
}

/// yazi's patterns for an XY pair, the first match winning, except that every unmerged pair is
/// Updated: yazi's order shows `AA` as added and `DD` as deleted.
fn state(xy: &[u8]) -> Option<State> {
    let (x, y) = (xy[0], xy[1]);
    let either = |codes: &[u8]| codes.contains(&x) || codes.contains(&y);
    Some(match (x, y) {
        (_, b'!') => State::Ignored,
        (_, b'?') => State::Untracked,
        (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => State::Updated,
        (_, b'M' | b'T') => State::Unstaged,
        (b'M' | b'T', b' ') => State::Staged,
        _ if either(b"AC") => State::Added,
        _ if either(b"D") => State::Deleted,
        _ => return None,
    })
}

/// The status of the repository at `root`, None if git fails or isn't installed.
pub fn status(root: &Path) -> Option<Status> {
    // --no-optional-locks keeps git from refreshing the index, a write to .git that the watcher
    // would answer with another status, forever.
    let out = Command::new("git")
        .args(["--no-optional-locks", "status", "--porcelain", "-z", "-unormal", "--no-renames", "--ignored=matching"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| Status::parse(&out.stdout))
}

/// Which repository the cwd is in and its latest status, with at most one `git status` running.
#[derive(Default)]
pub struct Tracker {
    /// The cwd `root` was found for.
    cwd: PathBuf,
    /// None outside a repository, or with `git: None` in the config.
    pub root: Option<PathBuf>,
    pub status: Option<Status>,
    running: bool,
    /// Something changed while git ran, so its answer may already be stale.
    again: bool,
}

impl Tracker {
    /// Finds the repository holding `cwd` when the cwd changes. Returns whether it's a new one,
    /// which needs a status.
    pub fn follow(&mut self, cwd: &Path) -> bool {
        if cwd == self.cwd {
            return false;
        }
        self.cwd = cwd.to_path_buf();
        // A `.git` directory, or a file for worktrees and submodules.
        let root = config::get().git.as_ref().and_then(|_| cwd.ancestors().find(|dir| dir.join(".git").exists()));
        if root == self.root.as_deref() {
            return false;
        }
        self.root = root.map(Path::to_path_buf);
        self.status = None;
        self.root.is_some()
    }

    /// Looks for the repository again at the next `follow`, for a `git init` or a removed `.git`
    /// in a directory already being shown.
    pub fn rescan(&mut self) {
        self.cwd = PathBuf::new();
    }

    /// Where git keeps the index and HEAD: `.git`, or for a worktree or submodule, the directory
    /// its `.git` file names.
    pub fn git_dir(&self) -> Option<PathBuf> {
        let dot_git = self.root.as_ref()?.join(".git");
        let Ok(text) = std::fs::read_to_string(&dot_git) else { return Some(dot_git) };
        let dir = text.strip_prefix("gitdir: ")?.trim_end();
        Some(self.root.as_ref()?.join(dir))
    }

    /// The repository to run `git status` in, unless one is running already, in which case it
    /// runs again once that's done.
    pub fn start(&mut self) -> Option<PathBuf> {
        let root = self.root.clone()?;
        if self.running {
            self.again = true;
            return None;
        }
        // zq: a git that never exits (a hung network filesystem) leaves this set, stopping
        // updates for the session; a timeout like the previewers' would lift that.
        self.running = true;
        Some(root)
    }

    /// Takes a finished status. Returns whether to run another.
    pub fn finish(&mut self, root: &Path, status: Option<Status>) -> bool {
        self.running = false;
        if self.root.as_deref() == Some(root) {
            self.status = status;
        }
        std::mem::take(&mut self.again)
    }

    /// The state of `path`, an absolute path.
    pub fn state(&self, path: &Path) -> Option<State> {
        self.status.as_ref()?.state(path.strip_prefix(self.root.as_ref()?).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_show_their_most_pressing_child() {
        let out = b" M src/a.rs\0A  src/new.rs\0 D docs/old.md\0?? notes/\0!! target/\0!! src/x.log\0M  Cargo.toml\0UU both.rs\0AA both-added.rs\0DD both-gone.rs\0";
        let status = Status::parse(out);
        let state = |p: &str| status.state(Path::new(p));
        assert_eq!(state("src/a.rs"), Some(State::Unstaged));
        assert_eq!(state("src"), Some(State::Unstaged));
        assert_eq!(state("docs"), Some(State::Deleted));
        assert_eq!(state("Cargo.toml"), Some(State::Staged));
        assert_eq!(state("both.rs"), Some(State::Updated));
        assert_eq!(state("both-added.rs"), Some(State::Updated));
        assert_eq!(state("both-gone.rs"), Some(State::Updated));
        // Whole untracked and ignored directories cover what's inside them.
        assert_eq!(state("notes"), Some(State::Untracked));
        assert_eq!(state("notes/deep/x.md"), Some(State::Untracked));
        assert_eq!(state("target/debug"), Some(State::Ignored));
        assert_eq!(state("src/x.log"), Some(State::Ignored));
        // Ignored files don't mark their directory, and a clean path has no state.
        assert_eq!(state("README.md"), None);
        assert_eq!(state("targetx"), None);
    }
}
