//! Showing image previews with kitty's graphics protocol.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::{env, fs, process};

use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::{queue, terminal};

use crate::preview::Image;

/// What the terminal holds, so a frame only sends escapes when something changed.
#[derive(Default)]
pub struct Kitty {
    /// The image whose data the terminal has.
    sent: Option<u32>,
    /// That image's placement on screen: id and cell position.
    placed: Option<(u32, u16, u16)>,
}

impl Kitty {
    /// Makes the screen show `want` (an image and the cell for its top-left corner), or nothing.
    pub fn sync(&mut self, out: &mut impl Write, want: Option<(&Image, u16, u16)>) -> io::Result<()> {
        let key = want.map(|(img, x, y)| (img.id, x, y));
        if self.placed == key {
            return Ok(());
        }
        if let Some((id, ..)) = self.placed.take() {
            // Lowercase i: drop the placement, keep the data.
            write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\")?;
        }
        let Some((img, x, y)) = want else { return Ok(()) };
        if self.sent != Some(img.id) {
            if let Some(old) = self.sent.take() {
                write!(out, "\x1b_Ga=d,d=I,i={old},q=2\x1b\\")?;
            }
            // t=t: kitty reads the file and deletes it. It insists on a temp directory and this
            // marker in the name.
            let path = env::temp_dir().join(format!("lazi-tty-graphics-protocol-{}-{}", process::id(), img.id));
            fs::write(&path, &img.rgba)?;
            let (w, h, id) = (img.width, img.height, img.id);
            write!(out, "\x1b_Ga=t,t=t,f=32,s={w},v={h},i={id},q=2;{}\x1b\\", base64(path.as_os_str().as_bytes()))?;
            self.sent = Some(img.id);
        }
        // Save and restore the cursor around the placement, so a prompt's cursor stays put.
        write!(out, "\x1b7")?;
        queue!(out, MoveTo(x, y))?;
        write!(out, "\x1b_Ga=p,i={},p=1,C=1,q=2\x1b\\\x1b8", img.id)?;
        self.placed = key;
        Ok(())
    }

    /// Deletes every image, e.g. before handing the terminal to another program.
    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        *self = Self::default();
        write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\")?;
        out.flush()
    }
}

/// A cell's size in pixels, from the terminal (kitty always reports it).
pub fn cell_size() -> (u32, u32) {
    match terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.columns > 0 && ws.rows > 0 => {
            ((ws.width / ws.columns) as u32, (ws.height / ws.rows) as u32)
        }
        _ => (8, 16),
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
