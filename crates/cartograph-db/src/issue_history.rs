use std::{collections::BTreeSet, time::Duration};

use cartograph_domain::{GenerationId, ProjectId, SymbolId};
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;

use crate::{
    CartographDatabase,
    database::{quoted_schema, set_local_statement_timeout},
};

const ISSUE_HISTORY_TIMEOUT: Duration = Duration::from_mins(5);
const MAXIMUM_ATTRIBUTIONS: usize = 500_000;
const INSERT_CHUNK: usize = 5_000;
const MAXIMUM_ISSUES_PER_SYMBOL: u16 = 500;
const MAXIMUM_PEERS: u16 = 500;
const MAXIMUM_SYMBOLS_PER_TAGGED_COMMIT: i64 = 50;
const SHARED_COMMIT_ECHO_LIMIT: usize = 5;
const MAXIMUM_COMMIT_GROUPS: usize = 64;

/// How one issue-tagged commit affected a current symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueAttributionKind {
    /// The commit modified an existing symbol.
    Modified,
    /// The commit introduced the symbol.
    Added,
    /// The commit removed the historical symbol.
    Removed,
}

impl IssueAttributionKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "modified" => Some(Self::Modified),
            "added" => Some(Self::Added),
            "removed" => Some(Self::Removed),
            _ => None,
        }
    }
}

/// One bounded candidate produced by immutable Git-source comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolIssueAttribution {
    symbol_id: SymbolId,
    issue_number: u64,
    commit_sha: String,
    kind: IssueAttributionKind,
}

/// Validated inputs for one symbol attribution.
pub struct SymbolIssueAttributionInput {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Positive issue number parsed from the tagged commit.
    pub issue_number: u64,
    /// Commit sha for this record.
    pub commit_sha: String,
    /// Kind for this record.
    pub kind: IssueAttributionKind,
}

impl SymbolIssueAttribution {
    /// Creates a validated symbol issue attribution.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue number is zero/out of PostgreSQL range or
    /// `commit_sha` is not a valid bounded hexadecimal commit identity.
    pub fn new(input: SymbolIssueAttributionInput) -> Result<Self, IssueHistoryError> {
        if input.issue_number == 0
            || input.issue_number > 9_223_372_036_854_775_807_u64
            || !valid_commit_sha(&input.commit_sha)
        {
            return Err(IssueHistoryError::InvalidInput);
        }
        Ok(Self {
            symbol_id: input.symbol_id,
            issue_number: input.issue_number,
            commit_sha: input.commit_sha,
            kind: input.kind,
        })
    }
}

/// Atomic replacement request for one current generation's issue evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueHistoryRefreshMetadata {
    head_commit: String,
    commits_scanned: u64,
    tagged_commits: u64,
    oversized_commits_skipped: u64,
    comparison_failures_skipped: u64,
    truncated: bool,
}

