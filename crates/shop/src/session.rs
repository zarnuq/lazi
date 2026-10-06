//! Which tabs were open when shop last quit, so the next start can bring them back, like emacs's
//! desktop-save. Saved as plain lines in $XDG_STATE_HOME/shop/session, one `lazi PATH` or panel
//! name per tab. Tab one, the first lazi tab, isn't really saved: it always opens where shop is
//! started; the others stay where they were left until closed.

use std::collections::VecDeque;
use std::{env, fs, io, mem};
use std::path::{Path, PathBuf};

use crate::config::PanelSpec;

#[derive(Debug, PartialEq)]
pub enum Tab {
    /// A lazi tab and the directory it was in.
    Lazi(PathBuf),
    /// Any other panel, by its tab name; its settings come from host.ron.
    Other(String),
}

#[derive(Debug, PartialEq)]
pub struct Session {
    pub tabs: Vec<Tab>,
}

/// A file in $XDG_STATE_HOME/shop, else ~/.local/state/shop.
fn state(name: &str) -> Option<PathBuf> {
    let state = env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| Some(PathBuf::from(env::var_os("HOME")?).join(".local/state")))?;
    Some(state.join("shop").join(name))
}

fn write_state(name: &str, text: &str) -> io::Result<()> {
    let path = state(name).ok_or_else(|| io::Error::other("no $XDG_STATE_HOME or $HOME"))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text)
}

/// The saved session, if there is one.
pub fn load() -> Option<Session> {
    Some(parse(&fs::read_to_string(state("session")?).ok()?))
}

pub fn save(session: &Session) -> io::Result<()> {
    write_state("session", &render(session))
}

pub fn render(session: &Session) -> String {
    let mut text = String::new();
    for tab in &session.tabs {
        match tab {
            Tab::Lazi(dir) => text.push_str(&format!("lazi {}\n", dir.display())),
            Tab::Other(name) => text.push_str(&format!("{name}\n")),
        }
    }
    text
}

/// Reads what `render` wrote. Lines it doesn't know are skipped, so a damaged file costs tabs,
/// never the start.
pub fn parse(text: &str) -> Session {
    let mut session = Session { tabs: Vec::new() };
    for line in text.lines() {
        if let Some(dir) = line.strip_prefix("lazi ") {
            session.tabs.push(Tab::Lazi(PathBuf::from(dir)));
        } else if !line.is_empty() {
            session.tabs.push(Tab::Other(line.to_owned()));
        }
    }
    session
}

/// The tabs to open and which to focus: the saved ones in order, built from the config's panel
/// settings, with tab one (the first lazi tab) opening `here` and focused. Other lazi tabs whose
/// directory is gone are dropped; panels the config gained since are added at the end. With
/// nothing saved, it's the config's own layout.
pub fn layout(saved: Option<Session>, specs: &[PanelSpec], here: PathBuf, exists: impl Fn(&Path) -> bool) -> (Vec<PanelSpec>, usize) {
    let Some(lazi_config) = specs.iter().find_map(|s| match s {
        PanelSpec::Lazi { config, .. } => Some(config.clone()),
        PanelSpec::Git(_) => None,
    }) else {
        // No lazi panel configured, so no directory to restore anywhere.
        return (specs.to_vec(), 0);
    };
    let mut out = Vec::new();
    if let Some(saved) = saved {
        let mut gits: VecDeque<PanelSpec> = specs.iter().filter(|s| matches!(s, PanelSpec::Git(_))).cloned().collect();
        let mut first = true;
        for tab in saved.tabs {
            let spec = match tab {
                // Tab one's folder is about to be replaced by `here`, so it needn't still exist.
                Tab::Lazi(dir) if mem::take(&mut first) || exists(&dir) => PanelSpec::Lazi { config: lazi_config.clone(), dir: Some(dir) },
                Tab::Other(name) if name == "git" => match gits.pop_front() {
                    Some(git) => git,
                    None => continue,
                },
                _ => continue,
            };
            out.push(spec);
        }
        out.extend(gits);
    }
    if !out.iter().any(|s| matches!(s, PanelSpec::Lazi { .. })) {
        // Nothing usable was saved: the config's layout.
        out = specs.to_vec();
    }
    let one = out.iter().position(|s| matches!(s, PanelSpec::Lazi { .. })).unwrap_or(0);
    if let PanelSpec::Lazi { dir, .. } = &mut out[one] {
        *dir = Some(here);
    }
    (out, one)
}

/// How many visited folders the search keeps.
const FOLDERS: usize = 500;

