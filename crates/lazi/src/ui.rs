use std::env;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};

use crate::app::{App, Mark, Menu, Prompt};
use crate::config::{self, Icon, Icons};
use crate::fs::{Entry, Listing, Matcher};
use crate::input::Input;
use crate::ops;
use crate::preview::Preview;

/// Row 0 is the cwd, the last row is the status line, and the three columns fill the middle.
/// Draws lazi into `area`, which is the whole screen standalone or a panel's share of it in shop.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height < 3 {
        return;
    }
    let buf = frame.buffer_mut();
    buf.set_stringn(area.x, area.y, header(&app.cwd), area.width as usize, config::get().style.header);

    let body = Rect { y: area.y + 1, height: area.height - 2, ..area };
    let [parent_col, cwd_col, preview_col] = columns(body);
    app.height = body.height as usize;
    app.preview_area = preview_col;
    app.scroll();
    let app = &*app;

    if let Some(parent) = app.cwd.parent()
        && let Some(listing) = app.listing(parent)
    {
        let cursor = app.cwd.file_name().and_then(|name| listing.position(name));
        draw_side(buf, parent_col, app, parent, listing, cursor);
    }
    if let Some(listing) = app.listing(&app.cwd) {
        let list = List { entries: app.entries(), error: listing.error.as_deref(), cursor: Some(app.cursor), offset: app.offset };
        draw_list(buf, cwd_col, app, &app.cwd, list, app.find_matcher().as_ref());
    }
    if let Some(dir) = app.preview_dir()
        && let Some(listing) = app.listing(&dir)
    {
        let cursor = app.hovered_in(&dir).and_then(|name| listing.position(name)).unwrap_or(0);
        draw_side(buf, preview_col, app, &dir, listing, Some(cursor));
    } else if let Some(preview) = app.current_preview() {
        draw_preview(buf, preview_col, app, preview);
    }

    let status = Rect { y: area.bottom() - 1, height: 1, ..area };
    let cursor = draw_status(buf, status, app);
    if let Some(menu) = &app.menu {
        draw_menu(buf, area, menu);
    }
    if let Some(pos) = cursor {
        frame.set_cursor_position(pos);
    }
}

/// A file's preview. Images only leave the area blank here; kitty draws them after the frame.
fn draw_preview(buf: &mut Buffer, area: Rect, app: &App, preview: &Preview) {
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
    match preview {
        Preview::Text(lines) => {
            // Each line wraps to at least one row, so this many lines always fill the area.
            let visible: Vec<_> = lines.iter().skip(app.preview_scroll).take(inner.height as usize).cloned().collect();
            Paragraph::new(visible).wrap(Wrap { trim: false }).render(inner, buf);
        }
        Preview::Image(_) => {}
        Preview::Note(note) => {
            buf.set_stringn(inner.x, inner.y, note, inner.width as usize, config::get().style.dim);
        }
    }
}

