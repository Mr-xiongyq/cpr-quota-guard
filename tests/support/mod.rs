//! 真实插件子进程的 SDK 帧驱动与合成额度事实，不连接宿主数据库或上游

use std::{
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gateway_plugin_sdk::{
    CallContext, Frame, Handshake, Manifest, Message, PROTOCOL_VERSION, PluginFault, Stage,
    call::{
        data::{
            QUOTA_DETAILS, QUOTA_REFRESH, QuotaDetails, QuotaFacts, QuotaFactsQuery,
            QuotaWindowDetails, QuotaWindowFacts,
        },
        policy::{AccountScheduleCandidate, AccountScheduleDecision, AccountScheduleRequest},
        registration::Registration,
    },
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_INCARNATION: AtomicU64 = AtomicU64::new(1);

pub struct PluginProcess {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    handshake: Handshake,
    next_call_id: u64,
}

pub struct QuotaCall {
    pub id: u64,
    pub parent_id: u64,
    pub account_id: String,
    pub method: String,
}

impl QuotaCall {
    pub fn from_frame(frame: Frame) -> Self {
        let Message::Callback {
            id,
            parent_id,
            method,
            params,
        } = frame.message
        else {
            panic!("expected quota callback");
        };
        assert!(method == QUOTA_DETAILS || method == QUOTA_REFRESH);
        assert_eq!(params, json!({}));
        assert!(id > 0 && id.is_multiple_of(2));
        let query: QuotaFactsQuery = serde_json::from_slice(&frame.payload).unwrap();
        Self {
            id,
            parent_id,
            account_id: query.account_id,
            method,
        }
    }
}

impl PluginProcess {
    pub async fn spawn(configuration: Value) -> Self {
        let manifest = Manifest::from_author_slice(include_bytes!("../../plugin.json")).unwrap();
        let handshake = Handshake {
            protocol_version: PROTOCOL_VERSION,
            artifact_sha256: "a".repeat(64),
            plugin_id: manifest.plugin_id().unwrap(),
            instance_id: "quota-guard-test".into(),
            generation: 1,
            incarnation: format!("test-{}", NEXT_INCARNATION.fetch_add(1, Ordering::Relaxed)),
            configuration,
            contributes: manifest.contributes,
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_cpr-quota-guard"))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("start actual plugin binary");
        let mut process = Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
            handshake,
            next_call_id: 1,
        };
        process
            .control(Message::Hello {
                handshake: process.handshake.clone(),
            })
            .await;
        process
    }

    pub async fn start(configuration: Value) -> Self {
        let mut process = Self::spawn(configuration).await;
        let ready = process.wire().await;
        assert!(ready.payload.is_empty());
        let Message::Ready {
            protocol_version,
            incarnation,
            ..
        } = ready.message
        else {
            panic!("expected Ready");
        };
        assert_eq!(protocol_version, PROTOCOL_VERSION);
        assert_eq!(incarnation, process.handshake.incarnation);
        let id = process
            .call("plugin.register", Stage::Registration, json!({}), vec![])
            .await;
        let reply = process.next_frame().await;
        assert!(reply.payload.is_empty());
        let Message::Result { id: actual, result } = reply.message else {
            panic!("expected registration result");
        };
        assert_eq!(actual, id);
        let registration: Registration = serde_json::from_value(result).unwrap();
        assert_eq!(registration.contributes, process.handshake.contributes);
        process
    }

    pub async fn call(
        &mut self,
        method: &str,
        stage: Stage,
        params: Value,
        payload: Vec<u8>,
    ) -> u64 {
        let id = self.next_call_id;
        self.next_call_id += 2;
        self.send(Frame {
            message: Message::Call {
                id,
                method: method.into(),
                context: CallContext {
                    request: None,
                    call_id: id,
                    instance_id: self.handshake.instance_id.clone(),
                    generation: self.handshake.generation,
                    incarnation: self.handshake.incarnation.clone(),
                    stage,
                    timeout_ms: 2_000,
                    resource_stream: false,
                    resource_scope_id: format!("scope-{id}"),
                    request_id: Some(format!("request-{id}")),
                    attempt_id: None,
                    account_id: None,
                    credential_revision: None,
                },
                params,
            },
            payload,
        })
        .await;
        id
    }

    pub async fn schedule_call(&mut self, request: AccountScheduleRequest) -> u64 {
        self.call(
            "policy.schedule_account",
            Stage::Scheduling,
            serde_json::to_value(request).unwrap(),
            vec![],
        )
        .await
    }

    pub async fn schedule<F>(
        &mut self,
        request: AccountScheduleRequest,
        respond: F,
    ) -> (AccountScheduleDecision, Vec<String>)
    where
        F: FnMut(&str) -> Result<QuotaDetails, PluginFault>,
    {
        let id = self.schedule_call(request).await;
        let (reply, queried) = self.serve(id, respond).await;
        (decision(reply, id), queried)
    }

    pub async fn serve<F>(&mut self, id: u64, mut respond: F) -> (Frame, Vec<String>)
    where
        F: FnMut(&str) -> Result<QuotaDetails, PluginFault>,
    {
        let mut queried = Vec::new();
        loop {
            let frame = self.next_frame().await;
            if matches!(frame.message, Message::Callback { .. }) {
                let callback = QuotaCall::from_frame(frame);
                assert_eq!(callback.parent_id, id);
                queried.push(callback.account_id.clone());
                self.reply(&callback, respond(&callback.account_id)).await;
            } else {
                match frame.message {
                    Message::Result { id: actual, .. } | Message::Error { id: actual, .. } => {
                        assert_eq!(actual, id);
                    }
                    _ => panic!("unexpected terminal frame: {:?}", frame.message),
                }
                return (frame, queried);
            }
        }
    }

    pub async fn reply(&mut self, call: &QuotaCall, quota: Result<QuotaDetails, PluginFault>) {
        assert_eq!(call.method, QUOTA_DETAILS);
        let frame = match quota {
            Ok(quota) => Frame {
                message: Message::Result {
                    id: call.id,
                    result: json!({}),
                },
                payload: serde_json::to_vec(&quota).unwrap(),
            },
            Err(error) => Frame::control(Message::Error { id: call.id, error }),
        };
        self.send(frame).await;
    }

    pub async fn reply_refresh(
        &mut self,
        call: &QuotaCall,
        quota: Result<QuotaFacts, PluginFault>,
    ) {
        assert_eq!(call.method, QUOTA_REFRESH);
        let frame = match quota {
            Ok(quota) => Frame {
                message: Message::Result {
                    id: call.id,
                    result: json!({}),
                },
                payload: serde_json::to_vec(&quota).unwrap(),
            },
            Err(error) => Frame::control(Message::Error { id: call.id, error }),
        };
        self.send(frame).await;
    }

    pub async fn reconcile<R, F>(
        &mut self,
        mut read: R,
        mut refresh: F,
    ) -> (Vec<String>, Vec<String>)
    where
        R: FnMut(&str) -> Result<QuotaDetails, PluginFault>,
        F: FnMut(&str) -> Result<QuotaFacts, PluginFault>,
    {
        let parent = self
            .call("plugin.reconcile", Stage::Maintenance, json!({}), vec![])
            .await;
        let mut reads = Vec::new();
        let mut refreshes = Vec::new();
        loop {
            let frame = self.next_frame().await;
            if matches!(frame.message, Message::Callback { .. }) {
                let callback = QuotaCall::from_frame(frame);
                assert_eq!(callback.parent_id, parent);
                if callback.method == QUOTA_DETAILS {
                    reads.push(callback.account_id.clone());
                    self.reply(&callback, read(&callback.account_id)).await;
                } else {
                    refreshes.push(callback.account_id.clone());
                    self.reply_refresh(&callback, refresh(&callback.account_id))
                        .await;
                }
            } else {
                assert!(frame.payload.is_empty());
                assert!(
                    matches!(frame.message, Message::Result { id, result } if id == parent && result == json!({}))
                );
                return (reads, refreshes);
            }
        }
    }

    pub async fn send(&mut self, frame: Frame) {
        tokio::time::timeout(TEST_TIMEOUT, write_frame(&mut self.input, &frame))
            .await
            .expect("plugin input stalled")
            .expect("write SDK frame");
    }

    pub async fn control(&mut self, message: Message) {
        self.send(Frame::control(message)).await;
    }

    // 超时立即终止测试，由 kill_on_drop 回收进程，不复用半帧读取器
    pub async fn wire(&mut self) -> Frame {
        tokio::time::timeout(TEST_TIMEOUT, read_frame(&mut self.output))
            .await
            .expect("plugin output stalled")
            .expect("read SDK frame")
    }

    pub async fn next_frame(&mut self) -> Frame {
        loop {
            let frame = self.wire().await;
            match frame.message {
                Message::Suspend { .. } | Message::CallbackConsumed { .. } => {
                    assert!(frame.payload.is_empty());
                }
                Message::Resume { id, epoch } => {
                    assert!(frame.payload.is_empty());
                    self.control(Message::Resumed { id, epoch }).await;
                }
                _ => return frame,
            }
        }
    }

    pub async fn shutdown(mut self) {
        self.control(Message::Shutdown).await;
        let status = tokio::time::timeout(TEST_TIMEOUT, self.child.wait())
            .await
            .expect("plugin did not shut down")
            .expect("wait for plugin");
        assert!(status.success());
    }

    pub async fn expect_configuration_rejected(mut self) {
        assert!(
            tokio::time::timeout(TEST_TIMEOUT, read_frame(&mut self.output))
                .await
                .expect("invalid configuration did not terminate")
                .is_err()
        );
        let status = tokio::time::timeout(TEST_TIMEOUT, self.child.wait())
            .await
            .expect("invalid configuration process did not exit")
            .unwrap();
        assert!(!status.success());
    }
}

pub fn decision(frame: Frame, expected_id: u64) -> AccountScheduleDecision {
    assert!(frame.payload.is_empty());
    let Message::Result { id, result } = frame.message else {
        panic!("expected scheduler result");
    };
    assert_eq!(id, expected_id);
    serde_json::from_value(result).unwrap()
}

pub fn configured(ids: &[&str]) -> Value {
    json!({
        "accounts": ids.iter().map(|id| json!({"accountId":id})).collect::<Vec<_>>(),
        "maxObservationAgeSeconds":300,
    })
}

pub fn request(provider: &str, accounts: &[(&str, bool)]) -> AccountScheduleRequest {
    AccountScheduleRequest {
        request_id: "request-fixture".into(),
        attempt_index: 0,
        provider: provider.into(),
        model: Some("gpt-fixture".into()),
        candidates: accounts
            .iter()
            .map(|(id, available)| AccountScheduleCandidate {
                account_id: (*id).into(),
                available: *available,
                weight: 1,
                in_flight: u32::from(!available),
                maximum_concurrency: 1,
                last_started_at_ms: None,
                quota_reset_at_ms: None,
                quota_remaining_rank: None,
                failure_rate_basis_points: None,
                first_output_latency_ms: None,
            })
            .collect(),
    }
}

pub fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

pub fn quota(account_id: &str, five_hour: f64, weekly: f64) -> QuotaDetails {
    let now = now_ms();
    QuotaDetails {
        schema_version: 1,
        account_id: account_id.into(),
        observed_at_ms: Some(now),
        limit_reached: false,
        windows: [(18_000, five_hour), (604_800, weekly)]
            .into_iter()
            .map(|(seconds, used)| QuotaWindowDetails {
                key: format!("window-{seconds}"),
                group: "primary".into(),
                label: format!("{seconds}s"),
                limit_id: Some("codex".into()),
                limit_name: Some("Codex".into()),
                role: None,
                window_seconds: Some(seconds),
                used_percent: Some(used),
                reset_at_ms: Some(now + 60_000),
                limit_reached: false,
            })
            .collect(),
        credits: None,
    }
}

pub fn refreshed(account_id: &str) -> QuotaFacts {
    let details = quota(account_id, 20.0, 20.0);
    QuotaFacts {
        schema_version: details.schema_version,
        account_id: details.account_id,
        observed_at_ms: details.observed_at_ms,
        windows: details
            .windows
            .into_iter()
            .map(|window| QuotaWindowFacts {
                key: window.key,
                window_seconds: window.window_seconds,
                used_percent: window.used_percent,
                reset_at_ms: window.reset_at_ms,
            })
            .collect(),
    }
}