/// Puts `dir` first among the visited folders, moving it up if it was already there.
pub fn remember(folders: &mut Vec<PathBuf>, dir: &Path) {
    folders.retain(|f| f != dir);
    folders.insert(0, dir.to_path_buf());
    folders.truncate(FOLDERS);
}

/// The visited folders, newest first, from the last runs.
pub fn load_folders() -> Vec<PathBuf> {
    let Some(path) = state("folders") else { return Vec::new() };
    fs::read_to_string(path).map(|text| text.lines().map(PathBuf::from).collect()).unwrap_or_default()
}

pub fn save_folders(folders: &[PathBuf]) -> io::Result<()> {
    let text: String = folders.iter().map(|f| format!("{}\n", f.display())).collect();
    write_state("folders", &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_folders_are_newest_first_without_repeats_and_capped() {
        let mut folders = Vec::new();
        for dir in ["/a", "/b", "/a"] {
            remember(&mut folders, Path::new(dir));
        }
        assert_eq!(folders, [PathBuf::from("/a"), PathBuf::from("/b")]);
        for i in 0..600 {
            remember(&mut folders, Path::new(&format!("/{i}")));
        }
        assert_eq!(folders.len(), 500);
        assert_eq!(folders[0], PathBuf::from("/599"));
    }
    use crate::config::GitSpec;

    fn lazi(dir: &str) -> PanelSpec {
        PanelSpec::Lazi { config: None, dir: Some(PathBuf::from(dir)) }
    }

    /// Enough of a git panel's settings to tell two apart.
    fn git(fetch_every: u64) -> PanelSpec {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/host.ron")).unwrap();
        let config: crate::config::Config =
            ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME | ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES).from_str(&text).unwrap();
        let mut spec: Box<GitSpec> = config.panels.into_iter().find_map(|p| if let PanelSpec::Git(g) = p { Some(g) } else { None }).unwrap();
        spec.fetch_every = fetch_every;
        PanelSpec::Git(spec)
    }

    /// Which panel each spec is, and where a lazi one starts, for comparing layouts.
    fn shape(specs: &[PanelSpec]) -> Vec<String> {
        specs
            .iter()
            .map(|s| match s {
                PanelSpec::Lazi { dir, .. } => format!("lazi {}", dir.as_ref().map_or("-".into(), |d| d.display().to_string())),
                PanelSpec::Git(g) => format!("git {}", g.fetch_every),
            })
            .collect()
    }

    #[test]
    fn a_session_reads_back_what_was_written() {
        let session = Session { tabs: vec![Tab::Lazi("/a b".into()), Tab::Other("git".into()), Tab::Lazi("/c".into())] };
        assert_eq!(parse(&render(&session)), session);
    }

    #[test]
    fn tab_one_opens_here_and_the_rest_where_they_were() {
        let saved = Session { tabs: vec![Tab::Lazi("/a".into()), Tab::Lazi("/b".into()), Tab::Other("git".into())] };
        let (specs, focus) = layout(Some(saved), &[lazi("/cfg"), git(300)], "/here".into(), |_| true);
        assert_eq!(shape(&specs), ["lazi /here", "lazi /b", "git 300"]);
        assert_eq!(focus, 0);
    }

    /// Tab one's saved folder was only where shop was last started, so it doesn't matter if it's
    /// gone; another tab's is dropped with it.
    #[test]
    fn gone_directories_drop_their_tabs_but_never_tab_one() {
        let saved = Session { tabs: vec![Tab::Lazi("/gone".into()), Tab::Lazi("/a".into()), Tab::Lazi("/gone".into()), Tab::Other("git".into())] };
        let (specs, focus) = layout(Some(saved), &[lazi("/cfg"), git(300)], "/here".into(), |p| p != Path::new("/gone"));
        assert_eq!(shape(&specs), ["lazi /here", "lazi /a", "git 300"]);
        assert_eq!(focus, 0);
    }

    #[test]
    fn without_a_session_the_config_layout_starts_here() {
        let (specs, focus) = layout(None, &[git(300), lazi("/cfg")], "/here".into(), |_| true);
        assert_eq!(shape(&specs), ["git 300", "lazi /here"]);
        assert_eq!(focus, 1);
    }

    #[test]
    fn panels_new_to_the_config_are_added_after_the_saved_ones() {
        let saved = Session { tabs: vec![Tab::Lazi("/a".into()), Tab::Other("git".into())] };
        let (specs, _) = layout(Some(saved), &[lazi("/cfg"), git(300), git(60)], "/here".into(), |_| true);
        assert_eq!(shape(&specs), ["lazi /here", "git 300", "git 60"]);
    }

}
