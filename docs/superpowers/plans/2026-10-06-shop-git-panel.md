# shop Git Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `Git` panel to shop that lists the repos directly under configured roots with zhimmer's status symbols, fetches in the background, and opens a repo in lazi or runs configured commands in it.

**Architecture:** A pure `status.rs` (porcelain v2 parser and repo discovery, unit-tested) feeds a `Git` panel in `git.rs`, which owns a worker thread (status and fetch jobs), a ticker thread (interval fetches), an eventfd `Waker` and an inotify `Watcher` on each repo's `.git`. lazi exposes the few pieces the panel reuses: `Outcome::Open`, `Lazi::goto`, `lazi::run`/`Cmd`, `lazi::watch`.

**Tech Stack:** Rust 2024, ratatui 0.30, serde + ron 0.12, libc, the `git` CLI.

**Spec:** `docs/superpowers/specs/2026-10-06-shop-git-panel-design.md`

## Global Constraints

- No new crates beyond what the workspace already uses (shop gains `libc`, which lazi already depends on).
- The main loop never wakes on a timer; worker and ticker threads reach it only through the panel's `Waker`.
- git runs with `GIT_TERMINAL_PROMPT=0` and `GIT_OPTIONAL_LOCKS=0`, stdin null, and (unless the user set `GIT_SSH_COMMAND`) `ssh -o BatchMode=yes`, so nothing can prompt on shop's terminal.
- Not rustfmt-formatted; never run `cargo fmt`. Comments say why; `// SAFETY:` on every `unsafe` block.
- Every `host.ron` field is required; config errors print `shop: …` and exit 1 before the terminal is taken.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass after every task.
- Commits: imperative subject, body says what and why, ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

Deviation from the spec decided while planning: the ticker sends `Msg::Tick` to the panel (one main-loop wakeup per interval) instead of queueing a job, because only the panel knows the repo list and which repos are already fetching. The wakeup is the interval the user asked for, and it always leads to work. Task 3 records this in the spec.

## Review Focus

1. **A credential or ssh prompt**: must fail the fetch (`✗fetch`), never draw a prompt over the TUI or hang. Pinned by Task 3 step 7 (env in `git()`), checked by reading `git()`.
2. **`fetch_every: 0`**: must be a config error, not a busy loop. Pinned by Task 3 step 3 test.
3. **Empty roots / missing root / no repos at all**: no panic in key handling (`Down`, `Open`, `Run` with zero repos) or drawing. Pinned by Task 3 step 9.
4. **Our own git commands triggering the watcher**: status must not write `.git` (optional locks off), or the panel would loop. Pinned by Task 3 step 9 (idle check).
5. **Tiny panel area** (1–2 rows): no u16 underflow in `draw`. Pinned by Task 3 step 9.

---

### Task 1: lazi API for host panels

**Files:**
- Modify: `crates/lazi/src/lib.rs` (module visibility, re-exports, `Outcome::Open`, `Lazi::goto`)
- Modify: `crates/lazi/src/app.rs:351-361` (`goto` split into `goto` + `goto_path`)
- Modify: `crates/lazi/src/main.rs` (match arm for `Outcome::Open`)
- Modify: `crates/shop/src/main.rs` (`handle` matches `Outcome` instead of comparing)

**Interfaces:**
- Produces: `lazi::Outcome::Open(PathBuf)`; `Lazi::goto(&mut self, dir: &Path)`; `lazi::run(term: &mut DefaultTerminal, cmd: &Cmd, cwd: &Path, on_fail: Box<dyn FnOnce(String) + Send>) -> io::Result<Option<String>>`; `lazi::Cmd<'a> { desc: &'a str, script: &'a str, args: &'a [PathBuf], block: bool }`; `lazi::watch::Watcher { new() -> Option<Self>, fd(&self) -> RawFd, set(&mut self, &[PathBuf]), changed(&mut self) -> Vec<PathBuf> }`.

No unit test: these are thin wrappers over code that needs lazi's process-wide config loaded; Task 3's manual checks exercise `goto` and `run`.

- [ ] **Step 1: lib.rs visibility and re-exports**

In `crates/lazi/src/lib.rs`: `mod watch;` → `pub mod watch;`; after the `pub use config::…` line add `pub use open::{Cmd, run};`.

- [ ] **Step 2: `Outcome::Open`**

Add to `pub enum Outcome`, after `QuitNoCwd`:

```rust
    /// Show this directory in a lazi panel. Never returned by lazi itself; shop's other panels
    /// use it.
    Open(PathBuf),
```

- [ ] **Step 3: `goto_path` in app.rs**

Replace `App::goto` with:

