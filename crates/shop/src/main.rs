//! shop: a terminal workspace that shows one panel at a time, the lazi file browser first.
//! Every key goes to shop's own bindings first (chords like Alt+1 that panels don't use); the
//! rest go to the focused panel.

mod config;
mod git;
mod panel;
mod status;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, io, mem, process};

use lazi::{Key, LOAD_GRACE, Lookup, Outcome, wake};
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use config::{Action, Config, PanelSpec};
use panel::Panel;

struct Shop {
    config: Config,
    panels: Vec<Panel>,
    focus: usize,
    /// Keys typed toward a shop binding that is a sequence.
    pending: Vec<Key>,
    /// The key-binding menu is up; the next key closes it.
    help: bool,
}

enum Exit {
    Quit,
    /// Quit without writing the cwd file, so the shell stays where it was.
    QuitNoCwd,
    /// Time to first frame.
    Bench(Duration),
}

fn main() {
    let start = Instant::now();
    let mut bench = false;
    let mut cwd_file = None;
    let mut config = None;
    let mut dir = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--bench" {
            bench = true;
        } else if arg == "--cwd-file" {
            cwd_file = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--cwd-file=") {
            cwd_file = Some(PathBuf::from(OsStr::from_bytes(path)));
        } else if arg == "--config" {
            config = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--config=") {
            config = Some(PathBuf::from(OsStr::from_bytes(path)));
        } else if arg.as_bytes().starts_with(b"-") {
            eprintln!("shop: unknown argument {}", arg.to_string_lossy());
            process::exit(2);
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    if let Err(e) = launch(config, dir, cwd_file, bench.then_some(start)) {
        eprintln!("shop: {e}");
        process::exit(1);
    }
}

