//! 额度保护阈值、未知事实、窗口归属及无状态恢复验证

use cpr_quota_guard::{
    config::AccountRule,
    policy::{Assessment, ReasonCode, assess, unavailable},
};
use gateway_plugin_sdk::call::data::{QuotaDetails, QuotaWindowDetails};

const NOW: i64 = 1_800_000_000_000;

fn rule() -> AccountRule {
    AccountRule {
        account_id: "acct_one".to_owned(),
        five_hour_limit_percent: Some(95.0),
        weekly_limit_percent: Some(95.0),
    }
}

fn window(seconds: u64, used: f64) -> QuotaWindowDetails {
    QuotaWindowDetails {
        key: format!("codex:{seconds}s"),
        group: "shortTerm".to_owned(),
        label: "测试窗口".to_owned(),
        limit_id: Some("codex".to_owned()),
        limit_name: None,
        role: None,
        window_seconds: Some(seconds),
        used_percent: Some(used),
        reset_at_ms: Some(NOW + 60_000),
        limit_reached: false,
    }
}

fn quota() -> QuotaDetails {
    QuotaDetails {
        schema_version: 1,
        account_id: "acct_one".to_owned(),
        observed_at_ms: Some(NOW - 1_000),
        limit_reached: false,
        windows: vec![window(18_000, 20.0), window(604_800, 30.0)],
        credits: None,
    }
}

fn has_reason(assessment: &Assessment, code: ReasonCode) -> bool {
    assessment.excluded && assessment.reasons.iter().any(|reason| reason.code == code)
}

#[test]
fn every_enabled_window_must_be_below_its_inclusive_threshold() {
    for index in [0, 1] {
        for (used, excluded) in [(94.999, false), (95.0, true), (100.0, true)] {
            let mut facts = quota();
            facts.windows[index].used_percent = Some(used);
            let assessment = assess(&rule(), &facts, NOW, 300);
            assert_eq!(assessment.excluded, excluded);
            if excluded {
                assert!(has_reason(&assessment, ReasonCode::LimitReached));
            }
        }
    }
}

#[test]
fn zero_limit_blocks_zero_usage_and_hundred_blocks_only_full_usage() {
    let mut policy = rule();
    policy.five_hour_limit_percent = Some(0.0);
    let mut facts = quota();
    facts.windows[0].used_percent = Some(0.0);
    assert!(assess(&policy, &facts, NOW, 300).excluded);
    policy.five_hour_limit_percent = Some(100.0);
    facts.windows[0].used_percent = Some(99.99);
    assert!(!assess(&policy, &facts, NOW, 300).excluded);
    facts.windows[0].used_percent = Some(100.0);
    assert!(assess(&policy, &facts, NOW, 300).excluded);
}

#[test]
fn disabled_window_does_not_require_facts() {
    for index in [0, 1] {
        let mut policy = rule();
        let mut facts = quota();
        if index == 0 {
            policy.five_hour_limit_percent = None;
        } else {
            policy.weekly_limit_percent = None;
        }
        facts.windows.remove(index);
        assert!(!assess(&policy, &facts, NOW, 300).excluded);
    }
}

#[test]
fn fully_disabled_rule_allows_even_unavailable_or_unreliable_facts() {
    let mut policy = rule();
    policy.five_hour_limit_percent = None;
    policy.weekly_limit_percent = None;
    let mut facts = quota();
    facts.schema_version = 999;
    facts.account_id = "different".to_owned();
    facts.observed_at_ms = None;
    facts.windows.clear();
    assert!(!assess(&policy, &facts, NOW, 300).excluded);
    assert!(!unavailable(&policy).excluded);
}

#[test]
fn observations_missing_in_future_or_too_old_keep_account_excluded() {
    for (observed_at, expected) in [
        (None, ReasonCode::ObservationMissing),
        (Some(NOW + 1), ReasonCode::ObservationInFuture),
        (Some(NOW - 300_001), ReasonCode::ObservationStale),
        (Some(i64::MIN), ReasonCode::ObservationStale),
    ] {
        let mut facts = quota();
        facts.observed_at_ms = observed_at;
        assert!(has_reason(&assess(&rule(), &facts, NOW, 300), expected));
    }
    for observed_at in [NOW, NOW - 300_000] {
        let mut facts = quota();
        facts.observed_at_ms = Some(observed_at);
        assert!(!assess(&rule(), &facts, NOW, 300).excluded);
    }
}

#[test]
fn unsupported_schema_and_wrong_account_are_not_accepted_as_safe_usage() {
    let mut facts = quota();
    facts.schema_version = 2;
    assert!(has_reason(
        &assess(&rule(), &facts, NOW, 300),
        ReasonCode::UnsupportedSchema
    ));
    facts.schema_version = 1;
    facts.account_id = "acct_two".to_owned();
    assert!(has_reason(
        &assess(&rule(), &facts, NOW, 300),
        ReasonCode::AccountMismatch
    ));
}

