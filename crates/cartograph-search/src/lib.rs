//! Deterministic, non-LLM retrieval over Cartograph's published PostgreSQL graph.

mod engine;
mod hybrid;
mod intent;
mod model;
mod packet;
mod qualified;
mod review;
mod traversal;

pub use cartograph_db::{EntryPointBucket, SimilarSymbolHit, SimilarSymbolsResult};
pub use engine::{DeterministicRetriever, FuzzyNameRequest, GenerationLexicalRequest};
pub use hybrid::{
    ChannelCandidate, ChannelContribution, ChannelResults, FusedSearchItem, HybridSearchInput,
    HybridSearchPacket, LexicalComponent, RerankReport, RerankState, RetrievalAbstention,
    RetrievalChannel, RetrievalChannels, RetrievalDocument, RetrievalDocumentInput,
    RetrievalExecution, RetrievalFallback, RetrievalPreference, SearchMode, SemanticReadiness,
    fuse_search,
};
pub use intent::{ContextGraphDirection, TaskIntent};
pub use model::{
    AffectedTest, AffectedTestsResult, BidirectionalTraversalResult, CONTEXT_ANCHOR_MAXIMUM_BYTES,
    CONTEXT_QUERY_MAXIMUM_BYTES, ContextAbstention, ContextAnchor, ContextBudget,
    ContextBudgetInput, ContextPacket, ContextRequest, ContextRequestOptions, DecisionRankEvidence,
    DecisionRankState, EditCandidate, EditCandidateBasis, EditCandidateSet, EntryPointsQuery,
    EntryPointsResult, EvidenceItem, EvidenceReason, ExactPathQuery, ExactPathResult,
    ExactTextQuery, FileInventoryQuery, FileInventoryResult, GenerationEvidence, GraphEvidence,
    GraphPathRequest, GraphPathRequestInput, GraphPathResult, GraphPathStep, IndexFreshness,
    LexicalQuery, ReferenceEvidence, ReferenceSpanPrecision, RetrievalConfidence, RetrievalError,
    ReviewAbstention, ReviewBudget, ReviewBudgetInput, ReviewPacket, ReviewRequest,
    ReviewRequestOptions, ReviewTruncation, SimilarRequest, SourceRangeQuery,
    SourceRangeQueryInput, SourceRangeResult, TraversalBudget, TraversalDirection, TraversalHop,
    TraversalNode, TraversalRequest, TraversalResult, WORKING_TREE_OVERLAY_MAXIMUM_EXCERPT_BYTES,
    WORKING_TREE_OVERLAY_MAXIMUM_FILES, WORKING_TREE_OVERLAY_MAXIMUM_RESULTS,
    WORKING_TREE_OVERLAY_MAXIMUM_SOURCE_BYTES, WorkingTreeChangeKind, WorkingTreeEvidence,
    WorkingTreeEvidenceInput, WorkingTreeOverlay, WorkingTreeOverlayInput,
    WorkingTreeOverlayStatus,
};
pub use qualified::{
    CentralityComparator, CentralityFilter, ParsedQualifiedQuery, QualifiedSort,
    parse_qualified_query,
};
pub use traversal::is_test_path;

#[cfg(test)]
use model::{GraphEvidenceFixture, evidence_fixture, graph_evidence_fixture};
#[cfg(test)]
use model::{
    ReferenceEvidenceFixture, SearchEvidenceFixture, enrich_search_evidence_fixture,
    reference_evidence_fixture,
};
#[cfg(test)]
use packet::{PacketAssembly, assemble_packet};
#[cfg(test)]
use review::{ReviewAssembly, assemble_review_packet};
#[cfg(test)]
use traversal::{FrontierInput, GraphArc, GraphArcFixture, expand_frontier, strongest_arc};

#[cfg(test)]
fn fixture_generation() -> GenerationEvidence {
    let generation_id =
        match cartograph_domain::GenerationId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa") {
            Ok(value) => value,
            Err(error) => panic!("fixture generation id is invalid: {error}"),
        };
    GenerationEvidence::new(generation_id, 1)
}

#[cfg(test)]
fn fixture_retrieval() -> HybridSearchPacket {
    let input = HybridSearchInput::new(
        SearchMode::Deterministic,
        SemanticReadiness::NotConfigured,
        1,
    )
    .unwrap_or_else(|error| panic!("fixture retrieval input failed: {error}"));
    fuse_search(input).unwrap_or_else(|error| panic!("fixture retrieval failed: {error}"))
}

#[cfg(test)]
mod contract_tests {
    use cartograph_domain::{DocumentKind, NormalizedPath, ProjectId, SourceLanguage, SymbolId};

    use super::*;