/// Git-scan provenance for one issue-history refresh.
pub struct IssueHistoryRefreshMetadataInput {
    /// Head commit for this record.
    pub head_commit: String,
    /// Number of commits scanned.
    pub commits_scanned: u64,
    /// Number of tagged commits.
    pub tagged_commits: u64,
    /// Number of oversized commits skipped.
    pub oversized_commits_skipped: u64,
    /// Number of comparison failures skipped.
    pub comparison_failures_skipped: u64,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

impl IssueHistoryRefreshMetadata {
    /// Validates provenance and counters for one issue-history refresh.
    ///
    /// # Errors
    ///
    /// Returns an error if the head commit is invalid or tagged/skipped counters
    /// exceed the commit totals they describe.
    pub fn new(input: IssueHistoryRefreshMetadataInput) -> Result<Self, IssueHistoryError> {
        if !valid_commit_sha(&input.head_commit)
            || input.tagged_commits > input.commits_scanned
            || input.oversized_commits_skipped > input.tagged_commits
            || input.comparison_failures_skipped > input.tagged_commits
        {
            return Err(IssueHistoryError::InvalidInput);
        }
        Ok(Self {
            head_commit: input.head_commit,
            commits_scanned: input.commits_scanned,
            tagged_commits: input.tagged_commits,
            oversized_commits_skipped: input.oversized_commits_skipped,
            comparison_failures_skipped: input.comparison_failures_skipped,
            truncated: input.truncated,
        })
    }
}

/// Atomic replacement request for one current generation's issue evidence.
pub struct IssueHistoryRefreshRequest {
    project_id: ProjectId,
    generation_id: GenerationId,
    metadata: IssueHistoryRefreshMetadata,
    attributions: Vec<SymbolIssueAttribution>,
}

/// Bounded issue rows and provenance supplied by one Git scan.
pub struct IssueHistoryRefreshInput {
    /// Metadata for this record.
    pub metadata: IssueHistoryRefreshMetadata,
    /// Bounded attributions included in this result.
    pub attributions: Vec<SymbolIssueAttribution>,
}

impl IssueHistoryRefreshRequest {
    /// Validates and binds a refresh payload to one project generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the attribution payload exceeds the bounded atomic
    /// refresh maximum.
    pub fn new(
        project_id: ProjectId,
        generation_id: GenerationId,
        input: IssueHistoryRefreshInput,
    ) -> Result<Self, IssueHistoryError> {
        if input.attributions.len() > MAXIMUM_ATTRIBUTIONS {
            return Err(IssueHistoryError::InvalidInput);
        }
        Ok(Self {
            project_id,
            generation_id,
            metadata: input.metadata,
            attributions: input.attributions,
        })
    }
}

/// Durable issue-history replacement accounting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueHistoryRefreshReport {
    /// Stable generation ID for this record.
    pub generation_id: GenerationId,
    /// Head commit for this record.
    pub head_commit: String,
    /// Number of commits scanned.
    pub commits_scanned: u64,
    /// Number of tagged commits.
    pub tagged_commits: u64,
    /// Number of oversized commits skipped.
    pub oversized_commits_skipped: u64,
    /// Number of comparison failures skipped.
    pub comparison_failures_skipped: u64,
    /// Number of candidates.
    pub candidates: u64,
    /// Number of attributions written.
    pub attributions_written: u64,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

/// One current symbol/issue/commit attribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssueRecord {
    /// Positive issue number parsed from the tagged commit.
    pub issue_number: u64,
    /// Commit sha for this record.
    pub commit_sha: String,
    /// Kind for this record.
    pub kind: IssueAttributionKind,
}

/// A current symbol sharing issue-tagged commits with an anchor symbol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssuePeer {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Project-relative path for this record.
    pub path: String,
    /// Qualified name for this record.
    pub qualified_name: String,
    /// Issue-tagged commits shared with the anchor symbol.
    pub co_occurrences: u64,
    /// Bounded shared commits included in this result.
    pub shared_commits: Vec<String>,
}

/// One current symbol attributed to the same issue-tagged commit as an anchor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssueCommitPeer {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Project-relative path for this record.
    pub path: String,
    /// Qualified name for this record.
    pub qualified_name: String,
    /// Symbol kind for this record.
    pub symbol_kind: String,
}

/// Bounded current-generation peers for one exact commit SHA.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIssueCommitPeers {
    /// Commit sha for this record.
    pub commit_sha: String,
    /// Total matching peers before the per-commit page bound.
    pub total: u64,
    /// Bounded peers included in this result.
    pub peers: Vec<SymbolIssueCommitPeer>,
    /// Whether additional matching rows were omitted.
    pub truncated: bool,
}

/// Bounded issue evidence query for one current symbol.
pub struct SymbolIssueQuery<'query> {
    /// Stable project ID for this record.
    pub project_id: &'query ProjectId,
    /// Stable symbol ID for this record.
    pub symbol_id: &'query SymbolId,
    /// Maximum number of entries returned by this bounded request.
    pub limit: u16,
}

/// Bounded co-attribution query for one current symbol.
pub struct SymbolIssuePeerQuery<'query> {
    /// Stable project ID for this record.
    pub project_id: &'query ProjectId,
    /// Stable symbol ID for this record.
    pub symbol_id: &'query SymbolId,
    /// Minimum number of shared required by this request.
    pub minimum_shared: u16,
    /// Maximum number of entries returned by this bounded request.
    pub limit: u16,
}

