//! Logging, for use while the TUI owns the terminal.
//!
//! Once `ratatui::init()` has switched to the alternate screen, stdout *is* the
//! display: a `println!` is either overwritten by the next frame or shifts the
//! layout, and `restore()` throws the alternate screen away on exit. So log
//! output has to leave the terminal entirely.
//!
//! Two sinks:
//!
//! - **Trace file**, only when the app runs with `--trace [path]` (default
//!   `ferrite-sdr.log`). Every macro writes here. Truncated at startup, so each run
//!   starts clean; tail it from a second terminal with `tail -f ferrite-sdr.log`.
//!   Without `--trace` nothing is written anywhere.
//! - **Alert slot** — [`log_warn!`] and [`log_error!`] also leave their message
//!   for the TUI, which flashes it on the status bar. There is no log panel any
//!   more (it never fit on a small screen), and a warning that only reaches a
//!   file nobody opened is a warning nobody sees.
//!
//! [`debug!`] and [`log_info!`] go to the trace file only.
//!
//! # Not from the hot path
//!
//! **Nothing on the USB, DSP or audio path may call any of these.** The sinks
//! take a mutex and format a `String`; either can park a thread that has a hard
//! deadline, which is the exact failure the three-thread split in PLAN.md §1
//! exists to prevent. A dropped ring block or an underrun is counted in an
//! atomic and rendered by the Info panel (PLAN.md R1.3) — "a full or empty ring
//! is a defect to **measure**", not to narrate. Only the UI and controller
//! threads log.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Instant;

/// `None` when tracing is off. Set once by [`init`]; unset reads as off, so a
/// log call that somehow runs before `init` is dropped rather than panicking.
static TRACE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Process start, for the elapsed-time stamp on each line.
static START: OnceLock<Instant> = OnceLock::new();

/// The latest warning or error the TUI has not shown yet. One slot, not a
/// queue: the status bar only ever shows one message, and a burst of warnings
/// should end on the newest.
static ALERT: Mutex<Option<(Level, String)>> = Mutex::new(None);

/// Open the trace file, or turn tracing off. Call once, first thing in `main`.
///
/// With tracing on, also installs a panic hook that writes the panic to the
/// file. A panic on the controller thread is otherwise invisible: the TUI owns
/// the screen, so the default hook's message is drawn over or thrown away with
/// the alternate screen, and the only symptom is every control going dead.
pub fn init(path: Option<&Path>) -> std::io::Result<()> {
    START.get_or_init(Instant::now);

    let file = match path {
        Some(p) => Some(Mutex::new(File::create(p)?)),
        None => None,
    };
    let tracing = file.is_some();
    let _ = TRACE.set(file);

    if tracing {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>");
            write_line(&format!("[PANIC] thread '{name}': {info}"));
            default(info);
        }));
    }
    Ok(())
}

/// Append one line to the trace file, if tracing. Flushed per call so a crash
/// or a `cancel_async_read` on the way out cannot swallow the last few lines —
/// the point of a trace is usually the last line before something went wrong.
pub fn write_line(text: &str) {
    let Some(Some(file)) = TRACE.get() else {
        return;
    };
    let secs = START.get().map_or(0.0, |s| s.elapsed().as_secs_f64());
    // A panic in one thread mid-write poisons the mutex; recovering the guard
    // keeps that from cascading into every other logging site.
    let mut f = file.lock().unwrap_or_else(|p| p.into_inner());
    let _ = writeln!(f, "{secs:9.3} {text}");
    let _ = f.flush();
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERR ",
        }
    }
}

/// The real stderr, parked by [`StderrRedirect`]; -1 when it is not redirected.
static SAVED_STDERR: AtomicI32 = AtomicI32::new(-1);

/// Points file descriptor 2 away from the terminal until dropped.
///
/// C libraries write straight to fd 2, which no Rust-side logging can catch:
/// librtlsdr prints `Allocating 15 zero-copy buffers…` on *every*
/// `rtlsdr_read_async`, so each resume from pause would land a line on the
/// bottom row of the TUI and scroll the whole frame up. With `--trace` that
/// output goes to the trace file, so it is still there to read; without it,
/// to `/dev/null`.
///
/// Create it before the source starts streaming and keep it until the
/// terminal is restored. A panic puts stderr back before the default hook
/// prints, so the message still reaches the screen.
pub struct StderrRedirect(());

impl StderrRedirect {
    pub fn new() -> std::io::Result<Self> {
        let target = match TRACE.get() {
            Some(Some(file)) => file
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .try_clone()?,
            _ => File::options().write(true).open("/dev/null")?,
        };
        // SAFETY: plain fd calls on descriptors this process owns; `target`
        // stays open until after the dup2, which takes its own reference.
        unsafe {
            let saved = libc::dup(libc::STDERR_FILENO);
            if saved < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(target.as_raw_fd(), libc::STDERR_FILENO) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(saved);
                return Err(err);
            }
            SAVED_STDERR.store(saved, Ordering::SeqCst);
        }

        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_stderr();
                prev(info);
            }));
        });
        Ok(Self(()))
    }
}

impl Drop for StderrRedirect {
    fn drop(&mut self) {
        restore_stderr();
    }
}

/// Undo [`StderrRedirect`]. Idempotent, so the panic hook and the guard's
/// drop can both call it.
fn restore_stderr() {
    let saved = SAVED_STDERR.swap(-1, Ordering::SeqCst);
    if saved >= 0 {
        // SAFETY: `saved` came from `dup` above and is closed exactly once,
        // guarded by the swap.
        unsafe {
            libc::dup2(saved, libc::STDERR_FILENO);
            libc::close(saved);
        }
    }
}

/// Use the [`log_info!`] family rather than calling this.
pub fn record(level: Level, text: String) {
    write_line(&format!("[{}] {}", level.tag(), text));
    if level != Level::Info {
        *ALERT.lock().unwrap_or_else(|p| p.into_inner()) = Some((level, text));
    }
}

/// Take the pending warning or error, if any. The TUI polls this once a frame.
pub fn take_alert() -> Option<(Level, String)> {
    ALERT.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// `println!` that survives the TUI. Same formatting syntax. Trace file only.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::write_line(&format!($($arg)*))
    };
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
