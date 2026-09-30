//! Compile-time configuration. Edit and rebuild.

use std::time::Duration;

use ratatui::crossterm::event::KeyCode::{self, Down, Left, PageDown, PageUp, Right, Up};
use ratatui::crossterm::event::KeyModifiers;
use ratatui::style::{Color, Modifier, Style};

pub const SHOW_HIDDEN: bool = true;
pub const SCROLLOFF: usize = 5;
/// Width ratio of the parent, current and preview columns.
pub const RATIO: [u16; 3] = [2, 5, 8];

/// How long a frame waits for directory reads before drawing without them.
pub const LOAD_GRACE: Duration = Duration::from_millis(10);
/// How often to check on reads still running after that.
pub const LOAD_POLL: Duration = Duration::from_millis(5);
/// Cached listings beyond this are dropped, except the ones on screen.
pub const CACHE_MAX: usize = 256;

pub const HEADER: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
pub const DIR: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
pub const LINK: Style = Style::new().fg(Color::Cyan);
pub const CURSOR: Style = Style::new().add_modifier(Modifier::REVERSED);
pub const ERROR: Style = Style::new().fg(Color::Red);
pub const DIM: Style = Style::new().add_modifier(Modifier::DIM);

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
