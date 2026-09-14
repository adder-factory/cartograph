use std::{collections::BTreeMap, time::Duration};

use cartograph_config::DatabaseSchema;
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use sqlx_postgres::PgConnection;
use thiserror::Error;

use crate::{CartographDatabase, CheckStatus, capabilities::probe_capabilities_connection};

const INITIAL_SCHEMA_VERSION: i64 = 1;
const OPERATION_LEASES_SCHEMA_VERSION: i64 = 2;
const COMPLETE_EDGE_KINDS_SCHEMA_VERSION: i64 = 3;
const REFERENCE_EVIDENCE_SCHEMA_VERSION: i64 = 4;
const DIGEST_VERSION_SCHEMA_VERSION: i64 = 5;
const BULK_RELATION_VALIDATION_SCHEMA_VERSION: i64 = 6;
const V1_IMPORT_RETENTION_SCHEMA_VERSION: i64 = 7;
const SEMANTIC_STORAGE_SCHEMA_VERSION: i64 = 8;
const REFERENCE_MULTIPLICITY_SCHEMA_VERSION: i64 = 9;
const EXACT_LOOKUP_INDEX_SCHEMA_VERSION: i64 = 10;
const GENERATION_SEARCH_RELATIONS_SCHEMA_VERSION: i64 = 11;
const TYPED_SYMBOL_SEMANTICS_SCHEMA_VERSION: i64 = 12;
const AGENT_EVIDENCE_SCHEMA_VERSION: i64 = 13;
const AGENT_SESSION_SCHEMA_VERSION: i64 = 14;
const STRUCTURAL_BRIDGE_SCHEMA_VERSION: i64 = 15;
const MATERIALIZED_SIMILARITY_SCHEMA_VERSION: i64 = 16;
const NATIVE_PARSE_CACHE_SCHEMA_VERSION: i64 = 17;
const SYMBOL_ISSUE_HISTORY_SCHEMA_VERSION: i64 = 18;
const SYMBOL_PAGERANK_SCHEMA_VERSION: i64 = 19;
const SUMMARY_PRIORITY_QUEUE_SCHEMA_VERSION: i64 = 20;
const DETERMINISTIC_COCHANGE_ORDER_SCHEMA_VERSION: i64 = 21;
const NATIVE_INDEX_DIGEST_V5_SCHEMA_VERSION: i64 = 22;
const STORAGE_LIFECYCLE_HARDENING_SCHEMA_VERSION: i64 = 23;
const RUST_WORKSPACE_RESOLUTION_DIGEST_V6_SCHEMA_VERSION: i64 = 24;
const DIRECTORY_IMPORT_SIMPLE_NAME_SCHEMA_VERSION: i64 = 25;
const NUMERICAL_EVIDENCE_DIGEST_V7_SCHEMA_VERSION: i64 = 26;
const STRUCTURAL_DIAGNOSTICS_DIGEST_V8_SCHEMA_VERSION: i64 = 27;
const BIOMARKER_PRECISION_DIGEST_V9_SCHEMA_VERSION: i64 = 28;
const DETECTOR_PRECISION_DIGEST_V10_SCHEMA_VERSION: i64 = 29;
const RUST_CLOSURE_CALL_TARGET_DIGEST_V11_SCHEMA_VERSION: i64 = 30;
const GO_CALL_TARGET_DIGEST_V12_SCHEMA_VERSION: i64 = 31;
const NATIVE_GENERATION_SPILL_SCHEMA_VERSION: i64 = 32;
const SPILL_CENTRALITY_LOOKUP_SCHEMA_VERSION: i64 = 33;
const SPILL_PARSE_CACHE_REFERENCE_SCHEMA_VERSION: i64 = 34;
const SEARCH_DOCUMENT_CANONICAL_METADATA_SCHEMA_VERSION: i64 = 35;
const JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_SCHEMA_VERSION: i64 = 36;
const STRUCTURAL_FINDING_CACHE_SCHEMA_VERSION: i64 = 37;
const SHADER_AND_DISPATCH_DIGEST_V14_SCHEMA_VERSION: i64 = 38;
const GENERATION_SOURCE_ADMISSION_SCHEMA_VERSION: i64 = 39;
const ADA_VHDL_NUMERICAL_DIGEST_V15_SCHEMA_VERSION: i64 = 40;
const RUST_SELF_RECEIVER_DIGEST_V16_SCHEMA_VERSION: i64 = 41;
const RESUMABLE_RETENTION_SCHEMA_VERSION: i64 = 42;
const GRAMMAR_REFRESH_DIGEST_V17_SCHEMA_VERSION: i64 = 43;
const LATEST_SCHEMA_VERSION: i64 = GRAMMAR_REFRESH_DIGEST_V17_SCHEMA_VERSION;
const MIGRATION_LOCK_NAMESPACE: &str = "cartograph-v2-schema-migration";

/// Latest append-only schema version understood by this native binary.
#[must_use]
pub const fn latest_schema_version() -> i64 {
    LATEST_SCHEMA_VERSION
}
const SEARCH_DOCUMENTS_BM25_INDEX_SQL_TEMPLATE: &str = r#"CREATE INDEX search_documents_bm25_idx
            ON {schema}."search_documents"
            USING bm25 (
                id,
                project_id,
                generation_id,
                document_id,
                file_id,
                symbol_id,
                path,
                language,
                document_kind,
                (qualified_name::pdb.source_code),
                (code::pdb.source_code),
                natural_text,
                metadata
            )
            WITH (key_field = 'id')"#;

struct Migration {
    version: i64,
    name: &'static str,
    statements: &'static [&'static str],
}

struct LedgerRecord {
    name: String,
    checksum: String,
}

struct ApplyMigrationInput<'a> {
    quoted_schema: &'a str,
    migration: &'a Migration,
    checksum: &'a str,
}

const INITIAL_SCHEMA: Migration = Migration {
    version: INITIAL_SCHEMA_VERSION,
    name: "generation_safe_core_and_bm25",
    statements: &[
        r#"CREATE TABLE {schema}."projects" (
            project_id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
            root_identity text NOT NULL UNIQUE CHECK (length(root_identity) BETWEEN 1 AND 4096),
            repository_fingerprint text NOT NULL CHECK (repository_fingerprint ~ '^[0-9a-f]{64}$'),
            current_generation_id uuid,
            next_generation_sequence bigint NOT NULL DEFAULT 1
                CHECK (next_generation_sequence > 0),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
        )"#,
        r#"CREATE TABLE {schema}."index_generations" (
            project_id uuid NOT NULL REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            generation_id uuid NOT NULL DEFAULT gen_random_uuid(),
            generation_sequence bigint NOT NULL CHECK (generation_sequence > 0),
            source_revision text NOT NULL CHECK (length(source_revision) BETWEEN 1 AND 1024),
            state text NOT NULL DEFAULT 'staging'
                CHECK (state IN ('staging', 'ready', 'current', 'superseded', 'failed')),
            worker_count smallint NOT NULL CHECK (worker_count BETWEEN 1 AND 256),
            content_digest text CHECK (content_digest ~ '^[0-9a-f]{64}$'),
            started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            ready_at timestamptz,
            published_at timestamptz,
            PRIMARY KEY (project_id, generation_id),
            UNIQUE (project_id, generation_sequence)
        )"#,
        r#"ALTER TABLE {schema}."projects"
            ADD CONSTRAINT projects_current_generation_fk
            FOREIGN KEY (project_id, current_generation_id)
            REFERENCES {schema}."index_generations"(project_id, generation_id)
            DEFERRABLE INITIALLY DEFERRED"#,
        r#"CREATE UNIQUE INDEX index_generations_one_current_idx
            ON {schema}."index_generations" (project_id)
            WHERE state = 'current'"#,
        r#"CREATE INDEX index_generations_state_idx
            ON {schema}."index_generations" (project_id, state, started_at DESC)"#,
        r#"CREATE TABLE {schema}."files" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            file_id uuid NOT NULL,
            normalized_path text NOT NULL CHECK (length(normalized_path) BETWEEN 1 AND 4096),
            language text NOT NULL CHECK (length(language) BETWEEN 1 AND 64),
            content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
            byte_size bigint NOT NULL CHECK (byte_size >= 0),
            parse_status text NOT NULL CHECK (parse_status IN ('parsed', 'partial', 'failed', 'skipped')),
            PRIMARY KEY (project_id, generation_id, file_id),
            UNIQUE (project_id, generation_id, normalized_path),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE TABLE {schema}."symbols" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            symbol_id uuid NOT NULL,
            file_id uuid NOT NULL,
            symbol_kind text NOT NULL CHECK (length(symbol_kind) BETWEEN 1 AND 64),
            qualified_name text NOT NULL CHECK (length(qualified_name) BETWEEN 1 AND 2048),
            signature text NOT NULL DEFAULT '',
            start_byte bigint NOT NULL CHECK (start_byte >= 0),
            end_byte bigint NOT NULL CHECK (end_byte >= start_byte),
            start_line integer NOT NULL CHECK (start_line >= 1),
            end_line integer NOT NULL CHECK (end_line >= start_line),
            structural_digest text NOT NULL CHECK (structural_digest ~ '^[0-9a-f]{64}$'),
            PRIMARY KEY (project_id, generation_id, symbol_id),
            FOREIGN KEY (project_id, generation_id, file_id)
                REFERENCES {schema}."files"(project_id, generation_id, file_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX symbols_file_span_idx
            ON {schema}."symbols" (project_id, generation_id, file_id, start_byte, end_byte)"#,
        r#"CREATE INDEX symbols_qualified_name_idx
            ON {schema}."symbols" (project_id, generation_id, qualified_name)"#,
        r#"CREATE TABLE {schema}."edges" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            source_symbol_id uuid NOT NULL,
            target_symbol_id uuid NOT NULL,
            edge_kind text NOT NULL
                CHECK (edge_kind IN ('calls', 'imports', 'references', 'implements', 'extends', 'tests', 'contains')),
            confidence real NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
            provenance text NOT NULL CHECK (length(provenance) BETWEEN 1 AND 256),
            PRIMARY KEY (
                project_id, generation_id, source_symbol_id, target_symbol_id, edge_kind, provenance
            ),
            FOREIGN KEY (project_id, generation_id, source_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, generation_id, target_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX edges_target_idx
            ON {schema}."edges" (project_id, generation_id, target_symbol_id, edge_kind)"#,
        r#"CREATE TABLE {schema}."references" (
            reference_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            file_id uuid NOT NULL,
            target_symbol_id uuid,
            reference_kind text NOT NULL CHECK (length(reference_kind) BETWEEN 1 AND 64),
            start_byte bigint NOT NULL CHECK (start_byte >= 0),
            end_byte bigint NOT NULL CHECK (end_byte >= start_byte),
            confidence real NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
            FOREIGN KEY (project_id, generation_id, file_id)
                REFERENCES {schema}."files"(project_id, generation_id, file_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, generation_id, target_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX references_file_span_idx
            ON {schema}."references" (project_id, generation_id, file_id, start_byte, end_byte)"#,
        r#"CREATE INDEX references_target_idx
            ON {schema}."references" (project_id, generation_id, target_symbol_id)
            WHERE target_symbol_id IS NOT NULL"#,
        r#"CREATE TABLE {schema}."search_documents" (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            document_id uuid NOT NULL,
            file_id uuid,
            symbol_id uuid,
            path text NOT NULL CHECK (length(path) BETWEEN 1 AND 4096),
            language text NOT NULL CHECK (length(language) BETWEEN 1 AND 64),
            document_kind text NOT NULL
                CHECK (document_kind IN ('symbol', 'file', 'documentation', 'test', 'configuration')),
            qualified_name text NOT NULL DEFAULT '',
            code text NOT NULL DEFAULT '',
            natural_text text NOT NULL DEFAULT '',
            metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
            UNIQUE (project_id, generation_id, document_id),
            CHECK (qualified_name <> '' OR code <> '' OR natural_text <> ''),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, generation_id, file_id)
                REFERENCES {schema}."files"(project_id, generation_id, file_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, generation_id, symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX search_documents_project_generation_idx
            ON {schema}."search_documents" (project_id, generation_id, id)"#,
        r#"CREATE INDEX search_documents_path_idx
            ON {schema}."search_documents" (project_id, generation_id, path)"#,
        SEARCH_DOCUMENTS_BM25_INDEX_SQL_TEMPLATE,
    ],
};

const OPERATION_LEASES_SCHEMA: Migration = Migration {
    version: OPERATION_LEASES_SCHEMA_VERSION,
    name: "observable_project_operation_leases",
    statements: &[
        r#"CREATE TABLE {schema}."project_operation_leases" (
            project_id uuid NOT NULL REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            operation text NOT NULL
                CHECK (operation IN ('index', 'sync', 'hook', 'migration', 'rebuild')),
            lease_id uuid NOT NULL DEFAULT gen_random_uuid() UNIQUE,
            owner_pid bigint NOT NULL CHECK (owner_pid BETWEEN 1 AND 4294967295),
            owner_process_start text NOT NULL
                CHECK (length(owner_process_start) BETWEEN 1 AND 256),
            generation_id uuid,
            acquired_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            heartbeat_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            expires_at timestamptz NOT NULL,
            PRIMARY KEY (project_id, operation),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE,
            CHECK (acquired_at <= heartbeat_at AND heartbeat_at < expires_at)
        )"#,
        r#"CREATE INDEX project_operation_leases_expiry_idx
            ON {schema}."project_operation_leases" (expires_at, project_id, operation)"#,
    ],
};

