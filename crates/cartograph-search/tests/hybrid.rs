//! Integration coverage for deterministic multi-channel retrieval fusion.

use cartograph_db as _;
use cartograph_domain::{
    DocumentId, DocumentKind, GenerationId, NormalizedPath, SourceLanguage, SymbolKind,
};
use cartograph_search::{
    ChannelCandidate, ChannelResults, HybridSearchInput, LexicalComponent, RerankReport,
    RerankState, RetrievalAbstention, RetrievalChannel, RetrievalChannels, RetrievalDocument,
    RetrievalDocumentInput, RetrievalExecution, RetrievalFallback, RetrievalPreference, SearchMode,
    SemanticReadiness, fuse_search,
};
use serde as _;
use thiserror as _;
use tokio as _;

const RESULT_LIMIT: u16 = 20;
const MAXIMUM_CHANNEL_CANDIDATES: u16 = 100;
const RRF_OFFSET: f64 = 60.0;
const LEXICAL_PRIMARY_SCORE: f64 = 12.5;
const LEXICAL_SECONDARY_SCORE: f64 = 8.25;
const SEMANTIC_PRIMARY_SCORE: f64 = 0.91;
const SEMANTIC_SECONDARY_SCORE: f64 = 0.82;

fn assert_score(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= f64::EPSILON,
        "score mismatch: expected {expected}, got {actual}"
    );
}

#[test]
fn reciprocal_rank_fusion_retains_channel_provenance_and_raw_values() {
    let lexical = channel(
        RetrievalChannel::Lexical,
        vec![
            lexical_candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE)),
            lexical_candidate("b", "src/b.rs", (2, LEXICAL_SECONDARY_SCORE)),
        ],
    );
    let semantic = channel(
        RetrievalChannel::Semantic,
        vec![
            candidate("b", "src/b.rs", (1, SEMANTIC_PRIMARY_SCORE)),
            candidate("c", "src/c.rs", (2, SEMANTIC_SECONDARY_SCORE)),
        ],
    );
    let packet = packet(
        SearchMode::Hybrid,
        SemanticReadiness::Ready,
        (lexical, semantic),
    );

    assert_eq!(packet.execution(), RetrievalExecution::Hybrid);
    assert_eq!(packet.fallback(), None);
    assert_eq!(paths(&packet), vec!["src/b.rs", "src/a.rs", "src/c.rs"]);
    let first = &packet.items()[0];
    assert_eq!(first.rank(), 1);
    assert_eq!(first.document().language(), SourceLanguage::Rust);
    assert_eq!(first.document().document_kind(), DocumentKind::Symbol);
    assert_eq!(first.contributions().len(), 2);
    assert_eq!(
        first.contributions()[0].channel(),
        RetrievalChannel::Lexical
    );
    assert_eq!(first.contributions()[0].rank(), 2);
    assert_score(
        first.contributions()[0].raw_score(),
        LEXICAL_SECONDARY_SCORE,
    );
    assert_eq!(
        first.contributions()[0].lexical_components(),
        &[LexicalComponent::QualifiedName]
    );
    assert_eq!(
        first.contributions()[1].channel(),
        RetrievalChannel::Semantic
    );
    assert_eq!(first.contributions()[1].rank(), 1);
    assert_score(first.contributions()[1].raw_score(), SEMANTIC_PRIMARY_SCORE);
    assert!(first.reciprocal_rank_score() > packet.items()[1].reciprocal_rank_score());
}

#[test]
fn fusion_is_repeatable_across_candidate_and_channel_completion_order() {
    let lexical_forward = vec![
        lexical_candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE)),
        lexical_candidate("b", "src/b.rs", (2, LEXICAL_SECONDARY_SCORE)),
    ];
    let mut lexical_reverse = lexical_forward.clone();
    lexical_reverse.reverse();
    let semantic_forward = vec![
        candidate("b", "src/b.rs", (1, SEMANTIC_PRIMARY_SCORE)),
        candidate("c", "src/c.rs", (2, SEMANTIC_SECONDARY_SCORE)),
    ];
    let mut semantic_reverse = semantic_forward.clone();
    semantic_reverse.reverse();

    let expected = fused_with_order(lexical_forward.clone(), semantic_forward.clone(), false);
    for (lexical, semantic, semantic_first) in [
        (lexical_forward, semantic_forward, true),
        (lexical_reverse.clone(), semantic_reverse.clone(), false),
        (lexical_reverse, semantic_reverse, true),
    ] {
        assert_eq!(
            fused_with_order(lexical, semantic, semantic_first),
            expected
        );
    }
}

