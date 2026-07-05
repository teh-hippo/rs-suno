//! A reqwest-backed adapter for the engine's [`Http`](suno_core::Http) port.

use std::future::Future;

use suno_core::{Http, HttpRequest, HttpResponse, Method, TransportError};

const USER_AGENT: &str = concat!("rs-suno/", env!("CARGO_PKG_VERSION"));

/// Suno-controlled domains this client is allowed to reach. Every request the
/// tool makes -- the Suno API, Clerk auth, and CDN downloads -- targets one of
/// these, so confining egress to them turns a hostile API-supplied download URL
/// (`image_url`, `audio_url`, a redirect, ...) into a rejected request rather
/// than an SSRF to an internal or link-local address (#246).
const ALLOWED_HOSTS: [&str; 2] = ["suno.ai", "suno.com"];

/// Whether `host` is a Suno-controlled host: one of [`ALLOWED_HOSTS`] exactly,
/// or a subdomain of one. Case- and trailing-dot-insensitive.
fn host_allowed(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ALLOWED_HOSTS.iter().any(|base| {
        host == *base
            || host
                .strip_suffix(base)
                .and_then(|label| label.strip_suffix('.'))
                .is_some()
    })
}

/// Whether `url` is an `https` request to an allow-listed Suno host. Parsing is
/// delegated to reqwest's own URL type so the check cannot diverge from the host
/// reqwest connects to (`user:pass@host`, backslashes, and the like).
fn url_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(host_allowed)
}

/// Map a reqwest transport error to the port's error, stripping the request URL
/// so a Clerk `session_id` embedded in the token-mint URL can never reach
/// shareable `doctor`/listing output (#250). The URL is the only place the
/// session_id appears; the cookie and JWT travel in headers reqwest never
/// prints.
fn map_transport_err(err: reqwest::Error) -> TransportError {
    TransportError(err.without_url().to_string())
}

/// An [`Http`] adapter backed by a shared [`reqwest::Client`].
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// Build an adapter with the rs-suno user agent and an egress policy that
    /// confines every request -- and every redirect hop -- to Suno hosts (#246).
    pub fn new() -> reqwest::Result<Self> {
        let redirect = reqwest::redirect::Policy::custom(|attempt| {
            if url_allowed(attempt.url()) {
                // Keep reqwest's default hop limit for allowed hosts.
                reqwest::redirect::Policy::default().redirect(attempt)
            } else {
                let host = attempt.url().host_str().unwrap_or("<none>").to_owned();
                attempt.error(format!(
                    "refusing redirect to a non-allowlisted host: {host}"
                ))
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(redirect)
            .build()?;
        Ok(Self { client })
    }
}

impl Http for ReqwestHttp {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        let client = self.client.clone();
        async move {
            let url = reqwest::Url::parse(&request.url)
                .map_err(|_| TransportError("refusing a malformed request URL".to_string()))?;
            if !url_allowed(&url) {
                return Err(TransportError(format!(
                    "refusing a request to a non-allowlisted host: {}",
                    url.host_str().unwrap_or("<none>")
                )));
            }
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
            };
            let mut builder = client.request(method, url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if !request.body.is_empty() {
                builder = builder
                    .header("content-type", "application/json")
                    .body(request.body);
            }
            let response = builder.send().await.map_err(map_transport_err)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            let body = response.bytes().await.map_err(map_transport_err)?.to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(url: &str) -> bool {
        url_allowed(&reqwest::Url::parse(url).unwrap())
    }

    #[test]
    fn allows_https_suno_hosts_and_subdomains() {
        assert!(allowed("https://cdn1.suno.ai/a.mp3"));
        assert!(allowed("https://cdn2.suno.ai/image.jpeg"));
        assert!(allowed("https://audiopipe.suno.ai/item"));
        assert!(allowed("https://studio-api-prod.suno.com/api/feed/v3"));
        assert!(allowed("https://auth.suno.com/v1/client"));
        assert!(allowed("https://suno.ai/"));
        assert!(allowed("https://suno.com/"));
        // The host is lower-cased by the parser and the check.
        assert!(allowed("https://CDN1.SUNO.AI/a.mp3"));
        // A fully-qualified trailing dot resolves to the same host.
        assert!(allowed("https://cdn1.suno.ai./a.mp3"));
    }

    #[test]
    fn rejects_plaintext_foreign_and_internal_hosts() {
        // Not https.
        assert!(!allowed("http://cdn1.suno.ai/a.mp3"));
        // Cloud metadata / link-local / loopback -- the SSRF targets.
        assert!(!allowed("http://169.254.169.254/latest/meta-data/"));
        assert!(!allowed("https://169.254.169.254/latest/meta-data/"));
        assert!(!allowed("https://localhost:8080/x"));
        assert!(!allowed("https://127.0.0.1/x"));
        assert!(!allowed("https://[::1]/x"));
        // Foreign hosts and near-miss look-alikes.
        assert!(!allowed("https://evil.com/x"));
        assert!(!allowed("https://evilsuno.ai/x"));
        assert!(!allowed("https://suno.ai.evil.com/x"));
        // Userinfo trick: the real host is evil.com, not cdn1.suno.ai.
        assert!(!allowed("https://cdn1.suno.ai@evil.com/x"));
        // A CDN Suno itself uses for some assets is still off the allow-list.
        assert!(!allowed("https://d2lwuy8qc234o3.cloudfront.net/clip.m4a"));
    }

    #[tokio::test]
    async fn transport_error_omits_the_request_url() {
        // A refused local connection yields a reqwest error carrying the URL;
        // the adapter must strip it so a Clerk session id in the path (or any
        // download URL) never reaches shareable output (#250).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let secret = "sess_LEAKED_SESSION_ID";
        let url = format!("http://127.0.0.1:1/v1/client/sessions/{secret}/tokens");
        let err = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect_err("a connection to 127.0.0.1:1 must fail");
        let mapped = map_transport_err(err);
        assert!(
            !mapped.0.contains(secret),
            "session id leaked: {}",
            mapped.0
        );
        assert!(!mapped.0.contains("127.0.0.1"), "url leaked: {}", mapped.0);
    }
}