const COMPLETE_EDGE_KINDS_SCHEMA: Migration = Migration {
    version: COMPLETE_EDGE_KINDS_SCHEMA_VERSION,
    name: "complete_structural_edge_kinds",
    statements: &[r#"ALTER TABLE {schema}."edges"
            DROP CONSTRAINT "edges_edge_kind_check",
            ADD CONSTRAINT "edges_edge_kind_check"
            CHECK (edge_kind IN (
                'calls', 'imports', 'references', 'implements', 'extends', 'tests',
                'type_of', 'returns', 'instantiates', 'overrides', 'decorates',
                'field_access', 'def_use', 'exports', 'contains'
            ))"#],
};

const REFERENCE_EVIDENCE_SCHEMA: Migration = Migration {
    version: REFERENCE_EVIDENCE_SCHEMA_VERSION,
    name: "persist_unresolved_reference_evidence",
    statements: &[
        r#"ALTER TABLE {schema}."references"
            ADD COLUMN owner_symbol_id uuid,
            ADD COLUMN reference_name text NOT NULL DEFAULT '<legacy-unavailable>'
                CHECK (length(reference_name) BETWEEN 1 AND 4096),
            ADD COLUMN resolution_provenance text NOT NULL DEFAULT 'legacy-unavailable'
                CHECK (length(resolution_provenance) BETWEEN 1 AND 256),
            ADD CONSTRAINT references_owner_symbol_fk
                FOREIGN KEY (project_id, generation_id, owner_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE"#,
        r#"ALTER TABLE {schema}."references"
            ALTER COLUMN reference_name DROP DEFAULT,
            ALTER COLUMN resolution_provenance DROP DEFAULT"#,
        r#"CREATE INDEX references_owner_idx
            ON {schema}."references" (project_id, generation_id, owner_symbol_id)
            WHERE owner_symbol_id IS NOT NULL"#,
    ],
};

const DIGEST_VERSION_SCHEMA: Migration = Migration {
    version: DIGEST_VERSION_SCHEMA_VERSION,
    name: "version_logical_generation_digests",
    statements: &[
        r#"ALTER TABLE {schema}."index_generations"
            ADD COLUMN content_digest_version smallint"#,
        r#"UPDATE {schema}."index_generations"
            SET content_digest_version = 1
            WHERE content_digest IS NOT NULL"#,
        r#"ALTER TABLE {schema}."index_generations"
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2)),
            ADD CONSTRAINT index_generations_digest_pair_check
                CHECK ((content_digest IS NULL) = (content_digest_version IS NULL))"#,
    ],
};

const BULK_RELATION_VALIDATION_SCHEMA: Migration = Migration {
    version: BULK_RELATION_VALIDATION_SCHEMA_VERSION,
    name: "generation_scoped_bulk_relation_integrity",
    statements: &[
        r#"ALTER TABLE {schema}."edges"
            DROP CONSTRAINT "edges_project_id_generation_id_source_symbol_id_fkey",
            DROP CONSTRAINT "edges_project_id_generation_id_target_symbol_id_fkey",
            ADD CONSTRAINT edges_generation_fk
                FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE"#,
        r#"ALTER TABLE {schema}."references"
            DROP CONSTRAINT "references_project_id_generation_id_file_id_fkey",
            DROP CONSTRAINT "references_project_id_generation_id_target_symbol_id_fkey",
            DROP CONSTRAINT references_owner_symbol_fk,
            ADD CONSTRAINT references_generation_fk
                FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE"#,
        r#"ALTER TABLE {schema}."search_documents"
            DROP CONSTRAINT "search_documents_project_id_generation_id_file_id_fkey",
            DROP CONSTRAINT "search_documents_project_id_generation_id_symbol_id_fkey""#,
    ],
};

const V1_IMPORT_RETENTION_SCHEMA: Migration = Migration {
    version: V1_IMPORT_RETENTION_SCHEMA_VERSION,
    name: "v1_postgres_import_and_generation_retention",
    statements: &[
        r#"CREATE TABLE {schema}."v1_import_runs" (
            import_id uuid PRIMARY KEY,
            project_id uuid NOT NULL REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            generation_id uuid NOT NULL,
            source_schema text NOT NULL CHECK (length(source_schema) BETWEEN 1 AND 63),
            source_fingerprint text NOT NULL CHECK (source_fingerprint ~ '^[0-9a-f]{64}$'),
            content_digest text NOT NULL CHECK (content_digest ~ '^[0-9a-f]{64}$'),
            source_files bigint NOT NULL CHECK (source_files >= 0),
            source_symbols bigint NOT NULL CHECK (source_symbols >= 0),
            source_edges bigint NOT NULL CHECK (source_edges >= 0),
            source_references bigint NOT NULL CHECK (source_references >= 0),
            source_documents bigint NOT NULL CHECK (source_documents >= 0),
            checkpoint text NOT NULL
                CHECK (checkpoint IN ('staged', 'ready', 'bm25_rebuilt', 'complete')),
            started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            UNIQUE (project_id, source_schema)
        )"#,
        r#"CREATE TABLE {schema}."v1_import_checkpoints" (
            checkpoint_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            import_id uuid NOT NULL
                REFERENCES {schema}."v1_import_runs"(import_id) ON DELETE CASCADE,
            checkpoint text NOT NULL
                CHECK (checkpoint IN ('staged', 'ready', 'bm25_rebuilt', 'complete')),
            recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            UNIQUE (import_id, checkpoint)
        )"#,
        r#"CREATE INDEX index_generations_retention_idx
            ON {schema}."index_generations" (project_id, state, generation_sequence DESC)"#,
    ],
};

const SEMANTIC_STORAGE_SCHEMA: Migration = Migration {
    version: SEMANTIC_STORAGE_SCHEMA_VERSION,
    name: "model_scoped_pgvector_semantic_storage",
    statements: &[
        r#"CREATE TABLE {schema}."embedding_models" (
            model_id uuid PRIMARY KEY,
            fingerprint text NOT NULL UNIQUE
                CHECK (fingerprint ~ '^[0-9a-f]{64}$'),
            provider text NOT NULL CHECK (length(provider) BETWEEN 1 AND 128),
            model_name text NOT NULL CHECK (length(model_name) BETWEEN 1 AND 256),
            dimension integer NOT NULL CHECK (dimension BETWEEN 1 AND 2000),
            normalization text NOT NULL CHECK (normalization IN ('none', 'l2')),
            state text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'retired')),
            registered_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            retired_at timestamptz,
            CHECK (
                (state = 'active' AND retired_at IS NULL)
                OR (state = 'retired' AND retired_at IS NOT NULL)
            )
        )"#,
        r#"CREATE TABLE {schema}."document_embeddings" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            document_id uuid NOT NULL,
            model_id uuid NOT NULL
                REFERENCES {schema}."embedding_models"(model_id) ON DELETE RESTRICT,
            source_digest text NOT NULL CHECK (source_digest ~ '^[0-9a-f]{64}$'),
            embedding vector NOT NULL,
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id, document_id, model_id),
            FOREIGN KEY (project_id, generation_id, document_id)
                REFERENCES {schema}."search_documents"(project_id, generation_id, document_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX document_embeddings_generation_model_idx
            ON {schema}."document_embeddings" (
                project_id, generation_id, model_id, document_id
            )"#,
        r#"CREATE FUNCTION {schema}."validate_document_embedding"()
            RETURNS trigger
            LANGUAGE plpgsql
            SET search_path = pg_catalog, public
            AS $body$
            DECLARE
                expected_dimension integer;
                expected_normalization text;
                model_state text;
                magnitude double precision;
            BEGIN
                SELECT dimension, normalization, state
                INTO expected_dimension, expected_normalization, model_state
                FROM {schema}."embedding_models"
                WHERE model_id = NEW.model_id;
                IF NOT FOUND OR model_state <> 'active' THEN
                    RAISE EXCEPTION USING
                        ERRCODE = '23514',
                        MESSAGE = 'document embedding model is unavailable';
                END IF;
                IF vector_dims(NEW.embedding) <> expected_dimension THEN
                    RAISE EXCEPTION USING
                        ERRCODE = '23514',
                        MESSAGE = 'document embedding dimension mismatch';
                END IF;
                magnitude := vector_norm(NEW.embedding);
                IF magnitude IS NULL
                    OR magnitude <= 0.0
                    OR magnitude = 'Infinity'::float8
                    OR magnitude = 'NaN'::float8 THEN
                    RAISE EXCEPTION USING
                        ERRCODE = '23514',
                        MESSAGE = 'document embedding magnitude is invalid';
                END IF;
                IF expected_normalization = 'l2' AND abs(magnitude - 1.0) > 0.001 THEN
                    RAISE EXCEPTION USING
                        ERRCODE = '23514',
                        MESSAGE = 'document embedding normalization mismatch';
                END IF;
                RETURN NEW;
            END
            $body$"#,
        r#"CREATE TRIGGER document_embeddings_validate_trigger
            BEFORE INSERT OR UPDATE OF model_id, embedding
            ON {schema}."document_embeddings"
            FOR EACH ROW
            EXECUTE FUNCTION {schema}."validate_document_embedding"()"#,
    ],
};

const REFERENCE_MULTIPLICITY_SCHEMA: Migration = Migration {
    version: REFERENCE_MULTIPLICITY_SCHEMA_VERSION,
    name: "reference_site_multiplicity_and_digest_v3",
    statements: &[
        r#"ALTER TABLE {schema}."references"
            ADD COLUMN site_count bigint NOT NULL DEFAULT 1
                CHECK (site_count BETWEEN 1 AND 100000000),
            ADD COLUMN span_precision text NOT NULL DEFAULT 'exact'
                CHECK (span_precision IN ('exact', 'coarse_point', 'coarse_owner'))"#,
        r#"ALTER TABLE {schema}."edges"
            ADD COLUMN site_count bigint NOT NULL DEFAULT 1
                CHECK (site_count BETWEEN 1 AND 100000000)"#,
        r#"ALTER TABLE {schema}."v1_import_runs"
            ADD COLUMN source_edge_sites bigint,
            ADD COLUMN source_reference_sites bigint"#,
        r#"UPDATE {schema}."v1_import_runs"
            SET source_edge_sites = source_edges,
                source_reference_sites = source_references"#,
        r#"ALTER TABLE {schema}."v1_import_runs"
            ALTER COLUMN source_edge_sites SET NOT NULL,
            ALTER COLUMN source_reference_sites SET NOT NULL,
            ADD CONSTRAINT v1_import_runs_source_edge_sites_check
                CHECK (source_edge_sites >= 0),
            ADD CONSTRAINT v1_import_runs_source_reference_sites_check
                CHECK (source_reference_sites >= 0)"#,
        r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3))"#,
    ],
};