```rust
    pub fn goto(&mut self, dir: &str) {
        let dir = match (dir.strip_prefix('~'), env::var_os("HOME")) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest.trim_start_matches('/')),
            _ => self.cwd.join(dir),
        };
        self.goto_path(dir);
    }

    pub fn goto_path(&mut self, dir: PathBuf) {
        if dir.is_dir() {
            self.cd(dir);
        } else {
            self.error = Some(format!("not a directory: {}", dir.display()));
        }
    }
```

- [ ] **Step 4: `Lazi::goto`**

In `impl Lazi`, after `cwd()`:

```rust
    /// Shows `dir`, picked somewhere else (shop's git panel).
    pub fn goto(&mut self, dir: &Path) {
        self.app.goto_path(dir.to_path_buf());
    }
```

- [ ] **Step 5: callers of `Outcome`**

In `crates/lazi/src/main.rs`, in `run`'s match on `lazi.key(…)`, add the arm `Outcome::Open(_) => {}` with the comment `// Only other panels in a host return this.`

In `crates/shop/src/main.rs`, `handle`'s `Route::Panel` arm becomes:

```rust
        Route::Panel(keys) => {
            for key in keys {
                match shop.panels[shop.focus].key(term, key)? {
                    Outcome::Continue => {}
                    Outcome::Open(dir) => {
                        open(term, shop, &dir)?;
                        return Ok(false);
                    }
                    Outcome::Quit | Outcome::QuitNoCwd => return Ok(true),
                }
            }
            return Ok(false);
        }
```

and the tail of `handle` (the `if next != shop.focus { … }`) becomes `focus(term, shop, next)?;` followed by `Ok(false)`. Add after `handle`:

```rust
/// Moves focus to panel `next`, taking the old one's images off the screen first.
fn focus(term: &mut DefaultTerminal, shop: &mut Shop, next: usize) -> io::Result<()> {
    if next != shop.focus {
        shop.panels[shop.focus].hide(term.backend_mut())?;
        shop.focus = next;
        shop.panels[next].show();
    }
    Ok(())
}

/// Shows `dir` in the first lazi panel and focuses it. Without a lazi panel it does nothing.
fn open(term: &mut DefaultTerminal, shop: &mut Shop, dir: &Path) -> io::Result<()> {
    let Some(i) = shop.panels.iter().position(|p| matches!(p, Panel::Lazi(_))) else { return Ok(()) };
    if let Panel::Lazi(lazi) = &mut shop.panels[i] {
        lazi.goto(dir);
    }
    focus(term, shop, i)
}
```

with `use std::path::{Path, PathBuf};` in shop's main.rs, and in `crates/shop/src/panel.rs` add:

```rust
    /// Called when the panel gains focus.
    pub fn show(&mut self) {
        match self {
            Panel::Lazi(_) => {}
        }
    }
```

- [ ] **Step 6: Verify**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: clean; 2 tests pass (kitty, routing).

- [ ] **Step 7: Commit**

```sh
git add crates && git commit -m "Let host panels open directories in lazi and run commands

Outcome gains Open(path), which shop answers by focusing its lazi panel
and calling the new Lazi::goto. lazi's command runner (run, Cmd) and its
inotify Watcher become public so shop's panels can reuse them.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: status parsing and repo discovery

**Files:**
- Create: `crates/shop/src/status.rs`
- Modify: `crates/shop/src/main.rs` (`mod status;`)

**Interfaces:**
- Produces: `status::Status { branch: String, detached: bool, ahead_behind: Option<(u32, u32)>, untracked, staged, modified, renamed, deleted, unmerged: bool }` (`Debug, Default, PartialEq`), `Status::dirty(&self) -> bool`, `status::parse(&str) -> Status`, `status::discover(&Path) -> io::Result<Vec<PathBuf>>`.

- [ ] **Step 1: Write the tests (with `todo!()` bodies so they compile and fail)**

Create `crates/shop/src/status.rs`:

```rust
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
        todo!()
    }
}

pub fn parse(text: &str) -> Status {
    todo!()
}

pub fn discover(root: &Path) -> io::Result<Vec<PathBuf>> {
    todo!()
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
        let found = discover(&root);
        let _ = fs::remove_dir_all(&root);
        let names: Vec<String> = found.unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["Alpha", "beta", "wt"]);
    }

    #[test]
    fn discover_fails_on_a_missing_root() {
        assert!(discover(Path::new("/nonexistent/shop-root")).is_err());
    }
}
```

Add `mod status;` to `crates/shop/src/main.rs` after `mod panel;`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p shop status::`
Expected: 7 failures panicking with `not yet implemented` (the missing-root test may pass or panic; either way the others fail).

- [ ] **Step 3: Implement**

Replace the three `todo!()` bodies:

```rust
    pub fn dirty(&self) -> bool {
        self.untracked || self.staged || self.modified || self.renamed || self.deleted || self.unmerged
    }
```

```rust
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
```

