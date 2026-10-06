//! shop: a terminal workspace that shows one panel at a time, the lazi file browser first.
//! Every key goes to shop's own bindings first (the number keys, Ctrl+c, Ctrl+p), unless a panel
//! is taking text; the rest go to the focused panel.

mod config;
mod git;
mod panel;
mod search;
mod session;
mod status;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, io, mem, process};

use lazi::{Cmd, Key, LOAD_GRACE, Lazi, Lookup, Outcome, wake};
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use config::{Action, Config, HelpAction, PanelSpec};
use panel::Panel;
use search::{Done, Hit, Search};

struct Shop {
    config: Config,
    panels: Vec<Panel>,
    focus: usize,
    /// Keys typed toward a shop binding that is a sequence.
    pending: Vec<Key>,
    /// The key-binding menu's first visible line, while it's up.
    help: Option<usize>,
    /// How many menu lines fit, from the last frame, for paging.
    help_page: usize,
    /// The Ctrl+p search box, while it's up.
    search: Option<Search>,
    /// Folders the lazi tabs have been in, newest first, for the search box.
    folders: Vec<PathBuf>,
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
    let config = config::load(config)?;
    // Where the focused file-browser tab opens: DIR, else the config's, else where shop was run.
    let configured = config.panels.iter().find_map(|p| match p {
        PanelSpec::Lazi { dir, .. } => dir.clone(),
        PanelSpec::Git(_) => None,
    });
    let here = match dir.or(configured) {
        Some(dir) => dir,
        None => env::current_dir().map_err(|e| e.to_string())?,
    };
    let saved = if config.restore { session::load() } else { None };
    let (specs, focus) = session::layout(saved, &config.panels, here, Path::is_dir);
    let panels = specs.iter().map(Panel::new).collect::<Result<Vec<_>, _>>()?;
    let mut shop = Shop { config, panels, focus, pending: Vec::new(), help: None, help_page: 0, search: None, folders: session::load_folders() };
    let mut term = ratatui::init();
    let res = run(&mut term, &mut shop, bench);
    for panel in &mut shop.panels {
        let _ = panel.clear_images(term.backend_mut());
    }
    ratatui::restore();

    if !matches!(res, Ok(Exit::Bench(_)))
        && let Err(e) = session::save_folders(&shop.folders)
    {
        eprintln!("shop: saving visited folders: {e}");
    }
    if shop.config.restore
        && !matches!(res, Ok(Exit::Bench(_)))
        && let Err(e) = session::save(&shop.session())
    {
        // The tabs just aren't restored next time; nothing else depends on it.
        eprintln!("shop: saving tabs: {e}");
    }