const EXACT_LOOKUP_INDEX_SCHEMA: Migration = Migration {
    version: EXACT_LOOKUP_INDEX_SCHEMA_VERSION,
    name: "indexed_exact_symbol_and_reference_names",
    statements: &[
        r#"ALTER TABLE {schema}."symbols"
            ADD COLUMN simple_name text GENERATED ALWAYS AS (
                reverse(split_part(reverse(replace(qualified_name, '::', '.')), '.', 1))
            ) STORED NOT NULL,
            ADD CONSTRAINT symbols_simple_name_check
                CHECK (length(simple_name) BETWEEN 1 AND 2048)"#,
        r#"CREATE INDEX symbols_simple_name_idx
            ON {schema}."symbols" (
                project_id, generation_id, simple_name, file_id, start_line, symbol_id
            )"#,
        r#"CREATE INDEX references_exact_name_site_idx
            ON {schema}."references" (
                project_id, generation_id, reference_name, file_id, start_byte, reference_id
            )"#,
    ],
};

const GENERATION_SEARCH_RELATIONS_SCHEMA: Migration = Migration {
    version: GENERATION_SEARCH_RELATIONS_SCHEMA_VERSION,
    name: "immutable_generation_scoped_bm25_relations",
    statements: &[
        r#"CREATE UNIQUE INDEX index_generations_global_identity_idx
            ON {schema}."index_generations" (generation_id)"#,
        r#"CREATE TABLE {schema}."generation_search_relations" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            document_count bigint NOT NULL CHECK (document_count >= 0),
            content_digest text NOT NULL CHECK (content_digest ~ '^[0-9a-f]{64}$'),
            relation_format_version smallint NOT NULL DEFAULT 1
                CHECK (relation_format_version = 1),
            built_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            verified_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
        r#"DROP INDEX IF EXISTS {schema}."search_documents_bm25_idx""#,
    ],
};

const TYPED_SYMBOL_SEMANTICS_SCHEMA: Migration = Migration {
    version: TYPED_SYMBOL_SEMANTICS_SCHEMA_VERSION,
    name: "typed_symbol_semantics",
    statements: &[
        r#"ALTER TABLE {schema}."symbols"
            ADD COLUMN visibility text,
            ADD COLUMN exported boolean NOT NULL DEFAULT false,
            ADD COLUMN default_export boolean NOT NULL DEFAULT false,
            ADD COLUMN async_symbol boolean NOT NULL DEFAULT false,
            ADD COLUMN static_member boolean NOT NULL DEFAULT false,
            ADD COLUMN declaration_only boolean NOT NULL DEFAULT false"#,
        r#"WITH semantics AS MATERIALIZED (
                SELECT DISTINCT ON (documents.project_id, documents.generation_id, documents.symbol_id)
                       documents.project_id, documents.generation_id, documents.symbol_id,
                       documents.metadata
                FROM {schema}."search_documents" AS documents
                WHERE documents.symbol_id IS NOT NULL
                ORDER BY documents.project_id, documents.generation_id, documents.symbol_id,
                         CASE WHEN documents.document_kind = 'symbol' THEN 0 ELSE 1 END,
                         documents.id
            )
            UPDATE {schema}."symbols" AS symbols
            SET visibility = CASE semantics.metadata ->> 'visibility'
                    WHEN 'public' THEN 'public'
                    WHEN 'private' THEN 'private'
                    WHEN 'protected' THEN 'protected'
                    WHEN 'internal' THEN 'internal'
                    ELSE NULL
                END,
                exported = COALESCE((semantics.metadata ->> 'exported') = 'true', false),
                default_export = COALESCE(
                    (semantics.metadata ->> 'default_export') = 'true', false
                ),
                async_symbol = COALESCE((semantics.metadata ->> 'async') = 'true', false),
                static_member = COALESCE((semantics.metadata ->> 'static') = 'true', false),
                declaration_only = COALESCE(
                    (semantics.metadata ->> 'declaration_only') = 'true', false
                )
            FROM semantics
            WHERE symbols.project_id = semantics.project_id
              AND symbols.generation_id = semantics.generation_id
              AND symbols.symbol_id = semantics.symbol_id"#,
        r#"ALTER TABLE {schema}."symbols"
            ADD CONSTRAINT symbols_visibility_check
                CHECK (visibility IS NULL OR visibility IN ('public', 'private', 'protected', 'internal'))"#,
        r#"CREATE INDEX symbols_exported_idx
            ON {schema}."symbols" (
                project_id, generation_id, exported, file_id, start_line, symbol_id
            ) WHERE exported"#,
        r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4))"#,
    ],
};

const AGENT_EVIDENCE_SCHEMA: Migration = Migration {
    version: AGENT_EVIDENCE_SCHEMA_VERSION,
    name: "durable_agent_coverage_history_and_artifacts",
    statements: &[
        r#"CREATE TABLE {schema}."coverage_sources" (
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            source_id uuid NOT NULL DEFAULT gen_random_uuid(),
            label text NOT NULL CHECK (length(label) BETWEEN 1 AND 256),
            report_format text NOT NULL CHECK (report_format IN ('lcov')),
            report_digest text NOT NULL CHECK (report_digest ~ '^[0-9a-f]{64}$'),
            report_metadata jsonb NOT NULL DEFAULT '{}'::jsonb
                CHECK (jsonb_typeof(report_metadata) = 'object'),
            loaded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, source_id),
            UNIQUE (project_id, label)
        )"#,
        r#"CREATE TABLE {schema}."symbol_coverage" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            source_id uuid NOT NULL,
            symbol_id uuid NOT NULL,
            lines_found bigint NOT NULL CHECK (lines_found >= 0),
            lines_hit bigint NOT NULL CHECK (lines_hit BETWEEN 0 AND lines_found),
            functions_found bigint NOT NULL DEFAULT 0 CHECK (functions_found >= 0),
            functions_hit bigint NOT NULL DEFAULT 0
                CHECK (functions_hit BETWEEN 0 AND functions_found),
            coverage_fraction double precision GENERATED ALWAYS AS (
                CASE WHEN lines_found = 0 THEN NULL
                     ELSE lines_hit::double precision / lines_found::double precision
                END
            ) STORED,
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id, source_id, symbol_id),
            FOREIGN KEY (project_id, generation_id, symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, source_id)
                REFERENCES {schema}."coverage_sources"(project_id, source_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX symbol_coverage_ranking_idx
            ON {schema}."symbol_coverage" (
                project_id, generation_id, coverage_fraction, symbol_id
            )"#,
        r#"CREATE TABLE {schema}."file_history" (
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            normalized_path text NOT NULL CHECK (length(normalized_path) BETWEEN 1 AND 4096),
            head_commit text NOT NULL CHECK (head_commit ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
            commit_count bigint NOT NULL CHECK (commit_count >= 0),
            author_count bigint NOT NULL CHECK (author_count >= 0),
            insertions bigint NOT NULL CHECK (insertions >= 0),
            deletions bigint NOT NULL CHECK (deletions >= 0),
            last_touched_at timestamptz,
            shallow_history boolean NOT NULL DEFAULT false,
            refreshed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, normalized_path)
        )"#,
        r#"CREATE INDEX file_history_hotspots_idx
            ON {schema}."file_history" (
                project_id, commit_count DESC, last_touched_at DESC, normalized_path
            )"#,
        r#"CREATE TABLE {schema}."file_cochanges" (
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            path_a text NOT NULL CHECK (length(path_a) BETWEEN 1 AND 4096),
            path_b text NOT NULL CHECK (length(path_b) BETWEEN 1 AND 4096),
            commit_count bigint NOT NULL CHECK (commit_count > 0),
            confidence real NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
            refreshed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, path_a, path_b),
            CHECK (path_a < path_b)
        )"#,
        r#"CREATE INDEX file_cochanges_reverse_idx
            ON {schema}."file_cochanges" (project_id, path_b, commit_count DESC, path_a)"#,
        r#"CREATE TABLE {schema}."agent_artifacts" (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            artifact_id uuid NOT NULL DEFAULT gen_random_uuid(),
            artifact_kind text NOT NULL
                CHECK (artifact_kind IN ('note', 'role', 'summary', 'session')),
            scope_kind text NOT NULL
                CHECK (scope_kind IN ('project', 'module', 'file', 'symbol', 'session')),
            scope_key text NOT NULL CHECK (length(scope_key) BETWEEN 1 AND 4096),
            body text NOT NULL CHECK (octet_length(body) <= 65536),
            metadata jsonb NOT NULL DEFAULT '{}'::jsonb
                CHECK (jsonb_typeof(metadata) = 'object' AND octet_length(metadata::text) <= 65536),
            generation_id uuid,
            source_digest text CHECK (source_digest ~ '^[0-9a-f]{64}$'),
            state text NOT NULL DEFAULT 'active'
                CHECK (state IN ('pending', 'active', 'complete', 'stale', 'archived')),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            UNIQUE (project_id, artifact_id),
            CHECK (body <> '' OR state = 'pending')
        )"#,
        r#"CREATE INDEX agent_artifacts_scope_idx
            ON {schema}."agent_artifacts" (
                project_id, artifact_kind, scope_kind, scope_key, updated_at DESC
            )"#,
        r#"CREATE UNIQUE INDEX agent_artifacts_current_unique_scope_idx
            ON {schema}."agent_artifacts" (
                project_id, artifact_kind, scope_kind, scope_key
            )
            WHERE artifact_kind IN ('role', 'summary') AND state <> 'archived'"#,
        r#"CREATE INDEX agent_artifacts_bm25_idx
            ON {schema}."agent_artifacts"
            USING bm25 (
                id,
                project_id,
                artifact_id,
                artifact_kind,
                scope_kind,
                scope_key,
                body,
                metadata
            ) WITH (key_field = 'id')"#,
    ],
};

const AGENT_SESSION_SCHEMA: Migration = Migration {
    version: AGENT_SESSION_SCHEMA_VERSION,
    name: "durable_agent_sessions_trace_and_macros",
    statements: &[
        r#"CREATE TABLE {schema}."mcp_sessions" (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            session_id uuid NOT NULL DEFAULT gen_random_uuid(),
            label text CHECK (label IS NULL OR length(label) BETWEEN 1 AND 256),
            objective text NOT NULL DEFAULT '' CHECK (octet_length(objective) <= 65536),
            session_kind text NOT NULL
                CHECK (session_kind IN ('automatic', 'named')),
            state text NOT NULL DEFAULT 'active'
                CHECK (state IN ('active', 'closed')),
            tool_count bigint NOT NULL DEFAULT 0 CHECK (tool_count >= 0),
            started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            last_activity_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            UNIQUE (project_id, session_id)
        )"#,
        r#"CREATE INDEX mcp_sessions_recent_idx
            ON {schema}."mcp_sessions" (
                project_id, last_activity_at DESC, id DESC
            )"#,
        r#"CREATE INDEX mcp_sessions_label_idx
            ON {schema}."mcp_sessions" (
                project_id, label, started_at DESC
            ) WHERE label IS NOT NULL"#,
        r#"CREATE TABLE {schema}."mcp_tool_calls" (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            project_id uuid NOT NULL,
            session_id uuid NOT NULL,
            step bigint NOT NULL CHECK (step > 0),
            called_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            tool_name text NOT NULL CHECK (length(tool_name) BETWEEN 1 AND 128),
            arguments jsonb NOT NULL DEFAULT '{}'::jsonb
                CHECK (jsonb_typeof(arguments) = 'object'
                       AND octet_length(arguments::text) <= 65536),
            result_summary text NOT NULL CHECK (octet_length(result_summary) <= 2048),
            result_kind text NOT NULL CHECK (result_kind IN ('success', 'error')),
            duration_ms bigint NOT NULL CHECK (duration_ms >= 0),
            UNIQUE (project_id, session_id, step),
            FOREIGN KEY (project_id, session_id)
                REFERENCES {schema}."mcp_sessions"(project_id, session_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX mcp_tool_calls_recent_idx
            ON {schema}."mcp_tool_calls" (
                project_id, called_at DESC, id DESC
            )"#,
        r#"CREATE TABLE {schema}."mcp_macros" (
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            name text NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
            steps jsonb NOT NULL
                CHECK (jsonb_typeof(steps) = 'array'
                       AND jsonb_array_length(steps) BETWEEN 1 AND 32
                       AND octet_length(steps::text) <= 262144),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            last_run_at timestamptz,
            run_count bigint NOT NULL DEFAULT 0 CHECK (run_count >= 0),
            PRIMARY KEY (project_id, name)
        )"#,
        r#"CREATE INDEX mcp_macros_recent_idx
            ON {schema}."mcp_macros" (project_id, updated_at DESC, name)"#,
    ],
};

