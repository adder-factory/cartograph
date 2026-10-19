//! Field-aware native BM25 with deterministic scoring and bounded candidate memory.

use num_traits::ToPrimitive as _;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    DEFAULT_INTERACTIVE_SEARCH_TIMEOUT, DocumentId, Duration, GenerationId, SearchComponent,
    SearchFlavor, SearchHit, SearchQuery, StorageError, SymbolKind, validate_query,
};
use crate::{
    CanonicalSearchDocument, NativeDatabase,
    rocks::{GraphSnapshot, LexicalPosting, ReadBudget, RocksError},
};

struct Candidate {
    document: DocumentId,
    length: u32,
    frequencies: Vec<u32>,
    components: u8,
}

struct CorpusStatistics {
    frequencies: Vec<u32>,
    total_length: u64,
    documents: u32,
}

struct IndexedCorpus {
    statistics: CorpusStatistics,
    candidates: CandidateWorkspace,
    positions: BTreeMap<DocumentId, usize>,
}

const MAXIMUM_CANDIDATE_BYTES: usize = 32 * 1_048_576;
const CANDIDATE_ALLOCATION_OVERHEAD: usize = 64;

#[derive(Default)]
struct CandidateWorkspace {
    candidates: Vec<Candidate>,
    heap_bytes: usize,
}

fn workspace_limit() -> RocksError {
    RocksError::Limit {
        resource: "lexical_workspace_bytes",
    }
}

impl CandidateWorkspace {
    fn push(&mut self, candidate: Candidate) -> Result<(), RocksError> {
        let heap_bytes = candidate
            .frequencies
            .capacity()
            .checked_mul(size_of::<u32>())
            .and_then(|bytes| bytes.checked_add(candidate.document.as_str().len()))
            .and_then(|bytes| bytes.checked_add(CANDIDATE_ALLOCATION_OVERHEAD))
            .and_then(|bytes| bytes.checked_add(self.heap_bytes))
            .ok_or_else(workspace_limit)?;
        let capacity = if self.candidates.len() == self.candidates.capacity() {
            self.candidates
                .len()
                .checked_add(256)
                .ok_or_else(workspace_limit)?
        } else {
            self.candidates.capacity()
        };
        Self::check_capacity(capacity, heap_bytes)?;
        if capacity > self.candidates.capacity() {
            self.candidates
                .try_reserve_exact(capacity - self.candidates.len())
                .map_err(|_| workspace_limit())?;
            Self::check_capacity(self.candidates.capacity(), heap_bytes)?;
        }
        self.candidates.push(candidate);
        self.heap_bytes = heap_bytes;
        Ok(())
    }

    fn check_capacity(capacity: usize, heap_bytes: usize) -> Result<(), RocksError> {
        let bytes = capacity
            .checked_mul(size_of::<Candidate>())
            .and_then(|bytes| bytes.checked_add(heap_bytes))
            .ok_or_else(workspace_limit)?;
        if bytes > MAXIMUM_CANDIDATE_BYTES {
            return Err(workspace_limit());
        }
        Ok(())
    }
}

impl CorpusStatistics {
    fn record(&mut self, length: u32, frequencies: &[u32]) -> Result<(), RocksError> {
        self.documents = self.documents.checked_add(1).ok_or(RocksError::Format)?;
        self.total_length = self
            .total_length
            .checked_add(u64::from(length))
            .ok_or(RocksError::Format)?;
        for (df, count) in self.frequencies.iter_mut().zip(frequencies) {
            if *count != 0 {
                *df = df.checked_add(1).ok_or(RocksError::Format)?;
            }
        }
        Ok(())
    }
}

