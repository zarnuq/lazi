# lazi

A small, fast terminal file manager in the style of [yazi](https://github.com/sxyazi/yazi)
and ranger: three columns (parent, current, preview), vim keys, and image previews over
kitty's graphics protocol. Every key, previewer, opener and colour comes from a config file;
nothing is built in.

Linux only (inotify, eventfd, memfd).

## Install

Gentoo: `app-misc/lazi` in the [zarnuq overlay](https://github.com/zarnuq/gentoo-overlay).

From source (Rust 1.88+):

```sh
cargo install --path crates/lazi
mkdir -p ~/.config/lazi && cp crates/lazi/config.ron ~/.config/lazi/
```

## shop

`crates/shop` is an experimental host that runs lazi as one panel of a terminal workspace,
with `Ctrl+x` bindings of its own. See `crates/shop/host.ron`.

## Usage

```sh
lazi [DIR] [--cwd-file PATH] [--config PATH]
```

`--cwd-file` writes the directory lazi was in when you quit with `q` (not `Q`). A shell
wrapper to follow it:

```sh
l() {
    local tmp="$(mktemp)"
    lazi --cwd-file "$tmp" "$@"
    local dir="$(cat "$tmp")"
    rm -f "$tmp"
    [ -n "$dir" ] && [ "$dir" != "$PWD" ] && cd "$dir"
}
```

`--bench` prints the time to the first complete frame and exits.

## Configuration

lazi reads the first of `--config PATH`, `$XDG_CONFIG_HOME/lazi/config.ron` (usually
`~/.config/lazi/config.ron`) and `$XDG_CONFIG_DIRS/lazi/config.ron` (usually
`/etc/xdg/lazi/config.ron`). There are no built-in defaults: every setting is required, an
unbound key does nothing, and lazi refuses to start without a config, or with a mistake in
it, naming the line. Start from [`config.ron`](crates/lazi/config.ron), which documents every option.

The file is [RON](https://github.com/ron-rs/ron). It covers:

- options: hidden files, column ratio, scrolloff, syntax theme, tab size, preview timeout,
  the clipboard and cross-filesystem trash commands;
- styles for every coloured element;
- icons (Nerd Font glyphs, optionally coloured) by directory name, file name or extension,
  or none at all;
- named openers (shell snippets, blocking or detached);
- ordered rules matching directories, extensions or text files to a chain of previewers and
  a list of openers;
- keymaps for normal mode, the opener menu and prompts, plus the confirmation keys. Keys read
  like `"Ctrl+u"` or `"Shift+PageUp"`, and sequences like `"gg"` or `"Ctrl+x Ctrl+s"`. `Run`
  binds any shell snippet to a key, with an option to go to the path it prints (the fzf
  binding works this way).

The example config calls out to these, all optional; lazi works without them and reports
what's missing when you use it:

| Tool | For |
| --- | --- |
| a kitty-graphics terminal (kitty, ghostty, wezterm) | image previews |
| `ffmpeg` | video thumbnails, images the `image` crate can't decode |
| `pdftoppm` (poppler) | PDF previews |
| `exiftool` | audio and document metadata previews |
| `bsdtar` (libarchive) | archive listings, "Extract here" |
| `file` | previews of everything else |
| `wl-copy` (wl-clipboard) | `cc` and friends |
| `fzf` | `z` |
| `gio` (glib) | trashing files on another filesystem |
| `$EDITOR` (else `nvim`), `xdg-open`, `swayimg`, `mpv`, `mediainfo` | openers |

## Features

**Navigation**
- Three columns: parent, current directory, preview (widths in a 2:5:8 ratio).
- Vim keys and arrows, half and full pages, `gg`/`G`, multi-key bindings.
- Back/forward history; remembers the entry you were on in every directory visited.
- Bookmarked directories (`gh` `gc` `gd`) and an fzf jump.
- Hidden-file toggle. Listings sort directories first, then names ignoring case.
- Optional file icons, picked by name or extension, never by reading the file.
- The cursor stays 5 rows from the edge when scrolling.

**Previews**
- Text with syntax highlighting from bat's grammars, picked by extension, then file
  name, then first line. The first screen shows in about 3ms and the rest follows.
- ANSI colours in files and command output are kept; tabs are expanded. `J`/`K` scroll.
- Images decoded in process and shown with kitty's graphics protocol. Formats the image
  crate can't read (avif, heic, jxl) go through ffmpeg.
- PDFs show a page at a time (pdftoppm), `J`/`K` turning pages. Video gets an ffmpeg
  thumbnail; audio, epub and djvu exiftool metadata; archives a bsdtar listing; everything
  else `file -b`.
- Only the file you stop on is previewed. Preview commands are killed after 3s, and a
  decoder panic shows as a note instead of a crash.

**File operations**
- Select, select all, invert selection.
- Yank/cut and paste with progress. A name clash gets a `_1`-style suffix, or `P`
  overwrites.
- Trash per the freedesktop.org spec (`gio` for other filesystems); permanent delete.
- Create a file, or a directory if the name ends in `/`. Rename.
- Actions apply to the selection if there is one, else the hovered entry. Selected, yanked
  and cut entries are marked.

**Find and filter**
- Incremental find in either direction with `n`/`N` and highlighted matches. Smart case:
  case-sensitive only if the query has an uppercase letter.
- Shell-style Tab completion in find, which moves into directories. Shift+Tab steps
  through the matches without closing the prompt.
- A live filter that narrows the listing as you type.

**Opening**
- Openers per file kind: `o` runs the first, `O` offers them all.
- Openers either take over the terminal or run detached; a detached one's error is shown
  if it fails.
- Copy the path, directory, file name or name without extension to the clipboard.

**System**
- inotify picks up outside changes without a keypress.
- No wakeups at all when idle.
- Synchronized updates, so no half-drawn frames; the terminal title shows the cwd.
- `--cwd-file` for cd-on-quit (`Q` skips it); `^z` suspends.
- `--bench` measures time to the first frame.
- Configured entirely from a RON file.

## Keys

These are the bindings in the example `config.ron`.

| Key | Action |
| --- | --- |
| `j` `k` / arrows | Move |
| `h` `l` / arrows | Parent directory / enter |
| `gg` `G` | Top / bottom |
| `^d` `^u` / `S-PgDn` `S-PgUp` | Half page |
| `^f` `^b` / `PgDn` `PgUp` | Full page |
| `H` `L` | Back / forward in history |
| `gh` `gc` `gd` | Go to `~`, `~/.config`, `~/Downloads` |
| `.` | Toggle hidden files |
| `s` / `S` | Cycle sort (name, size, mtime, extension) / reverse it |
| `o` / `Enter` | Open with the first opener |
| `O` | Open with… (menu: `j`/`k`, `Enter` or `1`–`9`) |
| `Space` | Select and move down |
| `^a` / `^r` | Select all / invert selection |
| `y` / `x` | Yank / cut |
| `Y` / `X` | Clear the yank |
| `p` / `P` | Paste (renaming to `name_1` on a clash) / paste overwriting |
| `d` / `D` | Trash / delete permanently |
| `a` | Create a file, or a directory if the name ends in `/` |
| `r` | Rename |
| `/` `?` | Find forward / backward; `Tab` completes and moves into directories, `S-Tab` steps through matches |
| `n` `N` | Next / previous match |
| `f` | Filter the listing |
| `Esc` | Clear the selection, else the filter, else the find highlight |
| `z` | Jump with fzf |
| `cc` `cd` `cf` `cn` | Copy path / directory / file name / name without extension |
| `J` `K` | Scroll the preview, or turn a PDF's pages |
| `^z` | Suspend |
| `q` / `Q` | Quit / quit without writing the cwd file |

Actions act on the selection if there is one, else on the hovered entry.