const STRUCTURAL_BRIDGE_SCHEMA: Migration = Migration {
    version: STRUCTURAL_BRIDGE_SCHEMA_VERSION,
    name: "sampled_brandes_structural_bridges",
    statements: &[
        r#"ALTER TABLE {schema}."symbols"
            ADD COLUMN betweenness double precision
                CHECK (betweenness IS NULL OR betweenness BETWEEN 0.0 AND 1.0)"#,
        r#"CREATE INDEX symbols_betweenness_idx
            ON {schema}."symbols" (
                project_id, generation_id, betweenness DESC, symbol_id
            ) WHERE betweenness IS NOT NULL"#,
    ],
};

const MATERIALIZED_SIMILARITY_SCHEMA: Migration = Migration {
    version: MATERIALIZED_SIMILARITY_SCHEMA_VERSION,
    name: "materialized_model_scoped_symbol_similarity",
    statements: &[
        r#"CREATE UNIQUE INDEX search_documents_one_symbol_document_idx
            ON {schema}."search_documents" (project_id, generation_id, symbol_id)
            WHERE document_kind = 'symbol' AND symbol_id IS NOT NULL"#,
        r#"CREATE TABLE {schema}."symbol_similarity_edges" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            model_id uuid NOT NULL
                REFERENCES {schema}."embedding_models"(model_id) ON DELETE CASCADE,
            source_symbol_id uuid NOT NULL,
            target_symbol_id uuid NOT NULL,
            score double precision NOT NULL CHECK (score BETWEEN 0.0 AND 1.0),
            neighbor_rank smallint NOT NULL CHECK (neighbor_rank BETWEEN 1 AND 50),
            built_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (
                project_id, generation_id, model_id, source_symbol_id, target_symbol_id
            ),
            FOREIGN KEY (project_id, generation_id, source_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, generation_id, target_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX symbol_similarity_edges_source_rank_idx
            ON {schema}."symbol_similarity_edges" (
                project_id, generation_id, model_id, source_symbol_id,
                neighbor_rank, target_symbol_id
            )"#,
        r#"CREATE TABLE {schema}."symbol_similarity_builds" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            model_id uuid NOT NULL
                REFERENCES {schema}."embedding_models"(model_id) ON DELETE CASCADE,
            neighbors_per_symbol smallint NOT NULL
                CHECK (neighbors_per_symbol BETWEEN 1 AND 50),
            minimum_score double precision NOT NULL
                CHECK (minimum_score BETWEEN 0.0 AND 1.0),
            source_symbols bigint NOT NULL CHECK (source_symbols >= 0),
            edges_written bigint NOT NULL CHECK (edges_written >= 0),
            built_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id, model_id),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
    ],
};

const NATIVE_PARSE_CACHE_SCHEMA: Migration = Migration {
    version: NATIVE_PARSE_CACHE_SCHEMA_VERSION,
    name: "path_and_content_addressed_native_parse_cache",
    statements: &[
        r#"CREATE TABLE {schema}."native_parse_cache" (
            project_id uuid NOT NULL
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            extractor_contract_digest text NOT NULL
                CHECK (extractor_contract_digest ~ '^[0-9a-f]{64}$'),
            path_digest text NOT NULL CHECK (path_digest ~ '^[0-9a-f]{64}$'),
            normalized_path text NOT NULL
                CHECK (length(normalized_path) BETWEEN 1 AND 4096),
            language text NOT NULL CHECK (length(language) BETWEEN 1 AND 64),
            content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
            source_bytes bigint NOT NULL CHECK (source_bytes >= 0),
            payload bytea NOT NULL
                CHECK (octet_length(payload) BETWEEN 1 AND 67108864),
            payload_digest text NOT NULL CHECK (payload_digest ~ '^[0-9a-f]{64}$'),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            last_used_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (
                project_id, extractor_contract_digest, path_digest, language, content_hash
            )
        )"#,
        r#"CREATE INDEX native_parse_cache_last_used_idx
            ON {schema}."native_parse_cache" (project_id, last_used_at, path_digest)"#,
    ],
};

const SYMBOL_ISSUE_HISTORY_SCHEMA: Migration = Migration {
    version: SYMBOL_ISSUE_HISTORY_SCHEMA_VERSION,
    name: "generation_fenced_symbol_issue_history",
    statements: &[
        r#"CREATE TABLE {schema}."symbol_issues" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            symbol_id uuid NOT NULL,
            issue_number bigint NOT NULL CHECK (issue_number > 0),
            commit_sha text NOT NULL CHECK (commit_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
            attribution_kind text NOT NULL
                CHECK (attribution_kind IN ('modified', 'added', 'removed')),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (
                project_id, generation_id, symbol_id, issue_number, commit_sha, attribution_kind
            ),
            FOREIGN KEY (project_id, generation_id, symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX symbol_issues_issue_idx
            ON {schema}."symbol_issues" (
                project_id, generation_id, issue_number, symbol_id
            )"#,
        r#"CREATE TABLE {schema}."issue_history_refreshes" (
            project_id uuid PRIMARY KEY
                REFERENCES {schema}."projects"(project_id) ON DELETE CASCADE,
            generation_id uuid NOT NULL,
            head_commit text NOT NULL
                CHECK (head_commit ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
            commits_scanned bigint NOT NULL CHECK (commits_scanned >= 0),
            tagged_commits bigint NOT NULL CHECK (tagged_commits >= 0),
            oversized_commits_skipped bigint NOT NULL
                CHECK (oversized_commits_skipped >= 0),
            comparison_failures_skipped bigint NOT NULL
                CHECK (comparison_failures_skipped >= 0),
            attributions_written bigint NOT NULL CHECK (attributions_written >= 0),
            truncated boolean NOT NULL,
            refreshed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
    ],
};

const SYMBOL_PAGERANK_SCHEMA: Migration = Migration {
    version: SYMBOL_PAGERANK_SCHEMA_VERSION,
    name: "generation_scoped_symbol_pagerank",
    statements: &[
        r#"ALTER TABLE {schema}."symbols"
            ADD COLUMN pagerank double precision
                CHECK (pagerank IS NULL OR pagerank BETWEEN 0.0 AND 1.0)"#,
        r#"CREATE INDEX symbols_pagerank_idx
            ON {schema}."symbols" (
                project_id, generation_id, pagerank DESC, symbol_id
            ) WHERE pagerank IS NOT NULL"#,
    ],
};

const SUMMARY_PRIORITY_QUEUE_SCHEMA: Migration = Migration {
    version: SUMMARY_PRIORITY_QUEUE_SCHEMA_VERSION,
    name: "generation_scoped_summary_priority_queue",
    statements: &[
        r#"CREATE TABLE {schema}."summary_priority_queue" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            symbol_id uuid NOT NULL,
            enqueued_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            requested_count bigint NOT NULL DEFAULT 1
                CHECK (requested_count BETWEEN 1 AND 1000000000),
            attempts smallint NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 3),
            PRIMARY KEY (project_id, generation_id, symbol_id),
            FOREIGN KEY (project_id, generation_id, symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX summary_priority_queue_drain_idx
            ON {schema}."summary_priority_queue" (
                project_id, generation_id, enqueued_at DESC,
                requested_count DESC, symbol_id
            )"#,
    ],
};

const DETERMINISTIC_COCHANGE_ORDER_SCHEMA: Migration = Migration {
    version: DETERMINISTIC_COCHANGE_ORDER_SCHEMA_VERSION,
    name: "deterministic_file_cochange_path_order",
    statements: &[
        r#"ALTER TABLE {schema}."file_cochanges"
            DROP CONSTRAINT file_cochanges_check"#,
        r#"ALTER TABLE {schema}."file_cochanges"
            ADD CONSTRAINT file_cochanges_check
            CHECK ((path_a COLLATE "C") < (path_b COLLATE "C"))"#,
    ],
};

const NATIVE_INDEX_DIGEST_V5_SCHEMA: Migration = Migration {
    version: NATIVE_INDEX_DIGEST_V5_SCHEMA_VERSION,
    name: "native_framework_resolver_and_test_ownership_digest_v5",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5))"#],
};

const STORAGE_LIFECYCLE_HARDENING_SCHEMA: Migration = Migration {
    version: STORAGE_LIFECYCLE_HARDENING_SCHEMA_VERSION,
    name: "bounded_cache_and_high_churn_autovacuum",
    statements: &[
        r#"ALTER TABLE {schema}."native_parse_cache"
            ADD COLUMN payload_bytes bigint
                GENERATED ALWAYS AS (octet_length(payload)::bigint) VIRTUAL"#,
        r#"CREATE INDEX native_parse_cache_contract_recency_idx
            ON {schema}."native_parse_cache" (
                project_id, extractor_contract_digest, last_used_at DESC, path_digest
            )"#,
        r#"ALTER TABLE {schema}."index_generations" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 100,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 100
            )"#,
        r#"ALTER TABLE {schema}."files" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 500
            )"#,
        r#"ALTER TABLE {schema}."symbols" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 500
            )"#,
        r#"ALTER TABLE {schema}."edges" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 500
            )"#,
        r#"ALTER TABLE {schema}."references" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 500
            )"#,
        r#"ALTER TABLE {schema}."search_documents" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 500
            )"#,
        r#"ALTER TABLE {schema}."native_parse_cache" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 100,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 100
            )"#,
    ],
};

const RUST_WORKSPACE_RESOLUTION_DIGEST_V6_SCHEMA: Migration = Migration {
    version: RUST_WORKSPACE_RESOLUTION_DIGEST_V6_SCHEMA_VERSION,
    name: "rust_workspace_crate_resolution_digest_v6",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6))"#],
};

const DIRECTORY_IMPORT_SIMPLE_NAME_SCHEMA: Migration = Migration {
    version: DIRECTORY_IMPORT_SIMPLE_NAME_SCHEMA_VERSION,
    name: "directory_import_simple_name_fallback",
    statements: &[r#"ALTER TABLE {schema}."symbols"
            ALTER COLUMN simple_name SET EXPRESSION AS (
                COALESCE(
                    NULLIF(
                        reverse(split_part(reverse(replace(qualified_name, '::', '.')), '.', 1)),
                        ''
                    ),
                    qualified_name
                )
            )"#],
};

const NUMERICAL_EVIDENCE_DIGEST_V7_SCHEMA: Migration = Migration {
    version: NUMERICAL_EVIDENCE_DIGEST_V7_SCHEMA_VERSION,
    name: "generation_scoped_static_numerical_evidence_v7",
    statements: &[
        r#"CREATE TABLE {schema}."numerical_sites" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            numerical_site_id uuid NOT NULL,
            file_id uuid NOT NULL,
            owner_symbol_id uuid,
            start_byte bigint NOT NULL CHECK (start_byte >= 0),
            end_byte bigint NOT NULL CHECK (end_byte >= start_byte),
            start_line integer NOT NULL CHECK (start_line >= 1),
            end_line integer NOT NULL CHECK (end_line >= start_line),
            operation text NOT NULL CHECK (operation ~ '^[a-z0-9_]{1,64}$'),
            hazard text NOT NULL CHECK (hazard ~ '^[a-z0-9_]{1,64}$'),
            precision text NOT NULL CHECK (precision ~ '^[a-z0-9_]{1,64}$'),
            expression_digest text NOT NULL CHECK (expression_digest ~ '^[0-9a-f]{64}$'),
            confidence_ppm integer NOT NULL CHECK (confidence_ppm BETWEEN 0 AND 1000000),
            provenance text NOT NULL CHECK (
                length(provenance) BETWEEN 1 AND 256
                AND provenance ~ '^[a-z0-9_]+$'
            ),
            evidence_level text NOT NULL
                CHECK (evidence_level IN ('proven', 'heuristic', 'coverage_gap')),
            unknowns text NOT NULL DEFAULT '' CHECK (length(unknowns) <= 256),
            PRIMARY KEY (project_id, generation_id, numerical_site_id),
            FOREIGN KEY (project_id, generation_id, file_id)
                REFERENCES {schema}."files"(project_id, generation_id, file_id)
                ON DELETE CASCADE,
            FOREIGN KEY (project_id, generation_id, owner_symbol_id)
                REFERENCES {schema}."symbols"(project_id, generation_id, symbol_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX numerical_sites_hazard_rank_idx
            ON {schema}."numerical_sites" (
                project_id, generation_id, hazard, confidence_ppm DESC,
                file_id, start_line, numerical_site_id
            )"#,
        r#"CREATE INDEX numerical_sites_owner_span_idx
            ON {schema}."numerical_sites" (
                project_id, generation_id, owner_symbol_id,
                start_line, end_line, numerical_site_id
            ) WHERE owner_symbol_id IS NOT NULL"#,
        r#"ALTER TABLE {schema}."numerical_sites" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 500
            )"#,
        r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7))"#,
    ],
};