/// Bounded current-symbol query for exact issue-tagged commits.
pub struct IssueCommitSymbolPeerQuery<'query> {
    /// Stable project ID for this record.
    pub project_id: &'query ProjectId,
    /// Stable excluded symbol ID for this record.
    pub excluded_symbol_id: &'query SymbolId,
    /// Commits for this record.
    pub commits: &'query [String],
    /// Maximum peers returned for each supplied commit.
    pub per_commit_limit: u16,
}

/// Safe issue-history persistence/query failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IssueHistoryError {
    #[error("issue-history input is invalid or exceeds its bound")]
    /// Supplied input violates a documented bound or invariant.
    InvalidInput,
    #[error("issue-history generation is no longer current")]
    /// The current generation changed before publication completed.
    CurrentGenerationChanged,
    #[error("Cartograph PostgreSQL issue-history operation failed during {operation}")]
    /// PostgreSQL could not complete the named operation.
    DatabaseOperation {
        /// Bounded operation label identifying the failed PostgreSQL phase.
        operation: &'static str,
    },
    #[error("Cartograph PostgreSQL issue-history row violates its durable contract")]
    /// A stored row violates its durable typed contract.
    CorruptStoredValue,
}

struct CurrentGenerationFence<'fence> {
    connection: &'fence mut sqlx_postgres::PgConnection,
    schema: &'fence str,
    project_id: &'fence ProjectId,
    generation_id: &'fence GenerationId,
}

