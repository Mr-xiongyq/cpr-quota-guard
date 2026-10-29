//! 真实插件二进制的握手、注册、额度回调、配置替换与调用生命周期测试

mod support;

use std::collections::{BTreeMap, BTreeSet};

use gateway_plugin_sdk::{
    ErrorCode, Message, PluginFault, Stage,
    call::{
        data::QuotaDetails,
        management::{CommandInvocation, CommandRegistration, CommandResult, CommandValue},
        policy::AccountScheduleDecision,
    },
};
use serde_json::{Value, json};
use support::{PluginProcess, QuotaCall, configured, decision, now_ms, quota, refreshed, request};

fn excluded(decision: AccountScheduleDecision) -> BTreeSet<String> {
    let AccountScheduleDecision::DelegateFiltered {
        exclude_account_ids,
    } = decision
    else {
        panic!("expected filtered delegation");
    };
    let unique: BTreeSet<_> = exclude_account_ids.iter().cloned().collect();
    assert_eq!(unique.len(), exclude_account_ids.len());
    unique
}

#[tokio::test]
async fn filters_configured_candidates_including_busy_accounts_and_preserves_other_scopes() {
    let config = json!({"accounts":[
        {"accountId":"acct-a"},
        {"accountId":"acct-b"},
        {"accountId":"not-a-candidate"},
        {"accountId":"weekly-only","fiveHourLimitPercent":null},
        {"accountId":"five-hour-only","weeklyLimitPercent":null}
    ]});
    let mut process = PluginProcess::start(config).await;
    let (result, queries) = process
        .schedule(
            request(
                "openai",
                &[
                    ("acct-a", true),
                    ("acct-b", false),
                    ("unconfigured", true),
                    ("weekly-only", true),
                    ("five-hour-only", true),
                ],
            ),
            |id| {
                let value = match id {
                    "acct-a" => quota(id, 95.0, 20.0),
                    "acct-b" => quota(id, 20.0, 96.0),
                    "weekly-only" => {
                        let mut value = quota(id, 100.0, 20.0);
                        let mut unrelated = value.windows[1].clone();
                        unrelated.limit_id = Some("other-model".into());
                        unrelated.used_percent = Some(100.0);
                        value.windows.push(unrelated);
                        value
                    }
                    "five-hour-only" => {
                        let mut value = quota(id, 20.0, 20.0);
                        value.windows.truncate(1);
                        value
                    }
                    _ => panic!("queried an account outside the configured candidate intersection"),
                };
                Ok(value)
            },
        )
        .await;
    assert_eq!(
        excluded(result),
        BTreeSet::from(["acct-a".into(), "acct-b".into()])
    );
    assert_eq!(queries.len(), 4);
    assert_eq!(
        queries.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "acct-a".into(),
            "acct-b".into(),
            "weekly-only".into(),
            "five-hour-only".into()
        ])
    );
    process.shutdown().await;
}

#[tokio::test]
async fn requests_without_enabled_matching_rules_delegate_without_callbacks() {
    for (config, provider, id) in [
        (json!({}), "openai", "acct-a"),
        (configured(&["acct-a"]), "xai", "acct-a"),
        (configured(&["acct-a"]), "openai", "unconfigured"),
        (
            json!({"accounts":[{"accountId":"acct-a","fiveHourLimitPercent":null,"weeklyLimitPercent":null}]}),
            "openai",
            "acct-a",
        ),
    ] {
        let mut process = PluginProcess::start(config).await;
        let (result, queried) = process
            .schedule(request(provider, &[(id, true)]), |_| {
                panic!("unexpected quota callback")
            })
            .await;
        assert_eq!(result, AccountScheduleDecision::Delegate);
        assert!(queried.is_empty());
        process.shutdown().await;
    }
}

