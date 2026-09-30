use std::env;
use std::path::Path;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Widget};

use crate::app::{App, Mark, Menu, Prompt};
use crate::config::{
    CURSOR, DIM, DIR, ERROR, HEADER, LINK, MARK_COPIED, MARK_CUT, MARK_SELECTED, RATIO, SCROLLOFF,
};
use crate::fs::Listing;
use crate::input::Input;

/// Row 0 is the cwd, the last row is the status line, and the three columns fill the middle.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.height < 3 {
        return;
    }
    let buf = frame.buffer_mut();
    buf.set_stringn(area.x, area.y, header(&app.cwd), area.width as usize, HEADER);

    let body = Rect { y: area.y + 1, height: area.height - 2, ..area };
    let [parent_col, cwd_col, preview_col] = columns(body);
    app.height = body.height as usize;
    app.scroll();
    let app = &*app;

    if let Some(parent) = app.cwd.parent()
        && let Some(listing) = app.listing(parent)
    {
        let cursor = app.cwd.file_name().and_then(|name| listing.position(name));
        draw_side(buf, parent_col, app, parent, listing, cursor);
    }
    if let Some(listing) = app.listing(&app.cwd) {
        draw_list(buf, cwd_col, app, &app.cwd, listing, Some(app.cursor), app.offset);
    }
    if let Some(dir) = app.preview_dir()
        && let Some(listing) = app.listing(&dir)
    {
        let cursor = app.hovered_in(&dir).and_then(|name| listing.position(name)).unwrap_or(0);
        draw_side(buf, preview_col, app, &dir, listing, Some(cursor));
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

/// Returns where the terminal cursor goes when a text prompt is open.
fn draw_status(buf: &mut Buffer, area: Rect, app: &App) -> Option<(u16, u16)> {
    let width = area.width as usize;
    let input = |buf: &mut Buffer, label: &str, input: &Input| {
        buf.set_stringn(area.x, area.y, format!("{label}{}", input.text), width, Style::new());
        Some((area.x + (label.len() + input.cursor_width()).min(width) as u16, area.y))
    };
    match &app.prompt {
        Some(Prompt::Create(i)) => return input(buf, "Create: ", i),
        Some(Prompt::Rename { input: i, .. }) => return input(buf, "Rename: ", i),
        Some(Prompt::Confirm { question, .. }) => {
            buf.set_stringn(area.x, area.y, question, width, Style::new());
            return None;
        }
        None => {}
    }

    if let Some(err) = &app.error {
        buf.set_stringn(area.x, area.y, err, width, ERROR);
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
        buf.set_stringn(area.x, area.y, "loading…", width, DIM);
    }

    let mut right = String::new();
    if app.in_visual() {
        right.push_str("VISUAL  ");
    }
    if !app.selected.is_empty() {
        right.push_str(&format!("{} selected  ", app.selected.len()));
    }
    let len = app.entries().len();
    if len > 0 {
        right.push_str(&format!("{}/{len}", app.cursor + 1));
    }
    let x = area.right().saturating_sub(right.chars().count() as u16);
    buf.set_stringn(x, area.y, right, width, Style::new());
    None
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
        let style = if i == menu.cursor { CURSOR } else { Style::new() };
        buf.set_style(Rect { y, height: 1, ..inner }, style);
        buf.set_stringn(inner.x + 1, y, format!("{} {}", i + 1, opener.desc), (inner.width as usize).saturating_sub(1), style);
    }
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
    let total: u32 = RATIO.iter().map(|&r| r as u32).sum();
    let width = |r: u16| (area.width as u32 * r as u32 / total) as u16;
    let (w0, w1) = (width(RATIO[0]), width(RATIO[1]));
    [
        Rect { width: w0, ..area },
        Rect { x: area.x + w0, width: w1, ..area },
        Rect { x: area.x + w0 + w1, width: area.width - w0 - w1, ..area },
    ]
}

/// Draws a column with no scroll state of its own, scrolled just enough to show `cursor`.
fn draw_side(buf: &mut Buffer, area: Rect, app: &App, dir: &Path, listing: &Listing, cursor: Option<usize>) {
    let height = area.height as usize;
    let so = SCROLLOFF.min(height.saturating_sub(1) / 2);
    let offset = cursor
        .map_or(0, |c| (c + so + 1).saturating_sub(height))
        .min(listing.entries.len().saturating_sub(height));
    draw_list(buf, area, app, dir, listing, cursor, offset);
}

fn draw_list(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    dir: &Path,
    listing: &Listing,
    cursor: Option<usize>,
    offset: usize,
) {
    let width = area.width.saturating_sub(2) as usize;
    if let Some(err) = &listing.error {
        buf.set_stringn(area.x + 1, area.y, err, width, ERROR);
        return;
    }
    let rows = listing.entries.iter().enumerate().skip(offset).take(area.height as usize);
    for (y, (i, entry)) in (area.y..).zip(rows) {
        let mut style = if entry.is_dir {
            DIR
        } else if entry.is_link {
            LINK
        } else {
            Style::new()
        };
        if Some(i) == cursor {
            buf.set_style(Rect { y, height: 1, ..area }, CURSOR);
            style = style.patch(CURSOR);
        }
        buf.set_stringn(area.x + 1, y, entry.name.to_string_lossy(), width, style);
        if let Some(mark) = app.mark(dir, i, entry)
            && let Some(cell) = buf.cell_mut((area.x, y))
        {
            // Reset first so the cursor's reverse video doesn't swap the colour away.
            cell.reset();
            cell.set_style(match mark {
                Mark::Selected => MARK_SELECTED,
                Mark::Copied => MARK_COPIED,
                Mark::Cut => MARK_CUT,
            });
        }
    }
}
