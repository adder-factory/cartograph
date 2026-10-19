//! Relationship-aware feature queries over the complete native graph snapshot.

mod findings;
mod references;
mod risk;
mod tests;

use crate::{
    GraphDirection, ReferenceInput,
    rocks::{GraphLink, GraphSnapshot, ReadBudget, RocksError},
};
use cartograph_domain::SymbolId;

fn visit_references(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(ReferenceInput, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let mut cursor = None;
    loop {
        let (references, next) = graph.reference_records(cursor.as_ref(), 256, budget)?;
        for reference in references {
            budget.checkpoint()?;
            visit(reference, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}
fn neighbors(
    graph: &GraphSnapshot<'_, '_>,
    id: &SymbolId,
    direction: GraphDirection,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(GraphLink, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let mut cursor = None;
    loop {
        let page = graph.neighbors(id, direction, cursor.as_ref(), 256, budget)?;
        for link in page.links {
            budget.checkpoint()?;
            visit(link, budget)?;
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "native-insight",
    }
}
fn workspace() -> RocksError {
    RocksError::Limit {
        resource: "insight-workspace",
    }
}
