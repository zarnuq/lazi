# CLAUDE.md

Guidance for working in this repo. Keep it short and current; delete anything that goes stale.

## What this is

**lazi** — a yazi/ranger-style terminal file manager in Rust (ratatui + crossterm). Three
columns, vim keys, previews (syntect highlighting, kitty-graphics images, command output),
inotify live updates. Linux only. Configured from a RON file (`crates/lazi/config.ron` is the
reference) with no built-in defaults: every option, key, previewer and opener comes from it.

## Build / run

```sh
cargo build --release      # profile: fat LTO, 1 codegen unit, stripped, unwinding
cargo run -p lazi -- --config crates/lazi/config.ron [DIR]   # needs a real terminal; drive it in tmux otherwise
cargo run --release -p lazi -- --config crates/lazi/config.ron --bench   # time to the first complete frame
cargo clippy --workspace   # keep it warning-free
```

There are no tests. Check behaviour by running it.

## Layout

lazi lives in `crates/lazi/src/`:

- `main.rs` — args, the event loop, key dispatch (`handle` → `config::lookup` → `apply`),
  prompt and `O`-menu key handling, frame drawing.
- `config.rs` — the config schema (serde structs, read with `ron`), loading into a static
  (`config::get()`), key-name parsing, and `lookup` for multi-key sequences. New features
  usually add an `Action` variant here, a match arm in `main::apply`, and a binding in
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

## Conventions & gotchas

- **No idle wakeups.** The main loop sleeps in `wake::wait`; anything that produces work
  for it from another thread must go through `Notifier::send` (which pokes the eventfd),
  never a timer or polling loop.
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
- The Gentoo ebuild is `app-misc/lazi/lazi-9999.ebuild` in `../gentoo-overlay`. If the
  minimum Rust version goes up, update `RUST_MIN_VER` there. It installs `config.ron` to
  `/etc/xdg/lazi/`.
