use std::collections::BTreeSet;

use cartograph_db::StorageUsageOffsets;

use super::{AssertSqlSafe, CartographDatabase, ProjectId, STATEMENT_TIMEOUT, query};

pub(super) async fn assert_statistics_and_inventory(
    database: &CartographDatabase,
    pool: &sqlx_postgres::PgPool,
    schema: &str,
    project: &ProjectId,
) {
    query(AssertSqlSafe(format!(
        r#"CREATE TABLE "{schema}".statistics_fixture (id integer PRIMARY KEY)"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("statistics fixture failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"INSERT INTO "{schema}".statistics_fixture VALUES (1)"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("statistics insert failed: {error}"));
    let mut connection = pool
        .acquire()
        .await
        .unwrap_or_else(|error| panic!("statistics connection failed: {error}"));
    query(AssertSqlSafe(format!(
        r#"VACUUM ANALYZE "{schema}".statistics_fixture"#
    )))
    .execute(&mut *connection)
    .await
    .unwrap_or_else(|error| panic!("statistics analysis failed: {error}"));
    query("SELECT pg_stat_force_next_flush()")
        .execute(&mut *connection)
        .await
        .unwrap_or_else(|error| panic!("statistics flush failed: {error}"));
    drop(connection);
    let before = database
        .storage_usage(project, 128, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("statistics usage failed: {error}"));
    assert!(before.statistics.track_counts);
    assert!(!before.statistics.observed_at.is_empty());
    let analyzed = before
        .tables
        .iter()
        .find(|table| table.relation == "statistics_fixture")
        .unwrap_or_else(|| panic!("statistics fixture absent from inventory"));
    assert_eq!(analyzed.estimated_live_rows, Some(1));
    assert_eq!(analyzed.estimated_dead_rows, Some(0));
    assert!(analyzed.last_vacuum.is_some());
    assert!(analyzed.last_analyze.is_some());
    query("SELECT pg_stat_reset_single_table_counters($1::regclass)")
        .bind(format!("{schema}.statistics_fixture"))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("statistics reset failed: {error}"));
    let reset = database
        .storage_usage(project, 128, STATEMENT_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("reset statistics usage failed: {error}"));
    let unobserved = reset
        .tables
        .iter()
        .find(|table| table.relation == "statistics_fixture")
        .unwrap_or_else(|| panic!("reset fixture absent from inventory"));
    assert_eq!(unobserved.estimated_live_rows, None);
    assert_eq!(unobserved.estimated_dead_rows, None);
    assert!(unobserved.total_bytes > 0);
    assert!(unobserved.last_vacuum.is_none());
    assert!(unobserved.last_analyze.is_none());
    assert_complete_inventory(database, project, &reset).await;
    query(AssertSqlSafe(format!(
        r#"DROP TABLE "{schema}".statistics_fixture"#
    )))
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("statistics fixture cleanup failed: {error}"));
}

async fn assert_complete_inventory(
    database: &CartographDatabase,
    project: &ProjectId,
    reset: &cartograph_db::StorageUsageReport,
) {
    let mut names = BTreeSet::new();
    let mut offset = 0;
    loop {
        let page = database
            .storage_usage_page(
                project,
                cartograph_db::StorageUsagePage {
                    limit: 32,
                    offsets: StorageUsageOffsets {
                        tables: 0,
                        indexes: offset,
                    },
                    statement_timeout: STATEMENT_TIMEOUT,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("inventory page failed: {error}"));
        assert_eq!(page.index_count, reset.index_count);
        for index in &page.indexes {
            assert!(names.insert(index.index.clone()));
        }
        if !page.indexes_truncated {
            break;
        }
        assert!(!page.indexes.is_empty());
        offset += u32::try_from(page.indexes.len())
            .unwrap_or_else(|_| panic!("inventory offset overflow"));
    }
    assert_eq!(u64::try_from(names.len()).ok(), Some(reset.index_count));
    let empty = database
        .storage_usage_page(
            project,
            cartograph_db::StorageUsagePage {
                limit: 32,
                offsets: StorageUsageOffsets {
                    tables: 100_000,
                    indexes: 100_000,
                },
                statement_timeout: STATEMENT_TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("empty inventory page failed: {error}"));
    assert_eq!(empty.index_count, reset.index_count);
    assert_eq!(empty.table_count, reset.table_count);
    assert!(empty.tables.is_empty());
    assert!(empty.indexes.is_empty());
}