#[test]
fn fusion_properties_hold_for_every_four_document_rank_permutation() {
    let orders = rank_orders();
    for lexical_order in &orders {
        for semantic_order in &orders {
            let lexical = ranked_candidates(lexical_order, RetrievalChannel::Lexical);
            let semantic = ranked_candidates(semantic_order, RetrievalChannel::Semantic);
            let expected = fused_with_order(lexical.clone(), semantic.clone(), false);

            let mut reversed_lexical = lexical;
            reversed_lexical.reverse();
            let mut reversed_semantic = semantic;
            reversed_semantic.reverse();
            let completion_reversed = fused_with_order(reversed_lexical, reversed_semantic, true);
            assert_eq!(completion_reversed, expected);

            assert_eq!(expected.items().len(), lexical_order.len());
            for (index, item) in expected.items().iter().enumerate() {
                let expected_rank = u16::try_from(index + 1)
                    .unwrap_or_else(|error| panic!("fixture rank conversion failed: {error}"));
                assert_eq!(item.rank(), expected_rank);
                assert_eq!(item.contributions().len(), 2);
                assert_eq!(item.contributions()[0].channel(), RetrievalChannel::Lexical);
                assert_eq!(
                    item.contributions()[1].channel(),
                    RetrievalChannel::Semantic
                );
                let identifier = item
                    .document()
                    .qualified_name()
                    .trim_start_matches("symbol_");
                let lexical_rank = rank_of(lexical_order, identifier);
                let semantic_rank = rank_of(semantic_order, identifier);
                let oracle =
                    reciprocal_rank_oracle(lexical_rank) + reciprocal_rank_oracle(semantic_rank);
                assert_score(item.reciprocal_rank_score(), oracle);
            }
            for [higher, lower] in expected.items().array_windows() {
                assert!(higher.reciprocal_rank_score() >= lower.reciprocal_rank_score());
            }
        }
    }
}

#[test]
fn smaller_result_limits_are_exact_prefixes_and_report_omissions() {
    let candidates = ranked_candidates(&["a", "b", "c", "z"], RetrievalChannel::Lexical);
    let complete = lexical_packet_with_limit(candidates.clone(), 4);
    assert!(!complete.truncated());

    for limit in 1_u16..=4 {
        let bounded = lexical_packet_with_limit(candidates.clone(), limit);
        let bounded_length = usize::from(limit);
        assert_eq!(bounded.items(), &complete.items()[..bounded_length]);
        assert_eq!(bounded.truncated(), limit < 4);
    }
}

#[test]
fn equal_rrf_scores_use_stable_document_tie_breaks() {
    let lexical = channel(
        RetrievalChannel::Lexical,
        vec![candidate("z", "src/z.rs", (1, LEXICAL_PRIMARY_SCORE))],
    );
    let semantic = channel(
        RetrievalChannel::Semantic,
        vec![candidate("a", "src/a.rs", (1, SEMANTIC_PRIMARY_SCORE))],
    );
    let packet = packet(
        SearchMode::Hybrid,
        SemanticReadiness::Ready,
        (lexical, semantic),
    );
    assert_eq!(paths(&packet), vec!["src/a.rs", "src/z.rs"]);
}

