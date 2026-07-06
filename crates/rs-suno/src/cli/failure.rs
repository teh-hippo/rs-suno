//! Map an auth or listing failure to its exit code, with the #250 redaction.
//!
//! Both helpers keep the underlying [`suno_core::Error`] out of any leak-prone
//! path: [`report_auth_failure`] swallows the error entirely (its `session_id`
//! must never reach stderr), and [`report_listing_failure`] classifies the error
//! into a transient-vs-general exit code, only interpolating the connection or
//! general variants whose `Display` is already safe.

use suno_core::Error as CoreError;

use crate::cli::desired::ExitCode;
use crate::cli::task_output::eprint_t;

pub(crate) fn report_auth_failure(label: &str, err: &CoreError) -> ExitCode {
    eprint_t!(
        "error: authentication failed for account '{label}'\n\nThe stored token may have expired. Re-authenticate with:\n  suno auth refresh {label}\n\nIf the token was rotated in Suno, update it with:\n  suno config add-account {label} --token <new-token>"
    );
    let _ = err;
    ExitCode::Auth
}

pub(crate) fn report_listing_failure(label: &str, err: &CoreError) -> ExitCode {
    match err {
        CoreError::Auth(_) => report_auth_failure(label, err),
        CoreError::Connection(_) | CoreError::RateLimited { .. } => {
            eprint_t!(
                "error: could not list the library for '{label}': {err}\n  No files were written. Re-run when connectivity is restored."
            );
            ExitCode::Transient
        }
        other => {
            eprint_t!("error: could not list the library for '{label}': {other}");
            ExitCode::General
        }
    }
}
