//! V23 retains the merged tracks' facts while reconstructing the removed Lua
//! subfeature: three symbol flags, three document records and two provenances.
//! V23 changes the digest domain; facts unchanged in the restored projection.
use super::{
    CanonicalGenerationFacts, EXPECTED_GENERIC_FAMILY_DIGEST, GenerationFacts, PipelineStage,
    SERIAL_WORKERS, SearchDocumentInput, SymbolExportFlags, TEST_GENERATION_BYTES, build,
    capability_symbol, generation_validation_limits, tempdir, validate_generation_facts,
    write_generic_family_project,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_lua_proof_restores_the_legacy_digest_fact_by_fact() {
    let directory = tempdir().unwrap_or_else(|error| panic!("fixture directory failed: {error}"));
    write_generic_family_project(directory.path());
    let generation = build(directory.path(), SERIAL_WORKERS).await;
    let receiver_restored = super::types_digest_proof::restore_receiver_delta(generation.facts());
    let baseline = super::bridge_digest_proof::restore(&receiver_restored);
    let current = &baseline;
    let mut legacy = raw_facts(current);
    for (name, default) in [("M", true), ("M.pack", false), ("M:size", false)] {
        let symbol = capability_symbol(current, "generic/fixture.lua", name);
        assert_eq!(symbol.export, SymbolExportFlags::default());
        let retained = legacy
            .symbols
            .iter_mut()
            .find(|candidate| candidate.symbol_id == symbol.symbol_id)
            .unwrap_or_else(|| panic!("missing retained symbol {name}"));
        retained.export = SymbolExportFlags::new(true, default);
        let document = legacy
            .documents
            .iter_mut()
            .find(|document| document.symbol_id.as_ref() == Some(&symbol.symbol_id))
            .unwrap_or_else(|| panic!("missing retained document {name}"));
        assert_eq!(document.metadata["exported"], false);
        assert_eq!(document.metadata["default_export"], false);
        document.metadata["exported"] = true.into();
        document.metadata["default_export"] = default.into();
    }
    let file = current
        .files()
        .iter()
        .find(|file| file.normalized_path == "generic/fixture.lua")
        .unwrap_or_else(|| panic!("missing Lua fixture"));
    for name in ["Helper.log", "Utils.size"] {
        let reference = legacy
            .references
            .iter_mut()
            .find(|reference| reference.file_id == file.file_id && reference.reference_name == name)
            .unwrap_or_else(|| panic!("missing namespace call {name}"));
        assert!(reference.target_symbol_id.is_none());
        assert_eq!(reference.resolution_provenance, "native-unresolved");
        reference.resolution_provenance = "native-unresolved-import".to_owned();
    }
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|error| panic!("validation limits failed: {error}"));
    let (legacy, _) = validate_generation_facts(legacy, limits, || false)
        .unwrap_or_else(|error| panic!("legacy fact validation failed: {error}"));
    assert_eq!(
        legacy.digest().as_str(),
        "64a235d60b4dbea66e4bf387b2e1053c42160463b81a98007fa3676568145647"
    );
    assert_eq!(current.edges(), legacy.edges());
    assert_eq!(
        generation.facts().digest().as_str(),
        EXPECTED_GENERIC_FAMILY_DIGEST
    );
}

pub(super) fn raw_facts(facts: &CanonicalGenerationFacts) -> GenerationFacts {
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
                    .unwrap_or_else(|error| panic!("document metadata failed: {error}")),
            })
            .collect(),
    }
}