    // The cwd file gets the tab shop was left on, or the first lazi tab when that was git.
    let lazi = shop.panels.get(shop.focus).and_then(Panel::lazi).or_else(|| shop.panels.iter().find_map(Panel::lazi));
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
            // Built each time round: tabs come and go.
            let mut fds = vec![tty, winch.as_raw_fd()];
            for panel in &shop.panels {
                fds.extend(panel.wake_fds());
            }
            fds.extend(shop.search.as_ref().map(Search::wake_fd));
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
            // Keys are what move lazi tabs, so after one is when a new folder can show up.
            if let Some(lazi) = shop.panels[shop.focus].lazi() {
                session::remember(&mut shop.folders, lazi.cwd());
            }
        }
        if let Some(search) = &mut shop.search {
            dirty |= search.receive();
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
    if let Some(search) = &mut shop.search {
        match search.key(key) {
            Done::Stay => {}
            Done::Close => shop.search = None,
            // A folder or repo goes to lazi either way.
            Done::Open(Hit::Repo(dir) | Hit::Dir(dir)) | Done::Reveal(Hit::Repo(dir) | Hit::Dir(dir)) => {
                shop.search = None;
                open(term, shop, &dir)?;
            }
            Done::Open(hit) => {
                let root = search.root.clone();
                let spec = &shop.config.search;
                let (script, args) = match hit {
                    Hit::Line(path, n, _) => (&spec.open_line, vec![root.join(path), PathBuf::from(n.to_string())]),
                    Hit::File(path) | Hit::Text(path) | Hit::Repo(path) | Hit::Dir(path) => (&spec.open, vec![root.join(path)]),
                };
                let cmd = Cmd { desc: script, script, args: &args, block: true };
                // Blocking, so a failure is only the editor's own exit status: nothing to keep.
                let _ = lazi::run(term, &cmd, &root, Box::new(|_| {}))?;
                shop.search = None;
            }
            Done::Reveal(Hit::File(path) | Hit::Text(path) | Hit::Line(path, ..)) => {
                let root = search.root.clone();
                shop.search = None;
                reveal(term, shop, &root.join(path))?;
            }
        }
        return Ok(None);
    }
    if let Some(top) = shop.help {
        // The menu takes every key; ones it doesn't bind do nothing.
        shop.pending.push(key);
        match lazi::lookup(&shop.config.help_keys, &shop.pending) {
            Lookup::Pending => {}
            Lookup::Unbound => shop.pending.clear(),
            Lookup::Action(action) => {
                shop.help = scroll(top, action, shop.help_page);
                shop.pending.clear();
            }
        }
        return Ok(None);
    }
    let len = shop.panels.len();
    // While a panel takes text (a lazi prompt or its opener menu), digits and letters are typing,
    // not shop's tab keys.
    let routed = if shop.panels[shop.focus].wants_text() {
        shop.pending.clear();
        Route::Panel(vec![key])
    } else {
        route(&shop.config.keys, &mut shop.pending, key)
    };
    let next = match routed {
        Route::Wait => return Ok(None),
        Route::Shop(Action::Quit) => return Ok(Some(Exit::Quit)),
        Route::Shop(Action::Search) => {
            let (root, repos) = search_root(shop);
            match Search::open(shop.config.search.clone(), root, repos, shop.folders.clone()) {
                Ok(search) => shop.search = Some(search),
                Err(e) => eprintln!("shop: search: {e}"),
            }
            return Ok(None);
        }
        Route::Shop(Action::Next) => (shop.focus + 1) % len,
        Route::Shop(Action::Prev) => (shop.focus + len - 1) % len,
        Route::Shop(Action::Focus(i)) => if *i < len { *i } else { shop.focus },
        Route::Panel(keys) => {
            for key in keys {
                let outcome = shop.panels[shop.focus].key(term, key)?;
                if !matches!(outcome, Outcome::Continue) {
                    return answer(term, shop, outcome);
                }
            }
            return Ok(None);
        }
    };
    focus(term, shop, next)?;
    Ok(None)
}

impl Shop {
    /// The open tabs, for the next start.
    fn session(&self) -> session::Session {
        let tabs = self
            .panels
            .iter()
            .map(|p| match p.lazi() {
                Some(lazi) => session::Tab::Lazi(lazi.cwd().to_path_buf()),
                None => session::Tab::Other(p.name().to_owned()),
            })
            .collect();
        session::Session { tabs }
    }
}

/// Does what a panel's key asked of shop.
fn answer(term: &mut DefaultTerminal, shop: &mut Shop, outcome: Outcome) -> io::Result<Option<Exit>> {
    let len = shop.panels.len();
    match outcome {
        Outcome::Continue => {}
        Outcome::Open(dir) => open(term, shop, &dir)?,
        Outcome::Help => shop.help = Some(0),
        Outcome::NewTab(dir) => {
            // Errors here would be lazi's config, which already loaded for the first tab, or a
            // directory that vanished; either way there's simply no new tab.
            if let Ok(lazi) = Lazi::new(None, Some(dir)) {
                shop.panels.insert(shop.focus + 1, Panel::Lazi(Box::new(lazi)));
                focus(term, shop, shop.focus + 1)?;
            }
        }
        Outcome::NextTab => focus(term, shop, (shop.focus + 1) % len)?,
        Outcome::PrevTab => focus(term, shop, (shop.focus + len - 1) % len)?,
        Outcome::Quit => {
            // q on tab one (the first lazi tab, the one that follows where shop starts) quits;
            // on any other lazi tab it closes just that tab.
            if shop.panels.iter().position(|p| matches!(p, Panel::Lazi(_))) == Some(shop.focus) {
                return Ok(Some(Exit::Quit));
            }
            shop.panels[shop.focus].hide(term.backend_mut())?;
            shop.panels.remove(shop.focus);
            // The tab to the left takes over, as in yazi.
            shop.focus = shop.focus.saturating_sub(1);
            shop.panels[shop.focus].show();
        }
        Outcome::QuitNoCwd => return Ok(Some(Exit::QuitNoCwd)),
    }
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

/// The lazi tab things go to: the focused one, else tab 1.
fn lazi_tab(shop: &Shop) -> Option<usize> {
    if shop.panels[shop.focus].lazi().is_some() { Some(shop.focus) } else { shop.panels.iter().position(|p| p.lazi().is_some()) }
}

/// Shows `dir` in a lazi tab and focuses it. Without a lazi panel it does nothing.
fn open(term: &mut DefaultTerminal, shop: &mut Shop, dir: &Path) -> io::Result<()> {
    let Some(i) = lazi_tab(shop) else { return Ok(()) };
    if let Panel::Lazi(lazi) = &mut shop.panels[i] {
        lazi.goto(dir);
    }
    focus(term, shop, i)
}

/// Shows `path` in a lazi tab, the cursor on it, and focuses that tab.
fn reveal(term: &mut DefaultTerminal, shop: &mut Shop, path: &Path) -> io::Result<()> {
    let Some(i) = lazi_tab(shop) else { return Ok(()) };
    if let Panel::Lazi(lazi) = &mut shop.panels[i] {
        lazi.reveal(path);
    }
    focus(term, shop, i)
}

/// Where the search box looks, and the repos it offers: under the focused lazi tab, or from the
/// git tab under its selected repo.
fn search_root(shop: &Shop) -> (PathBuf, Vec<PathBuf>) {
    let repos: Vec<PathBuf> = shop.panels.iter().flat_map(Panel::repos).collect();
    let root = match &shop.panels[shop.focus] {
        Panel::Git(git) => git.selected(),
        Panel::Lazi(lazi) => Some(lazi.cwd().to_path_buf()),
    };
    let root = root.or_else(|| shop.panels.iter().find_map(|p| p.lazi()).map(|l| l.cwd().to_path_buf())).unwrap_or_else(|| PathBuf::from("."));
    (root, repos)
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
        if shop.help.is_some() {
            help(frame.buffer_mut(), body, shop);
        }
        if let Some(search) = &mut shop.search {
            search.draw(frame.buffer_mut(), body);
        }
    })?;
    if shop.help.is_some() || shop.search.is_some() {
        // An image would sit on top of the menu.
        shop.panels[shop.focus].hide(term.backend_mut())?;
    } else {
        shop.panels[shop.focus].sync_image(term.backend_mut())?;
    }
    execute!(term.backend_mut(), EndSynchronizedUpdate)
}

