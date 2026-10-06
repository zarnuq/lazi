# shop search: one catch-all search box

Date: 2026-10-06. Status: approved design.

## Goal

One key (`Ctrl+p`) opens a popup over any tab that searches, as you type, four sources at once
and shows them grouped, like Obsidian's search: repos, recently visited folders, file names
and file contents. Matching is an exact, smart-case regex; there is no fuzzy matching, which
the user finds too tolerant.

## Behaviour

- **Opening:** `Ctrl+p` is a shop key, so it works from every tab except while a lazi prompt
  or opener menu takes text.
- **Query:** a regex in Rust syntax, case-insensitive unless it contains an uppercase
  letter. A prefix narrows it to one source: `repo:`, `dir:`, `file:`, `content:`. An empty
  query shows nothing. An invalid regex shows its error in the box title.
- **Root:** files and contents are searched under the focused lazi tab's folder; from the git
  tab, under the selected repo.
- **Sources:**
  - Repos: the git tab's repo list, matched on the full path.
  - Folders: lazi folders visited, newest first, saved across runs in
    `$XDG_STATE_HOME/shop/folders` (at most 500), matched on the full path.
  - Files: `fd --type f --color never` under the root, run once when the popup opens (it
    respects .gitignore and skips hidden files), matched on the path relative to the root.
  - Content: `rg --null --line-number --no-heading --color never` under the root, re-run on
    each query change on a thread; a newer query kills the older run. At most 500 lines.
- **Results:** sections Repos, Folders, Files, Content in that order, empty ones hidden.
  Content is grouped by file: a file row, then its matching lines (`48: text`). The cursor
  moves over result rows, skipping section headers.
- **Preview:** the right half. A file shows its first lines; a content line shows the lines
  around it with the matching one highlighted; a folder or repo lists its entries. Files
  over 64 KiB are read only that far; a file with NUL bytes shows `binary`.
- **Enter:** a file runs `search.open` (`$1` = path), a content line runs `search.open_line`
  (`$1` = path, `$2` = line), both blocking, from the root. A folder or repo makes the
  focused lazi tab (from git: tab 1) go there. **Ctrl+r** on a file or line reveals it in
  that lazi tab instead. Either closes the popup.
- **Keys inside** come from `search.keys` in host.ron: moving, Open, Reveal, Close, and
  editing the query (DeleteChar, DeleteWord, Clear). Any other printable key types.

## Structure

- `crates/shop/src/search.rs`: query parsing, the smart-case regex, rg output parsing,
  result rows (pure, unit-tested); the `Search` popup state, its worker thread, drawing.
- `crates/shop/src/session.rs`: load/save of the folder history next to the tab session.
- `crates/shop/src/main.rs`: `Action::Search`, routing keys to the popup while it's open,
  recording the focused lazi tab's folder after each key, drawing the popup.
- `crates/shop/src/git.rs`: `Git::repos()`, `Git::selected()`.
- `lazi::Lazi::reveal(&Path)`.
- `fancy-regex` becomes a direct dependency of shop; it is already in the tree via syntect.

## No idle cost

Nothing runs until the popup opens. fd and rg run on threads that wake the main loop through
the popup's eventfd; the popup's fd is polled like a panel's.

## Tests

Unit: prefix parsing, smart-case regex, rg `--null` line parsing, folder history ordering
(newest first, no duplicates, capped), row grouping. Manual in tmux: each source, a prefix,
an invalid regex, Enter on each kind, Ctrl+r, Esc, typing fast in a large root.