#[test]
fn production_preference_promotes_definitions_without_hiding_tests_or_parameters() {
    let ranked = vec![
        typed_candidate(
            "a",
            "tests/search.rs",
            DocumentKind::Test,
            SymbolKind::Function,
            (1, 12.0),
        ),
        typed_candidate(
            "b",
            "src/search.rs",
            DocumentKind::Symbol,
            SymbolKind::Parameter,
            (2, 11.0),
        ),
        typed_candidate(
            "c",
            "src/search.rs",
            DocumentKind::Symbol,
            SymbolKind::Import,
            (3, 10.0),
        ),
        typed_candidate(
            "z",
            "src/search.rs",
            DocumentKind::Symbol,
            SymbolKind::Function,
            (4, 9.0),
        ),
    ];
    let neutral = input(SearchMode::Deterministic, SemanticReadiness::NotConfigured)
        .with_channel(channel(RetrievalChannel::Lexical, ranked.clone()))
        .and_then(fuse_search)
        .unwrap_or_else(|error| panic!("neutral fusion failed: {error}"));
    assert_eq!(
        paths(&neutral),
        vec![
            "tests/search.rs",
            "src/search.rs",
            "src/search.rs",
            "src/search.rs",
        ]
    );

    let preferred = input(SearchMode::Deterministic, SemanticReadiness::NotConfigured)
        .with_preference(RetrievalPreference::ProductionDefinitions)
        .with_channel(channel(RetrievalChannel::Lexical, ranked))
        .and_then(fuse_search)
        .unwrap_or_else(|error| panic!("preferred fusion failed: {error}"));
    let kinds = preferred
        .items()
        .iter()
        .map(|item| item.document().symbol_kind())
        .collect::<Vec<_>>();
    assert_eq!(
        preferred.preference(),
        RetrievalPreference::ProductionDefinitions
    );
    assert_eq!(
        kinds,
        vec![
            Some(SymbolKind::Function),
            Some(SymbolKind::Parameter),
            Some(SymbolKind::Import),
            Some(SymbolKind::Function),
        ]
    );
    assert_eq!(
        preferred.items()[3].document().document_kind(),
        DocumentKind::Test
    );
}

#[test]
fn unavailable_or_empty_channels_degrade_explicitly() {
    let lexical = channel(
        RetrievalChannel::Lexical,
        vec![candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE))],
    );
    let not_ready = input(SearchMode::Auto, SemanticReadiness::NotIndexed)
        .with_channel(lexical.clone())
        .unwrap_or_else(|error| panic!("lexical channel failed: {error}"));
    let packet = fuse_search(not_ready).unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(packet.execution(), RetrievalExecution::Lexical);
    assert_eq!(packet.fallback(), Some(RetrievalFallback::SemanticNotReady));
    assert_eq!(packet.abstention(), None);

    let semantic_only = input(SearchMode::Hybrid, SemanticReadiness::Ready)
        .with_channel(channel(
            RetrievalChannel::Semantic,
            vec![candidate("b", "src/b.rs", (1, SEMANTIC_PRIMARY_SCORE))],
        ))
        .unwrap_or_else(|error| panic!("semantic channel failed: {error}"));
    let packet =
        fuse_search(semantic_only).unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(packet.execution(), RetrievalExecution::Semantic);
    assert_eq!(
        packet.fallback(),
        Some(RetrievalFallback::LexicalUnavailable)
    );

    let semantic_empty = input(SearchMode::Hybrid, SemanticReadiness::Ready)
        .with_channel(lexical)
        .and_then(|value| value.with_channel(channel(RetrievalChannel::Semantic, Vec::new())))
        .unwrap_or_else(|error| panic!("channels failed: {error}"));
    let packet =
        fuse_search(semantic_empty).unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(packet.execution(), RetrievalExecution::Lexical);
    assert_eq!(packet.fallback(), Some(RetrievalFallback::SemanticEmpty));
}

#[test]
fn deterministic_mode_ignores_semantic_candidates_without_hiding_readiness() {
    let input = input(SearchMode::Deterministic, SemanticReadiness::Ready)
        .with_channel(channel(
            RetrievalChannel::Lexical,
            vec![candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE))],
        ))
        .and_then(|value| {
            value.with_channel(channel(
                RetrievalChannel::Semantic,
                vec![candidate("b", "src/b.rs", (1, SEMANTIC_PRIMARY_SCORE))],
            ))
        })
        .unwrap_or_else(|error| panic!("channels failed: {error}"));
    let packet = fuse_search(input).unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(packet.requested_mode(), SearchMode::Deterministic);
    assert_eq!(packet.semantic_readiness(), SemanticReadiness::Ready);
    assert_eq!(packet.execution(), RetrievalExecution::Lexical);
    assert_eq!(packet.fallback(), None);
    assert_eq!(packet.rerank_report().state(), RerankState::NotRequested);
    assert_eq!(paths(&packet), vec!["src/a.rs"]);
}