impl CartographDatabase {
    /// Remove issue-derived evidence when the project explicitly disables the feature.
    /// # Errors
    ///
    /// Returns an error if the generation fence is not current or the bounded
    /// transactional deletion of disabled issue evidence cannot commit.
    pub async fn clear_issue_history(
        &self,
        project_id: &ProjectId,
        generation_id: &GenerationId,
    ) -> Result<(), IssueHistoryError> {
        let schema = quoted_schema(&self.schema);
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("clear-begin"))?;
        set_local_statement_timeout(&mut transaction, ISSUE_HISTORY_TIMEOUT)
            .await
            .map_err(|()| database_error("clear-timeout"))?;
        require_current_generation(CurrentGenerationFence {
            connection: &mut transaction,
            schema: &schema,
            project_id,
            generation_id,
        })
        .await?;
        for (relation, operation) in [
            ("symbol_issues", "clear-disabled-attributions"),
            ("issue_history_refreshes", "clear-disabled-refresh"),
        ] {
            let statement =
                format!(r#"DELETE FROM {schema}."{relation}" WHERE project_id = $1::uuid"#);
            query(AssertSqlSafe(statement))
                .bind(project_id.as_str())
                .execute(&mut *transaction)
                .await
                .map_err(|_| database_error(operation))?;
        }
        require_current_generation(CurrentGenerationFence {
            connection: &mut transaction,
            schema: &schema,
            project_id,
            generation_id,
        })
        .await?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("clear-commit"))
    }

    /// Atomically replace issue attributions only when the named generation remains current.
    /// # Errors
    ///
    /// Returns an error if the generation changes, the refresh rows violate
    /// database constraints, or atomic replacement/provenance commit fails.
    pub async fn replace_issue_history(
        &self,
        request: IssueHistoryRefreshRequest,
    ) -> Result<IssueHistoryRefreshReport, IssueHistoryError> {
        let schema = quoted_schema(&self.schema);
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| database_error("begin"))?;
        set_local_statement_timeout(&mut transaction, ISSUE_HISTORY_TIMEOUT)
            .await
            .map_err(|()| database_error("timeout"))?;
        require_current_generation(CurrentGenerationFence {
            connection: &mut transaction,
            schema: &schema,
            project_id: &request.project_id,
            generation_id: &request.generation_id,
        })
        .await?;
        let delete = format!(r#"DELETE FROM {schema}."symbol_issues" WHERE project_id = $1::uuid"#);
        query(AssertSqlSafe(delete))
            .bind(request.project_id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|_| database_error("clear"))?;
        let written = insert_issue_attributions(&mut transaction, &schema, &request).await?;
        record_issue_refresh(
            &mut transaction,
            IssueRefreshWrite {
                schema: &schema,
                request: &request,
                written,
            },
        )
        .await?;
        require_current_generation(CurrentGenerationFence {
            connection: &mut transaction,
            schema: &schema,
            project_id: &request.project_id,
            generation_id: &request.generation_id,
        })
        .await?;
        transaction
            .commit()
            .await
            .map_err(|_| database_error("commit"))?;
        Ok(IssueHistoryRefreshReport {
            generation_id: request.generation_id,
            head_commit: request.metadata.head_commit,
            commits_scanned: request.metadata.commits_scanned,
            tagged_commits: request.metadata.tagged_commits,
            oversized_commits_skipped: request.metadata.oversized_commits_skipped,
            comparison_failures_skipped: request.metadata.comparison_failures_skipped,
            candidates: u64::try_from(request.attributions.len()).unwrap_or(u64::MAX),
            attributions_written: written,
            truncated: request.metadata.truncated,
        })
    }

    /// Read bounded issue evidence for one current symbol.
    /// # Errors
    ///
    /// Returns an error if `limit` is zero/too large or current symbol issue
    /// rows cannot be queried or decoded.
    pub async fn current_symbol_issues(
        &self,
        request: SymbolIssueQuery<'_>,
    ) -> Result<Vec<SymbolIssueRecord>, IssueHistoryError> {
        if request.limit == 0 || request.limit > MAXIMUM_ISSUES_PER_SYMBOL {
            return Err(IssueHistoryError::InvalidInput);
        }
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT issues.issue_number, issues.commit_sha, issues.attribution_kind
               FROM {schema}."projects" AS projects
               JOIN {schema}."symbol_issues" AS issues
                 ON issues.project_id = projects.project_id
                AND issues.generation_id = projects.current_generation_id
               WHERE projects.project_id = $1::uuid
                 AND issues.symbol_id = $2::uuid
               ORDER BY issues.issue_number, issues.commit_sha, issues.attribution_kind
               LIMIT $3"#
        );
        let rows = query(AssertSqlSafe(statement))
            .bind(request.project_id.as_str())
            .bind(request.symbol_id.as_str())
            .bind(i64::from(request.limit))
            .fetch_all(&self.pool)
            .await
            .map_err(|_| database_error("read-symbol"))?;
        rows.iter().map(decode_issue).collect()
    }

    /// Rank current symbols by issue-tagged commits shared with one anchor.
    /// # Errors
    ///
    /// Returns an error if shared-commit/result bounds are invalid or ranked
    /// current symbol peers cannot be queried or decoded.
    pub async fn current_symbol_issue_peers(
        &self,
        request: SymbolIssuePeerQuery<'_>,
    ) -> Result<Vec<SymbolIssuePeer>, IssueHistoryError> {
        if request.minimum_shared == 0 || request.limit == 0 || request.limit > MAXIMUM_PEERS {
            return Err(IssueHistoryError::InvalidInput);
        }
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current_project AS (
                   SELECT current_generation_id AS generation_id
                   FROM {schema}."projects"
                   WHERE project_id = $1::uuid AND current_generation_id IS NOT NULL
               ), anchor AS (
                   SELECT DISTINCT issues.commit_sha
                   FROM {schema}."symbol_issues" AS issues
                   JOIN current_project
                     ON issues.generation_id = current_project.generation_id
                   WHERE issues.project_id = $1::uuid
                     AND issues.symbol_id = $2::uuid
                     AND issues.attribution_kind = 'modified'
               ), commit_symbol_counts AS (
                   SELECT issues.commit_sha, count(DISTINCT issues.symbol_id) AS symbols
                   FROM {schema}."symbol_issues" AS issues
                   JOIN current_project
                     ON issues.generation_id = current_project.generation_id
                   WHERE issues.project_id = $1::uuid
                     AND issues.attribution_kind = 'modified'
                   GROUP BY issues.commit_sha
               )
               SELECT symbols.symbol_id::text, files.normalized_path,
                      symbols.qualified_name,
                      count(DISTINCT peers.commit_sha)::bigint AS shared,
                      (array_agg(DISTINCT peers.commit_sha ORDER BY peers.commit_sha))
                          [1:{SHARED_COMMIT_ECHO_LIMIT}] AS commits
               FROM current_project
               JOIN {schema}."symbol_issues" AS peers
                 ON peers.project_id = $1::uuid
                AND peers.generation_id = current_project.generation_id
               JOIN anchor ON anchor.commit_sha = peers.commit_sha
               JOIN commit_symbol_counts AS counts
                 ON counts.commit_sha = peers.commit_sha
                AND counts.symbols <= {MAXIMUM_SYMBOLS_PER_TAGGED_COMMIT}
               JOIN {schema}."symbols" AS symbols
                 ON symbols.project_id = peers.project_id
                AND symbols.generation_id = peers.generation_id
                AND symbols.symbol_id = peers.symbol_id
               JOIN {schema}."files" AS files
                 ON files.project_id = symbols.project_id
                AND files.generation_id = symbols.generation_id
                AND files.file_id = symbols.file_id
               WHERE peers.symbol_id <> $2::uuid
                 AND peers.attribution_kind = 'modified'
               GROUP BY symbols.symbol_id, files.normalized_path, symbols.qualified_name
               HAVING count(DISTINCT peers.commit_sha) >= $3
               ORDER BY shared DESC, files.normalized_path, symbols.qualified_name,
                        symbols.symbol_id
               LIMIT $4"#
        );
        let rows = query(AssertSqlSafe(statement))
            .bind(request.project_id.as_str())
            .bind(request.symbol_id.as_str())
            .bind(i64::from(request.minimum_shared))
            .bind(i64::from(request.limit))
            .fetch_all(&self.pool)
            .await
            .map_err(|_| database_error("read-peers"))?;
        rows.iter().map(decode_peer).collect()
    }

    /// Count current-generation symbol/issue rows without reading their contents.
    /// # Errors
    ///
    /// Returns an error if PostgreSQL cannot count current attributions or the
    /// stored count is negative or otherwise not representable as `u64`.
    pub async fn current_issue_history_attribution_count(
        &self,
        project_id: &ProjectId,
    ) -> Result<u64, IssueHistoryError> {
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"SELECT count(*)::bigint
               FROM {schema}."projects" AS projects
               JOIN {schema}."symbol_issues" AS issues
                 ON issues.project_id = projects.project_id
                AND issues.generation_id = projects.current_generation_id
               WHERE projects.project_id = $1::uuid"#
        );
        let count = query(AssertSqlSafe(statement))
            .bind(project_id.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(|_| database_error("count-current"))?
            .try_get::<i64, _>(0)
            .ok()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(IssueHistoryError::CorruptStoredValue)?;
        Ok(count)
    }

    /// Fetch current modified-symbol peers for many exact issue-tagged commits in one query.
    /// # Errors
    ///
    /// Returns an error if commit identities/count bounds are invalid or the
    /// batched current modified-symbol peer rows cannot be queried or decoded.
    pub async fn current_issue_commit_symbol_peers(
        &self,
        request: IssueCommitSymbolPeerQuery<'_>,
    ) -> Result<Vec<SymbolIssueCommitPeers>, IssueHistoryError> {
        if !valid_issue_commit_peer_query(&request) {
            return Err(IssueHistoryError::InvalidInput);
        }
        let schema = quoted_schema(&self.schema);
        let statement = format!(
            r#"WITH current_project AS (
                   SELECT current_generation_id AS generation_id
                   FROM {schema}."projects"
                   WHERE project_id = $1::uuid AND current_generation_id IS NOT NULL
               ), requested AS (
                   SELECT commit_sha, ordinality
                   FROM unnest($2::text[]) WITH ORDINALITY AS input(commit_sha, ordinality)
               ), candidates AS (
                   SELECT DISTINCT requested.commit_sha, requested.ordinality,
                          symbols.symbol_id::text, files.normalized_path,
                          symbols.qualified_name, symbols.symbol_kind
                   FROM requested
                   JOIN {schema}."symbol_issues" AS issues
                     ON issues.project_id = $1::uuid
                    AND issues.commit_sha = requested.commit_sha
                    AND issues.attribution_kind = 'modified'
                   JOIN current_project
                     ON issues.generation_id = current_project.generation_id
                   JOIN {schema}."symbols" AS symbols
                     ON symbols.project_id = issues.project_id
                    AND symbols.generation_id = issues.generation_id
                    AND symbols.symbol_id = issues.symbol_id
                   JOIN {schema}."files" AS files
                     ON files.project_id = symbols.project_id
                    AND files.generation_id = symbols.generation_id
                    AND files.file_id = symbols.file_id
                   WHERE issues.symbol_id <> $3::uuid
               ), ranked AS (
                   SELECT candidates.*,
                          count(*) OVER (PARTITION BY commit_sha)::bigint AS total,
                          row_number() OVER (
                              PARTITION BY commit_sha
                              ORDER BY normalized_path, qualified_name, symbol_id
                          ) AS rank
                   FROM candidates
               )
               SELECT commit_sha, total, symbol_id, normalized_path,
                      qualified_name, symbol_kind
               FROM ranked
               WHERE rank <= $4
               ORDER BY ordinality, rank"#
        );
        let rows = query(AssertSqlSafe(statement))
            .bind(request.project_id.as_str())
            .bind(request.commits)
            .bind(request.excluded_symbol_id.as_str())
            .bind(i64::from(request.per_commit_limit))
            .fetch_all(&self.pool)
            .await
            .map_err(|_| database_error("read-commit-peers"))?;
        let mut groups = request
            .commits
            .iter()
            .cloned()
            .map(|commit_sha| SymbolIssueCommitPeers {
                commit_sha,
                total: 0,
                peers: Vec::new(),
                truncated: false,
            })
            .collect::<Vec<_>>();
        for row in rows {
            let commit_sha = row
                .try_get::<String, _>(0)
                .ok()
                .filter(|value| valid_commit_sha(value))
                .ok_or(IssueHistoryError::CorruptStoredValue)?;
            let total = row
                .try_get::<i64, _>(1)
                .ok()
                .and_then(|value| u64::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or(IssueHistoryError::CorruptStoredValue)?;
            let peer = decode_commit_peer(&row)?;
            let group = groups
                .iter_mut()
                .find(|group| group.commit_sha == commit_sha)
                .ok_or(IssueHistoryError::CorruptStoredValue)?;
            if group.total != 0 && group.total != total {
                return Err(IssueHistoryError::CorruptStoredValue);
            }
            group.total = total;
            group.peers.push(peer);
        }
        for group in &mut groups {
            group.truncated = group.total > u64::try_from(group.peers.len()).unwrap_or(u64::MAX);
        }
        Ok(groups)
    }
}

