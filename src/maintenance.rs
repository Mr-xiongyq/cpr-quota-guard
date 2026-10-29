//! 在宿主维护调用内公平刷新过期额度，仅保留轮转位置与尝试限频

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::data::{QuotaDetails, QuotaFactsQuery},
    client::{Empty, TypedCall, TypedReply},
};
use tokio::sync::Mutex;

use crate::{
    config::{AccountRule, Config},
    policy::{ReasonCode, assess},
};

const MINIMUM_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const MAXIMUM_REFRESHES_PER_CYCLE: usize = 32;

pub struct Maintenance {
    config: Arc<Config>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    cursor: usize,
    last_attempt: HashMap<String, Instant>,
}

impl Maintenance {
    #[must_use]
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            state: Mutex::new(State::default()),
        }
    }

    pub async fn reconcile(
        &self,
        call: TypedCall<Empty>,
    ) -> Result<TypedReply<Empty>, PluginFault> {
        let Ok(mut state) = self.state.try_lock() else {
            return Ok(TypedReply::new(Empty {}));
        };
        let accounts = &self.config.accounts;
        let mut refreshes = 0;
        for _ in 0..accounts.len() {
            if call.cancellation.is_cancelled() {
                return Err(cancelled());
            }
            if refreshes == MAXIMUM_REFRESHES_PER_CYCLE {
                break;
            }
            let rule = &accounts[state.cursor];
            // 先推进轮转位置，取消当前回调后下一轮仍能服务后续账号
            state.cursor = (state.cursor + 1) % accounts.len();
            if !rule.is_enabled()
                || state
                    .last_attempt
                    .get(&rule.account_id)
                    .is_some_and(|attempt| attempt.elapsed() < MINIMUM_REFRESH_INTERVAL)
            {
                continue;
            }
            let query = QuotaFactsQuery {
                account_id: rule.account_id.clone(),
            };
            let quota = call.host.quota_details(query.clone()).await;
            let refresh = match quota {
                Ok(quota) => needs_refresh(
                    rule,
                    &quota,
                    now_ms()?,
                    self.config.max_observation_age_seconds,
                ),
                Err(fault) if fault.code == ErrorCode::Cancelled => return Err(cancelled()),
                Err(_) => true,
            };
            if !refresh {
                continue;
            }
            if call.cancellation.is_cancelled() {
                return Err(cancelled());
            }
            // 失败与取消也消耗本轮尝试，防止持续错误触发紧密上游重试
            state
                .last_attempt
                .insert(rule.account_id.clone(), Instant::now());
            refreshes += 1;
            if let Err(fault) = call.host.refresh_account_quota(query).await
                && fault.code == ErrorCode::Cancelled
            {
                return Err(cancelled());
            }
            // 刷新返回的简略事实不改变保护结论，后续调度重新读取完整窗口
        }
        Ok(TypedReply::new(Empty {}))
    }
}

fn needs_refresh(
    rule: &AccountRule,
    quota: &QuotaDetails,
    now_ms: i64,
    max_age_seconds: u64,
) -> bool {
    if assess(rule, quota, now_ms, max_age_seconds)
        .reasons
        .iter()
        .any(|reason| reason.code != ReasonCode::LimitReached)
    {
        return true;
    }
    let half_age_ms = max_age_seconds
        .checked_mul(500)
        .and_then(|milliseconds| i64::try_from(milliseconds).ok());
    let age_ms = quota
        .observed_at_ms
        .and_then(|observed| now_ms.checked_sub(observed));
    !matches!((age_ms, half_age_ms), (Some(age), Some(maximum)) if age >= 0 && age <= maximum)
}

fn now_ms() -> Result<i64, PluginFault> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .ok_or_else(|| PluginFault::new(ErrorCode::Fault, "无法读取当前时间，额度维护已暂停"))
}

fn cancelled() -> PluginFault {
    PluginFault::new(ErrorCode::Cancelled, "额度维护已取消")
}
