# CLAUDE.md

Guidance for working in this repo. Keep it short and current; delete anything that goes stale.

## What this is

**shop** — a terminal workspace in Rust (ratatui + crossterm), one tab at a time. Its main tab
is **lazi**, a yazi/ranger-style file manager library (`crates/lazi`, no binary of its own):
three columns, vim keys, previews (syntect highlighting, kitty-graphics images, command output),
inotify live updates. Linux only. Configured from a RON file (`crates/lazi/config.ron` is the
reference) with no built-in defaults: every option, key, previewer and opener comes from it.

## Build / run

```sh
cargo build --release      # profile: fat LTO, 1 codegen unit, stripped, unwinding
cargo run -p shop -- --config crates/shop/host.ron [DIR]   # needs a real terminal; drive it in tmux otherwise
cargo run --release -p shop -- --config crates/shop/host.ron --bench   # time to the first complete frame
cargo clippy --workspace   # keep it warning-free
```

Tests are few: kitty image tracking in lazi; key routing, git status parsing, repo discovery
and the reference `host.ron` in shop (`cargo test --workspace`).
Check behaviour by running it.

## Layout

lazi lives in `crates/lazi/src/`:

- `lib.rs` — `Lazi`, lazi as a panel shop drives: key dispatch (`Lazi::key` →
  `config::lookup` → `apply`), prompt and `O`-menu key handling, drawing into a `Rect`.
- `config.rs` — the config schema (serde structs, read with `ron`), loading into a static
  (`config::get()`), key-name parsing, and `lookup` for multi-key sequences. New features
  usually add an `Action` variant here, a match arm in `apply` (`lib.rs`), and a binding in
  `config.ron`. New options are required fields: add them to `config.ron` too.
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

## shop

`crates/shop` is the program: args (`[DIR] --cwd-file --config --bench`, which the zsh `y`
wrapper relies on), the event loop, and one panel at a time. Panels: lazi (via `lazi::Lazi` in
`crates/lazi/src/lib.rs`) and git (`git.rs`: repos found under configured roots, down to `depth` levels, with zhimmer's
status symbols, a worker thread for `git status`/`git fetch`, inotify on each `.git`;
`status.rs` holds the pure parser and discovery the tests cover). Its config is `host.ron`
(`crates/shop/host.ron` is the reference). Keys go to shop's sequence map first and fall
through to the focused panel when nothing there starts with them, so shop's bindings live
on chords the panels don't use (`Alt+1`, `Alt+2`, `Alt+q`). New panels are a `Panel` variant in
`panel.rs` plus a `PanelSpec` variant in `config.rs`.

```sh
cargo test -p shop          # routing, status parsing, discovery, the reference host.ron
```

## Conventions & gotchas

- **No idle wakeups.** The main loop sleeps in `wake::wait`; anything that produces work
  for it from another thread must go through `Notifier::send` (which pokes the eventfd),
  never a timer or polling loop. The one periodic thing is shop's git ticker
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
  `config.ron` and any helpers it alone used. Also update the key table in `README.md`.
- The Gentoo ebuild is `app-misc/shop/shop-9999.ebuild` in `../gentoo-overlay`. If the
  minimum Rust version goes up, update `RUST_MIN_VER` there. It installs `host.ron` to
  `/etc/xdg/shop/` and lazi's `config.ron` to `/etc/xdg/lazi/`.
