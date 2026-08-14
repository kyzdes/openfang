//! Fallback driver — tries multiple LLM drivers in sequence.
//!
//! If the primary driver fails with a non-retryable error, the fallback driver
//! moves to the next driver in the chain.

use crate::llm_driver::{
    CallReport, CompletionRequest, CompletionResponse, LlmDriver, LlmError, StreamEvent,
};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::warn;

/// One entry of a fallback chain.
///
/// Two model names, deliberately: the **wire** name is what the upstream API
/// expects (provider prefix stripped), the **accounting** name is how the model
/// is spelled in config — and that is the key usage is booked under, so that
/// existing `/api/usage/by-model` rows keep their identity.
pub struct FallbackTarget {
    /// The driver to call.
    pub driver: Arc<dyn LlmDriver>,
    /// Wire model name. Empty = leave `request.model` alone (that is how the
    /// primary entry is expressed: "whatever the caller asked for").
    pub model: String,
    /// Accounting model name. Empty = "the requested model".
    pub model_id: String,
    /// Provider name — for the report and the log only, never sent on the wire.
    pub provider: String,
}

impl FallbackTarget {
    /// The model name this entry asks the wire for, given the incoming request.
    fn wire_model<'a>(&'a self, request: &'a CompletionRequest) -> &'a str {
        if self.model.is_empty() {
            &request.model
        } else {
            &self.model
        }
    }

    /// The name this entry is booked under when it acts as a substitute.
    fn accounting_model(&self) -> String {
        if self.model_id.is_empty() {
            self.model.clone()
        } else {
            self.model_id.clone()
        }
    }
}

/// A driver that wraps multiple LLM drivers and tries each in order.
///
/// On failure (including rate-limit and overload), moves to the next driver.
/// Only returns an error when ALL drivers in the chain are exhausted.
/// Each driver is paired with the model name it should use.
pub struct FallbackDriver {
    targets: Vec<FallbackTarget>,
}

impl FallbackDriver {
    /// Create a new fallback driver from an ordered chain of drivers.
    ///
    /// The first entry is the primary; subsequent are fallbacks. Every entry
    /// keeps the request's own model name.
    pub fn new(drivers: Vec<Arc<dyn LlmDriver>>) -> Self {
        Self::with_models(drivers.into_iter().map(|d| (d, String::new())).collect())
    }

    /// Create a new fallback driver with explicit model names for each driver.
    ///
    /// The wire name doubles as the accounting name and the provider is unknown
    /// — callers that know better use [`FallbackDriver::with_targets`].
    pub fn with_models(drivers: Vec<(Arc<dyn LlmDriver>, String)>) -> Self {
        Self::with_targets(
            drivers
                .into_iter()
                .map(|(driver, model)| FallbackTarget {
                    driver,
                    model_id: model.clone(),
                    model,
                    provider: String::new(),
                })
                .collect(),
        )
    }

    /// Create a new fallback driver from fully described targets.
    pub fn with_targets(targets: Vec<FallbackTarget>) -> Self {
        Self { targets }
    }

    /// Build the report for a successful attempt at chain position `i`.
    ///
    /// A nested chain wins: if the inner driver already reported a
    /// substitution, that is the model that reached the wire.
    fn report_for(
        &self,
        i: usize,
        target: &FallbackTarget,
        inner: CallReport,
        first_error: &Option<String>,
    ) -> CallReport {
        if inner.substituted.is_some() {
            let CallReport {
                substituted,
                provider,
                reason,
            } = inner;
            return CallReport {
                substituted,
                provider: provider
                    .or_else(|| (!target.provider.is_empty()).then(|| target.provider.clone())),
                reason,
            };
        }
        if i == 0 {
            // The primary answered — nothing was substituted.
            return CallReport::default();
        }
        CallReport {
            substituted: Some(target.accounting_model()),
            provider: (!target.provider.is_empty()).then(|| target.provider.clone()),
            reason: first_error.clone(),
        }
    }

    fn exhausted(last_error: Option<LlmError>) -> LlmError {
        last_error.unwrap_or_else(|| LlmError::Api {
            status: 0,
            message: "No drivers configured in fallback chain".to_string(),
        })
    }
}