#[tokio::test]
async fn unavailable_observations_filter_all_candidates_and_fresh_data_restores_routing() {
    let mut process = PluginProcess::start(configured(&["acct-a", "acct-b"])).await;
    let (result, _) = process
        .schedule(
            request("openai", &[("acct-a", true), ("acct-b", true)]),
            |id| {
                if id == "acct-b" {
                    return Err(PluginFault::new(
                        ErrorCode::Upstream,
                        "synthetic provider failure",
                    ));
                }
                let mut value = quota(id, 20.0, 20.0);
                value.windows.truncate(1);
                Ok(value)
            },
        )
        .await;
    assert_eq!(
        excluded(result),
        BTreeSet::from(["acct-a".into(), "acct-b".into()])
    );
    for case in ["unknown", "stale", "future", "expired"] {
        let (result, _) = process
            .schedule(request("openai", &[("acct-a", true)]), |id| {
                let mut value = quota(id, 20.0, 20.0);
                match case {
                    "unknown" => value.windows[0].used_percent = None,
                    "stale" => value.observed_at_ms = Some(now_ms() - 301_000),
                    "future" => value.observed_at_ms = Some(now_ms() + 60_000),
                    "expired" => value.windows[0].reset_at_ms = Some(now_ms() - 1),
                    _ => unreachable!(),
                }
                Ok(value)
            })
            .await;
        assert_eq!(
            excluded(result),
            BTreeSet::from(["acct-a".into()]),
            "{case}"
        );
    }
    let (result, queries) = process
        .schedule(
            request("openai", &[("acct-a", true), ("acct-b", true)]),
            |id| Ok(quota(id, 20.0, 20.0)),
        )
        .await;
    assert_eq!(result, AccountScheduleDecision::Delegate);
    assert_eq!(queries.len(), 2);
    process.shutdown().await;
}

#[tokio::test]
async fn a_new_process_uses_its_own_handshake_configuration() {
    for (limit, should_exclude) in [(95, false), (85, true)] {
        let mut process = PluginProcess::start(json!({"accounts":[{
            "accountId":"acct-a","fiveHourLimitPercent":limit,"weeklyLimitPercent":null
        }]}))
        .await;
        let (result, _) = process
            .schedule(request("openai", &[("acct-a", true)]), |id| {
                Ok(quota(id, 90.0, 20.0))
            })
            .await;
        if should_exclude {
            assert_eq!(excluded(result), BTreeSet::from(["acct-a".into()]));
        } else {
            assert_eq!(result, AccountScheduleDecision::Delegate);
        }
        process.shutdown().await;
    }
}

#[tokio::test]
async fn quota_reads_are_bounded_and_out_of_order_replies_stay_with_their_accounts() {
    let ids: Vec<_> = (0..17).map(|index| format!("acct-{index}")).collect();
    let names: Vec<_> = ids.iter().map(String::as_str).collect();
    let candidates: Vec<_> = names.iter().map(|id| (*id, true)).collect();
    let mut process = PluginProcess::start(configured(&names)).await;
    let parent = process.schedule_call(request("openai", &candidates)).await;
    let mut seen = BTreeSet::new();
    let mut pending = Vec::new();
    let result = loop {
        let frame = process.wire().await;
        match frame.message {
            Message::Callback { .. } => {
                let callback = QuotaCall::from_frame(frame);
                assert_eq!(callback.parent_id, parent);
                assert!(
                    seen.insert(callback.account_id.clone()),
                    "duplicate quota read"
                );
                pending.push(callback);
                assert!(
                    pending.len() <= 8,
                    "more than eight simultaneous quota reads"
                );
            }
            Message::Suspend { id, .. } => {
                assert_eq!(id, parent);
                for callback in pending.drain(..).rev() {
                    let high = callback.account_id == "acct-2" || callback.account_id == "acct-12";
                    process
                        .reply(
                            &callback,
                            Ok(quota(
                                &callback.account_id,
                                if high { 96.0 } else { 20.0 },
                                20.0,
                            )),
                        )
                        .await;
                }
            }
            Message::Resume { id, epoch } => process.control(Message::Resumed { id, epoch }).await,
            Message::CallbackConsumed { parent_id, .. } => assert_eq!(parent_id, parent),
            Message::Result { .. } => break decision(frame, parent),
            _ => panic!("unexpected frame during bounded reads"),
        }
    };
    assert!(pending.is_empty());
    assert_eq!(seen, ids.into_iter().collect());
    assert_eq!(
        excluded(result),
        BTreeSet::from(["acct-2".into(), "acct-12".into()])
    );
    process.shutdown().await;
}

