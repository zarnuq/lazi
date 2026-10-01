# lazi

A small, fast terminal file manager in the style of [yazi](https://github.com/sxyazi/yazi)
and ranger: three columns (parent, current, preview), vim keys, and image previews over
kitty's graphics protocol. It is configured by editing `src/config.rs` and rebuilding.

- **Previews** — text with syntax highlighting (bat's grammar set), images decoded in
  process, video thumbnails, and command output for audio, PDFs and archives. Built on a
  worker thread that only works on the file you stop on.
- **Live** — inotify watches the visible directories, so outside changes show without a
  keypress. An idle lazi sleeps in `poll(2)` and makes no wakeups.
- **File operations** — select, yank/cut/paste with progress, trash (freedesktop.org
  spec), permanent delete, create, rename.
- **Find and filter** — incremental smart-case find with Tab completion, and a live
  filter that narrows the listing.
- **Openers** — per file kind, with an open-with menu.
- **cd on quit** — writes where you left off for a shell wrapper to `cd` into.

Linux only (inotify, eventfd, memfd).

## Install

Gentoo: `app-misc/lazi` in the [zarnuq overlay](https://github.com/zarnuq/gentoo-overlay).

From source (Rust 1.88+):

```sh
cargo install --path .
```

## Usage

```sh
lazi [DIR] [--cwd-file PATH]
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

## Keys

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
| `/` `?` | Find forward / backward; `Tab` completes and moves into directories |
| `n` `N` | Next / previous match |
| `f` | Filter the listing |
| `Esc` | Clear the selection, else the filter, else the find highlight |
| `z` | Jump with fzf |
| `cc` `cd` `cf` `cn` | Copy path / directory / file name / name without extension |
| `J` `K` | Scroll the preview |
| `^z` | Suspend |
| `q` / `Q` | Quit / quit without writing the cwd file |

Actions act on the selection if there is one, else on the hovered entry.

## Configuration

Everything lives in [`src/config.rs`](src/config.rs): the keymap, colours, column ratio,
syntax theme, which previewer and which openers each file kind gets, and the clipboard
command. Edit and rebuild.

The defaults call out to these, all optional; lazi works without them and reports what's
missing when you use it:

| Tool | For |
| --- | --- |
| a kitty-graphics terminal (kitty, ghostty, wezterm) | image previews |
| `ffmpeg` | video thumbnails, images the `image` crate can't decode |
| `exiftool` | audio and PDF metadata previews |
| `bsdtar` (libarchive) | archive listings, "Extract here" |
| `file` | previews of everything else |
| `wl-copy` (wl-clipboard) | `cc` and friends |
| `fzf` | `z` |
| `gio` (glib) | trashing files on another filesystem |
| `$EDITOR` (else `nvim`), `xdg-open`, `swayimg`, `zathura`, `mpv`, `mediainfo` | openers |
