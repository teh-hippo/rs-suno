//! Rendered-WAV response decoding (the `/api/gen/{id}/convert_wav/` poll).

use serde_json::Value;

use crate::error::{Error, Result};

/// Parse the rendered-WAV response body (`{"wav_file_url": "..."}`).
///
/// Returns the URL when present and non-empty, `None` when the render is not
/// ready (an absent or empty `wav_file_url`), and an [`Error::Api`] only for
/// bytes that are not valid JSON.
pub(crate) fn parse_wav_url(body: &[u8]) -> Result<Option<String>> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid wav_file JSON: {err}")))?;
    Ok(data
        .get("wav_file_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wav_url_reads_ready_treats_empty_as_pending_and_rejects_junk() {
        // A ready render returns the url.
        assert_eq!(
            parse_wav_url(br#"{"wav_file_url": "https://cdn1.suno.ai/z.wav"}"#).unwrap(),
            Some("https://cdn1.suno.ai/z.wav".to_owned())
        );
        // An absent or empty wav_file_url means "not ready yet", not an error.
        assert_eq!(parse_wav_url(br#"{}"#).unwrap(), None);
        assert_eq!(parse_wav_url(br#"{"wav_file_url": ""}"#).unwrap(), None);
        // Only non-JSON bytes are an error.
        assert!(parse_wav_url(b"nope").is_err());
    }
}
