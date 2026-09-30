mod app;
mod config;
mod fs;
mod input;
mod open;
mod ops;
mod ui;

use std::os::unix::ffi::OsStrExt;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{env, io};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::crossterm::{execute, queue};

use app::{App, Menu, Prompt};
use config::{Action, Key, LOAD_GRACE, Lookup, Opener};

enum Exit {
    Quit,
    QuitNoCwd,
    /// Time to first frame.
    Bench(Duration),
}

fn main() -> io::Result<()> {
    let start = Instant::now();
    let mut bench = false;
    let mut cwd_file = None;
    let mut dir = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--bench" {
            bench = true;
        } else if arg == "--cwd-file" {
            cwd_file = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--cwd-file=") {
            cwd_file = Some(PathBuf::from(OsStr::from_bytes(path)));
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    let cwd = match dir {
        Some(dir) => dir.canonicalize()?,
        None => env::current_dir()?,
    };

    let mut app = App::new(cwd);
    let mut term = ratatui::init();
    let res = run(&mut term, &mut app, bench.then_some(start));
    ratatui::restore();

    match res? {
        Exit::Quit => {
            // For a shell wrapper to cd into where lazi left off.
            if let Some(path) = cwd_file {
                std::fs::write(path, app.cwd.as_os_str().as_bytes())?;
            }
        }
        Exit::QuitNoCwd => {}
        Exit::Bench(elapsed) => eprintln!("first frame: {elapsed:?} ({} entries)", app.entries().len()),
    }
    Ok(())
}

