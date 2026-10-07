//! lazi: a terminal workspace that shows one panel at a time, the files file browser first.
//! Every key goes to lazi's own bindings first (the number keys, Ctrl+c, Ctrl+p), unless a panel
//! is taking text; the rest go to the focused panel.

mod config;
mod dashboard;
mod git;
mod magit;
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

use files::{Cmd, Key, LOAD_GRACE, Files, Lookup, Outcome, wake};
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

struct Lazi {
    config: Config,
    panels: Vec<Panel>,
    focus: usize,
    /// Keys typed toward a lazi binding that is a sequence.
    pending: Vec<Key>,
    /// The key-binding menu's first visible line, while it's up.
    help: Option<usize>,
    /// How many menu lines fit, from the last frame, for paging.
    help_page: usize,
    /// The Ctrl+p search box, while it's up.
    search: Option<Search>,
    /// Folders the files tabs have been in, newest first, for the search box.
    folders: Vec<PathBuf>,
    /// Files opened from files, the search box or the dashboard, newest first, for the dashboard.
    files: Vec<PathBuf>,
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
            eprintln!("lazi: unknown argument {}", arg.to_string_lossy());
            process::exit(2);
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    if let Err(e) = launch(config, dir, cwd_file, bench.then_some(start)) {
        eprintln!("lazi: {e}");
        process::exit(1);
    }
}

