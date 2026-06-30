//! A test-only in-memory [`Http`] double.

use std::future::Future;

use crate::http::{Http, HttpRequest, HttpResponse, TransportError};

/// A canned reply for any request whose URL contains `url_contains`.
pub(crate) struct Rule {
    url_contains: &'static str,
    status: u16,
    body: String,
}

impl Rule {
    pub(crate) fn new(url_contains: &'static str, status: u16, body: String) -> Self {
        Self {
            url_contains,
            status,
            body,
        }
    }
}

/// An [`Http`] double that replies from the first matching [`Rule`], in order.
pub(crate) struct MockHttp {
    rules: Vec<Rule>,
}

impl MockHttp {
    pub(crate) fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }
}

impl Http for MockHttp {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        let reply = self
            .rules
            .iter()
            .find(|rule| request.url.contains(rule.url_contains))
            .map(|rule| HttpResponse {
                status: rule.status,
                body: rule.body.clone().into_bytes(),
            })
            .ok_or_else(|| TransportError(format!("no rule matched {}", request.url)));
        async move { reply }
    }
}