fn run(term: &mut DefaultTerminal, app: &mut App, bench: Option<Instant>) -> io::Result<Exit> {
    let mut pending: Vec<Key> = Vec::new();
    let mut title = PathBuf::new();
    let mut dirty = true;
    loop {
        if dirty {
            // Hold the frame briefly so fast reads land in it instead of flashing an empty column.
            // A benchmark waits for everything, so it measures a complete frame.
            app.receive(if bench.is_some() { None } else { Some(LOAD_GRACE) });
            if title != app.cwd {
                title.clone_from(&app.cwd);
                queue!(term.backend_mut(), SetTitle(format!("lazi: {}", ui::header(&title))))?;
            }
            draw(term, app)?;
            dirty = false;
            if let Some(start) = bench {
                return Ok(Exit::Bench(start.elapsed()));
            }
        }

        // Block on input, but wake up periodically while background work is running to pick it up.
        let ready = match app.poll_timeout() {
            None => true,
            Some(timeout) => event::poll(timeout)?,
        };
        if ready {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.error = None;
                    dirty = true;
                    let key = normalize(key);
                    if app.prompt.is_some() {
                        prompt_key(app, key);
                        continue;
                    }
                    if app.menu.is_some() {
                        menu_key(term, app, key)?;
                        continue;
                    }
                    pending.push(key);
                    match config::lookup(&pending) {
                        Lookup::Pending => {}
                        Lookup::Unbound => pending.clear(),
                        Lookup::Action(action) => {
                            pending.clear();
                            // Pick up outside changes first, so e.g. `gg` lands on a file created since the last read.
                            app.refresh();
                            app.receive(Some(LOAD_GRACE));
                            if let Some(exit) = apply(term, app, action)? {
                                return Ok(exit);
                            }
                        }
                    }
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        dirty |= app.receive(Some(Duration::ZERO));
    }
}

fn apply(term: &mut DefaultTerminal, app: &mut App, action: Action) -> io::Result<Option<Exit>> {
    match action {
        Action::Quit => return Ok(Some(Exit::Quit)),
        Action::QuitNoCwd => return Ok(Some(Exit::QuitNoCwd)),
        Action::Move(delta) => app.move_by(delta),
        Action::Page(percent) => app.page(percent),
        Action::Top => app.move_to(0),
        Action::Bottom => app.move_to(usize::MAX),
        Action::Leave => app.leave(),
        Action::Enter => app.enter(),
        Action::Back => app.back(),
        Action::Forward => app.forward(),
        Action::ToggleHidden => app.toggle_hidden(),
        Action::Goto(dir) => app.goto(dir),
        Action::Open | Action::OpenWith => {
            let files = app.targets();
            if let Some(first) = files.first() {
                // The first target decides the kind, like yazi.
                let openers = config::openers(open::kind(first, first.is_dir()));
                if let Action::Open = action {
                    run_opener(term, app, &openers[0], &files)?;
                } else {
                    app.menu = Some(Menu { openers, files, cursor: 0 });
                }
            }
        }
        Action::ToggleSelect => app.toggle_select(),
        Action::SelectAll => app.select_all(false),
        Action::InvertSelection => app.select_all(true),
        Action::Visual(unset) => app.visual(unset),
        Action::Escape => app.escape(),
        Action::Yank(cut) => app.yank(cut),
        Action::Unyank => app.unyank(),
        Action::Paste(force) => app.paste(force),
        Action::Remove(permanently) => app.remove(permanently),
        Action::Create => app.start_create(),
        Action::Rename => app.start_rename(),
    }
    Ok(None)
}

/// Keys while the `O` menu is up: j/k to move, Enter/o/l or a number to run, Esc/q/h to cancel.
fn menu_key(term: &mut DefaultTerminal, app: &mut App, key: Key) -> io::Result<()> {
    let Some(menu) = &mut app.menu else { return Ok(()) };
    let len = menu.openers.len();
    let pick = match key.0 {
        KeyCode::Char('j') | KeyCode::Down => {
            menu.cursor = (menu.cursor + 1) % len;
            None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            menu.cursor = (menu.cursor + len - 1) % len;
            None
        }
        KeyCode::Enter | KeyCode::Char('o' | 'l') => Some(menu.cursor),
        KeyCode::Char(c @ '1'..='9') => Some(c as usize - '1' as usize).filter(|&i| i < len),
        KeyCode::Esc | KeyCode::Char('q' | 'h') => {
            app.menu = None;
            None
        }
        _ => None,
    };
    if let Some(i) = pick
        && let Some(menu) = app.menu.take()
    {
        run_opener(term, app, &menu.openers[i], &menu.files)?;
    }
    Ok(())
}

/// Keys while a prompt is open: y/n for a confirmation, text editing for the rest.
fn prompt_key(app: &mut App, key: Key) {
    match (&mut app.prompt, key.0) {
        (Some(Prompt::Confirm { .. }), KeyCode::Char('y' | 'Y')) => app.submit(),
        (Some(Prompt::Confirm { .. }), _) | (_, KeyCode::Esc) => app.prompt = None,
        (_, KeyCode::Enter) => app.submit(),
        (Some(Prompt::Create(input) | Prompt::Rename { input, .. }), _) => {
            input.key(key);
        }
        (None, _) => {}
    }
}

fn run_opener(term: &mut DefaultTerminal, app: &mut App, opener: &Opener, files: &[PathBuf]) -> io::Result<()> {
    app.error = open::run(term, opener, files, &app.cwd)?;
    // A blocking program (an editor, a shell) may have changed what's on disk.
    app.refresh();
    Ok(())
}

fn normalize(key: KeyEvent) -> Key {
    let mut mods = key.modifiers;
    // A char already says whether shift was held, so 'G' binds as 'G', not shift+'G'.
    if let KeyCode::Char(_) = key.code {
        mods.remove(KeyModifiers::SHIFT);
    }
    (key.code, mods)
}

/// Wraps each frame in a synchronized update so the terminal never shows a half-drawn frame.
fn draw(term: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    queue!(term.backend_mut(), BeginSynchronizedUpdate)?;
    term.draw(|frame| ui::draw(frame, app))?;
    execute!(term.backend_mut(), EndSynchronizedUpdate)
}