impl IndexedCorpus {
    fn record(
        &mut self,
        posting: LexicalPosting,
        flavor: SearchFlavor,
        term_index: usize,
    ) -> Result<(), RocksError> {
        let Some((length, frequency, components)) = posting_fields(&posting, flavor) else {
            return Ok(());
        };
        if frequency == 0 {
            return Ok(());
        }
        self.statistics.frequencies[term_index] = self.statistics.frequencies[term_index]
            .checked_add(1)
            .ok_or(RocksError::Format)?;
        let index = if let Some(index) = self.positions.get(&posting.document) {
            *index
        } else {
            // Account for the B-tree's keys/nodes before insertion, in
            // addition to the vector and candidate-owned allocations.
            self.candidates.heap_bytes = self
                .candidates
                .heap_bytes
                .checked_add(128 + posting.document.as_str().len())
                .ok_or_else(workspace_limit)?;
            let index = self.candidates.candidates.len();
            let document = posting.document.clone();
            self.candidates.push(Candidate {
                document: posting.document,
                length,
                frequencies: vec![0; self.statistics.frequencies.len()],
                components: 0,
            })?;
            self.positions.insert(document, index);
            index
        };
        let candidate = self
            .candidates
            .candidates
            .get_mut(index)
            .ok_or(RocksError::Format)?;
        if candidate.length != length || candidate.frequencies[term_index] != 0 {
            return Err(RocksError::Format);
        }
        candidate.frequencies[term_index] = frequency;
        candidate.components |= components;
        Ok(())
    }
}

fn posting_fields(posting: &LexicalPosting, flavor: SearchFlavor) -> Option<(u32, u32, u8)> {
    match flavor {
        SearchFlavor::All => Some((
            posting.lengths.into_iter().fold(0_u32, u32::saturating_add),
            posting
                .frequencies
                .into_iter()
                .fold(0_u32, u32::saturating_add),
            posting
                .frequencies
                .iter()
                .enumerate()
                .fold(0_u8, |bits, (field, frequency)| {
                    bits | if *frequency == 0 { 0 } else { 1 << field }
                }),
        )),
        SearchFlavor::Name if posting.symbol => {
            Some((posting.lengths[0], posting.frequencies[0], 1))
        }
        SearchFlavor::Intent => Some((posting.lengths[2], posting.frequencies[2], 4)),
        _ => None,
    }
}

impl GraphSnapshot<'_, '_> {
    /// Execute the shared native BM25 scorer against this immutable snapshot.
    /// The boolean reports additional matches beyond the requested output.
    /// # Errors
    /// Rejects invalid bounds, corrupt postings, cancellation, and read exhaustion.
    pub fn search_code(
        &self,
        query: &str,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<SearchHit>, bool), RocksError> {
        if query.is_empty()
            || query.len() > 4096
            || query.contains('\0')
            || !(1..=100).contains(&limit)
        {
            return Err(RocksError::InvalidInput {
                field: "lexical_query",
            });
        }
        let mut hits = lexical(
            self,
            budget,
            self.generation_id(),
            query,
            limit + 1,
            SearchFlavor::All,
        )?;
        let truncated = hits.len() > usize::from(limit);
        hits.truncate(usize::from(limit));
        Ok((hits, truncated))
    }
}

impl NativeDatabase {
    /// Search current code-aware and natural-language fields with native BM25.
    /// # Errors
    /// Rejects invalid query bounds, changed generation, or exhausted read budgets.
    pub async fn search_current_code(
        &self,
        input: SearchQuery,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.search_current_code_bounded(input, DEFAULT_INTERACTIVE_SEARCH_TIMEOUT)
            .await
    }

    /// Execute native BM25 under an explicit finite deadline.
    /// # Errors
    /// Rejects invalid input, changed generation, or native request limits.
    pub async fn search_current_code_bounded(
        &self,
        input: SearchQuery,
        timeout: Duration,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.native_search(input, timeout, SearchFlavor::All).await
    }

    /// Search only declaration-name tokens.
    /// # Errors
    /// Rejects invalid input, changed generation, or exhausted native budgets.
    pub async fn search_current_names(
        &self,
        input: SearchQuery,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.native_search(
            input,
            DEFAULT_INTERACTIVE_SEARCH_TIMEOUT,
            SearchFlavor::Name,
        )
        .await
    }

    /// Search only documentation and intent text.
    /// # Errors
    /// Rejects invalid input, changed generation, or exhausted native budgets.
    pub async fn search_current_intent(
        &self,
        input: SearchQuery,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.native_search(
            input,
            DEFAULT_INTERACTIVE_SEARCH_TIMEOUT,
            SearchFlavor::Intent,
        )
        .await
    }

    /// Search declaration-name tokens with bounded Damerau-Levenshtein distance.
    /// # Errors
    /// Rejects invalid edit distance, query input, or exhausted native budgets.
    pub async fn search_current_names_fuzzy(
        &self,
        input: SearchQuery,
        edit_distance: u8,
    ) -> Result<Vec<SearchHit>, StorageError> {
        if !(1..=2).contains(&edit_distance) {
            return Err(StorageError::InvalidInput {
                field: "fuzzy_edit_distance",
            });
        }
        self.native_search(
            input,
            DEFAULT_INTERACTIVE_SEARCH_TIMEOUT,
            SearchFlavor::FuzzyName(edit_distance),
        )
        .await
    }