#[tokio::test]
async fn concurrent_calls_are_isolated_and_cancelled_callbacks_do_not_break_the_next_call() {
    let mut process = PluginProcess::start(configured(&["acct-a", "acct-b"])).await;
    let cancelled = process
        .schedule_call(request("openai", &[("acct-a", true)]))
        .await;
    let concurrent = process
        .schedule_call(request("openai", &[("acct-b", true)]))
        .await;
    let mut callbacks = BTreeMap::new();
    while callbacks.len() < 2 {
        let callback = QuotaCall::from_frame(process.next_frame().await);
        assert!(callbacks.insert(callback.parent_id, callback).is_none());
    }
    let waiting = callbacks.remove(&cancelled).unwrap();
    let other = callbacks.remove(&concurrent).unwrap();
    assert_eq!(waiting.account_id, "acct-a");
    assert_eq!(other.account_id, "acct-b");
    process.reply(&other, Ok(quota("acct-b", 96.0, 20.0))).await;
    assert_eq!(
        excluded(decision(process.next_frame().await, concurrent)),
        BTreeSet::from(["acct-b".into()])
    );
    process.control(Message::Cancel { id: cancelled }).await;
    assert!(
        matches!(process.next_frame().await.message, Message::Cancelled { id } if id == cancelled)
    );
    process
        .reply(&waiting, Ok(quota("acct-a", 100.0, 100.0)))
        .await;
    let (result, queried) = process
        .schedule(request("openai", &[("acct-a", true)]), |id| {
            Ok(quota(id, 20.0, 20.0))
        })
        .await;
    assert_eq!(result, AccountScheduleDecision::Delegate);
    assert_eq!(queried, ["acct-a"]);
    process.shutdown().await;
}

#[tokio::test]
async fn status_reports_safe_reasons_and_rejects_unconfigured_accounts_without_callbacks() {
    let mut process = PluginProcess::start(configured(&["acct-a"])).await;
    let registration = process
        .call(
            "command_line.register",
            Stage::Registration,
            json!({}),
            vec![],
        )
        .await;
    let frame = process.next_frame().await;
    assert!(
        matches!(frame.message, Message::Result { id, ref result } if id == registration && result == &json!({}))
    );
    let commands: CommandRegistration = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(commands.commands.len(), 1);
    assert_eq!(commands.commands[0].name, "status");
    for (account, should_query) in [("acct-a", true), ("unconfigured", false)] {
        let invocation = CommandInvocation {
            name: "status".into(),
            arguments: BTreeMap::from([(
                "account-id".into(),
                CommandValue::String(account.into()),
            )]),
        };
        let id = process
            .call(
                "command_line.execute",
                Stage::CommandLine,
                json!({}),
                serde_json::to_vec(&invocation).unwrap(),
            )
            .await;
        let (frame, queried) = process
            .serve(id, |requested| {
                assert!(should_query);
                assert_eq!(requested, account);
                Err(PluginFault::new(
                    ErrorCode::Upstream,
                    "synthetic-secret-host-error",
                ))
            })
            .await;
        if should_query {
            assert_eq!(queried, ["acct-a"]);
            assert!(
                matches!(frame.message, Message::Result { ref result, .. } if result == &json!({}))
            );
            let result: CommandResult = serde_json::from_slice(&frame.payload).unwrap();
            assert_eq!(result.exit_code, 0);
            assert!(result.stderr.is_empty());
            assert!(result.accounts.is_empty());
            assert!(!result.stdout.contains("synthetic-secret-host-error"));
            let report: Value = serde_json::from_str(&result.stdout).unwrap();
            assert_eq!(report["maxObservationAgeSeconds"], 300);
            assert_eq!(report["accounts"].as_array().unwrap().len(), 1);
            assert_eq!(report["accounts"][0]["accountId"], "acct-a");
            assert_eq!(report["accounts"][0]["excluded"], true);
            assert_eq!(
                report["accounts"][0]["reasons"][0]["code"],
                "quota_unavailable"
            );
        } else {
            assert!(queried.is_empty());
            assert!(
                matches!(frame.message, Message::Error { error, .. } if error.code == ErrorCode::InvalidInput)
            );
        }
    }
    process.shutdown().await;
}

