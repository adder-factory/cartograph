//! Remove the six physical `ObjC` alias links to recover the exact wave-2 facts.

use super::*;

pub(super) const PREVIOUS_GENERIC_DIGEST: &str =
    "a4e55aab2f0dbbaa33046f1a9975810b75c6cee9ea831482cdda5b958a437e2b";
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
    let restored = restore(&facts);
    assert_eq!(facts.symbols(), restored.symbols());
    assert_eq!(facts.references(), restored.references());
    assert_eq!(facts.documents(), restored.documents());
    assert_eq!(facts.digest().as_str(), EXPECTED_GENERIC_FAMILY_DIGEST);
}
