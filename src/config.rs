//! The configuration, read once at startup from a RON file. Nothing has a built-in default: the
//! file is the whole truth, and a missing or invalid one stops lazi before it takes the screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use std::{env, fs};

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ron::extensions::Extensions;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub show_hidden: bool,
    /// Width ratio of the parent, current and preview columns.
    pub ratio: (u16, u16, u16),
    pub scrolloff: usize,
    /// Ask before trashing. Permanent deletes always ask.
    pub confirm_trash: bool,
    pub syntax_theme: String,
    pub tab_size: usize,
    /// Largest image size (pixels) sent to the terminal, whatever the preview area.
    pub image_max: (u32, u32),
    #[serde(deserialize_with = "seconds")]
    pub preview_timeout: Duration,
    /// Reads text on stdin; empty disables copying.
    pub clipboard: Vec<String>,
    /// Trashes a path given as the last argument, for files on another filesystem.
    pub trash_fallback: Vec<String>,
    pub style: Styles,
    pub openers: HashMap<String, Opener>,
    pub rules: Vec<Rule>,
    pub keys: Keys,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Styles {
    #[serde(deserialize_with = "style")]
    pub header: Style,
    #[serde(deserialize_with = "style")]
    pub dir: Style,
    #[serde(deserialize_with = "style")]
    pub link: Style,
    #[serde(deserialize_with = "style")]
    pub cursor: Style,
    #[serde(deserialize_with = "style")]
    pub error: Style,
    #[serde(deserialize_with = "style")]
    pub dim: Style,
    #[serde(deserialize_with = "style")]
    pub find: Style,
    /// The bar left of a marked entry.
    #[serde(deserialize_with = "style")]
    pub mark_selected: Style,
    #[serde(deserialize_with = "style")]
    pub mark_copied: Style,
    #[serde(deserialize_with = "style")]
    pub mark_cut: Style,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opener {
    pub desc: String,
    /// A `sh -c` snippet; the files are "$@".
    pub run: String,
    /// Hand the terminal over and wait, instead of detaching.
    #[serde(default)]
    pub block: bool,
}

/// Which files get which previewers and openers. Rules are tried in order and the first match
/// wins. Directories only match rules with `dir: true`, and files only rules without it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default)]
    pub dir: bool,
    /// Lowercase extensions; empty matches any.
    #[serde(default)]
    pub ext: Vec<String>,
    /// Only files with no NUL in their first KB.
    #[serde(default)]
    pub text: bool,
    /// Tried in order until one works.
    #[serde(default)]
    pub preview: Vec<Previewer>,
    /// Names from `openers`; `Open` runs the first.
    #[serde(default)]
    open: Vec<String>,
    #[serde(skip)]
    pub openers: Vec<Opener>,
}

/// How a file is previewed. Scripts get the file as "$1" and the area as $COLUMNS/$LINES.
#[derive(Deserialize)]
pub enum Previewer {
    /// The file's own text.
    Text,
    /// Decoded in process and shown with kitty's graphics protocol.
    Image,
    /// A script printing an image (e.g. PNG) to stdout.
    ImageCmd(String),
    /// A script whose output is shown as text; ANSI colours are kept.
    Cmd(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    #[serde(deserialize_with = "sequences")]
    pub normal: Vec<(Vec<Key>, Action)>,
    /// While the `O` menu is up.
    #[serde(deserialize_with = "singles")]
    pub menu: HashMap<Key, MenuAction>,
    /// While a text prompt is open. Unbound printable keys type themselves.
    #[serde(deserialize_with = "singles")]
    pub prompt: HashMap<Key, PromptAction>,
    /// Answers yes to a confirmation; any other key is no.
    pub confirm: Confirm,
}

#[derive(Deserialize)]
pub enum Action {
    Quit,
    /// Quit without writing the cwd file, so the shell stays where it was.
    QuitNoCwd,
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
    Goto(String),
    /// Run the first opener for the targets.
    Open,
    /// Pick from all the openers for the targets.
    OpenWith,
    /// Toggle the hovered entry's selection and move down.
    ToggleSelect,
    SelectAll,
    InvertSelection,
    /// Clear the selection, else the filter, else the find highlight.
    Escape,
    Yank,
    Cut,
    Unyank,
    /// Paste the yanked files here, renaming on a clash.
    Paste,
    PasteOverwrite,
    Trash,
    /// Delete permanently.
    Delete,
    /// Create a file, or a directory if the name ends in `/`.
    Create,
    Rename,
    Find,
    FindBack,
    FindNext,
    FindPrev,
    /// Narrow the listing to names matching as you type.
    Filter,
    CopyPath(Part),
    Suspend,
    /// Scroll the file preview by this many lines.
    Seek(isize),
    /// A `sh -c` snippet with the targets as "$@". `block` hands it the terminal; `reveal` also
    /// does, and then goes to the path it prints (as with fzf).
    Run {
        run: String,
        #[serde(default)]
        block: bool,
        #[serde(default)]
        reveal: bool,
    },
}

/// Which part of the targets' paths to copy.
#[derive(Clone, Copy, Deserialize)]
pub enum Part {
    Path,
    Dir,
    Name,
    Stem,
}

#[derive(Deserialize)]
pub enum MenuAction {
    Down,
    Up,
    Accept,
    /// Run the nth opener, counting from 1.
    Pick(usize),
    Cancel,
}

#[derive(Deserialize)]
pub enum PromptAction {
    Submit,
    Cancel,
    /// Tab completion in the find prompt.
    Complete,
    /// Move to the next find match without leaving the prompt, in the find's direction.
    NextMatch,
    PrevMatch,
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    /// Delete back to the previous space or slash.
    DeleteWord,
    KillToStart,
    KillToEnd,
}

#[derive(Deserialize)]
#[serde(try_from = "Vec<String>")]
pub struct Confirm {
    /// The first key as written, for the question's "(y/N)".
    pub hint: String,
    pub keys: Vec<Key>,
}

impl TryFrom<Vec<String>> for Confirm {
    type Error = String;