#[tokio::test]
async fn invalid_configuration_exits_before_ready() {
    PluginProcess::spawn(json!({"accounts":[{"accountId":"acct-a","fiveHourLimitPercent":101}]}))
        .await
        .expect_configuration_rejected()
        .await;
}

fn stale_quota(account: &str) -> QuotaDetails {
    let mut value = quota(account, 20.0, 20.0);
    value.observed_at_ms = Some(now_ms() - 301_000);
    value
}

#[tokio::test]
async fn maintenance_refresh_success_requires_new_full_details_before_routing_recovers() {
    let mut process = PluginProcess::start(configured(&["acct-a"])).await;
    let candidates = request("openai", &[("acct-a", true)]);
    let (before, _) = process
        .schedule(candidates.clone(), |id| Ok(stale_quota(id)))
        .await;
    assert_eq!(excluded(before), BTreeSet::from(["acct-a".into()]));
    let (reads, refreshes) = process
        .reconcile(|id| Ok(stale_quota(id)), |id| Ok(refreshed(id)))
        .await;
    assert_eq!(reads, ["acct-a"]);
    assert_eq!(refreshes, ["acct-a"]);
    let (still_old, _) = process
        .schedule(candidates.clone(), |id| Ok(stale_quota(id)))
        .await;
    assert_eq!(excluded(still_old), BTreeSet::from(["acct-a".into()]));
    let (fresh, _) = process
        .schedule(candidates, |id| Ok(quota(id, 20.0, 20.0)))
        .await;
    assert_eq!(fresh, AccountScheduleDecision::Delegate);
    let (_, refreshes) = process
        .reconcile(
            |id| Ok(stale_quota(id)),
            |_| panic!("duplicate refresh inside sixty seconds"),
        )
        .await;
    assert!(refreshes.is_empty());
    process.shutdown().await;
}

#[tokio::test]
async fn maintenance_skips_disabled_and_fresh_samples_but_refreshes_before_observations_expire() {
    let mut process = PluginProcess::start(json!({"accounts":[
        {"accountId":"healthy"}, {"accountId":"limited"}, {"accountId":"aging"},
        {"accountId":"disabled","fiveHourLimitPercent":null,"weeklyLimitPercent":null}
    ]}))
    .await;
    let (reads, refreshes) = process
        .reconcile(
            |id| {
                let mut value = quota(id, 20.0, 20.0);
                match id {
                    "healthy" => {}
                    "limited" => {
                        value.limit_reached = true;
                        value.windows[0].limit_reached = true;
                    }
                    "aging" => value.observed_at_ms = Some(now_ms() - 151_000),
                    _ => panic!("disabled protection must not read quota"),
                }
                Ok(value)
            },
            |id| {
                assert_eq!(id, "aging");
                Ok(refreshed(id))
            },
        )
        .await;
    assert_eq!(
        reads.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from(["healthy".into(), "limited".into(), "aging".into()])
    );
    assert_eq!(refreshes, ["aging"]);
    process.shutdown().await;
}

#[tokio::test]
async fn failed_refreshes_are_rate_limited_and_do_not_starve_accounts_after_the_batch_limit() {
    let ids: Vec<_> = (0..33).map(|index| format!("acct-{index}")).collect();
    let names: Vec<_> = ids.iter().map(String::as_str).collect();
    let mut process = PluginProcess::start(configured(&names)).await;
    let (_, first) = process
        .reconcile(
            |id| Ok(stale_quota(id)),
            |_| {
                Err(PluginFault::new(
                    ErrorCode::Upstream,
                    "synthetic refresh failure",
                ))
            },
        )
        .await;
    assert_eq!(first.len(), 32);
    let (blocked, _) = process
        .schedule(request("openai", &[("acct-0", true)]), |id| {
            Ok(stale_quota(id))
        })
        .await;
    assert_eq!(excluded(blocked), BTreeSet::from(["acct-0".into()]));
    let (_, second) = process
        .reconcile(
            |id| Ok(stale_quota(id)),
            |_| {
                Err(PluginFault::new(
                    ErrorCode::Upstream,
                    "synthetic refresh failure",
                ))
            },
        )
        .await;
    assert_eq!(second.len(), 1);
    let all: BTreeSet<_> = first.into_iter().chain(second).collect();
    assert_eq!(all, ids.into_iter().collect());
    let (_, refreshes) = process
        .reconcile(
            |id| Ok(stale_quota(id)),
            |_| panic!("failed refresh retried before sixty seconds"),
        )
        .await;
    assert!(refreshes.is_empty());
    process.shutdown().await;
}