#[test]
fn missing_unknown_and_nonfinite_usage_is_conservative() {
    for usage in [
        None,
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(-0.1),
        Some(100.1),
    ] {
        let mut facts = quota();
        facts.windows[0].used_percent = usage;
        let assessment = assess(&rule(), &facts, NOW, 300);
        assert!(has_reason(&assessment, ReasonCode::UsageUnknown));
        assert!(serde_json::to_value(assessment).is_ok());
    }
}

#[test]
fn missing_duration_or_generic_bucket_does_not_guess_window_identity() {
    for replacement in [None, Some(18_001), Some(600)] {
        let mut facts = quota();
        facts.windows[0].window_seconds = replacement;
        assert!(has_reason(
            &assess(&rule(), &facts, NOW, 300),
            ReasonCode::WindowMissing
        ));
    }
    for limit_id in [None, Some("code_review"), Some("named_model_bucket")] {
        let mut facts = quota();
        facts.windows[0].limit_id = limit_id.map(str::to_owned);
        assert!(has_reason(
            &assess(&rule(), &facts, NOW, 300),
            ReasonCode::WindowMissing
        ));
    }
}

#[test]
fn named_buckets_and_display_roles_do_not_override_generic_bucket_usage() {
    let mut facts = quota();
    let mut named = window(18_000, 100.0);
    named.limit_id = Some("code_review".to_owned());
    named.limit_reached = true;
    facts.windows.push(named);
    facts.limit_reached = true;
    facts.windows[0].role = Some("secondary".to_owned());
    facts.windows[1].role = Some("primary".to_owned());
    facts.windows[0].label = "周额度".to_owned();
    facts.windows[1].group = "other".to_owned();
    assert!(!assess(&rule(), &facts, NOW, 300).excluded);
}

#[test]
fn duplicate_duration_windows_are_all_checked_instead_of_first_match_winning() {
    let mut facts = quota();
    let duplicate = window(18_000, 99.0);
    facts.windows.push(duplicate);
    assert!(has_reason(
        &assess(&rule(), &facts, NOW, 300),
        ReasonCode::LimitReached
    ));
    facts.windows.reverse();
    assert!(has_reason(
        &assess(&rule(), &facts, NOW, 300),
        ReasonCode::LimitReached
    ));
}

#[test]
fn missing_or_elapsed_reset_requires_new_evidence_even_with_low_usage() {
    for reset in [None, Some(NOW), Some(NOW - 1)] {
        let mut facts = quota();
        facts.windows[0].reset_at_ms = reset;
        let code = if reset.is_none() {
            ReasonCode::ResetUnknown
        } else {
            ReasonCode::WindowExpired
        };
        assert!(has_reason(&assess(&rule(), &facts, NOW, 300), code));
    }
}

#[test]
fn host_window_limit_reached_is_preserved() {
    let mut facts = quota();
    facts.windows[0].limit_reached = true;
    assert!(has_reason(
        &assess(&rule(), &facts, NOW, 300),
        ReasonCode::LimitReached
    ));
}

#[test]
fn read_failure_cannot_remove_protection() {
    assert!(has_reason(
        &unavailable(&rule()),
        ReasonCode::QuotaUnavailable
    ));
}

#[test]
fn restart_or_repeated_assessment_cannot_release_a_blocked_account() {
    let mut facts = quota();
    facts.windows[0].used_percent = Some(97.0);
    let initial = assess(&rule(), &facts, NOW, 300);
    assert!(initial.excluded);
    let restored: QuotaDetails =
        serde_json::from_value(serde_json::to_value(&facts).unwrap()).unwrap();
    assert_eq!(assess(&rule(), &restored, NOW, 300), initial);
    assert!(assess(&rule(), &restored, NOW + 600_000, 300).excluded);
    assert!(unavailable(&rule()).excluded);
}

#[test]
fn only_fresh_host_observation_can_release_protection_after_reset() {
    let mut facts = quota();
    facts.windows[0].used_percent = Some(97.0);
    assert!(assess(&rule(), &facts, NOW, 300).excluded);
    assert!(assess(&rule(), &facts, NOW + 60_000, 300).excluded);
    facts.observed_at_ms = Some(NOW + 60_000);
    for window in &mut facts.windows {
        window.reset_at_ms = Some(NOW + 120_000);
        window.used_percent = Some(0.0);
    }
    assert!(!assess(&rule(), &facts, NOW + 60_000, 300).excluded);
}

#[test]
fn diagnostic_payload_has_stable_codes_and_window_evidence() {
    let mut facts = quota();
    facts.windows[1].used_percent = Some(95.0);
    let value = serde_json::to_value(assess(&rule(), &facts, NOW, 300)).unwrap();
    assert_eq!(value["accountId"], "acct_one");
    assert_eq!(value["excluded"], true);
    assert_eq!(value["reasons"][0]["code"], "limit_reached");
    assert_eq!(value["reasons"][0]["windowSeconds"], 604_800);
    assert_eq!(value["reasons"][0]["usedPercent"], 95.0);
    assert_eq!(value["reasons"][0]["limitPercent"], 95.0);
    assert_eq!(value["reasons"][0]["resetAtMs"], NOW + 60_000);
}
