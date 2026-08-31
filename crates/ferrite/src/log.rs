//! Logging, for use while the TUI owns the terminal.
//!
//! Once `ratatui::init()` has switched to the alternate screen, stdout *is* the
//! display: a `println!` is either overwritten by the next frame or shifts the
//! layout, and `restore()` throws the alternate screen away on exit. So debug
//! output has to leave the terminal entirely.
//!
//! Two sinks, one for each audience:
//!
//! - [`debug!`] — file only. Firehose; tail it from a second terminal with
//!   `tail -f ferrite.log`. Truncated on first write, so each run starts clean.
//!   Override the path with `FERRITE_LOG=/tmp/whatever cargo run -p ferrite`.
//! - [`log_info!`] / [`log_warn!`] / [`log_error!`] — file *and* the in-TUI log
//!   panel. For events with a human cause: a retune, a setting the hardware
//!   silently refused, a device error.
//!
//! # Not from the hot path
//!
//! **Nothing on the USB, DSP or audio path may call any of these.** Both sinks
//! take a mutex and the panel sink allocates a `String`; either can park a
//! thread that has a hard deadline, which is the exact failure the three-thread
//! split in PLAN.md §1 exists to prevent. A dropped ring block or an underrun is
//! counted in an atomic and rendered by the Info panel (PLAN.md R1.3) — "a full
//! or empty ring is a defect to **measure**", not to narrate. Only the UI and
//! controller threads log.

use std::collections::VecDeque;
use std::fs::File;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

/// The open log file, created on first use.
pub fn file() -> &'static Mutex<File> {
    LOG.get_or_init(|| {
        let path = std::env::var("FERRITE_LOG").unwrap_or_else(|_| "ferrite.log".into());
        Mutex::new(File::create(&path).unwrap_or_else(|e| panic!("log: cannot open {path}: {e}")))
    })
}

/// `println!` that survives the TUI. Same formatting syntax. File only — see
/// [`log_info!`] for lines that should also reach the panel.
///
/// Flushed per call so a crash or a `cancel_async_read` on the way out cannot
/// swallow the last few lines — the point of the thing is usually the last line
/// before something went wrong.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        use std::io::Write;
        // A panic in one thread mid-write poisons the mutex; recovering the
        // guard keeps that from cascading into every other logging site.
        let mut f = $crate::log::file()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writeln!(f, $($arg)*);
        let _ = f.flush();
    }};
}

// ── In-TUI log panel ────────────────────────────────────────────────────────

/// How many entries the panel keeps. Oldest is dropped on overflow — the ring
/// is a tail, not an archive; `ferrite.log` is the archive.
const CAPACITY: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    /// Prefix written to the file. The panel colours instead of prefixing, so
    /// the narrow left column spends its width on the message.
    pub fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERR ",
        }
    }
}

pub struct Entry {
    /// Since process start. Wall-clock would need a date crate for one line of
    /// formatting, and elapsed is the more useful axis in a log you read while
    /// watching the radio do something.
    pub at: Instant,
    pub level: Level,
    pub text: String,
}

struct Ring {
    entries: VecDeque<Entry>,
    start: Instant,
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| {
        Mutex::new(Ring {
            entries: VecDeque::with_capacity(CAPACITY),
            start: Instant::now(),
        })
    })
}

/// Append to both sinks. Use the [`log_info!`] family rather than calling this.
pub fn record(level: Level, text: String) {
    debug!("[{}] {}", level.tag(), text);

    let mut r = ring().lock().unwrap_or_else(|p| p.into_inner());
    if r.entries.len() == CAPACITY {
        r.entries.pop_front();
    }
    r.entries.push_back(Entry {
        at: Instant::now(),
        level,
        text,
    });
}

/// Read the tail under the lock.
///
/// Borrowing rather than cloning keeps a redraw from allocating one `String`
/// per visible line every frame. The closure runs with the lock held, so it
/// must only format — never log, and never block.
pub fn with_entries<R>(f: impl FnOnce(&VecDeque<Entry>, Instant) -> R) -> R {
    let r = ring().lock().unwrap_or_else(|p| p.into_inner());
    f(&r.entries, r.start)
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Info, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Warn, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::record($crate::log::Level::Error, format!($($arg)*))
    };
}