#[tokio::test]
async fn cancelled_maintenance_releases_its_lock_and_preserves_fairness_and_refresh_limits() {
    let mut process = PluginProcess::start(configured(&["acct-a", "acct-b"])).await;
    let first = process
        .call("plugin.reconcile", Stage::Maintenance, json!({}), vec![])
        .await;
    let held_read = QuotaCall::from_frame(process.next_frame().await);
    assert_eq!(held_read.parent_id, first);
    assert_eq!(held_read.account_id, "acct-a");
    process.control(Message::Cancel { id: first }).await;
    assert!(matches!(process.next_frame().await.message, Message::Cancelled { id } if id == first));
    process.reply(&held_read, Ok(stale_quota("acct-a"))).await;

    let second = process
        .call("plugin.reconcile", Stage::Maintenance, json!({}), vec![])
        .await;
    let next_read = QuotaCall::from_frame(process.next_frame().await);
    assert_eq!(next_read.parent_id, second);
    assert_eq!(next_read.account_id, "acct-b");
    process.reply(&next_read, Ok(stale_quota("acct-b"))).await;
    let held_refresh = QuotaCall::from_frame(process.next_frame().await);
    assert_eq!(held_refresh.parent_id, second);
    assert_eq!(held_refresh.account_id, "acct-b");
    process.control(Message::Cancel { id: second }).await;
    assert!(
        matches!(process.next_frame().await.message, Message::Cancelled { id } if id == second)
    );
    process
        .reply_refresh(&held_refresh, Ok(refreshed("acct-b")))
        .await;

    let (reads, refreshes) = process
        .reconcile(|id| Ok(stale_quota(id)), |id| Ok(refreshed(id)))
        .await;
    assert_eq!(reads.first().map(String::as_str), Some("acct-a"));
    assert_eq!(refreshes, ["acct-a"]);
    let (result, _) = process
        .schedule(
            request("openai", &[("acct-a", true), ("acct-b", true)]),
            |id| Ok(quota(id, 20.0, 20.0)),
        )
        .await;
    assert_eq!(result, AccountScheduleDecision::Delegate);
    process.shutdown().await;
}

struct ManagementHost {
    instance: Value,
    updates: Vec<Value>,
    conflict_on_save: bool,
    account_queries: Vec<Value>,
    quota_queries: Vec<String>,
}

impl ManagementHost {
    fn new(configuration: Value) -> Self {
        Self {
            instance: json!({
                "id":"quota-guard-test", "name":"用户自定义名称", "artifactSha256":"a".repeat(64),
                "enabled":true, "configuration":configuration, "revision":7,
                "bindings":[{"contribution":"mr-xiongyq.quota-guard.scheduler","stage":"scheduling","order":4,"failurePolicy":"reject","providerIds":["openai"],"clientKeyIds":["key-one"],"accountGroupIds":["group-one"],"models":["gpt-5"]}],
            }),
            updates: Vec::new(),
            conflict_on_save: false,
            account_queries: Vec::new(),
            quota_queries: Vec::new(),
        }
    }

