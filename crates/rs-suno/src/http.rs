//! A reqwest-backed adapter for the engine's [`Http`](suno_core::Http) port.

use std::future::Future;

use suno_core::{Http, HttpRequest, HttpResponse, Method, TransportError};

const USER_AGENT: &str = concat!("rs-suno/", env!("CARGO_PKG_VERSION"));

/// An [`Http`] adapter backed by a shared [`reqwest::Client`].
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// Build an adapter with a default client and the rs-suno user agent.
    pub fn new() -> reqwest::Result<Self> {
        let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
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
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
            };
            let mut builder = client.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if !request.body.is_empty() {
                builder = builder
                    .header("content-type", "application/json")
                    .body(request.body);
            }
            let response = builder
                .send()
                .await
                .map_err(|err| TransportError(err.to_string()))?;
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
            let body = response
                .bytes()
                .await
                .map_err(|err| TransportError(err.to_string()))?
                .to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        }
    }
}
