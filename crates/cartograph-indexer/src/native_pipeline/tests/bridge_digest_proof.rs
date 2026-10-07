//! Remove six physical `ObjC` alias links after restoring the receiver delta
//! to recover the exact wave-2 facts, rehashed under V23.

use super::*;

// V23 changes the digest domain; facts unchanged in the restored projection.
pub(super) const PREVIOUS_GENERIC_DIGEST: &str =
    "04a9f9b9e4b108c1728a10565c0fc81cba23f10f625c351307f2be56d85c21d9";
const PHYSICAL_ALIAS_EDGES: usize = 6;

pub(super) fn restore(facts: &CanonicalGenerationFacts) -> CanonicalGenerationFacts {
    let mut restored = super::unqual_digests::raw_facts(facts);
    let before = restored.edges.len();
    restored
        .edges
        .retain(|edge| edge.provenance != native_bridge_details::PHYSICAL_METHOD_PROVENANCE);
    assert_eq!(before - restored.edges.len(), PHYSICAL_ALIAS_EDGES);
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|error| panic!("invalid bridge proof limits: {error}"));
    let (restored, _) = validate_generation_facts(restored, limits, || false)
        .unwrap_or_else(|error| panic!("invalid restored bridge facts: {error}"));
    assert_eq!(restored.digest().as_str(), PREVIOUS_GENERIC_DIGEST);
    restored
}

#[test]
fn physical_alias_links_are_the_only_generic_corpus_fact_change() {
    let fixtures = GENERIC_FAMILY_FIXTURES
        .iter()
        .map(|(path, source, _)| (*path, *source))
        .collect::<Vec<_>>();
    let facts = build_capability_generation(&fixtures, false);
    for edge in facts
        .edges()
        .iter()
        .filter(|edge| edge.provenance == native_bridge_details::PHYSICAL_METHOD_PROVENANCE)
    {
        let alias = facts
            .symbols()
            .iter()
            .find(|s| s.symbol_id == edge.source_symbol_id)
            .unwrap_or_else(|| panic!("missing source alias: {edge:?}"));
        let physical = facts
            .symbols()
            .iter()
            .find(|s| s.symbol_id == edge.target_symbol_id)
            .unwrap_or_else(|| panic!("missing target declaration: {edge:?}"));
        assert_eq!(edge.kind, EdgeKind::References);
        assert_eq!(
            edge.confidence,
            native_bridge_details::CONVENTION_CONFIDENCE
        );
        assert!(alias.qualified_name.contains("::objc-swift-method::"));
        assert!(
            [
                ("Greetable::greeting", 13),
                ("Person::initWithName:age:", 19),
                ("Person::describe", 20),
                ("Person::initWithName:age:", 25),
                ("Person::describe", 34),
                ("Person::greeting", 38),
            ]
            .contains(&(physical.qualified_name.as_str(), physical.start_line))
        );
        assert!(facts.edges().iter().any(|e| e.kind == EdgeKind::Contains
            && e.source_symbol_id == physical.symbol_id
            && e.target_symbol_id == alias.symbol_id));
    }
    let receiver_restored = super::types_digest_proof::restore_receiver_delta(&facts);
    let restored = restore(&receiver_restored);
    assert_eq!(receiver_restored.symbols(), restored.symbols());
    assert_eq!(receiver_restored.references(), restored.references());
    assert_eq!(receiver_restored.documents(), restored.documents());
    assert_eq!(facts.digest().as_str(), EXPECTED_GENERIC_FAMILY_DIGEST);
}