#[test]
fn empty_inputs_abstain_and_limits_are_explicit() {
    let unavailable = fuse_search(input(SearchMode::Auto, SemanticReadiness::Unavailable))
        .unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(unavailable.execution(), RetrievalExecution::Abstained);
    assert_eq!(
        unavailable.abstention(),
        Some(RetrievalAbstention::NoUsableChannel)
    );

    let empty = input(SearchMode::Hybrid, SemanticReadiness::Ready)
        .with_channel(channel(RetrievalChannel::Lexical, Vec::new()))
        .and_then(|value| value.with_channel(channel(RetrievalChannel::Semantic, Vec::new())))
        .unwrap_or_else(|error| panic!("channels failed: {error}"));
    let empty = fuse_search(empty).unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(
        empty.abstention(),
        Some(RetrievalAbstention::NoRelevantEvidence)
    );

    let limited = HybridSearchInput::new(SearchMode::Deterministic, SemanticReadiness::Ready, 1)
        .and_then(|value| {
            value.with_channel(
                ChannelResults::new(
                    RetrievalChannel::Lexical,
                    vec![
                        candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE)),
                        candidate("b", "src/b.rs", (2, LEXICAL_SECONDARY_SCORE)),
                    ],
                )
                .map(|channel| channel.with_truncated(true))?,
            )
        })
        .unwrap_or_else(|error| panic!("limited input failed: {error}"));
    let limited = fuse_search(limited).unwrap_or_else(|error| panic!("fusion failed: {error}"));
    assert_eq!(limited.items().len(), 1);
    assert!(limited.truncated());
}

#[test]
fn malformed_channel_inputs_fail_closed() {
    assert!(ChannelCandidate::new(document("a", "src/a.rs"), 0, 1.0).is_err());
    assert!(
        ChannelCandidate::new(
            document("a", "src/a.rs"),
            MAXIMUM_CHANNEL_CANDIDATES + 1,
            1.0,
        )
        .is_err()
    );
    assert!(ChannelCandidate::new(document("a", "src/a.rs"), 1, f64::NAN).is_err());
    assert!(ChannelCandidate::new(document("a", "src/a.rs"), 1, f64::INFINITY).is_err());
    assert!(ChannelCandidate::new(document("a", "src/a.rs"), 1, f64::NEG_INFINITY).is_err());
    assert!(
        HybridSearchInput::new(
            SearchMode::Hybrid,
            SemanticReadiness::Ready,
            MAXIMUM_CHANNEL_CANDIDATES + 1,
        )
        .is_err()
    );
    let duplicate_rank = ChannelResults::new(
        RetrievalChannel::Lexical,
        vec![
            candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE)),
            candidate("b", "src/b.rs", (1, LEXICAL_SECONDARY_SCORE)),
        ],
    );
    assert!(duplicate_rank.is_err());
    let duplicate_document = ChannelResults::new(
        RetrievalChannel::Lexical,
        vec![
            candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE)),
            candidate("a", "src/a.rs", (2, LEXICAL_SECONDARY_SCORE)),
        ],
    );
    assert!(duplicate_document.is_err());

    let semantic_components = ChannelResults::new(
        RetrievalChannel::Semantic,
        vec![lexical_candidate(
            "a",
            "src/a.rs",
            (1, SEMANTIC_PRIMARY_SCORE),
        )],
    );
    assert!(semantic_components.is_err());

    let duplicate_channel = input(SearchMode::Hybrid, SemanticReadiness::Ready)
        .with_channel(channel(RetrievalChannel::Lexical, Vec::new()))
        .and_then(|value| value.with_channel(channel(RetrievalChannel::Lexical, Vec::new())));
    assert!(duplicate_channel.is_err());
}

