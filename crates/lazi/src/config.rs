//! lazi's config: which panels to open, how the tab bar looks, and lazi's own keys. Like
//! the files tab's, it has no built-in defaults.

use std::fs;
use std::path::PathBuf;

use files::Key;
use ratatui::style::Style;
use ron::extensions::Extensions;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub panels: Vec<PanelSpec>,
    /// Reopen the tabs lazi had when it last quit.
    pub restore: bool,
    pub style: Styles,
    /// Whatever no binding here claims goes to the focused panel.
    #[serde(deserialize_with = "files::sequences")]
    pub keys: Vec<(Vec<Key>, Action)>,    pub search: SearchSpec,
    /// While the `?` menu is up. Unbound keys do nothing.
    #[serde(deserialize_with = "files::sequences")]
    pub help_keys: Vec<(Vec<Key>, HelpAction)>,
}

/// The Ctrl+p search box.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SearchSpec {
    /// A `sh -c` snippet that opens file $1.
    pub open: String,
    /// The same at line $2.
    pub open_line: String,
    /// Search hidden files too, which is where dotfiles live.
    pub hidden: bool,
    /// Globs never searched, such as .git or ~/.cache.
    pub exclude: Vec<String>,
    pub style: SearchStyles,
    /// Unbound printable keys type into the query.
    #[serde(deserialize_with = "files::sequences")]
    pub keys: Vec<(Vec<Key>, SearchAction)>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SearchStyles {
    #[serde(deserialize_with = "files::style")]
    pub section: Style,
    #[serde(deserialize_with = "files::style")]
    pub line_number: Style,
    #[serde(deserialize_with = "files::style")]
    pub cursor: Style,
}

#[derive(Deserialize, Clone, Debug)]
pub enum SearchAction {
    Down,
    Up,
    /// Open the file in the editor, or go to the folder in files.
    Open,
    /// Show the file in files instead.
    Reveal,
    Close,
    DeleteChar,
    DeleteWord,
    Clear,
}

#[derive(Deserialize, Debug, PartialEq)]
pub enum HelpAction {
    Down,
    Up,
    /// Half the menu's height.
    PageDown,
    PageUp,
    Top,
    Bottom,
    Close,
}

#[derive(Deserialize, Clone)]
pub enum PanelSpec {
    /// `config: None` is the files tab's usual lookup; `dir: None` is lazi's working directory.
    Files { config: Option<PathBuf>, dir: Option<PathBuf> },
    Git(Box<GitSpec>),
    Dashboard(Box<DashSpec>),
}

impl PanelSpec {
    /// The tab's name, which the saved session refers to it by.
    pub fn name(&self) -> &'static str {
        match self {
            PanelSpec::Files { .. } => "files",
            PanelSpec::Git(_) => "git",
            PanelSpec::Dashboard(_) => "home",
        }
    }
}

/// The start page, like Doom Emacs's: a menu of shortcuts above the files opened last.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DashSpec {
    /// How many recently opened files to list.
    pub recent: usize,
    /// Shown in this order, each with its key.
    pub menu: Vec<MenuItem>,
    pub style: DashStyles,
    /// The panel's own keys; the menu's come from `menu`.
    #[serde(deserialize_with = "files::sequences")]
    pub keys: Vec<(Vec<Key>, DashAction)>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct MenuItem {
    /// Written as in `keys`.
    pub key: String,
    pub label: String,
    pub run: Shortcut,
}