#[async_trait]
impl LlmDriver for FallbackDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.complete_reported(request).await.map(|(r, _)| r)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        self.stream_reported(request, tx).await.map(|(r, _)| r)
    }

    async fn complete_reported(
        &self,
        request: CompletionRequest,
    ) -> Result<(CompletionResponse, CallReport), LlmError> {
        let mut last_error = None;
        let mut first_error: Option<String> = None;

        for (i, target) in self.targets.iter().enumerate() {
            let mut req = request.clone();
            if !target.model.is_empty() {
                req.model = target.model.clone();
            }
            let asked_for = target.wire_model(&request).to_string();
            match target.driver.complete_reported(req).await {
                Ok((response, inner)) => {
                    return Ok((response, self.report_for(i, target, inner, &first_error)));
                }
                Err(e) => {
                    let rate_limited = matches!(
                        e,
                        LlmError::RateLimited { .. } | LlmError::Overloaded { .. }
                    );
                    warn!(
                        driver_index = i,
                        model = %asked_for,
                        error = %e,
                        "{}",
                        if rate_limited {
                            "Driver rate-limited/overloaded, trying next fallback"
                        } else {
                            "Fallback driver failed, trying next"
                        }
                    );
                    if i == 0 {
                        // Sanitised, not raw: this string reaches the caller in the response
                        // body (fallback.reason, calls[].reason) and onward to
                        // /v1/chat/completions, SSE and WS. A provider's 401 body quotes the
                        // key it rejected, so `e.to_string()` here published one. The ordinary
                        // error path already guards this — llm_errors::sanitize_for_user
                        // redacts sk-/key-/Bearer, strips HTML error pages and caps length.
                        let raw = e.to_string();
                        first_error =
                            Some(crate::llm_errors::classify_error(&raw, None).sanitized_message);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(Self::exhausted(last_error))
    }

    async fn stream_reported(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<(CompletionResponse, CallReport), LlmError> {
        let mut last_error = None;
        let mut first_error: Option<String> = None;

        for (i, target) in self.targets.iter().enumerate() {
            let mut req = request.clone();
            if !target.model.is_empty() {
                req.model = target.model.clone();
            }
            let asked_for = target.wire_model(&request).to_string();
            match target.driver.stream_reported(req, tx.clone()).await {
                Ok((response, inner)) => {
                    return Ok((response, self.report_for(i, target, inner, &first_error)));
                }
                Err(e) => {
                    let rate_limited = matches!(
                        e,
                        LlmError::RateLimited { .. } | LlmError::Overloaded { .. }
                    );
                    warn!(
                        driver_index = i,
                        model = %asked_for,
                        error = %e,
                        "{}",
                        if rate_limited {
                            "Driver rate-limited/overloaded (stream), trying next fallback"
                        } else {
                            "Fallback driver (stream) failed, trying next"
                        }
                    );
                    if i == 0 {
                        // Sanitised, not raw: this string reaches the caller in the response
                        // body (fallback.reason, calls[].reason) and onward to
                        // /v1/chat/completions, SSE and WS. A provider's 401 body quotes the
                        // key it rejected, so `e.to_string()` here published one. The ordinary
                        // error path already guards this — llm_errors::sanitize_for_user
                        // redacts sk-/key-/Bearer, strips HTML error pages and caps length.
                        let raw = e.to_string();
                        first_error =
                            Some(crate::llm_errors::classify_error(&raw, None).sanitized_message);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(Self::exhausted(last_error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_driver::CompletionResponse;
    use openfang_types::message::{ContentBlock, StopReason, TokenUsage};

    struct FailDriver;

    #[async_trait]
    impl LlmDriver for FailDriver {
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, LlmError> {
            Err(LlmError::Api {
                status: 500,
                message: "Internal error".to_string(),
            })
        }
    }

    struct OkDriver;

    #[async_trait]
    impl LlmDriver for OkDriver {
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "OK".to_string(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            })
        }
    }

    fn test_request() -> CompletionRequest {
        CompletionRequest {
            model: "test".to_string(),
            messages: vec![],
            tools: vec![],
            max_tokens: 100,
            temperature: 0.0,
            system: None,
            thinking: None,
        }
    }

    #[tokio::test]
    async fn test_fallback_primary_succeeds() {
        let driver = FallbackDriver::new(vec![
            Arc::new(OkDriver) as Arc<dyn LlmDriver>,
            Arc::new(FailDriver) as Arc<dyn LlmDriver>,
        ]);
        let result = driver.complete(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().text(), "OK");
    }

    #[tokio::test]
    async fn test_fallback_primary_fails_secondary_succeeds() {
        let driver = FallbackDriver::new(vec![
            Arc::new(FailDriver) as Arc<dyn LlmDriver>,
            Arc::new(OkDriver) as Arc<dyn LlmDriver>,
        ]);
        let result = driver.complete(test_request()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_fallback_all_fail() {
        let driver = FallbackDriver::new(vec![
            Arc::new(FailDriver) as Arc<dyn LlmDriver>,
            Arc::new(FailDriver) as Arc<dyn LlmDriver>,
        ]);
        let result = driver.complete(test_request()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rate_limit_falls_through() {
        struct RateLimitDriver;

        #[async_trait]
        impl LlmDriver for RateLimitDriver {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, LlmError> {
                Err(LlmError::RateLimited {
                    retry_after_ms: 5000,
                })
            }
        }

        let driver = FallbackDriver::new(vec![
            Arc::new(RateLimitDriver) as Arc<dyn LlmDriver>,
            Arc::new(OkDriver) as Arc<dyn LlmDriver>,
        ]);
        let result = driver.complete(test_request()).await;
        // Rate limit should fall through to the OkDriver fallback
        assert!(result.is_ok());
        assert_eq!(result.unwrap().text(), "OK");
    }

    #[tokio::test]
    async fn test_rate_limit_all_fail() {
        struct RateLimitDriver;

        #[async_trait]
        impl LlmDriver for RateLimitDriver {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, LlmError> {
                Err(LlmError::RateLimited {
                    retry_after_ms: 5000,
                })
            }
        }

        let driver = FallbackDriver::new(vec![
            Arc::new(RateLimitDriver) as Arc<dyn LlmDriver>,
            Arc::new(RateLimitDriver) as Arc<dyn LlmDriver>,
        ]);
        let result = driver.complete(test_request()).await;
        // All drivers rate-limited — error should bubble up
        assert!(matches!(result, Err(LlmError::RateLimited { .. })));
    }

    /// Regression test for #1003: when the primary driver returns a network /
    /// connection error (e.g. LM Studio shut down → reqwest connection refused),
    /// the FallbackDriver MUST escalate to the next driver in the chain instead
    /// of bubbling the error up to the agent loop (which would then retry the
    /// dead primary forever).
    #[tokio::test]
    async fn test_network_error_falls_through_to_secondary() {
        struct NetworkFailDriver;

        #[async_trait]
        impl LlmDriver for NetworkFailDriver {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, LlmError> {
                // Simulates `reqwest::Error` from a connection refused — exactly
                // what an offline LM Studio looks like in production.
                Err(LlmError::Http(
                    "error sending request: connection refused (os error 10061)".to_string(),
                ))
            }
        }

        let driver = FallbackDriver::new(vec![
            Arc::new(NetworkFailDriver) as Arc<dyn LlmDriver>,
            Arc::new(OkDriver) as Arc<dyn LlmDriver>,
        ]);
        let result = driver.complete(test_request()).await;
        assert!(
            result.is_ok(),
            "FallbackDriver should escalate network errors to the next driver"
        );
        assert_eq!(result.unwrap().text(), "OK");
    }

    /// Same as above but for streaming. The streaming path is what the agent
    /// loop hits in practice for LM Studio etc., so it must also fall through.
    #[tokio::test]
    async fn test_network_error_falls_through_streaming() {
        struct NetworkFailDriver;

        #[async_trait]
        impl LlmDriver for NetworkFailDriver {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, LlmError> {
                Err(LlmError::Http("connection refused".to_string()))
            }

            async fn stream(
                &self,
                _req: CompletionRequest,
                _tx: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> Result<CompletionResponse, LlmError> {
                Err(LlmError::Http("connection refused".to_string()))
            }
        }

        let driver = FallbackDriver::new(vec![
            Arc::new(NetworkFailDriver) as Arc<dyn LlmDriver>,
            Arc::new(OkDriver) as Arc<dyn LlmDriver>,
        ]);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let result = driver.stream(test_request(), tx).await;
        assert!(
            result.is_ok(),
            "FallbackDriver::stream should also escalate network errors"
        );
    }

    // ---------------------------------------------------------------------
    // Per-call reporting (who actually served the call)
    // ---------------------------------------------------------------------

    /// Fails with a fixed message so a test can tell *which* entry's error the
    /// report carries.
    struct TaggedFailDriver(&'static str);

    #[async_trait]
    impl LlmDriver for TaggedFailDriver {
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, LlmError> {
            Err(LlmError::Http(self.0.to_string()))
        }

        async fn stream(
            &self,
            _req: CompletionRequest,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> Result<CompletionResponse, LlmError> {
            Err(LlmError::Http(self.0.to_string()))
        }
    }

    fn target(
        driver: Arc<dyn LlmDriver>,
        model: &str,
        model_id: &str,
        provider: &str,
    ) -> FallbackTarget {
        FallbackTarget {
            driver,
            model: model.to_string(),
            model_id: model_id.to_string(),
            provider: provider.to_string(),
        }
    }

    #[tokio::test]
    async fn test_primary_answers_reports_nothing_substituted() {
        let driver = FallbackDriver::with_targets(vec![
            target(Arc::new(OkDriver), "", "", "hyperfusion"),
            target(
                Arc::new(FailDriver),
                "adv-fallback",
                "adv-fallback",
                "hyperfusion",
            ),
        ]);
        let (response, report) = driver.complete_reported(test_request()).await.unwrap();
        assert_eq!(response.text(), "OK");
        assert_eq!(
            report,
            CallReport::default(),
            "a primary that answered must not be reported as a substitution"
        );
    }

    #[tokio::test]
    async fn test_substitution_reports_the_substitute_and_the_primary_error() {
        // Three entries: the report must carry entry 0's error (the model that
        // was asked for), not entry 1's.
        let driver = FallbackDriver::with_targets(vec![
            target(
                Arc::new(TaggedFailDriver("ERR-ZERO")),
                "",
                "",
                "hyperfusion",
            ),
            target(
                Arc::new(TaggedFailDriver("ERR-ONE")),
                "mid",
                "mid",
                "hyperfusion",
            ),
            target(
                Arc::new(OkDriver),
                "adv-fallback",
                "y7router/adv-fallback",
                "hyperfusion",
            ),
        ]);
        let (_, report) = driver.complete_reported(test_request()).await.unwrap();
        assert_eq!(
            report.substituted.as_deref(),
            Some("y7router/adv-fallback"),
            "the substitute is booked under its configured (accounting) name"
        );
        assert_eq!(report.provider.as_deref(), Some("hyperfusion"));
        let reason = report.reason.expect("substitution carries a reason");
        assert!(
            reason.contains("ERR-ZERO") && !reason.contains("ERR-ONE"),
            "reason must be the requested model's failure, got: {reason}"
        );
    }

    #[tokio::test]
    async fn test_stream_substitution_reports_the_substitute() {
        let driver = FallbackDriver::with_targets(vec![
            target(
                Arc::new(TaggedFailDriver("stream-boom")),
                "",
                "",
                "hyperfusion",
            ),
            target(
                Arc::new(OkDriver),
                "adv-fallback",
                "adv-fallback",
                "hyperfusion",
            ),
        ]);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let (_, report) = driver.stream_reported(test_request(), tx).await.unwrap();
        assert_eq!(report.substituted.as_deref(), Some("adv-fallback"));
        assert!(report.reason.unwrap().contains("stream-boom"));
    }

    #[tokio::test]
    async fn test_nested_chain_inner_report_wins() {
        // Outer entry 1 is itself a FallbackDriver whose own primary failed:
        // the model that reached the wire is the inner substitute, so the inner
        // report must survive — the outer only fills in a missing provider.
        let inner = FallbackDriver::with_targets(vec![
            target(Arc::new(FailDriver), "inner-primary", "inner-primary", ""),
            target(Arc::new(OkDriver), "inner-sub", "inner-sub", ""),
        ]);
        let outer = FallbackDriver::with_targets(vec![
            target(Arc::new(FailDriver), "", "", "hyperfusion"),
            target(Arc::new(inner), "outer-sub", "outer-sub", "nested-provider"),
        ]);
        let (_, report) = outer.complete_reported(test_request()).await.unwrap();
        assert_eq!(
            report.substituted.as_deref(),
            Some("inner-sub"),
            "the innermost served model is the one that reached the wire"
        );
        assert_eq!(report.provider.as_deref(), Some("nested-provider"));
    }

    #[tokio::test]
    async fn test_with_models_falls_back_to_the_wire_name_for_accounting() {
        // The `with_models` path knows only one name; the report must use it
        // rather than an empty string.
        let driver = FallbackDriver::with_models(vec![
            (Arc::new(FailDriver) as Arc<dyn LlmDriver>, String::new()),
            (Arc::new(OkDriver) as Arc<dyn LlmDriver>, "sub".to_string()),
        ]);
        let (_, report) = driver.complete_reported(test_request()).await.unwrap();
        assert_eq!(report.substituted.as_deref(), Some("sub"));
        assert_eq!(report.provider, None, "with_models knows no provider");
    }

    /// FANG-38: the kernel hands the *same* `FallbackDriver` instance to every
    /// agent (`resolve_driver` can return the shared `default_driver`). Two
    /// concurrent turns must not read each other's dispatch — which is exactly
    /// what a "last dispatch" slot behind a mutex would allow.
    #[tokio::test]
    async fn test_concurrent_calls_get_their_own_report() {
        /// Fails only for the request model "dead", so one concurrent caller
        /// gets a substitution and the other does not.
        struct PickyDriver;

        #[async_trait]
        impl LlmDriver for PickyDriver {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, LlmError> {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if req.model == "dead" {
                    return Err(LlmError::Http("primary is down".to_string()));
                }
                OkDriver.complete(req).await
            }
        }

        let driver = Arc::new(FallbackDriver::with_targets(vec![
            target(Arc::new(PickyDriver), "", "", "hyperfusion"),
            target(
                Arc::new(OkDriver),
                "adv-fallback",
                "adv-fallback",
                "hyperfusion",
            ),
        ]));

        let a = {
            let driver = Arc::clone(&driver);
            let mut req = test_request();
            req.model = "dead".to_string();
            tokio::spawn(async move { driver.complete_reported(req).await.unwrap().1 })
        };
        let b = {
            let driver = Arc::clone(&driver);
            let mut req = test_request();
            req.model = "alive".to_string();
            tokio::spawn(async move { driver.complete_reported(req).await.unwrap().1 })
        };
        let (report_a, report_b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(
            report_a.substituted.as_deref(),
            Some("adv-fallback"),
            "the turn whose primary died must see the substitution"
        );
        assert_eq!(
            report_b,
            CallReport::default(),
            "the concurrent turn whose primary answered must see no substitution"
        );
    }

    /// The WARN line for a failed attempt interpolates this, so `driver_index=0`
    /// no longer logs an empty `model=` (its wire name lives on the request).
    #[test]
    fn test_wire_model_names_the_model_the_attempt_asked_for() {
        let req = test_request();
        let primary = target(Arc::new(OkDriver), "", "", "hyperfusion");
        assert_eq!(primary.wire_model(&req), "test");
        let sub = target(Arc::new(OkDriver), "adv-fallback", "acct-name", "p");
        assert_eq!(sub.wire_model(&req), "adv-fallback");
        assert_eq!(sub.accounting_model(), "acct-name");
    }
}
