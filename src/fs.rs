use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;
use std::{fs, io, path::Path};

use crate::config::{Sort, SortBy};

#[derive(Clone)]
pub struct Entry {
    pub name: OsString,
    key: String,
    pub is_dir: bool,
    pub is_link: bool,
    /// Size and mtime (seconds since the epoch), following symlinks. Only read when sorting by
    /// them, else 0.
    size: u64,
    mtime: i64,
}

pub struct Listing {
    pub entries: Vec<Entry>,
    pub error: Option<String>,
    /// The directory's mtime when read. Creating, deleting or renaming anything inside bumps it.
    mtime: Option<SystemTime>,
    hidden: bool,
    sort: Sort,
    /// Known to be out of date whatever the mtime says, e.g. from an inotify event landing
    /// within the same mtime tick as the last read.
    stale: bool,
}

impl Listing {
    pub fn read(dir: &Path, show_hidden: bool, sort: Sort) -> Self {
        // Stat before reading, so a change made mid-read still leaves the listing stale.
        let mtime = mtime(dir);
        let (entries, error) = match read_entries(dir, show_hidden, sort) {
            Ok(entries) => (entries, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        Self { entries, error, mtime, hidden: show_hidden, sort, stale: false }
    }

    pub fn is_fresh(&self, dir: &Path, show_hidden: bool, sort: Sort) -> bool {
        !self.stale && self.hidden == show_hidden && self.sort == sort && self.mtime == mtime(dir)
    }

    pub fn invalidate(&mut self) {
        self.stale = true;
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
        self.subject(entry).contains(&self.query)
    }

    pub fn is_prefix(&self, entry: &Entry) -> bool {
        self.subject(entry).starts_with(&self.query)
    }

    /// The longest start the entries' names share (as compared, so lowercased unless the query
    /// is case-sensitive): what Tab can extend the query to.
    pub fn common_prefix(&self, entries: &[&Entry]) -> String {
        let Some((first, rest)) = entries.split_first() else { return self.query.clone() };
        let first = self.subject(first);
        let mut len = first.len();
        for entry in rest {
            let subject = self.subject(entry);
            len = first.char_indices().zip(subject.chars()).find(|((_, a), b)| a != b).map_or(len.min(subject.len()), |((i, _), _)| i.min(len));
        }
        first[..len].to_owned()
    }

    /// The name as this matcher sees it.
    fn subject<'a>(&self, entry: &'a Entry) -> Cow<'a, str> {
        if self.sensitive { entry.name.to_string_lossy() } else { Cow::Borrowed(&entry.key) }
    }
}

fn mtime(dir: &Path) -> Option<SystemTime> {
    fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// Reads `dir` sorted directories-first, then by `sort`.
fn read_entries(dir: &Path, show_hidden: bool, sort: Sort) -> io::Result<Vec<Entry>> {
    let stat = matches!(sort.by, SortBy::Size | SortBy::Mtime);
    let mut out = Vec::new();
    for ent in fs::read_dir(dir)? {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name();
        if !show_hidden && name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let Ok(ft) = ent.file_type() else { continue };
        let is_link = ft.is_symlink();
        // d_type answers everything except where a symlink points, so only symlinks and sorts
        // by size or mtime need a stat: one per entry doubles the time to list /usr/bin.
        let meta = if is_link {
            fs::metadata(ent.path()).ok()
        } else if stat {
            ent.metadata().ok()
        } else {
            None
        };
        let is_dir = if is_link { meta.as_ref().is_some_and(fs::Metadata::is_dir) } else { ft.is_dir() };
        let (size, mtime) = meta.map_or((0, 0), |m| (m.size(), m.mtime()));
        let key = name.to_string_lossy().to_lowercase();
        out.push(Entry { name, key, is_dir, is_link, size, mtime });
    }
    out.sort_unstable_by(|a, b| {
        let order = match sort.by {
            SortBy::Name => a.key.cmp(&b.key),
            SortBy::Size => b.size.cmp(&a.size),
            SortBy::Mtime => b.mtime.cmp(&a.mtime),
            SortBy::Ext => Path::new(&a.key).extension().cmp(&Path::new(&b.key).extension()),
        };
        let order = order.then_with(|| a.key.cmp(&b.key));
        b.is_dir.cmp(&a.is_dir).then(if sort.reverse { order.reverse() } else { order })
    });
    Ok(out)
}
