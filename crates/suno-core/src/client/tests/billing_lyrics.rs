use super::*;

#[test]
fn get_billing_info_reads_remaining_credits() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        BILLING_INFO_PATH,
        200,
        r#"{"total_credits_left":500,"monthly_limit":1000,"monthly_usage":500}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let billing = pollster::block_on(client.get_billing_info(&http)).unwrap();
    assert_eq!(billing.total_credits_left, Some(500));
    assert_eq!(billing.monthly_limit, Some(1000));
    assert_eq!(billing.monthly_usage, Some(500));
}

#[test]
fn get_billing_info_tolerates_missing_balance() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        BILLING_INFO_PATH,
        200,
        r#"{"monthly_usage":12}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let billing = pollster::block_on(client.get_billing_info(&http)).unwrap();
    assert_eq!(billing.total_credits_left, None);
    assert_eq!(billing.monthly_usage, Some(12));
}

#[test]
fn aligned_lyrics_reads_words_and_lines() {
    let mut rules = auth_rules();
    let body = serde_json::json!({
        "aligned_words": [
            {"word": "hi", "success": true, "start_s": 0.5, "end_s": 0.9, "p_align": 0.99}
        ],
        "aligned_lyrics": [
            {"text": "hi", "start_s": 0.5, "end_s": 0.9, "section": "Verse 1",
             "words": [{"text": "hi", "start_s": 0.5, "end_s": 0.9}]}
        ],
        "hoot_cer": 0.2, "is_streamed": false
    })
    .to_string();
    rules.push(Rule::new("/aligned_lyrics/v2/", 200, body));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let aligned = pollster::block_on(client.aligned_lyrics(&http, "clip-1")).unwrap();
    assert_eq!(aligned.words.len(), 1);
    assert_eq!(aligned.lines.len(), 1);
    assert_eq!(aligned.lines[0].section, "Verse 1");
    assert!(!aligned.is_empty());
}

#[test]
fn aligned_lyrics_empty_arrays_map_to_empty() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/aligned_lyrics/v2/",
        200,
        r#"{"aligned_words":[],"aligned_lyrics":[],"hoot_cer":1.0}"#.to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let aligned = pollster::block_on(client.aligned_lyrics(&http, "instr")).unwrap();
    assert!(aligned.is_empty());
}

#[test]
fn aligned_lyrics_maps_404_to_empty() {
    let mut rules = auth_rules();
    rules.push(Rule::new(
        "/aligned_lyrics/v2/",
        404,
        "not found".to_string(),
    ));
    let http = MockHttp::new(rules);
    let client = authed_client(&http);

    let aligned = pollster::block_on(client.aligned_lyrics(&http, "missing")).unwrap();
    assert!(aligned.is_empty());
}
