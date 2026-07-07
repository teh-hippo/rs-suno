use super::*;

#[test]
fn api_request_uses_clock_now_unix_for_jwt_expiry() {
    use crate::consts::JWT_REFRESH_BUFFER;
    use base64::Engine;
    let exp = 1_000_000i64;
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
    let jwt_str = format!("hdr.{}.sig", payload);
    let token_body = format!(r#"{{"jwt": "{jwt_str}"}}"#);
    let client_body = serde_json::json!({
        "response": {
            "last_active_session_id": "s",
            "sessions": [{"id": "s", "user": {"id": "u", "username": "h"}}]
        }
    })
    .to_string();

    let make_http = || {
        ScriptedHttp::new()
            .route("/v1/client/sessions/", Reply::json(&token_body))
            .route("/v1/client", Reply::json(&client_body))
            .route("/api/feed/v3", Reply::json(&feed_body()))
    };

    // At the refresh boundary: ensure_jwt triggers a second refresh_jwt call.
    let http = make_http();
    let auth = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth.authenticate(&http)).unwrap();
    let client = SunoClient::new(auth, RecordingClock::at(exp - JWT_REFRESH_BUFFER));
    let (clips, _, _) = pollster::block_on(client.list_clips(&http, false, None)).unwrap();
    assert_eq!(clips.len(), 1);
    // authenticate + api_request refresh = 2 token calls.
    assert_eq!(http.count("/v1/client/sessions/"), 2);

    // Just before the boundary: no additional refresh.
    let http2 = make_http();
    let auth2 = ClerkAuth::new("eyJtoken");
    pollster::block_on(auth2.authenticate(&http2)).unwrap();
    let client2 = SunoClient::new(auth2, RecordingClock::at(exp - JWT_REFRESH_BUFFER - 1));
    let (clips2, _, _) = pollster::block_on(client2.list_clips(&http2, false, None)).unwrap();
    assert_eq!(clips2.len(), 1);
    // Only authenticate's token call; no extra refresh.
    assert_eq!(http2.count("/v1/client/sessions/"), 1);
}

#[test]
fn post_allow_list_permits_only_feed_and_wav_render() {
    assert!(post_path_allowed(FEED_V3_PATH));
    assert!(post_path_allowed("/api/gen/abc123/convert_wav/"));
    // No generation endpoint is on the list.
    assert!(!post_path_allowed("/api/gen/abc123/stem_task"));
    assert!(!post_path_allowed("/api/gen/abc123/separate"));
    // Path traversal or extra segments can't smuggle a match.
    assert!(!post_path_allowed("/api/gen/a/../evil/convert_wav/"));
    assert!(!post_path_allowed("/api/gen/a/b/convert_wav/"));
    // The stems endpoints are GET-only and never on the POST allow-list.
    assert!(!post_path_allowed("/api/clip/x/stems/pages"));
    assert!(!post_path_allowed("/api/clip/x/stems?page=0"));
}

#[test]
fn api_request_refuses_a_post_off_the_allow_list() {
    // The single POST chokepoint rejects an off-list POST before the wire, so
    // a credit-spending endpoint can never be reached by accident.
    let http = MockHttp::new(auth_rules());
    let client = authed_client(&http);
    let err = pollster::block_on(client.api_request(
        &http,
        Method::Post,
        "/api/gen/x/stem_task",
        b"{}".to_vec(),
    ))
    .unwrap_err();
    assert!(matches!(err, Error::Refused(_)));
}