    async fn serve(&mut self, process: &mut PluginProcess, parent: u64) -> (u16, Value) {
        use gateway_plugin_sdk::Frame;
        let mut bodies = BTreeMap::<String, Vec<u8>>::new();
        loop {
            let frame = process.next_frame().await;
            let Message::Callback {
                id,
                parent_id,
                method,
                params,
            } = frame.message
            else {
                let Message::Result { id, result } = frame.message else {
                    panic!("expected management response")
                };
                assert_eq!(id, parent);
                let response: gateway_plugin_sdk::call::management::ManagementResponse =
                    serde_json::from_value(result).unwrap();
                assert_eq!(response.content_type, "application/json");
                return (
                    response.status,
                    serde_json::from_slice(&frame.payload).unwrap(),
                );
            };
            assert_eq!(parent_id, parent);
            let (result, payload) = match method.as_str() {
                "host.http.dispatch" => {
                    assert!(
                        params["headers"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|h| h["name"] == "content-type")
                    );
                    let (status, data) = match params["uri"].as_str().unwrap() {
                        "/api/admin/plugins/instances" => {
                            assert_eq!(params["method"], "GET");
                            assert!(frame.payload.is_empty());
                            (200, json!([self.instance.clone()]))
                        }
                        "/api/admin/plugins/instances/update" => {
                            assert_eq!(params["method"], "POST");
                            self.updates
                                .push(serde_json::from_slice(&frame.payload).unwrap());
                            if self.conflict_on_save {
                                (409, json!({"private":"synthetic-host-secret"}))
                            } else {
                                (200, Value::Null)
                            }
                        }
                        _ => panic!("unexpected host path"),
                    };
                    let handle = format!("body-{id}");
                    bodies.insert(
                        handle.clone(),
                        serde_json::to_vec(
                            &json!({"code":if status==200 {200}else{40901},"data":data}),
                        )
                        .unwrap(),
                    );
                    (
                        json!({"status":status,"version":"HTTP/1.1","headers":[],"body":{"kind":"handle","handle":handle},"response":null,"session":false}),
                        vec![],
                    )
                }
                "host.http.body_read" => {
                    let body = bodies.get_mut(params["handle"].as_str().unwrap()).unwrap();
                    let eof = body.is_empty();
                    (json!({"eof":eof}), std::mem::take(body))
                }
                "host.data.accounts.list" => {
                    assert_eq!(params, json!({}));
                    let query: Value = serde_json::from_slice(&frame.payload).unwrap();
                    assert_eq!(query["provider_id"], "openai");
                    let second = query["cursor"].as_str().is_some();
                    let account = if second { "acct-b" } else { "acct-a" };
                    self.account_queries.push(query);
                    (json!({}),serde_json::to_vec(&json!({"schema_version":1,"accounts":[{"account_id":account,"provider_id":"openai","name":format!("名称 {account}"),"email":format!("{account}@example.test"),"group_ids":[],"enabled":true,"updated_at_ms":1}],"next_cursor":if second {Value::Null}else{json!("acct-a")}})).unwrap())
                }
                "host.auth.list" => {
                    assert_eq!(params, json!({}));
                    let query: Value = serde_json::from_slice(&frame.payload).unwrap();
                    assert_eq!(query["provider_id"], "openai");
                    let second = query["cursor"].as_str().is_some();
                    let account = if second { "acct-b" } else { "acct-a" };
                    self.account_queries.push(query);
                    (
                        json!({}),
                        serde_json::to_vec(&json!({"accounts":[{
                        "account_id":account,"provider_id":"openai","credential_revision":1,
                        "name":format!("名称 {account}"),"email":format!("{account}@example.test"),
                        "upstream_user_id":null,"upstream_account_id":null,"plan_type":null,
                        "authentication_kind":if second {"oauth"} else {"api_key"},
                        "enabled":true,"credential_state":"valid","has_refresh_token":false,
                        "access_token_expires_at_ms":null,"next_refresh_at_ms":null
                    }],"next_cursor":if second {Value::Null}else{json!("acct-a")}}))
                        .unwrap(),
                    )
                }
                "host.data.quota.details" => {
                    let query: Value = serde_json::from_slice(&frame.payload).unwrap();
                    let account = query["account_id"].as_str().unwrap();
                    self.quota_queries.push(account.into());
                    if account == "deleted" {
                        process
                            .control(Message::Error {
                                id,
                                error: PluginFault::new(
                                    ErrorCode::Upstream,
                                    "synthetic-host-secret",
                                ),
                            })
                            .await;
                        continue;
                    }
                    (
                        json!({}),
                        serde_json::to_vec(&quota(
                            account,
                            if account == "acct-a" { 95.0 } else { 20.0 },
                            20.0,
                        ))
                        .unwrap(),
                    )
                }
                _ => panic!("unexpected management callback {method}"),
            };
            process
                .send(Frame {
                    message: Message::Result { id, result },
                    payload,
                })
                .await;
        }
    }
}