    async fn native_search(
        &self,
        input: SearchQuery,
        timeout: Duration,
        flavor: SearchFlavor,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.require_project(&input.project_id)?;
        validate_query(&input)?;
        let generation = input.expected_generation_id.clone();
        self.graph_with_budget(
            &input.expected_generation_id,
            timeout,
            192 * 1_048_576,
            move |graph, budget| {
                lexical(
                    graph,
                    budget,
                    &generation,
                    &input.query,
                    input.limit,
                    flavor,
                )
            },
        )
        .await
    }
}

fn lexical(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    generation: &GenerationId,
    query: &str,
    limit: u16,
    flavor: SearchFlavor,
) -> Result<Vec<SearchHit>, RocksError> {
    let mut terms = BTreeSet::new();
    tokens(query, budget, |token, _| {
        terms.insert(token);
        Ok(())
    })?;
    if terms.is_empty() || terms.len() > 32 {
        return Err(RocksError::Limit {
            resource: "lexical_query_terms",
        });
    }
    let terms = terms.into_iter().collect::<Vec<_>>();
    let (statistics, candidates) = if let Some(statistics) = graph.lexical_statistics(budget)?
        && statistics.scan_documents == 0
        && !matches!(flavor, SearchFlavor::FuzzyName(_))
    {
        indexed_corpus(graph, budget, &terms, flavor, &statistics)?
    } else {
        scan_corpus(graph, budget, &terms, flavor)?
    };
    // Scan metadata once, retaining only explicitly bounded term counts and
    // identities. Drop that workspace before hydrating the final top-K from
    // the same immutable snapshot. Statistics do not depend on the limit.
    let ranked = rank_candidates(candidates, &statistics, usize::from(limit), budget)?;

    ranked
        .into_iter()
        .map(|(score, candidate)| {
            let document = graph
                .document(&candidate.document, budget)?
                .ok_or(RocksError::Format)?;
            let symbol_kind = document
                .symbol_id()
                .map(|id| graph.symbol(id, budget))
                .transpose()?
                .flatten()
                .and_then(|symbol| SymbolKind::from_stable_str(&symbol.symbol_kind));
            Ok(SearchHit {
                document_id: candidate.document,
                generation_id: generation.clone(),
                file_id: document.file_id().cloned(),
                symbol_id: document.symbol_id().cloned(),
                path: document.path().to_owned(),
                language: document.language().to_owned(),
                document_kind: document.kind().as_str().to_owned(),
                qualified_name: document.qualified_name().to_owned(),
                symbol_kind,
                score,
                components: [
                    (1, SearchComponent::QualifiedName),
                    (2, SearchComponent::Code),
                    (4, SearchComponent::NaturalText),
                ]
                .into_iter()
                .filter(|(bit, _)| candidate.components & bit != 0)
                .map(|(_, component)| component)
                .collect(),
            })
        })
        .collect()
}

