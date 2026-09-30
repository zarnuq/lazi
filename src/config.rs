//! Compile-time configuration. Edit and rebuild.

use std::time::Duration;

use ratatui::crossterm::event::KeyCode::{self, Down, Esc, Left, PageDown, PageUp, Right, Up};
use ratatui::crossterm::event::KeyModifiers;
use ratatui::style::{Color, Modifier, Style};

use crate::open::Kind;

pub const SHOW_HIDDEN: bool = true;
/// Reads text for `cc` and friends on stdin.
pub const CLIPBOARD: &[&str] = &["wl-copy"];
pub const SCROLLOFF: usize = 5;
/// Ask before trashing. Permanent deletes always ask.
pub const CONFIRM_TRASH: bool = true;
/// Width ratio of the parent, current and preview columns.
pub const RATIO: [u16; 3] = [2, 5, 8];

/// How long a frame waits for directory reads before drawing without them.
pub const LOAD_GRACE: Duration = Duration::from_millis(10);
/// How often to check on reads still running after that.
pub const LOAD_POLL: Duration = Duration::from_millis(5);
/// How often to redraw while file operations run.
pub const TASK_POLL: Duration = Duration::from_millis(100);
/// Minimum gap between progress reports from a file operation.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(50);
/// How long to keep an eye out for a detached program failing right after it starts.
pub const FAIL_WATCH: Duration = Duration::from_secs(3);
/// Cached listings beyond this are dropped, except the ones on screen.
pub const CACHE_MAX: usize = 256;

pub const HEADER: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
pub const DIR: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
pub const LINK: Style = Style::new().fg(Color::Cyan);
pub const CURSOR: Style = Style::new().add_modifier(Modifier::REVERSED);
pub const ERROR: Style = Style::new().fg(Color::Red);
pub const DIM: Style = Style::new().add_modifier(Modifier::DIM);
pub const FIND: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::UNDERLINED);
/// The bar left of a marked entry.
pub const MARK_SELECTED: Style = Style::new().bg(Color::Yellow);
pub const MARK_COPIED: Style = Style::new().bg(Color::Green);
pub const MARK_CUT: Style = Style::new().bg(Color::Red);

