//! shop's config: which panels to open, how the tab bar looks, and shop's own keys. Like
//! lazi's, it has no built-in defaults.

use std::fs;
use std::path::PathBuf;

use lazi::Key;
use ratatui::style::Style;
use ron::extensions::Extensions;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub panels: Vec<PanelSpec>,
    pub style: Styles,
    /// Whatever no binding here claims goes to the focused panel.
    #[serde(deserialize_with = "lazi::sequences")]
    pub keys: Vec<(Vec<Key>, Action)>,
}

#[derive(Deserialize)]
pub enum PanelSpec {
    /// `config: None` is lazi's usual lookup; `dir: None` is shop's working directory.
    Lazi { config: Option<PathBuf>, dir: Option<PathBuf> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Styles {
    #[serde(deserialize_with = "lazi::style")]
    pub tab: Style,
    #[serde(deserialize_with = "lazi::style")]
    pub tab_focused: Style,
}

#[derive(Deserialize, Debug, PartialEq)]
pub enum Action {
    Next,
    Prev,
    /// The panel at this position in `panels`, from 0. Past the end does nothing.
    Focus(usize),
    Quit,
}

/// Reads `path`, or else the first of $XDG_CONFIG_HOME/shop/host.ron and
/// $XDG_CONFIG_DIRS/shop/host.ron that exists.
pub fn load(path: Option<PathBuf>) -> Result<Config, String> {
    let path = match path {
        Some(path) => path,
        None => lazi::find("shop/host.ron", "shop's source has an example host.ron.")?,
    };
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // So `fg: "blue"` needn't be `fg: Some("blue")`.
    let ron = ron::Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
    let config: Config = ron.from_str(&text).map_err(|e| format!("{}:{e}", path.display()))?;
    if config.panels.is_empty() {
        return Err(format!("{}: panels is empty; shop needs at least one", path.display()));
    }
    Ok(config)
}