fn indexed_corpus(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    terms: &[String],
    flavor: SearchFlavor,
    corpus: &crate::rocks::LexicalStatistics,
) -> Result<(CorpusStatistics, CandidateWorkspace), RocksError> {
    let flavor_index = match flavor {
        SearchFlavor::All => 0,
        SearchFlavor::Name => 1,
        SearchFlavor::Intent => 2,
        SearchFlavor::FuzzyName(_) => return Err(RocksError::Format),
    };
    let mut indexed = IndexedCorpus {
        statistics: CorpusStatistics {
            frequencies: vec![0; terms.len()],
            total_length: corpus.total_lengths[flavor_index],
            documents: corpus.documents,
        },
        candidates: CandidateWorkspace::default(),
        positions: BTreeMap::new(),
    };
    for (term_index, term) in terms.iter().enumerate() {
        let mut cursor = None;
        loop {
            let (postings, next) = graph.lexical_postings(term, cursor.as_ref(), 64, budget)?;
            for posting in postings {
                budget.checkpoint()?;
                indexed.record(posting, flavor, term_index)?;
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok((indexed.statistics, indexed.candidates))
}

fn scan_corpus(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    terms: &[String],
    flavor: SearchFlavor,
) -> Result<(CorpusStatistics, CandidateWorkspace), RocksError> {
    let mut statistics = CorpusStatistics {
        frequencies: vec![0_u32; terms.len()],
        total_length: 0,
        documents: 0,
    };
    let mut candidates = CandidateWorkspace::default();
    let mut cursor = None;
    loop {
        let (rows, next) = graph.documents(cursor.as_ref(), 64, budget)?;
        for document in rows {
            let (length, frequencies, components) = counts(&document, terms, flavor, budget)?;
            statistics.record(length, &frequencies)?;
            if frequencies.iter().any(|count| *count != 0) {
                candidates.push(Candidate {
                    document: document.document_id().clone(),
                    length,
                    frequencies,
                    components,
                })?;
            }
        }
        cursor = next;
        if cursor.is_none() {
            return Ok((statistics, candidates));
        }
    }
}

fn rank_candidates(
    workspace: CandidateWorkspace,
    statistics: &CorpusStatistics,
    limit: usize,
    budget: &ReadBudget,
) -> Result<Vec<(f64, Candidate)>, RocksError> {
    budget.checkpoint()?;
    let average = statistics.total_length.to_f64().ok_or(RocksError::Format)?
        / f64::from(statistics.documents.max(1));
    let mut ranked = Vec::with_capacity(limit);
    for candidate in workspace.candidates {
        budget.checkpoint()?;
        let score = bm25(
            &candidate,
            &statistics.frequencies,
            statistics.documents,
            average,
        );
        retain_candidate(&mut ranked, (score, candidate), limit);
    }
    Ok(ranked)
}

fn bm25(candidate: &Candidate, document_frequencies: &[u32], documents: u32, average: f64) -> f64 {
    candidate
        .frequencies
        .iter()
        .zip(document_frequencies)
        .map(|(frequency, df)| {
            let frequency = f64::from(*frequency);
            let idf =
                (1.0 + (f64::from(documents) - f64::from(*df) + 0.5) / (f64::from(*df) + 0.5)).ln();
            idf * frequency * 2.2
                / (frequency + 1.2 * (0.25 + 0.75 * f64::from(candidate.length) / average.max(1.0)))
        })
        .sum()
}

fn retain_candidate(ranked: &mut Vec<(f64, Candidate)>, item: (f64, Candidate), limit: usize) {
    crate::retrieval::native::retain(ranked, item, limit, |(a, left), (b, right)| {
        b.total_cmp(a).then(left.document.cmp(&right.document))
    });
}

fn counts(
    document: &CanonicalSearchDocument,
    terms: &[String],
    flavor: SearchFlavor,
    budget: &ReadBudget,
) -> Result<(u32, Vec<u32>, u8), RocksError> {
    let (mut length, mut components) = (0_u32, 0_u8);
    let mut frequencies = vec![0_u32; terms.len()];
    for (text, weight, bit) in [
        (document.qualified_name(), 4_u32, 1_u8),
        (document.code(), 1, 2),
        (document.natural_text(), 2, 4),
    ] {
        if !includes_field(document, flavor, bit) {
            continue;
        }
        tokens(text, budget, |token, budget| {
            length = length.saturating_add(weight);
            for (index, term) in terms.iter().enumerate() {
                if token_matches(&token, term, flavor, budget)? {
                    frequencies[index] = frequencies[index].saturating_add(weight);
                    components |= bit;
                }
            }
            Ok(())
        })?;
    }
    Ok((length, frequencies, components))
}

fn includes_field(document: &CanonicalSearchDocument, flavor: SearchFlavor, bit: u8) -> bool {
    match flavor {
        SearchFlavor::All => true,
        SearchFlavor::Name | SearchFlavor::FuzzyName(_) => {
            bit == 1 && document.symbol_id().is_some()
        }
        SearchFlavor::Intent => bit == 4,
    }
}

fn token_matches(
    token: &str,
    term: &str,
    flavor: SearchFlavor,
    budget: &ReadBudget,
) -> Result<bool, RocksError> {
    match flavor {
        SearchFlavor::FuzzyName(distance) => within_distance(token, term, distance, budget),
        _ => Ok(token == term),
    }
}

pub(crate) fn tokens(
    text: &str,
    budget: &ReadBudget,
    mut visit: impl FnMut(String, &ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    budget.checkpoint()?;
    let (mut start, mut checked) = (0, 0);
    for (index, character) in text.char_indices() {
        if index - checked >= 4096 {
            budget.checkpoint()?;
            checked = index;
        }
        if !character.is_alphanumeric() && character != '_' {
            word_tokens(&text[start..index], budget, &mut visit)?;
            start = index + character.len_utf8();
        }
    }
    word_tokens(&text[start..], budget, &mut visit)?;
    budget.checkpoint()
}

fn word_tokens(
    word: &str,
    budget: &ReadBudget,
    visit: &mut impl FnMut(String, &ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    if word.is_empty() || word.len() > 256 {
        return Ok(());
    }
    visit(word.to_lowercase(), budget)?;
    if word.contains('_') {
        for part in word.split('_').filter(|part| !part.is_empty()) {
            visit(part.to_lowercase(), budget)?;
        }
    } else {
        let (mut start, mut previous_lower) = (0, false);
        for (index, character) in word.char_indices() {
            if index > 0 && character.is_uppercase() && previous_lower {
                visit(word[start..index].to_lowercase(), budget)?;
                start = index;
            }
            previous_lower = character.is_lowercase();
        }
        if start > 0 {
            visit(word[start..].to_lowercase(), budget)?;
        }
    }
    Ok(())
}

fn within_distance(
    left: &str,
    right: &str,
    maximum: u8,
    budget: &ReadBudget,
) -> Result<bool, RocksError> {
    budget.checkpoint()?;
    let (left, right) = (
        left.chars().collect::<Vec<_>>(),
        right.chars().collect::<Vec<_>>(),
    );
    let maximum = usize::from(maximum);
    if left.len().abs_diff(right.len()) > maximum {
        return Ok(false);
    }
    let infinity = maximum + 1;
    let mut previous_previous = vec![infinity; right.len() + 1];
    let mut previous = (0..=right.len())
        .map(|i| i.min(infinity))
        .collect::<Vec<_>>();
    let mut row = vec![infinity; right.len() + 1];
    for (i0, a) in left.iter().enumerate() {
        if i0 % 16 == 0 {
            budget.checkpoint()?;
        }
        let i = i0 + 1;
        let start = i.saturating_sub(maximum).max(1);
        let end = (i + maximum).min(right.len());
        row[0] = i.min(infinity);
        if start > 1 {
            row[start - 1] = infinity;
        }
        for j in start..=end {
            let b = right[j - 1];
            row[j] = (previous[j] + 1)
                .min(row[j - 1] + 1)
                .min(previous[j - 1] + usize::from(*a != b))
                .min(infinity);
            if i > 1 && j > 1 && *a == right[j - 2] && left[i - 2] == b {
                row[j] = row[j].min(previous_previous[j - 2] + 1);
            }
        }
        if end < right.len() {
            row[end + 1] = infinity;
        }
        std::mem::swap(&mut previous_previous, &mut previous);
        std::mem::swap(&mut previous, &mut row);
    }
    Ok(previous[right.len()] <= maximum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn budget() -> Result<ReadBudget, RocksError> {
        ReadBudget::new(1, 1, Duration::from_secs(30))
    }

    #[test]
    fn field_weighted_bm25_matches_independent_corpus_statistics() {
        // These literal scores were independently evaluated from the frozen
        // real-source corpus statistics, without using the native scorer.
        let document_frequencies = [60, 11, 14, 21];
        let average = 348_835.0 / 6_362.0;
        let candidate = |length, frequency| Candidate {
            document: DocumentId::from_uuid_v8([1; 16]),
            length,
            frequencies: vec![frequency; 4],
            components: 1,
        };
        let corpus = bm25(&candidate(28, 4), &document_frequencies, 6_362, average);
        let score = bm25(&candidate(42, 5), &document_frequencies, 6_362, average);
        assert!((corpus - 42.054_701_281_997_91).abs() < 1e-10);
        assert!((score - 41.774_553_035_347_694).abs() < 1e-10);
        assert!(corpus > score);
    }

    #[test]
    fn common_term_top_k_stays_bounded_beyond_the_old_match_limit() {
        let mut ranked = Vec::new();
        // Fixed equal scores must retain the smallest stable document identity,
        // independently of reverse admission and the number of matching rows.
        for id in (1_u64..=140_000).rev() {
            let mut bytes = [0; 16];
            bytes[8..].copy_from_slice(&id.to_be_bytes());
            retain_candidate(
                &mut ranked,
                (
                    1.0,
                    Candidate {
                        document: DocumentId::from_uuid_v8(bytes),
                        length: 1,
                        frequencies: vec![1],
                        components: 1,
                    },
                ),
                3,
            );
            assert!(ranked.len() <= 3);
        }
        let mut first = [0; 16];
        first[8..].copy_from_slice(&1_u64.to_be_bytes());
        assert_eq!(ranked[0].1.document, DocumentId::from_uuid_v8(first));
        assert_eq!(ranked.len(), 3);
    }

    fn compact_candidate(id: u64, terms: usize) -> Candidate {
        let mut bytes = [0; 16];
        bytes[8..].copy_from_slice(&id.to_be_bytes());
        Candidate {
            document: DocumentId::from_uuid_v8(bytes),
            length: 1,
            frequencies: vec![1; terms],
            components: 1,
        }
    }

    #[test]
    fn compact_common_term_workspace_preserves_large_corpus_order() -> TestResult {
        let mut workspace = CandidateWorkspace::default();
        for id in (1_u64..=140_000).rev() {
            workspace.push(compact_candidate(id, 1))?;
        }
        let statistics = CorpusStatistics {
            frequencies: vec![140_000],
            total_length: 140_000,
            documents: 140_000,
        };
        let ranked = rank_candidates(workspace, &statistics, 3, &budget()?)?;
        assert_eq!(ranked.len(), 3);
        for (item, id) in ranked.iter().zip(1_u64..=3) {
            assert_eq!(item.1.document, compact_candidate(id, 1).document);
        }
        Ok(())
    }

    #[test]
    fn dense_candidate_workspace_fails_explicitly_before_unbounded_growth() {
        let mut workspace = CandidateWorkspace::default();
        let failure =
            (1_u64..=200_000).find_map(|id| workspace.push(compact_candidate(id, 32)).err());
        assert_eq!(failure, Some(workspace_limit()));
        assert!(workspace.candidates.len() > 10_000);
        assert!(workspace.candidates.len() < 200_000);
        assert!(
            CandidateWorkspace::check_capacity(
                workspace.candidates.capacity(),
                workspace.heap_bytes
            )
            .is_ok()
        );
    }

    #[test]
    fn candidate_ranking_keeps_the_original_cancellation_control() -> TestResult {
        let request = crate::rocks::StorageRequest::new(Duration::from_secs(30))?;
        let budget = budget()?.with_control(request.clone());
        let mut workspace = CandidateWorkspace::default();
        workspace.push(compact_candidate(1, 1))?;
        let statistics = CorpusStatistics {
            frequencies: vec![1],
            total_length: 1,
            documents: 1,
        };
        request.cancel();
        assert_eq!(
            rank_candidates(workspace, &statistics, 1, &budget).err(),
            Some(RocksError::Cancelled)
        );
        Ok(())
    }

    #[test]
    fn banded_fuzzy_distance_preserves_unicode_edits_and_transpositions() -> TestResult {
        let budget = budget()?;
        for (left, right, maximum, expected) in [
            ("parse", "prase", 1, true),
            ("café", "cafe", 1, true),
            ("", "é", 1, true),
            ("é", "", 1, true),
            ("abc", "axy", 1, false),
            ("abc", "axy", 2, true),
            ("abcdefgh", "badcefgh", 2, true),
            ("abcd", "badc", 1, false),
        ] {
            assert_eq!(
                within_distance(left, right, maximum, &budget)?,
                expected,
                "{left} / {right}"
            );
        }
        Ok(())
    }

    #[test]
    fn cpu_work_observes_cancellation_without_another_native_read() -> TestResult {
        let request = crate::rocks::StorageRequest::new(Duration::from_secs(30))?;
        let budget = budget()?.with_control(request.clone());
        let mut calls = 0;
        let result = tokens(&"parse ".repeat(1_000_000), &budget, |_, _| {
            calls += 1;
            request.cancel();
            Ok(())
        });
        assert_eq!(result, Err(RocksError::Cancelled));
        assert!(calls < 1000);
        assert_eq!(
            within_distance(&"a".repeat(256), &"b".repeat(256), 2, &budget),
            Err(RocksError::Cancelled)
        );
        Ok(())
    }
}