#[test]
fn all_non_ready_semantic_states_have_the_same_explicit_lexical_fallback() {
    for readiness in [
        SemanticReadiness::NotConfigured,
        SemanticReadiness::NotIndexed,
        SemanticReadiness::Stale,
        SemanticReadiness::Unavailable,
    ] {
        let input = input(SearchMode::Auto, readiness)
            .with_channel(channel(
                RetrievalChannel::Lexical,
                vec![candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE))],
            ))
            .unwrap_or_else(|error| panic!("lexical channel failed: {error}"));
        let packet = fuse_search(input).unwrap_or_else(|error| panic!("fusion failed: {error}"));
        assert_eq!(packet.execution(), RetrievalExecution::Lexical);
        assert_eq!(packet.fallback(), Some(RetrievalFallback::SemanticNotReady));
        assert_eq!(paths(&packet), vec!["src/a.rs"]);
    }
}

#[test]
fn packet_rejects_mixed_generations_and_inconsistent_shared_documents() {
    let mixed_generation = input(SearchMode::Hybrid, SemanticReadiness::Ready)
        .with_channel(channel(
            RetrievalChannel::Lexical,
            vec![candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE))],
        ))
        .and_then(|value| {
            value.with_channel(channel(
                RetrievalChannel::Semantic,
                vec![
                    ChannelCandidate::new(
                        document_in_generation("b", "src/b.rs", second_generation()),
                        1,
                        SEMANTIC_PRIMARY_SCORE,
                    )
                    .unwrap_or_else(|error| panic!("candidate failed: {error}")),
                ],
            ))
        })
        .unwrap_or_else(|error| panic!("channel input failed: {error}"));
    assert!(fuse_search(mixed_generation).is_err());

    let inconsistent_document = input(SearchMode::Hybrid, SemanticReadiness::Ready)
        .with_channel(channel(
            RetrievalChannel::Lexical,
            vec![candidate("a", "src/a.rs", (1, LEXICAL_PRIMARY_SCORE))],
        ))
        .and_then(|value| {
            value.with_channel(channel(
                RetrievalChannel::Semantic,
                vec![candidate("a", "src/other.rs", (1, SEMANTIC_PRIMARY_SCORE))],
            ))
        })
        .unwrap_or_else(|error| panic!("channel input failed: {error}"));
    assert!(fuse_search(inconsistent_document).is_err());
}

#[test]
fn compact_packet_serialization_keeps_explanation_without_query_or_source() {
    let packet = packet(
        SearchMode::Hybrid,
        SemanticReadiness::Ready,
        (
            channel(
                RetrievalChannel::Lexical,
                vec![lexical_candidate(
                    "a",
                    "src/a.rs",
                    (1, LEXICAL_PRIMARY_SCORE),
                )],
            ),
            channel(
                RetrievalChannel::Semantic,
                vec![candidate("a", "src/a.rs", (1, SEMANTIC_PRIMARY_SCORE))],
            ),
        ),
    );
    let serialized = serde_json::to_string(&packet)
        .unwrap_or_else(|error| panic!("packet serialization failed: {error}"));
    assert!(serialized.contains("\"requested_mode\":\"hybrid\""));
    assert!(serialized.contains("\"channel\":\"lexical\""));
    assert!(serialized.contains("\"language\":\"rust\""));
    assert!(serialized.contains("\"document_kind\":\"symbol\""));
    assert!(serialized.contains("\"raw_score\""));
    assert!(!serialized.contains("query"));
    assert!(!serialized.contains("source"));
}

