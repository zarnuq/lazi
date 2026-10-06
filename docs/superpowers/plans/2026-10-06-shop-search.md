# shop Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** A Ctrl+p popup that searches repos, recent folders, file names and file contents at once, grouped like Obsidian.

**Architecture:** Pure helpers in `search.rs` (query, regex, rg parsing, rows) and `session.rs` (folder history), unit-tested; a `Search` popup that owns a worker thread for fd/rg and an eventfd, drawn by shop over the focused tab.

**Tech Stack:** Rust 2024, ratatui 0.30, fancy-regex 0.16 (already in the tree), fd, rg.

**Spec:** `docs/superpowers/specs/2026-10-06-shop-search-design.md`

## Global Constraints

- No new crates in the tree; `fancy-regex` is promoted from transitive to direct.
- The main loop never polls: fd/rg threads report through the popup's `Waker`.
- Every key comes from host.ron (`Ctrl+p` in `keys`, the popup's in `search.keys`).
- Not rustfmt-formatted; comments say why; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` green.

## Review Focus

1. A huge root (`~`): fd and rg must not block typing; the result caps hold.
2. Typing fast: stale rg results never overwrite newer ones; old rg processes are killed.
3. Invalid regex mid-typing: error in the title, no panic, previous results kept.
4. Zero results / empty query / no git tab: no panic in moving or Enter.
5. Binary or unreadable files in preview: `binary` / the error, no panic.

---

### Task 1: pure helpers

**Files:** Create `crates/shop/src/search.rs` (helpers only); modify `crates/shop/src/session.rs`; `crates/shop/Cargo.toml` (fancy-regex).

**Interfaces (produced):**
- `enum Scope { All, Repos, Dirs, Files, Content }`
- `fn split(query: &str) -> (Scope, &str)` — strips `repo:`/`dir:`/`file:`/`content:`.
- `fn compile(pattern: &str) -> Result<Option<Regex>, String>` — None for empty; `(?i)` unless an uppercase letter appears.
- `fn parse_rg(line: &[u8]) -> Option<(PathBuf, u64, String)>` — `path\0line:text`.
- `enum Hit { Repo(PathBuf), Dir(PathBuf), File(PathBuf), Line(PathBuf, u64, String) }`
- `enum Row { Section(&'static str), Hit(usize) }` and `fn rows(hits: &[Hit]) -> Vec<Row>` — section headers before each kind; for content, a `Hit::File` header per file comes from the caller.
- `session::remember(folders: &mut Vec<PathBuf>, dir: &Path)` — front-insert, dedupe, cap 500; `session::load_folders() / save_folders()`.

Tests (write first, watch fail): `split` for each prefix and none; `compile` smart case (lower matches upper, upper is exact), empty → None, bad regex → Err; `parse_rg` with a colon in the path; `remember` order/dedupe/cap; `rows` inserts one header per kind in order.

Commit: "Add search helpers: query prefixes, smart-case regex, rg parsing".

### Task 2: the popup

**Files:** `search.rs` (Search state, worker, draw), `main.rs`, `config.rs`, `git.rs`, `panel.rs`, `crates/lazi/src/lib.rs` (`Lazi::reveal`), both host.ron, README, CLAUDE.md.

**Interfaces (consumed):** Task 1's; `lazi::run`, `lazi::Cmd`, `lazi::wake::Waker`, `Lazi::goto`, `Lazi::reveal`.

Config: `Action::Search`; `Config.search: SearchSpec { open: String, open_line: String, style: SearchStyles { section, line_number, cursor }, keys: Vec<(Vec<Key>, SearchAction)> }`; `SearchAction { Down, Up, Open, Reveal, Close, DeleteChar, DeleteWord, Clear }`.

Verify in tmux (spec's manual list), then commit "Add a catch-all search popup to shop" and merge.
