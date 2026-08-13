//! Usage tracking store — records LLM usage events for cost monitoring.

use chrono::Utc;
use openfang_types::agent::AgentId;
use openfang_types::error::{OpenFangError, OpenFangResult};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// A single usage event recording one LLM call.
///
/// One row per **call**, not per turn: a turn whose primary model died on the
/// first iteration and came back on the second is two rows sharing a
/// `turn_id`, each booked to the model that actually served it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Which agent made the call.
    pub agent_id: AgentId,
    /// Model used — spelled as configured (the accounting name).
    pub model: String,
    /// Provider that served the call. Empty is stored as NULL ("unknown"),
    /// which is what every pre-v9 row carries.
    pub provider: String,
    /// Turn this call belongs to. Empty means "this row is its own turn" and is
    /// stored as the row id, matching the v9 backfill of legacy rows.
    pub turn_id: String,
    /// 0-based position of this call within its turn.
    pub call_index: u32,
    /// `Some` only when a substitute served the call: who was asked for.
    pub requested_model: Option<String>,
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens consumed.
    pub output_tokens: u64,
    /// Estimated cost in USD.
    pub cost_usd: f64,
    /// Number of tool calls attributed to this call.
    pub tool_calls: u32,
}

impl UsageRecord {
    /// A record for a whole turn served by one model — the pre-v9 shape.
    ///
    /// Used by the kernel's safety net (a turn that somehow reported no calls
    /// still books its tokens) and by tests that do not care about the grain.
    pub fn turn(
        agent_id: AgentId,
        model: impl Into<String>,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
        tool_calls: u32,
    ) -> Self {
        Self {
            agent_id,
            model: model.into(),
            provider: String::new(),
            turn_id: String::new(),
            call_index: 0,
            requested_model: None,
            input_tokens,
            output_tokens,
            cost_usd,
            tool_calls,
        }
    }
}

/// Summary of usage over a period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    /// Total input tokens.
    pub total_input_tokens: u64,
    /// Total output tokens.
    pub total_output_tokens: u64,
    /// Total estimated cost in USD.
    pub total_cost_usd: f64,
    /// Total number of LLM calls.
    pub call_count: u64,
    /// Total tool calls.
    pub total_tool_calls: u64,
    /// Number of distinct turns those calls belong to. Equals `call_count` for
    /// pre-v9 rows, which were one row per turn.
    pub turn_count: u64,
}

/// Usage grouped by model (and provider).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Model name, as configured.
    pub model: String,
    /// Provider that served these calls; `None` for pre-v9 rows.
    pub provider: Option<String>,
    /// Total cost for this model.
    pub total_cost_usd: f64,
    /// Total input tokens.
    pub total_input_tokens: u64,
    /// Total output tokens.
    pub total_output_tokens: u64,
    /// Number of LLM calls.
    pub call_count: u64,
    /// Number of distinct turns these calls belong to.
    pub turn_count: u64,
    /// How many of these calls this model served as a *substitute* for another.
    pub substitute_calls: u64,
}

/// The most recent LLM call of an agent — an observation, not configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastCall {
    /// Provider that served it; `None` for pre-v9 rows.
    pub provider: Option<String>,
    /// Model that served it, as configured.
    pub model: String,
    /// `Some` only when this call was served by a substitute.
    pub requested: Option<String>,
    /// When it happened (RFC3339).
    pub at: String,
    /// True when *any* call of that same turn was served by a substitute —
    /// a turn-level fact kept out of `model` so it cannot corrupt it.
    pub turn_substituted: bool,
}

/// Daily usage breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyBreakdown {
    /// Date string (YYYY-MM-DD).
    pub date: String,
    /// Total cost for this day.
    pub cost_usd: f64,
    /// Total tokens (input + output).
    pub tokens: u64,
    /// Number of API calls.
    pub calls: u64,
}

/// Usage store backed by SQLite.
#[derive(Clone)]
pub struct UsageStore {
    conn: Arc<Mutex<Connection>>,
}

