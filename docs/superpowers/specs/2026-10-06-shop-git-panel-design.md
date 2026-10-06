# shop git panel: status of every repo under configured roots

Date: 2026-10-06. Status: approved design, awaiting implementation plan.

## Goal

A second shop panel that lists the git repositories directly under configured roots
(`~/Projects`, `~/CCSO`) and shows, per repo, the same status symbols the user's zhimmer
prompt shows: what is uncommitted, what is to push and what is to pull. From a row you can
jump into the repo in the lazi panel or run a configured command in it (lazygit, a new
terminal with claude or codex, `git pull`).

Today's setup: 15 repos, all direct children of the two roots, all with GitHub https
remotes, no git credential helper. Local status costs about 5ms per repo; a fetch about
0.45s.

## Decisions

| Question | Decision |
|---|---|
| Git access | The `git` CLI (`status --porcelain=v2 --branch`, `fetch`); no new crates |
| Rows | One per repo: name, current branch, zhimmer symbols |
| Discovery | Immediate children of each root that contain `.git`; no recursion |
| Fetching | At startup, on the refresh key, and every `fetch_every` seconds |
| Actions | Enter opens the repo in the lazi panel; configured `Run` commands, blocking or detached |
| Symbols and colours | zhimmer's, set in `host.ron` |

## Configuration

A new `PanelSpec` variant in `host.ron`. Every field is required.

```ron
Git(
    roots: ["~/Projects", "~/CCSO"],
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
        "j": Down, "k": Up, "g g": Top, "G": Bottom,
        "Enter": Open,
        "r": Refresh,
        "g l": Run(run: "lazygit", block: true),
        "p": Run(run: "git pull --ff-only", block: true),
        "c": Run(run: r#"kitty --directory "$1" claude"#, block: false),
        "x": Run(run: r#"kitty --directory "$1" codex"#, block: false),
    },
)
```

A leading `~` in a root is the home directory. Keys parse with `lazi::sequences`, like
shop's own.

## Structure

- `crates/shop/src/git.rs`: the `Git` panel. State, key handling, drawing, and the worker
  and ticker threads.
- `crates/shop/src/status.rs`: the pure parser from porcelain v2 text to a `Status`, with
  unit tests.
- `crates/shop/src/panel.rs`: `Panel::Git(Git)` and a match arm in every method.
- `crates/shop/src/config.rs`: `PanelSpec::Git { … }`, `GitSymbols`, `GitStyles`,
  `GitAction`.
- `crates/shop/src/main.rs`: handles a panel's new `Outcome::Open(path)` by focusing the
  first lazi panel and calling `Lazi::goto(path)`.

lazi gains a little public API:

- `Outcome::Open(PathBuf)`: a panel asks shop to show a directory in lazi. lazi itself never
  returns it.
- `Lazi::goto(&mut self, dir: &Path)`: cds there; sets lazi's error line if it isn't a
  directory.
- `lazi::run` and `lazi::Cmd` (re-exports of `open::run`, `open::Cmd`): run a `sh -c`
  snippet blocking (terminal handed over) or detached (own process group, stderr read back
  on failure).
- `lazi::watch::Watcher` (the module becomes public): inotify on a set of directories.
- `lazi::wake::Waker` is already public through `lazi::wake`.

## Data flow

The panel owns a `Waker` (eventfd) and an mpsc channel. Its `wake_fds` are the waker and
its `Watcher`; shop's loop already polls every panel's fds, so nothing in shop's loop
changes.

**Worker thread** (one per panel, takes `Job`s on a channel):

- `Job::Status(paths)`: runs `git status --porcelain=v2 --branch` with
  `GIT_OPTIONAL_LOCKS=0` in each repo, sequentially (15 × ~5ms), and sends one
  `Msg::Status(path, Result<Status, String>)` per repo.
