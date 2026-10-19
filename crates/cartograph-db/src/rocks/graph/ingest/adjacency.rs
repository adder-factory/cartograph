//! Index-only sorting keeps both directions within the publication workspace.

use std::cmp::Ordering;

use cartograph_domain::SymbolId;

use super::{EdgeInput, RecordKey, Result, StorageRequest, codec, count};
use crate::rocks::GraphPolicy;

#[derive(Default)]
pub(in crate::rocks::graph) struct AdjacencyReport {
    pub(in crate::rocks::graph) blocks: u64,
    pub(in crate::rocks::graph) maximum_degree: u64,
}

pub(in crate::rocks::graph) fn visit(
    edges: &[EdgeInput],
    incoming: bool,
    policy: GraphPolicy,
    request: &StorageRequest,
    mut emit: impl FnMut(RecordKey, Vec<u8>) -> Result<()>,
) -> Result<AdjacencyReport> {
    super::check_workspace::<usize>(edges.len(), 0, policy)?;
    let mut order = Vec::new();
    order
        .try_reserve_exact(edges.len())
        .map_err(|_| super::super::limit())?;
    super::check_workspace::<usize>(order.capacity(), 0, policy)?;
    for index in 0..edges.len() {
        request.check()?;
        order.push(index);
    }
    order.sort_unstable_by(|&left, &right| compare(&edges[left], &edges[right], incoming));
    request.check()?;
    let mut report = AdjacencyReport::default();
    let prefix = if incoming {
        super::INCOMING
    } else {
        super::OUTGOING
    };
    let mut offset = 0;
    while offset < order.len() {
        request.check()?;
        let node = parts(&edges[order[offset]], incoming).0;
        let end = offset
            + order[offset..].partition_point(|&index| parts(&edges[index], incoming).0 == node);
        report.maximum_degree = report.maximum_degree.max(count(end - offset)?);
        let node_key = codec::key(prefix, &codec::uuid(node.as_str())?)?;
        PackedPartition {
            rows: &order[offset..end],
            edges,
            incoming,
            node: node_key.as_bytes(),
        }
        .emit(0, request, &mut emit, &mut report)?;
        offset = end;
    }
    Ok(report)
}

fn parts(edge: &EdgeInput, incoming: bool) -> (&SymbolId, &SymbolId) {
    if incoming {
        (&edge.target_symbol_id, &edge.source_symbol_id)
    } else {
        (&edge.source_symbol_id, &edge.target_symbol_id)
    }
}

fn compare(left: &EdgeInput, right: &EdgeInput, incoming: bool) -> Ordering {
    let (left_node, left_neighbor) = parts(left, incoming);
    let (right_node, right_neighbor) = parts(right, incoming);
    (left_node, left_neighbor, left.kind as u8)
        .cmp(&(right_node, right_neighbor, right.kind as u8))
        .then_with(|| {
            blake3::hash(left.provenance.as_bytes())
                .as_bytes()
                .cmp(blake3::hash(right.provenance.as_bytes()).as_bytes())
        })
}

fn entry(edge: &EdgeInput, incoming: bool) -> Result<[u8; codec::ENTRY_BYTES]> {
    codec::entry(parts(edge, incoming).1, edge.kind, &edge.provenance)
}

struct PackedPartition<'a> {
    rows: &'a [usize],
    edges: &'a [EdgeInput],
    incoming: bool,
    node: &'a [u8],
}

impl PackedPartition<'_> {
    fn emit(
        &self,
        depth: usize,
        request: &StorageRequest,
        emit: &mut impl FnMut(RecordKey, Vec<u8>) -> Result<()>,
        report: &mut AdjacencyReport,
    ) -> Result<()> {
        request.check()?;
        let first = entry(
            &self.edges[*self.rows.first().ok_or_else(codec::corrupt)?],
            self.incoming,
        )?;
        if self.rows.len() <= codec::BLOCK_CAPACITY {
            let suffix = codec::radix_prefix(&first, depth)?;
            let key = codec::key(self.node, if depth == 0 { &[] } else { &suffix })?;
            let mut value = codec::BLOCK_HEADER.to_vec();
            value.extend_from_slice(
                &u16::try_from(depth)
                    .map_err(|_| codec::corrupt())?
                    .to_be_bytes(),
            );
            for &index in self.rows {
                request.check()?;
                value.extend_from_slice(&entry(&self.edges[index], self.incoming)?);
            }
            emit(key, value)?;
            report.blocks = report
                .blocks
                .checked_add(1)
                .ok_or_else(super::super::limit)?;
            return Ok(());
        }
        let last = entry(
            &self.edges[*self.rows.last().ok_or_else(codec::corrupt)?],
            self.incoming,
        )?;
        let byte = first
            .iter()
            .zip(last)
            .position(|(&first, last)| first != last)
            .ok_or_else(codec::corrupt)?;
        let bit = usize::try_from((first[byte] ^ last[byte]).leading_zeros())
            .map_err(|_| codec::corrupt())?;
        let split = byte * 8 + bit;
        if split < depth {
            return Err(codec::corrupt());
        }
        // The comparator used precisely these binary entry fields. Search with
        // checked entry decoding so an invalid identity cannot become a key.
        let mut middle = 0;
        for &index in self.rows {
            request.check()?;
            if entry(&self.edges[index], self.incoming)?[byte] & (0x80 >> bit) != 0 {
                break;
            }
            middle += 1;
        }
        if middle == 0 || middle == self.rows.len() {
            return Err(codec::corrupt());
        }
        for rows in [&self.rows[..middle], &self.rows[middle..]] {
            Self {
                rows,
                edges: self.edges,
                incoming: self.incoming,
                node: self.node,
            }
            .emit(split + 1, request, emit, report)?;
        }
        Ok(())
    }
}
