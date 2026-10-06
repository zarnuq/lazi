mod app;
mod config;
mod fs;
mod input;
mod kitty;
mod open;
mod ops;
mod preview;
mod ui;
mod wake;
mod watch;

use std::os::unix::ffi::OsStrExt;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, io, process};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::crossterm::{execute, queue};

use app::{App, Menu, Prompt};
use config::{Action, Key, Lookup, MenuAction, Opener, PromptAction};
use open::Cmd;

/// How long a frame waits for directory reads before drawing without them.
const LOAD_GRACE: Duration = Duration::from_millis(10);

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
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    if let Err(e) = config::load(config) {
        eprintln!("lazi: {e}");
        process::exit(1);
    }
    let cwd = match dir {
        Some(dir) => dir.canonicalize()?,
        None => env::current_dir()?,
    };

    let mut app = App::new(cwd)?;
    let mut term = ratatui::init();
    let res = run(&mut term, &mut app, bench.then_some(start));
    let _ = app.kitty.clear(term.backend_mut());
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
    // crossterm reads keys from stdin if it's a terminal, else from /dev/tty; sleep on the same.
    let tty_file;
    // SAFETY: isatty has no preconditions.
    let tty = if unsafe { libc::isatty(0) } == 1 {
        0
    } else {
        tty_file = File::open("/dev/tty")?;
        tty_file.as_raw_fd()
    };
    // A resize has to wake the sleep too; the next draw picks up the new size.
    let (winch, winch_tx) = UnixStream::pair()?;
    winch.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, winch_tx)?;
    let mut fds = vec![tty, winch.as_raw_fd()];
    fds.extend(app.wake_fds());

    let mut pending: Vec<Key> = Vec::new();
    let mut dirty = true;
    loop {
        if dirty {
            // Hold the frame briefly so fast reads land in it instead of flashing an empty column.
            // A benchmark waits for everything, so it measures a complete frame.
            app.receive(if bench.is_some() { None } else { Some(LOAD_GRACE) });
            // Every frame, since programs lazi hands the terminal to may have changed it.
            queue!(term.backend_mut(), SetTitle(format!("lazi: {}", ui::header(&app.cwd))))?;
            draw(term, app)?;
            // The first frame, or a resize, may have changed the preview area.
            app.request_preview();
            dirty = false;
            if let Some(start) = bench {
                return Ok(Exit::Bench(start.elapsed()));
            }
        }

        // Sleep until a key, a worker thread, a filesystem change or a resize, unless crossterm
        // already has input buffered from an earlier read.
        if !event::poll(Duration::ZERO)? {
            wake::wait(&fds)?;
        }
        dirty |= wake::drain(winch.as_raw_fd());
        dirty |= app.on_wake();
        while event::poll(Duration::ZERO)? {
            dirty = true;
            if let Some(exit) = handle(term, app, &mut pending, event::read()?)? {
                return Ok(exit);
            }
        }
        dirty |= app.receive(Some(Duration::ZERO));
    }
}

fn handle(term: &mut DefaultTerminal, app: &mut App, pending: &mut Vec<Key>, event: Event) -> io::Result<Option<Exit>> {
    let Event::Key(key) = event else { return Ok(None) };
    if key.kind != KeyEventKind::Press {
        return Ok(None);
    }
    app.error = None;
    app.info = None;
    let key = normalize(key);
    if app.prompt.is_some() {
        prompt_key(app, key);
        return Ok(None);
    }
    if app.menu.is_some() {
        menu_key(term, app, key)?;
        return Ok(None);
    }
    pending.push(key);
    match config::lookup(pending) {
        Lookup::Pending => Ok(None),
        Lookup::Unbound => {
            pending.clear();
            Ok(None)
        }
        Lookup::Action(action) => {
            pending.clear();
            // Pick up outside changes first, so e.g. `gg` lands on a file created since the last
            // read. inotify usually got there already; this covers filesystems it doesn't.
            app.refresh();
            app.receive(Some(LOAD_GRACE));
            apply(term, app, action)
        }
    }
}