/// Everything that can fail before the terminal is taken happens first, so errors print to a
/// normal screen. `dir` overrides where the first lazi panel starts; `cwd_file` gets where it
/// ended, for a shell wrapper to cd into.
fn launch(config: Option<PathBuf>, dir: Option<PathBuf>, cwd_file: Option<PathBuf>, bench: Option<Instant>) -> Result<(), String> {
    let mut config = config::load(config)?;
    if let Some(dir) = dir
        && let Some(PanelSpec::Lazi { dir: start, .. }) = config.panels.iter_mut().find(|p| matches!(p, PanelSpec::Lazi { .. }))
    {
        *start = Some(dir);
    }
    let panels = config.panels.iter().map(Panel::new).collect::<Result<Vec<_>, _>>()?;
    let mut shop = Shop { config, panels, focus: 0, pending: Vec::new(), help: false };
    let mut term = ratatui::init();
    let res = run(&mut term, &mut shop, bench);
    for panel in &mut shop.panels {
        let _ = panel.clear_images(term.backend_mut());
    }
    ratatui::restore();

    let lazi = shop.panels.iter().find_map(|p| match p {
        Panel::Lazi(lazi) => Some(lazi),
        _ => None,
    });
    match res.map_err(|e| e.to_string())? {
        Exit::Quit => {
            if let (Some(path), Some(lazi)) = (cwd_file, lazi) {
                std::fs::write(&path, lazi.cwd().as_os_str().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        Exit::QuitNoCwd => {}
        Exit::Bench(elapsed) => eprintln!("first frame: {elapsed:?} ({} entries)", lazi.map_or(0, |l| l.entry_count())),
    }
    Ok(())
}

fn run(term: &mut DefaultTerminal, shop: &mut Shop, bench: Option<Instant>) -> io::Result<Exit> {
    let (tty, _tty_file) = wake::tty()?;
    let winch = wake::winch()?;
    let mut fds = vec![tty, winch.as_raw_fd()];
    for panel in &shop.panels {
        fds.extend(panel.wake_fds());
    }

    let mut dirty = true;
    loop {
        if dirty {
            // Hold the frame briefly so fast reads land in it instead of flashing an empty column.
            // A benchmark waits for everything, so it measures a complete frame.
            shop.panels[shop.focus].receive(if bench.is_some() { None } else { Some(LOAD_GRACE) });
            // Every frame, since programs a panel hands the terminal to may have changed it.
            queue!(term.backend_mut(), SetTitle(format!("shop: {}", shop.panels[shop.focus].title())))?;
            draw(term, shop)?;
            dirty = false;
            if let Some(start) = bench {
                return Ok(Exit::Bench(start.elapsed()));
            }
        }

        // Sleep until a key, a panel's worker or watcher, or a resize, unless crossterm already
        // has input buffered from an earlier read.
        if !event::poll(Duration::ZERO)? {
            wake::wait(&fds)?;
        }
        dirty |= wake::drain(winch.as_raw_fd());
        // Hidden panels too, so their watchers and workers keep draining.
        for panel in &mut shop.panels {
            dirty |= panel.on_wake();
        }
        while event::poll(Duration::ZERO)? {
            dirty = true;
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(exit) = handle(term, shop, lazi::normalize(key))? {
                return Ok(exit);
            }
        }
        for panel in &mut shop.panels {
            dirty |= panel.receive(Some(Duration::ZERO));
        }
    }
}

/// Where a key goes once it joins the keys already typed toward a shop binding.
#[derive(Debug, PartialEq)]
enum Route<'a> {
    /// Part of a longer shop binding; wait for the next key.
    Wait,
    Shop(&'a Action),
    /// No shop binding starts this way: these keys, in the order typed, go to the panel.
    Panel(Vec<Key>),
}

fn route<'a>(bound: &'a [(Vec<Key>, Action)], pending: &mut Vec<Key>, key: Key) -> Route<'a> {
    pending.push(key);
    match lazi::lookup(bound, pending) {
        Lookup::Pending => Route::Wait,
        Lookup::Action(action) => {
            pending.clear();
            Route::Shop(action)
        }
        Lookup::Unbound => Route::Panel(mem::take(pending)),
    }
}

/// Handles a key press. Returns how shop should exit, if it should.
fn handle(term: &mut DefaultTerminal, shop: &mut Shop, key: Key) -> io::Result<Option<Exit>> {
    if shop.help {
        // Any key closes the menu, and only closes it.
        shop.help = false;
        return Ok(None);
    }
    let len = shop.panels.len();
    let next = match route(&shop.config.keys, &mut shop.pending, key) {
        Route::Wait => return Ok(None),
        Route::Shop(Action::Quit) => return Ok(Some(Exit::Quit)),
        Route::Shop(Action::Next) => (shop.focus + 1) % len,
        Route::Shop(Action::Prev) => (shop.focus + len - 1) % len,
        Route::Shop(Action::Focus(i)) => if *i < len { *i } else { shop.focus },
        Route::Panel(keys) => {
            for key in keys {
                match shop.panels[shop.focus].key(term, key)? {
                    Outcome::Continue => {}
                    Outcome::Open(dir) => {
                        open(term, shop, &dir)?;
                        return Ok(None);
                    }
                    Outcome::Help => {
                        shop.help = true;
                        return Ok(None);
                    }
                    Outcome::Quit => return Ok(Some(Exit::Quit)),
                    Outcome::QuitNoCwd => return Ok(Some(Exit::QuitNoCwd)),
                }
            }
            return Ok(None);
        }
    };
    focus(term, shop, next)?;
    Ok(None)
}

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

/// The tab bar on the top row, the focused panel below it, as one synchronized update so the
/// terminal never shows a half-drawn frame.
fn draw(term: &mut DefaultTerminal, shop: &mut Shop) -> io::Result<()> {
    queue!(term.backend_mut(), BeginSynchronizedUpdate)?;
    term.draw(|frame| {
        let area = frame.area();
        if area.height < 2 {
            return;
        }
        tabs(frame.buffer_mut(), Rect { height: 1, ..area }, shop);
        let body = Rect { y: area.y + 1, height: area.height - 1, ..area };
        shop.panels[shop.focus].draw(frame, body);
        if shop.help {
            help(frame.buffer_mut(), body, shop);
        }
    })?;
    if shop.help {
        // An image would sit on top of the menu.
        shop.panels[shop.focus].hide(term.backend_mut())?;
    } else {
        shop.panels[shop.focus].sync_image(term.backend_mut())?;
    }
    execute!(term.backend_mut(), EndSynchronizedUpdate)
}

/// The key-binding menu over the panel: shop's own keys, then the focused panel's, in as many
/// columns as fit. ponytail: anything past the last column is cut; scroll it if a keymap ever
/// outgrows a screen.
fn help(buf: &mut Buffer, area: Rect, shop: &Shop) {
    let style = &shop.config.style;
    let ours: Vec<(String, String)> = shop.config.keys.iter().map(|(keys, action)| (lazi::key_label(keys), format!("{action:?}"))).collect();
    let panel = &shop.panels[shop.focus];
    let mut lines: Vec<Line> = Vec::new();
    for (title, entries) in [("shop", ours), (panel.name(), panel.help())] {
        lines.push(Line::from(Span::styled(title.to_owned(), style.tab_focused)));
        // Keymaps are unordered; one line per action, alphabetical, with all its keys.
        let mut by_action: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (keys, action) in entries {
            by_action.entry(action).or_default().push(keys);
        }
        let entries: Vec<(String, String)> = by_action
            .into_iter()
            .map(|(action, mut keys)| {
                keys.sort();
                (keys.join(", "), action)
            })
            .collect();
        let key_w = entries.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(16);
        for (keys, action) in entries {
            lines.push(Line::from(vec![Span::styled(format!("{keys:<key_w$}  "), Style::new().add_modifier(Modifier::BOLD)), Span::raw(action)]));
        }
        lines.push(Line::default());
    }
    Clear.render(area, buf);
    let block = Block::bordered().title(" keys · any key closes ");
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    const COLUMN: u16 = 44;
    let columns = (inner.width / COLUMN).max(1);
    let width = inner.width / columns;
    for (i, line) in lines.iter().enumerate() {
        let (col, row) = (i / inner.height as usize, i % inner.height as usize);
        if col >= columns as usize {
            break;
        }
        buf.set_line(inner.x + col as u16 * width, inner.y + row as u16, line, width.saturating_sub(1));
    }
}

/// Panel names with the focused one highlighted, and at the right any shop prefix being typed.
fn tabs(buf: &mut Buffer, area: Rect, shop: &Shop) {
    let style = &shop.config.style;
    let mut x = area.x;
    for (i, panel) in shop.panels.iter().enumerate() {
        let tab = if i == shop.focus { style.tab_focused } else { style.tab };
        (x, _) = buf.set_stringn(x, area.y, format!(" {} ", panel.name()), area.right().saturating_sub(x) as usize, tab);
    }
    if !shop.pending.is_empty() {
        let typed: Vec<String> = shop.pending.iter().map(|&key| key_name(key)).collect();
        let text = format!("{}-", typed.join(" "));
        let width = text.chars().count() as u16;
        buf.set_stringn(area.right().saturating_sub(width), area.y, text, area.width as usize, style.tab);
    }
}

/// A key the way emacs shows a pending prefix: C-x, M-g, or the character itself.
fn key_name((code, mods): Key) -> String {
    let mut name = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        name.push_str("C-");
    }
    if mods.contains(KeyModifiers::ALT) {
        name.push_str("M-");
    }
    match code {
        KeyCode::Char(' ') => name.push_str("SPC"),
        KeyCode::Char(c) => name.push(c),
        other => name.push_str(&format!("{other:?}")),
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    const CTRL_X: Key = (KeyCode::Char('x'), KeyModifiers::CONTROL);
    const B: Key = (KeyCode::Char('b'), KeyModifiers::NONE);
    const J: Key = (KeyCode::Char('j'), KeyModifiers::NONE);

    #[test]
    fn route_holds_a_prefix_then_runs_or_falls_through() {
        let bound = vec![(vec![CTRL_X, B], Action::Next)];
        let mut pending = Vec::new();
        assert_eq!(route(&bound, &mut pending, J), Route::Panel(vec![J]));
        assert_eq!(route(&bound, &mut pending, CTRL_X), Route::Wait);
        assert_eq!(route(&bound, &mut pending, B), Route::Shop(&Action::Next));
        assert!(pending.is_empty());
        // A prefix that leads nowhere hands every key typed, in order, to the panel.
        assert_eq!(route(&bound, &mut pending, CTRL_X), Route::Wait);
        assert_eq!(route(&bound, &mut pending, J), Route::Panel(vec![CTRL_X, J]));
        assert!(pending.is_empty());
    }
}
