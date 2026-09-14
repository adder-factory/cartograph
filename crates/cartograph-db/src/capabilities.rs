use std::{collections::HashMap, time::Duration};

use serde::Serialize;
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::{PgConnection, PgPool};

use crate::DatabaseError;

const MINIMUM_POSTGRES_VERSION_NUM: i32 = 180_004;
const NEXT_POSTGRES_MAJOR_VERSION_NUM: i32 = 190_000;
pub(crate) const SUPPORTED_PG_SEARCH_VERSION: &str = "0.25.9";
pub(crate) const MANAGED_PGVECTOR_VERSION: &str = "0.8.4";
const MINIMUM_PGVECTOR_VERSION: [u32; 3] = [0, 8, 4];
const DEFAULT_CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const EXPECTED_SOURCE_CODE_TOKENS: [&str; 5] = ["cartograph", "search", "snake", "case", "42"];

/// Stable status for one database capability check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The hard v2 requirement is satisfied.
    Pass,
    /// The hard v2 requirement is not satisfied.
    Fail,
}

/// One actionable capability result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CapabilityCheck {
    /// Stable machine-readable identifier.
    pub id: &'static str,
    /// Pass/fail result.
    pub status: CheckStatus,
    /// Human-readable evidence.
    pub message: String,
    /// Concrete fix when the check fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<&'static str>,
}

/// Complete v2 database readiness report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CapabilityReport {
    /// True only when every hard requirement passes.
    pub ready: bool,
    /// Numeric PostgreSQL server version.
    pub postgres_version_num: i32,
    /// Display PostgreSQL server version.
    pub postgres_version: String,
    /// Installed `pg_search` extension version, when present.
    pub pg_search_version: Option<String>,
    /// Installed pgvector extension version, when present.
    pub pgvector_version: Option<String>,
    /// Ordered evidence for every hard requirement.
    pub checks: Vec<CapabilityCheck>,
}

#[derive(Debug)]
struct ProbeFacts {
    postgres_version_num: i32,
    postgres_version: String,
    extensions: HashMap<String, String>,
    preload_libraries: String,
    has_paradedb_access_method: bool,
    source_code_tokens: Result<Vec<String>, ()>,
}

struct CheckInput {
    id: &'static str,
    passed: bool,
    message: String,
    remediation: &'static str,
}

/// Probe PostgreSQL, `ParadeDB`, and pgvector without mutating the database.
/// # Errors
///
/// Returns an error if a required PostgreSQL version, extension, preload,
/// BM25, vector, or tokenizer capability cannot be probed successfully.
pub async fn probe_capabilities(pool: &PgPool) -> Result<CapabilityReport, DatabaseError> {
    probe_capabilities_bounded(pool, DEFAULT_CAPABILITY_PROBE_TIMEOUT).await
}

