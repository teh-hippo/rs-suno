//! Billing info decoding (`/api/billing/info/`) and the tolerant number readers.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::BillingInfo;

/// Parse `/api/billing/info/` into the billing snapshot we report in `doctor`.
///
/// Only genuinely invalid JSON bytes fail; any valid JSON value (even a
/// non-object such as `null` or `[]`) degrades to [`BillingInfo::default`].
pub(crate) fn parse_billing_info(body: &[u8]) -> Result<BillingInfo> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|err| Error::Api(format!("invalid billing JSON: {err}")))?;
    Ok(from_billing_json(&data))
}

/// Map the raw billing JSON into the domain [`BillingInfo`].
///
/// Reads each field independently through `.get()`, defaulting to `None`/empty
/// on a missing key or type mismatch, and never fails on a single field.
/// `features` is the union of `accessible_features[].name` and
/// `plan.usage_plan_features[].name`.
fn from_billing_json(data: &Value) -> BillingInfo {
    let plan = data.get("plan");
    let mut features = BTreeSet::new();
    collect_feature_names(data.get("accessible_features"), &mut features);
    collect_feature_names(
        plan.and_then(|plan| plan.get("usage_plan_features")),
        &mut features,
    );
    BillingInfo {
        total_credits_left: data.get("total_credits_left").and_then(json_i64),
        monthly_limit: data.get("monthly_limit").and_then(json_i64),
        monthly_usage: data.get("monthly_usage").and_then(json_i64),
        credits: data.get("credits").and_then(json_i64),
        period: json_string(data.get("period")),
        period_end: json_string(data.get("period_end")),
        renews_on: json_string(data.get("renews_on")),
        is_active: data.get("is_active").and_then(Value::as_bool),
        is_paused: data.get("is_paused").and_then(Value::as_bool),
        is_past_due: data.get("is_past_due").and_then(Value::as_bool),
        is_gifted: data.get("is_gifted").and_then(Value::as_bool),
        subscription_platform: json_string(data.get("subscription_platform")),
        plan_key: json_string(plan.and_then(|plan| plan.get("plan_key"))),
        plan_name: json_string(plan.and_then(|plan| plan.get("name"))),
        plan_level: plan.and_then(|plan| plan.get("level")).and_then(json_i64),
        features,
    }
}

/// Add the `name` of each `{ "name": ... }` element of a feature array to
/// `out`, skipping non-arrays, non-object elements, and empty or missing names.
fn collect_feature_names(array: Option<&Value>, out: &mut BTreeSet<String>) {
    let Some(items) = array.and_then(Value::as_array) else {
        return;
    };
    for name in items
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
    {
        if !name.is_empty() {
            out.insert(name.to_owned());
        }
    }
}

/// Read a signed integer that Suno may encode as a JSON integer, an integral
/// JSON float (`2450.0`), or a decimal string (`"2450"` or `"2450.0"`).
///
/// Non-integral values (`2450.5`), overflow, and junk yield `None`. The
/// conversion is lossless and never saturates a value into range.
fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().and_then(f64_to_i64)),
        Value::String(text) => str_to_i64(text),
        _ => None,
    }
}

/// Convert a finite, integral `f64` to `i64`, rejecting fractional values and
/// anything outside the exactly representable range.
fn f64_to_i64(value: f64) -> Option<i64> {
    // Beyond 2^53 an f64 cannot losslessly represent an integer: serde has
    // already rounded (or saturated) such a value before we see it, so we
    // refuse rather than return a wrong result. Below 2^53 the cast is exact.
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Some(value as i64)
    } else {
        None
    }
}

/// Parse a decimal string into `i64`, accepting an all-zero fractional part
/// (`"2450.0"`) but rejecting non-integral values, overflow, and junk.
fn str_to_i64(text: &str) -> Option<i64> {
    match text.split_once('.') {
        Some((integer, fraction)) => {
            let integral = fraction.is_empty() || fraction.bytes().all(|byte| byte == b'0');
            integral.then(|| integer.parse().ok()).flatten()
        }
        None => text.parse().ok(),
    }
}

