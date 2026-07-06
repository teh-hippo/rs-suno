//! Interrupt handling: resolve when a SIGINT (or, on Unix, SIGTERM) arrives.

/// Resolve when a SIGINT (Ctrl-C) or, on Unix, a SIGTERM arrives.
///
/// `ctrl_c` is cross-platform; the extra `SIGTERM` arm is Unix-only because
/// Windows has no such signal.
pub(crate) async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
