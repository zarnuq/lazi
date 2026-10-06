# CLAUDE.md

Guidance for working in this repo. Keep it short and current; delete anything that goes stale.

## What this is

**lazi** — a terminal workspace in Rust (ratatui + crossterm), one tab at a time. Its main tab
is **files**, a yazi/ranger-style file manager library (`crates/files`, no binary of its own):
three columns, vim keys, previews (syntect highlighting, kitty-graphics images, command output),
inotify live updates. Linux only. Configured from RON files (`crates/lazi/config.ron` and
`crates/files/files.ron` are the references) with no built-in defaults: every option, key,
previewer and opener comes from them.

## Build / run

```sh
cargo build --release      # profile: fat LTO, 1 codegen unit, stripped, unwinding
cargo run -p lazi -- --config crates/lazi/config.ron [DIR]   # needs a real terminal; drive it in tmux otherwise
cargo run --release -p lazi -- --config crates/lazi/config.ron --bench   # time to the first complete frame
cargo clippy --workspace   # keep it warning-free
```

Tests are few: kitty image tracking in files; key routing, git status parsing, repo discovery,
session layout and the reference `config.ron` in lazi (`cargo test --workspace`).
Check behaviour by running it.

## Layout

The file browser lives in `crates/files/src/`:

- `lib.rs` — `Files`, the file browser as a panel lazi drives: key dispatch (`Files::key` →
  `config::lookup` → `apply`), prompt and `O`-menu key handling, drawing into a `Rect`.
- `config.rs` — the config schema (serde structs, read with `ron`), loading into a static
  (`config::get()`), key-name parsing, and `lookup` for multi-key sequences. New features
  usually add an `Action` variant here, a match arm in `apply` (`lib.rs`), and a binding in
  `files.ron`. New options are required fields: add them to `files.ron` too.
- `app.rs` — `App` state: cwd, cursor, listing cache, selection, yank, prompts, filter,
  find, history, background tasks, preview requests. Most logic lives here.
- `fs.rs` — `Listing` (sorted dirs-first, case-insensitive; freshness by mtime plus an
  explicit stale flag) and `Matcher` (smart-case find/filter, Tab-completion prefix).
- `ops.rs` — paste/move/delete/trash on worker threads with progress. Trash follows the
  freedesktop.org spec itself and falls back to the configured `trash_fallback` across
  filesystems.
- `preview.rs` — the preview worker (holds only the latest request; stale jobs are
  dropped), text/ANSI/syntax rendering, image decode and fit, previewer scripts with a
  timeout that kills the process group.
- `open.rs` — `rule()` picks the config rule for a path, running openers blocking or
  detached (detached stderr goes to a memfd, read back on failure), fzf capture, clipboard.
- `kitty.rs` — kitty graphics protocol; tracks what the terminal holds so frames only send
  escapes on change.
- `watch.rs` — inotify on the parent, cwd and previewed directory.
- `wake.rs` — `poll(2)` over the tty, an eventfd poked by workers, inotify and a SIGWINCH
  pipe.
- `ui.rs` — drawing: header, three columns, status line/prompts, opener menu.
- `input.rs` — single-line readline-style input for prompts.

## lazi

`crates/lazi` is the program: args (`[DIR] --cwd-file --config --bench`, which the zsh `y`
wrapper relies on), the event loop, and one panel at a time. Panels: files (via `files::Files` in
`crates/files/src/lib.rs`) and git (`git.rs`: repos found under configured roots, down to `depth` levels, with zhimmer's
status symbols, a worker thread for `git status`/`git fetch`, inotify on each `.git`;
`status.rs` holds the pure parser and discovery the tests cover). `dashboard.rs` is the home tab: configured shortcuts above the recently opened files
(files reports what its openers opened through `Files::take_opened`). `session.rs` saves the tabs
on exit and lays them out again on start (`restore` in `config.ron`), and keeps the visited-folder
and opened-file lists. `search.rs` is the Ctrl+p popup: pure query/regex/rg-parsing helpers (tested), plus fd
and rg on threads that stream results back through the popup's eventfd. Its config is `config.ron`
(`crates/lazi/config.ron` is the reference). Keys go to lazi's sequence map first and fall
through to the focused panel when nothing there starts with them, so lazi's bindings live
on keys the panels don't use (the number keys, `Ctrl+c`), and stand aside entirely while a
panel takes text (`Panel::wants_text`: a files prompt or opener menu). The files tab's `t`/`H`/`L`/`q` ask lazi
for tab changes through `Outcome`; every files tab shares one yank register (a static in `app.rs`). New panels are a `Panel` variant in
`panel.rs` plus a `PanelSpec` variant in `config.rs`.

```sh
cargo test -p lazi          # routing, status parsing, discovery, the reference config.ron
```

## Conventions & gotchas

- **No idle wakeups.** The main loop sleeps in `wake::wait`; anything that produces work
  for it from another thread must go through `Notifier::send` (which pokes the eventfd),
  never a timer or polling loop. The one periodic thing is lazi's git ticker
  thread, which wakes the loop once per `fetch_every` to start a fetch round.
- **Workers never block the UI.** Directory reads, previews and file operations happen off
  the main thread and come back as `Msg`. Only the initial cwd read is inline.
- **Before handing over the terminal** (blocking opener, fzf, suspend), call
  `app.kitty.clear(...)` or the image stays on screen.
- **Not rustfmt-formatted.** Lines run to ~140 columns and short `let … else` stays on one
  line. Don't run `cargo fmt`; match the surrounding style by hand.
- **Comments say why**, in full sentences; doc comments on most items. `// SAFETY:` on
  every `unsafe` block.
- **Keep dependencies minimal and features trimmed** (`default-features = false`
  throughout `Cargo.toml`).
- **Commits:** imperative subject; the body explains what changed and why, with numbers
  where they matter (timings, costs).
- Removing a feature is common. Delete it outright, including its `Action`, its bindings in
  `files.ron` or `config.ron` and any helpers it alone used. Also update the key table in `README.md`.
- The Gentoo ebuild is `app-misc/lazi/lazi-9999.ebuild` in `../gentoo-overlay`. If the
  minimum Rust version goes up, update `RUST_MIN_VER` there. It installs `config.ron` and
  `files.ron` to `/etc/xdg/lazi/`.