async fn insert_issue_attributions(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    schema: &str,
    request: &IssueHistoryRefreshRequest,
) -> Result<u64, IssueHistoryError> {
    let insert = format!(
        r#"INSERT INTO {schema}."symbol_issues" (
               project_id, generation_id, symbol_id, issue_number,
               commit_sha, attribution_kind
           )
           SELECT $1::uuid, $2::uuid, input.symbol_id::uuid, input.issue_number,
                  input.commit_sha, input.attribution_kind
           FROM UNNEST($3::text[], $4::bigint[], $5::text[], $6::text[])
                AS input(symbol_id, issue_number, commit_sha, attribution_kind)
           JOIN {schema}."symbols" AS symbols
             ON symbols.project_id = $1::uuid
            AND symbols.generation_id = $2::uuid
            AND symbols.symbol_id = input.symbol_id::uuid
           ON CONFLICT DO NOTHING"#
    );
    let mut written = 0_u64;
    for chunk in request.attributions.chunks(INSERT_CHUNK) {
        let symbol_ids = chunk
            .iter()
            .map(|attribution| attribution.symbol_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let issue_numbers = chunk
            .iter()
            .map(|attribution| i64::try_from(attribution.issue_number))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| IssueHistoryError::InvalidInput)?;
        let commits = chunk
            .iter()
            .map(|attribution| attribution.commit_sha.clone())
            .collect::<Vec<_>>();
        let kinds = chunk
            .iter()
            .map(|attribution| attribution.kind.as_str().to_owned())
            .collect::<Vec<_>>();
        written = written.saturating_add(
            query(AssertSqlSafe(insert.clone()))
                .bind(request.project_id.as_str())
                .bind(request.generation_id.as_str())
                .bind(symbol_ids)
                .bind(issue_numbers)
                .bind(commits)
                .bind(kinds)
                .execute(&mut **transaction)
                .await
                .map_err(|_| database_error("insert"))?
                .rows_affected(),
        );
    }
    Ok(written)
}

