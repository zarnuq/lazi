use std::ffi::{OsStr, OsString};
use std::time::SystemTime;
use std::{fs, io, path::Path};

#[derive(Clone)]
pub struct Entry {
    pub name: OsString,
    key: String,
    pub is_dir: bool,
    pub is_link: bool,
}

pub struct Listing {
    pub entries: Vec<Entry>,
    pub error: Option<String>,
    /// The directory's mtime when read. Creating, deleting or renaming anything inside bumps it.
    mtime: Option<SystemTime>,
    hidden: bool,
}

impl Listing {
    pub fn read(dir: &Path, show_hidden: bool) -> Self {
        // Stat before reading, so a change made mid-read still leaves the listing stale.
        let mtime = mtime(dir);
        let (entries, error) = match read_entries(dir, show_hidden) {
            Ok(entries) => (entries, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        Self { entries, error, mtime, hidden: show_hidden }
    }

    pub fn is_fresh(&self, dir: &Path, show_hidden: bool) -> bool {
        self.hidden == show_hidden && self.mtime == mtime(dir)
    }

    pub fn position(&self, name: &OsStr) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }
}

/// Smart-case substring match: case-sensitive only if the query has an uppercase letter.
pub struct Matcher {
    query: String,
    sensitive: bool,
}

impl Matcher {
    pub fn new(query: &str) -> Self {
        let sensitive = query.chars().any(char::is_uppercase);
        let query = if sensitive { query.to_owned() } else { query.to_lowercase() };
        Self { query, sensitive }
    }

    pub fn matches(&self, entry: &Entry) -> bool {
        if self.sensitive {
            entry.name.to_string_lossy().contains(&self.query)
        } else {
            entry.key.contains(&self.query)
        }
    }
}

fn mtime(dir: &Path) -> Option<SystemTime> {
    fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// Reads `dir` sorted directories-first, then case-insensitively by name.
fn read_entries(dir: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for ent in fs::read_dir(dir)? {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name();
        if !show_hidden && name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let Ok(ft) = ent.file_type() else { continue };
        let is_link = ft.is_symlink();
        // d_type answers everything except where a symlink points, so only symlinks get a stat.
        let is_dir = if is_link {
            fs::metadata(ent.path()).is_ok_and(|m| m.is_dir())
        } else {
            ft.is_dir()
        };
        let key = name.to_string_lossy().to_lowercase();
        out.push(Entry { name, key, is_dir, is_link });
    }
    out.sort_unstable_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}