async fn management_call(
    process: &mut PluginProcess,
    method: &str,
    path: &str,
    query: &str,
    body: Option<Value>,
) -> u64 {
    process.call("management.handle",Stage::Management,json!({"method":method,"path":path,"query":query,"content_type":body.as_ref().map(|_|"application/json"),"headers":[]}),body.map(|value|serde_json::to_vec(&value).unwrap()).unwrap_or_default()).await
}

#[tokio::test]
async fn management_registers_private_page_resources_and_bodyless_get_routes() {
    let mut process = PluginProcess::start(configured(&[])).await;
    let parent = process
        .call(
            "management.register",
            Stage::Registration,
            json!({}),
            vec![],
        )
        .await;
    let frame = process.next_frame().await;
    let Message::Result { id, ref result } = frame.message else {
        panic!("expected registration")
    };
    assert_eq!(id, parent);
    assert_eq!(result, &json!({}));
    let registration: gateway_plugin_sdk::call::management::ManagementRegistration =
        serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(registration.pages[0].entry, "web/index.html");
    assert_eq!(registration.pages[0].id, "quota-guard");
    assert!(registration.resources.iter().all(|r| !r.public));
    assert_eq!(registration.resources.len(), 3);
    assert!(
        registration
            .routes
            .iter()
            .filter(|r| r.method == "GET")
            .all(|r| r.request_content_types.is_empty())
    );
    process.shutdown().await;
}

#[tokio::test]
async fn management_overview_uses_paginated_account_facts_and_the_scheduler_assessment() {
    let config = configured(&["acct-a", "acct-b", "deleted"]);
    let mut process = PluginProcess::start(config.clone()).await;
    let mut host = ManagementHost::new(config);
    let parent = management_call(&mut process, "GET", "overview", "", None).await;
    let (status, body) = host.serve(&mut process, parent).await;
    assert_eq!(status, 200);
    assert_eq!(body["revision"], 7);
    assert_eq!(host.account_queries.len(), 2);
    assert_eq!(body["accounts"][0]["name"], "名称 acct-a");
    assert_eq!(body["accounts"][0]["assessment"]["excluded"], true);
    assert_eq!(
        body["accounts"][0]["quota"]["windows"][0]["usedPercent"],
        95.0
    );
    assert_eq!(body["accounts"][1]["assessment"]["excluded"], false);
    assert_eq!(
        body["accounts"][1]["quota"]["windows"][0]["usedPercent"],
        20.0
    );
    assert!(body["accounts"][2]["name"].is_null());
    assert!(body["accounts"][2]["quota"].is_null());
    assert_eq!(
        body["accounts"][2]["assessment"]["reasons"][0]["code"],
        "quota_unavailable"
    );
    assert!(!body.to_string().contains("synthetic-host-secret"));
    assert!(host.updates.is_empty());
    process.shutdown().await;
}

#[tokio::test]
async fn management_save_preserves_host_owned_fields_and_cannot_target_another_instance() {
    let config = configured(&["acct-a"]);
    let mut process = PluginProcess::start(config.clone()).await;
    let mut host = ManagementHost::new(config);
    let next = json!({"accounts":[{"accountId":"acct-a","fiveHourLimitPercent":80,"weeklyLimitPercent":null}],"maxObservationAgeSeconds":120});
    let parent = management_call(
        &mut process,
        "POST",
        "configuration",
        "",
        Some(json!({"expectedRevision":7,"configuration":next})),
    )
    .await;
    let (status, body) = host.serve(&mut process, parent).await;
    assert_eq!(
        (status, body),
        (200, json!({"saved":true,"reloadRequired":true}))
    );
    assert_eq!(host.updates.len(), 1);
    let update = &host.updates[0];
    assert_eq!(update["id"], "quota-guard-test");
    for key in ["name", "artifactSha256", "enabled", "bindings"] {
        assert_eq!(update["instance"][key], host.instance[key]);
    }
    assert_eq!(update["instance"]["expectedRevision"], 7);
    assert_eq!(
        cpr_quota_guard::config::Config::from_value(update["instance"]["configuration"].clone())
            .unwrap(),
        cpr_quota_guard::config::Config::from_value(next).unwrap(),
    );
    assert!(update["instance"].get("secrets").is_none());
    let parent = management_call(
        &mut process,
        "POST",
        "configuration",
        "",
        Some(json!({"id":"another-instance","expectedRevision":7,"configuration":{}})),
    )
    .await;
    let (status, _) = host.serve(&mut process, parent).await;
    assert_eq!(status, 400);
    assert_eq!(host.updates.len(), 1);
    process.shutdown().await;
}

