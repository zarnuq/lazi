mod app;
mod config;
mod fs;
mod ui;

use std::time::{Duration, Instant};
use std::{env, io, path::PathBuf};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::crossterm::{execute, queue};

use app::App;
use config::{Action, Key, LOAD_GRACE, LOAD_POLL, Lookup};

fn main() -> io::Result<()> {
    let start = Instant::now();
    let mut bench = false;
    let mut dir = None;
    for arg in env::args_os().skip(1) {
        if arg == "--bench" {
            bench = true;
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

    if let Some(elapsed) = res? {
        eprintln!("first frame: {elapsed:?} ({} entries)", app.entries().len());
    }
    Ok(())
}

/// Returns the time to first frame when benchmarking, otherwise runs until quit.
fn run(term: &mut DefaultTerminal, app: &mut App, bench: Option<Instant>) -> io::Result<Option<Duration>> {
    let mut pending: Vec<Key> = Vec::new();
    let mut dirty = true;
    loop {
        if dirty {
            // Hold the frame briefly so fast reads land in it instead of flashing an empty column.
            // A benchmark waits for everything, so it measures a complete frame.
            app.receive(if bench.is_some() { None } else { Some(LOAD_GRACE) });
            draw(term, app)?;
            dirty = false;
            if let Some(start) = bench {
                return Ok(Some(start.elapsed()));
            }
        }

        // Block on input, but wake up periodically while reads are running to pick them up.
        if !app.is_loading() || event::poll(LOAD_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    pending.push(normalize(key));
                    match config::lookup(&pending) {
                        Lookup::Pending => {}
                        Lookup::Unbound => pending.clear(),
                        Lookup::Action(action) => {
                            pending.clear();
                            // Pick up outside changes first, so e.g. `gg` lands on a file created since the last read.
                            app.refresh();
                            app.receive(Some(LOAD_GRACE));
                            if !apply(app, action) {
                                return Ok(None);
                            }
                            dirty = true;
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

/// Returns false to quit.
fn apply(app: &mut App, action: Action) -> bool {
    match action {
        Action::Quit => return false,
        Action::Move(delta) => app.move_by(delta),
        Action::Page(percent) => app.page(percent),
        Action::Top => app.move_to(0),
        Action::Bottom => app.move_to(usize::MAX),
        Action::Leave => app.leave(),
        Action::Enter => app.enter(),
        Action::Back => app.back(),
        Action::Forward => app.forward(),
        Action::ToggleHidden => app.toggle_hidden(),
    }
    true
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
