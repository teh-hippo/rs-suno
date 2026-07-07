//! The Suno API client test suite: end-to-end scenarios that script the
//! [`Http`](crate::Http) port with in-memory doubles and drive `SunoClient`
//! methods, asserting feed paging, retry/pacing, and each endpoint's decode.

use super::*;
use crate::testutil::{MockHttp, RecordingClock, Reply, Rule, ScriptedHttp};
use serde_json::Value;
use std::time::Duration;

fn feed_body() -> String {
    serde_json::json!({
        "has_more": false,
        "clips": [
            {
                "id": "a", "title": "Song A", "status": "complete",
                "audio_url": "https://cdn1.suno.ai/a.mp3",
                "metadata": {"tags": "rock", "duration": 120.5, "type": "gen"}
            },
            {"id": "b", "title": "Infill", "status": "complete", "metadata": {"task": "infill"}},
            {"id": "c", "title": "Streaming", "status": "streaming", "metadata": {}},
            {
                "id": "d", "title": "Context", "status": "complete",
                "metadata": {"type": "rendered_context_window"}
            }
        ]
    })
    .to_string()
}

fn auth_rules() -> Vec<Rule> {
    let client_body = serde_json::json!({
        "response": {
            "last_active_session_id": "s",
            "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
        }
    })
    .to_string();
    vec![
        Rule::new(
            "/v1/client/sessions/",
            200,
            r#"{"jwt": "a.b.c"}"#.to_string(),
        ),
        Rule::new("/v1/client", 200, client_body),
    ]
}

fn authed_client(http: &MockHttp) -> SunoClient<RecordingClock> {
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(http)).unwrap();
    SunoClient::new(auth, RecordingClock::new())
}

fn scripted_client(http: &ScriptedHttp, clock: RecordingClock) -> SunoClient<RecordingClock> {
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(http)).unwrap();
    SunoClient::new(auth, clock)
}

fn one_clip_page(id: &str, next_cursor: Option<&str>) -> String {
    let mut page = serde_json::json!({
        "has_more": next_cursor.is_some(),
        "clips": [{
            "id": id, "title": "Song", "status": "complete",
            "audio_url": format!("https://cdn1.suno.ai/{id}.mp3"),
            "metadata": {"type": "gen"}
        }]
    });
    if let Some(cursor) = next_cursor {
        page["next_cursor"] = serde_json::json!(cursor);
    }
    page.to_string()
}

mod billing_lyrics;
mod clips;
mod feed;
mod lineage;
mod playlists;
mod request;
mod stems;