    const VALID_DEPTH: u8 = 2;
    const EXCESSIVE_DEPTH: u8 = 51;
    const VALID_NODE_LIMIT: u16 = 50;
    const SAMPLE_NODE_LIMIT: u16 = 10;
    const EXCESSIVE_NODE_LIMIT: u16 = 501;
    const PACKET_EVIDENCE_LIMIT: u16 = 10;
    const FIRST_EDGE_CONFIDENCE: f32 = 0.8;
    const SECOND_EDGE_CONFIDENCE: f32 = 0.9;
    const REFERENCE_EDGE_CONFIDENCE: f32 = 0.7;
    const GRAPH_SITE_COUNT: u32 = 1;
    const STRONG_GRAPH_SITE_COUNT: u32 = 3;
    const WEAK_HIGH_MULTIPLICITY_COUNT: u32 = 9;
    const REPRESENTED_REFERENCE_SITES: u64 = 7;
    const ADDITIONAL_REFERENCE_SITES: u64 = 4;
    const TOTAL_REFERENCE_SITES: u64 = REPRESENTED_REFERENCE_SITES + ADDITIONAL_REFERENCE_SITES;
    const REFERENCE_ROW_COUNT: usize = 3;
    const FIRST_REFERENCE_ID: u64 = 101;
    const SECOND_REFERENCE_ID: u64 = 102;
    const COARSE_REFERENCE_ID: u64 = 103;
    const FIRST_REFERENCE_START: u64 = 12;
    const FIRST_REFERENCE_END: u64 = 19;
    const SECOND_REFERENCE_START: u64 = 22;
    const SECOND_REFERENCE_END: u64 = 29;
    const COARSE_REFERENCE_START: u64 = 40;
    const COARSE_REFERENCE_END: u64 = 41;
    const REFERENCE_OWNER_SYMBOL_ID: &str = "44444444-4444-4444-8444-444444444444";
    const REFERENCE_TARGET_SYMBOL_ID: &str = "55555555-5555-4555-8555-555555555555";
    const SERIALIZED_CONFIDENCE_TOLERANCE: f64 = 0.000_001;

