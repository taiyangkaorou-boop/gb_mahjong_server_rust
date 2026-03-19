use std::sync::Arc;

#[cfg(test)]
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};

use crate::proto::engine::{
    self, rule_engine_service_client::RuleEngineServiceClient, CalculateScoreRequest,
    CalculateScoreResponse, ValidateActionRequest, ValidateActionResponse,
};

pub const DEFAULT_RULE_ENGINE_ENDPOINT: &str = "http://127.0.0.1:50051";

#[derive(Clone)]
pub struct RuleEngineHandle {
    // 业务层只依赖这个 trait 对象，便于在 tonic、noop、mock 之间切换。
    inner: Arc<dyn RuleEngine>,
}

impl RuleEngineHandle {
    pub fn new(rule_engine: impl RuleEngine + 'static) -> Self {
        Self {
            inner: Arc::new(rule_engine),
        }
    }

    pub fn noop() -> Self {
        Self::new(NoopRuleEngine)
    }

    pub fn tonic(endpoint: impl Into<String>) -> Result<Self> {
        Ok(Self::new(TonicRuleEngine::new(endpoint)?))
    }

    // Handle 自身不做任何业务裁决，只是把调用转发给具体实现。
    pub async fn validate_action(
        &self,
        request: ValidateActionRequest,
    ) -> Result<ValidateActionResponse> {
        self.inner.validate_action(request).await
    }

    pub async fn calculate_score(
        &self,
        request: CalculateScoreRequest,
    ) -> Result<CalculateScoreResponse> {
        self.inner.calculate_score(request).await
    }
}

impl Default for RuleEngineHandle {
    fn default() -> Self {
        Self::noop()
    }
}

#[async_trait]
pub trait RuleEngine: Send + Sync {
    // 校验接口用于“动作是否允许”这一前置判断，不负责房间并发裁决。
    async fn validate_action(
        &self,
        request: ValidateActionRequest,
    ) -> Result<ValidateActionResponse>;
    // 结算接口用于终局算番/算分，调用时机由 Rust 房间状态机决定。
    async fn calculate_score(
        &self,
        request: CalculateScoreRequest,
    ) -> Result<CalculateScoreResponse>;
}

#[derive(Clone)]
pub struct TonicRuleEngine {
    // endpoint 主要用于错误上下文和日志诊断。
    endpoint: String,
    // Channel 是可共享的连接句柄，clone 成本很低。
    channel: Channel,
}

impl TonicRuleEngine {
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        // 使用 lazy channel，避免服务启动时就因规则引擎暂时未就绪而失败。
        let channel = Endpoint::from_shared(endpoint.clone())
            .with_context(|| format!("parse rule engine endpoint {endpoint}"))?
            .tcp_nodelay(true)
            .concurrency_limit(32)
            .connect_lazy();

        Ok(Self { endpoint, channel })
    }
}

#[async_trait]
impl RuleEngine for TonicRuleEngine {
    async fn validate_action(
        &self,
        request: ValidateActionRequest,
    ) -> Result<ValidateActionResponse> {
        // 每次调用都克隆一个轻量 client，底层连接池由 tonic channel 复用。
        let mut client = RuleEngineServiceClient::new(self.channel.clone());
        let response = client
            .validate_action(request)
            .await
            .with_context(|| format!("validate action via rule engine {}", self.endpoint))?;

        Ok(response.into_inner())
    }

    async fn calculate_score(
        &self,
        request: CalculateScoreRequest,
    ) -> Result<CalculateScoreResponse> {
        // 算分与校验共用同一条 channel，便于房间任务在高并发下复用连接池。
        let mut client = RuleEngineServiceClient::new(self.channel.clone());
        let response = client
            .calculate_score(request)
            .await
            .with_context(|| format!("calculate score via rule engine {}", self.endpoint))?;

        Ok(response.into_inner())
    }
}

#[derive(Clone, Copy)]
pub struct NoopRuleEngine;

#[async_trait]
impl RuleEngine for NoopRuleEngine {
    async fn validate_action(
        &self,
        _request: ValidateActionRequest,
    ) -> Result<ValidateActionResponse> {
        // noop 仅用于本地骨架调试，真实房间逻辑不能依赖它给出的合法性结果。
        Ok(ValidateActionResponse {
            is_legal: true,
            reject_code: engine::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine::ActionKind::Unspecified as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "noop rule engine accepted action".to_owned(),
        })
    }

    async fn calculate_score(
        &self,
        _request: CalculateScoreRequest,
    ) -> Result<CalculateScoreResponse> {
        Ok(CalculateScoreResponse {
            total_fan: 0,
            fan_details: Vec::new(),
            score_delta_by_seat: Vec::new(),
            settlement_flags: Vec::new(),
        })
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct MockRuleEngine {
    validate_response: ValidateActionResponse,
    calculate_response: CalculateScoreResponse,
    // 测试里记录请求快照，方便断言房间循环是否把上下文正确拼给引擎。
    validate_requests: Arc<Mutex<Vec<ValidateActionRequest>>>,
    calculate_requests: Arc<Mutex<Vec<CalculateScoreRequest>>>,
}

#[cfg(test)]
impl MockRuleEngine {
    pub fn new(validate_response: ValidateActionResponse) -> Self {
        Self {
            validate_response,
            calculate_response: CalculateScoreResponse {
                total_fan: 0,
                fan_details: Vec::new(),
                score_delta_by_seat: Vec::new(),
                settlement_flags: Vec::new(),
            },
            validate_requests: Arc::new(Mutex::new(Vec::new())),
            calculate_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_calculate_response(mut self, calculate_response: CalculateScoreResponse) -> Self {
        self.calculate_response = calculate_response;
        self
    }

    // 测试读取时返回克隆副本，避免把互斥锁暴露给测试代码。
    pub fn validate_requests(&self) -> Vec<ValidateActionRequest> {
        self.validate_requests
            .lock()
            .expect("validate request lock should not be poisoned")
            .clone()
    }

    pub fn calculate_requests(&self) -> Vec<CalculateScoreRequest> {
        self.calculate_requests
            .lock()
            .expect("calculate request lock should not be poisoned")
            .clone()
    }
}

#[cfg(test)]
#[async_trait]
impl RuleEngine for MockRuleEngine {
    async fn validate_action(
        &self,
        request: ValidateActionRequest,
    ) -> Result<ValidateActionResponse> {
        self.validate_requests
            .lock()
            .expect("validate request lock should not be poisoned")
            .push(request);

        Ok(self.validate_response.clone())
    }

    async fn calculate_score(
        &self,
        _request: CalculateScoreRequest,
    ) -> Result<CalculateScoreResponse> {
        self.calculate_requests
            .lock()
            .expect("calculate request lock should not be poisoned")
            .push(_request);

        Ok(self.calculate_response.clone())
    }
}