struct IssueRefreshWrite<'request> {
    schema: &'request str,
    request: &'request IssueHistoryRefreshRequest,
    written: u64,
}

async fn record_issue_refresh(
    transaction: &mut sqlx_postgres::PgTransaction<'_>,
    input: IssueRefreshWrite<'_>,
) -> Result<(), IssueHistoryError> {
    let refresh = format!(
        r#"INSERT INTO {}."issue_history_refreshes" (
               project_id, generation_id, head_commit, commits_scanned,
               tagged_commits, oversized_commits_skipped,
               comparison_failures_skipped, attributions_written, truncated
           ) VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9)
           ON CONFLICT (project_id) DO UPDATE SET
               generation_id = EXCLUDED.generation_id,
               head_commit = EXCLUDED.head_commit,
               commits_scanned = EXCLUDED.commits_scanned,
               tagged_commits = EXCLUDED.tagged_commits,
               oversized_commits_skipped = EXCLUDED.oversized_commits_skipped,
               comparison_failures_skipped = EXCLUDED.comparison_failures_skipped,
               attributions_written = EXCLUDED.attributions_written,
               truncated = EXCLUDED.truncated,
               refreshed_at = clock_timestamp()"#,
        input.schema
    );
    query(AssertSqlSafe(refresh))
        .bind(input.request.project_id.as_str())
        .bind(input.request.generation_id.as_str())
        .bind(&input.request.metadata.head_commit)
        .bind(to_i64(input.request.metadata.commits_scanned)?)
        .bind(to_i64(input.request.metadata.tagged_commits)?)
        .bind(to_i64(input.request.metadata.oversized_commits_skipped)?)
        .bind(to_i64(input.request.metadata.comparison_failures_skipped)?)
        .bind(to_i64(input.written)?)
        .bind(input.request.metadata.truncated)
        .execute(&mut **transaction)
        .await
        .map_err(|_| database_error("record-refresh"))?;
    Ok(())
}