impl UsageStore {
    /// Create a new usage store wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Record a usage event.
    pub fn record(&self, record: &UsageRecord) -> OpenFangResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        // An empty turn_id means the row is its own turn — same rule the v9
        // backfill applied to legacy rows, so turn counts stay coherent no
        // matter which caller wrote the row.
        let turn_id = if record.turn_id.is_empty() {
            id.clone()
        } else {
            record.turn_id.clone()
        };
        // Columns are listed explicitly so an older binary writing to a v9
        // database keeps working.
        conn.execute(
            "INSERT INTO usage_events (id, agent_id, timestamp, model, input_tokens, output_tokens,
                                       cost_usd, tool_calls, provider, turn_id, call_index, requested_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                record.agent_id.0.to_string(),
                now,
                record.model,
                record.input_tokens as i64,
                record.output_tokens as i64,
                record.cost_usd,
                record.tool_calls as i64,
                (!record.provider.is_empty()).then(|| record.provider.clone()),
                turn_id,
                record.call_index as i64,
                record.requested_model,
            ],
        )
        .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Query total cost in the last hour for an agent.
    pub fn query_hourly(&self, agent_id: AgentId) -> OpenFangResult<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let cost: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events
                 WHERE agent_id = ?1 AND timestamp > datetime('now', '-1 hour')",
                rusqlite::params![agent_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(cost)
    }

    /// Query total cost today for an agent.
    pub fn query_daily(&self, agent_id: AgentId) -> OpenFangResult<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let cost: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events
                 WHERE agent_id = ?1 AND timestamp > datetime('now', 'start of day')",
                rusqlite::params![agent_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(cost)
    }

    /// Query total cost in the current calendar month for an agent.
    pub fn query_monthly(&self, agent_id: AgentId) -> OpenFangResult<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let cost: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events
                 WHERE agent_id = ?1 AND timestamp > datetime('now', 'start of month')",
                rusqlite::params![agent_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(cost)
    }

    /// Query total cost across all agents for the current hour.
    pub fn query_global_hourly(&self) -> OpenFangResult<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let cost: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events
                 WHERE timestamp > datetime('now', '-1 hour')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(cost)
    }

    /// Query total cost across all agents for the current calendar month.
    pub fn query_global_monthly(&self) -> OpenFangResult<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let cost: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events
                 WHERE timestamp > datetime('now', 'start of month')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(cost)
    }

    /// Query usage summary, optionally filtered by agent.
    pub fn query_summary(&self, agent_id: Option<AgentId>) -> OpenFangResult<UsageSummary> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;

        let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match agent_id {
            Some(aid) => (
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cost_usd), 0.0), COUNT(*), COALESCE(SUM(tool_calls), 0),
                        COUNT(DISTINCT turn_id)
                 FROM usage_events WHERE agent_id = ?1",
                vec![Box::new(aid.0.to_string())],
            ),
            None => (
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cost_usd), 0.0), COUNT(*), COALESCE(SUM(tool_calls), 0),
                        COUNT(DISTINCT turn_id)
                 FROM usage_events",
                vec![],
            ),
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let summary = conn
            .query_row(sql, params_refs.as_slice(), |row| {
                Ok(UsageSummary {
                    total_input_tokens: row.get::<_, i64>(0)? as u64,
                    total_output_tokens: row.get::<_, i64>(1)? as u64,
                    total_cost_usd: row.get(2)?,
                    call_count: row.get::<_, i64>(3)? as u64,
                    total_tool_calls: row.get::<_, i64>(4)? as u64,
                    turn_count: row.get::<_, i64>(5)? as u64,
                })
            })
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;

        Ok(summary)
    }

    /// Query usage grouped by model and provider.
    pub fn query_by_model(&self) -> OpenFangResult<Vec<ModelUsage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;

        let mut stmt = conn
            .prepare(
                // Grouped by model alone, deliberately. `GROUP BY model, provider` splits every
                // model in two after the v9 migration: rows written before it carry
                // provider = NULL, rows written after carry the real provider. The dashboard
                // keys its table on `m.model` (static/index_body.html), and Alpine keeps one
                // node per key — so a duplicated key silently renders the wrong row's numbers
                // on the one screen this data exists for.
                //
                // `provider` is therefore reported only when the model has exactly one across
                // all its rows; a model served by two providers reports NULL rather than
                // arbitrarily picking one. Per-provider figures live in the new
                // openfang_llm_* metrics, which carry both labels without this ambiguity.
                "SELECT model,
                        CASE WHEN COUNT(DISTINCT provider) = 1 THEN MAX(provider) ELSE NULL END,
                        COALESCE(SUM(cost_usd), 0.0), COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0), COUNT(*), COUNT(DISTINCT turn_id),
                        COALESCE(SUM(requested_model IS NOT NULL), 0)
                 FROM usage_events GROUP BY model ORDER BY SUM(cost_usd) DESC",
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ModelUsage {
                    model: row.get(0)?,
                    provider: row.get(1)?,
                    total_cost_usd: row.get(2)?,
                    total_input_tokens: row.get::<_, i64>(3)? as u64,
                    total_output_tokens: row.get::<_, i64>(4)? as u64,
                    call_count: row.get::<_, i64>(5)? as u64,
                    turn_count: row.get::<_, i64>(6)? as u64,
                    substitute_calls: row.get::<_, i64>(7)? as u64,
                })
            })
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| OpenFangError::Memory(e.to_string()))?);
        }
        Ok(results)
    }

    /// Query the agent's most recent LLM call.
    ///
    /// Answered from the database rather than an in-memory slot: one source of
    /// truth, and it survives a daemon restart. `None` means no turn of this
    /// agent has reached an LLM yet — which is *not* the same statement as
    /// "the configured model".
    pub fn query_last_call(&self, agent_id: AgentId) -> OpenFangResult<Option<LastCall>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;

        use rusqlite::OptionalExtension;
        let row = conn
            .query_row(
                "SELECT provider, model, requested_model, timestamp, turn_id
                 FROM usage_events WHERE agent_id = ?1
                 ORDER BY timestamp DESC, call_index DESC LIMIT 1",
                rusqlite::params![agent_id.0.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;

        let Some((provider, model, requested, at, turn_id)) = row else {
            return Ok(None);
        };

        // The substitution is a fact about the turn, not about its last call:
        // the primary can come back and write the final answer.
        let turn_substituted = match &turn_id {
            Some(tid) => conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM usage_events
                                   WHERE turn_id = ?1 AND requested_model IS NOT NULL)",
                    rusqlite::params![tid],
                    |row| row.get::<_, i64>(0),
                )
                .map(|v| v != 0)
                .unwrap_or(false),
            None => requested.is_some(),
        };

        Ok(Some(LastCall {
            provider,
            model,
            requested,
            at,
            turn_substituted,
        }))
    }

    /// Query daily usage breakdown for the last N days.
    pub fn query_daily_breakdown(&self, days: u32) -> OpenFangResult<Vec<DailyBreakdown>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;

        let mut stmt = conn
            .prepare(&format!(
                "SELECT date(timestamp) as day,
                            COALESCE(SUM(cost_usd), 0.0),
                            COALESCE(SUM(input_tokens) + SUM(output_tokens), 0),
                            COUNT(*)
                     FROM usage_events
                     WHERE timestamp > datetime('now', '-{days} days')
                     GROUP BY day
                     ORDER BY day ASC"
            ))
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(DailyBreakdown {
                    date: row.get(0)?,
                    cost_usd: row.get(1)?,
                    tokens: row.get::<_, i64>(2)? as u64,
                    calls: row.get::<_, i64>(3)? as u64,
                })
            })
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| OpenFangError::Memory(e.to_string()))?);
        }
        Ok(results)
    }

    /// Query the timestamp of the earliest usage event.
    pub fn query_first_event_date(&self) -> OpenFangResult<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let result: Option<String> = conn
            .query_row("SELECT MIN(timestamp) FROM usage_events", [], |row| {
                row.get(0)
            })
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(result)
    }

    /// Query today's total cost across all agents.
    pub fn query_today_cost(&self) -> OpenFangResult<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let cost: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events
                 WHERE timestamp > datetime('now', 'start of day')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(cost)
    }

    /// Delete usage events older than the given number of days.
    pub fn cleanup_old(&self, days: u32) -> OpenFangResult<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| OpenFangError::Internal(e.to_string()))?;
        let deleted = conn
            .execute(
                &format!(
                    "DELETE FROM usage_events WHERE timestamp < datetime('now', '-{days} days')"
                ),
                [],
            )
            .map_err(|e| OpenFangError::Memory(e.to_string()))?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;

    fn setup() -> UsageStore {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        UsageStore::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn test_record_and_query_summary() {
        let store = setup();
        let agent_id = AgentId::new();

        store
            .record(&UsageRecord::turn(agent_id, "claude-haiku", 100, 50, 0.001, 2))
            .unwrap();

        store
            .record(&UsageRecord::turn(agent_id, "claude-sonnet", 500, 200, 0.01, 1))
            .unwrap();

        let summary = store.query_summary(Some(agent_id)).unwrap();
        assert_eq!(summary.call_count, 2);
        assert_eq!(summary.total_input_tokens, 600);
        assert_eq!(summary.total_output_tokens, 250);
        assert!((summary.total_cost_usd - 0.011).abs() < 0.0001);
        assert_eq!(summary.total_tool_calls, 3);
    }

    #[test]
    fn test_query_summary_all_agents() {
        let store = setup();
        let a1 = AgentId::new();
        let a2 = AgentId::new();

        store
            .record(&UsageRecord::turn(a1, "haiku", 100, 50, 0.001, 0))
            .unwrap();

        store
            .record(&UsageRecord::turn(a2, "sonnet", 200, 100, 0.005, 1))
            .unwrap();

        let summary = store.query_summary(None).unwrap();
        assert_eq!(summary.call_count, 2);
        assert_eq!(summary.total_input_tokens, 300);
    }

    #[test]
    fn test_query_by_model() {
        let store = setup();
        let agent_id = AgentId::new();

        for _ in 0..3 {
            store
                .record(&UsageRecord::turn(agent_id, "haiku", 100, 50, 0.001, 0))
                .unwrap();
        }

        store
            .record(&UsageRecord::turn(agent_id, "sonnet", 500, 200, 0.01, 1))
            .unwrap();

        let by_model = store.query_by_model().unwrap();
        assert_eq!(by_model.len(), 2);
        // sonnet should be first (highest cost)
        assert_eq!(by_model[0].model, "sonnet");
        assert_eq!(by_model[1].model, "haiku");
        assert_eq!(by_model[1].call_count, 3);
    }

    #[test]
    fn test_query_hourly() {
        let store = setup();
        let agent_id = AgentId::new();

        store
            .record(&UsageRecord::turn(agent_id, "haiku", 100, 50, 0.05, 0))
            .unwrap();

        let hourly = store.query_hourly(agent_id).unwrap();
        assert!((hourly - 0.05).abs() < 0.001);
    }

    #[test]
    fn test_query_daily() {
        let store = setup();
        let agent_id = AgentId::new();

        store
            .record(&UsageRecord::turn(agent_id, "haiku", 100, 50, 0.123, 0))
            .unwrap();

        let daily = store.query_daily(agent_id).unwrap();
        assert!((daily - 0.123).abs() < 0.001);
    }

    #[test]
    fn test_cleanup_old() {
        let store = setup();
        let agent_id = AgentId::new();

        store
            .record(&UsageRecord::turn(agent_id, "haiku", 100, 50, 0.001, 0))
            .unwrap();

        // Cleanup events older than 1 day should not remove today's events
        let deleted = store.cleanup_old(1).unwrap();
        assert_eq!(deleted, 0);

        let summary = store.query_summary(None).unwrap();
        assert_eq!(summary.call_count, 1);
    }

    #[test]
    fn test_empty_summary() {
        let store = setup();
        let summary = store.query_summary(None).unwrap();
        assert_eq!(summary.call_count, 0);
        assert_eq!(summary.total_cost_usd, 0.0);
    }

    // ---------------------------------------------------------------------
    // Per-call grain (v9)
    // ---------------------------------------------------------------------

    /// The mixed turn from the adversarial probe: iteration 0 served by the
    /// substitute, iteration 1 by the primary that came back.
    fn record_mixed_turn(store: &UsageStore, agent_id: AgentId) -> String {
        let turn_id = "turn-mixed".to_string();
        store
            .record(&UsageRecord {
                agent_id,
                model: "adv-fallback".to_string(),
                provider: "hyperfusion".to_string(),
                turn_id: turn_id.clone(),
                call_index: 0,
                requested_model: Some("adv-primary".to_string()),
                input_tokens: 202,
                output_tokens: 22,
                cost_usd: 0.000268,
                tool_calls: 1,
            })
            .unwrap();
        store
            .record(&UsageRecord {
                agent_id,
                model: "adv-primary".to_string(),
                provider: "hyperfusion".to_string(),
                turn_id: turn_id.clone(),
                call_index: 1,
                requested_model: None,
                input_tokens: 101,
                output_tokens: 11,
                cost_usd: 0.000134,
                tool_calls: 0,
            })
            .unwrap();
        turn_id
    }

    #[test]
    fn test_mixed_turn_splits_tokens_across_two_model_rows() {
        let store = setup();
        let agent_id = AgentId::new();
        record_mixed_turn(&store, agent_id);

        let by_model = store.query_by_model().unwrap();
        assert_eq!(by_model.len(), 2, "each model that served must be present");
        let sub = by_model
            .iter()
            .find(|m| m.model == "adv-fallback")
            .expect("the substitute must not be missing — it served 202/22");
        let primary = by_model.iter().find(|m| m.model == "adv-primary").unwrap();

        assert_eq!(sub.total_input_tokens, 202);
        assert_eq!(sub.total_output_tokens, 22);
        assert_eq!(sub.provider.as_deref(), Some("hyperfusion"));
        assert_eq!(sub.call_count, 1);
        assert_eq!(sub.turn_count, 1);
        assert_eq!(sub.substitute_calls, 1);

        assert_eq!(primary.total_input_tokens, 101);
        assert_eq!(primary.total_output_tokens, 11);
        assert_eq!(primary.call_count, 1);
        assert_eq!(primary.turn_count, 1);
        assert_eq!(
            primary.substitute_calls, 0,
            "the primary served as itself, not as a substitute"
        );
    }

    #[test]
    fn test_summary_counts_calls_and_turns_separately() {
        let store = setup();
        let agent_id = AgentId::new();
        record_mixed_turn(&store, agent_id);

        let s = store.query_summary(Some(agent_id)).unwrap();
        assert_eq!(s.call_count, 2, "two LLM calls");
        assert_eq!(s.turn_count, 1, "one turn");
        assert_eq!(s.total_input_tokens, 303);
        assert_eq!(s.total_output_tokens, 33);
        assert_eq!(
            s.total_tool_calls, 1,
            "iterations - 1, unchanged by the new grain"
        );
    }

    #[test]
    fn test_legacy_shaped_rows_report_one_turn_per_call() {
        // Rows written without a turn_id (pre-v9 writers, and the kernel's
        // safety net) must each count as their own turn.
        let store = setup();
        let agent_id = AgentId::new();
        store
            .record(&UsageRecord::turn(agent_id, "haiku", 100, 50, 0.001, 0))
            .unwrap();
        store
            .record(&UsageRecord::turn(agent_id, "haiku", 100, 50, 0.001, 0))
            .unwrap();

        let s = store.query_summary(Some(agent_id)).unwrap();
        assert_eq!(s.call_count, 2);
        assert_eq!(s.turn_count, s.call_count);
        let row = &store.query_by_model().unwrap()[0];
        assert_eq!(row.provider, None, "provider is not invented");
        assert_eq!(row.turn_count, 2);
    }

    #[test]
    fn test_last_call_reports_the_turn_substitution_not_just_the_last_model() {
        let store = setup();
        let agent_id = AgentId::new();
        record_mixed_turn(&store, agent_id);

        let last = store.query_last_call(agent_id).unwrap().expect("a call");
        assert_eq!(last.model, "adv-primary", "the last call was the primary's");
        assert_eq!(
            last.requested, None,
            "that call was not itself a substitution"
        );
        assert!(
            last.turn_substituted,
            "but its turn did fall back — the fact belongs to the turn"
        );
        assert_eq!(last.provider.as_deref(), Some("hyperfusion"));
        assert!(!last.at.is_empty());
    }

    #[test]
    fn test_last_call_is_none_before_any_llm_call() {
        let store = setup();
        assert!(store.query_last_call(AgentId::new()).unwrap().is_none());
    }
}