const STRUCTURAL_DIAGNOSTICS_DIGEST_V8_SCHEMA: Migration = Migration {
    version: STRUCTURAL_DIAGNOSTICS_DIGEST_V8_SCHEMA_VERSION,
    name: "context_classified_structural_diagnostics_digest_v8",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8))"#],
};

const BIOMARKER_PRECISION_DIGEST_V9_SCHEMA: Migration = Migration {
    version: BIOMARKER_PRECISION_DIGEST_V9_SCHEMA_VERSION,
    name: "biomarker_precision_digest_v9",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9))"#],
};

const DETECTOR_PRECISION_DIGEST_V10_SCHEMA: Migration = Migration {
    version: DETECTOR_PRECISION_DIGEST_V10_SCHEMA_VERSION,
    name: "detector_precision_digest_v10",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10))"#],
};

const RUST_CLOSURE_CALL_TARGET_DIGEST_V11_SCHEMA: Migration = Migration {
    version: RUST_CLOSURE_CALL_TARGET_DIGEST_V11_SCHEMA_VERSION,
    name: "rust_closure_call_target_digest_v11",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11))"#],
};

const GO_CALL_TARGET_DIGEST_V12_SCHEMA: Migration = Migration {
    version: GO_CALL_TARGET_DIGEST_V12_SCHEMA_VERSION,
    name: "go_call_target_digest_v12",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12))"#],
};

const NATIVE_GENERATION_SPILL_SCHEMA: Migration = Migration {
    version: NATIVE_GENERATION_SPILL_SCHEMA_VERSION,
    name: "generation_fenced_native_spill",
    statements: &[
        r#"CREATE TABLE {schema}."native_generation_spills" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            generation_sequence bigint NOT NULL CHECK (generation_sequence > 0),
            phase text NOT NULL DEFAULT 'parsing'
                CHECK (phase IN (
                    'parsing', 'resolving', 'sealed', 'canonicalizing', 'canonicalized'
                )),
            maximum_bytes bigint NOT NULL CHECK (maximum_bytes > 0),
            maximum_rows bigint NOT NULL CHECK (maximum_rows > 0),
            logical_bytes bigint NOT NULL DEFAULT 0
                CHECK (logical_bytes BETWEEN 0 AND maximum_bytes),
            raw_rows bigint NOT NULL DEFAULT 0
                CHECK (raw_rows BETWEEN 0 AND maximum_rows),
            extracted_files bigint NOT NULL DEFAULT 0 CHECK (extracted_files >= 0),
            canonical_relation text CHECK (canonical_relation IN (
                'files', 'symbols', 'edges', 'references', 'numerical_sites', 'documents'
            )),
            canonical_partition integer NOT NULL DEFAULT 0
                CHECK (canonical_partition BETWEEN 0 AND 64),
            canonical_rows bigint NOT NULL DEFAULT 0 CHECK (canonical_rows >= 0),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_batches" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text NOT NULL CHECK (relation IN (
                'extracted_files', 'files', 'symbols', 'edges', 'references',
                'numerical_sites', 'documents'
            )),
            batch_sequence bigint NOT NULL CHECK (batch_sequence >= 0),
            row_count integer NOT NULL CHECK (row_count BETWEEN 1 AND 100000),
            logical_bytes bigint NOT NULL CHECK (logical_bytes > 0),
            batch_digest text NOT NULL CHECK (batch_digest ~ '^[0-9a-f]{64}$'),
            created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id, relation, batch_sequence),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."native_generation_spills"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_rows" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text NOT NULL,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            sort_key bytea NOT NULL CHECK (octet_length(sort_key) BETWEEN 1 AND 131072),
            payload bytea NOT NULL CHECK (octet_length(payload) BETWEEN 1 AND 268435456),
            payload_digest text NOT NULL CHECK (payload_digest ~ '^[0-9a-f]{64}$'),
            logical_bytes bigint GENERATED ALWAYS AS (
                octet_length(sort_key)::bigint + octet_length(payload)::bigint
            ) STORED,
            PRIMARY KEY (
                project_id, generation_id, relation, batch_sequence, row_ordinal
            ),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_rows_order_idx
            ON {schema}."native_generation_spill_rows" (
                project_id, generation_id, relation, sort_key,
                batch_sequence, row_ordinal
            )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_files" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text GENERATED ALWAYS AS ('files') STORED,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            bucket smallint NOT NULL CHECK (bucket BETWEEN 0 AND 4095),
            file_id uuid NOT NULL,
            normalized_path text NOT NULL CHECK (length(normalized_path) BETWEEN 1 AND 4096),
            language text NOT NULL CHECK (length(language) BETWEEN 1 AND 64),
            content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
            byte_size bigint NOT NULL CHECK (byte_size >= 0),
            parse_status text NOT NULL
                CHECK (parse_status IN ('parsed', 'partial', 'failed', 'skipped')),
            PRIMARY KEY (project_id, generation_id, batch_sequence, row_ordinal),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_files_reduce_idx
            ON {schema}."native_generation_spill_files" (
                project_id, generation_id, bucket, file_id,
                batch_sequence, row_ordinal
            )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_symbols" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text GENERATED ALWAYS AS ('symbols') STORED,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            bucket smallint NOT NULL CHECK (bucket BETWEEN 0 AND 4095),
            symbol_id uuid NOT NULL,
            file_id uuid NOT NULL,
            symbol_kind text NOT NULL CHECK (length(symbol_kind) BETWEEN 1 AND 64),
            qualified_name text NOT NULL CHECK (length(qualified_name) BETWEEN 1 AND 2048),
            signature text NOT NULL CHECK (length(signature) <= 65536),
            start_byte bigint NOT NULL CHECK (start_byte >= 0),
            end_byte bigint NOT NULL CHECK (end_byte >= start_byte),
            start_line integer NOT NULL CHECK (start_line >= 1),
            end_line integer NOT NULL CHECK (end_line >= start_line),
            structural_digest text NOT NULL CHECK (structural_digest ~ '^[0-9a-f]{64}$'),
            visibility text CHECK (
                visibility IS NULL OR visibility IN ('public', 'private', 'protected', 'internal')
            ),
            exported boolean NOT NULL,
            default_export boolean NOT NULL,
            async_symbol boolean NOT NULL,
            static_member boolean NOT NULL,
            declaration_only boolean NOT NULL,
            betweenness double precision CHECK (
                betweenness IS NULL OR betweenness BETWEEN 0.0 AND 1.0
            ),
            pagerank double precision CHECK (pagerank IS NULL OR pagerank BETWEEN 0.0 AND 1.0),
            PRIMARY KEY (project_id, generation_id, batch_sequence, row_ordinal),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_symbols_reduce_idx
            ON {schema}."native_generation_spill_symbols" (
                project_id, generation_id, bucket, symbol_id,
                batch_sequence, row_ordinal
            )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_edges" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text GENERATED ALWAYS AS ('edges') STORED,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            bucket smallint NOT NULL CHECK (bucket BETWEEN 0 AND 4095),
            source_symbol_id uuid NOT NULL,
            target_symbol_id uuid NOT NULL,
            edge_kind text NOT NULL CHECK (edge_kind IN (
                'calls', 'imports', 'references', 'implements', 'extends', 'tests',
                'type_of', 'returns', 'instantiates', 'overrides', 'decorates',
                'field_access', 'def_use', 'exports', 'contains'
            )),
            confidence real NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
            provenance text NOT NULL CHECK (length(provenance) BETWEEN 1 AND 256),
            site_count bigint NOT NULL CHECK (site_count BETWEEN 1 AND 100000000),
            PRIMARY KEY (project_id, generation_id, batch_sequence, row_ordinal),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_edges_reduce_idx
            ON {schema}."native_generation_spill_edges" (
                project_id, generation_id, bucket, source_symbol_id,
                target_symbol_id, edge_kind, provenance, batch_sequence, row_ordinal
            )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_references" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text GENERATED ALWAYS AS ('references') STORED,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            bucket smallint NOT NULL CHECK (bucket BETWEEN 0 AND 4095),
            file_id uuid NOT NULL,
            owner_symbol_id uuid,
            target_symbol_id uuid,
            reference_name text NOT NULL CHECK (length(reference_name) BETWEEN 1 AND 4096),
            reference_kind text NOT NULL CHECK (length(reference_kind) BETWEEN 1 AND 64),
            start_byte bigint NOT NULL CHECK (start_byte >= 0),
            end_byte bigint NOT NULL CHECK (end_byte >= start_byte),
            confidence real NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
            resolution_provenance text NOT NULL
                CHECK (length(resolution_provenance) BETWEEN 1 AND 256),
            site_count bigint NOT NULL CHECK (site_count BETWEEN 1 AND 100000000),
            span_precision text NOT NULL
                CHECK (span_precision IN ('exact', 'coarse_point', 'coarse_owner')),
            PRIMARY KEY (project_id, generation_id, batch_sequence, row_ordinal),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_references_reduce_idx
            ON {schema}."native_generation_spill_references" (
                project_id, generation_id, bucket, file_id, owner_symbol_id,
                target_symbol_id, reference_name, reference_kind, start_byte,
                end_byte, resolution_provenance, batch_sequence, row_ordinal
            )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_numerical_sites" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text GENERATED ALWAYS AS ('numerical_sites') STORED,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            bucket smallint NOT NULL CHECK (bucket BETWEEN 0 AND 4095),
            numerical_site_id uuid NOT NULL,
            file_id uuid NOT NULL,
            owner_symbol_id uuid,
            start_byte bigint NOT NULL CHECK (start_byte >= 0),
            end_byte bigint NOT NULL CHECK (end_byte >= start_byte),
            start_line integer NOT NULL CHECK (start_line >= 1),
            end_line integer NOT NULL CHECK (end_line >= start_line),
            operation text NOT NULL CHECK (operation ~ '^[a-z0-9_]{1,64}$'),
            hazard text NOT NULL CHECK (hazard ~ '^[a-z0-9_]{1,64}$'),
            precision text NOT NULL CHECK (precision ~ '^[a-z0-9_]{1,64}$'),
            expression_digest text NOT NULL CHECK (expression_digest ~ '^[0-9a-f]{64}$'),
            confidence_ppm integer NOT NULL CHECK (confidence_ppm BETWEEN 0 AND 1000000),
            provenance text NOT NULL CHECK (
                length(provenance) BETWEEN 1 AND 256 AND provenance ~ '^[a-z0-9_]+$'
            ),
            evidence_level text NOT NULL
                CHECK (evidence_level IN ('proven', 'heuristic', 'coverage_gap')),
            unknowns text NOT NULL CHECK (length(unknowns) <= 256),
            PRIMARY KEY (project_id, generation_id, batch_sequence, row_ordinal),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_numerical_reduce_idx
            ON {schema}."native_generation_spill_numerical_sites" (
                project_id, generation_id, bucket, numerical_site_id,
                batch_sequence, row_ordinal
            )"#,
        r#"CREATE TABLE {schema}."native_generation_spill_documents" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            relation text GENERATED ALWAYS AS ('documents') STORED,
            batch_sequence bigint NOT NULL,
            row_ordinal integer NOT NULL CHECK (row_ordinal BETWEEN 0 AND 99999),
            bucket smallint NOT NULL CHECK (bucket BETWEEN 0 AND 4095),
            document_id uuid NOT NULL,
            file_id uuid,
            symbol_id uuid,
            path text NOT NULL CHECK (length(path) BETWEEN 1 AND 4096),
            language text NOT NULL CHECK (length(language) BETWEEN 1 AND 64),
            document_kind text NOT NULL CHECK (
                document_kind IN ('symbol', 'file', 'documentation', 'test', 'configuration')
            ),
            qualified_name text NOT NULL,
            code text NOT NULL,
            natural_text text NOT NULL,
            metadata jsonb NOT NULL,
            metadata_json text NOT NULL CHECK (octet_length(metadata_json) <= 65536),
            CHECK (qualified_name <> '' OR code <> '' OR natural_text <> ''),
            PRIMARY KEY (project_id, generation_id, batch_sequence, row_ordinal),
            FOREIGN KEY (project_id, generation_id, relation, batch_sequence)
                REFERENCES {schema}."native_generation_spill_batches"(
                    project_id, generation_id, relation, batch_sequence
                ) ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX native_generation_spill_documents_reduce_idx
            ON {schema}."native_generation_spill_documents" (
                project_id, generation_id, bucket, document_id,
                batch_sequence, row_ordinal
            )"#,
        r#"ALTER TABLE {schema}."native_generation_spill_rows" SET (
                autovacuum_vacuum_scale_factor = 0.01,
                autovacuum_vacuum_threshold = 1000,
                autovacuum_analyze_scale_factor = 0.02,
                autovacuum_analyze_threshold = 1000
            )"#,
    ],
};