#[derive(Clone, Copy)]
pub enum Action {
    Quit,
    Move(isize),
    /// Move by a percentage of the visible rows.
    Page(isize),
    Top,
    Bottom,
    Leave,
    Enter,
    Back,
    Forward,
    ToggleHidden,
    /// Go to a directory; a leading `~` is $HOME.
    Goto(&'static str),
    Open,
    /// Pick from all the openers for the hovered file.
    OpenWith,
    /// Quit without writing the cwd file, so the shell stays where it was.
    QuitNoCwd,
    /// Toggle the hovered entry's selection and move down.
    ToggleSelect,
    SelectAll,
    InvertSelection,
    /// Visual mode; `true` unselects the range instead.
    Visual(bool),
    /// Leave visual mode, or clear the selection.
    Escape,
    /// Yank the targets; `true` cuts.
    Yank(bool),
    Unyank,
    /// Paste the yanked files here; `true` overwrites instead of renaming.
    Paste(bool),
    /// Trash the targets; `true` deletes permanently.
    Remove(bool),
    /// Create a file, or a directory if the name ends in `/`.
    Create,
    Rename,
    /// Incremental find; `true` searches upwards.
    Find(bool),
    /// Jump to the next find match; `true` for the previous one.
    FindNext(bool),
    /// Narrow the listing to names matching as you type.
    Filter,
    /// Prompt for a shell command; `true` hands it the terminal and waits.
    Shell(bool),
    /// Run a fixed command with the terminal handed over.
    Run(&'static str),
    /// Prompt for a directory to go to.
    Cd,
    /// Run a picker (fzf, zoxide) and go to what it prints.
    Jump(&'static str),
    CopyPath(Part),
    Suspend,
}

/// Which part of the targets' paths to copy.
#[derive(Clone, Copy)]
pub enum Part {
    Path,
    Dir,
    Name,
    Stem,
}

pub struct Opener {
    pub desc: &'static str,
    /// A `sh -c` snippet; the files are "$@".
    pub run: &'static str,
    /// Hand the terminal over and wait, instead of detaching.
    pub block: bool,
}

const fn block(desc: &'static str, run: &'static str) -> Opener {
    Opener { desc, run, block: true }
}
const fn detach(desc: &'static str, run: &'static str) -> Opener {
    Opener { desc, run, block: false }
}

const SHELL: Opener = block("Shell here", r#"cd "$1" && exec "$SHELL""#);
const EDIT: Opener = block("$EDITOR", r#"${EDITOR:-nvim} "$@""#);
const OPEN: Opener = detach("Open", r#"xdg-open "$1""#);
const REVEAL: Opener = detach("Reveal", r#"xdg-open "$(dirname "$1")""#);
const VIEW: Opener = detach("View", r#"swayimg "$1""#);
const SETBG: Opener = detach("Set wallpaper", r#"qs ipc call wallpaper set "$1""#);
const READ: Opener = detach("Read", r#"zathura "$1""#);
const PLAY: Opener = detach("Play", r#"mpv --force-window "$@""#);
const MEDIAINFO: Opener = block("Media info", r#"mediainfo "$1"; echo "Press enter to exit"; read _"#);
const EXTRACT: Opener = detach("Extract here", r#"for f; do bsdtar -xf "$f"; done"#);

/// `o` runs the first opener; `O` offers them all.
pub fn openers(kind: Kind) -> &'static [Opener] {
    match kind {
        Kind::Dir => &[SHELL, EDIT, REVEAL],
        Kind::Image => &[VIEW, SETBG, OPEN, REVEAL],
        Kind::Pdf => &[READ, OPEN, REVEAL],
        Kind::Media => &[PLAY, MEDIAINFO, REVEAL],
        Kind::Archive => &[EXTRACT, OPEN, REVEAL],
        Kind::Text => &[EDIT, REVEAL],
        Kind::Other => &[OPEN, REVEAL],
    }
}

pub type Key = (KeyCode, KeyModifiers);

const fn key(c: char) -> Key {
    (KeyCode::Char(c), KeyModifiers::NONE)
}
const fn ctrl(c: char) -> Key {
    (KeyCode::Char(c), KeyModifiers::CONTROL)
}
const fn code(c: KeyCode) -> Key {
    (c, KeyModifiers::NONE)
}
const fn shift(c: KeyCode) -> Key {
    (c, KeyModifiers::SHIFT)
}

pub const KEYMAP: &[(&[Key], Action)] = &[
    (&[key('q')], Action::Quit),
    (&[key('Q')], Action::QuitNoCwd),
    (&[ctrl('c')], Action::Quit),
    (&[key('k')], Action::Move(-1)),
    (&[key('j')], Action::Move(1)),
    (&[code(Up)], Action::Move(-1)),
    (&[code(Down)], Action::Move(1)),
    (&[ctrl('u')], Action::Page(-50)),
    (&[ctrl('d')], Action::Page(50)),
    (&[ctrl('b')], Action::Page(-100)),
    (&[ctrl('f')], Action::Page(100)),
    (&[shift(PageUp)], Action::Page(-50)),
    (&[shift(PageDown)], Action::Page(50)),
    (&[code(PageUp)], Action::Page(-100)),
    (&[code(PageDown)], Action::Page(100)),
    (&[key('g'), key('g')], Action::Top),
    (&[key('G')], Action::Bottom),
    (&[key('h')], Action::Leave),
    (&[key('l')], Action::Enter),
    (&[code(Left)], Action::Leave),
    (&[code(Right)], Action::Enter),
    (&[key('H')], Action::Back),
    (&[key('L')], Action::Forward),
    (&[key('.')], Action::ToggleHidden),
    (&[key('g'), key('h')], Action::Goto("~")),
    (&[key('g'), key('c')], Action::Goto("~/.config")),
    (&[key('g'), key('d')], Action::Goto("~/Downloads")),
    (&[key('o')], Action::Open),
    (&[code(KeyCode::Enter)], Action::Open),
    (&[key('O')], Action::OpenWith),
    (&[key(' ')], Action::ToggleSelect),
    (&[ctrl('a')], Action::SelectAll),
    (&[ctrl('r')], Action::InvertSelection),
    (&[key('v')], Action::Visual(false)),
    (&[key('V')], Action::Visual(true)),
    (&[code(Esc)], Action::Escape),
    (&[key('y')], Action::Yank(false)),
    (&[key('x')], Action::Yank(true)),
    (&[key('Y')], Action::Unyank),
    (&[key('X')], Action::Unyank),
    (&[key('p')], Action::Paste(false)),
    (&[key('P')], Action::Paste(true)),
    (&[key('d')], Action::Remove(false)),
    (&[key('D')], Action::Remove(true)),
    (&[key('a')], Action::Create),
    (&[key('r')], Action::Rename),
    (&[key('/')], Action::Find(false)),
    (&[key('?')], Action::Find(true)),
    (&[key('n')], Action::FindNext(false)),
    (&[key('N')], Action::FindNext(true)),
    (&[key('f')], Action::Filter),
    (&[key(';')], Action::Shell(false)),
    (&[key(':')], Action::Shell(true)),
    (&[key('!')], Action::Run("pwsh")),
    (&[key('s')], Action::Cd),
    (&[key('z')], Action::Jump("fzf")),
    (&[key('Z')], Action::Jump("zoxide query -i")),
    (&[key('c'), key('c')], Action::CopyPath(Part::Path)),
    (&[key('c'), key('d')], Action::CopyPath(Part::Dir)),
    (&[key('c'), key('f')], Action::CopyPath(Part::Name)),
    (&[key('c'), key('n')], Action::CopyPath(Part::Stem)),
    (&[ctrl('z')], Action::Suspend),
];

pub enum Lookup {
    Action(Action),
    /// `keys` starts a longer binding; wait for the next key.
    Pending,
    Unbound,
}

pub fn lookup(keys: &[Key]) -> Lookup {
    let mut prefix = false;
    for (bound, action) in KEYMAP {
        if *bound == keys {
            return Lookup::Action(*action);
        }
        prefix |= bound.starts_with(keys);
    }
    if prefix { Lookup::Pending } else { Lookup::Unbound }
}
