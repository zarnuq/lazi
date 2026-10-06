//! lazi as a library: `Lazi` is the file manager as a panel that some other program drives,
//! with the terminal and the event loop left to the caller. lazi's own binary is one caller,
//! shop another.

mod app;
mod config;
mod fs;
mod input;
mod kitty;
mod open;
mod ops;
mod preview;
mod ui;
pub mod wake;
mod watch;

use std::env;
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Frame};

use app::{App, Menu, Prompt};
use config::{Action, MenuAction, Opener, PromptAction};
use open::Cmd;

pub use config::{Key, Lookup, find, lookup, normalize, sequences, style};

/// How long a frame or a key waits for directory reads before going on without them.
pub const LOAD_GRACE: Duration = Duration::from_millis(10);

/// What a key asks of whoever runs the panel.
#[derive(PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
    /// Quit without writing the cwd file, so the shell stays where it was.
    QuitNoCwd,
}

pub struct Lazi {
    app: App,
    /// The keys of a sequence typed so far, like the first `g` of `gg`.
    pending: Vec<Key>,
}

impl Lazi {
    /// Loads lazi's config and reads `dir` (else the working directory) inline. The config is
    /// process-wide: the first `new` in a process decides it.
    pub fn new(config: Option<PathBuf>, dir: Option<PathBuf>) -> Result<Self, String> {
        config::load(config)?;
        let cwd = match dir {
            Some(dir) => dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?,
            None => env::current_dir().map_err(|e| e.to_string())?,
        };
        let app = App::new(cwd).map_err(|e| e.to_string())?;
        Ok(Self { app, pending: Vec::new() })
    }

    pub fn cwd(&self) -> &Path {
        &self.app.cwd
    }

    /// The cwd, shortened the way the header shows it.
    pub fn title(&self) -> String {
        ui::header(&self.app.cwd)
    }

    /// How many entries the current directory shows; `--bench` reports it.
    pub fn entry_count(&self) -> usize {
        self.app.entries().len()
    }

    /// What the caller's `wake::wait` should watch on lazi's behalf.
    pub fn wake_fds(&self) -> Vec<RawFd> {
        self.app.wake_fds()
    }

    /// Handles a wakeup on one of `wake_fds`. Returns whether a redraw is needed.
    pub fn on_wake(&mut self) -> bool {
        self.app.on_wake()
    }

    /// Takes in what workers finished, waiting up to `grace` (forever if None) for directory
    /// reads. Returns whether a redraw is needed.
    pub fn receive(&mut self, grace: Option<Duration>) -> bool {
        self.app.receive(grace)
    }

    /// Handles a key press, already `normalize`d. Takes the terminal because openers, fzf and
    /// suspend hand it to other programs.
    pub fn key(&mut self, term: &mut DefaultTerminal, key: Key) -> io::Result<Outcome> {
        let app = &mut self.app;
        app.error = None;
        app.info = None;
        if app.prompt.is_some() {
            prompt_key(app, key);
            return Ok(Outcome::Continue);
        }
        if app.menu.is_some() {
            menu_key(term, app, key)?;
            return Ok(Outcome::Continue);
        }
        self.pending.push(key);
        match config::lookup(&config::get().keys.normal, &self.pending) {
            Lookup::Pending => Ok(Outcome::Continue),
            Lookup::Unbound => {
                self.pending.clear();
                Ok(Outcome::Continue)
            }
            Lookup::Action(action) => {
                self.pending.clear();
                // Pick up outside changes first, so e.g. `gg` lands on a file created since the last
                // read. inotify usually got there already; this covers filesystems it doesn't.
                app.refresh();
                app.receive(Some(LOAD_GRACE));
                apply(term, app, action)
            }
        }
    }

    /// Draws into `area`, then asks for a preview sized to it: the first frame, or a resize,
    /// may have changed the preview column.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        ui::draw(frame, area, &mut self.app);
        self.app.request_preview();
    }

    /// Shows or hides the image preview to match the frame just drawn.
    pub fn sync_image(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.app.sync_image(out)
    }

    /// Takes lazi's image off the screen while another panel has it. The terminal keeps the
    /// image data, so coming back doesn't resend it.
    pub fn hide(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.app.kitty.sync(out, None)
    }

    /// Deletes every image, before the terminal is restored on exit.
    pub fn clear_images(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.app.kitty.clear(out)
    }
}

fn apply(term: &mut DefaultTerminal, app: &mut App, action: &Action) -> io::Result<Outcome> {
    match action {
        Action::Quit => return Ok(Outcome::Quit),
        Action::QuitNoCwd => return Ok(Outcome::QuitNoCwd),
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
    Ok(Outcome::Continue)
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