    fn project_id() -> ProjectId {
        match ProjectId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa") {
            Ok(value) => value,
            Err(error) => panic!("fixture project id is invalid: {error}"),
        }
    }

    fn symbol_id(value: &str) -> SymbolId {
        match SymbolId::parse(value) {
            Ok(value) => value,
            Err(error) => panic!("fixture symbol id is invalid: {error}"),
        }
    }

    fn ranked_evidence(path: &str, reason: EvidenceReason, rank: u16) -> EvidenceItem {
        enrich_search_evidence_fixture(
            evidence_fixture(path, path, reason),
            SearchEvidenceFixture {
                file_id: None,
                symbol_id: None,
                language: SourceLanguage::Rust,
                document_kind: DocumentKind::Symbol,
                fused_rank: Some(rank),
                reciprocal_rank_score: Some(1.0 / f64::from(rank)),
            },
        )
    }

    fn ranked_symbol_evidence(
        path: &str,
        qualified_name: &str,
        reason: EvidenceReason,
        rank: u16,
        symbol: &str,
    ) -> EvidenceItem {
        enrich_search_evidence_fixture(
            evidence_fixture(path, qualified_name, reason),
            SearchEvidenceFixture {
                file_id: None,
                symbol_id: Some(symbol_id(symbol)),
                language: SourceLanguage::TypeScript,
                document_kind: DocumentKind::Symbol,
                fused_rank: Some(rank),
                reciprocal_rank_score: Some(1.0 / f64::from(rank)),
            },
        )
    }

    fn reference_evidence_rows(owner: SymbolId, target: SymbolId) -> Vec<EvidenceItem> {
        let first = reference_evidence_fixture(ReferenceEvidenceFixture {
            reference_id: FIRST_REFERENCE_ID,
            path: "src/a_call.ts",
            qualified_name: "shared_target",
            owner_symbol_id: Some(owner.clone()),
            target_symbol_id: Some(target.clone()),
            start_byte: FIRST_REFERENCE_START,
            end_byte: FIRST_REFERENCE_END,
            span_precision: ReferenceSpanPrecision::Exact,
            confidence: FIRST_EDGE_CONFIDENCE,
            provenance: "resolved_fixture",
            represented_site_count: REPRESENTED_REFERENCE_SITES,
        });
        let second = reference_evidence_fixture(ReferenceEvidenceFixture {
            reference_id: SECOND_REFERENCE_ID,
            path: "src/b_call.ts",
            qualified_name: "shared_target",
            owner_symbol_id: Some(owner.clone()),
            target_symbol_id: Some(target),
            start_byte: SECOND_REFERENCE_START,
            end_byte: SECOND_REFERENCE_END,
            span_precision: ReferenceSpanPrecision::Exact,
            confidence: SECOND_EDGE_CONFIDENCE,
            provenance: "resolved_fixture",
            represented_site_count: ADDITIONAL_REFERENCE_SITES,
        });
        let coarse = reference_evidence_fixture(ReferenceEvidenceFixture {
            reference_id: COARSE_REFERENCE_ID,
            path: "legacy/coarse_call.ts",
            qualified_name: "legacy_target",
            owner_symbol_id: Some(owner),
            target_symbol_id: None,
            start_byte: COARSE_REFERENCE_START,
            end_byte: COARSE_REFERENCE_END,
            span_precision: ReferenceSpanPrecision::CoarseOwner,
            confidence: REFERENCE_EDGE_CONFIDENCE,
            provenance: "legacy_fixture",
            represented_site_count: GRAPH_SITE_COUNT.into(),
        });
        vec![coarse, second, first.clone(), first]
    }

    #[test]
    fn traversal_budget_rejects_unbounded_work() {
        assert!(TraversalBudget::new(0, SAMPLE_NODE_LIMIT).is_err());
        assert!(TraversalBudget::new(VALID_DEPTH, 0).is_err());
        assert!(TraversalBudget::new(EXCESSIVE_DEPTH, SAMPLE_NODE_LIMIT).is_err());
        assert!(TraversalBudget::new(VALID_DEPTH, EXCESSIVE_NODE_LIMIT).is_err());
        assert!(TraversalBudget::new(VALID_DEPTH, VALID_NODE_LIMIT).is_ok());
    }

    #[test]
    fn similar_requests_reject_unbounded_results_and_non_finite_scores() {
        let source = symbol_id("11111111-1111-4111-8111-111111111111");
        assert!(SimilarRequest::new(project_id(), source.clone(), 1).is_ok());
        assert!(SimilarRequest::new(project_id(), source.clone(), 0).is_err());
        assert!(SimilarRequest::new(project_id(), source.clone(), 51).is_err());
        let request = SimilarRequest::new(project_id(), source, 5)
            .unwrap_or_else(|error| panic!("similar request failed: {error}"));
        assert!(request.clone().with_minimum_score(0.0).is_ok());
        assert!(request.clone().with_minimum_score(1.0).is_ok());
        assert!(request.clone().with_minimum_score(-0.1).is_err());
        assert!(request.clone().with_minimum_score(1.1).is_err());
        assert!(request.with_minimum_score(f64::NAN).is_err());
    }

    #[test]
    fn traversal_roots_preserve_caller_relevance_order_while_deduplicating() {
        let high_id = symbol_id("ffffffff-ffff-4fff-8fff-ffffffffffff");
        let low_id = symbol_id("11111111-1111-4111-8111-111111111111");
        let request = TraversalRequest::new(
            project_id(),
            [high_id.clone(), low_id.clone(), high_id.clone()],
            TraversalBudget::new(VALID_DEPTH, VALID_NODE_LIMIT)
                .unwrap_or_else(|error| panic!("traversal budget failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("traversal request failed: {error}"));
        assert_eq!(request.roots(), &[high_id, low_id]);
    }

    #[test]
    fn context_request_debug_redacts_query_and_anchor_text() {
        let request = match ContextRequest::new(
            project_id(),
            "credential shaped private query",
            ContextRequestOptions::new(IndexFreshness::Current, ContextBudget::default()),
        ) {
            Ok(request) => request,
            Err(error) => panic!("context request fixture was rejected: {error}"),
        };
        let request =
            match request.with_anchor(ContextAnchor::ExactName("private_anchor_name".to_owned())) {
                Ok(request) => request,
                Err(error) => panic!("context anchor fixture was rejected: {error}"),
            };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("credential shaped private query"));
        assert!(!rendered.contains("private_anchor_name"));
        assert!(rendered.contains("query: <redacted>"));
    }

    #[test]
    fn affected_test_detection_is_conservative_and_platform_independent() {
        assert!(is_test_path("tests/search/retrieval.rs"));
        assert!(is_test_path("src/search/__tests__/packet.ts"));
        assert!(is_test_path("src/search/packet.test.ts"));
        assert!(is_test_path("src/search/packet_spec.rb"));
        assert!(!is_test_path("src/contest/parser.ts"));
        assert!(!is_test_path("src/testing_helpers.ts"));
    }

    #[test]
    fn graph_frontier_is_deduplicated_and_stably_ordered() {
        let root = symbol_id("11111111-1111-4111-8111-111111111111");
        let first = symbol_id("22222222-2222-4222-8222-222222222222");
        let second = symbol_id("33333333-3333-4333-8333-333333333333");
        let arcs = vec![
            GraphArc::fixture(GraphArcFixture {
                source: root.clone(),
                target: second.clone(),
                edge_kind: "calls",
                confidence: FIRST_EDGE_CONFIDENCE,
                site_count: GRAPH_SITE_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: root.clone(),
                target: first.clone(),
                edge_kind: "calls",
                confidence: SECOND_EDGE_CONFIDENCE,
                site_count: GRAPH_SITE_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: root.clone(),
                target: first.clone(),
                edge_kind: "references",
                confidence: REFERENCE_EDGE_CONFIDENCE,
                site_count: GRAPH_SITE_COUNT,
            }),
        ];
        let roots = [root];
        let expansion = expand_frontier(FrontierInput {
            frontier: &roots,
            arcs: &arcs,
            direction: TraversalDirection::Outgoing,
            max_new_nodes: usize::from(SAMPLE_NODE_LIMIT),
        });
        assert_eq!(expansion.next, vec![first, second]);
        assert_eq!(expansion.arcs.len(), 3);
    }

    #[test]
    fn graph_frontier_fairly_admits_relevant_roots_before_extra_fanout() {
        let first_root = symbol_id("ffffffff-ffff-4fff-8fff-ffffffffffff");
        let second_root = symbol_id("11111111-1111-4111-8111-111111111111");
        let first_best = symbol_id("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
        let first_extra = symbol_id("dddddddd-dddd-4ddd-8ddd-dddddddddddd");
        let second_best = symbol_id("22222222-2222-4222-8222-222222222222");
        let arcs = vec![
            GraphArc::fixture(GraphArcFixture {
                source: first_root.clone(),
                target: first_extra,
                edge_kind: "calls",
                confidence: FIRST_EDGE_CONFIDENCE,
                site_count: GRAPH_SITE_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: first_root.clone(),
                target: first_best.clone(),
                edge_kind: "calls",
                confidence: SECOND_EDGE_CONFIDENCE,
                site_count: GRAPH_SITE_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: second_root.clone(),
                target: second_best.clone(),
                edge_kind: "calls",
                confidence: REFERENCE_EDGE_CONFIDENCE,
                site_count: GRAPH_SITE_COUNT,
            }),
        ];
        let roots = [first_root, second_root];
        let expansion = expand_frontier(FrontierInput {
            frontier: &roots,
            arcs: &arcs,
            direction: TraversalDirection::Outgoing,
            max_new_nodes: 2,
        });
        assert_eq!(expansion.next, vec![first_best, second_best]);
        assert!(expansion.truncated);
    }

    #[test]
    fn strongest_graph_hop_is_independent_of_database_row_order() {
        let first_root = symbol_id("11111111-1111-4111-8111-111111111111");
        let second_root = symbol_id("22222222-2222-4222-8222-222222222222");
        let target = symbol_id("33333333-3333-4333-8333-333333333333");
        let mut arcs = vec![
            GraphArc::fixture(GraphArcFixture {
                source: second_root.clone(),
                target: target.clone(),
                edge_kind: "references",
                confidence: REFERENCE_EDGE_CONFIDENCE,
                site_count: WEAK_HIGH_MULTIPLICITY_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: second_root.clone(),
                target: target.clone(),
                edge_kind: "imports",
                confidence: SECOND_EDGE_CONFIDENCE,
                site_count: STRONG_GRAPH_SITE_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: second_root,
                target: target.clone(),
                edge_kind: "calls",
                confidence: SECOND_EDGE_CONFIDENCE,
                site_count: STRONG_GRAPH_SITE_COUNT,
            }),
            GraphArc::fixture(GraphArcFixture {
                source: first_root.clone(),
                target: target.clone(),
                edge_kind: "calls",
                confidence: SECOND_EDGE_CONFIDENCE,
                site_count: STRONG_GRAPH_SITE_COUNT,
            }),
        ];
        let selected = strongest_arc(&arcs, &target, TraversalDirection::Outgoing)
            .unwrap_or_else(|| panic!("strongest graph arc was not selected"))
            .clone();
        assert_eq!(selected.source, first_root);
        assert_eq!(selected.edge_kind, "calls");
        assert_eq!(selected.site_count, STRONG_GRAPH_SITE_COUNT);

        arcs.reverse();
        let reversed = strongest_arc(&arcs, &target, TraversalDirection::Outgoing)
            .unwrap_or_else(|| panic!("reversed graph arc was not selected"));
        assert_eq!(reversed, &selected);
    }

    #[test]
    fn graph_evidence_serializes_edge_strength_provenance_and_multiplicity() {
        let from = symbol_id("11111111-1111-4111-8111-111111111111");
        let to = symbol_id("22222222-2222-4222-8222-222222222222");
        let evidence = graph_evidence_fixture(
            "src/graph_target.rs",
            "graph_target",
            GraphEvidenceFixture {
                from_symbol_id: from.clone(),
                to_symbol_id: to.clone(),
                depth: VALID_DEPTH,
                edge_kind: "calls",
                confidence: SECOND_EDGE_CONFIDENCE,
                provenance: "extractor_fixture",
                site_count: u64::from(GRAPH_SITE_COUNT),
            },
        );
        let graph = evidence
            .graph()
            .unwrap_or_else(|| panic!("graph fixture lost typed edge evidence"));
        assert_eq!(graph.from_symbol_id(), &from);
        assert_eq!(graph.to_symbol_id(), &to);
        assert_eq!(graph.site_count(), u64::from(GRAPH_SITE_COUNT));
        assert!(
            (graph.confidence() - SECOND_EDGE_CONFIDENCE).abs() <= f32::EPSILON,
            "graph confidence changed: {}",
            graph.confidence()
        );
        assert_eq!(graph.provenance(), "extractor_fixture");
        let serialized = serde_json::to_string(&evidence)
            .unwrap_or_else(|error| panic!("graph evidence did not serialize: {error}"));
        for field in [
            "from_symbol_id",
            "to_symbol_id",
            "edge_kind",
            "confidence",
            "provenance",
            "site_count",
        ] {
            assert!(serialized.contains(field));
        }
    }

    #[test]
    fn packet_abstains_without_evidence_and_orders_evidence_deterministically() {
        let empty = assemble_packet(PacketAssembly {
            task: "change missing project",
            generation: None,
            intent: TaskIntent::ChangePlanning,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Unknown,
            retrieval: fixture_retrieval(),
            evidence: Vec::new(),
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        assert_eq!(empty.confidence(), RetrievalConfidence::None);
        assert_eq!(
            empty.abstention(),
            Some(ContextAbstention::NoCurrentGeneration)
        );
        let serialized = match serde_json::to_string(&empty) {
            Ok(serialized) => serialized,
            Err(error) => panic!("packet did not serialize: {error}"),
        };
        assert!(serialized.contains("\"abstention\":\"no_current_generation\""));
        assert!(serialized.contains("\"freshness\":\"unknown\""));

        let mut evidence = vec![
            evidence_fixture("src/z.ts", "z", EvidenceReason::Bm25),
            evidence_fixture("src/a.ts", "a", EvidenceReason::ExactName),
            evidence_fixture("src/b.ts", "b", EvidenceReason::ExactReference),
        ];
        evidence.reverse();
        let packet = assemble_packet(PacketAssembly {
            task: "change exact symbol",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ChangePlanning,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence,
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        let paths = packet
            .evidence()
            .iter()
            .map(EvidenceItem::path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["src/a.ts", "src/b.ts", "src/z.ts"]);
        assert_eq!(packet.confidence(), RetrievalConfidence::High);
        assert_eq!(packet.abstention(), None);
    }

    #[test]
    fn decision_relevance_reorders_only_retrieval_candidates_and_validates_input() {
        let packet = assemble_packet(PacketAssembly {
            task: "explain the lookup",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ArchitectureSurvey,
            graph_direction: None,
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![
                evidence_fixture("src/anchor.ts", "anchor", EvidenceReason::ExactName),
                ranked_symbol_evidence(
                    "src/a.ts",
                    "alpha",
                    EvidenceReason::Bm25,
                    1,
                    "11111111-1111-4111-8111-111111111111",
                ),
                ranked_symbol_evidence(
                    "src/b.ts",
                    "beta",
                    EvidenceReason::Bm25,
                    2,
                    "22222222-2222-4222-8222-222222222222",
                ),
                ranked_symbol_evidence(
                    "src/c.ts",
                    "gamma",
                    EvidenceReason::Semantic,
                    3,
                    "33333333-3333-4333-8333-333333333333",
                ),
            ],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        let names = |packet: &ContextPacket| {
            packet
                .evidence()
                .iter()
                .map(|item| item.qualified_name().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&packet), ["anchor", "alpha", "beta", "gamma"]);
        let eligible = packet.retrieval_evidence_indices(24);
        assert_eq!(eligible, [1, 2, 3]);
        assert_eq!(packet.retrieval_evidence_indices(2), [1, 2]);
        let ranked = packet
            .clone()
            .with_decision_relevance("explain the lookup", &[(1, 0.1), (2, 0.9), (3, 0.9)])
            .unwrap_or_else(|error| panic!("valid ranking rejected: {error}"));
        // The anchor keeps its position; equal probabilities keep packet order.
        assert_eq!(names(&ranked), ["anchor", "beta", "gamma", "alpha"]);
        assert_eq!(ranked.evidence()[0].decision_relevance(), None);
        assert_eq!(ranked.evidence()[3].decision_relevance(), Some(0.1));
        let edit_paths = ranked
            .edit_candidates()
            .candidates()
            .iter()
            .map(|candidate| (candidate.path().to_owned(), candidate.basis()))
            .collect::<Vec<_>>();
        assert_eq!(
            edit_paths,
            [
                ("src/b.ts".to_owned(), EditCandidateBasis::DecisionRelevance),
                ("src/c.ts".to_owned(), EditCandidateBasis::DecisionRelevance),
            ],
            "relevant judged files lead the edit surface; irrelevant ones are omitted"
        );
        for invalid in [
            vec![(1, 0.5), (1, 0.4)],
            vec![(9, 0.5)],
            vec![(0, 0.5)],
            vec![(1, f64::NAN)],
            vec![(1, 1.5)],
        ] {
            assert!(matches!(
                packet
                    .clone()
                    .with_decision_relevance("explain the lookup", &invalid),
                Err(RetrievalError::InvalidInput {
                    field: "decision_relevance"
                })
            ));
        }
    }

    #[test]
    fn packet_edit_candidates_prefer_concentrated_task_terms_over_generic_hits() {
        let packet = assemble_packet(PacketAssembly {
            task: "Fix the watcher event gate so an empty file path never triggers incremental sync",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ChangePlanning,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![
                ranked_symbol_evidence(
                    "src/watcher.ts",
                    "watcherHandleFileEvent",
                    EvidenceReason::Bm25,
                    1,
                    "11111111-1111-4111-8111-111111111111",
                ),
                ranked_symbol_evidence(
                    "src/sync.ts",
                    "runSync",
                    EvidenceReason::Bm25,
                    4,
                    "22222222-2222-4222-8222-222222222222",
                ),
                ranked_symbol_evidence(
                    "src/postgres-maintenance.ts",
                    "syncPostgresGraph",
                    EvidenceReason::Bm25,
                    7,
                    "33333333-3333-4333-8333-333333333333",
                ),
            ],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        assert_eq!(packet.edit_candidates().candidates().len(), 1);
        let candidate = &packet.edit_candidates().candidates()[0];
        assert_eq!(candidate.path(), "src/watcher.ts");
        assert_eq!(candidate.basis(), EditCandidateBasis::TaskTerms);
        assert_eq!(candidate.matched_term_count(), 2);
        assert_eq!(candidate.best_rank(), Some(1));
        assert_eq!(candidate.qualified_names(), &["watcherHandleFileEvent"]);
        assert!(!packet.edit_candidates().truncated());
    }

    #[test]
    fn packet_edit_candidates_honor_exact_anchors_and_abstain_without_term_overlap() {
        let exact = assemble_packet(PacketAssembly {
            task: "change watcher behavior",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ChangePlanning,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![
                ranked_symbol_evidence(
                    "src/watcher.ts",
                    "watcherHandleFileEvent",
                    EvidenceReason::Bm25,
                    1,
                    "44444444-4444-4444-8444-444444444444",
                ),
                ranked_symbol_evidence(
                    "src/explicit.ts",
                    "explicitOverride",
                    EvidenceReason::ExactName,
                    8,
                    "55555555-5555-4555-8555-555555555555",
                ),
            ],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        assert_eq!(exact.edit_candidates().candidates().len(), 1);
        assert_eq!(
            exact.edit_candidates().candidates()[0].path(),
            "src/explicit.ts"
        );
        assert_eq!(
            exact.edit_candidates().candidates()[0].basis(),
            EditCandidateBasis::ExactAnchor
        );

        let absent = assemble_packet(PacketAssembly {
            task: "mobile push APNS retry",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ChangePlanning,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![ranked_symbol_evidence(
                "src/watcher.ts",
                "watcherHandleFileEvent",
                EvidenceReason::Bm25,
                1,
                "66666666-6666-4666-8666-666666666666",
            )],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        assert_eq!(absent.edit_candidates().candidates(), []);
    }

    #[test]
    fn packet_bounds_exact_then_fused_rank_without_path_reordering() {
        let packet = assemble_packet(PacketAssembly {
            task: "trace ranked evidence",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ImplementationTrace,
            graph_direction: Some(ContextGraphDirection::Callees),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![
                ranked_evidence("src/a_rank_2.rs", EvidenceReason::Bm25, 2),
                ranked_evidence("src/z_rank_1.rs", EvidenceReason::Semantic, 1),
                evidence_fixture("src/z_exact.rs", "exact", EvidenceReason::ExactName),
            ],
            affected_tests: Vec::new(),
            evidence_limit: 2,
            truncated: false,
        });
        assert_eq!(
            packet
                .evidence()
                .iter()
                .map(EvidenceItem::path)
                .collect::<Vec<_>>(),
            vec!["src/z_exact.rs", "src/z_rank_1.rs"]
        );
        assert!(packet.truncated());
        assert_eq!(packet.evidence()[1].fused_rank(), Some(1));
        assert_eq!(packet.evidence()[1].language(), Some(SourceLanguage::Rust));
        assert_eq!(
            packet.evidence()[1].document_kind(),
            Some(DocumentKind::Symbol)
        );
    }

    #[test]
    fn duplicate_symbol_evidence_keeps_metadata_from_the_stronger_fused_hit() {
        let shared_symbol = symbol_id("99999999-9999-4999-8999-999999999999");
        let strongest = enrich_search_evidence_fixture(
            evidence_fixture("tests/shared.py", "shared", EvidenceReason::Semantic),
            SearchEvidenceFixture {
                file_id: None,
                symbol_id: Some(shared_symbol.clone()),
                language: SourceLanguage::Python,
                document_kind: DocumentKind::Test,
                fused_rank: Some(1),
                reciprocal_rank_score: Some(1.0),
            },
        );
        let weaker = enrich_search_evidence_fixture(
            evidence_fixture("tests/shared.py", "shared", EvidenceReason::Bm25),
            SearchEvidenceFixture {
                file_id: None,
                symbol_id: Some(shared_symbol),
                language: SourceLanguage::Rust,
                document_kind: DocumentKind::Symbol,
                fused_rank: Some(2),
                reciprocal_rank_score: Some(0.5),
            },
        );
        let packet = assemble_packet(PacketAssembly {
            task: "select shared tests",
            generation: Some(fixture_generation()),
            intent: TaskIntent::TestSelection,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![strongest, weaker],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        assert_eq!(packet.evidence().len(), 1);
        assert_eq!(packet.evidence()[0].fused_rank(), Some(1));
        assert_eq!(
            packet.evidence()[0].language(),
            Some(SourceLanguage::Python)
        );
        assert_eq!(
            packet.evidence()[0].document_kind(),
            Some(DocumentKind::Test)
        );
    }

    #[test]
    fn reference_evidence_preserves_rows_precision_links_and_site_totals() {
        let owner = symbol_id(REFERENCE_OWNER_SYMBOL_ID);
        let target = symbol_id(REFERENCE_TARGET_SYMBOL_ID);
        let packet = assemble_packet(PacketAssembly {
            task: "change exact reference",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ChangePlanning,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: reference_evidence_rows(owner, target),
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });

        assert_eq!(packet.evidence().len(), REFERENCE_ROW_COUNT);
        let references = packet
            .evidence()
            .iter()
            .map(|item| {
                item.reference()
                    .unwrap_or_else(|| panic!("reference fixture lost metadata"))
            })
            .collect::<Vec<_>>();
        assert_eq!(references[0].reference_id(), SECOND_REFERENCE_ID);
        assert_eq!(references[1].reference_id(), FIRST_REFERENCE_ID);
        assert_eq!(references[2].reference_id(), COARSE_REFERENCE_ID);
        assert_eq!(
            references[0].target_symbol_id(),
            references[1].target_symbol_id()
        );
        assert!(references[2].target_symbol_id().is_none());
        assert!(references[2].owner_symbol_id().is_some());
        assert_eq!(
            references[2].span_precision(),
            ReferenceSpanPrecision::CoarseOwner
        );
        assert_eq!(
            packet.evidence()[2].reasons(),
            &[EvidenceReason::CoarseReference]
        );
        let represented_sites = references
            .iter()
            .map(|reference| reference.represented_site_count())
            .sum::<u64>();
        assert_eq!(
            represented_sites,
            TOTAL_REFERENCE_SITES + u64::from(GRAPH_SITE_COUNT)
        );
        let serialized = serde_json::to_string(&packet)
            .unwrap_or_else(|error| panic!("reference packet did not serialize: {error}"));
        for field in [
            "reference_id",
            "owner_symbol_id",
            "target_symbol_id",
            "start_byte",
            "end_byte",
            "span_precision",
            "confidence",
            "provenance",
            "represented_site_count",
        ] {
            assert!(serialized.contains(field));
        }
        assert!(serialized.contains("coarse_owner"));
        assert!(serialized.contains("coarse_reference"));
        let serialized = serde_json::from_str::<serde_json::Value>(&serialized)
            .unwrap_or_else(|error| panic!("reference packet JSON was invalid: {error}"));
        let reference = &serialized["evidence"][0]["reference"];
        assert_eq!(reference["reference_id"], SECOND_REFERENCE_ID);
        assert_eq!(reference["owner_symbol_id"], REFERENCE_OWNER_SYMBOL_ID);
        assert_eq!(reference["target_symbol_id"], REFERENCE_TARGET_SYMBOL_ID);
        assert_eq!(reference["start_byte"], SECOND_REFERENCE_START);
        assert_eq!(reference["end_byte"], SECOND_REFERENCE_END);
        assert_eq!(reference["span_precision"], "exact");
        assert_eq!(reference["provenance"], "resolved_fixture");
        assert_eq!(
            reference["represented_site_count"],
            ADDITIONAL_REFERENCE_SITES
        );
        let confidence = reference["confidence"]
            .as_f64()
            .unwrap_or_else(|| panic!("serialized reference confidence was not numeric"));
        assert!(
            (confidence - f64::from(SECOND_EDGE_CONFIDENCE)).abs()
                <= SERIALIZED_CONFIDENCE_TOLERANCE
        );
    }

    #[test]
    fn packet_support_contracts_preserve_empty_states_bounds_and_redaction() {
        let generation = fixture_generation();
        assert_eq!(generation.sequence(), 1);

        let clean = WorkingTreeOverlay::clean();
        assert_eq!(clean.status(), WorkingTreeOverlayStatus::Clean);
        assert_eq!(clean.files(), []);
        assert!(!clean.truncated());

        let unavailable = WorkingTreeOverlay::unavailable();
        assert_eq!(unavailable.status(), WorkingTreeOverlayStatus::Unavailable);
        assert_eq!(unavailable.files(), []);
        assert!(!unavailable.truncated());

        let no_matches = WorkingTreeOverlay::completed(WorkingTreeOverlayInput {
            changed_file_count: 2,
            considered_file_count: 1,
            unreadable_file_count: 1,
            files: Vec::new(),
            truncated: true,
        })
        .unwrap_or_else(|error| panic!("valid empty overlay failed: {error}"));
        assert_eq!(no_matches.status(), WorkingTreeOverlayStatus::NoMatches);
        assert_eq!(no_matches.files(), []);
        assert!(no_matches.truncated());

        let traversal = TraversalBudget::new(2, SAMPLE_NODE_LIMIT)
            .unwrap_or_else(|error| panic!("review traversal failed: {error}"));
        let budget = ReviewBudget::new(ReviewBudgetInput {
            symbols_per_file: 7,
            root_limit: 3,
            traversal,
            evidence_limit: 5,
            affected_test_limit: 6,
        })
        .unwrap_or_else(|error| panic!("review budget failed: {error}"));
        assert_eq!(budget.symbols_per_file(), 7);
        assert_eq!(budget.root_limit(), 3);
        assert_eq!(budget.traversal(), traversal);
        assert_eq!(budget.evidence_limit(), 5);
        assert_eq!(budget.affected_test_limit(), 6);

        let path = NormalizedPath::parse("src/private-service.rs")
            .unwrap_or_else(|error| panic!("review path fixture failed: {error}"));
        let project = project_id();
        let request = ReviewRequest::new(
            Some(project.clone()),
            [path.clone(), path],
            ReviewRequestOptions::new(IndexFreshness::Current, budget)
                .with_changed_files_truncated(true),
        )
        .unwrap_or_else(|error| panic!("review request failed: {error}"));
        assert_eq!(request.project_id(), Some(&project));
        assert_eq!(request.changed_paths().len(), 1);
        assert_eq!(request.freshness(), IndexFreshness::Current);
        assert_eq!(request.budget(), budget);
        assert!(request.changed_files_truncated());
        let debug = format!("{request:?}");
        assert!(debug.contains("<redacted:1>"));
        assert!(!debug.contains("private-service"));

        let truncation = ReviewTruncation::default()
            .with_graph_truncation(true)
            .with_affected_tests_truncation(true)
            .with_evidence(true);
        assert!(truncation.graph());
        assert!(truncation.affected_tests());
        assert!(truncation.evidence());
        assert!(truncation.any());
    }

    #[test]
    fn intent_budgets_and_packet_provenance_are_typed_and_bounded() {
        let lookup = ContextBudget::for_intent(TaskIntent::SymbolLookup);
        let change = ContextBudget::for_intent(TaskIntent::ChangePlanning);
        let tests = ContextBudget::for_intent(TaskIntent::TestSelection);
        assert!(lookup.candidate_limit() < change.candidate_limit());
        assert!(lookup.traversal().max_depth() < change.traversal().max_depth());
        assert!(tests.affected_test_limit() > change.affected_test_limit());

        let packet = assemble_packet(PacketAssembly {
            task: "diagnose failure",
            generation: Some(fixture_generation()),
            intent: TaskIntent::ErrorDiagnosis,
            graph_direction: Some(ContextGraphDirection::Callers),
            freshness: IndexFreshness::Current,
            retrieval: fixture_retrieval(),
            evidence: vec![evidence_fixture(
                "src/failure.rs",
                "failure",
                EvidenceReason::Bm25,
            )],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncated: false,
        });
        assert_eq!(packet.intent(), TaskIntent::ErrorDiagnosis);
        assert_eq!(
            packet.graph_direction(),
            Some(ContextGraphDirection::Callers)
        );
        assert_eq!(
            packet.working_tree_overlay().status(),
            WorkingTreeOverlayStatus::NotChecked
        );
        let live = WorkingTreeEvidence::new(WorkingTreeEvidenceInput {
            path: NormalizedPath::parse("src/failure.rs")
                .unwrap_or_else(|error| panic!("overlay path failed: {error}")),
            change_kind: WorkingTreeChangeKind::Modified,
            content_digest: cartograph_domain::ContentDigest::from_bytes([7_u8; 32]),
            start_line: 1,
            end_line: 2,
            excerpt: "fn failure() {}\n".to_owned(),
            matched_terms: vec!["failure".to_owned()],
        })
        .unwrap_or_else(|error| panic!("overlay evidence failed: {error}"));
        let overlay = WorkingTreeOverlay::completed(WorkingTreeOverlayInput {
            changed_file_count: 1,
            considered_file_count: 1,
            unreadable_file_count: 0,
            files: vec![live],
            truncated: false,
        })
        .unwrap_or_else(|error| panic!("overlay failed: {error}"));
        let packet = packet.with_working_tree_overlay(overlay);
        assert_eq!(
            packet.working_tree_overlay().status(),
            WorkingTreeOverlayStatus::Used
        );
        let serialized = serde_json::to_string(&packet)
            .unwrap_or_else(|error| panic!("intent packet did not serialize: {error}"));
        assert!(serialized.contains("\"intent\":\"error_diagnosis\""));
        assert!(serialized.contains("\"graph_direction\":\"callers\""));
        assert!(serialized.contains("\"status\":\"used\""));
    }

    #[test]
    fn code_search_query_expansion_bridges_natural_language_to_identifier_terms() {
        let bm25 = LexicalQuery::for_code_search(
            "How are generation-local BM25 tables checked before a search runs?",
            20,
        )
        .unwrap_or_else(|error| panic!("BM25 query expansion failed: {error}"));
        for term in [
            "generation_search_relation",
            "relation",
            "require",
            "validate",
        ] {
            assert!(bm25.query().contains(term), "missing expansion: {term}");
        }

        let freshness = LexicalQuery::for_code_search(
            "How does freshness decide whether a live checkout matches the current generation?",
            20,
        )
        .unwrap_or_else(|error| panic!("freshness query expansion failed: {error}"));
        for term in ["project_status", "source_revision", "scan_source", "digest"] {
            assert!(
                freshness.query().contains(term),
                "missing expansion: {term}",
            );
        }

        let bounded = LexicalQuery::for_code_search("x".repeat(CONTEXT_QUERY_MAXIMUM_BYTES), 20)
            .unwrap_or_else(|error| panic!("bounded query expansion failed: {error}"));
        assert_eq!(bounded.query().len(), CONTEXT_QUERY_MAXIMUM_BYTES);
    }

    #[test]
    fn review_packet_exposes_freshness_abstention_and_per_stage_truncation() {
        let path = NormalizedPath::parse("src/service.rs")
            .unwrap_or_else(|error| panic!("review path fixture failed: {error}"));
        let no_generation = assemble_review_packet(ReviewAssembly {
            generation: None,
            freshness: IndexFreshness::Unknown,
            changed_file_count: 1,
            indexed_changed_files: Vec::new(),
            evidence: Vec::new(),
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncation: ReviewTruncation::default(),
        });
        assert_eq!(
            no_generation.abstention(),
            Some(ReviewAbstention::NoCurrentGeneration)
        );
        assert_eq!(no_generation.confidence(), RetrievalConfidence::None);

        let no_changes = assemble_review_packet(ReviewAssembly {
            generation: Some(fixture_generation()),
            freshness: IndexFreshness::Current,
            changed_file_count: 0,
            indexed_changed_files: Vec::new(),
            evidence: Vec::new(),
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncation: ReviewTruncation::default(),
        });
        assert_eq!(
            no_changes.abstention(),
            Some(ReviewAbstention::NoChangedFiles)
        );

        let stale = assemble_review_packet(ReviewAssembly {
            generation: Some(fixture_generation()),
            freshness: IndexFreshness::Stale,
            changed_file_count: 1,
            indexed_changed_files: vec![path],
            evidence: vec![evidence_fixture(
                "src/service.rs",
                "service",
                EvidenceReason::ExactPath,
            )],
            affected_tests: Vec::new(),
            evidence_limit: PACKET_EVIDENCE_LIMIT,
            truncation: ReviewTruncation::default()
                .with_changed_files_truncation(true)
                .with_symbol_roots_truncation(true),
        });
        assert_eq!(stale.abstention(), Some(ReviewAbstention::StaleIndex));
        assert_eq!(stale.confidence(), RetrievalConfidence::Low);
        assert!(stale.truncation().changed_files());
        assert!(stale.truncation().symbol_roots());
        assert!(stale.truncation().any());
    }

    #[test]
    fn file_inventory_and_source_range_requests_reject_unbounded_inputs() {
        assert!(FileInventoryQuery::new(0).is_err());
        assert!(FileInventoryQuery::new(501).is_err());
        assert!(FileInventoryQuery::new(500).is_ok());
        assert!(EntryPointsQuery::new(0).is_err());
        assert!(EntryPointsQuery::new(201).is_err());
        let entry_points = EntryPointsQuery::new(200)
            .unwrap_or_else(|error| panic!("entry-point fixture failed: {error}"))
            .with_bucket(EntryPointBucket::PublicExports);
        assert_eq!(entry_points.bucket(), Some(EntryPointBucket::PublicExports));
        let path = NormalizedPath::parse("src/service.rs")
            .unwrap_or_else(|error| panic!("range path fixture failed: {error}"));
        let query = |path, start_line, end_line, limit| {
            SourceRangeQuery::new(SourceRangeQueryInput {
                path,
                start_line,
                end_line,
                limit,
            })
        };
        assert!(query(path.clone(), 0, 1, 20).is_err());
        assert!(query(path.clone(), 2, 1, 20).is_err());
        assert!(query(path.clone(), 1, 100_001, 20).is_err());
        assert!(query(path.clone(), 1, 1, 0).is_err());
        assert!(query(path, 1, 1, 200).is_ok());
    }
}
