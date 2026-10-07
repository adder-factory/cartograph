//! Remove the two receiver calls and restore centrality, retaining the bridge
//! delta. Removing that delta afterwards reconstructs the wave-2 base facts.

use super::{
    CanonicalGenerationFacts, CapabilityReferenceQuery, EdgeKind, GENERIC_FAMILY_FIXTURES,
    GenerationFacts, PipelineStage, ReferenceKind, TEST_GENERATION_BYTES, UNRESOLVED_CONFIDENCE,
    apply_page_rank, apply_sampled_betweenness, assert_confidence, build_capability_generation,
    capability_symbol, generation_validation_limits, validate_generation_facts,
};

// V23 changes the digest domain; facts unchanged in both restored projections.
const PREVIOUS_RECEIVER_DIGEST: &str =
    "04a9f9b9e4b108c1728a10565c0fc81cba23f10f625c351307f2be56d85c21d9";
const BRIDGE_ONLY_DIGEST: &str = "afc77153d56af2f93f22e1d8abbc75c702874033d3d2fd78f01c0e1f6224f04b";
const EXPECTED_RECEIVER_CONFIDENCE: f32 = 0.95;
const RECEIVER_DELTA: &[(&str, u64, u64, &str)] = &[
    ("generic/fixture.rb", 293, 296, "native-unresolved"),
    (
        "generic/fixture.dart",
        310,
        317,
        "native-external-reference",
    ),
];

#[test]
fn removing_explicit_receiver_facts_restores_the_previous_digest() {
    let fixtures = GENERIC_FAMILY_FIXTURES
        .iter()
        .map(|(path, source, _)| (*path, *source))
        .collect::<Vec<_>>();
    let current = build_capability_generation(&fixtures, false);
    let restored = restore_receiver_delta(&current);
    assert_eq!(current.files(), restored.files());
    assert_eq!(current.documents(), restored.documents());
    assert_eq!(current.references().len(), restored.references().len());
    assert_eq!(
        current.edges().len() - restored.edges().len(),
        RECEIVER_DELTA.len()
    );
    let baseline = super::bridge_digest_proof::restore(&restored);
    assert_eq!(baseline.digest().as_str(), PREVIOUS_RECEIVER_DIGEST);
}

pub(super) fn restore_receiver_delta(
    current: &CanonicalGenerationFacts,
) -> CanonicalGenerationFacts {
    let mut restored = super::generic_digest_proof::unordered_copy(current);
    for &site in RECEIVER_DELTA {
        restore_call(current, &mut restored, site);
    }
    apply_page_rank(&mut restored, || false)
        .unwrap_or_else(|error| panic!("restored PageRank: {error}"));
    apply_sampled_betweenness(&mut restored, || false)
        .unwrap_or_else(|error| panic!("restored betweenness: {error}"));
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|error| panic!("invalid proof limits: {error}"));
    let (restored, _) = validate_generation_facts(restored, limits, || false)
        .unwrap_or_else(|error| panic!("invalid restored receiver facts: {error}"));
    assert_eq!(restored.digest().as_str(), BRIDGE_ONLY_DIGEST);
    restored
}

fn restore_call(
    current: &CanonicalGenerationFacts,
    restored: &mut GenerationFacts,
    site: (&str, u64, u64, &str),
) {
    let (path, start, end, previous_provenance) = site;
    let owner = capability_symbol(current, path, "process");
    let target = capability_symbol(current, path, "Container::add");
    let call = CapabilityReferenceQuery::new(current, owner).named("c.add", ReferenceKind::Calls);
    assert_eq!((call.start_byte, call.end_byte), (start, end));
    assert_eq!(call.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(call.resolution_provenance, "native-explicit-receiver-type");
    assert_confidence(call.confidence, EXPECTED_RECEIVER_CONFIDENCE);
    let retained = restored
        .references
        .iter_mut()
        .find(|reference| **reference == *call)
        .unwrap_or_else(|| panic!("missing exact receiver call {path}"));
    retained.target_symbol_id = None;
    retained.confidence = UNRESOLVED_CONFIDENCE;
    retained.resolution_provenance = previous_provenance.to_owned();
    let before = restored.edges.len();
    restored.edges.retain(|edge| {
        !(edge.kind == EdgeKind::Calls
            && edge.source_symbol_id == owner.symbol_id
            && edge.target_symbol_id == target.symbol_id
            && edge.provenance == "native-explicit-receiver-type")
    });
    assert_eq!(before - restored.edges.len(), 1);
}