    fn try_from(names: Vec<String>) -> Result<Self, String> {
        let hint = names.first().ok_or("confirm needs at least one key")?.clone();
        let keys = names.iter().map(|name| single(name)).collect::<Result<_, _>>()?;
        Ok(Self { hint, keys })
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get() -> &'static Config {
    CONFIG.get().expect("config::load runs first thing in main")
}

/// Reads `path`, or else the first of $XDG_CONFIG_HOME/lazi/config.ron and
/// $XDG_CONFIG_DIRS/lazi/config.ron that exists.
pub fn load(path: Option<PathBuf>) -> Result<(), String> {
    let path = match path {
        Some(path) => path,
        None => find()?,
    };
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // So `fg: "blue"` needn't be `fg: Some("blue")`.
    let ron = ron::Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
    let mut config: Config = ron.from_str(&text).map_err(|e| format!("{}:{e}", path.display()))?;
    for rule in &mut config.rules {
        for name in &rule.open {
            let opener = config.openers.get(name).ok_or_else(|| format!("{}: no opener named \"{name}\"", path.display()))?;
            rule.openers.push(opener.clone());
        }
    }
    let _ = CONFIG.set(config);
    Ok(())
}

fn find() -> Result<PathBuf, String> {
    let home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"));
    let dirs = env::var("XDG_CONFIG_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "/etc/xdg".into());
    let candidates: Vec<PathBuf> =
        [home].into_iter().chain(dirs.split(':').map(PathBuf::from)).map(|dir| dir.join("lazi/config.ron")).collect();
    candidates.iter().find(|path| path.is_file()).cloned().ok_or_else(|| {
        let tried: Vec<_> = candidates.iter().map(|p| p.display().to_string()).collect();
        format!("no config file; tried {}. lazi's source has an example config.ron.", tried.join(", "))
    })
}

fn seconds<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    Duration::try_from_secs_f64(f64::deserialize(d)?).map_err(D::Error::custom)
}

/// Unset colours and attributes are left to the terminal.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StyleSpec {
    /// A name ("blue", "lightred"), "#rrggbb" or a 256-colour index ("208").
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underlined: bool,
    reversed: bool,
}

fn style<'de, D: Deserializer<'de>>(d: D) -> Result<Style, D::Error> {
    let spec = StyleSpec::deserialize(d)?;
    let color = |name: &str| name.parse::<Color>().map_err(|_| D::Error::custom(format!("unknown colour \"{name}\"")));
    let mut style = Style::new();
    if let Some(fg) = &spec.fg {
        style = style.fg(color(fg)?);
    }
    if let Some(bg) = &spec.bg {
        style = style.bg(color(bg)?);
    }
    for (on, modifier) in [
        (spec.bold, Modifier::BOLD),
        (spec.dim, Modifier::DIM),
        (spec.italic, Modifier::ITALIC),
        (spec.underlined, Modifier::UNDERLINED),
        (spec.reversed, Modifier::REVERSED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    Ok(style)
}

pub type Key = (KeyCode, KeyModifiers);

/// A keymap whose bindings may be sequences. A binding that starts a longer one would make the
/// longer one unreachable, so that's an error rather than a surprise.
fn sequences<'de, D: Deserializer<'de>, A: Deserialize<'de>>(d: D) -> Result<Vec<(Vec<Key>, A)>, D::Error> {
    let raw = HashMap::<String, A>::deserialize(d)?;
    let mut map = Vec::new();
    for (name, action) in raw {
        map.push((sequence(&name).map_err(D::Error::custom)?, action, name));
    }
    for (keys, _, name) in &map {
        if let Some((_, _, other)) = map.iter().find(|(k, _, n)| n != name && k.starts_with(keys)) {
            return Err(D::Error::custom(format!("\"{name}\" hides \"{other}\"")));
        }
    }
    Ok(map.into_iter().map(|(keys, action, _)| (keys, action)).collect())
}

fn singles<'de, D: Deserializer<'de>, A: Deserialize<'de>>(d: D) -> Result<HashMap<Key, A>, D::Error> {
    let raw = HashMap::<String, A>::deserialize(d)?;
    raw.into_iter().map(|(name, action)| Ok((single(&name).map_err(D::Error::custom)?, action))).collect()
}

fn single(name: &str) -> Result<Key, String> {
    match sequence(name)?.as_slice() {
        &[key] => Ok(key),
        _ => Err(format!("\"{name}\": only single keys here")),
    }
}

/// Parses a binding: keys separated by spaces, each a key name or character with optional
/// `+`-joined modifiers ("Ctrl+u", "Shift+PageUp", "Ctrl+x Ctrl+s"). A run of plain characters
/// that isn't a key name is one key per character, so "gg" is "g g".
fn sequence(name: &str) -> Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    for word in name.split_whitespace() {
        // The last `+` separates the key, unless the key is `+` itself.
        let (mods, key) = match word.strip_suffix("++") {
            Some(mods) => (Some(mods), "+"),
            None => match word.rsplit_once('+') {
                Some((mods, key)) if !mods.is_empty() && !key.is_empty() => (Some(mods), key),
                _ => (None, word),
            },
        };
        let mut modifiers = KeyModifiers::NONE;
        for m in mods.into_iter().flat_map(|m| m.split('+')) {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" => KeyModifiers::SUPER,
                _ => return Err(format!("\"{name}\": unknown modifier \"{m}\"")),
            };
        }
        if let Some(code) = named(key) {
            // Terminals send shift+Tab as BackTab.
            if code == KeyCode::Tab && modifiers.contains(KeyModifiers::SHIFT) {
                keys.push((KeyCode::BackTab, modifiers - KeyModifiers::SHIFT));
            } else {
                keys.push((code, modifiers));
            }
            continue;
        }
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            keys.push(char_key(c, modifiers));
        } else if mods.is_none() {
            keys.extend(key.chars().map(|c| char_key(c, modifiers)));
        } else {
            return Err(format!("\"{name}\": unknown key \"{key}\""));
        }
    }
    if keys.is_empty() {
        return Err("empty key binding".into());
    }
    Ok(keys)
}

/// Terminals send a shifted character as itself ('G'), so that's how it's bound too.
fn char_key(c: char, mut modifiers: KeyModifiers) -> Key {
    if modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.remove(KeyModifiers::SHIFT);
        return (KeyCode::Char(c.to_ascii_uppercase()), modifiers);
    }
    (KeyCode::Char(c), modifiers)
}

fn named(name: &str) -> Option<KeyCode> {
    Some(match name {
        "Enter" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        "Tab" => KeyCode::Tab,
        "BackTab" => KeyCode::BackTab,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Space" => KeyCode::Char(' '),
        _ => KeyCode::F(name.strip_prefix('F')?.parse().ok().filter(|n| (1..=24).contains(n))?),
    })
}

pub enum Lookup {
    Action(&'static Action),
    /// `keys` starts a longer binding; wait for the next key.
    Pending,
    Unbound,
}

pub fn lookup(keys: &[Key]) -> Lookup {
    let mut prefix = false;
    for (bound, action) in &get().keys.normal {
        if *bound == keys {
            return Lookup::Action(action);
        }
        prefix |= bound.starts_with(keys);
    }
    if prefix { Lookup::Pending } else { Lookup::Unbound }
}