/// Read an optional string field, cloning the value when present.
fn json_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_billing_info_reads_full_real_body() {
        let billing = parse_billing_info(BILLING_FULL.as_bytes()).unwrap();
        assert_eq!(billing.total_credits_left, Some(2450));
        assert_eq!(billing.monthly_limit, Some(2500));
        assert_eq!(billing.monthly_usage, Some(50));
        assert_eq!(billing.credits, Some(0));
        assert_eq!(billing.period.as_deref(), Some("month"));
        assert_eq!(billing.is_active, Some(true));
        assert_eq!(billing.is_paused, Some(false));
        assert_eq!(billing.is_past_due, Some(false));
        assert_eq!(billing.is_gifted, Some(false));
        assert_eq!(billing.subscription_platform.as_deref(), Some("stripe"));
        assert_eq!(billing.plan_key.as_deref(), Some("pro"));
        assert_eq!(billing.plan_name.as_deref(), Some("Pro Plan"));
        assert_eq!(billing.plan_level, Some(10));
        assert!(billing.can_get_stems());
        assert!(billing.can_convert_audio());
        assert!(billing.has_feature("custom_models"));
    }

    #[test]
    fn json_i64_reads_string_integral_float_and_negative_encodings() {
        // A credits field may arrive string-encoded, as an integral float, or as
        // the -1 sentinel; all three decode to the same integer.
        let credits = |body: &[u8]| parse_billing_info(body).unwrap().total_credits_left;
        assert_eq!(credits(br#"{"total_credits_left":"2450"}"#), Some(2450));
        assert_eq!(credits(br#"{"total_credits_left":2450.0}"#), Some(2450));
        assert_eq!(credits(br#"{"total_credits_left":-1}"#), Some(-1));
    }

    #[test]
    fn json_i64_rejects_non_integral_float_but_object_still_parses() {
        let billing =
            parse_billing_info(br#"{"total_credits_left":2450.5,"period":"month"}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.period.as_deref(), Some("month"));
    }

    #[test]
    fn str_to_i64_handles_encodings_and_junk() {
        assert_eq!(str_to_i64("2450"), Some(2450));
        assert_eq!(str_to_i64("2450.0"), Some(2450));
        assert_eq!(str_to_i64("-1"), Some(-1));
        assert_eq!(str_to_i64("2450.5"), None);
        assert_eq!(str_to_i64(".5"), None);
        assert_eq!(str_to_i64("nope"), None);
        assert_eq!(str_to_i64("99999999999999999999999"), None);
    }

    #[test]
    fn json_i64_rejects_overflow() {
        let billing =
            parse_billing_info(br#"{"total_credits_left":99999999999999999999999}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
    }

    #[test]
    fn json_i64_covers_i64_and_float_boundaries() {
        // Integers arrive through the lossless i64 path, so the full i64 range works.
        assert_eq!(json_i64(&serde_json::json!(i64::MAX)), Some(i64::MAX));
        assert_eq!(json_i64(&serde_json::json!(i64::MIN)), Some(i64::MIN));
        // A JSON integer of 2^63 exceeds i64::MAX and must not saturate.
        assert_eq!(
            json_i64(&serde_json::json!(9_223_372_036_854_775_808_u64)),
            None
        );
        // Floats are trusted only below 2^53, so both i64 extremes are rejected.
        assert_eq!(f64_to_i64(i64::MAX as f64), None);
        assert_eq!(f64_to_i64(i64::MIN as f64), None);
        assert_eq!(f64_to_i64(2450.5), None);
        assert_eq!(f64_to_i64(f64::NAN), None);
        assert_eq!(f64_to_i64(f64::INFINITY), None);
    }

    #[test]
    fn f64_to_i64_rejects_values_below_i64_min() {
        // A float below i64::MIN must not silently saturate to i64::MIN.
        let below_min: f64 = "-9223372036854775809".parse().unwrap();
        assert_eq!(f64_to_i64(below_min), None);
        // The matching string is rejected by the lossless i64 parse.
        assert_eq!(str_to_i64("-9223372036854775809"), None);
        assert_eq!(json_i64(&serde_json::json!("-9223372036854775809")), None);
    }

    #[test]
    fn f64_to_i64_trusts_only_the_safe_integer_range() {
        // 2^53 - 1 is the largest integer an f64 represents exactly.
        assert_eq!(
            f64_to_i64(9_007_199_254_740_991.0),
            Some(9_007_199_254_740_991)
        );
        // 9007199254740993 (2^53 + 1) is not representable, so serde rounds it to
        // 2^53 before we see it; the rounded value must be refused, not returned.
        let rounded: f64 = "9007199254740993".parse().unwrap();
        assert_eq!(rounded, 9_007_199_254_740_992.0);
        assert_eq!(f64_to_i64(rounded), None);
    }

    #[test]
    fn parse_billing_info_defaults_missing_fields() {
        let billing = parse_billing_info(br#"{"monthly_usage":12}"#).unwrap();
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.monthly_usage, Some(12));
        assert_eq!(billing.plan_key, None);
        assert!(billing.features.is_empty());
        assert!(!billing.can_get_stems());
    }

    #[test]
    fn from_billing_json_ignores_surprising_types() {
        // `subscription_type` is a bool despite its name; a numeric field carrying
        // the wrong type must fall back to None rather than panic.
        let value = serde_json::json!({
            "subscription_type": true,
            "total_credits_left": {"unexpected": "object"},
            "is_active": "yes",
        });
        let billing = from_billing_json(&value);
        assert_eq!(billing.total_credits_left, None);
        assert_eq!(billing.is_active, None);
    }

    #[test]
    fn parse_billing_info_treats_non_object_json_as_default() {
        for body in [
            b"null".as_slice(),
            b"[]".as_slice(),
            br#""hello""#.as_slice(),
        ] {
            assert_eq!(parse_billing_info(body).unwrap(), BillingInfo::default());
        }
    }

    #[test]
    fn parse_billing_info_rejects_non_json_bytes() {
        let err = parse_billing_info(b"nope").unwrap_err();
        assert!(err.to_string().contains("invalid billing JSON"));
    }

    #[test]
    fn from_billing_json_unions_feature_sources() {
        let accessible_only = serde_json::json!({
            "accessible_features": [{"name": "get_stems"}],
        });
        assert!(from_billing_json(&accessible_only).can_get_stems());

        let plan_only = serde_json::json!({
            "plan": {"usage_plan_features": [{"name": "convert_audio"}]},
        });
        assert!(from_billing_json(&plan_only).can_convert_audio());

        let both = serde_json::json!({
            "accessible_features": [{"name": "get_stems"}, {"name": ""}, {"other": "x"}],
            "plan": {"usage_plan_features": [{"name": "convert_audio"}]},
        });
        let billing = from_billing_json(&both);
        assert!(billing.can_get_stems());
        assert!(billing.can_convert_audio());
        // Empty and malformed feature entries are ignored.
        assert_eq!(billing.features.len(), 2);
    }

    /// The anonymised full 43-field `GET /api/billing/info/` body from issue
    /// #223, used as a real-shape parse fixture.
    const BILLING_FULL: &str = r#"{
  "subscription_platform": "stripe",
  "is_active": true,
  "is_past_due": false,
  "credits": 0,
  "subscription_type": true,
  "subscription_anchor": "REDACTED",
  "subscription_id": "REDACTED",
  "renews_on": "REDACTED",
  "period": "month",
  "monthly_usage": 50,
  "monthly_limit": 2500,
  "credit_packs": [
    {
      "id": "00000000-0000-4000-8000-000000000001",
      "amount": 500,
      "price_usd": 4
    },
    {
      "id": "00000000-0000-4000-8000-000000000002",
      "amount": 1000,
      "price_usd": 8
    }
  ],
  "plan": {
    "id": "00000000-0000-4000-8000-000000000005",
    "level": 10,
    "plan_key": "pro",
    "name": "Pro Plan",
    "features": "Access to our newest model, v4\n2,500 credits (up to 500 songs), refreshes monthly\nCommercial use rights for songs made while subscribed\nCreate up to 10 songs at once\nEarly access to new features\nPriority creation queue\nAbility to purchase add-on credits",
    "monthly_price_usd": 10.0,
    "annual_price_usd": 96.0,
    "usage_plan_features": [
      {
        "name": "v4"
      },
      {
        "name": "cover"
      },
      {
        "name": "edit_mode"
      },
      {
        "name": "persona"
      },
      {
        "name": "can_buy_credit_top_ups"
      },
      {
        "name": "commercial_rights"
      },
      {
        "name": "get_stems"
      },
      {
        "name": "generate_song_image"
      },
      {
        "name": "auk"
      },
      {
        "name": "negative_tags"
      },
      {
        "name": "remaster"
      },
      {
        "name": "generate_song_video"
      },
      {
        "name": "long_uploads"
      },
      {
        "name": "convert_audio"
      },
      {
        "name": "create_control_sliders"
      },
      {
        "name": "playlist_condition"
      },
      {
        "name": "tag_upsample"
      },
      {
        "name": "custom_models"
      }
    ]
  },
  "models": [
    {
      "can_use": true,
      "max_lengths": {
        "title": 100,
        "prompt": 5000,
        "tags": 1000,
        "negative_tags": 1000,
        "gpt_description_prompt": 3000
      },
      "name": "Example Artist 5",
      "external_key": "chirp-fenix",
      "major_version": 5,
      "description": "[description redacted]",
      "is_default_free_model": false,
      "is_default_model": true,
      "badges": [
        "pro"
      ],
      "model_badges": [
        {
          "display_name": "Example Artist 1",
          "light": {
            "text_color": "000000",
            "background_color": "00000000",
            "border_color": "000000"
          },
          "dark": {
            "text_color": "FFFFFF",
            "background_color": "00000000",
            "border_color": "FFFFFF"
          }
        }
      ],
      "style": {
        "light": {
          "text_color": "FD429C"
        },
        "dark": {
          "text_color": "FD429C"
        }
      },
      "capabilities": [
        "all"
      ],
      "features": [
        "create_control_sliders",
        "tag_upsample",
        "mumble_mode",
        "vox_and_voices",
        "reuse_styles_lyrics"
      ],
      "allowed_condition_combinations": [
        [
          "extend"
        ],
        [
          "cover"
        ],
        [
          "infill"
        ],
        [
          "persona"
        ],
        [
          "persona",
          "extend"
        ],
        [
          "persona",
          "cover"
        ],
        [
          "playlist"
        ],
        [
          "underpaint"
        ],
        [
          "overpaint"
        ],
        [
          "vox"
        ],
        [
          "vox",
          "extend"
        ],
        [
          "vox",
          "cover"
        ],
        [
          "vox",
          "playlist"
        ],
        [
          "persona",
          "infill"
        ],
        [
          "cover",
          "infill"
        ]
      ],
      "id": "00000000-0000-4000-8000-000000000006"
    }
  ],
  "plan_price": 10.0,
  "plan_currency": "AUD",
  "plan_currency_price": 15.0,
  "payment_method_type": "card",
  "can_upgrade_immediately": true,
  "plans": [
    {
      "id": "00000000-0000-4000-8000-000000000015",
      "level": 0,
      "plan_key": "free",
      "name": "Free Plan",
      "features": "50 credits renew daily (10 songs)\nCreate up to 4 songs at once\nNo commercial use\nNo credit top ups\nShared generation queue",
      "monthly_price_usd": 0.0,
      "annual_price_usd": 0.0,
      "usage_plan_features": [
        {
          "name": "tag_upsample"
        }
      ],
      "prices": []
    }
  ],
  "accessible_features": [
    {
      "name": "v4"
    },
    {
      "name": "cover"
    },
    {
      "name": "edit_mode"
    },
    {
      "name": "persona"
    },
    {
      "name": "can_buy_credit_top_ups"
    },
    {
      "name": "commercial_rights"
    },
    {
      "name": "get_stems"
    },
    {
      "name": "generate_song_image"
    },
    {
      "name": "auk"
    },
    {
      "name": "negative_tags"
    },
    {
      "name": "remaster"
    },
    {
      "name": "generate_song_video"
    },
    {
      "name": "long_uploads"
    },
    {
      "name": "convert_audio"
    },
    {
      "name": "create_control_sliders"
    },
    {
      "name": "playlist_condition"
    },
    {
      "name": "tag_upsample"
    },
    {
      "name": "custom_models"
    }
  ],
  "revcat_subscriptions_offering_id": "REDACTED",
  "total_credits_left": 2450,
  "free_persona_clips_remaining": 0,
  "free_cover_clips_remaining": 0,
  "free_remasters_remaining": 0,
  "free_mobile_remasters_remaining": 0,
  "free_mobile_v4_gens_remaining": 0,
  "free_web_v4_gens_remaining": 0,
  "free_vox_gens_remaining": 0,
  "has_been_subscriber_before": true,
  "has_valid_school_email": false,
  "has_been_student_subscriber_before": false,
  "day0_boost": -1,
  "promotions": [],
  "audio_upload_limits": {
    "min": 6,
    "max": 1800
  },
  "voice_upload_limits": {
    "min": 10,
    "max": 900
  },
  "voice_record_limits": {
    "min": 10,
    "max": 240
  },
  "period_end": "REDACTED",
  "remaster_model_types": [
    {
      "name": "Example Artist 5",
      "external_key": "chirp-flounder",
      "is_default_model": true,
      "can_use": false
    },
    {
      "name": "Example Artist 2",
      "external_key": "chirp-carp",
      "is_default_model": false,
      "can_use": false
    },
    {
      "name": "v4.5+",
      "external_key": "chirp-bass",
      "is_default_model": false,
      "can_use": false
    }
  ],
  "is_pause_scheduled": false,
  "is_paused": false,
  "is_gifted": false
}"#;
}
