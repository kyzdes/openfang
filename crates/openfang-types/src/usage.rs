//! Per-call LLM accounting facts.
//!
//! The unit of accounting is **one LLM call**, not one agent turn. A turn is an
//! array of [`LlmCall`]s and every reporting surface is a projection of that
//! array, so surfaces cannot contradict each other.
//!
//! Why per-call: a turn may switch models between iterations (the primary is
//! down for iteration 0, back for iteration 1). A per-turn record physically
//! cannot split those tokens, which is how the whole turn ended up booked to a
//! model that served only its last call.

use serde::{Deserialize, Serialize};

/// One LLM call — the unit of accounting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmCall {
    /// 0-based call index within the turn; equals the agent-loop iteration.
    pub n: u32,
    /// Provider that actually served this call.
    pub provider: String,
    /// Accounting name of the model that served this call — spelled as
    /// configured (e.g. `y7router/kimi/k3`), not as sent on the wire.
    pub model: String,
    /// `Some` only when a **substitute** served this call: who was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    /// `Some` only on substitution: how the requested model's attempt ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Input tokens consumed by this call.
    pub input_tokens: u64,
    /// Output tokens produced by this call.
    pub output_tokens: u64,
    /// Tool calls attributed to this call (see the turn-sum rule in the loop).
    pub tool_calls: u32,
    /// Estimated cost in USD (filled in by the kernel; the loop cannot price).
    pub cost_usd: f64,
}

impl LlmCall {
    /// True when a substitute — not the requested model — served this call.
    pub fn substituted(&self) -> bool {
        self.requested.is_some()
    }
}

/// Turn-level rollup of substitution. Never stored — always derived from
/// `&[LlmCall]`, which is why "fell back from X to X" cannot be expressed:
/// `requested` and `served_by` are read from *different fields of one row*.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FallbackSummary {
    /// Always `true` — the struct is only emitted when a substitution happened.
    pub used: bool,
    /// How many calls of the turn a substitute served.
    pub calls: u32,
    /// Total calls in the turn.
    pub of: u32,
    /// The model that was asked for on the first substituted call.
    pub requested: String,
    /// Models that actually served substituted calls, in order, de-duplicated.
    pub served_by: Vec<String>,
    /// How the requested model's attempt ended on the first substituted call.
    pub reason: String,
}

/// Derive the substitution rollup for a turn, or `None` when every call was
/// served by the model that was asked for.
pub fn fallback_summary(calls: &[LlmCall]) -> Option<FallbackSummary> {
    let first = calls.iter().find(|c| c.substituted())?;
    let mut served_by: Vec<String> = Vec::new();
    let mut substituted_calls = 0u32;
    for call in calls.iter().filter(|c| c.substituted()) {
        substituted_calls += 1;
        if !served_by.contains(&call.model) {
            served_by.push(call.model.clone());
        }
    }
    // Note what cannot happen here: `requested` is read from the substituted
    // call's `requested` field and `served_by` from its `model` field, so the
    // two can only coincide if a fallback entry really is configured with the
    // same model name as the primary (two endpoints for one model — a legal
    // setup). What is structurally impossible is the defect this grain change
    // exists to kill: naming the model that merely *failed* as the one that
    // served, which is what a per-turn record could not avoid.
    Some(FallbackSummary {
        used: true,
        calls: substituted_calls,
        of: calls.len() as u32,
        requested: first.requested.clone().unwrap_or_default(),
        served_by,
        reason: first.reason.clone().unwrap_or_default(),
    })
}

/// Provider and accounting model name of the turn's last call.
pub fn last_served(calls: &[LlmCall]) -> Option<(&str, &str)> {
    calls
        .last()
        .map(|c| (c.provider.as_str(), c.model.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(n: u32, model: &str, requested: Option<&str>) -> LlmCall {
        LlmCall {
            n,
            provider: "hyperfusion".to_string(),
            model: model.to_string(),
            requested: requested.map(str::to_string),
            reason: requested.map(|_| "HTTP error: connection refused".to_string()),
            input_tokens: 100,
            output_tokens: 10,
            tool_calls: 0,
            cost_usd: 0.0,
        }
    }

    #[test]
    fn mixed_turn_names_the_substitute_and_the_requested_model_separately() {
        // Iteration 0 served by the substitute, iteration 1 by the primary —
        // the exact shape that produced "fell back from adv-primary to
        // adv-primary" under per-turn accounting.
        let calls = vec![
            call(0, "adv-fallback", Some("adv-primary")),
            call(1, "adv-primary", None),
        ];
        let s = fallback_summary(&calls).expect("substitution happened");
        assert_eq!(s.requested, "adv-primary");
        assert_eq!(s.served_by, vec!["adv-fallback".to_string()]);
        assert_eq!(s.calls, 1);
        assert_eq!(s.of, 2);
        assert!(
            !s.served_by.contains(&s.requested),
            "the requested model must never appear as one of its own servers"
        );
    }

    #[test]
    fn no_substitution_yields_no_summary() {
        let calls = vec![call(0, "adv-primary", None), call(1, "adv-primary", None)];
        assert!(fallback_summary(&calls).is_none());
        assert!(fallback_summary(&[]).is_none());
    }

    #[test]
    fn served_by_is_deduplicated_in_order_of_appearance() {
        let calls = vec![
            call(0, "sub-b", Some("primary")),
            call(1, "sub-a", Some("primary")),
            call(2, "sub-b", Some("primary")),
        ];
        let s = fallback_summary(&calls).unwrap();
        assert_eq!(s.served_by, vec!["sub-b".to_string(), "sub-a".to_string()]);
        assert_eq!(s.calls, 3);
        assert_eq!(s.of, 3);
    }

    #[test]
    fn last_served_reports_the_final_call() {
        let calls = vec![
            call(0, "adv-fallback", Some("adv-primary")),
            call(1, "adv-primary", None),
        ];
        assert_eq!(last_served(&calls), Some(("hyperfusion", "adv-primary")));
        assert_eq!(last_served(&[]), None);
    }

    #[test]
    fn requested_and_reason_are_omitted_when_no_substitution() {
        let json = serde_json::to_value(call(1, "adv-primary", None)).unwrap();
        assert!(json.get("requested").is_none());
        assert!(json.get("reason").is_none());
        let json = serde_json::to_value(call(0, "adv-fallback", Some("adv-primary"))).unwrap();
        assert_eq!(json["requested"], "adv-primary");
    }
}