const SPILL_CENTRALITY_LOOKUP_SCHEMA: Migration = Migration {
    version: SPILL_CENTRALITY_LOOKUP_SCHEMA_VERSION,
    name: "spill_centrality_identity_lookup",
    statements: &[r#"CREATE INDEX native_generation_spill_symbols_identity_idx
            ON {schema}."native_generation_spill_symbols" (
                project_id, generation_id, symbol_id
            )"#],
};

const SPILL_PARSE_CACHE_REFERENCE_SCHEMA: Migration = Migration {
    version: SPILL_PARSE_CACHE_REFERENCE_SCHEMA_VERSION,
    name: "spill_parse_cache_payload_references",
    statements: &[
        r#"ALTER TABLE {schema}."native_generation_spill_rows"
            ADD COLUMN cache_extractor_contract_digest text,
            ADD COLUMN cache_path_digest text,
            ADD COLUMN cache_language text,
            ADD COLUMN cache_content_hash text,
            ADD COLUMN cache_payload_bytes bigint"#,
        r#"ALTER TABLE {schema}."native_generation_spill_rows"
            ALTER COLUMN payload DROP NOT NULL"#,
        r#"ALTER TABLE {schema}."native_generation_spill_rows"
            DROP COLUMN logical_bytes"#,
        r#"ALTER TABLE {schema}."native_generation_spill_rows"
            ADD COLUMN logical_bytes bigint GENERATED ALWAYS AS (
                octet_length(sort_key)::bigint
                + COALESCE(octet_length(payload)::bigint, cache_payload_bytes)
            ) STORED"#,
        r#"ALTER TABLE {schema}."native_generation_spill_rows"
            ADD CONSTRAINT native_generation_spill_rows_payload_source_check CHECK (
                (payload IS NOT NULL
                    AND cache_extractor_contract_digest IS NULL
                    AND cache_path_digest IS NULL
                    AND cache_language IS NULL
                    AND cache_content_hash IS NULL
                    AND cache_payload_bytes IS NULL)
                OR
                (payload IS NULL
                    AND cache_extractor_contract_digest IS NOT NULL
                    AND cache_path_digest IS NOT NULL
                    AND cache_language IS NOT NULL
                    AND cache_content_hash IS NOT NULL
                    AND cache_payload_bytes BETWEEN 1 AND 268435456)
            )"#,
        r#"ALTER TABLE {schema}."native_generation_spill_rows"
            ADD CONSTRAINT native_generation_spill_rows_cache_fk FOREIGN KEY (
                project_id, cache_extractor_contract_digest, cache_path_digest,
                cache_language, cache_content_hash
            ) REFERENCES {schema}."native_parse_cache" (
                project_id, extractor_contract_digest, path_digest, language, content_hash
            )"#,
        r#"CREATE INDEX native_generation_spill_rows_cache_idx
            ON {schema}."native_generation_spill_rows" (
                project_id, cache_extractor_contract_digest, cache_path_digest,
                cache_language, cache_content_hash
            ) WHERE payload IS NULL"#,
    ],
};

const SEARCH_DOCUMENT_CANONICAL_METADATA_SCHEMA: Migration = Migration {
    version: SEARCH_DOCUMENT_CANONICAL_METADATA_SCHEMA_VERSION,
    name: "search_document_canonical_metadata_text",
    statements: &[r#"ALTER TABLE {schema}."search_documents"
            ADD COLUMN metadata_json text,
            ADD CONSTRAINT search_documents_metadata_json_check CHECK (
                metadata_json IS NULL
                OR (
                    octet_length(metadata_json) BETWEEN 2 AND 1048576
                    AND CAST(metadata_json AS jsonb) = metadata
                )
            )"#],
};

const JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_SCHEMA: Migration = Migration {
    version: JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_SCHEMA_VERSION,
    name: "javascript_construction_target_digest_v13",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13))"#],
};

const STRUCTURAL_FINDING_CACHE_SCHEMA: Migration = Migration {
    version: STRUCTURAL_FINDING_CACHE_SCHEMA_VERSION,
    name: "structural_finding_generation_cache",
    statements: &[
        r#"CREATE TABLE {schema}."structural_finding_runs" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            inputs_digest text NOT NULL CHECK (inputs_digest ~ '^[0-9a-f]{64}$'),
            analyzed_symbols bigint NOT NULL CHECK (analyzed_symbols >= 0),
            computed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            PRIMARY KEY (project_id, generation_id),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."index_generations"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE TABLE {schema}."structural_findings" (
            project_id uuid NOT NULL,
            generation_id uuid NOT NULL,
            symbol_id uuid NOT NULL,
            finding text NOT NULL CHECK (length(finding) BETWEEN 1 AND 128),
            path text NOT NULL CHECK (length(path) BETWEEN 1 AND 4096),
            qualified_name text NOT NULL CHECK (length(qualified_name) BETWEEN 1 AND 4096),
            severity text NOT NULL CHECK (severity IN ('info', 'warning', 'error')),
            start_line integer NOT NULL CHECK (start_line >= 0),
            end_line integer NOT NULL CHECK (end_line >= start_line),
            metric_name text NOT NULL CHECK (length(metric_name) BETWEEN 1 AND 128),
            metric double precision NOT NULL,
            degree_centrality double precision NOT NULL
                CHECK (degree_centrality BETWEEN 0.0 AND 1.0),
            outgoing_edges bigint NOT NULL CHECK (outgoing_edges >= 0),
            unresolved_references bigint NOT NULL CHECK (unresolved_references >= 0),
            detail jsonb,
            PRIMARY KEY (project_id, generation_id, symbol_id, finding),
            FOREIGN KEY (project_id, generation_id)
                REFERENCES {schema}."structural_finding_runs"(project_id, generation_id)
                ON DELETE CASCADE
        )"#,
        r#"CREATE INDEX structural_findings_ranked_idx
            ON {schema}."structural_findings" (
                project_id, generation_id, severity, degree_centrality DESC, metric DESC
            )"#,
        r#"CREATE INDEX structural_findings_detector_idx
            ON {schema}."structural_findings" (
                project_id, generation_id, finding
            )"#,
    ],
};

const SHADER_AND_DISPATCH_DIGEST_V14_SCHEMA: Migration = Migration {
    version: SHADER_AND_DISPATCH_DIGEST_V14_SCHEMA_VERSION,
    name: "shader_and_dispatch_digest_v14",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14))"#],
};

const GENERATION_SOURCE_ADMISSION_SCHEMA: Migration = Migration {
    version: GENERATION_SOURCE_ADMISSION_SCHEMA_VERSION,
    name: "generation_source_admission_policy",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            ADD COLUMN run_excludes text[] NOT NULL DEFAULT ARRAY[]::text[],
            ADD CONSTRAINT index_generations_run_excludes_check CHECK (
                cardinality(run_excludes) <= 4096
                AND array_position(run_excludes, NULL) IS NULL
            )"#],
};

const ADA_VHDL_NUMERICAL_DIGEST_V15_SCHEMA: Migration = Migration {
    version: ADA_VHDL_NUMERICAL_DIGEST_V15_SCHEMA_VERSION,
    name: "ada_vhdl_numerical_digest_v15",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15))"#],
};

const RUST_SELF_RECEIVER_DIGEST_V16_SCHEMA: Migration = Migration {
    version: RUST_SELF_RECEIVER_DIGEST_V16_SCHEMA_VERSION,
    name: "rust_self_receiver_digest_v16",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16))"#],
};

const RESUMABLE_RETENTION_SCHEMA: Migration = Migration {
    version: RESUMABLE_RETENTION_SCHEMA_VERSION,
    name: "resumable_generation_retention",
    statements: &[
        r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_state_check,
            ADD CONSTRAINT index_generations_state_check
                CHECK (state IN ('staging', 'ready', 'current', 'superseded', 'failed', 'retiring')),
            ADD COLUMN retention_original_state text
                CHECK (retention_original_state IN ('staging', 'ready', 'superseded', 'failed')),
            ADD COLUMN retention_started_at timestamptz,
            ADD CONSTRAINT index_generations_retention_state_check CHECK (
                (state = 'retiring' AND retention_original_state IS NOT NULL AND retention_started_at IS NOT NULL)
                OR (state <> 'retiring' AND retention_original_state IS NULL AND retention_started_at IS NULL)
            )"#,
        r#"ALTER TABLE {schema}."projects"
            ADD COLUMN retention_last_attempt_at timestamptz,
            ADD COLUMN retention_last_outcome jsonb CHECK (octet_length(retention_last_outcome::text) <= 16384),
            ADD COLUMN retention_consecutive_failures bigint NOT NULL DEFAULT 0 CHECK (retention_consecutive_failures >= 0)"#,
    ],
};

const GRAMMAR_REFRESH_DIGEST_V17_SCHEMA: Migration = Migration {
    version: GRAMMAR_REFRESH_DIGEST_V17_SCHEMA_VERSION,
    name: "grammar_refresh_digest_v17",
    statements: &[r#"ALTER TABLE {schema}."index_generations"
            DROP CONSTRAINT index_generations_digest_version_check,
            ADD CONSTRAINT index_generations_digest_version_check
                CHECK (content_digest_version IS NULL OR content_digest_version IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17))"#],
};

const MIGRATIONS: [&Migration; 43] = [
    &INITIAL_SCHEMA,
    &OPERATION_LEASES_SCHEMA,
    &COMPLETE_EDGE_KINDS_SCHEMA,
    &REFERENCE_EVIDENCE_SCHEMA,
    &DIGEST_VERSION_SCHEMA,
    &BULK_RELATION_VALIDATION_SCHEMA,
    &V1_IMPORT_RETENTION_SCHEMA,
    &SEMANTIC_STORAGE_SCHEMA,
    &REFERENCE_MULTIPLICITY_SCHEMA,
    &EXACT_LOOKUP_INDEX_SCHEMA,
    &GENERATION_SEARCH_RELATIONS_SCHEMA,
    &TYPED_SYMBOL_SEMANTICS_SCHEMA,
    &AGENT_EVIDENCE_SCHEMA,
    &AGENT_SESSION_SCHEMA,
    &STRUCTURAL_BRIDGE_SCHEMA,
    &MATERIALIZED_SIMILARITY_SCHEMA,
    &NATIVE_PARSE_CACHE_SCHEMA,
    &SYMBOL_ISSUE_HISTORY_SCHEMA,
    &SYMBOL_PAGERANK_SCHEMA,
    &SUMMARY_PRIORITY_QUEUE_SCHEMA,
    &DETERMINISTIC_COCHANGE_ORDER_SCHEMA,
    &NATIVE_INDEX_DIGEST_V5_SCHEMA,
    &STORAGE_LIFECYCLE_HARDENING_SCHEMA,
    &RUST_WORKSPACE_RESOLUTION_DIGEST_V6_SCHEMA,
    &DIRECTORY_IMPORT_SIMPLE_NAME_SCHEMA,
    &NUMERICAL_EVIDENCE_DIGEST_V7_SCHEMA,
    &STRUCTURAL_DIAGNOSTICS_DIGEST_V8_SCHEMA,
    &BIOMARKER_PRECISION_DIGEST_V9_SCHEMA,
    &DETECTOR_PRECISION_DIGEST_V10_SCHEMA,
    &RUST_CLOSURE_CALL_TARGET_DIGEST_V11_SCHEMA,
    &GO_CALL_TARGET_DIGEST_V12_SCHEMA,
    &NATIVE_GENERATION_SPILL_SCHEMA,
    &SPILL_CENTRALITY_LOOKUP_SCHEMA,
    &SPILL_PARSE_CACHE_REFERENCE_SCHEMA,
    &SEARCH_DOCUMENT_CANONICAL_METADATA_SCHEMA,
    &JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_SCHEMA,
    &STRUCTURAL_FINDING_CACHE_SCHEMA,
    &SHADER_AND_DISPATCH_DIGEST_V14_SCHEMA,
    &GENERATION_SOURCE_ADMISSION_SCHEMA,
    &ADA_VHDL_NUMERICAL_DIGEST_V15_SCHEMA,
    &RUST_SELF_RECEIVER_DIGEST_V16_SCHEMA,
    &RESUMABLE_RETENTION_SCHEMA,
    &GRAMMAR_REFRESH_DIGEST_V17_SCHEMA,
];