/// The key-binding menu over the panel: shop's own keys, then the focused panel's, one
/// scrollable column.
fn help(buf: &mut Buffer, area: Rect, shop: &mut Shop) {
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
    let page = area.height.saturating_sub(2) as usize;
    // Clamp here, where the total is known, so Bottom (usize::MAX) lands on the last page.
    let top = shop.help.unwrap_or(0).min(lines.len().saturating_sub(page));
    shop.help = Some(top);
    shop.help_page = page;
    let shown = format!(" keys · {}-{} of {} · q closes ", top + 1, (top + page).min(lines.len()), lines.len());
    let block = Block::bordered().title(shown);
    let inner = block.inner(area);
    block.render(area, buf);
    for (row, line) in lines.iter().skip(top).take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + row as u16, line, inner.width);
    }
}

/// Panel names with the focused one highlighted, and at the right any shop prefix being typed.
fn tabs(buf: &mut Buffer, area: Rect, shop: &Shop) {
    let style = &shop.config.style;
    let mut x = area.x;
    for (i, panel) in shop.panels.iter().enumerate() {
        let tab = if i == shop.focus { style.tab_focused } else { style.tab };
        // Numbered, since the number keys pick tabs.
        (x, _) = buf.set_stringn(x, area.y, format!(" {} {} ", i + 1, panel.name()), area.right().saturating_sub(x) as usize, tab);
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

/// Where the help menu's first line ends up after `action`, given the lines that fit (`page`);
/// None closes it. Past the end is clamped when it's drawn, which knows the total.
fn scroll(top: usize, action: &HelpAction, page: usize) -> Option<usize> {
    let half = (page / 2).max(1);
    Some(match action {
        HelpAction::Down => top + 1,
        HelpAction::Up => top.saturating_sub(1),
        HelpAction::PageDown => top + half,
        HelpAction::PageUp => top.saturating_sub(half),
        HelpAction::Top => 0,
        HelpAction::Bottom => usize::MAX,
        HelpAction::Close => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_scrolls_by_line_and_half_page_and_closes() {
        assert_eq!(scroll(0, &HelpAction::Down, 20), Some(1));
        assert_eq!(scroll(0, &HelpAction::Up, 20), Some(0));
        assert_eq!(scroll(30, &HelpAction::PageUp, 20), Some(20));
        assert_eq!(scroll(5, &HelpAction::PageDown, 20), Some(15));
        assert_eq!(scroll(5, &HelpAction::Top, 20), Some(0));
        assert_eq!(scroll(5, &HelpAction::Bottom, 20), Some(usize::MAX));
        assert_eq!(scroll(5, &HelpAction::Close, 20), None);
    }
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