```rust
/// The git repositories directly under `root`: subdirectories with a `.git` entry (a directory,
/// or a file for worktrees and submodules), hidden ones skipped, sorted case-insensitively
/// like lazi's listings.
pub fn discover(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut repos: Vec<PathBuf> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().as_bytes().starts_with(b"."))
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").exists())
        .collect();
    repos.sort_by_cached_key(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()));
    Ok(repos)
}
```

- [ ] **Step 4: Run them to see them pass**

Run: `cargo test -p shop status::`
Expected: 8 passed. `cargo clippy --workspace --all-targets -- -D warnings` may flag `status` as dead code (nothing uses it until Task 3); if so, add `#[cfg_attr(not(test), allow(dead_code))]` above `mod status;` with the comment `// Used by the git panel from the next commit on.` and remove it in Task 3.

- [ ] **Step 5: Commit**

```sh
git add crates/shop/src && git commit -m "Parse git status and find repos for shop's git panel

parse reads git status --porcelain=v2 --branch into the flags zhimmer's
prompt shows (its XY rules, with v2's . for unchanged), plus the branch,
detached HEAD and ahead/behind. discover lists the repos directly under
a root. Both are pure and covered by tests.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: the git panel

**Files:**
- Create: `crates/shop/src/git.rs`
- Modify: `crates/shop/src/config.rs` (Git spec types, newtype-variant extension, `fetch_every` check, test)
- Modify: `crates/shop/src/panel.rs` (`Panel::Git` arms)
- Modify: `crates/shop/src/main.rs` (`mod git;`, remove any Task 2 dead-code allow)
- Modify: `crates/shop/Cargo.toml` (`libc`)
- Modify: `crates/shop/host.ron` (Git panel, `Ctrl+x 2`)
- Modify: `CLAUDE.md`, `README.md`, `docs/superpowers/specs/2026-10-06-shop-git-panel-design.md`

**Interfaces:**
- Consumes: Task 1's `Outcome::Open`, `lazi::run`, `lazi::Cmd`, `lazi::watch::Watcher`, `Panel::show`; Task 2's `status::{Status, parse, discover}`.
- Produces: `PanelSpec::Git(GitSpec)`; `git::Git` with `new(GitSpec) -> Result<Git, String>`, `wake_fds`, `on_wake`, `receive`, `show`, `key`, `draw`.

- [ ] **Step 1: Failing config test**

At the end of `crates/shop/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped host.ron is what /etc/xdg/shop gets, so it must stay loadable.
    #[test]
    fn reference_config_loads() {
        let config = load(Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/host.ron")))).unwrap();
        assert!(config.panels.iter().any(|p| matches!(p, PanelSpec::Git(_))));
    }

    #[test]
    fn fetch_every_zero_is_rejected() {
        let path = std::env::temp_dir().join(format!("shop-fetch-every-{}.ron", std::process::id()));
        let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/host.ron")).unwrap().replace("fetch_every: 300", "fetch_every: 0");
        fs::write(&path, text).unwrap();
        let err = load(Some(path.clone())).err();
        let _ = fs::remove_file(&path);
        assert!(err.unwrap().contains("fetch_every"));
    }
}
```

Run: `cargo test -p shop config::`
Expected: both fail to compile (`PanelSpec::Git` doesn't exist).

- [ ] **Step 2: Config types**

In `crates/shop/src/config.rs`, `PanelSpec` becomes:

```rust
#[derive(Deserialize)]
pub enum PanelSpec {
    /// `config: None` is lazi's usual lookup; `dir: None` is shop's working directory.
    Lazi { config: Option<PathBuf>, dir: Option<PathBuf> },
    Git(GitSpec),
}

