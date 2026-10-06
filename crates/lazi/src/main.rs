use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{env, io, process};

use lazi::{LOAD_GRACE, Lazi, Outcome, normalize, wake};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle};
use ratatui::crossterm::{execute, queue};

enum Exit {
    Quit,
    QuitNoCwd,
    /// Time to first frame.
    Bench(Duration),
}

fn main() -> io::Result<()> {
    let start = Instant::now();
    let mut bench = false;
    let mut cwd_file = None;
    let mut config = None;
    let mut dir = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--bench" {
            bench = true;
        } else if arg == "--cwd-file" {
            cwd_file = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--cwd-file=") {
            cwd_file = Some(PathBuf::from(OsStr::from_bytes(path)));
        } else if arg == "--config" {
            config = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.as_bytes().strip_prefix(b"--config=") {
            config = Some(PathBuf::from(OsStr::from_bytes(path)));
        } else {
            dir = Some(PathBuf::from(arg));
        }
    }
    let mut lazi = match Lazi::new(config, dir) {
        Ok(lazi) => lazi,
        Err(e) => {
            eprintln!("lazi: {e}");
            process::exit(1);
        }
    };

    let mut term = ratatui::init();
    let res = run(&mut term, &mut lazi, bench.then_some(start));
    let _ = lazi.clear_images(term.backend_mut());
    ratatui::restore();

    match res? {
        Exit::Quit => {
            // For a shell wrapper to cd into where lazi left off.
            if let Some(path) = cwd_file {
                std::fs::write(path, lazi.cwd().as_os_str().as_bytes())?;
            }
        }
        Exit::QuitNoCwd => {}
        Exit::Bench(elapsed) => eprintln!("first frame: {elapsed:?} ({} entries)", lazi.entry_count()),
    }
    Ok(())
}

fn run(term: &mut DefaultTerminal, lazi: &mut Lazi, bench: Option<Instant>) -> io::Result<Exit> {
    let (tty, _tty_file) = wake::tty()?;
    let winch = wake::winch()?;
    let mut fds = vec![tty, winch.as_raw_fd()];
    fds.extend(lazi.wake_fds());

    let mut dirty = true;
    loop {
        if dirty {
            // Hold the frame briefly so fast reads land in it instead of flashing an empty column.
            // A benchmark waits for everything, so it measures a complete frame.
            lazi.receive(if bench.is_some() { None } else { Some(LOAD_GRACE) });
            // Every frame, since programs lazi hands the terminal to may have changed it.
            queue!(term.backend_mut(), SetTitle(format!("lazi: {}", lazi.title())))?;
            draw(term, lazi)?;
            dirty = false;
            if let Some(start) = bench {
                return Ok(Exit::Bench(start.elapsed()));
            }
        }

        // Sleep until a key, a worker thread, a filesystem change or a resize, unless crossterm
        // already has input buffered from an earlier read.
        if !event::poll(Duration::ZERO)? {
            wake::wait(&fds)?;
        }
        dirty |= wake::drain(winch.as_raw_fd());
        dirty |= lazi.on_wake();
        while event::poll(Duration::ZERO)? {
            dirty = true;
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match lazi.key(term, normalize(key))? {
                Outcome::Continue => {}
                Outcome::Quit => return Ok(Exit::Quit),
                Outcome::QuitNoCwd => return Ok(Exit::QuitNoCwd),
                // Only other panels in a host return this.
                Outcome::Open(_) => {}
            }
        }
        dirty |= lazi.receive(Some(Duration::ZERO));
    }
}

/// Wraps each frame in a synchronized update so the terminal never shows a half-drawn frame.
fn draw(term: &mut DefaultTerminal, lazi: &mut Lazi) -> io::Result<()> {
    queue!(term.backend_mut(), BeginSynchronizedUpdate)?;
    term.draw(|frame| {
        let area = frame.area();
        lazi.draw(frame, area);
    })?;
    lazi.sync_image(term.backend_mut())?;
    execute!(term.backend_mut(), EndSynchronizedUpdate)
}