fn valid_issue_commit_peer_query(request: &IssueCommitSymbolPeerQuery<'_>) -> bool {
    if request.commits.is_empty() || request.commits.len() > MAXIMUM_COMMIT_GROUPS {
        return false;
    }
    if request.per_commit_limit == 0 || request.per_commit_limit > MAXIMUM_PEERS {
        return false;
    }
    request
        .commits
        .iter()
        .all(|commit| valid_commit_sha(commit))
        && request.commits.iter().collect::<BTreeSet<_>>().len() == request.commits.len()
}

async fn require_current_generation(
    fence: CurrentGenerationFence<'_>,
) -> Result<(), IssueHistoryError> {
    let statement = format!(
        r#"SELECT current_generation_id::text
           FROM {}."projects"
           WHERE project_id = $1::uuid
           FOR UPDATE"#,
        fence.schema
    );
    let current = query(AssertSqlSafe(statement))
        .bind(fence.project_id.as_str())
        .fetch_optional(&mut *fence.connection)
        .await
        .map_err(|_| database_error("fence"))?
        .and_then(|row| row.try_get::<Option<String>, _>(0).ok().flatten());
    if current.as_deref() == Some(fence.generation_id.as_str()) {
        Ok(())
    } else {
        Err(IssueHistoryError::CurrentGenerationChanged)
    }
}

fn decode_issue(row: &sqlx_postgres::PgRow) -> Result<SymbolIssueRecord, IssueHistoryError> {
    let issue_number = row
        .try_get::<i64, _>(0)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let commit_sha = row
        .try_get::<String, _>(1)
        .ok()
        .filter(|value| valid_commit_sha(value))
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let kind = row
        .try_get::<String, _>(2)
        .ok()
        .and_then(|value| IssueAttributionKind::parse(&value))
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    Ok(SymbolIssueRecord {
        issue_number,
        commit_sha,
        kind,
    })
}