fn apply(term: &mut DefaultTerminal, app: &mut App, action: &Action) -> io::Result<Option<Exit>> {
    match action {
        Action::Quit => return Ok(Some(Exit::Quit)),
        Action::QuitNoCwd => return Ok(Some(Exit::QuitNoCwd)),
        Action::Move(delta) => app.move_by(*delta),
        Action::Page(percent) => app.page(*percent),
        Action::Top => app.move_to(0),
        Action::Bottom => app.move_to(usize::MAX),
        Action::Leave => app.leave(),
        Action::Enter => app.enter(),
        Action::Back => app.back(),
        Action::Forward => app.forward(),
        Action::ToggleHidden => app.toggle_hidden(),
        Action::CycleSort => app.cycle_sort(),
        Action::ReverseSort => app.reverse_sort(),
        Action::Goto(dir) => app.goto(dir),
        Action::Open | Action::OpenWith => {
            let files = app.targets();
            if let Some(first) = files.first() {
                // The first target decides the rule, like yazi.
                let openers = open::rule(first, first.is_dir()).map_or(&[][..], |rule| &rule.openers);
                if openers.is_empty() {
                    app.error = Some(format!("no opener for {}", first.display()));
                } else if let Action::Open = action {
                    run_opener(term, app, &openers[0], &files)?;
                } else {
                    app.menu = Some(Menu { openers, files, cursor: 0 });
                }
            }
        }
        Action::ToggleSelect => app.toggle_select(),
        Action::SelectAll => app.select_all(false),
        Action::InvertSelection => app.select_all(true),
        Action::Escape => app.escape(),
        Action::Yank => app.yank(false),
        Action::Cut => app.yank(true),
        Action::Unyank => app.unyank(),
        Action::Paste => app.paste(false),
        Action::PasteOverwrite => app.paste(true),
        Action::Trash => app.remove(false),
        Action::Delete => app.remove(true),
        Action::Create => app.start_create(),
        Action::Rename => app.start_rename(),
        Action::Find => app.start_find(false),
        Action::FindBack => app.start_find(true),
        Action::FindNext => app.find_next(false),
        Action::FindPrev => app.find_next(true),
        Action::Filter => app.start_filter(),
        Action::CopyPath(part) => app.copy_path(*part),
        Action::Seek(delta) => app.seek(*delta),
        Action::Run { run, block, reveal } => {
            let files = app.targets();
            if *reveal {
                app.kitty.clear(term.backend_mut())?;
                if let Some(out) = open::capture(term, run, &files, &app.cwd)? {
                    app.reveal(Path::new(&out));
                }
            } else {
                run_cmd(term, app, &Cmd { desc: run, script: run, args: &files, block: *block })?;
            }
        }
        Action::Suspend => {
            app.kitty.clear(term.backend_mut())?;
            open::suspend(term)?;
            // Raw mode turned off the terminal's own ^Z handling, so stop ourselves; the shell's
            // `fg` resumes here.
            // SAFETY: raising a signal on ourselves has no memory-safety preconditions.
            unsafe { libc::raise(libc::SIGTSTP) };
            open::resume(term)?;
        }
    }
    Ok(None)
}

/// Keys while the `O` menu is up.
fn menu_key(term: &mut DefaultTerminal, app: &mut App, key: Key) -> io::Result<()> {
    let Some(menu) = &mut app.menu else { return Ok(()) };
    let len = menu.openers.len();
    let pick = match config::get().keys.menu.get(&key) {
        Some(MenuAction::Down) => {
            menu.cursor = (menu.cursor + 1) % len;
            None
        }
        Some(MenuAction::Up) => {
            menu.cursor = (menu.cursor + len - 1) % len;
            None
        }
        Some(MenuAction::Accept) => Some(menu.cursor),
        Some(MenuAction::Pick(n)) => n.checked_sub(1).filter(|&i| i < len),
        Some(MenuAction::Cancel) => {
            app.menu = None;
            None
        }
        None => None,
    };
    if let Some(i) = pick
        && let Some(menu) = app.menu.take()
    {
        run_opener(term, app, &menu.openers[i], &menu.files)?;
    }
    Ok(())
}

/// Keys while a prompt is open: a confirmation takes its yes keys and treats anything else as
/// no; the rest edit text, with unbound printable keys typing themselves.
fn prompt_key(app: &mut App, key: Key) {
    let keys = &config::get().keys;
    let Some(prompt) = &mut app.prompt else { return };
    if let Prompt::Confirm { .. } = prompt {
        if keys.confirm.keys.contains(&key) { app.submit() } else { app.cancel_prompt() }
        return;
    }
    let changed = match keys.prompt.get(&key) {
        Some(PromptAction::Submit) => return app.submit(),
        Some(PromptAction::Cancel) => return app.cancel_prompt(),
        Some(PromptAction::Complete) => return app.find_complete(),
        Some(PromptAction::NextMatch) => return app.cycle_match(false),
        Some(PromptAction::PrevMatch) => return app.cycle_match(true),
        Some(edit) => prompt.input_mut().is_some_and(|input| input.edit(edit)),
        None => match key {
            (KeyCode::Char(c), mods) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) => {
                prompt.input_mut().map(|input| input.insert(c)).is_some()
            }
            _ => false,
        },
    };
    if changed {
        app.prompt_changed();
    }
}

fn run_opener(term: &mut DefaultTerminal, app: &mut App, opener: &Opener, files: &[PathBuf]) -> io::Result<()> {
    let cmd = Cmd { desc: &opener.desc, script: &opener.run, args: files, block: opener.block };
    run_cmd(term, app, &cmd)
}

fn run_cmd(term: &mut DefaultTerminal, app: &mut App, cmd: &Cmd) -> io::Result<()> {
    if cmd.block {
        app.kitty.clear(term.backend_mut())?;
    }
    let on_fail = app.on_fail();
    app.error = open::run(term, cmd, &app.cwd, on_fail)?;
    // A blocking program (an editor, a shell) may have changed what's on disk.
    app.refresh();
    Ok(())
}

fn normalize(key: KeyEvent) -> Key {
    let mut mods = key.modifiers;
    // A char already says whether shift was held, so 'G' binds as 'G', not shift+'G'. BackTab
    // likewise is already shift+Tab.
    if let KeyCode::Char(_) | KeyCode::BackTab = key.code {
        mods.remove(KeyModifiers::SHIFT);
    }
    (key.code, mods)
}

/// Wraps each frame in a synchronized update so the terminal never shows a half-drawn frame.
fn draw(term: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    queue!(term.backend_mut(), BeginSynchronizedUpdate)?;
    term.draw(|frame| ui::draw(frame, app))?;
    app.sync_image(term.backend_mut())?;
    execute!(term.backend_mut(), EndSynchronizedUpdate)
}