#[test]
fn reranker_provenance_is_bounded_and_does_not_replace_rrf_evidence() {
    let semantic = channel(
        RetrievalChannel::Semantic,
        vec![candidate("a", "src/a.rs", (1, SEMANTIC_PRIMARY_SCORE))],
    );
    let report = RerankReport::applied("gte-modernbert-int8", 1)
        .unwrap_or_else(|error| panic!("rerank report failed: {error}"));
    let channels = RetrievalChannels::new()
        .with_channel(semantic)
        .unwrap_or_else(|error| panic!("semantic channel failed: {error}"))
        .with_rerank_report(report);
    let packet =
        fuse_search(input(SearchMode::Hybrid, SemanticReadiness::Ready).with_channels(channels))
            .unwrap_or_else(|error| panic!("fusion failed: {error}"));

    assert_eq!(packet.rerank_report().state(), RerankState::Applied);
    assert_eq!(packet.rerank_report().model(), Some("gte-modernbert-int8"));
    assert_eq!(packet.rerank_report().reranked_documents(), 1);
    assert_eq!(packet.items()[0].contributions()[0].rank(), 1);
    assert!(RerankReport::applied("", 1).is_err());
    assert!(RerankReport::applied("model", 0).is_err());
}

fn packet(
    mode: SearchMode,
    readiness: SemanticReadiness,
    channels: (ChannelResults, ChannelResults),
) -> cartograph_search::HybridSearchPacket {
    let (lexical, semantic) = channels;
    let input = input(mode, readiness)
        .with_channel(lexical)
        .and_then(|value| value.with_channel(semantic))
        .unwrap_or_else(|error| panic!("channels failed: {error}"));
    fuse_search(input).unwrap_or_else(|error| panic!("fusion failed: {error}"))
}

fn fused_with_order(
    lexical: Vec<ChannelCandidate>,
    semantic: Vec<ChannelCandidate>,
    semantic_first: bool,
) -> cartograph_search::HybridSearchPacket {
    let lexical = channel(RetrievalChannel::Lexical, lexical);
    let semantic = channel(RetrievalChannel::Semantic, semantic);
    let input = input(SearchMode::Hybrid, SemanticReadiness::Ready);
    let input = if semantic_first {
        input
            .with_channel(semantic)
            .and_then(|value| value.with_channel(lexical))
    } else {
        input
            .with_channel(lexical)
            .and_then(|value| value.with_channel(semantic))
    }
    .unwrap_or_else(|error| panic!("channels failed: {error}"));
    fuse_search(input).unwrap_or_else(|error| panic!("fusion failed: {error}"))
}

fn lexical_packet_with_limit(
    candidates: Vec<ChannelCandidate>,
    result_limit: u16,
) -> cartograph_search::HybridSearchPacket {
    HybridSearchInput::new(
        SearchMode::Deterministic,
        SemanticReadiness::NotConfigured,
        result_limit,
    )
    .and_then(|value| {
        value.with_channel(ChannelResults::new(RetrievalChannel::Lexical, candidates)?)
    })
    .and_then(fuse_search)
    .unwrap_or_else(|error| panic!("bounded lexical fusion failed: {error}"))
}

fn rank_orders() -> Vec<[&'static str; 4]> {
    let mut order = ["a", "b", "c", "z"];
    let mut output = Vec::new();
    collect_rank_orders(&mut order, 0, &mut output);
    output
}