/// Probe required capabilities under an explicit PostgreSQL-side deadline.
/// # Errors
///
/// Returns an error if the deadline cannot be installed or any required
/// PostgreSQL, extension, BM25, vector, preload, or tokenizer check fails.
pub async fn probe_capabilities_bounded(
    pool: &PgPool,
    statement_timeout: Duration,
) -> Result<CapabilityReport, DatabaseError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|_| DatabaseError::CapabilityProbe {
            check: "connection",
        })?;
    if crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
        .await
        .is_err()
    {
        let _ = transaction.rollback().await;
        return Err(DatabaseError::CapabilityProbe {
            check: "statement-timeout",
        });
    }
    let report = probe_capabilities_connection(&mut transaction).await;
    match report {
        Ok(report) => {
            transaction
                .commit()
                .await
                .map_err(|_| DatabaseError::CapabilityProbe { check: "commit" })?;
            Ok(report)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

pub(crate) async fn probe_capabilities_connection(
    connection: &mut PgConnection,
) -> Result<CapabilityReport, DatabaseError> {
    let version_row = query("SELECT current_setting('server_version_num'), version()")
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| DatabaseError::CapabilityProbe {
            check: "postgres-version",
        })?;
    let postgres_version_num = version_row
        .try_get::<String, _>(0)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .ok_or(DatabaseError::CapabilityProbe {
            check: "postgres-version",
        })?;
    let postgres_version =
        version_row
            .try_get::<String, _>(1)
            .map_err(|_| DatabaseError::CapabilityProbe {
                check: "postgres-version",
            })?;

    let extension_rows = query(
        "SELECT extname, extversion FROM pg_extension WHERE extname IN ('pg_search', 'vector')",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|_| DatabaseError::CapabilityProbe {
        check: "extensions",
    })?;
    let mut extensions = HashMap::with_capacity(extension_rows.len());
    for row in extension_rows {
        let name = row
            .try_get::<String, _>(0)
            .map_err(|_| DatabaseError::CapabilityProbe {
                check: "extensions",
            })?;
        let version = row
            .try_get::<String, _>(1)
            .map_err(|_| DatabaseError::CapabilityProbe {
                check: "extensions",
            })?;
        extensions.insert(name, version);
    }

    let preload_libraries =
        query_scalar::<_, String>("SELECT current_setting('shared_preload_libraries')")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| DatabaseError::CapabilityProbe {
                check: "pg-search-preload",
            })?;
    let has_paradedb_access_method =
        query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'paradedb')")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| DatabaseError::CapabilityProbe {
                check: "bm25-access-method",
            })?;

    let source_code_tokens =
        if extensions.get("pg_search").map(String::as_str) == Some(SUPPORTED_PG_SEARCH_VERSION) {
            query_scalar::<_, Vec<String>>(
                "SELECT 'cartographSearch snake_case 42'::pdb.source_code::text[]",
            )
            .fetch_one(connection)
            .await
            .map_err(|_| ())
        } else {
            // Loading extension-defined types while the catalog and shared library
            // versions differ can terminate the PostgreSQL backend. Report the
            // version mismatch first and leave runtime probing to the supported
            // catalog version.
            Err(())
        };

    Ok(build_report(ProbeFacts {
        postgres_version_num,
        postgres_version,
        extensions,
        preload_libraries,
        has_paradedb_access_method,
        source_code_tokens,
    }))
}