#[tokio::test]
async fn management_rejects_stale_saves_and_surfaces_host_cas_conflicts_without_retrying() {
    let config = configured(&["acct-a"]);
    let mut process = PluginProcess::start(config.clone()).await;
    let mut host = ManagementHost::new(config);
    for revision in [6, 7] {
        host.conflict_on_save = true;
        let parent = management_call(
            &mut process,
            "POST",
            "configuration",
            "",
            Some(json!({"expectedRevision":revision,"configuration":{"accounts":[]}})),
        )
        .await;
        let (status, body) = host.serve(&mut process, parent).await;
        assert_eq!(status, 409);
        assert!(!body.to_string().contains("synthetic-host-secret"));
        assert_eq!(host.updates.len(), usize::from(revision == 7));
    }
    let (decision, _) = process
        .schedule(request("openai", &[("acct-a", true)]), |id| {
            Ok(quota(id, 96.0, 20.0))
        })
        .await;
    assert_eq!(excluded(decision), BTreeSet::from(["acct-a".into()]));
    process.shutdown().await;
}

#[tokio::test]
async fn management_validates_configuration_and_paginates_candidates_without_quota_reads() {
    let config = configured(&[]);
    let mut process = PluginProcess::start(config.clone()).await;
    let mut host = ManagementHost::new(config);
    let parent=management_call(&mut process,"POST","configuration","",Some(json!({"expectedRevision":7,"configuration":{"accounts":[{"accountId":"acct-a","fiveHourLimitPercent":101}]}}))).await;
    let (status, _) = host.serve(&mut process, parent).await;
    assert_eq!(status, 400);
    assert!(host.updates.is_empty());
    let parent = management_call(
        &mut process,
        "GET",
        "accounts",
        "cursor=acct%2Da&limit=20",
        None,
    )
    .await;
    let (status, body) = host.serve(&mut process, parent).await;
    assert_eq!(status, 200);
    assert_eq!(body["items"][0]["accountId"], "acct-b");
    assert_eq!(host.account_queries[0]["cursor"], "acct-a");
    assert_eq!(host.account_queries[0]["limit"], 20);
    assert!(host.quota_queries.is_empty());
    for query in [
        "limit=201",
        "limit=10&limit=20",
        "cursor=%Q1",
        "provider=xai",
    ] {
        let parent = management_call(&mut process, "GET", "accounts", query, None).await;
        let (status, _) = host.serve(&mut process, parent).await;
        assert_eq!(status, 400);
    }
    assert_eq!(host.account_queries.len(), 1);
    process.shutdown().await;
}

#[tokio::test]
async fn management_candidate_filter_preserves_cursor_when_a_page_has_no_oauth_accounts() {
    let config = configured(&[]);
    let mut process = PluginProcess::start(config.clone()).await;
    let mut host = ManagementHost::new(config);
    let parent = management_call(&mut process, "GET", "accounts", "limit=10", None).await;
    let (status, first) = host.serve(&mut process, parent).await;
    assert_eq!(status, 200);
    assert_eq!(first["items"], json!([]));
    assert_eq!(first["nextCursor"], "acct-a");
    let parent = management_call(
        &mut process,
        "GET",
        "accounts",
        "cursor=acct-a&limit=10",
        None,
    )
    .await;
    let (status, second) = host.serve(&mut process, parent).await;
    assert_eq!(status, 200);
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"][0]["accountId"], "acct-b");
    assert!(second["nextCursor"].is_null());
    assert!(second["items"][0].get("credential_revision").is_none());
    assert!(host.quota_queries.is_empty());
    process.shutdown().await;
}