- `Job::Fetch(paths)`: spawns one thread per repo running
  `git fetch --quiet --prune` with `GIT_TERMINAL_PROMPT=0` (a private repo fails instead
  of prompting on shop's terminal), sends `Msg::Fetched(path, Result<(), String>)` as each
  finishes, then queues a `Status` for that repo so its arrows update.

Every send pokes the waker. The panel's `on_wake` drains the waker and the watcher; its
`receive` drains the channel and applies results. No main-loop timers.

**Ticker thread**: sleeps `fetch_every` seconds and queues `Job::Fetch(all)`, forever. It
wakes the main loop only through the results it causes, so the main loop still sleeps
until there is something to draw. This replaces the timerfd mentioned while brainstorming:
same effect, no unsafe, and the main loop's no-idle-wakeup rule holds as written.

**When status refreshes**: at startup; on `Refresh`; when the panel gains focus; after each
repo's fetch; and when the watcher sees a create, delete or rename in a repo's `.git` or
`.git/refs/heads` (git writes `index`, `HEAD` and branch refs through a lock file and a
rename, so commits, checkouts, pulls and resets made elsewhere show up within one event).
Edits inside a working tree touch no `.git` file, so a newly modified file shows at the
next of the other triggers. Watching every file of 15 working trees is not worth it.

**When fetch runs**: at startup, on `Refresh`, and from the ticker. A fetch already running
for a repo is not started twice.

**Discovery** runs at startup and on `Refresh`: read each root, keep subdirectories with a
`.git` entry, sort by name (case-insensitive, like lazi). Hidden directories are skipped.

## Status parsing

A pure function `parse(porcelain_v2: &str) -> Status`:

```rust
pub struct Status {
    /// The branch name, or the short commit id when HEAD is detached.
    pub branch: String,
    pub detached: bool,
    /// None when the branch has no upstream.
    pub ahead_behind: Option<(u32, u32)>,
    pub untracked: bool,
    pub staged: bool,
    pub modified: bool,
    pub renamed: bool,
    pub deleted: bool,
    pub unmerged: bool,
}
```

Input lines: `# branch.oid <sha>`, `# branch.head <name>|(detached)`,
`# branch.upstream <ref>`, `# branch.ab +A -B`, `1 XY …`, `2 XY …` (rename or copy),
`u XY …` (unmerged), `? path`. In v2, `.` means unchanged where v1 had a space. The XY
rules are zhimmer's (`lib/prompt.zsh`), with `.` for space:

- untracked: a `?` line
- staged: X=A and Y∈{.,M,D,A,U}; X=M and Y∈{.,M,D}; X=U and Y=A
- modified: X∈{.,M,A,R,C} and Y=M
- renamed: X=R and Y∈{.,M,D}
- deleted: X∈{M,A,R,C,D,U,.} and Y=D; X=D and Y∈{.,U,M}
- unmerged: any `u` line

Detached HEAD: `branch` is the first 7 characters of `branch.oid`.

## Screen

```
 lazi  [git]                                        fetched 12:03
~/Projects
> lazi          main      ⇡1
  zhimmer       main      !?
  gendtree      dev       +⇣2
  secret        main      ✓ ✗fetch
~/CCSO
  ccso-net      main      ✓ …
  reach         a1b2c3d   !
3 to push · 1 to pull · 2 dirty
```

- One header row per root (`root` style), then its repos. The cursor moves over repo rows
  only. The list scrolls to keep the cursor visible.
- Columns: name, branch (`branch` style; dim when there is no upstream or HEAD is
  detached), symbols. Symbols come in zhimmer's order: unmerged, deleted, renamed,
  modified, staged, untracked (`status` style), then `⇡N` and `⇣N` (`arrows` style, only
  when non-zero). A repo with none of these shows `clean` (`clean` style).
- After the symbols: `…` while a fetch is running for that repo; `✗fetch` (`error` style)
  when the last fetch failed.
- A repo whose status failed shows `✗` in place of the symbols.
- A root that doesn't exist shows its header with `(missing)` and no rows.
- The top-right of the first row is the clock time of the last completed fetch round
  (`fetched 12:03`), or `fetching…` during the first one. A clock time rather than "2m ago",
  which would need a timer to keep current.
- The last row is the status line: when the cursor's repo has an error (fetch or status, or
  a failed detached `Run`), that message in `error` style; else the totals across all repos:
  `N to push · N to pull · N dirty`, or `all clean`. "Dirty" is any of the six local
  symbols.

## Actions

- `Down`, `Up`, `Top`, `Bottom`: move the cursor over repo rows.
- `Open`: returns `Outcome::Open(repo path)`. shop focuses the first lazi panel (hiding the
  git panel) and calls `Lazi::goto`. With no lazi panel configured, nothing happens.
- `Refresh`: rediscover repos, then queue a status for all and a fetch for all.
- `Run(run, block)`: `lazi::run` with the repo path as `$1`, from the repo directory.
  Blocking: shop's terminal is handed over and the repo's status is refreshed afterwards.
  Detached: the failure, if any, arrives later through the panel's channel and shows on the
  status line for that repo.

## shop changes

- `Panel::Git` arms everywhere. The tab label is `git`; the terminal title is `git`.
- A panel gaining focus gets a new `Panel::show()` call (no-op for lazi, a status refresh
  for git).
- `handle` treats `Outcome::Open(path)`: find the first `Panel::Lazi`, call `goto`, switch
  focus to it with the usual `hide`. `Quit`/`QuitNoCwd` still quit shop.
- The reference `host.ron` gains the `Git(…)` panel above, after `Lazi`, and a
  `"Ctrl+x 2": Focus(1)` binding.

## Errors

- A root that can't be read: header with `(missing)`.
- `git` not installed: every repo shows `✗` with `git: No such file or directory`.
- A fetch that fails (network, auth, no remote): `✗fetch`, with git's last stderr line on
  the status line. Status still works.
- A bad `Git(…)` block in `host.ron` (unknown colour, bad key, missing field): `shop: …`,
  exit 1, before the terminal is taken, as with every other config error.

## Testing and verification

- Unit tests (`cargo test -p shop`) for `parse`: clean on a branch with upstream; ahead and
  behind; no upstream; detached HEAD; each of the six local flags from representative
  `1`/`2`/`u`/`?` lines; a v2 `.` never counted as a change.
- A unit test for discovery against a temporary directory tree: repos found, non-repos and
  hidden directories skipped, sorted.
- Manual, in tmux, against the real roots: rows match `git status` in each repo; editing a
  file then pressing `r` shows `!`; `git commit` in another tmux window updates the row
  without a key; `Enter` lands lazi in the repo; a blocking `Run` (`git log` piped to
  `less`) hands the terminal over and back; a detached `Run` that fails shows its error; a
  root that doesn't exist shows `(missing)`; `fetch_every: 5` shows the fetch time advance
  while idle and nothing else redraws in between.
- `cargo clippy --workspace --all-targets` warning-free.

## Packaging and docs

- No ebuild change: `app-misc/shop` already builds `crates/shop` and installs `host.ron`.
- `CLAUDE.md`: the shop section mentions the git panel and its threads; the no-idle-wakeups
  rule notes that the ticker thread is the one periodic thing, and it wakes the main loop
  only with results.
- `README.md`: the shop section mentions the git panel.

## Out of scope

- Per-branch rows, stash counts, built-in pull/push (use `Run`).
- Recursing below the roots' immediate children.
- Watching working trees for edits.
