//! shop: a terminal workspace that shows one panel at a time. Every key goes to shop's own
//! bindings first (all behind a prefix like Ctrl+x); the rest go to the focused panel.

mod config;
mod panel;

use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;
use std::{env, io, mem, process};

use lazi::{Key, LOAD_GRACE, Lookup, Outcome, wake};
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Rect;

use config::{Action, Config};
use panel::Panel;

struct Shop {
    config: Config,
    panels: Vec<Panel>,
    focus: usize,
    /// Keys typed toward a shop binding, like the Ctrl+x of "Ctrl+x b".
    pending: Vec<Key>,
}

fn main() {
    let mut config = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            config = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--config=") {
            config = Some(PathBuf::from(OsStr::from_bytes(path)));
        } else {
            eprintln!("shop: unknown argument {}", arg.to_string_lossy());
            process::exit(2);
        }
    }
    if let Err(e) = start(config) {
        eprintln!("shop: {e}");
        process::exit(1);
    }
}

/// Everything that can fail before the terminal is taken happens first, so errors print to a
/// normal screen.
fn start(config: Option<PathBuf>) -> Result<(), String> {
    let config = config::load(config)?;
    let panels = config.panels.iter().map(Panel::new).collect::<Result<Vec<_>, _>>()?;
    let mut shop = Shop { config, panels, focus: 0, pending: Vec::new() };
    let mut term = ratatui::init();
    let res = run(&mut term, &mut shop);
    for panel in &mut shop.panels {
        let _ = panel.clear_images(term.backend_mut());
    }
    ratatui::restore();
    res.map_err(|e| e.to_string())
}

fn run(term: &mut DefaultTerminal, shop: &mut Shop) -> io::Result<()> {
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
            shop.panels[shop.focus].receive(Some(LOAD_GRACE));
            // Every frame, since programs a panel hands the terminal to may have changed it.
            queue!(term.backend_mut(), SetTitle(format!("shop: {}", shop.panels[shop.focus].title())))?;
            draw(term, shop)?;
            dirty = false;
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
            if handle(term, shop, lazi::normalize(key))? {
                return Ok(());
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

/// Handles a key press. Returns whether shop should quit.
fn handle(term: &mut DefaultTerminal, shop: &mut Shop, key: Key) -> io::Result<bool> {
    let len = shop.panels.len();
    let next = match route(&shop.config.keys, &mut shop.pending, key) {
        Route::Wait => return Ok(false),
        Route::Shop(Action::Quit) => return Ok(true),
        Route::Shop(Action::Next) => (shop.focus + 1) % len,
        Route::Shop(Action::Prev) => (shop.focus + len - 1) % len,
        Route::Shop(Action::Focus(i)) => if *i < len { *i } else { shop.focus },
        Route::Panel(keys) => {
            for key in keys {
                if shop.panels[shop.focus].key(term, key)? != Outcome::Continue {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
    };
    if next != shop.focus {
        shop.panels[shop.focus].hide(term.backend_mut())?;
        shop.focus = next;
    }
    Ok(false)
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
    })?;
    shop.panels[shop.focus].sync_image(term.backend_mut())?;
    execute!(term.backend_mut(), EndSynchronizedUpdate)
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
