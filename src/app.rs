//! 只在当前候选中评估额度，选号、租约、刷新与账号启用状态由宿主持有

use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream};
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        data::QuotaFactsQuery,
        management::{
            CommandDescriptor, CommandInvocation, CommandParameter, CommandParameterType,
            CommandRegistration, CommandResult, CommandValue,
        },
        policy::{AccountScheduleDecision, AccountScheduleRequest},
    },
    client::{
        AuthorError, ComposedPlugin, HostClient, PluginBuilder, TypedCall, TypedReply, methods,
    },
};
use serde::Serialize;

use crate::{
    config::{AccountRule, Config},
    maintenance::Maintenance,
    management,
    policy::{self, Assessment},
};

pub(crate) const MAX_PARALLEL_READS: usize = 8;

pub fn build(config: Config, artifact_sha256: String) -> Result<ComposedPlugin, AuthorError> {
    let config = Arc::new(config);
    let maintenance = Arc::new(Maintenance::new(Arc::clone(&config)));
    let schedule_config = Arc::clone(&config);
    let management_config = Arc::clone(&config);
    PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .management(management::registration(), move |call| {
            let config = Arc::clone(&management_config);
            let artifact_sha256 = artifact_sha256.clone();
            async move { management::handle(call, &config, &artifact_sha256).await }
        })?
        .on(methods::RECONCILE, move |call| {
            let maintenance = Arc::clone(&maintenance);
            async move { maintenance.reconcile(call).await }
        })?
        .on(methods::SCHEDULE_ACCOUNT, move |call| {
            let config = Arc::clone(&schedule_config);
            async move { schedule(call, &config).await }
        })?
        .command_line(commands(), move |call| {
            let config = Arc::clone(&config);
            async move { status(call, &config).await }
        })?
        .build()
}

async fn schedule(
    call: TypedCall<AccountScheduleRequest>,
    config: &Config,
) -> Result<TypedReply<AccountScheduleDecision>, PluginFault> {
    if call.request.provider != "openai" || config.accounts.is_empty() {
        return Ok(TypedReply::new(AccountScheduleDecision::Delegate));
    }
    // 暂时繁忙的账号也要过滤，避免宿主排队唤醒后绕过额度保护
    let candidates: HashSet<_> = call
        .request
        .candidates
        .iter()
        .map(|candidate| candidate.account_id.as_str())
        .collect();
    let rules: Vec<_> = config
        .accounts
        .iter()
        .filter(|rule| candidates.contains(rule.account_id.as_str()))
        .collect();
    let assessments = evaluate(&call.host, &rules, config.max_observation_age_seconds).await?;
    let exclude_account_ids: Vec<_> = assessments
        .into_iter()
        .filter(|assessment| assessment.excluded)
        .map(|assessment| assessment.account_id)
        .collect();
    let decision = if exclude_account_ids.is_empty() {
        AccountScheduleDecision::Delegate
    } else {
        AccountScheduleDecision::DelegateFiltered {
            exclude_account_ids,
        }
    };
    Ok(TypedReply::new(decision))
}

async fn evaluate(
    host: &HostClient,
    rules: &[&AccountRule],
    max_age_seconds: u64,
) -> Result<Vec<Assessment>, PluginFault> {
    // 不缓存或主动刷新，避免请求路径访问上游以及并发维护第二份额度事实
    let rules: Vec<_> = rules.iter().map(|rule| (*rule).clone()).collect();
    let host = host.clone();
    stream::iter(rules)
        .map(move |rule| {
            let host = host.clone();
            async move {
                if !rule.is_enabled() {
                    return Ok(policy::unavailable(&rule));
                }
                read_assessment(&host, &rule, max_age_seconds)
                    .await
                    .map(|(assessment, _)| assessment)
            }
        })
        .buffered(MAX_PARALLEL_READS)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

pub(crate) async fn read_assessment(
    host: &HostClient,
    rule: &AccountRule,
    max_age_seconds: u64,
) -> Result<
    (
        Assessment,
        Option<gateway_plugin_sdk::call::data::QuotaDetails>,
    ),
    PluginFault,
> {
    match host
        .quota_details(QuotaFactsQuery {
            account_id: rule.account_id.clone(),
        })
        .await
    {
        Ok(quota) => Ok((
            policy::assess(rule, &quota, now_ms()?, max_age_seconds),
            Some(quota),
        )),
        Err(fault) if fault.code == ErrorCode::Cancelled => Err(fault),
        Err(_) => Ok((policy::unavailable(rule), None)),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusReport {
    evaluated_at_ms: i64,
    max_observation_age_seconds: u64,
    accounts: Vec<Assessment>,
}

async fn status(
    call: TypedCall<CommandInvocation>,
    config: &Config,
) -> Result<TypedReply<CommandResult>, PluginFault> {
    if call.request.name != "status" || call.request.arguments.keys().any(|key| key != "account-id")
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "请使用 status 命令及可选的 account-id 参数",
        ));
    }
    let account_id = match call.request.arguments.get("account-id") {
        None => None,
        Some(CommandValue::String(value)) if !value.is_empty() => Some(value.as_str()),
        _ => {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "account-id 必须是非空的账号 ID",
            ));
        }
    };
    let rules: Vec<_> = config
        .accounts
        .iter()
        .filter(|rule| account_id.is_none_or(|id| id == rule.account_id))
        .collect();
    if account_id.is_some() && rules.is_empty() {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "该账号未配置额度保护",
        ));
    }
    let accounts = evaluate(&call.host, &rules, config.max_observation_age_seconds).await?;
    let report = StatusReport {
        evaluated_at_ms: now_ms()?,
        max_observation_age_seconds: config.max_observation_age_seconds,
        accounts,
    };
    let stdout = serde_json::to_string_pretty(&report)
        .map_err(|_| PluginFault::new(ErrorCode::Fault, "无法生成额度保护状态"))?;
    Ok(TypedReply::new(CommandResult {
        stdout: format!("{stdout}\n"),
        stderr: String::new(),
        exit_code: 0,
        accounts: Vec::new(),
    }))
}

fn commands() -> CommandRegistration {
    CommandRegistration {
        commands: vec![CommandDescriptor {
            name: "status".into(),
            description: "读取受保护账号的额度状态、暂停原因和上游观测的重置时间，不刷新上游"
                .into(),
            parameters: vec![CommandParameter {
                name: "account-id".into(),
                description: "仅查看一个已配置的账号；省略时查看全部受保护账号".into(),
                value_type: CommandParameterType::String,
                required: false,
                sensitive: false,
                default: None,
            }],
        }],
    }
}

pub(crate) fn now_ms() -> Result<i64, PluginFault> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| PluginFault::new(ErrorCode::Fault, "System clock is unavailable."))
}