fn build_report(facts: ProbeFacts) -> CapabilityReport {
    let pg_search_version = facts.extensions.get("pg_search").cloned();
    let pgvector_version = facts.extensions.get("vector").cloned();
    let pg_search_version_supported =
        pg_search_version.as_deref() == Some(SUPPORTED_PG_SEARCH_VERSION);
    let pgvector_version_supported = pgvector_version
        .as_deref()
        .is_some_and(pgvector_version_is_supported);
    let source_code_tokenizer_ready = facts.source_code_tokens.as_ref().is_ok_and(|tokens| {
        tokens
            .iter()
            .map(String::as_str)
            .eq(EXPECTED_SOURCE_CODE_TOKENS)
    });
    let pg_search_preloaded = facts
        .preload_libraries
        .split(',')
        .map(str::trim)
        .any(|library| library == "pg_search");

    let checks = vec![
        check(CheckInput {
            id: "postgres-18",
            passed: (MINIMUM_POSTGRES_VERSION_NUM..NEXT_POSTGRES_MAJOR_VERSION_NUM)
                .contains(&facts.postgres_version_num),
            message: format!(
                "PostgreSQL server reports version number {}",
                facts.postgres_version_num
            ),
            remediation: "Run Cartograph's pinned PostgreSQL 18.4 + ParadeDB image or upgrade the configured server to PostgreSQL 18.4 or newer.",
        }),
        check(CheckInput {
            id: "pg-search-extension",
            passed: pg_search_version_supported,
            message: extension_message("pg_search", pg_search_version.as_deref()),
            remediation: "Install the Cartograph-supported pg_search 0.25.9 build, add it to shared_preload_libraries, restart PostgreSQL, and create or update the extension to 0.25.9.",
        }),
        check(CheckInput {
            id: "pgvector-extension",
            passed: pgvector_version_supported,
            message: extension_message("vector", pgvector_version.as_deref()),
            remediation: "Install pgvector 0.8.4 or newer and create or update the vector extension in the Cartograph database; 0.8.6 is recommended for external PostgreSQL.",
        }),
        check(CheckInput {
            id: "pg-search-preload",
            passed: pg_search_preloaded,
            message: if pg_search_preloaded {
                "pg_search is present in shared_preload_libraries".to_owned()
            } else {
                "pg_search is absent from shared_preload_libraries".to_owned()
            },
            remediation: "Add pg_search to shared_preload_libraries and restart PostgreSQL.",
        }),
        check(CheckInput {
            id: "bm25-access-method",
            passed: facts.has_paradedb_access_method,
            message: if facts.has_paradedb_access_method {
                "ParadeDB index access method is registered".to_owned()
            } else {
                "ParadeDB index access method is unavailable".to_owned()
            },
            remediation: "Verify that pg_search 0.25.9 is installed, preloaded, and exposes the paradedb access method.",
        }),
        check(CheckInput {
            id: "source-code-tokenizer",
            passed: source_code_tokenizer_ready,
            message: if source_code_tokenizer_ready {
                "pdb.source_code splits camelCase and snake_case as required".to_owned()
            } else {
                "pdb.source_code is unavailable or returned an incompatible token stream".to_owned()
            },
            remediation: "Upgrade pg_search to the pinned Cartograph-supported version and recreate affected BM25 indexes.",
        }),
    ];
    let ready = checks.iter().all(|check| check.status == CheckStatus::Pass);

    CapabilityReport {
        ready,
        postgres_version_num: facts.postgres_version_num,
        postgres_version: facts.postgres_version,
        pg_search_version,
        pgvector_version,
        checks,
    }
}

fn check(input: CheckInput) -> CapabilityCheck {
    CapabilityCheck {
        id: input.id,
        status: if input.passed {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        message: input.message,
        remediation: (!input.passed).then_some(input.remediation),
    }
}

fn extension_message(name: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{name} extension {version} is installed"),
        None => format!("{name} extension is not installed"),
    }
}

fn pgvector_version_is_supported(version: &str) -> bool {
    parse_three_part_version(version).is_some_and(|parsed| parsed >= MINIMUM_PGVECTOR_VERSION)
}