/// Everything that can fail before the terminal is taken happens first, so errors print to a
/// normal screen. `dir` overrides where the first files panel starts; `cwd_file` gets where it
/// ended, for a shell wrapper to cd into.
fn launch(config: Option<PathBuf>, dir: Option<PathBuf>, cwd_file: Option<PathBuf>, bench: Option<Instant>) -> Result<(), String> {
    let config = config::load(config)?;
    // Where the focused file-browser tab opens: DIR, else the config's, else where lazi was run.
    let configured = config.panels.iter().find_map(|p| match p {
        PanelSpec::Files { dir, .. } => dir.clone(),
        _ => None,
    });
    let here = match dir.or(configured) {
        Some(dir) => dir,
        None => env::current_dir().map_err(|e| e.to_string())?,
    };
    let saved = if config.restore { session::load() } else { None };
    let (specs, focus) = session::layout(saved, &config.panels, here, Path::is_dir);
    let panels = specs.iter().map(Panel::new).collect::<Result<Vec<_>, _>>()?;
    let mut lazi = Lazi { config, panels, focus, pending: Vec::new(), help: None, help_page: 0, search: None,
        folders: session::load_history("folders"),
        files: session::load_history("files"),
    };
    let mut term = ratatui::init();
    let res = run(&mut term, &mut lazi, bench);
    for panel in &mut lazi.panels {
        let _ = panel.clear_images(term.backend_mut());
    }
    ratatui::restore();

    if !matches!(res, Ok(Exit::Bench(_))) {
        lazi.settle();
        for (name, list) in [("folders", &lazi.folders), ("files", &lazi.files)] {
            if let Err(e) = session::save_history(name, list) {
                eprintln!("lazi: saving {name}: {e}");
            }
        }
    }
    if lazi.config.restore
        && !matches!(res, Ok(Exit::Bench(_)))
        && let Err(e) = session::save(&lazi.session())
    {
        // The tabs just aren't restored next time; nothing else depends on it.
        eprintln!("lazi: saving tabs: {e}");
    }

    // The cwd file gets the tab lazi was left on, or the first files tab when that was git.
    let files = lazi.panels.get(lazi.focus).and_then(Panel::files).or_else(|| lazi.panels.iter().find_map(Panel::files));
    match res.map_err(|e| e.to_string())? {
        Exit::Quit => {
            if let (Some(path), Some(files)) = (cwd_file, files) {
                std::fs::write(&path, files.cwd().as_os_str().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        Exit::QuitNoCwd => {}
        Exit::Bench(elapsed) => eprintln!("first frame: {elapsed:?} ({} entries)", files.map_or(0, |l| l.entry_count())),
    }
    Ok(())
}

fn run(term: &mut DefaultTerminal, lazi: &mut Lazi, bench: Option<Instant>) -> io::Result<Exit> {
    let (tty, _tty_file) = wake::tty()?;
    let winch = wake::winch()?;
    let mut dirty = true;
    loop {
        if dirty {
            // Hold the frame briefly so fast reads land in it instead of flashing an empty column.
            // A benchmark waits for everything, so it measures a complete frame.
            lazi.panels[lazi.focus].receive(if bench.is_some() { None } else { Some(LOAD_GRACE) });
            // Every frame, since programs a panel hands the terminal to may have changed it.
            queue!(term.backend_mut(), SetTitle(format!("lazi: {}", lazi.panels[lazi.focus].title())))?;
            draw(term, lazi)?;
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
            for panel in &lazi.panels {
                fds.extend(panel.wake_fds());
            }
            fds.extend(lazi.search.as_ref().map(Search::wake_fd));
            wake::wait(&fds)?;
        }
        dirty |= wake::drain(winch.as_raw_fd());
        // Hidden panels too, so their watchers and workers keep draining.
        for panel in &mut lazi.panels {
            dirty |= panel.on_wake();
        }
        while event::poll(Duration::ZERO)? {
            dirty = true;
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(exit) = handle(term, lazi, files::normalize(key))? {
                return Ok(exit);
            }
            if let Panel::Files(files) = &mut lazi.panels[lazi.focus] {
                let opened = files.take_opened();
                for file in &opened {
                    session::remember(&mut lazi.files, file);
                }
                if !opened.is_empty() {
                    lazi.settle();
                }
            }
        }
        if let Some(search) = &mut lazi.search {
            dirty |= search.receive();
        }
        for panel in &mut lazi.panels {
            dirty |= panel.receive(Some(Duration::ZERO));
        }
    }
}

/// Where a key goes once it joins the keys already typed toward a lazi binding.
#[derive(Debug, PartialEq)]
enum Route<'a> {
    /// Part of a longer lazi binding; wait for the next key.
    Wait,
    Lazi(&'a Action),
    /// No lazi binding starts this way: these keys, in the order typed, go to the panel.
    Panel(Vec<Key>),
}

fn route<'a>(bound: &'a [(Vec<Key>, Action)], pending: &mut Vec<Key>, key: Key) -> Route<'a> {
    pending.push(key);
    match files::lookup(bound, pending) {
        Lookup::Pending => Route::Wait,
        Lookup::Action(action) => {
            pending.clear();
            Route::Lazi(action)
        }
        Lookup::Unbound => Route::Panel(mem::take(pending)),
    }
}

/// Handles a key press. Returns how lazi should exit, if it should.
fn handle(term: &mut DefaultTerminal, lazi: &mut Lazi, key: Key) -> io::Result<Option<Exit>> {
    if let Some(search) = &mut lazi.search {
        match search.key(key) {
            Done::Stay => {}
            Done::Close => lazi.search = None,
            // A folder or repo goes to files either way.
            Done::Open(Hit::Repo(dir) | Hit::Dir(dir)) | Done::Reveal(Hit::Repo(dir) | Hit::Dir(dir)) => {
                lazi.search = None;
                open(term, lazi, &dir)?;
            }
            Done::Open(hit) => {
                let root = search.root.clone();
                let (path, line) = match hit {
                    Hit::Line(path, n, _) => (path, Some(n)),
                    Hit::File(path) | Hit::Text(path) | Hit::Repo(path) | Hit::Dir(path) => (path, None),
                };
                edit(term, lazi, &root, &root.join(path), line)?;
                lazi.search = None;
            }
            Done::Reveal(Hit::File(path) | Hit::Text(path) | Hit::Line(path, ..)) => {
                let root = search.root.clone();
                lazi.search = None;
                reveal(term, lazi, &root.join(path))?;
            }
        }
        return Ok(None);
    }
    if let Some(top) = lazi.help {
        // The menu takes every key; ones it doesn't bind do nothing.
        lazi.pending.push(key);
        match files::lookup(&lazi.config.help_keys, &lazi.pending) {
            Lookup::Pending => {}
            Lookup::Unbound => lazi.pending.clear(),
            Lookup::Action(action) => {
                lazi.help = scroll(top, action, lazi.help_page);
                lazi.pending.clear();
            }
        }
        return Ok(None);
    }
    let len = lazi.panels.len();
    // While a panel takes text (a files prompt or its opener menu), digits and letters are typing,
    // not lazi's tab keys.
    let routed = if lazi.panels[lazi.focus].wants_text() {
        lazi.pending.clear();
        Route::Panel(vec![key])
    } else {
        route(&lazi.config.keys, &mut lazi.pending, key)
    };
    let next = match routed {
        Route::Wait => return Ok(None),
        Route::Lazi(Action::Quit) => return Ok(Some(Exit::Quit)),
        Route::Lazi(Action::Search) => {
            open_search(lazi, "");
            return Ok(None);
        }
        Route::Lazi(Action::Next) => (lazi.focus + 1) % len,
        Route::Lazi(Action::Prev) => (lazi.focus + len - 1) % len,
        Route::Lazi(Action::Focus(i)) => if *i < len { *i } else { lazi.focus },
        Route::Panel(keys) => {
            for key in keys {
                let outcome = lazi.panels[lazi.focus].key(term, key)?;
                if !matches!(outcome, Outcome::Continue) {
                    return answer(term, lazi, outcome);
                }
            }
            return Ok(None);
        }
    };
    focus(term, lazi, next)?;
    Ok(None)
}

impl Lazi {
    /// Records the focused files tab's folder as visited. Called when something is done there
    /// (a file opened, the tab left, a search started, lazi quit) rather than on every move, so
    /// folders only passed through on the way somewhere don't fill the list.
    fn settle(&mut self) {
        if let Some(files) = self.panels[self.focus].files() {
            session::remember(&mut self.folders, files.cwd());
        }
    }

    /// The open tabs, for the next start.
    fn session(&self) -> session::Session {
        let tabs = self
            .panels
            .iter()
            .map(|p| match p.files() {
                Some(files) => session::Tab::Files(files.cwd().to_path_buf()),
                None => session::Tab::Other(p.name().to_owned()),
            })
            .collect();
        session::Session { tabs }
    }
}

/// Does what a panel's key asked of lazi.
fn answer(term: &mut DefaultTerminal, lazi: &mut Lazi, outcome: Outcome) -> io::Result<Option<Exit>> {
    let len = lazi.panels.len();
    match outcome {
        Outcome::Continue => {}
        Outcome::Open(dir) => open(term, lazi, &dir)?,
        Outcome::Help => lazi.help = Some(0),
        Outcome::NewTab(dir) => {
            // Errors here would be the files tab's config, which already loaded for the first tab, or a
            // directory that vanished; either way there's simply no new tab.
            if let Ok(files) = Files::new(None, Some(dir)) {
                lazi.panels.insert(lazi.focus + 1, Panel::Files(Box::new(files)));
                focus(term, lazi, lazi.focus + 1)?;
            }
        }
        Outcome::NextTab => focus(term, lazi, (lazi.focus + 1) % len)?,
        Outcome::PrevTab => focus(term, lazi, (lazi.focus + len - 1) % len)?,
        Outcome::Quit => {
            // q on tab one (the first files tab, the one that follows where lazi starts) quits;
            // on any other files tab it closes just that tab.
            if lazi.panels.iter().position(|p| matches!(p, Panel::Files(_))) == Some(lazi.focus) {
                return Ok(Some(Exit::Quit));
            }
            lazi.settle();
            lazi.panels[lazi.focus].hide(term.backend_mut())?;
            lazi.panels.remove(lazi.focus);
            // The tab to the left takes over, as in yazi.
            lazi.focus = lazi.focus.saturating_sub(1);
            lazi.panels[lazi.focus].show();
        }
        Outcome::QuitNoCwd => return Ok(Some(Exit::QuitNoCwd)),
        Outcome::Search(query) => open_search(lazi, &query),
        Outcome::Edit(file) => edit(term, lazi, file.parent().unwrap_or(Path::new("/")), &file, None)?,
        Outcome::Reveal(file) => reveal(term, lazi, &file)?,
    }
    Ok(None)
}

/// Moves focus to panel `next`, taking the old one's images off the screen first.
fn focus(term: &mut DefaultTerminal, lazi: &mut Lazi, next: usize) -> io::Result<()> {
    if next != lazi.focus {
        lazi.settle();
        lazi.panels[lazi.focus].hide(term.backend_mut())?;
        lazi.focus = next;
        lazi.panels[next].show();
    }
    Ok(())
}

/// The files tab things go to: the focused one, else tab 1.
fn files_tab(lazi: &Lazi) -> Option<usize> {
    if lazi.panels[lazi.focus].files().is_some() { Some(lazi.focus) } else { lazi.panels.iter().position(|p| p.files().is_some()) }
}

/// Shows `dir` in a files tab and focuses it. Without a files panel it does nothing.
fn open(term: &mut DefaultTerminal, lazi: &mut Lazi, dir: &Path) -> io::Result<()> {
    let Some(i) = files_tab(lazi) else { return Ok(()) };
    if let Panel::Files(files) = &mut lazi.panels[i] {
        files.goto(dir);
    }
    focus(term, lazi, i)
}

/// Shows `path` in a files tab, the cursor on it, and focuses that tab.
fn reveal(term: &mut DefaultTerminal, lazi: &mut Lazi, path: &Path) -> io::Result<()> {
    let Some(i) = files_tab(lazi) else { return Ok(()) };
    if let Panel::Files(files) = &mut lazi.panels[i] {
        files.reveal(path);
    }
    focus(term, lazi, i)
}

/// Opens the search box, with `query` already typed.
fn open_search(lazi: &mut Lazi, query: &str) {
    lazi.settle();
    let (root, repos) = search_root(lazi);
    match Search::open(lazi.config.search.clone(), root, repos, lazi.folders.clone()) {
        Ok(mut search) => {
            search.set_query(query);
            lazi.search = Some(search);
        }
        // An eventfd lazi couldn't make; there's nowhere better to say so.
        Err(e) => eprintln!("lazi: search: {e}"),
    }
}

/// Opens `file` in the editor (at `line`, if given) from `dir`, and remembers it for the
/// dashboard.
fn edit(term: &mut DefaultTerminal, lazi: &mut Lazi, dir: &Path, file: &Path, line: Option<u64>) -> io::Result<()> {
    let spec = &lazi.config.search;
    let (script, args) = match line {
        Some(n) => (&spec.open_line, vec![file.to_path_buf(), PathBuf::from(n.to_string())]),
        None => (&spec.open, vec![file.to_path_buf()]),
    };
    let cmd = Cmd { desc: script, script, args: &args, block: true };
    // Blocking, so a failure is only the editor's own exit status: nothing to keep.
    let _ = files::run(term, &cmd, dir, Box::new(|_| {}))?;
    session::remember(&mut lazi.files, file);
    Ok(())
}

/// Where the search box looks, and the repos it offers: under the focused files tab, or from the
/// git tab under its selected repo.
fn search_root(lazi: &Lazi) -> (PathBuf, Vec<PathBuf>) {
    let repos: Vec<PathBuf> = lazi.panels.iter().flat_map(Panel::repos).collect();
    let root = match &lazi.panels[lazi.focus] {
        Panel::Git(git) => git.selected(),
        Panel::Files(files) => Some(files.cwd().to_path_buf()),
        Panel::Dashboard(_) => None,
    };
    let root = root.or_else(|| lazi.panels.iter().find_map(|p| p.files()).map(|l| l.cwd().to_path_buf())).unwrap_or_else(|| PathBuf::from("."));
    (root, repos)
}

/// The tab bar on the top row, the focused panel below it, as one synchronized update so the
/// terminal never shows a half-drawn frame.
fn draw(term: &mut DefaultTerminal, lazi: &mut Lazi) -> io::Result<()> {
    queue!(term.backend_mut(), BeginSynchronizedUpdate)?;
    term.draw(|frame| {
        let area = frame.area();
        if area.height < 2 {
            return;
        }
        tabs(frame.buffer_mut(), Rect { height: 1, ..area }, lazi);
        let body = Rect { y: area.y + 1, height: area.height - 1, ..area };
        if let Panel::Dashboard(dash) = &mut lazi.panels[lazi.focus] {
            // Files since deleted or moved are left out rather than offered.
            dash.files = lazi.files.iter().filter(|f| f.is_file()).take(dash.limit()).cloned().collect();
            dash.folders = lazi.folders.iter().filter(|d| d.is_dir()).take(dash.limit()).cloned().collect();
        }
        lazi.panels[lazi.focus].draw(frame, body);
        if lazi.help.is_some() {
            help(frame.buffer_mut(), body, lazi);
        }
        if let Some(search) = &mut lazi.search {
            search.draw(frame.buffer_mut(), body);
        }
    })?;
    if lazi.help.is_some() || lazi.search.is_some() {
        // An image would sit on top of the menu.
        lazi.panels[lazi.focus].hide(term.backend_mut())?;
    } else {
        lazi.panels[lazi.focus].sync_image(term.backend_mut())?;
    }
    execute!(term.backend_mut(), EndSynchronizedUpdate)
}

/// The key-binding menu over the panel: lazi's own keys, then the focused panel's, one
/// scrollable column.
fn help(buf: &mut Buffer, area: Rect, lazi: &mut Lazi) {
    let style = &lazi.config.style;
    let ours: Vec<(String, String)> = lazi.config.keys.iter().map(|(keys, action)| (files::key_label(keys), format!("{action:?}"))).collect();
    let panel = &lazi.panels[lazi.focus];
    let mut lines: Vec<Line> = Vec::new();
    for (title, entries) in [("lazi", ours), (panel.name(), panel.help())] {
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
    let top = lazi.help.unwrap_or(0).min(lines.len().saturating_sub(page));
    lazi.help = Some(top);
    lazi.help_page = page;
    let shown = format!(" keys · {}-{} of {} · q closes ", top + 1, (top + page).min(lines.len()), lines.len());
    let block = Block::bordered().title(shown);
    let inner = block.inner(area);
    block.render(area, buf);
    for (row, line) in lines.iter().skip(top).take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + row as u16, line, inner.width);
    }
}

/// Panel names with the focused one highlighted, and at the right any lazi prefix being typed.
fn tabs(buf: &mut Buffer, area: Rect, lazi: &Lazi) {
    let style = &lazi.config.style;
    let mut x = area.x;
    for (i, panel) in lazi.panels.iter().enumerate() {
        let tab = if i == lazi.focus { style.tab_focused } else { style.tab };
        // Numbered, since the number keys pick tabs.
        (x, _) = buf.set_stringn(x, area.y, format!(" {} {} ", i + 1, panel.name()), area.right().saturating_sub(x) as usize, tab);
    }
    if !lazi.pending.is_empty() {
        let typed: Vec<String> = lazi.pending.iter().map(|&key| key_name(key)).collect();
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
        assert_eq!(route(&bound, &mut pending, B), Route::Lazi(&Action::Next));
        assert!(pending.is_empty());
        // A prefix that leads nowhere hands every key typed, in order, to the panel.
        assert_eq!(route(&bound, &mut pending, CTRL_X), Route::Wait);
        assert_eq!(route(&bound, &mut pending, J), Route::Panel(vec![CTRL_X, J]));
        assert!(pending.is_empty());
    }
}
