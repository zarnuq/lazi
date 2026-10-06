# shop: a TUI host with lazi as a panel

Date: 2026-10-06. Status: approved design, awaiting implementation plan.

## Goal

shop is a light, emacs-`-nw`-style terminal workspace. It owns the terminal and shows one
panel at a time; lazi is the first panel and keeps doing what it does now, including handing
the terminal to an editor when it opens files. Later panels (first: git status, local and
remote, across many projects) are separate specs. This spec covers only:

1. Turning this repo into a Cargo workspace.
2. Splitting lazi into a library with a panel-shaped API, with standalone lazi unchanged.
3. The shop binary: event loop, prefix-key routing, tab bar, panel switching.

Constraints carried over from lazi: Linux only, RON config with no built-in defaults, no idle
wakeups, workers never block the UI, minimal dependencies with default features off, no
rustfmt, comments say why.

## Decisions

| Question | Decision |
|---|---|
| Where shop lives | Cargo workspace in this repo |
| Screen layout | One panel fullscreen at a time, switch between them |
| Key routing | Host-first sequence map with a prefix (`Ctrl+x`); everything else goes to the focused panel |
| Config | Separate files: shop reads `host.ron`, the lazi panel reads lazi's `config.ron` as today |
| Panel dispatch | `enum Panel` in shop, `match` per method; no trait |
| lazi `q` inside shop | Quits shop |
| Run loop | Copied into shop, not shared; the two will diverge |

## Structure

```
Cargo.toml              [workspace] members = ["crates/*"], shared [profile.release]
Cargo.lock
README.md
crates/lazi/
  Cargo.toml            lib + bin, same dependencies as today
  config.ron            moved from the repo root
  src/lib.rs            pub struct Lazi, pub mod keys (Key, sequences, lookup, Lookup)
  src/main.rs           args and the run loop only, driving Lazi
  src/*.rs              existing modules
crates/shop/
  Cargo.toml            depends on lazi by path, plus ratatui, serde, ron, libc, signal-hook
  host.ron              reference config
  src/main.rs           args, terminal, run loop, key routing, tab bar
  src/panel.rs          enum Panel { Lazi(lazi::Lazi) }
  src/config.rs         host.ron schema and loading
```

## lazi library API

`lazi::Lazi` wraps `App` and its pending-key buffer. Today's `handle`, `apply`, `menu_key`,
`prompt_key`, `run_opener` and `run_cmd` move from `main.rs` onto it.

```rust
pub enum Outcome { Continue, Quit, QuitNoCwd }

impl Lazi {
    /// Loads lazi's config (once per process) and reads `cwd` inline.
    pub fn new(config: Option<PathBuf>, cwd: PathBuf) -> Result<Self, String>;
    pub fn wake_fds(&self) -> Vec<RawFd>;
    /// Drains watcher and worker events; returns whether a redraw is needed.
    pub fn on_wake(&mut self) -> bool;
    pub fn receive(&mut self, grace: Option<Duration>) -> bool;
    /// Takes the terminal because blocking openers, fzf and suspend hand it over.
    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome>;
    pub fn draw(&mut self, frame: &mut Frame, area: Rect);
    pub fn sync_image(&mut self, out: &mut impl Write) -> io::Result<()>;
    /// Removes lazi's image from the screen when another panel takes over.
    pub fn hide(&mut self, out: &mut impl Write) -> io::Result<()>;
    pub fn title(&self) -> String;
    pub fn cwd(&self) -> &Path;
}
```

Supporting changes inside lazi:

- `ui::draw` takes a `Rect` instead of using `frame.area()`. kitty placement already uses
  absolute cells, so the preview image follows the panel's area.
- `config::lookup` becomes generic over the bound action type:
  `lookup<'a, A>(bound: &'a [(Vec<Key>, A)], keys: &[Key]) -> Lookup<'a, A>`. lazi passes
  `&get().keys.normal`; shop passes its own map.
- `config::sequences` (the serde helper that parses `"Ctrl+x Ctrl+s"`) becomes public, so
  shop's config parses keys exactly as lazi's does. These two, `Key` and `Lookup`, are
  re-exported as `lazi::keys`.