fn collect_rank_orders(
    order: &mut [&'static str; 4],
    start: usize,
    output: &mut Vec<[&'static str; 4]>,
) {
    if start == order.len() {
        output.push(*order);
        return;
    }
    for index in start..order.len() {
        order.swap(start, index);
        collect_rank_orders(order, start + 1, output);
        order.swap(start, index);
    }
}

fn ranked_candidates(order: &[&str; 4], channel: RetrievalChannel) -> Vec<ChannelCandidate> {
    order
        .iter()
        .enumerate()
        .map(|(index, identifier)| {
            let rank = u16::try_from(index + 1)
                .unwrap_or_else(|error| panic!("fixture rank conversion failed: {error}"));
            let path = format!("src/{identifier}.rs");
            let score = 100.0 - f64::from(rank);
            match channel {
                RetrievalChannel::Lexical => lexical_candidate(identifier, &path, (rank, score)),
                RetrievalChannel::Semantic => candidate(identifier, &path, (rank, score)),
            }
        })
        .collect()
}

fn rank_of(order: &[&str; 4], identifier: &str) -> u16 {
    let index = order
        .iter()
        .position(|candidate| *candidate == identifier)
        .unwrap_or_else(|| panic!("identifier {identifier} was absent from rank fixture"));
    u16::try_from(index + 1)
        .unwrap_or_else(|error| panic!("fixture rank conversion failed: {error}"))
}

fn reciprocal_rank_oracle(rank: u16) -> f64 {
    1.0 / (RRF_OFFSET + f64::from(rank))
}

fn input(mode: SearchMode, readiness: SemanticReadiness) -> HybridSearchInput {
    HybridSearchInput::new(mode, readiness, RESULT_LIMIT)
        .unwrap_or_else(|error| panic!("hybrid input failed: {error}"))
}

fn channel(channel: RetrievalChannel, candidates: Vec<ChannelCandidate>) -> ChannelResults {
    ChannelResults::new(channel, candidates)
        .unwrap_or_else(|error| panic!("channel input failed: {error}"))
}

fn lexical_candidate(id: &str, path: &str, ranking: (u16, f64)) -> ChannelCandidate {
    candidate(id, path, ranking).with_lexical_components(vec![LexicalComponent::QualifiedName])
}

fn candidate(id: &str, path: &str, ranking: (u16, f64)) -> ChannelCandidate {
    let (rank, score) = ranking;
    ChannelCandidate::new(document(id, path), rank, score)
        .unwrap_or_else(|error| panic!("candidate failed: {error}"))
}

fn typed_candidate(
    id: &str,
    path: &str,
    document_kind: DocumentKind,
    symbol_kind: SymbolKind,
    ranking: (u16, f64),
) -> ChannelCandidate {
    let document = typed_document(id, path, document_kind, symbol_kind);
    ChannelCandidate::new(document, ranking.0, ranking.1)
        .unwrap_or_else(|error| panic!("typed candidate failed: {error}"))
}

fn typed_document(
    id: &str,
    path: &str,
    document_kind: DocumentKind,
    symbol_kind: SymbolKind,
) -> RetrievalDocument {
    let document_id = DocumentId::parse(document_uuid(id))
        .unwrap_or_else(|error| panic!("document id fixture failed: {error}"));
    let path = NormalizedPath::parse(path)
        .unwrap_or_else(|error| panic!("document path fixture failed: {error}"));
    RetrievalDocument::new(RetrievalDocumentInput {
        document_id,
        generation_id: primary_generation(),
        path,
        language: SourceLanguage::Rust,
        document_kind,
    })
    .with_symbol_kind(symbol_kind)
    .with_qualified_name(format!("symbol_{id}"))
    .unwrap_or_else(|error| panic!("qualified name fixture failed: {error}"))
}

fn document(id: &str, path: &str) -> RetrievalDocument {
    document_in_generation(id, path, primary_generation())
}

fn document_in_generation(id: &str, path: &str, generation_id: GenerationId) -> RetrievalDocument {
    let document_id = DocumentId::parse(document_uuid(id))
        .unwrap_or_else(|error| panic!("document id fixture failed: {error}"));
    let path = NormalizedPath::parse(path)
        .unwrap_or_else(|error| panic!("document path fixture failed: {error}"));
    RetrievalDocument::new(RetrievalDocumentInput {
        document_id,
        generation_id,
        path,
        language: SourceLanguage::Rust,
        document_kind: DocumentKind::Symbol,
    })
    .with_qualified_name(format!("symbol_{id}"))
    .unwrap_or_else(|error| panic!("qualified name fixture failed: {error}"))
}

fn primary_generation() -> GenerationId {
    generation("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
}

fn second_generation() -> GenerationId {
    generation("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
}

fn generation(value: &str) -> GenerationId {
    GenerationId::parse(value)
        .unwrap_or_else(|error| panic!("generation id fixture failed: {error}"))
}

fn document_uuid(id: &str) -> &str {
    match id {
        "a" => "11111111-1111-4111-8111-111111111111",
        "b" => "22222222-2222-4222-8222-222222222222",
        "c" => "33333333-3333-4333-8333-333333333333",
        "z" => "99999999-9999-4999-8999-999999999999",
        _ => panic!("unknown document fixture"),
    }
}

fn paths(packet: &cartograph_search::HybridSearchPacket) -> Vec<&str> {
    packet
        .items()
        .iter()
        .map(|item| item.document().path().as_str())
        .collect()
}
