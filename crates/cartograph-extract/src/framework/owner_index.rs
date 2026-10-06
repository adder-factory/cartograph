//! Indexed version of the framework builder's original nearest-owner rule.

use std::collections::{BTreeMap, BTreeSet};

use crate::{ExtractError, ExtractedSymbol};

const CANCELLATION_INTERVAL_ITEMS: usize = 256;
const NEAR_OWNER_BYTES: u64 = 1_024;
pub(super) const OWNER_INDEX_ENTRY_BYTES: usize = 256;

pub(super) struct SourceOwnerIndex {
    boundaries: Vec<(u64, Option<usize>)>,
    starts: BTreeMap<u64, usize>,
    ends: BTreeMap<u64, usize>,
}

impl SourceOwnerIndex {
    pub(super) fn build(
        symbols: &[ExtractedSymbol],
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        let mut result = Self {
            boundaries: Vec::new(),
            starts: BTreeMap::new(),
            ends: BTreeMap::new(),
        };
        let mut events = Vec::new();
        events
            .try_reserve(symbols.len().saturating_mul(2))
            .map_err(|_| ExtractError::OutputLimit)?;
        for (index, symbol) in symbols.iter().enumerate() {
            poll(cancelled, index)?;
            let start = symbol.span.start_byte();
            let end = symbol.span.end_byte();
            result.starts.entry(start).or_insert(index);
            result.ends.insert(end, index);
            if start < end {
                events.push((start, true, index));
                events.push((end, false, index));
            }
        }
        poll(cancelled, 0)?;
        events.sort_unstable();
        let mut active = BTreeSet::new();
        result
            .boundaries
            .try_reserve(events.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        for (position, (offset, entering, index)) in events.into_iter().enumerate() {
            poll(cancelled, position)?;
            let symbol = &symbols[index];
            let key = (symbol.span.end_byte() - symbol.span.start_byte(), index);
            if entering {
                active.insert(key);
            } else {
                active.remove(&key);
            }
            result
                .boundaries
                .push((offset, active.first().map(|(_, index)| *index)));
        }
        Ok(result)
    }

    pub(super) fn containing(&self, offset: u64) -> Option<usize> {
        let end = self
            .boundaries
            .partition_point(|(start, _)| *start <= offset);
        end.checked_sub(1)
            .and_then(|index| self.boundaries[index].1)
    }

    pub(super) fn near(&self, offset: u64) -> Option<usize> {
        self.containing(offset)
            .or_else(|| {
                self.starts
                    .range(offset..)
                    .next()
                    .filter(|(start, _)| start.saturating_sub(offset) <= NEAR_OWNER_BYTES)
                    .map(|(_, index)| *index)
            })
            .or_else(|| {
                self.ends
                    .range(..=offset)
                    .next_back()
                    .filter(|(end, _)| offset.saturating_sub(**end) <= NEAR_OWNER_BYTES)
                    .map(|(_, index)| *index)
            })
    }
}

fn poll(cancelled: &mut dyn FnMut() -> bool, position: usize) -> Result<(), ExtractError> {
    if position.is_multiple_of(CANCELLATION_INTERVAL_ITEMS) && cancelled() {
        Err(ExtractError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NativeExtractor, SourceLimits, SourceSnapshot};
    use cartograph_domain::SourceLanguage;
    use std::fmt::Write;

    #[test]
    fn ownership_index_polls_cancellation_during_construction() {
        const SYMBOL_COUNT: usize = 1_024;
        const CANCEL_AT_POLL: usize = 4;
        let mut source = String::new();
        for index in 0..SYMBOL_COUNT {
            writeln!(source, "fn owner{index}() {{}}")
                .unwrap_or_else(|error| panic!("write owner source: {error}"));
        }
        let snapshot = SourceSnapshot::from_bytes(
            "owner.rs",
            source.as_bytes(),
            SourceLimits::new(source.len())
                .unwrap_or_else(|error| panic!("owner source limit: {error}")),
        )
        .unwrap_or_else(|error| panic!("owner source snapshot: {error}"));
        let file = NativeExtractor::new(SourceLanguage::Rust)
            .unwrap_or_else(|error| panic!("owner native extractor: {error}"))
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("extract owner source: {error}"));
        assert_eq!(file.symbols.len(), SYMBOL_COUNT);
        let mut polls = 0;
        let result = SourceOwnerIndex::build(&file.symbols, &mut || {
            polls += 1;
            polls >= CANCEL_AT_POLL
        });
        assert!(matches!(result, Err(ExtractError::Cancelled)));
        assert_eq!(polls, CANCEL_AT_POLL);
    }
}