- The standalone `lazi` binary keeps `--bench`, `--cwd-file` and `--config`, and behaves
  exactly as before.

## shop runtime

**Event loop.** Same shape as lazi's: `wake::wait` polls the tty, a SIGWINCH pipe and every
panel's `wake_fds()`. On wake, every panel gets `on_wake` and `receive`, so a hidden panel's
workers keep draining; only the focused panel draws. No timers.

**Keys.** `host.ron` holds a sequence map like lazi's `keys.normal`. Each key press is
appended to a pending buffer and looked up in shop's map first:

- Bound: run the shop action, clear the buffer.
- Prefix of a shop binding (e.g. `Ctrl+x`): keep waiting.
- Unbound: send the buffered keys, in order, to the focused panel and clear the buffer.

Initial shop actions: `Next`, `Prev`, `Focus(usize)` (by position in `panels`), `Quit`.
A panel returning `Outcome::Quit` or `QuitNoCwd` quits shop.

**Screen.** Row 0 is a tab bar: panel titles, the focused one in a highlight style, and the
pending prefix (e.g. `C-x-`) at the right while one is held. The focused panel gets the rest
of the screen. Each frame is wrapped in a synchronized update, as in lazi.

**Switching.** The outgoing panel's `hide` runs before the incoming panel draws, so a kitty
image never lingers over another panel.

**Terminal handoff.** lazi's blocking openers, fzf and `Ctrl+z` suspend already take the
terminal and clear kitty first, so they work unchanged inside shop.

**Errors.** A panel that fails to start (bad lazi `config.ron`, missing directory) exits shop
with `shop: <message>`, as lazi does today.

**Config.** `--config PATH`, else `$XDG_CONFIG_HOME/shop/host.ron`, else the first
`$XDG_CONFIG_DIRS/shop/host.ron`. Every field is required; `crates/shop/host.ron` is the
reference:

```ron
(
    panels: [
        Lazi(config: None, dir: None),
    ],
    style: (tab: (fg: "gray"), tab_focused: (fg: "black", bg: "blue")),
    keys: {
        "Ctrl+x b": Next,
        "Ctrl+x B": Prev,
        "Ctrl+x 1": Focus(0),
        "Ctrl+x Ctrl+c": Quit,
    },
)
```

`config: None` means lazi's usual lookup; `dir: None` means shop's cwd.

## Migration order

1. **Workspace move.** `git mv` `src` and `config.ron` into `crates/lazi/`, add the root
   workspace `Cargo.toml` with the shared release profile. No code changes.
2. **lazi library split.** Add `lib.rs` and `Lazi`, make `ui::draw` take a `Rect`, make
   `lookup` generic and `sequences` public, slim `main.rs`. Standalone lazi behaves the same.
3. **shop.** Add `crates/shop` with the loop, routing, tab bar and reference `host.ron`.

## Verification

The repo has no tests; behaviour is checked by running it.

- `cargo clippy --workspace` stays warning-free after each step.
- `cargo run --release -p lazi -- --config crates/lazi/config.ron --bench` before step 1 and
  after step 2: the first-frame time must not regress beyond run-to-run noise.
- Drive both binaries in tmux: navigation, a text preview, an image preview, an editor
  opener, `Ctrl+z` and `fg`, and in shop the `Ctrl+x` prefix (bound, unbound-falls-through,
  and the tab-bar indicator), plus `q` quitting shop.

## Packaging and docs

- `../gentoo-overlay/app-misc/lazi/lazi-9999.ebuild`: install from `crates/lazi`
  (`cargo_src_install --path crates/lazi`) and `doins crates/lazi/config.ron`. Without this the
  live ebuild breaks after step 1. shop is not packaged yet.
- `CLAUDE.md`: new layout, build commands with `-p`, and a short shop section.
- `README.md`: build path note; the lazi key table is unchanged.

## Out of scope

- The git projects panel (next spec). The `Panel` enum ships with only `Lazi`.
- Splits, multiple lazi instances, embedding arbitrary programs via PTY.