/// Returns where the terminal cursor goes when a text prompt is open.
fn draw_status(buf: &mut Buffer, area: Rect, app: &App) -> Option<(u16, u16)> {
    let width = area.width as usize;
    let input = |buf: &mut Buffer, label: &str, input: &Input| {
        buf.set_stringn(area.x, area.y, format!("{label}{}", input.text), width, Style::new());
        let label_width = label.chars().count();
        Some((area.x + (label_width + input.cursor_width()).min(width) as u16, area.y))
    };
    match &app.prompt {
        Some(Prompt::Create(i)) => return input(buf, "Create: ", i),
        Some(Prompt::Rename { input: i, .. }) => return input(buf, "Rename: ", i),
        Some(Prompt::Find { input: i, backward, .. }) => return input(buf, if *backward { "Find ↑: " } else { "Find: " }, i),
        Some(Prompt::Filter(i)) => return input(buf, "Filter: ", i),
        Some(Prompt::Confirm { question, .. }) => {
            buf.set_stringn(area.x, area.y, question, width, Style::new());
            return None;
        }
        None => {}
    }

    let mut right = String::new();
    if let Some(filter) = &app.filter {
        right.push_str(&format!("filter: {}  ", filter.query));
    }
    if let Some(find) = &app.find {
        right.push_str(&format!("find: {find}  "));
    }
    if !app.selected.is_empty() {
        right.push_str(&format!("{} selected  ", app.selected.len()));
    }
    let len = app.entries().len();
    if len > 0 {
        right.push_str(&format!("{}/{len}", app.cursor + 1));
    }
    let right_width = right.chars().count();
    let x = area.right().saturating_sub(right_width as u16);
    buf.set_stringn(x, area.y, right, width, Style::new());

    // Messages get whatever the right side leaves.
    let width = width.saturating_sub(right_width + 1);
    if let Some(err) = &app.error {
        buf.set_stringn(area.x, area.y, tail(err, width), width, config::get().style.error);
    } else if let Some(info) = &app.info {
        buf.set_stringn(area.x, area.y, info, width, Style::new());
    } else if !app.tasks.is_empty() {
        let tasks: Vec<String> = app
            .tasks
            .values()
            .map(|p| match p.total {
                0 => p.label.to_owned(),
                total if p.bytes => format!("{} {}%", p.label, p.done * 100 / total),
                total => format!("{} {}/{total}", p.label, p.done),
            })
            .collect();
        buf.set_stringn(area.x, area.y, tasks.join(" · "), width, Style::new());
    } else if app.is_loading() {
        buf.set_stringn(area.x, area.y, "loading…", width, config::get().style.dim);
    } else if let Some(entry) = app.entries().get(app.cursor) {
        // One stat per frame, and frames only follow events.
        let path = app.cwd.join(&entry.name);
        if let Ok(meta) = fs::metadata(&path).or_else(|_| fs::symlink_metadata(&path)) {
            buf.set_stringn(area.x, area.y, details(entry, &meta), width, config::get().style.dim);
        }
    }

    None
}

/// Permissions, size (files only) and mtime, like `ls -l`.
fn details(entry: &Entry, meta: &Metadata) -> String {
    let kind = if entry.is_link { 'l' } else if entry.is_dir { 'd' } else { '-' };
    let perms: String = (0..9).rev().map(|bit| if meta.mode() >> bit & 1 == 1 { b"xwr"[bit % 3] as char } else { '-' }).collect();
    let size = if entry.is_dir { String::new() } else { format!("{}  ", human(meta.size())) };
    let tm = ops::local_time(meta.mtime());
    let (year, month, day, hour, min) = (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min);
    format!("{kind}{perms}  {size}{year:04}-{month:02}-{day:02} {hour:02}:{min:02}")
}

/// Bytes in powers of 1024, one decimal past the first unit.
fn human(bytes: u64) -> String {
    let (mut size, mut unit) = (bytes as f64, 0);
    while size >= 1024.0 && unit < 5 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes}B") } else { format!("{size:.1}{}", ["", "K", "M", "G", "T", "P"][unit]) }
}

/// A centred box listing the openers, numbered for quick picking.
fn draw_menu(buf: &mut Buffer, area: Rect, menu: &Menu) {
    let inner_width = menu.openers.iter().map(|o| o.desc.chars().count()).max().unwrap_or(0) as u16 + 4;
    let width = (inner_width + 2).max(14).min(area.width);
    let height = (menu.openers.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    Clear.render(rect, buf);
    let block = Block::bordered().title(" Open with ");
    let inner = block.inner(rect);
    block.render(rect, buf);
    for (i, (y, opener)) in (inner.y..inner.bottom()).zip(menu.openers).enumerate() {
        let style = if i == menu.cursor { config::get().style.cursor } else { Style::new() };
        buf.set_style(Rect { y, height: 1, ..inner }, style);
        buf.set_stringn(inner.x + 1, y, format!("{} {}", i + 1, opener.desc), (inner.width as usize).saturating_sub(1), style);
    }
}

fn icon<'a>(icons: &'a Icons, entry: &Entry) -> &'a Icon {
    let name = entry.name.to_str().unwrap_or_default();
    if entry.is_dir {
        return icons.dirs.get(name).unwrap_or(&icons.dir);
    }
    let ext = || Path::new(name).extension().and_then(OsStr::to_str).map(str::to_ascii_lowercase);
    icons
        .files
        .get(name)
        .or_else(|| icons.exts.get(&ext()?))
        .unwrap_or(if entry.is_link { &icons.link } else { &icons.file })
}

/// The end of `text` if it's wider than `width`, since errors put the cause last.
fn tail(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len <= width || width == 0 {
        return text.to_owned();
    }
    let rest: String = text.chars().skip(len - width + 1).collect();
    format!("…{rest}")
}

