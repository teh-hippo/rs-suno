//! Failure classification for the executor: turns raw core errors and HTTP
//! results into a [`Class`] (SYNC-17), the per-clip [`Fail`] the run loop
//! records, and the run-ending status a fatal class demands.

use super::*;

/// How a failure should be handled (SYNC-17).
#[derive(Debug, Clone, Copy)]
pub(super) enum Class {
    /// Stop the account run; do not retry.
    Auth,
    /// Stop the account run: a full disk is systemic, like auth, so aborting
    /// beats skipping every remaining clip (each of which would first burn a
    /// server-side WAV-render budget before failing the same way).
    Disk,
    /// Retry a bounded number of times, then record and skip.
    Transient,
    /// Record and skip immediately.
    Permanent,
}

/// A classified action failure attributed to a clip.
pub(super) struct Fail {
    pub(super) class: Class,
    pub(super) clip_id: String,
    pub(super) reason: String,
}

/// The run-ending status for a failure class, or `None` when the failure is
/// per-clip and the run continues.
pub(super) fn abort_status(class: Class) -> Option<RunStatus> {
    match class {
        Class::Auth => Some(RunStatus::AuthAborted),
        Class::Disk => Some(RunStatus::DiskFull),
        Class::Transient | Class::Permanent => None,
    }
}

pub(super) fn auth_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Auth,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

pub(super) fn transient_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Transient,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

pub(super) fn permanent_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Permanent,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

pub(super) fn disk_fail(clip_id: impl Into<String>, reason: impl Into<String>) -> Fail {
    Fail {
        class: Class::Disk,
        clip_id: clip_id.into(),
        reason: reason.into(),
    }
}

/// A classified fetch failure, not yet attributed to a clip.
pub(super) struct FetchError {
    pub(super) class: Class,
    reason: String,
    pub(super) retry_after: Option<Duration>,
}

impl FetchError {
    fn transient(reason: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            class: Class::Transient,
            reason: reason.into(),
            retry_after,
        }
    }

    fn permanent(reason: impl Into<String>) -> Self {
        Self {
            class: Class::Permanent,
            reason: reason.into(),
            retry_after: None,
        }
    }

    pub(super) fn attribute(self, clip_id: &str) -> Fail {
        Fail {
            class: self.class,
            clip_id: clip_id.to_owned(),
            reason: self.reason,
        }
    }
}

/// Classify one HTTP result into bytes or a [`FetchError`] (SYNC-14/17).
pub(super) fn classify_response(
    result: Result<crate::http::HttpResponse, crate::http::TransportError>,
) -> Result<Vec<u8>, FetchError> {
    let response = match result {
        Ok(response) => response,
        Err(err) => {
            return Err(FetchError::transient(
                format!("transport error: {err}"),
                None,
            ));
        }
    };
    match response.status {
        200..=299 => {
            if let Some(expected) = content_length(&response) {
                let actual = response.body.len() as u64;
                if actual != expected {
                    return Err(FetchError::transient(
                        format!("truncated download: {actual} of {expected} bytes"),
                        None,
                    ));
                }
            }
            Ok(response.body)
        }
        401 | 403 => Err(FetchError::transient(
            format!("download rejected: status {}", response.status),
            None,
        )),
        408 => Err(FetchError::transient("request timed out", None)),
        429 => Err(FetchError::transient(
            "rate limited",
            retry_after(&response),
        )),
        500..=599 => Err(FetchError::transient(
            format!("server error {}", response.status),
            None,
        )),
        status => Err(FetchError::permanent(format!(
            "download failed: status {status}"
        ))),
    }
}

/// Map a core [`Error`] from the authenticated WAV flow to a [`Fail`].
pub(super) fn classify_core(id: &str, err: Error) -> Fail {
    let reason = err.to_string();
    match err {
        Error::Auth(_) => auth_fail(id, reason),
        Error::RateLimited { .. } | Error::Connection(_) => transient_fail(id, reason),
        Error::Api(_)
        | Error::BadRequest(_)
        | Error::NotFound(_)
        | Error::Tag(_)
        | Error::Config(_)
        | Error::Refused(_) => permanent_fail(id, reason),
    }
}

/// The provider-reported body size from `Content-Length`, if present and valid.
pub(super) fn content_length(response: &crate::http::HttpResponse) -> Option<u64> {
    response.header("content-length")?.trim().parse().ok()
}
