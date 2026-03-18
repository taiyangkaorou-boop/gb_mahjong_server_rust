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
    async fn validate_action(&self, request: ValidateActionRequest) -> Result<ValidateActionResponse>;
    async fn calculate_score(&self, request: CalculateScoreRequest) -> Result<CalculateScoreResponse>;
}

#[derive(Clone)]
pub struct TonicRuleEngine {
    endpoint: String,
    channel: Channel,
}

impl TonicRuleEngine {
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
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
    async fn validate_action(&self, request: ValidateActionRequest) -> Result<ValidateActionResponse> {
        let mut client = RuleEngineServiceClient::new(self.channel.clone());
        let response = client
            .validate_action(request)
            .await
            .with_context(|| format!("validate action via rule engine {}", self.endpoint))?;

        Ok(response.into_inner())
    }

    async fn calculate_score(&self, request: CalculateScoreRequest) -> Result<CalculateScoreResponse> {
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
    async fn validate_action(&self, _request: ValidateActionRequest) -> Result<ValidateActionResponse> {
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

    async fn calculate_score(&self, _request: CalculateScoreRequest) -> Result<CalculateScoreResponse> {
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
    validate_requests: Arc<Mutex<Vec<ValidateActionRequest>>>,
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
        }
    }

    pub fn validate_requests(&self) -> Vec<ValidateActionRequest> {
        self.validate_requests
            .lock()
            .expect("validate request lock should not be poisoned")
            .clone()
    }
}

#[cfg(test)]
#[async_trait]
impl RuleEngine for MockRuleEngine {
    async fn validate_action(&self, request: ValidateActionRequest) -> Result<ValidateActionResponse> {
        self.validate_requests
            .lock()
            .expect("validate request lock should not be poisoned")
            .push(request);

        Ok(self.validate_response.clone())
    }

    async fn calculate_score(&self, _request: CalculateScoreRequest) -> Result<CalculateScoreResponse> {
        Ok(self.calculate_response.clone())
    }
}
