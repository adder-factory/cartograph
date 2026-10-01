WITH batch AS MATERIALIZED (
                SELECT next_documents.document_id, next_documents.path,
                       next_documents.language, next_documents.document_kind,
                       next_documents.qualified_name, next_documents.code,
                       next_documents.natural_text
                FROM {quoted_schema}."search_documents" AS next_documents
                WHERE next_documents.project_id = CAST($1 AS uuid)
                  AND next_documents.generation_id = CAST($2 AS uuid)
                  AND ($3::uuid IS NULL OR next_documents.document_id > $3::uuid)
                ORDER BY next_documents.document_id
                LIMIT $4
            ), carried AS (
                INSERT INTO {quoted_schema}."document_embeddings" (
                    project_id, generation_id, document_id, model_id,
                    source_digest, embedding, created_at, updated_at
                )
                SELECT previous_embeddings.project_id, CAST($2 AS uuid),
                       batch.document_id, previous_embeddings.model_id,
                       previous_embeddings.source_digest, previous_embeddings.embedding,
                       previous_embeddings.created_at, clock_timestamp()
                FROM batch
                JOIN {quoted_schema}."projects" AS projects
                  ON projects.project_id = CAST($1 AS uuid)
                 AND projects.current_generation_id IS NOT NULL
                JOIN {quoted_schema}."search_documents" AS previous_documents
                  ON previous_documents.project_id = projects.project_id
                 AND previous_documents.generation_id = projects.current_generation_id
                 AND previous_documents.document_id = batch.document_id
                 AND previous_documents.path = batch.path
                 AND previous_documents.language = batch.language
                 AND previous_documents.document_kind = batch.document_kind
                 AND previous_documents.qualified_name = batch.qualified_name
                 AND previous_documents.code = batch.code
                 AND previous_documents.natural_text = batch.natural_text
                JOIN {quoted_schema}."document_embeddings" AS previous_embeddings
                  ON previous_embeddings.project_id = previous_documents.project_id
                 AND previous_embeddings.generation_id = previous_documents.generation_id
                 AND previous_embeddings.document_id = previous_documents.document_id
                JOIN {quoted_schema}."embedding_models" AS models
                  ON models.model_id = previous_embeddings.model_id
                 AND models.state = 'active'
                ON CONFLICT (project_id, generation_id, document_id, model_id) DO NOTHING
                RETURNING 1
            )
            SELECT (SELECT count(*) FROM batch)::bigint AS scanned,
                   (SELECT count(*) FROM carried)::bigint AS carried,
                   (SELECT document_id::text FROM batch
                       ORDER BY document_id DESC LIMIT 1) AS last_document_id
