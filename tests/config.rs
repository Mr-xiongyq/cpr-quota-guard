//! 配置默认值、独立关闭、输入边界与安全错误验证

use cpr_quota_guard::config::Config;
use serde_json::json;

#[test]
fn defaults_protect_only_explicitly_configured_accounts() {
    let empty = Config::from_value(json!({})).unwrap();
    assert!(empty.accounts.is_empty());
    assert_eq!(empty.max_observation_age_seconds, 300);

    let configured = Config::from_value(json!({"accounts": [{"accountId": "acct_one"}]})).unwrap();
    let rule = &configured.accounts[0];
    assert_eq!(rule.five_hour_limit_percent, Some(95.0));
    assert_eq!(rule.weekly_limit_percent, Some(95.0));
    assert!(rule.is_enabled());
}

#[test]
fn explicit_null_disables_each_window_independently() {
    for (five_hour, weekly, enabled) in [
        (None, Some(95.0), true),
        (Some(95.0), None, true),
        (None, None, false),
    ] {
        let config = Config::from_value(json!({"accounts": [{
            "accountId": "acct_one",
            "fiveHourLimitPercent": five_hour,
            "weeklyLimitPercent": weekly
        }]}))
        .unwrap();
        assert_eq!(config.accounts[0].five_hour_limit_percent, five_hour);
        assert_eq!(config.accounts[0].weekly_limit_percent, weekly);
        assert_eq!(config.accounts[0].is_enabled(), enabled);
    }
}

#[test]
fn percentage_bounds_are_inclusive_and_zero_is_not_disabled() {
    for limit in [0.0, 0.25, 95.5, 100.0] {
        let config = Config::from_value(json!({"accounts": [{
            "accountId": "acct_one",
            "fiveHourLimitPercent": limit,
            "weeklyLimitPercent": limit
        }]}))
        .unwrap();
        assert!(config.accounts[0].is_enabled());
    }
    for limit in [-0.01, 100.01] {
        for field in ["fiveHourLimitPercent", "weeklyLimitPercent"] {
            let mut value = json!({"accounts": [{"accountId": "acct_one"}]});
            value["accounts"][0][field] = json!(limit);
            assert!(Config::from_value(value).is_err());
        }
    }
}

#[test]
fn observation_age_has_bounded_positive_seconds() {
    for seconds in [60, 300, 86_400] {
        assert!(Config::from_value(json!({"maxObservationAgeSeconds": seconds})).is_ok());
    }
    for seconds in [
        json!(0),
        json!(59),
        json!(86_401),
        json!(-1),
        json!(1.5),
        json!(null),
    ] {
        assert!(Config::from_value(json!({"maxObservationAgeSeconds": seconds})).is_err());
    }
}

#[test]
fn empty_whitespace_and_duplicate_account_ids_are_rejected() {
    for account_id in ["", " ", " acct_one", "acct_one\n", "\tacct_one"] {
        assert!(Config::from_value(json!({"accounts": [{"accountId": account_id}]})).is_err());
    }
    assert!(
        Config::from_value(json!({"accounts": [
            {"accountId": "acct_one"}, {"accountId": "acct_one"}
        ]}))
        .is_err()
    );
    assert!(
        Config::from_value(json!({"accounts": [
            {"accountId": "acct_one"}, {"accountId": "acct_two"}
        ]}))
        .is_ok()
    );
}

#[test]
fn malformed_and_unknown_fields_do_not_echo_input() {
    let private_value = "sensitive-fixture-value";
    for value in [
        json!({"unknown": private_value}),
        json!({"accounts": [{"accountId": "acct_one", "unknown": private_value}]}),
        json!({"accounts": private_value}),
        json!({"accounts": [{"accountId": "acct_one", "weeklyLimitPercent": private_value}]}),
        json!({"accounts": [{}]}),
        json!({"accounts": [["acct_one", 95, 95]]}),
        json!([]),
        json!(null),
    ] {
        let error = Config::from_value(value).unwrap_err();
        assert!(!error.contains(private_value));
        assert!(!error.is_ascii());
    }
}

#[test]
fn account_count_and_identifier_length_match_manifest_limits() {
    for (length, valid) in [(256, true), (257, false)] {
        for character in ['a', '账'] {
            let account_id = character.to_string().repeat(length);
            assert_eq!(
                Config::from_value(json!({"accounts": [{"accountId": account_id}]})).is_ok(),
                valid
            );
        }
    }
    for separator in ['\n', '\r', '\u{2028}', '\u{2029}'] {
        assert!(
            Config::from_value(json!({"accounts": [{
                "accountId": format!("acct{separator}one")
            }]}))
            .is_err()
        );
    }
    for (count, valid) in [(1_000, true), (1_001, false)] {
        let accounts = (0..count)
            .map(|index| json!({"accountId": format!("acct_{index}")}))
            .collect::<Vec<_>>();
        assert_eq!(
            Config::from_value(json!({"accounts": accounts})).is_ok(),
            valid
        );
    }
}
