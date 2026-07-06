//! Per-account stderr buffering for the concurrent multi-account run path.
//!
//! [`eprint_t!`] writes through the thread-local [`TASK_STDERR`] buffer when a
//! concurrent account thread has activated it, so lines from parallel accounts
//! never interleave; a single-account (sequential) run leaves the buffer unset
//! and writes straight to stderr.

std::thread_local! {
    /// Per-account stderr buffer. When active (multi-account concurrent path),
    /// `eprint_t!` writes here instead of directly to stderr, so concurrent
    /// accounts' output lines never interleave. Flushed atomically after each
    /// account's thread completes.
    pub(crate) static TASK_STDERR: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Write a formatted line to the per-account buffer when in a concurrent thread,
/// or directly to stderr for single-account (sequential) runs.
macro_rules! eprint_t {
    ($($arg:tt)*) => {{
        $crate::cli::task_output::TASK_STDERR.with(|b| {
            let mut guard = b.borrow_mut();
            if let Some(buf) = guard.as_mut() {
                buf.push(format!($($arg)*));
            } else {
                eprintln!($($arg)*);
            }
        });
    }};
}
pub(crate) use eprint_t;

/// Begin buffering this thread's `eprint_t!` output (multi-account path).
pub(crate) fn capture_task_stderr() {
    TASK_STDERR.with(|b| *b.borrow_mut() = Some(Vec::new()));
}

/// Take and clear this thread's buffered `eprint_t!` output, for an atomic flush
/// after the account's thread completes.
pub(crate) fn flush_task_stderr() -> Vec<String> {
    TASK_STDERR.with(|b| b.borrow_mut().take().unwrap_or_default())
}
