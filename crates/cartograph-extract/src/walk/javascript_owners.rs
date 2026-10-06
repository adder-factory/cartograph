//! Bounded interval lookup for the owners of JavaScript-family value reads.

use std::{collections::BTreeMap, mem::size_of};

use cartograph_domain::{SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::ExtractError;

use super::{AstVisitBudget, ExtractionBuilder};

/// One symbol interval in start-byte order. The median of each slice is its
/// tree root; `maximum_end` covers that entire subtree.
struct OwnerInterval {
    start: u64,
    end: u64,
    ordinal: usize,
    maximum_end: u64,
}

/// Built once after declarations, then reused by both value-read passes and
/// embedded-program replays. A changed symbol count rebuilds the index.
pub(super) struct OwnerIndex {
    intervals: Vec<OwnerInterval>,
    symbol_count: usize,
    /// Construction and every lookup share a bounded, cancellation-aware budget.
    budget: AstVisitBudget<0>,
}

/// The smallest containing symbol, preserving declaration order on equal spans.
pub(super) fn owner_for_node(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<SymbolId>, ExtractError> {
    builder.context.ensure_active()?;
    let mut index = match builder.javascript.owners.take() {
        Some(index) if index.symbol_count == builder.facts.symbols.len() => index,
        _ => OwnerIndex::build(builder)?,
    };
    let start = u64::try_from(node.start_byte()).map_err(|_| ExtractError::InvalidSpan)?;
    let end = u64::try_from(node.end_byte()).map_err(|_| ExtractError::InvalidSpan)?;
    let found = index.find(builder, (start, end));
    builder.javascript.owners = Some(index);
    Ok(found?
        .and_then(|ordinal| builder.facts.symbols.get(ordinal))
        .map(|symbol| symbol.id.clone())
        .or_else(|| builder.embedded.module_owner(&builder.owners)))
}

impl OwnerIndex {
    fn build(builder: &mut ExtractionBuilder<'_, '_>) -> Result<Self, ExtractError> {
        let symbol_count = builder.facts.symbols.len();
        let mut budget = AstVisitBudget::default();
        let mut ordered = BTreeMap::new();
        for ordinal in 0..symbol_count {
            budget.observe(builder, 0)?;
            let symbol = &builder.facts.symbols[ordinal];
            if matches!(symbol.kind, SymbolKind::File | SymbolKind::Import) {
                continue;
            }
            let start = symbol.span.start_byte();
            let end = symbol.span.end_byte();
            // Account for the ordered map's nodes and the retained vector.
            let bytes =
                u64::try_from(size_of::<OwnerInterval>()).map_err(|_| ExtractError::OutputLimit)?;
            builder.context.budget.reserve_working_bytes(bytes * 4)?;
            ordered.insert(
                (start, ordinal),
                OwnerInterval {
                    start,
                    end,
                    ordinal,
                    maximum_end: end,
                },
            );
        }
        let mut intervals = Vec::new();
        intervals
            .try_reserve_exact(ordered.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        intervals.extend(ordered.into_values());
        let mut index = Self {
            intervals,
            symbol_count,
            budget,
        };
        index.fill_maximum_end(builder, (0, index.intervals.len()))?;
        Ok(index)
    }

    /// Balanced construction recurses only logarithmically in the admitted
    /// interval count; every retained interval spends a work-budget unit.
    fn fill_maximum_end(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        (start, end): (usize, usize),
    ) -> Result<u64, ExtractError> {
        if start == end {
            return Ok(0);
        }
        self.budget.observe(builder, 0)?;
        let middle = start + (end - start) / 2;
        let left = self.fill_maximum_end(builder, (start, middle))?;
        let right = self.fill_maximum_end(builder, (middle + 1, end))?;
        let maximum = self.intervals[middle].end.max(left).max(right);
        self.intervals[middle].maximum_end = maximum;
        Ok(maximum)
    }

    fn find(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        (start, end): (u64, u64),
    ) -> Result<Option<usize>, ExtractError> {
        let mut pending = vec![(0, self.intervals.len())];
        let mut best = None;
        while let Some((first, last)) = pending.pop() {
            if first == last {
                continue;
            }
            self.budget.observe(builder, 0)?;
            let middle = first + (last - first) / 2;
            let interval = &self.intervals[middle];
            if interval.maximum_end < end || self.intervals[first].start > start {
                continue;
            }
            if interval.start <= start && end <= interval.end {
                let candidate = (
                    interval.end.saturating_sub(interval.start),
                    interval.ordinal,
                );
                best = Some(best.map_or(candidate, |previous| candidate.min(previous)));
            }
            pending
                .try_reserve(2)
                .map_err(|_| ExtractError::OutputLimit)?;
            pending.push((first, middle));
            if interval.start <= start {
                pending.push((middle + 1, last));
            }
        }
        Ok(best.map(|(_, ordinal)| ordinal))
    }
}
#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use cartograph_domain::ReferenceKind;
    use tree_sitter::Parser;

    use crate::{DEFAULT_MAXIMUM_AST_DEPTH, NativeGrammar, SourceLimits, SourceSnapshot};

    use super::{
        super::{ExtractionBuilder, javascript_reads, prepare_extraction},
        OwnerIndex,
    };

    #[test]
    fn binding_tables_reuse_owner_intervals_with_many_declarations() {
        const REFERENCES: usize = 8_192;
        let mut declarations = String::new();
        for index in 0..20_000 {
            writeln!(declarations, "function ordinary{index}() {{}}")
                .unwrap_or_else(|error| panic!("owner fixture formatting failed: {error}"));
        }
        let source = format!(
            "import {{ handler }} from './m';\n{declarations}const table = [{}];",
            "handler, ".repeat(REFERENCES)
        );
        let limits = SourceLimits::new(source.len())
            .unwrap_or_else(|error| panic!("owner fixture limit failed: {error}"));
        let snapshot = SourceSnapshot::from_bytes("src/large-table.ts", source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("owner fixture snapshot failed: {error}"));
        let mut parser = Parser::new();
        parser
            .set_language(&NativeGrammar::TypeScript.language())
            .unwrap_or_else(|error| panic!("owner fixture grammar failed: {error}"));
        let tree = parser
            .parse(snapshot.source(), None)
            .unwrap_or_else(|| panic!("owner fixture parsing failed"));
        let root = tree.root_node();
        let mut cancelled = || false;
        let mut builder =
            ExtractionBuilder::new(&snapshot, DEFAULT_MAXIMUM_AST_DEPTH, &mut cancelled)
                .unwrap_or_else(|error| panic!("owner fixture builder failed: {error}"));
        prepare_extraction(&mut builder, root)
            .and_then(|()| builder.visit(root, 0))
            .unwrap_or_else(|error| panic!("owner fixture declarations failed: {error}"));
        let index = OwnerIndex::build(&mut builder)
            .unwrap_or_else(|error| panic!("owner index construction failed: {error}"));
        let construction_visits = index.budget.visits;
        assert!(construction_visits <= 2 * builder.facts.symbols.len());
        // Fewer than 2^15 intervals give a balanced search at most 15 levels.
        assert!(index.intervals.len() < (1 << 15));
        builder.javascript.owners = Some(index);

        javascript_reads::enrich_binding_tables(&mut builder, root)
            .unwrap_or_else(|error| panic!("binding-table enrichment failed: {error}"));
        let index = builder
            .javascript
            .owners
            .as_ref()
            .unwrap_or_else(|| panic!("binding-table enrichment lost its owner index"));
        let lookup_visits = index.budget.visits - construction_visits;
        assert!(
            lookup_visits >= REFERENCES,
            "every owner lookup must charge work"
        );
        assert!(
            lookup_visits <= 30 * REFERENCES,
            "owner lookup work must stay logarithmic"
        );

        let table = builder
            .facts
            .symbols
            .iter()
            .find(|symbol| symbol.name == "table")
            .unwrap_or_else(|| panic!("binding table symbol is missing"));
        let references = builder
            .facts
            .references
            .iter()
            .filter(|reference| {
                reference.owner.as_ref() == Some(&table.id)
                    && reference.kind == ReferenceKind::References
            })
            .collect::<Vec<_>>();
        assert_eq!(references.len(), REFERENCES);
        assert!(
            references
                .iter()
                .all(|reference| reference.name == "handler")
        );
    }
}
