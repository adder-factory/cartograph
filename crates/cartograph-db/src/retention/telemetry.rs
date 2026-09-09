use super::{
    CartographDatabase, GenerationRetentionError, GenerationRetentionPolicy,
    GenerationRetentionReport, LeaseFence, RetentionContext, acquire_retention_locks,
    database_error, require_live_fence, validate_fence_shape,
};
use crate::NativeParseCacheRetentionReport;
use serde::Serialize;
use sqlx_core::{query::query, sql_str::AssertSqlSafe};
use std::time::Duration;

/// Bounded, credential-free outcome of the two independent maintenance phases.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationRetentionAttempt {
    generation: Option<GenerationRetentionReport>,
    generation_failure: Option<&'static str>,
    parse_cache: Option<NativeParseCacheRetentionReport>,
}

impl GenerationRetentionAttempt {
    /// Preserve both phase outcomes, including cache progress after a generation failure.
    #[must_use]
    pub fn new(
        generation: &Result<GenerationRetentionReport, GenerationRetentionError>,
        parse_cache: Option<NativeParseCacheRetentionReport>,
    ) -> Self {
        Self {
            generation: generation.as_ref().ok().copied(),
            generation_failure: generation.as_ref().err().map(super::drain::error_reason),
            parse_cache,
        }
    }

    fn failed(self) -> bool {
        self.generation_failure.is_some()
            || self.parse_cache.is_none()
            || self.generation.is_some_and(|report| {
                report.deferred_reason.is_some_and(|reason| {
                    !matches!(
                        reason,
                        "work_budget_reached"
                            | "search_relation_byte_budget"
                            | "search_relation_ddl_budget"
                    )
                })
            })
    }
}

impl CartographDatabase {
    /// Persist the last maintenance outcome in one bounded project row under its exact lease.
    /// Successful and failed automatic indexing both use this record; history never accumulates.
    /// # Errors
    /// Returns a credential-free error for an expired fence, invalid deadline or failed write.
    pub async fn record_generation_retention_attempt(
        &self,
        fence: &LeaseFence,
        attempt: GenerationRetentionAttempt,
        statement_timeout: Duration,
    ) -> Result<(), GenerationRetentionError> {
        validate_fence_shape(fence)?;
        let encoded = serde_json::to_string(&attempt)
            .map_err(|_| database_error("encode-maintenance-attempt"))?;
        if encoded.len() > 16_384 || statement_timeout.is_zero() {
            return Err(GenerationRetentionError::InvalidPolicy);
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("begin-maintenance-attempt"))?;
        crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
            .await
            .map_err(|()| GenerationRetentionError::InvalidPolicy)?;
        let context = RetentionContext {
            database: self,
            policy: GenerationRetentionPolicy::new(2, 1)?,
            fence,
            quoted_schema: crate::database::quoted_schema(&self.schema),
        };
        acquire_retention_locks(&mut transaction, &context).await?;
        require_live_fence(&mut transaction, &context).await?;
        let result = query(AssertSqlSafe(format!(r#"UPDATE {}."projects"
            SET retention_last_attempt_at = clock_timestamp(), retention_last_outcome = $2::jsonb,
                retention_consecutive_failures = CASE WHEN NOT $3 THEN 0
                    WHEN retention_consecutive_failures < 9223372036854775807 THEN retention_consecutive_failures + 1
                    ELSE retention_consecutive_failures END
            WHERE project_id = $1::uuid"#, context.quoted_schema)))
            .bind(fence.target().project_id().as_str()).bind(encoded).bind(attempt.failed())
            .execute(&mut *transaction).await.map_err(|_| database_error("record-maintenance-attempt"))?;
        if result.rows_affected() != 1 {
            return Err(GenerationRetentionError::ProjectNotFound);
        }
        require_live_fence(&mut transaction, &context).await?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("commit-maintenance-attempt"))
    }
}