/// The git panel: repos directly under `roots`, their status in zhimmer's symbols.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitSpec {
    /// A leading `~` is the home directory.
    pub roots: Vec<String>,
    /// Seconds between background fetch rounds.
    pub fetch_every: u64,
    pub symbols: GitSymbols,
    pub style: GitStyles,
    #[serde(deserialize_with = "lazi::sequences")]
    pub keys: Vec<(Vec<Key>, GitAction)>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitSymbols {
    pub untracked: String,
    pub staged: String,
    pub modified: String,
    pub renamed: String,
    pub deleted: String,
    pub unmerged: String,
    pub ahead: String,
    pub behind: String,
    pub clean: String,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitStyles {
    #[serde(deserialize_with = "lazi::style")]
    pub root: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub branch: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub status: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub arrows: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub clean: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub error: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub cursor: Style,
}

#[derive(Deserialize, Clone)]
pub enum GitAction {
    Down,
    Up,
    Top,
    Bottom,
    /// Show the repo in the lazi panel.
    Open,
    /// Find repos again, then re-read every status and fetch every repo.
    Refresh,
    /// A `sh -c` snippet run in the repo, with its path as $1: blocking hands over the
    /// terminal and waits; otherwise it's detached and only a failure is reported.
    Run { run: String, block: bool },
}
```

In `load`, the extensions line becomes

```rust
    // So `fg: "blue"` needn't be `fg: Some("blue")`, and `Git(roots: …)` needn't be `Git((roots: …))`.
    let ron = ron::Options::default().with_default_extension(Extensions::IMPLICIT_SOME | Extensions::UNWRAP_VARIANT_NEWTYPES);
```

and before `Ok(config)` add:

```rust
    for panel in &config.panels {
        if let PanelSpec::Git(git) = panel
            && git.fetch_every == 0
        {
            return Err(format!("{}: fetch_every must be at least 1", path.display()));
        }
    }
```

- [ ] **Step 3: Reference host.ron**

In `crates/shop/host.ron`, the comment block above `panels` gains a line
`//   Git(roots, fetch_every, symbols, style, keys): repos directly under each root, with`
`//   zhimmer's status symbols. Keys are the panel's own; Run's $1 is the repo's path.`
and `panels` becomes:

```ron
    panels: [
        Lazi(config: None, dir: None),
        Git(
            roots: ["~/Projects"],
            // Seconds between background fetch rounds.
            fetch_every: 300,
            symbols: (
                untracked: "?", staged: "+", modified: "!", renamed: "»", deleted: "✘",
                unmerged: "=", ahead: "⇡", behind: "⇣", clean: "✓",
            ),
            style: (
                root: (fg: "#89b4fa", bold: true),
                branch: (fg: "#cba6f7"),
                status: (fg: "#f9e2af"),
                arrows: (fg: "#fab387"),
                clean: (fg: "#a6e3a1"),
                error: (fg: "#f38ba8"),
                cursor: (reversed: true),
            ),
            keys: {
                "j": Down, "k": Up, "Down": Down, "Up": Up, "g g": Top, "G": Bottom,
                "Enter": Open, "l": Open,
                "r": Refresh,
                "g l": Run(run: "lazygit", block: true),
                "p": Run(run: "git pull --ff-only", block: true),
                "c": Run(run: r#"kitty --directory "$1" claude"#, block: false),
                "x": Run(run: r#"kitty --directory "$1" codex"#, block: false),
            },
        ),
    ],
```

and `keys` gains `"Ctrl+x 2": Focus(1),`.

- [ ] **Step 4: Panel enum arms (stubbed `git.rs` so it compiles)**

Add `libc = "0.2.189"` to `crates/shop/Cargo.toml` `[dependencies]`. Add `mod git;` in main.rs. Remove any dead-code allow on `mod status;`.

In `crates/shop/src/panel.rs`: `use crate::git::Git;`; `Panel::Git(Git)` variant; `new` gains `PanelSpec::Git(spec) => Ok(Panel::Git(Git::new(spec.clone())?)),`; and each method gains an arm:

| method | `Panel::Git(p)` arm |
|---|---|
| `name` | `"git"` |
| `title` | `"git".into()` |
| `wake_fds` | `p.wake_fds()` |
| `on_wake` | `p.on_wake()` |
| `receive` | `p.receive()` |
| `show` | `p.show()` |
| `key` | `p.key(term, key)` |
| `draw` | `p.draw(frame, area)` |
| `sync_image`, `hide`, `clear_images` | `Ok(())` (the git panel draws no images) |

- [ ] **Step 5: Run the config tests**

Run: `cargo test -p shop` (with Step 7's `git.rs` written; to see Step 1's tests go green first, a temporary `git.rs` with `pub struct Git;` and the methods returning defaults is fine).
Expected: the two config tests pass along with the 8 status tests and the routing test.

- [ ] **Step 6: Ledger the spec deviation**

In the spec's Data flow section, replace the Ticker paragraph's first two sentences with: "**Ticker thread**: sleeps `fetch_every` seconds and sends `Msg::Tick` through the panel's waker, forever. The panel answers by queueing a fetch of every repo not already fetching." and drop "This replaces the timerfd…" to the end of that paragraph, replacing it with "One main-loop wakeup per interval, which always leads to work; no timerfd and no unsafe."

- [ ] **Step 7: `crates/shop/src/git.rs`**

```rust
//! The git panel: every repository directly under the configured roots, with the status
//! symbols the zhimmer prompt shows, kept current by a worker thread, a ticker thread and
//! inotify on each repo's `.git`.

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use std::{env, mem, slice, thread};

use lazi::wake::{self, Waker};
use lazi::watch::Watcher;
use lazi::{Cmd, Key, Lookup, Outcome};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{GitAction, GitSpec, GitStyles, GitSymbols};
use crate::status::{self, Status};

/// Work for the worker thread.
enum Job {
    Status(Vec<PathBuf>),
    Fetch(Vec<PathBuf>),
}

/// What comes back from the worker, the fetch threads, the ticker and detached commands.
enum Msg {
    Status(PathBuf, Result<Status, String>),
    Fetched(PathBuf, Result<(), String>),
    /// A detached `Run` failed.
    Failed(PathBuf, String),
    /// Time for a fetch round.
    Tick,
}

/// Sends to the panel and wakes the main loop, which may be asleep in `wake::wait`.
#[derive(Clone)]
struct Notifier {
    tx: Sender<Msg>,
    waker: Waker,
}

impl Notifier {
    /// Returns false once the panel is gone, so threads know to stop.
    fn send(&self, msg: Msg) -> bool {
        let sent = self.tx.send(msg).is_ok();
        if sent {
            self.waker.wake();
        }
        sent
    }
}

struct Root {
    /// As written in the config, for the header.
    label: String,
    /// None when the root couldn't be read.
    repos: Option<Vec<PathBuf>>,
}

#[derive(Default)]
struct Repo {
    /// None until the first read comes back.
    status: Option<Result<Status, String>>,
    fetching: bool,
    fetch_error: Option<String>,
    /// A command's failure, shown until the next key.
    run_error: Option<String>,
}

pub struct Git {
    spec: GitSpec,
    roots: Vec<Root>,
    repos: HashMap<PathBuf, Repo>,
    /// Index into `all()`.
    cursor: usize,
    /// First list row on screen.
    offset: usize,
    /// Keys of a sequence typed so far, like the first `g` of `gg`.
    pending: Vec<Key>,
    jobs: Sender<Job>,
    rx: Receiver<Msg>,
    notify: Notifier,
    watcher: Option<Watcher>,
    /// Local time the last fetch round finished, as HH:MM.
    fetched: Option<String>,
}

impl Git {
    pub fn new(spec: GitSpec) -> Result<Self, String> {
        let waker = Waker::new().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        let notify = Notifier { tx, waker };
        let (jobs, job_rx) = mpsc::channel();
        let to_main = notify.clone();
        thread::spawn(move || worker(job_rx, to_main));
        let ticker = notify.clone();
        let every = Duration::from_secs(spec.fetch_every);
        thread::spawn(move || {
            loop {
                thread::sleep(every);
                if !ticker.send(Msg::Tick) {
                    break;
                }
            }
        });
        let mut git = Self {
            spec,
            roots: Vec::new(),
            repos: HashMap::new(),
            cursor: 0,
            offset: 0,
            pending: Vec::new(),
            jobs,
            rx,
            notify,
            watcher: Watcher::new(),
            fetched: None,
        };
        git.refresh();
        Ok(git)
    }

    pub fn wake_fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.notify.waker.fd()];
        fds.extend(self.watcher.as_ref().map(Watcher::fd));
        fds
    }

    /// Re-reads the status of repos whose `.git` changed. Results arrive through `receive`,
    /// so this never needs a redraw itself.
    pub fn on_wake(&mut self) -> bool {
        wake::drain(self.notify.waker.fd());
        let Some(watcher) = &mut self.watcher else { return false };
        let changed = watcher.changed();
        if !changed.is_empty() {
            // Each repo's watched directories (.git, .git/refs/heads) lie under it.
            let repos = self.all().into_iter().filter(|repo| changed.iter().any(|dir| dir.starts_with(repo))).collect();
            self.status(repos);
        }
        false
    }

    /// Takes in what the threads sent. Returns whether anything arrived.
    pub fn receive(&mut self) -> bool {
        let mut any = false;
        while let Ok(msg) = self.rx.try_recv() {
            any = true;
            match msg {
                Msg::Status(path, result) => {
                    if let Some(repo) = self.repos.get_mut(&path) {
                        repo.status = Some(result);
                    }
                }
                Msg::Fetched(path, result) => {
                    if let Some(repo) = self.repos.get_mut(&path) {
                        repo.fetching = false;
                        repo.fetch_error = result.err();
                    }
                    // The fetch moved the remote-tracking branch, so the arrows changed.
                    self.status(vec![path]);
                    if !self.repos.values().any(|repo| repo.fetching) {
                        self.fetched = Some(clock());
                    }
                }
                Msg::Failed(path, err) => {
                    if let Some(repo) = self.repos.get_mut(&path) {
                        repo.run_error = Some(err);
                    }
                }
                Msg::Tick => self.fetch(self.all()),
            }
        }
        any
    }

    /// Edits in a working tree touch nothing in `.git`, so coming back to the panel is the
    /// moment to look again.
    pub fn show(&mut self) {
        self.status(self.all());
    }

    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome> {
        for repo in self.repos.values_mut() {
            repo.run_error = None;
        }
        self.pending.push(key);
        let action = match lazi::lookup(&self.spec.keys, &self.pending) {
            Lookup::Pending => return Ok(Outcome::Continue),
            Lookup::Unbound => {
                self.pending.clear();
                return Ok(Outcome::Continue);
            }
            Lookup::Action(action) => action.clone(),
        };
        self.pending.clear();
        let all = self.all();
        let last = all.len().saturating_sub(1);
        match action {
            GitAction::Down => self.cursor = (self.cursor + 1).min(last),
            GitAction::Up => self.cursor = self.cursor.saturating_sub(1),
            GitAction::Top => self.cursor = 0,
            GitAction::Bottom => self.cursor = last,
            GitAction::Refresh => self.refresh(),
            GitAction::Open => {
                if let Some(path) = all.get(self.cursor) {
                    return Ok(Outcome::Open(path.clone()));
                }
            }
            GitAction::Run { run, block } => {
                if let Some(path) = all.get(self.cursor) {
                    let notify = self.notify.clone();
                    let failed = path.clone();
                    let on_fail = Box::new(move |msg| {
                        notify.send(Msg::Failed(failed, msg));
                    });
                    let cmd = Cmd { desc: &run, script: &run, args: slice::from_ref(path), block };
                    let err = lazi::run(term, &cmd, path, on_fail)?;
                    if let Some(repo) = self.repos.get_mut(path) {
                        repo.run_error = err;
                    }
                    if block {
                        // Whatever ran (a pull, lazygit) has likely changed the repo.
                        self.status(vec![path.clone()]);
                    }
                }
            }
        }
        Ok(Outcome::Continue)
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 2 {
            return;
        }
        let list = Rect { height: area.height - 1, ..area };
        let all = self.all();
        let name_w = all.iter().map(|p| name(p).chars().count()).max().unwrap_or(0).min(30);
        let branch_w = self.repos.values().filter_map(|r| r.status.as_ref()?.as_ref().ok()).map(|s| s.branch.chars().count()).max().unwrap_or(0).min(24);

        let mut rows: Vec<Line> = Vec::new();
        let mut cursor_row = None;
        let mut index = 0;
        for root in &self.roots {
            let mut header = vec![Span::styled(root.label.clone(), self.spec.style.root)];
            if root.repos.is_none() {
                header.push(Span::styled(" (missing)", self.spec.style.error));
            }
            rows.push(Line::from(header));
            for path in root.repos.iter().flatten() {
                if index == self.cursor {
                    cursor_row = Some(rows.len());
                }
                rows.push(self.row(path, name_w, branch_w));
                index += 1;
            }
        }

        let height = list.height as usize;
        if let Some(row) = cursor_row {
            if row < self.offset {
                // Show the root header above the first repo rather than cutting it off.
                self.offset = row.saturating_sub(1);
            } else if row >= self.offset + height {
                self.offset = row + 1 - height;
            }
        }
        self.offset = self.offset.min(rows.len().saturating_sub(1));

        let style = &self.spec.style;
        let buf = frame.buffer_mut();
        for (i, line) in rows.iter().skip(self.offset).take(height).enumerate() {
            let y = list.y + i as u16;
            buf.set_line(list.x, y, line, list.width);
            if cursor_row == Some(self.offset + i) {
                buf.set_style(Rect { y, height: 1, ..list }, style.cursor);
            }
        }
        let stamp = match &self.fetched {
            Some(time) => format!("fetched {time}"),
            None => "fetching…".to_owned(),
        };
        let width = stamp.chars().count() as u16;
        buf.set_stringn(list.right().saturating_sub(width), list.y, stamp, list.width as usize, Style::new());

        let (text, line_style) = self.status_line();
        buf.set_stringn(area.x, area.bottom() - 1, text, area.width as usize, line_style);
    }

    /// One repo's row: name, branch, symbols, and fetch state.
    fn row(&self, path: &Path, name_w: usize, branch_w: usize) -> Line<'static> {
        let style = &self.spec.style;
        let mut spans = vec![Span::raw(format!("  {:<name_w$}  ", name(path)))];
        let repo = self.repos.get(path);
        match repo.and_then(|r| r.status.as_ref()) {
            None => {}
            Some(Err(_)) => spans.push(Span::styled("✗", style.error)),
            Some(Ok(s)) => {
                // Dim: there's no upstream to compare with, so no arrows can show.
                let branch = if s.detached || s.ahead_behind.is_none() { style.branch.add_modifier(Modifier::DIM) } else { style.branch };
                spans.push(Span::styled(format!("{:<branch_w$}  ", s.branch), branch));
                spans.extend(symbols(s, &self.spec.symbols, style));
            }
        }
        if repo.is_some_and(|r| r.fetching) {
            spans.push(Span::raw(" …"));
        }
        if repo.is_some_and(|r| r.fetch_error.is_some()) {
            spans.push(Span::styled(" ✗fetch", style.error));
        }
        Line::from(spans)
    }

    /// The cursor repo's error if it has one, else what needs doing across all repos.
    fn status_line(&self) -> (String, Style) {
        if let Some(repo) = self.all().get(self.cursor).and_then(|path| self.repos.get(path)) {
            let status_err = match &repo.status {
                Some(Err(e)) => Some(e),
                _ => None,
            };
            if let Some(err) = repo.run_error.as_ref().or(repo.fetch_error.as_ref()).or(status_err) {
                return (err.clone(), self.spec.style.error);
            }
        }
        let (mut push, mut pull, mut dirty) = (0, 0, 0);
        for s in self.repos.values().filter_map(|r| r.status.as_ref()?.as_ref().ok()) {
            let (ahead, behind) = s.ahead_behind.unwrap_or((0, 0));
            push += usize::from(ahead > 0);
            pull += usize::from(behind > 0);
            dirty += usize::from(s.dirty());
        }
        if push + pull + dirty == 0 {
            return ("all clean".to_owned(), Style::new());
        }
        (format!("{push} to push · {pull} to pull · {dirty} dirty"), Style::new())
    }

    /// Every repo, in screen order.
    fn all(&self) -> Vec<PathBuf> {
        self.roots.iter().flat_map(|root| root.repos.iter().flatten().cloned()).collect()
    }

    /// Finds the repos again, watches their `.git`, and asks for every status and a fetch.
    fn refresh(&mut self) {
        self.roots = self.spec.roots.iter().map(|label| Root { label: label.clone(), repos: status::discover(&expand(label)).ok() }).collect();
        let all = self.all();
        self.repos.retain(|path, _| all.contains(path));
        for path in &all {
            self.repos.entry(path.clone()).or_default();
        }
        if let Some(watcher) = &mut self.watcher {
            // git writes the index, HEAD and branch refs through a lock file and a rename, which
            // these catch; commits, checkouts and pulls made elsewhere show up at once.
            let dirs: Vec<PathBuf> = all.iter().flat_map(|repo| [repo.join(".git"), repo.join(".git/refs/heads")]).collect();
            watcher.set(&dirs);
        }
        self.cursor = self.cursor.min(all.len().saturating_sub(1));
        self.status(all.clone());
        self.fetch(all);
    }

    fn status(&self, repos: Vec<PathBuf>) {
        if !repos.is_empty() {
            let _ = self.jobs.send(Job::Status(repos));
        }
    }

    /// Fetches the repos not already fetching.
    fn fetch(&mut self, repos: Vec<PathBuf>) {
        let mut todo = Vec::new();
        for path in repos {
            if let Some(repo) = self.repos.get_mut(&path)
                && !repo.fetching
            {
                repo.fetching = true;
                todo.push(path);
            }
        }
        if !todo.is_empty() {
            let _ = self.jobs.send(Job::Fetch(todo));
        }
    }
}

/// zhimmer's order: unmerged, deleted, renamed, modified, staged, untracked, then the arrows
/// with their counts; `clean` when there's none of it.
fn symbols(s: &Status, sym: &GitSymbols, style: &GitStyles) -> Vec<Span<'static>> {
    let local: String = [
        (s.unmerged, &sym.unmerged),
        (s.deleted, &sym.deleted),
        (s.renamed, &sym.renamed),
        (s.modified, &sym.modified),
        (s.staged, &sym.staged),
        (s.untracked, &sym.untracked),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, symbol)| symbol.as_str())
    .collect();
    let (ahead, behind) = s.ahead_behind.unwrap_or((0, 0));
    let mut arrows = String::new();
    if ahead > 0 {
        arrows.push_str(&format!("{}{ahead}", sym.ahead));
    }
    if behind > 0 {
        arrows.push_str(&format!("{}{behind}", sym.behind));
    }
    if local.is_empty() && arrows.is_empty() {
        return vec![Span::styled(sym.clean.clone(), style.clean)];
    }
    vec![Span::styled(local, style.status), Span::styled(arrows, style.arrows)]
}

/// Runs status jobs in order, and each fetch on a thread of its own: fetches wait on the
/// network, so they're done in parallel.
fn worker(jobs: Receiver<Job>, notify: Notifier) {
    for job in jobs {
        match job {
            Job::Status(repos) => {
                for path in repos {
                    let result = git(&path, &["status", "--porcelain=v2", "--branch"]).map(|out| status::parse(&out));
                    if !notify.send(Msg::Status(path, result)) {
                        return;
                    }
                }
            }
            Job::Fetch(repos) => {
                for path in repos {
                    let notify = notify.clone();
                    thread::spawn(move || {
                        let result = git(&path, &["fetch", "--quiet", "--prune"]).map(drop);
                        notify.send(Msg::Fetched(path, result));
                    });
                }
            }
        }
    }
}

/// Runs git in `repo`: its stdout, or its last stderr line.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo).stdin(Stdio::null());
    // Nothing may prompt on shop's terminal: a fetch that needs a password fails instead.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    // status would otherwise refresh the index, a write to .git that the watcher would see
    // and answer with another status, forever.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).map_or_else(|| out.status.to_string(), str::to_owned);
    Err(format!("git {}: {line}", args[0]))
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// A leading `~` is the home directory, as in a shell.
fn expand(root: &str) -> PathBuf {
    if let Some(rest) = root.strip_prefix('~')
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest.trim_start_matches('/'));
    }
    PathBuf::from(root)
}

/// The local time as HH:MM.
fn clock() -> String {
    // SAFETY: time accepts a null pointer and only returns the time.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: tm is plain data; all zeroes is a valid value for localtime_r to overwrite.
    let mut tm: libc::tm = unsafe { mem::zeroed() };
    // SAFETY: both pointers are to live locals of the right types.
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}
```

If `Lookup::Action(action) => action.clone()` trips the borrow checker (the borrow of `self.spec.keys` outliving into `self.pending.clear()`), bind `let found = …;` first and clone inside the match; the intent is "own the action before mutating self".

- [ ] **Step 8: Build, test, lint**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo build --release -p shop`
Expected: 13 tests pass (kitty 1, routing 1, status 8, config 2, plus 1 if counted differently — all green), clippy clean.

- [ ] **Step 9: Drive it in tmux**

Write `$SCRATCH/host.ron`: the reference file with `config: "/home/miles/Projects/lazi/crates/lazi/config.ron"` on the Lazi panel and `roots: ["~/Projects", "~/CCSO", "~/nonexistent-root"]`. Start shop in a tmux shell session (`tmux new-session -d -s shop -x 140 -y 40`), then check, capturing after each:

- `C-x 2`: the git tab is highlighted; `~/Projects` and `~/CCSO` headers with their repos; `~/nonexistent-root (missing)`; `fetching…` at top right, then `fetched HH:MM`; bottom line totals or `all clean`.
- Each row's symbols match `git -C <repo> status -sb` for three repos spot-checked.
- `touch ~/Projects/lazi/shop-probe && ` press `r`: lazi's row gains `?`; `rm` it, `r`: gone.
- In a second tmux window: `git -C ~/Projects/lazi commit --allow-empty -m probe`; the lazi row shows `⇡1` without a key in shop. Then `git -C ~/Projects/lazi reset --hard HEAD~1` and the row returns.
- `j`/`k`/`G`/`g g` move the cursor; the list scrolls in a 12-row window (`tmux resize-window -y 12`).
- `Enter` on lazi: focus switches to the lazi tab, which shows `~/Projects/lazi`.
- Back with `C-x 2`; `g l` with `run` changed to `git log --oneline | head -3; sleep 1` in the test config: the output shows, then shop redraws.
- A detached failing command (add `"z": Run(run: "false", block: false)` to the test config): pressing `z` shows `false: exit status: 1` on the bottom line in the error style.
- Idle check: with `fetch_every: 5`, leave shop for 20s; `fetched HH:MM` advances, and `strace -p <shop pid> -e trace=poll,ppoll -f 2>&1 | head` shows wakeups only around the ticks (no busy loop from the watcher).
- `tmux resize-window -y 2` then `-y 3` then back: no panic.
- A config with `roots: []` and with only a missing root: `j`, `Enter`, `p` do nothing, no panic.

- [ ] **Step 10: Docs**

`CLAUDE.md`, shop section: after "lazi … is the only panel so far" change to "Panels: lazi (via `lazi::Lazi`) and git (`git.rs`, repos under configured roots with zhimmer's status symbols; `status.rs` holds the pure parser and discovery, which the tests cover)." In "No idle wakeups", append: "The one periodic thing is shop's git ticker thread, which wakes the loop once per `fetch_every` to start a fetch round." Update the tests line to name the status and config tests. `README.md` shop section: "…runs lazi as one panel of a terminal workspace, next to a git panel that shows every repo under your project folders with zhimmer-style status symbols…".

- [ ] **Step 11: Commit**

```sh
git add crates/shop CLAUDE.md README.md docs/superpowers/specs/2026-10-06-shop-git-panel-design.md Cargo.lock
git commit -m "Add a git panel to shop

Lists the repos directly under configured roots with zhimmer's status
symbols: uncommitted changes, commits to push and to pull. A worker
thread reads git status (one --porcelain=v2 call per repo, ~5ms) and
fetches every repo in parallel at startup, on refresh and every
fetch_every seconds; inotify on each .git picks up commits, checkouts
and pulls made elsewhere at once. Enter opens the repo in the lazi
panel; Run bindings run commands in it, blocking or detached.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