#[cfg(test)]
pub(crate) fn expected_migration_versions() -> Vec<i64> {
    MIGRATIONS
        .iter()
        .map(|migration| migration.version)
        .collect()
}

/// Result of applying the append-only schema migration ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    /// Versions newly committed by this invocation.
    pub applied_versions: Vec<i64>,
    /// Highest schema version now recorded.
    pub current_version: i64,
}

/// Migration failures. No driver message or connection string is rendered.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MigrationError {
    /// Required PostgreSQL/ParadeDB capabilities were absent before mutation.
    #[error("database is not ready for Cartograph migrations; failed checks: {checks:?}")]
    MissingCapabilities {
        /// Stable doctor check identifiers.
        checks: Vec<&'static str>,
    },
    /// A migration or ledger query failed.
    #[error("Cartograph schema migration failed during {operation}")]
    DatabaseOperation {
        /// Stable operation identifier.
        operation: &'static str,
    },
    /// An applied version's immutable name or checksum changed.
    #[error("migration ledger entry {version} does not match this Cartograph binary")]
    LedgerConflict {
        /// Conflicting migration version.
        version: i64,
    },
    /// The append-only ledger contains a later version but omits a predecessor.
    #[error(
        "migration ledger records version {recorded_version} but is missing version {missing_version}"
    )]
    LedgerGap {
        /// Required predecessor that is absent.
        missing_version: i64,
        /// Highest version already present.
        recorded_version: i64,
    },
    /// The database was created by a newer Cartograph binary.
    #[error("database schema version {version} is newer than this Cartograph binary")]
    SchemaVersionAhead {
        /// Newer recorded migration version.
        version: i64,
    },
    /// A read-only caller found an older, otherwise valid append-only ledger.
    #[error("database schema version {version} is older than required version {required_version}")]
    SchemaVersionBehind {
        /// Highest migration version recorded by the database.
        version: i64,
        /// Exact migration version required by this binary.
        required_version: i64,
    },
}

impl CartographDatabase {
    /// Verify the immutable migration ledger without creating a schema, applying
    /// migrations, or performing derived-index maintenance.
    /// # Errors
    ///
    /// Returns an error if the ledger cannot be read or its version sequence,
    /// names, checksums, or expected migration set are inconsistent.
    pub async fn verify_current_schema(&self) -> Result<MigrationReport, MigrationError> {
        let quoted_schema = crate::database::quoted_schema(&self.schema);
        let mut connection =
            self.pool
                .acquire()
                .await
                .map_err(|_| MigrationError::DatabaseOperation {
                    operation: "acquire-read-only-ledger",
                })?;
        let ledger = load_ledger(&mut connection, &quoted_schema).await?;
        validate_current_ledger(&ledger)
    }

    /// Verify hard capabilities, then apply append-only migrations under a
    /// transaction-scoped advisory lock.
    /// # Errors
    ///
    /// Returns an error if required capabilities fail, the advisory lock or
    /// transaction cannot be acquired, or an append-only migration cannot commit.
    pub async fn migrate(&self) -> Result<MigrationReport, MigrationError> {
        self.migrate_inner(None).await
    }

    /// Apply append-only migrations with a PostgreSQL-side statement deadline.
    /// # Errors
    ///
    /// Returns an error if the deadline cannot be installed, capability or
    /// ledger validation fails, or a locked append-only migration cannot commit.
    pub async fn migrate_bounded(
        &self,
        statement_timeout: Duration,
    ) -> Result<MigrationReport, MigrationError> {
        self.migrate_inner(Some(statement_timeout)).await
    }

    async fn migrate_inner(
        &self,
        statement_timeout: Option<Duration>,
    ) -> Result<MigrationReport, MigrationError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| MigrationError::DatabaseOperation { operation: "begin" })?;
        if let Some(statement_timeout) = statement_timeout
            && crate::database::set_local_statement_timeout(&mut transaction, statement_timeout)
                .await
                .is_err()
        {
            let _ = transaction.rollback().await;
            return Err(MigrationError::DatabaseOperation {
                operation: "statement-timeout",
            });
        }
        let capabilities = probe_capabilities_connection(&mut transaction)
            .await
            .map_err(|_| MigrationError::DatabaseOperation {
                operation: "capability-probe",
            })?;
        let failed_checks = capabilities
            .checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
            .map(|check| check.id)
            .collect::<Vec<_>>();
        if !failed_checks.is_empty() {
            let _ = transaction.rollback().await;
            return Err(MigrationError::MissingCapabilities {
                checks: failed_checks,
            });
        }
        let result = migrate_transaction(&mut transaction, &self.schema).await;
        let report = match result {
            Ok(report) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| MigrationError::DatabaseOperation {
                        operation: "commit",
                    })?;
                report
            }
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|_| MigrationError::DatabaseOperation {
                        operation: "rollback",
                    })?;
                return Err(error);
            }
        };
        self.maintain_generation_search_relations(statement_timeout)
            .await
            .map_err(|_| MigrationError::DatabaseOperation {
                operation: "search-relation-maintenance",
            })?;
        Ok(report)
    }
}

async fn migrate_transaction(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
) -> Result<MigrationReport, MigrationError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(migration_lock_key(schema))
        .execute(&mut *connection)
        .await
        .map_err(|_| MigrationError::DatabaseOperation {
            operation: "advisory-lock",
        })?;

    let quoted_schema = crate::database::quoted_schema(schema);
    execute_dynamic(
        connection,
        format!("CREATE SCHEMA IF NOT EXISTS {quoted_schema}"),
        "create-schema",
    )
    .await?;
    execute_dynamic(
        connection,
        format!(
            r#"CREATE TABLE IF NOT EXISTS {quoted_schema}."schema_migrations" (
                version bigint PRIMARY KEY CHECK (version > 0),
                name text NOT NULL,
                checksum text NOT NULL CHECK (checksum ~ '^[0-9a-f]{{64}}$'),
                applied_at timestamptz NOT NULL DEFAULT clock_timestamp()
            )"#
        ),
        "create-ledger",
    )
    .await?;

    let ledger = load_ledger(connection, &quoted_schema).await?;
    if let Some(version) = ledger
        .keys()
        .copied()
        .find(|version| *version > LATEST_SCHEMA_VERSION)
    {
        return Err(MigrationError::SchemaVersionAhead { version });
    }

    let recorded_version = ledger.keys().next_back().copied().unwrap_or_default();
    let mut applied_versions = Vec::new();
    for migration in MIGRATIONS {
        let checksum = migration_checksum(migration);
        match ledger.get(&migration.version) {
            Some(record) if record.name != migration.name || record.checksum != checksum => {
                return Err(MigrationError::LedgerConflict {
                    version: migration.version,
                });
            }
            Some(_) => {}
            None if migration.version <= recorded_version => {
                return Err(MigrationError::LedgerGap {
                    missing_version: migration.version,
                    recorded_version,
                });
            }
            None => {
                apply_migration(
                    connection,
                    ApplyMigrationInput {
                        quoted_schema: &quoted_schema,
                        migration,
                        checksum: &checksum,
                    },
                )
                .await?;
                applied_versions.push(migration.version);
            }
        }
    }

    Ok(MigrationReport {
        applied_versions,
        current_version: LATEST_SCHEMA_VERSION,
    })
}

pub(crate) fn migration_lock_key(schema: &DatabaseSchema) -> String {
    format!("{MIGRATION_LOCK_NAMESPACE}:{}", schema.as_str())
}

async fn load_ledger(
    connection: &mut PgConnection,
    quoted_schema: &str,
) -> Result<BTreeMap<i64, LedgerRecord>, MigrationError> {
    let sql = format!(
        r#"SELECT version, name, checksum FROM {quoted_schema}."schema_migrations" ORDER BY version"#
    );
    let rows = query(AssertSqlSafe(sql))
        .fetch_all(connection)
        .await
        .map_err(|_| MigrationError::DatabaseOperation {
            operation: "read-ledger",
        })?;
    let mut ledger = BTreeMap::new();
    for row in rows {
        let version = row
            .try_get::<i64, _>(0)
            .map_err(|_| MigrationError::DatabaseOperation {
                operation: "decode-ledger",
            })?;
        let name = row
            .try_get::<String, _>(1)
            .map_err(|_| MigrationError::DatabaseOperation {
                operation: "decode-ledger",
            })?;
        let checksum =
            row.try_get::<String, _>(2)
                .map_err(|_| MigrationError::DatabaseOperation {
                    operation: "decode-ledger",
                })?;
        ledger.insert(version, LedgerRecord { name, checksum });
    }
    Ok(ledger)
}

fn validate_current_ledger(
    ledger: &BTreeMap<i64, LedgerRecord>,
) -> Result<MigrationReport, MigrationError> {
    if let Some(version) = ledger
        .keys()
        .copied()
        .find(|version| *version > LATEST_SCHEMA_VERSION)
    {
        return Err(MigrationError::SchemaVersionAhead { version });
    }

    let recorded_version = ledger.keys().next_back().copied().unwrap_or_default();
    for migration in MIGRATIONS {
        let checksum = migration_checksum(migration);
        match ledger.get(&migration.version) {
            Some(record) if record.name != migration.name || record.checksum != checksum => {
                return Err(MigrationError::LedgerConflict {
                    version: migration.version,
                });
            }
            Some(_) => {}
            None if migration.version <= recorded_version => {
                return Err(MigrationError::LedgerGap {
                    missing_version: migration.version,
                    recorded_version,
                });
            }
            None => {
                return Err(MigrationError::SchemaVersionBehind {
                    version: recorded_version,
                    required_version: LATEST_SCHEMA_VERSION,
                });
            }
        }
    }

    Ok(MigrationReport {
        applied_versions: Vec::new(),
        current_version: LATEST_SCHEMA_VERSION,
    })
}

async fn apply_migration(
    connection: &mut PgConnection,
    input: ApplyMigrationInput<'_>,
) -> Result<(), MigrationError> {
    for template in input.migration.statements {
        execute_dynamic(
            connection,
            template.replace("{schema}", input.quoted_schema),
            "apply-version",
        )
        .await?;
    }
    let sql = format!(
        r#"INSERT INTO {schema}."schema_migrations" (version, name, checksum)
            VALUES ($1, $2, $3)"#,
        schema = input.quoted_schema,
    );
    query(AssertSqlSafe(sql))
        .bind(input.migration.version)
        .bind(input.migration.name)
        .bind(input.checksum)
        .execute(connection)
        .await
        .map_err(|_| MigrationError::DatabaseOperation {
            operation: "record-version",
        })?;
    Ok(())
}

async fn execute_dynamic(
    connection: &mut PgConnection,
    sql: String,
    operation: &'static str,
) -> Result<(), MigrationError> {
    // The only dynamic fragment comes from DatabaseSchema, which rejects every
    // byte outside [A-Za-z0-9_] and is quoted by quoted_schema(). Migration
    // templates are compile-time constants.
    query(AssertSqlSafe(sql))
        .execute(connection)
        .await
        .map_err(|_| MigrationError::DatabaseOperation { operation })?;
    Ok(())
}

