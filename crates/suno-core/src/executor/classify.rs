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

/// Classify a filesystem or transcode failure into the run-ending disk-full
/// abort or a per-clip permanent skip, from the port error's own out-of-space
/// flag.
///
/// A full disk is systemic: it aborts the run (exit 9) rather than skipping one
/// clip, so this is the single place the disk-vs-permanent verdict is decided,
/// keeping the call sites from drifting. Each site still supplies its own two
/// messages, because the wording is specific to the operation; only the verdict
/// is shared. The predicate is a plain `bool` (the caller reads its error's
/// `is_out_of_space` flag), so `suno-core` needs no `std::io`.
pub(super) fn disk_or_permanent(
    clip_id: impl Into<String>,
    out_of_space: bool,
    disk: impl Into<String>,
    permanent: impl Into<String>,
) -> Fail {
    if out_of_space {
        disk_fail(clip_id, disk)
    } else {
        permanent_fail(clip_id, permanent)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_or_permanent_out_of_space_aborts_the_run() {
        // The sole direct guard for the collapsed sites whose disk-full path has
        // no dedicated integration test (webp sidecar, delete_artifact, stem
        // remove-old, delete_stem, audio remove-old): out-of-space is systemic.
        let fail = disk_or_permanent(
            "clip-1",
            true,
            "disk full: no space left",
            "write failed: x",
        );
        assert!(matches!(fail.class, Class::Disk));
        assert_eq!(fail.clip_id, "clip-1");
        assert_eq!(fail.reason, "disk full: no space left");
        assert_eq!(abort_status(fail.class), Some(RunStatus::DiskFull));
    }

    #[test]
    fn disk_or_permanent_other_error_is_a_per_clip_skip() {
        // Any non-disk failure stays per-clip: the run continues, no exit-9.
        let fail = disk_or_permanent(
            "clip-2",
            false,
            "disk full: no space left",
            "write failed: x",
        );
        assert!(matches!(fail.class, Class::Permanent));
        assert_eq!(fail.clip_id, "clip-2");
        assert_eq!(fail.reason, "write failed: x");
        assert_eq!(abort_status(fail.class), None);
    }
}
