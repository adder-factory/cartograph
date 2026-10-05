use cartograph_domain::{SourcePosition, SourceSpan};

use crate::ExtractError;

#[derive(Clone, Copy)]
pub(crate) struct SourceByteRange {
    start: usize,
    end: usize,
    source_len: usize,
}

impl SourceByteRange {
    pub(crate) const fn new(start: usize, end: usize, source_len: usize) -> Self {
        Self {
            start,
            end,
            source_len,
        }
    }
}

pub(crate) struct LineMap {
    starts: Vec<usize>,
}

impl LineMap {
    pub(crate) fn new(source: &str) -> Result<Self, ExtractError> {
        Ok(Self {
            starts: line_starts(source)?,
        })
    }

    pub(crate) fn span(&self, range: SourceByteRange) -> Result<SourceSpan, ExtractError> {
        if range.start >= range.end || range.end > range.source_len {
            return Err(ExtractError::InvalidSpan);
        }
        SourceSpan::new(self.position(range.start)?, self.position(range.end)?)
            .map_err(|_| ExtractError::InvalidSpan)
    }

    /// Zero-based row and byte column of one byte offset, as a parser point.
    pub(crate) fn point(&self, byte: usize) -> tree_sitter::Point {
        let line_index = self
            .starts
            .partition_point(|start| *start <= byte)
            .saturating_sub(1);
        let line_start = self.starts.get(line_index).copied().unwrap_or_default();
        tree_sitter::Point::new(line_index, byte.saturating_sub(line_start))
    }

    fn position(&self, byte: usize) -> Result<SourcePosition, ExtractError> {
        let line_index = self.starts.partition_point(|start| *start <= byte) - 1;
        SourcePosition::new(
            u64::try_from(byte).map_err(|_| ExtractError::InvalidSpan)?,
            u32::try_from(line_index.saturating_add(1)).map_err(|_| ExtractError::InvalidSpan)?,
            u32::try_from(byte.saturating_sub(self.starts[line_index]))
                .map_err(|_| ExtractError::InvalidSpan)?,
        )
        .map_err(|_| ExtractError::InvalidSpan)
    }
}

fn line_starts(source: &str) -> Result<Vec<usize>, ExtractError> {
    let line_count = source
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1);
    let mut starts = Vec::new();
    starts
        .try_reserve_exact(line_count)
        .map_err(|_| ExtractError::OutputLimit)?;
    starts.push(0);
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index.saturating_add(1));
        }
    }
    Ok(starts)
}

pub(crate) fn physical_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0_usize;
    source.split_inclusive('\n').map(move |raw| {
        let start = offset;
        offset = offset.saturating_add(raw.len());
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        (start, line.strip_suffix('\r').unwrap_or(line))
    })
}