#[derive(Deserialize, Clone, Debug)]
pub enum Shortcut {
    /// Open the search box with this query, like "repo:." for every repo.
    Search(String),
    /// Show this folder in files. A leading `~` is the home directory.
    Goto(String),
    /// Open this file in the editor (`search.open`).
    Edit(String),
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DashStyles {
    #[serde(deserialize_with = "files::style")]
    pub title: Style,
    #[serde(deserialize_with = "files::style")]
    pub key: Style,
    #[serde(deserialize_with = "files::style")]
    pub cursor: Style,
}

#[derive(Deserialize, Clone, Debug)]
pub enum DashAction {
    Down,
    Up,
    Top,
    Bottom,
    /// Run the menu item, or open the file in the editor.
    Open,
    /// Show the file in files instead.
    Reveal,
    Help,
}

/// The git panel: repos directly under `roots`, their status in zhimmer's symbols.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitSpec {
    /// Where to search for repos. A leading `~` is the home directory.
    pub roots: Vec<String>,
    /// How many levels below each root to search: 1 is its direct subdirectories.
    pub depth: usize,
    /// Seconds between background fetch rounds.
    pub fetch_every: u64,
    pub symbols: GitSymbols,
    pub style: GitStyles,
    #[serde(deserialize_with = "files::sequences")]
    pub keys: Vec<(Vec<Key>, GitAction)>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitSymbols {
    pub untracked: String,
    pub staged: String,
    pub modified: String,
    pub renamed: String,
    pub deleted: String,
    pub unmerged: String,
    pub ahead: String,
    pub behind: String,
    pub clean: String,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitStyles {
    #[serde(deserialize_with = "files::style")]
    pub root: Style,
    #[serde(deserialize_with = "files::style")]
    pub branch: Style,
    #[serde(deserialize_with = "files::style")]
    pub status: Style,
    #[serde(deserialize_with = "files::style")]
    pub arrows: Style,
    #[serde(deserialize_with = "files::style")]
    pub clean: Style,
    #[serde(deserialize_with = "files::style")]
    pub error: Style,
    #[serde(deserialize_with = "files::style")]
    pub cursor: Style,
}

#[derive(Deserialize, Clone, Debug)]
pub enum GitAction {
    Down,
    Up,
    Top,
    Bottom,
    /// Show the repo in the files panel.
    Open,
    /// Find repos again, then re-read every status and fetch every repo.
    Refresh,
    /// A `sh -c` snippet run in the repo, with its path as $1: blocking hands over the
    /// terminal and waits; otherwise it's detached and only a failure is reported.
    Run { run: String, block: bool },
    /// Show the key bindings.
    Help,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Styles {
    #[serde(deserialize_with = "files::style")]
    pub tab: Style,
    #[serde(deserialize_with = "files::style")]
    pub tab_focused: Style,
}

#[derive(Deserialize, Debug, PartialEq)]
pub enum Action {
    Next,
    Prev,
    /// The panel at this position in `panels`, from 0. Past the end does nothing.
    Focus(usize),
    Quit,
    /// Open the search box.
    Search,
}

/// Reads `path`, or else the first of $XDG_CONFIG_HOME/lazi/config.ron and
/// $XDG_CONFIG_DIRS/lazi/config.ron that exists.
pub fn load(path: Option<PathBuf>) -> Result<Config, String> {
    let path = match path {
        Some(path) => path,
        None => files::find("lazi/config.ron", "lazi's source has an example config.ron.")?,
    };
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // So `fg: "blue"` needn't be `fg: Some("blue")`, and `Git(roots: …)` needn't be `Git((roots: …))`.
    let ron = ron::Options::default().with_default_extension(Extensions::IMPLICIT_SOME | Extensions::UNWRAP_VARIANT_NEWTYPES);
    let config: Config = ron.from_str(&text).map_err(|e| format!("{}:{e}", path.display()))?;
    if config.panels.is_empty() {
        return Err(format!("{}: panels is empty; lazi needs at least one", path.display()));
    }
    for panel in &config.panels {
        if let PanelSpec::Git(git) = panel
            && git.fetch_every == 0
        {
            return Err(format!("{}: fetch_every must be at least 1", path.display()));
        }
        if let PanelSpec::Git(git) = panel
            && git.depth == 0
        {
            return Err(format!("{}: depth must be at least 1", path.display()));
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped config.ron is what /etc/xdg/lazi gets, so it must stay loadable.
    #[test]
    fn reference_config_loads() {
        let config = load(Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/config.ron")))).unwrap();
        assert!(config.panels.iter().any(|p| matches!(p, PanelSpec::Git(_))));
    }

    #[test]
    fn fetch_every_zero_is_rejected() {
        let path = std::env::temp_dir().join(format!("lazi-fetch-every-{}.ron", std::process::id()));
        let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.ron")).unwrap().replace("fetch_every: 300", "fetch_every: 0");
        fs::write(&path, text).unwrap();
        let err = load(Some(path.clone())).err();
        let _ = fs::remove_file(&path);
        assert!(err.unwrap().contains("fetch_every"));
    }
}