pub fn header(cwd: &Path) -> String {
    if let Some(home) = env::var_os("HOME")
        && let Ok(rest) = cwd.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
    }
    cwd.display().to_string()
}

fn columns(area: Rect) -> [Rect; 3] {
    let (r0, r1, r2) = config::get().ratio;
    let total = (r0 as u32 + r1 as u32 + r2 as u32).max(1);
    let width = |r: u16| (area.width as u32 * r as u32 / total) as u16;
    let (w0, w1) = (width(r0), width(r1));
    [
        Rect { width: w0, ..area },
        Rect { x: area.x + w0, width: w1, ..area },
        Rect { x: area.x + w0 + w1, width: area.width - w0 - w1, ..area },
    ]
}

/// The rows of one column.
struct List<'a> {
    entries: &'a [Entry],
    error: Option<&'a str>,
    cursor: Option<usize>,
    offset: usize,
}

/// Draws a column with no scroll state of its own, scrolled just enough to show `cursor`.
fn draw_side(buf: &mut Buffer, area: Rect, app: &App, dir: &Path, listing: &Listing, cursor: Option<usize>) {
    let height = area.height as usize;
    let so = config::get().scrolloff.min(height.saturating_sub(1) / 2);
    let offset = cursor
        .map_or(0, |c| (c + so + 1).saturating_sub(height))
        .min(listing.entries.len().saturating_sub(height));
    let list = List { entries: &listing.entries, error: listing.error.as_deref(), cursor, offset };
    draw_list(buf, area, app, dir, list, None);
}

/// `find` highlights matching names.
fn draw_list(buf: &mut Buffer, area: Rect, app: &App, dir: &Path, list: List, find: Option<&Matcher>) {
    let styles = &config::get().style;
    let width = area.width.saturating_sub(2) as usize;
    if let Some(err) = list.error {
        buf.set_stringn(area.x + 1, area.y, err, width, styles.error);
        return;
    }
    let rows = list.entries.iter().enumerate().skip(list.offset).take(area.height as usize);
    for (y, (i, entry)) in (area.y..).zip(rows) {
        let base = if entry.is_dir {
            styles.dir
        } else if entry.is_link {
            styles.link
        } else {
            let ext = Path::new(&entry.name).extension().and_then(OsStr::to_str).map(str::to_ascii_lowercase);
            ext.and_then(|ext| styles.files.iter().find(|f| f.ext.contains(&ext))).map_or(Style::new(), |f| f.style)
        };
        let mut style = base;
        if find.is_some_and(|m| m.matches(entry)) {
            style = style.patch(styles.find);
        }
        let on_cursor = Some(i) == list.cursor;
        if on_cursor {
            buf.set_style(Rect { y, height: 1, ..area }, styles.cursor);
            style = style.patch(styles.cursor);
        }
        let (mut x, mut width) = (area.x + 1, width);
        if let Some(icon) = config::get().icons.as_ref().map(|icons| icon(icons, entry))
            && !icon.glyph.is_empty()
        {
            // Not underlined with a find match, and on the cursor row plain cursor colours:
            // the icon's own colour there would turn into a block of it under reverse video.
            let icon_style = if on_cursor {
                base.patch(styles.cursor)
            } else {
                icon.fg.map_or(base, |fg| base.fg(fg))
            };
            buf.set_stringn(x, y, &icon.glyph, width, icon_style);
            // The gap is an en space, not a space: kitty draws an icon across a following space
            // or en space, but ratatui never sends a space to a cell it thinks is blank already,
            // which after a clear leaves an empty cell and some icons drawn narrow. An en space
            // is always sent, so every icon gets the same two cells.
            let gap = x + icon.width;
            buf.set_stringn(gap, y, "\u{2002}", width.saturating_sub(icon.width as usize), icon_style);
            let used = (icon.width as usize + 1).min(width);
            x += used as u16;
            width -= used;
        }
        buf.set_stringn(x, y, entry.name.to_string_lossy(), width, style);
        if let Some(mark) = app.mark(dir, entry)
            && let Some(cell) = buf.cell_mut((area.x, y))
        {
            // Reset first so the cursor's reverse video doesn't swap the colour away.
            cell.reset();
            cell.set_style(match mark {
                Mark::Selected => styles.mark_selected,
                Mark::Copied => styles.mark_copied,
                Mark::Cut => styles.mark_cut,
            });
        }
    }
}