fn migration_checksum(migration: &Migration) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&migration.version.to_be_bytes());
    hasher.update(migration.name.as_bytes());
    for statement in migration.statements {
        hasher.update(&[0]);
        hasher.update(statement.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATION_CHECKSUM_HEX_LENGTH: usize = 64;
    const CHECKSUM_COMPARISON_WINDOW: usize = 2;
    const EXPECTED_MIGRATION_VERSIONS: [i64; 43] = [
        INITIAL_SCHEMA_VERSION,
        OPERATION_LEASES_SCHEMA_VERSION,
        COMPLETE_EDGE_KINDS_SCHEMA_VERSION,
        REFERENCE_EVIDENCE_SCHEMA_VERSION,
        DIGEST_VERSION_SCHEMA_VERSION,
        BULK_RELATION_VALIDATION_SCHEMA_VERSION,
        V1_IMPORT_RETENTION_SCHEMA_VERSION,
        SEMANTIC_STORAGE_SCHEMA_VERSION,
        REFERENCE_MULTIPLICITY_SCHEMA_VERSION,
        EXACT_LOOKUP_INDEX_SCHEMA_VERSION,
        GENERATION_SEARCH_RELATIONS_SCHEMA_VERSION,
        TYPED_SYMBOL_SEMANTICS_SCHEMA_VERSION,
        AGENT_EVIDENCE_SCHEMA_VERSION,
        AGENT_SESSION_SCHEMA_VERSION,
        STRUCTURAL_BRIDGE_SCHEMA_VERSION,
        MATERIALIZED_SIMILARITY_SCHEMA_VERSION,
        NATIVE_PARSE_CACHE_SCHEMA_VERSION,
        SYMBOL_ISSUE_HISTORY_SCHEMA_VERSION,
        SYMBOL_PAGERANK_SCHEMA_VERSION,
        SUMMARY_PRIORITY_QUEUE_SCHEMA_VERSION,
        DETERMINISTIC_COCHANGE_ORDER_SCHEMA_VERSION,
        NATIVE_INDEX_DIGEST_V5_SCHEMA_VERSION,
        STORAGE_LIFECYCLE_HARDENING_SCHEMA_VERSION,
        RUST_WORKSPACE_RESOLUTION_DIGEST_V6_SCHEMA_VERSION,
        DIRECTORY_IMPORT_SIMPLE_NAME_SCHEMA_VERSION,
        NUMERICAL_EVIDENCE_DIGEST_V7_SCHEMA_VERSION,
        STRUCTURAL_DIAGNOSTICS_DIGEST_V8_SCHEMA_VERSION,
        BIOMARKER_PRECISION_DIGEST_V9_SCHEMA_VERSION,
        DETECTOR_PRECISION_DIGEST_V10_SCHEMA_VERSION,
        RUST_CLOSURE_CALL_TARGET_DIGEST_V11_SCHEMA_VERSION,
        GO_CALL_TARGET_DIGEST_V12_SCHEMA_VERSION,
        NATIVE_GENERATION_SPILL_SCHEMA_VERSION,
        SPILL_CENTRALITY_LOOKUP_SCHEMA_VERSION,
        SPILL_PARSE_CACHE_REFERENCE_SCHEMA_VERSION,
        SEARCH_DOCUMENT_CANONICAL_METADATA_SCHEMA_VERSION,
        JAVASCRIPT_CONSTRUCTION_TARGET_DIGEST_V13_SCHEMA_VERSION,
        STRUCTURAL_FINDING_CACHE_SCHEMA_VERSION,
        SHADER_AND_DISPATCH_DIGEST_V14_SCHEMA_VERSION,
        GENERATION_SOURCE_ADMISSION_SCHEMA_VERSION,
        ADA_VHDL_NUMERICAL_DIGEST_V15_SCHEMA_VERSION,
        RUST_SELF_RECEIVER_DIGEST_V16_SCHEMA_VERSION,
        RESUMABLE_RETENTION_SCHEMA_VERSION,
        GRAMMAR_REFRESH_DIGEST_V17_SCHEMA_VERSION,
    ];

    const EXPECTED_MIGRATION_CHECKSUMS: [(i64, &str); 43] = [
        (
            1,
            "47651685dfea852db86d644f0e777bd479a3926cfce9e7750887a61cfe4ddc8e",
        ),
        (
            2,
            "083e31b8263939c8c1c63a9d5898a51a1ff09f28a849d9b77cb09963e89ea7ef",
        ),
        (
            3,
            "ffc733927236a877a389a03da4c784d27d09898e657b7558d6da39f3eba01d5d",
        ),
        (
            4,
            "862554bdb310e3d7465fd4b54e2163e43279711f6eab21b46f2f6c7fc05cd532",
        ),
        (
            5,
            "30c3544a771e12864cc4cc12d9ab4600237b1262f3cd3f50e499e8aac9084ae2",
        ),
        (
            6,
            "75d236412c6c083510b6d7e7a2536ea81f93c79e106f3977661403ee9898e533",
        ),
        (
            7,
            "09c2053732c77b7b04527729290c6d55de769750a56bfcc7895cfdf429e17915",
        ),
        (
            8,
            "b80330cccdb261bef4db21948c5ea1cb0965917891d4483b8a35543325c63134",
        ),
        (
            9,
            "5b6f83886c8d247edd7a594a57ece10510fd612913662e6224118b06156f53a4",
        ),
        (
            10,
            "e9ba5a57487dd2f8d9c8e903147b2e083d19094d8c4ec289ac459b84e8b7b86a",
        ),
        (
            11,
            "a899f5923b2659a8b2be52da09b97784a954de8abd72d03a833005f5276d60ef",
        ),
        (
            12,
            "b4539a68d90dac58c142fd0c6573965a93f70400ac94fa924c872c848db2a50c",
        ),
        (
            13,
            "6ddfc988c68cf7e88dc70c3291379e3afeceab667916fbbe597ef0fdd988eff8",
        ),
        (
            14,
            "687d57c314fb32a6a16b6c892b6b04494384e651a575c9342ce0f0d22974168d",
        ),
        (
            15,
            "78209ee822c0c4cde775d8a23996b7aefab8947ff94c2f639a6bb89deccc91b5",
        ),
        (
            16,
            "1e4a57e1527e96950cbdcd4511d498d4f0fe65b9ae84b9dfe1f994a5136baa6e",
        ),
        (
            17,
            "3ee3c429c9835ce4470d4a3c579718b14cb8e0c684a713a9936ffaee7d83544e",
        ),
        (
            18,
            "6488e6899cbf80b392042173811d094d37fbfdeefdffc092dfb0948f9d455f4c",
        ),
        (
            19,
            "a4a3450b21f94ce716f2da8a3b29318c23844c786f273b8c518e1bfcd7a277e0",
        ),
        (
            20,
            "ebd8f4f39844b6552712f6fc312a30767c8b1e4bec600809b4653900eec90ad2",
        ),
        (
            21,
            "5cbc965cc09530332f8c320c70aac3b083324a21f78fe6ed8edb23057d6af518",
        ),
        (
            22,
            "ac9255910ba9dcd7babba294440758ee3bdee9ed3f142b9cd8291cc3e1128edb",
        ),
        (
            23,
            "273e9a9d09aa4d15926c0d5f8d935d99857e0976e1dd3ee7a9638604f0fa36da",
        ),
        (
            24,
            "aa6f62e612975ad71d5f3d44d7636f958f6b13c64bb8f0a795e150ed2105f9cd",
        ),
        (
            25,
            "6e3150fef9c6e7adba0f17f66864a1b217104b813725a0e99feedb07f2d88331",
        ),
        (
            26,
            "821c3fa10f3c60766d0a38c6dd0c747fdc273f2d10089fd6f715b347b9d47441",
        ),
        (
            27,
            "b0e245329c8698665484bbfcdba2fdb8dee225b56162bf9591057ba7e7af05f4",
        ),
        (
            28,
            "702c0fdafe0c2aa6bac5b967f373a310a4ad4ef4ecc0ff710dd0fb2e4c86a8b8",
        ),
        (
            29,
            "84f354b54354963e1a0733e8415d87b4dec7475bdf32d6e6365e0e390eb19b22",
        ),
        (
            30,
            "263ca9fb0ce149525b45c77ab037dd363e4c294d880c06a584ec93904efc30ef",
        ),
        (
            31,
            "a078b0076c3448fe6fee0fb99d0ddf0c2073dfc64be1ca860dea74b222714b46",
        ),
        (
            32,
            "8402b1f6e0d9993dca0e7a08865be454e2f2545baf8d9fbc2726454315f7f34e",
        ),
        (
            33,
            "4b6f2ed64babea897343ce67e26d1832926259b8f40491f7ed2438416e05c56e",
        ),
        (
            34,
            "1e6aa1a62752dca9dfba79e5021161529c41bcdb95b3873711c977fde20941d5",
        ),
        (
            35,
            "7e3ca281ee6a770b484904e1d834574fc01323beea75f18a3f4124edf2376840",
        ),
        (
            36,
            "d6cd2cbb7c422e5738b5e32acea7eb200ccd90ab79a07c363f53f94e8ec9815e",
        ),
        (
            37,
            "0446eff6def9284dc72e231dcd1bec2c7562524a2142044133ea80716c7f9843",
        ),
        (
            38,
            "3cbb510a7453da463f127d7c92f4c216d5fb65748154e19ff035711b1f3a0d46",
        ),
        (
            39,
            "f6a2d4ac41b97e8d47a58eb95c2d8b739b14f6c2318c4ad89af5a43bb039141b",
        ),
        (
            40,
            "a5fc95d44ff7b12038baa012552316c3ac5ba7e7a214c4aa2bcc47d7c04e14c5",
        ),
        (
            41,
            "1bf568eee031e851410c76a5bea4a847e61060560679b7576fc9d3bf238de583",
        ),
        (
            42,
            "fa7dfc71f61d0fccbb9da5d8255b3771b38af4d3cb6ac27739b2dd2c5604114c",
        ),
        (
            43,
            "d16123b4cba07e6249e72552c3853cdda61ff1993fbb08d7f767c3cedf6bf956",
        ),
    ];

    #[test]
    fn migration_checksum_changes_with_schema_contract_content() {
        let changed = Migration {
            version: INITIAL_SCHEMA.version,
            name: INITIAL_SCHEMA.name,
            statements: &["CREATE TABLE {schema}.changed (id bigint PRIMARY KEY)"],
        };

        assert_ne!(
            migration_checksum(&INITIAL_SCHEMA),
            migration_checksum(&changed)
        );
        assert_eq!(
            migration_checksum(&INITIAL_SCHEMA).len(),
            MIGRATION_CHECKSUM_HEX_LENGTH
        );
    }

    #[test]
    fn append_only_migration_catalog_is_contiguous_and_checksum_distinct() {
        let versions = MIGRATIONS
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>();
        let checksums = MIGRATIONS
            .iter()
            .map(|migration| migration_checksum(migration))
            .collect::<Vec<_>>();

        assert_eq!(versions, EXPECTED_MIGRATION_VERSIONS);
        assert_eq!(checksums.len(), MIGRATIONS.len());
        assert!(
            checksums
                .iter()
                .all(|checksum| checksum.len() == MIGRATION_CHECKSUM_HEX_LENGTH)
        );
        assert!(
            checksums
                .windows(CHECKSUM_COMPARISON_WINDOW)
                .all(|pair| pair[0] != pair[1])
        );
        assert_eq!(
            LATEST_SCHEMA_VERSION,
            GRAMMAR_REFRESH_DIGEST_V17_SCHEMA_VERSION
        );
    }

    #[test]
    fn committed_migrations_match_frozen_checksums() {
        let actual = MIGRATIONS
            .iter()
            .map(|migration| (migration.version, migration_checksum(migration)))
            .collect::<Vec<_>>();
        let expected = EXPECTED_MIGRATION_CHECKSUMS
            .iter()
            .map(|(version, checksum)| (*version, (*checksum).to_owned()))
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn read_only_ledger_verification_requires_the_exact_current_schema() {
        let mut ledger = MIGRATIONS
            .iter()
            .map(|migration| {
                (
                    migration.version,
                    LedgerRecord {
                        name: migration.name.to_owned(),
                        checksum: migration_checksum(migration),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            validate_current_ledger(&ledger),
            Ok(MigrationReport {
                applied_versions: Vec::new(),
                current_version: LATEST_SCHEMA_VERSION,
            })
        );

        ledger.remove(&LATEST_SCHEMA_VERSION);
        assert_eq!(
            validate_current_ledger(&ledger),
            Err(MigrationError::SchemaVersionBehind {
                version: LATEST_SCHEMA_VERSION - 1,
                required_version: LATEST_SCHEMA_VERSION,
            })
        );
    }
}