fn decode_peer(row: &sqlx_postgres::PgRow) -> Result<SymbolIssuePeer, IssueHistoryError> {
    let symbol_id = row
        .try_get::<String, _>(0)
        .ok()
        .and_then(|value| SymbolId::parse(&value).ok())
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let path = row
        .try_get::<String, _>(1)
        .map_err(|_| IssueHistoryError::CorruptStoredValue)?;
    let qualified_name = row
        .try_get::<String, _>(2)
        .map_err(|_| IssueHistoryError::CorruptStoredValue)?;
    let co_occurrences = row
        .try_get::<i64, _>(3)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let shared_commits = row
        .try_get::<Vec<String>, _>(4)
        .ok()
        .filter(|values| {
            !values.is_empty()
                && values.len() <= SHARED_COMMIT_ECHO_LIMIT
                && values.iter().all(|value| valid_commit_sha(value))
        })
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    Ok(SymbolIssuePeer {
        symbol_id,
        path,
        qualified_name,
        co_occurrences,
        shared_commits,
    })
}

const COMMIT_PEER_PATH_COLUMN: usize = 3;
const COMMIT_PEER_QUALIFIED_NAME_COLUMN: usize = 4;
const COMMIT_PEER_SYMBOL_KIND_COLUMN: usize = 5;

fn decode_commit_peer(
    row: &sqlx_postgres::PgRow,
) -> Result<SymbolIssueCommitPeer, IssueHistoryError> {
    let symbol_id = row
        .try_get::<String, _>(2)
        .ok()
        .and_then(|value| SymbolId::parse(&value).ok())
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let path = row
        .try_get::<String, _>(COMMIT_PEER_PATH_COLUMN)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let qualified_name = row
        .try_get::<String, _>(COMMIT_PEER_QUALIFIED_NAME_COLUMN)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    let symbol_kind = row
        .try_get::<String, _>(COMMIT_PEER_SYMBOL_KIND_COLUMN)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(IssueHistoryError::CorruptStoredValue)?;
    Ok(SymbolIssueCommitPeer {
        symbol_id,
        path,
        qualified_name,
        symbol_kind,
    })
}

fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn to_i64(value: u64) -> Result<i64, IssueHistoryError> {
    i64::try_from(value).map_err(|_| IssueHistoryError::InvalidInput)
}

const fn database_error(operation: &'static str) -> IssueHistoryError {
    IssueHistoryError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn attribution_validation_accepts_sha1_and_sha256_but_rejects_zero_issue() {
        let symbol = SymbolId::parse("11111111-1111-4111-8111-111111111111")
            .unwrap_or_else(|error| panic!("symbol failed: {error}"));
        assert!(
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id: symbol.clone(),
                issue_number: 42,
                commit_sha: "a".repeat(40),
                kind: IssueAttributionKind::Modified,
            })
            .is_ok()
        );
        assert!(
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id: symbol.clone(),
                issue_number: 42,
                commit_sha: "b".repeat(64),
                kind: IssueAttributionKind::Added,
            })
            .is_ok()
        );
        assert_eq!(
            SymbolIssueAttribution::new(SymbolIssueAttributionInput {
                symbol_id: symbol,
                issue_number: 0,
                commit_sha: "c".repeat(40),
                kind: IssueAttributionKind::Removed,
            }),
            Err(IssueHistoryError::InvalidInput)
        );
    }

    #[test]
    fn refresh_request_rejects_skip_counts_larger_than_tagged_history() {
        assert_matches!(
            IssueHistoryRefreshMetadata::new(IssueHistoryRefreshMetadataInput {
                head_commit: "a".repeat(40),
                commits_scanned: 1,
                tagged_commits: 1,
                oversized_commits_skipped: 2,
                comparison_failures_skipped: 0,
                truncated: false,
            }),
            Err(IssueHistoryError::InvalidInput)
        );
    }
}