fn parse_three_part_version(version: &str) -> Option<[u32; 3]> {
    let mut parts = version.split('.');
    let parsed = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_facts() -> ProbeFacts {
        ProbeFacts {
            postgres_version_num: 180_004,
            postgres_version: "PostgreSQL 18.4".to_owned(),
            extensions: HashMap::from([
                (
                    "pg_search".to_owned(),
                    SUPPORTED_PG_SEARCH_VERSION.to_owned(),
                ),
                ("vector".to_owned(), MANAGED_PGVECTOR_VERSION.to_owned()),
            ]),
            preload_libraries: "pg_search,pg_cron".to_owned(),
            has_paradedb_access_method: true,
            source_code_tokens: Ok(EXPECTED_SOURCE_CODE_TOKENS
                .iter()
                .map(ToString::to_string)
                .collect()),
        }
    }

    #[test]
    fn ready_requires_every_postgres_paradedb_and_vector_capability() {
        let report = build_report(ready_facts());

        assert!(report.ready);
        assert_eq!(report.checks.len(), 6);
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == CheckStatus::Pass)
        );
    }

    #[test]
    fn pinned_managed_image_capabilities_are_supported() {
        let mut facts = ready_facts();
        facts
            .extensions
            .insert("pg_search".to_owned(), "0.25.9".to_owned());
        facts
            .extensions
            .insert("vector".to_owned(), "0.8.4".to_owned());

        assert!(build_report(facts).ready);
    }

    #[test]
    fn missing_pg_search_does_not_hide_other_diagnostics() {
        let mut facts = ready_facts();
        facts.extensions.remove("pg_search");
        facts.preload_libraries = "pg_cron".to_owned();
        facts.has_paradedb_access_method = false;
        facts.source_code_tokens = Err(());

        let report = build_report(facts);
        let failed: Vec<_> = report
            .checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
            .map(|check| check.id)
            .collect();

        assert!(!report.ready);
        assert_eq!(
            failed,
            vec![
                "pg-search-extension",
                "pg-search-preload",
                "bm25-access-method",
                "source-code-tokenizer"
            ]
        );
        assert!(
            report
                .checks
                .iter()
                .filter_map(|check| check.remediation)
                .count()
                >= 4
        );
    }

    #[test]
    fn postgres_17_is_rejected_even_when_extensions_exist() {
        let mut facts = ready_facts();
        facts.postgres_version_num = 170_009;
        facts.postgres_version = "PostgreSQL 17.9".to_owned();

        let report = build_report(facts);

        assert!(!report.ready);
        assert_eq!(report.checks[0].id, "postgres-18");
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
    }

    #[test]
    fn vulnerable_postgres_18_minor_is_rejected() {
        let mut facts = ready_facts();
        facts.postgres_version_num = 180_003;
        facts.postgres_version = "PostgreSQL 18.3".to_owned();

        let report = build_report(facts);

        assert!(!report.ready);
        assert_eq!(report.checks[0].id, "postgres-18");
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
    }

    #[test]
    fn unvalidated_postgres_19_is_rejected() {
        let mut facts = ready_facts();
        facts.postgres_version_num = 190_000;
        facts.postgres_version = "PostgreSQL 19".to_owned();

        let report = build_report(facts);

        assert!(!report.ready);
        assert_eq!(report.checks[0].id, "postgres-18");
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
    }

    #[test]
    fn unsupported_pg_search_version_is_rejected() {
        for version in ["0.24.3", "0.25.6", "0.25.7", "0.25.8", "0.25.10"] {
            let mut facts = ready_facts();
            facts
                .extensions
                .insert("pg_search".to_owned(), version.to_owned());

            let report = build_report(facts);
            let extension_check = report
                .checks
                .iter()
                .find(|check| check.id == "pg-search-extension");

            assert!(!report.ready);
            assert!(matches!(extension_check, Some(check) if check.status == CheckStatus::Fail));
        }
    }

    #[test]
    fn pgvector_before_0_8_4_is_rejected() {
        let mut facts = ready_facts();
        facts
            .extensions
            .insert("vector".to_owned(), "0.8.3".to_owned());

        let report = build_report(facts);
        let extension_check = report
            .checks
            .iter()
            .find(|check| check.id == "pgvector-extension");

        assert!(matches!(extension_check, Some(check) if check.status == CheckStatus::Fail));
        assert!(pgvector_version_is_supported("0.8.4"));
        assert!(pgvector_version_is_supported("0.8.5"));
        assert!(pgvector_version_is_supported("0.8.6"));
        assert!(pgvector_version_is_supported("0.9.0"));
        assert!(!pgvector_version_is_supported("0.8.3"));
        assert!(!pgvector_version_is_supported("0.8.4.1"));
    }

    #[test]
    fn incompatible_source_code_tokenizer_is_rejected() {
        let mut facts = ready_facts();
        facts.source_code_tokens = Ok(vec!["cartographsearch".to_owned(), "snake_case".to_owned()]);

        let report = build_report(facts);
        let tokenizer_check = report
            .checks
            .iter()
            .find(|check| check.id == "source-code-tokenizer");

        assert!(matches!(tokenizer_check, Some(check) if check.status == CheckStatus::Fail));
    }
}
