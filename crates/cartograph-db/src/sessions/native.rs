//! Durable native sessions, traces, and macros use atomic state transactions.

use std::collections::BTreeMap;

use super::{
    MAX_MACRO_LIST, MAX_SESSION_CALLS, MAX_SESSION_LIST, McpMacroRecord, McpSessionCallsQuery,
    McpSessionLookup, McpSessionRecord, McpToolCallRecord, McpToolCallWrite, McpToolUsage,
    McpTraceUsage, NewMcpMacro, NewMcpSession, ProjectId, StorageError, database_error,
    validate_arguments, validate_label, validate_limit, validate_macro_name, validate_macro_steps,
    validate_objective, validate_result_summary, validate_tool_call, validate_tool_name,
    validate_uuid,
};
use crate::{
    NativeDatabase,
    native_database::{decode, encode, now, timestamp_order, visit_state},
    retrieval::native::retain,
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};

const WRITE_BYTES: u32 = 2 * 1_048_576;
const COUNTER: &[u8] = b"trace_sequence";
const COUNT: &[u8] = b"trace_count";

impl NativeDatabase {
    /// Create a durable session without requiring a graph publication.
    /// # Errors
    /// Rejects invalid project/input or a failed atomic native write.
    pub async fn create_mcp_session(
        &self,
        project: &ProjectId,
        input: &NewMcpSession,
    ) -> Result<McpSessionRecord, StorageError> {
        self.require_project(project)?;
        if let Some(label) = &input.label {
            validate_label(label)?;
        }
        validate_objective(&input.objective)?;
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy).map_err(|_| database_error("session-identity"))?;
        let timestamp = now();
        let record = McpSessionRecord {
            session_id: cartograph_domain::SessionId::from_uuid_v8(entropy).to_string(),
            label: input.label.clone(),
            objective: input.objective.clone(),
            kind: input.kind,
            state: "active".to_owned(),
            tool_count: 0,
            started_at: timestamp.clone(),
            last_activity_at: timestamp,
        };
        self.state(None, WRITE_BYTES, 1, move |transaction, budget| {
            if transaction
                .get(
                    StateNamespace::Sessions,
                    record.session_id.as_bytes(),
                    budget,
                )?
                .is_some()
            {
                return Err(RocksError::ReplayConflict);
            }
            transaction.put(
                StateNamespace::Sessions,
                record.session_id.as_bytes(),
                encode(&record)?,
            )?;
            Ok(record)
        })
        .await
    }

    /// List newest sessions using bounded snapshot reads.
    /// # Errors
    /// Rejects invalid project/limit or malformed native state.
    pub async fn list_mcp_sessions(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<McpSessionRecord>, StorageError> {
        self.require_project(project)?;
        validate_limit(limit, MAX_SESSION_LIST)?;
        self.state(None, 0, 0, move |transaction, budget| {
            let mut rows = Vec::new();
            visit_state(
                transaction,
                StateNamespace::Sessions,
                b"",
                budget,
                |_, record: McpSessionRecord, _| {
                    validate_session(&record)?;
                    retain(&mut rows, record, usize::from(limit), session_order);
                    Ok(())
                },
            )?;
            Ok(rows)
        })
        .await
    }

    /// Find an exact session or newest matching resumable trace.
    /// # Errors
    /// Rejects invalid discriminators, project identity, or native read failure.
    pub async fn find_mcp_session(
        &self,
        lookup: McpSessionLookup<'_>,
    ) -> Result<Option<McpSessionRecord>, StorageError> {
        self.require_project(lookup.project_id)?;
        if lookup.session_id.is_none() && lookup.label.is_none() && !lookup.require_calls {
            return Err(StorageError::InvalidInput {
                field: "session_lookup",
            });
        }
        if let Some(id) = lookup.session_id {
            validate_uuid(id, "session_id")?;
        }
        if let Some(label) = lookup.label {
            validate_label(label)?;
        }
        let (id, label, calls) = (
            lookup.session_id.map(str::to_ascii_lowercase),
            lookup.label.map(str::to_owned),
            lookup.require_calls,
        );
        self.state(None, 0, 0, move |transaction, budget| {
            let matches = |record: &McpSessionRecord| {
                (!calls || record.tool_count > 0)
                    && (id.is_some()
                        || label
                            .as_ref()
                            .is_none_or(|label| record.label.as_ref() == Some(label)))
            };
            if let Some(id) = &id {
                let record: Option<McpSessionRecord> = transaction
                    .get(StateNamespace::Sessions, id.as_bytes(), budget)?
                    .map(|bytes| decode(&bytes))
                    .transpose()?;
                if let Some(record) = &record {
                    validate_session(record)?;
                }
                return Ok(record.filter(matches));
            }
            let mut found = Vec::new();
            visit_state(
                transaction,
                StateNamespace::Sessions,
                b"",
                budget,
                |_, record: McpSessionRecord, _| {
                    validate_session(&record)?;
                    if matches(&record) {
                        retain(&mut found, record, 1, session_order);
                    }
                    Ok(())
                },
            )?;
            Ok(found.pop())
        })
        .await
    }

    /// Delete one session and its retained trace in the same durable batch.
    /// # Errors
    /// Rejects invalid identity, malformed state, or a failed atomic write.
    pub async fn delete_mcp_session(
        &self,
        project: &ProjectId,
        session: &str,
    ) -> Result<bool, StorageError> {
        self.require_project(project)?;
        validate_uuid(session, "session_id")?;
        let session = session.to_ascii_lowercase();
        self.state(None, WRITE_BYTES, 10_002, move |transaction, budget| {
            if transaction
                .get(StateNamespace::Sessions, session.as_bytes(), budget)?
                .is_none()
            {
                return Ok(false);
            }
            let mut keys = Vec::new();
            visit_state(
                transaction,
                StateNamespace::Trace,
                b"",
                budget,
                |key, record: McpToolCallRecord, _| {
                    if record.session_id == session {
                        keys.push(key);
                    }
                    Ok(())
                },
            )?;
            let count = counter(transaction, COUNT, budget)?;
            let remaining = count.checked_sub(keys.len() as u64).ok_or(corrupt())?;
            for key in keys {
                transaction.delete(StateNamespace::Trace, &key)?;
            }
            transaction.delete(StateNamespace::Sessions, session.as_bytes())?;
            transaction.put(StateNamespace::Metadata, COUNT, encode(&remaining)?)?;
            Ok(true)
        })
        .await
    }

    /// Allocate a session step, append a trace, and enforce project retention atomically.
    /// # Errors
    /// Rejects invalid input, missing session, exhausted bounds, or a failed write.
    pub async fn record_mcp_tool_call(
        &self,
        request: McpToolCallWrite<'_>,
    ) -> Result<McpToolCallRecord, StorageError> {
        self.require_project(request.project_id)?;
        validate_uuid(request.session_id, "session_id")?;
        validate_tool_call(request.input)?;
        let session = request.session_id.to_ascii_lowercase();
        let input = McpToolCallRecord {
            session_id: session.clone(),
            step: 0,
            called_at: now(),
            tool_name: request.input.tool_name.clone(),
            arguments: request.input.arguments.clone(),
            result_summary: request.input.result_summary.clone(),
            success: request.input.success,
            duration_ms: request.input.duration_ms,
        };
        self.state(None, WRITE_BYTES, 5, move |transaction, budget| {
            let mut record: McpSessionRecord = transaction
                .get(StateNamespace::Sessions, session.as_bytes(), budget)?
                .map(|bytes| decode(&bytes))
                .transpose()?
                .ok_or(RocksError::InvalidInput {
                    field: "session_id",
                })?;
            validate_session(&record)?;
            let sequence = counter(transaction, COUNTER, budget)?
                .checked_add(1)
                .ok_or(corrupt())?;
            let count = counter(transaction, COUNT, budget)?;
            let oldest = if count >= 10_000 {
                transaction
                    .scan(StateNamespace::Trace, b"", None, 1, budget)?
                    .rows
                    .into_iter()
                    .next()
                    .map(|(key, _)| key)
            } else {
                None
            };
            if count >= 10_000 && oldest.is_none() {
                return Err(corrupt());
            }
            record.tool_count = record.tool_count.checked_add(1).ok_or(corrupt())?;
            record.last_activity_at.clone_from(&input.called_at);
            let mut input = input;
            input.step = record.tool_count;
            if let Some(oldest) = oldest {
                transaction.delete(StateNamespace::Trace, oldest.as_bytes())?;
            }
            transaction.put(
                StateNamespace::Trace,
                format!("{sequence:016x}").as_bytes(),
                encode(&input)?,
            )?;
            transaction.put(
                StateNamespace::Sessions,
                session.as_bytes(),
                encode(&record)?,
            )?;
            transaction.put(StateNamespace::Metadata, COUNTER, encode(&sequence)?)?;
            transaction.put(
                StateNamespace::Metadata,
                COUNT,
                encode(&count.saturating_add(1).min(10_000))?,
            )?;
            Ok(input)
        })
        .await
    }

    /// Return a session's newest retained calls in ascending step order.
    /// # Errors
    /// Rejects invalid project/session/limit or malformed native trace rows.
    pub async fn mcp_calls_for_session(
        &self,
        request: McpSessionCallsQuery<'_>,
    ) -> Result<Vec<McpToolCallRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_uuid(request.session_id, "session_id")?;
        validate_limit(request.limit, MAX_SESSION_CALLS)?;
        let session = request.session_id.to_ascii_lowercase();
        let limit = usize::from(request.limit);
        self.state(None, 0, 0, move |transaction, budget| {
            let mut rows = Vec::new();
            visit_state(
                transaction,
                StateNamespace::Trace,
                b"",
                budget,
                |_, record: McpToolCallRecord, _| {
                    validate_call(&record)?;
                    if record.session_id == session {
                        retain(&mut rows, record, limit, |a, b| b.step.cmp(&a.step));
                    }
                    Ok(())
                },
            )?;
            rows.reverse();
            Ok(rows)
        })
        .await
    }

    /// Aggregate retained trace usage and exact discrete latency percentiles.
    /// # Errors
    /// Rejects invalid project identity or malformed/excessive native state.
    pub async fn mcp_trace_usage(
        &self,
        project: &ProjectId,
    ) -> Result<McpTraceUsage, StorageError> {
        self.require_project(project)?;
        self.state(None, 0, 0, |transaction, budget| {
            let mut session_count = 0_u64;
            visit_state(
                transaction,
                StateNamespace::Sessions,
                b"",
                budget,
                |_, record: McpSessionRecord, _| {
                    validate_session(&record)?;
                    session_count = session_count.checked_add(1).ok_or(corrupt())?;
                    Ok(())
                },
            )?;
            let mut durations = BTreeMap::<String, Vec<u64>>::new();
            let (mut calls, mut errors) = (0_u64, 0_u64);
            visit_state(
                transaction,
                StateNamespace::Trace,
                b"",
                budget,
                |_, record: McpToolCallRecord, _| {
                    validate_call(&record)?;
                    calls = calls.checked_add(1).ok_or(corrupt())?;
                    errors += u64::from(!record.success);
                    durations
                        .entry(record.tool_name)
                        .or_default()
                        .push(record.duration_ms);
                    Ok(())
                },
            )?;
            let mut tools = Vec::new();
            for (tool_name, mut durations) in durations {
                durations.sort_unstable();
                let length = durations.len();
                let percentile =
                    |percent: usize| durations[(length * percent).div_ceil(100).saturating_sub(1)];
                tools.push(McpToolUsage {
                    tool_name,
                    call_count: length as u64,
                    p50_duration_ms: percentile(50),
                    p95_duration_ms: percentile(95),
                    max_duration_ms: durations.last().copied().ok_or(corrupt())?,
                });
            }
            tools.sort_by(|a, b| {
                b.call_count
                    .cmp(&a.call_count)
                    .then(a.tool_name.cmp(&b.tool_name))
            });
            Ok(McpTraceUsage {
                session_count,
                tool_call_count: calls,
                error_count: errors,
                tools,
            })
        })
        .await
    }

    /// Save a bounded macro recipe, retaining its execution metadata.
    /// # Errors
    /// Rejects invalid project/recipe or a failed atomic write.
    pub async fn save_mcp_macro(
        &self,
        project: &ProjectId,
        input: &NewMcpMacro,
    ) -> Result<McpMacroRecord, StorageError> {
        self.require_project(project)?;
        validate_macro_name(&input.name)?;
        validate_macro_steps(&input.steps)?;
        let (name, steps) = (input.name.clone(), input.steps.clone());
        self.state(None, WRITE_BYTES, 1, move |transaction, budget| {
            let prior: Option<McpMacroRecord> = transaction
                .get(StateNamespace::Macros, name.as_bytes(), budget)?
                .map(|bytes| decode(&bytes))
                .transpose()?;
            if let Some(record) = &prior {
                validate_macro(record)?;
            }
            let timestamp = now();
            let record = McpMacroRecord {
                name,
                steps,
                created_at: prior
                    .as_ref()
                    .map_or_else(|| timestamp.clone(), |record| record.created_at.clone()),
                updated_at: timestamp,
                last_run_at: prior.as_ref().and_then(|record| record.last_run_at.clone()),
                run_count: prior.as_ref().map_or(0, |record| record.run_count),
            };
            transaction.put(
                StateNamespace::Macros,
                record.name.as_bytes(),
                encode(&record)?,
            )?;
            Ok(record)
        })
        .await
    }

    /// Read one project-local saved macro.
    /// # Errors
    /// Rejects invalid project/name or malformed native macro state.
    pub async fn get_mcp_macro(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<McpMacroRecord>, StorageError> {
        self.require_project(project)?;
        validate_macro_name(name)?;
        let name = name.to_owned();
        self.state(None, 0, 0, move |transaction, budget| {
            let record: Option<McpMacroRecord> = transaction
                .get(StateNamespace::Macros, name.as_bytes(), budget)?
                .map(|bytes| decode(&bytes))
                .transpose()?;
            if let Some(record) = &record {
                validate_macro(record)?;
            }
            Ok(record)
        })
        .await
    }

    /// List bounded macro recipes in newest-update order.
    /// # Errors
    /// Rejects invalid project/limit or malformed native macro state.
    pub async fn list_mcp_macros(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<McpMacroRecord>, StorageError> {
        self.require_project(project)?;
        validate_limit(limit, MAX_MACRO_LIST)?;
        self.state(None, 0, 0, move |transaction, budget| {
            let mut rows = Vec::new();
            visit_state(
                transaction,
                StateNamespace::Macros,
                b"",
                budget,
                |_, record: McpMacroRecord, _| {
                    validate_macro(&record)?;
                    retain(&mut rows, record, usize::from(limit), |a, b| {
                        timestamp_order(&b.updated_at, &a.updated_at).then(a.name.cmp(&b.name))
                    });
                    Ok(())
                },
            )?;
            Ok(rows)
        })
        .await
    }

    /// Increment a saved macro's successful run count atomically.
    /// # Errors
    /// Rejects invalid project/name, malformed state, or a failed write.
    pub async fn mark_mcp_macro_run(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<(), StorageError> {
        self.require_project(project)?;
        validate_macro_name(name)?;
        let name = name.to_owned();
        self.state(None, WRITE_BYTES, 1, move |transaction, budget| {
            if let Some(bytes) = transaction.get(StateNamespace::Macros, name.as_bytes(), budget)? {
                let mut record: McpMacroRecord = decode(&bytes)?;
                validate_macro(&record)?;
                record.run_count = record.run_count.checked_add(1).ok_or(corrupt())?;
                record.last_run_at = Some(now());
                transaction.put(StateNamespace::Macros, name.as_bytes(), encode(&record)?)?;
            }
            Ok(())
        })
        .await
    }

    /// Delete one exact project-local macro recipe.
    /// # Errors
    /// Rejects invalid project/name or a failed atomic write.
    pub async fn delete_mcp_macro(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<bool, StorageError> {
        self.require_project(project)?;
        validate_macro_name(name)?;
        let name = name.to_owned();
        self.state(None, WRITE_BYTES, 1, move |transaction, budget| {
            let exists = transaction
                .get(StateNamespace::Macros, name.as_bytes(), budget)?
                .is_some();
            if exists {
                transaction.delete(StateNamespace::Macros, name.as_bytes())?;
            }
            Ok(exists)
        })
        .await
    }
}

fn counter(
    transaction: &StateTransaction<'_>,
    key: &[u8],
    budget: &mut ReadBudget,
) -> Result<u64, RocksError> {
    transaction
        .get(StateNamespace::Metadata, key, budget)?
        .map_or(Ok(0), |bytes| decode(&bytes))
}

fn session_order(a: &McpSessionRecord, b: &McpSessionRecord) -> std::cmp::Ordering {
    timestamp_order(&b.last_activity_at, &a.last_activity_at).then(b.session_id.cmp(&a.session_id))
}

fn validate_session(record: &McpSessionRecord) -> Result<(), RocksError> {
    validate_uuid(&record.session_id, "session_id").map_err(|_| corrupt())?;
    if let Some(label) = &record.label {
        validate_label(label).map_err(|_| corrupt())?;
    }
    validate_objective(&record.objective).map_err(|_| corrupt())?;
    if record.state != "active" && record.state != "closed" {
        return Err(corrupt());
    }
    validate_timestamp(&record.started_at)?;
    validate_timestamp(&record.last_activity_at)
}

fn validate_call(record: &McpToolCallRecord) -> Result<(), RocksError> {
    validate_uuid(&record.session_id, "session_id").map_err(|_| corrupt())?;
    validate_tool_name(&record.tool_name).map_err(|_| corrupt())?;
    validate_arguments(&record.arguments).map_err(|_| corrupt())?;
    validate_result_summary(&record.result_summary).map_err(|_| corrupt())?;
    if record.step == 0 {
        return Err(corrupt());
    }
    validate_timestamp(&record.called_at)
}

fn validate_macro(record: &McpMacroRecord) -> Result<(), RocksError> {
    validate_macro_name(&record.name).map_err(|_| corrupt())?;
    validate_macro_steps(&record.steps).map_err(|_| corrupt())?;
    validate_timestamp(&record.created_at)?;
    validate_timestamp(&record.updated_at)?;
    if let Some(timestamp) = &record.last_run_at {
        validate_timestamp(timestamp)?;
    }
    Ok(())
}

fn validate_timestamp(value: &str) -> Result<(), RocksError> {
    value
        .parse::<jiff::Timestamp>()
        .map(|_| ())
        .map_err(|_| corrupt())
}

const fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "session_state",
    }
}
