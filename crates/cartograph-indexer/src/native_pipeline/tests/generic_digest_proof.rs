use super::{
    CanonicalGenerationFacts, EdgeKind, GENERIC_FAMILY_FIXTURES, GenerationFacts, PipelineStage,
    ReferenceKind, SearchDocumentInput, TEST_GENERATION_BYTES, UNRESOLVED_CONFIDENCE,
    build_capability_generation, capability_symbol, generation_validation_limits,
    validate_generation_facts,
};

// V23 changes the digest domain; facts unchanged in the restored projection.
const PREVIOUS_GENERIC_DIGEST: &str =
    "27faff0015ddb9d70d8602f5a5a85a159ad3d161e8f9499857f642d349f4e438";

pub(super) fn unordered_copy(facts: &CanonicalGenerationFacts) -> GenerationFacts {
    GenerationFacts {
        files: facts.files().to_vec(),
        symbols: facts.symbols().to_vec(),
        edges: facts.edges().to_vec(),
        references: facts.references().to_vec(),
        numerical_sites: facts.numerical_sites().to_vec(),
        documents: facts
            .documents()
            .iter()
            .map(|document| SearchDocumentInput {
                document_id: document.document_id().clone(),
                file_id: document.file_id().cloned(),
                symbol_id: document.symbol_id().cloned(),
                path: document.path().to_owned(),
                language: document.language().to_owned(),
                kind: document.kind(),
                qualified_name: document.qualified_name().to_owned(),
                code: document.code().to_owned(),
                natural_text: document.natural_text().to_owned(),
                metadata: serde_json::from_str(document.metadata_json())
                    .unwrap_or_else(|error| panic!("invalid canonical metadata: {error}")),
            })
            .collect(),
    }
}

#[test]
fn generic_corpus_resolution_delta_reconstructs_previous_frozen_digest() {
    let fixtures = GENERIC_FAMILY_FIXTURES
        .iter()
        .map(|(path, source, _)| (*path, *source))
        .collect::<Vec<_>>();
    let current = build_capability_generation(&fixtures, false);
    let receiver_restored = super::types_digest_proof::restore_receiver_delta(&current);
    let facts = super::bridge_digest_proof::restore(&receiver_restored);
    let build = capability_symbol(&facts, "generic/fixture.ets", "CounterView::build");
    let increment = capability_symbol(&facts, "generic/fixture.ets", "CounterView::increment");
    let mut restored = unordered_copy(&facts);
    let edges_before = restored.edges.len();
    restored.edges.retain(|edge| {
        !(edge.kind == EdgeKind::Calls
            && edge.source_symbol_id == build.symbol_id
            && edge.target_symbol_id == increment.symbol_id)
    });
    assert_eq!(edges_before - restored.edges.len(), 1);
    let mut restored_references = 0;
    for reference in &mut restored.references {
        let previous = if reference.owner_symbol_id.as_ref() == Some(&build.symbol_id)
            && ["increment", "this.increment"].contains(&reference.reference_name.as_str())
        {
            Some((
                if reference.reference_name == "increment" {
                    "native-dynamic-unresolved"
                } else {
                    "native-unresolved"
                },
                &increment.symbol_id,
            ))
        } else {
            None
        };
        let Some((provenance, target)) = previous else {
            continue;
        };
        assert_eq!(reference.reference_kind, ReferenceKind::Calls.as_str());
        assert_eq!(reference.target_symbol_id.as_ref(), Some(target));
        reference.target_symbol_id = None;
        reference.confidence = UNRESOLVED_CONFIDENCE;
        reference.resolution_provenance = provenance.to_owned();
        restored_references += 1;
    }
    assert_eq!(restored_references, 2);
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|error| panic!("invalid proof limits: {error}"));
    let (restored, _) = validate_generation_facts(restored, limits, || false)
        .unwrap_or_else(|error| panic!("invalid restored corpus: {error}"));
    assert_eq!(restored.digest().as_str(), PREVIOUS_GENERIC_DIGEST);
}
