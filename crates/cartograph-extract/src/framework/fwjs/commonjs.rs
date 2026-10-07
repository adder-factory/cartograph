//! Add callable ownership to already accepted `CommonJS` sites. Earlier facts
//! keep their order and file ownership; no require syntax is reparsed here.
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, FrameworkReferenceInput},
};
use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use std::{collections::BTreeMap, mem::size_of};
use tree_sitter::Node;

type Range = (usize, usize);
const MAP_ENTRY_OVERHEAD: usize = size_of::<usize>() * 4;

#[derive(Default)]
pub(super) struct Index {
    callables: BTreeMap<Range, SymbolId>,
    imports: BTreeMap<Range, String>,
}

impl Index {
    pub(super) fn build(builder: &mut FrameworkBuilder<'_, '_>) -> Result<Self, ExtractError> {
        let mut index = Self::default();
        for ordinal in 0..builder.original_symbol_count() {
            builder.bridge.charge_work(1)?;
            let Some(symbol) = builder.original_symbol(ordinal) else {
                continue;
            };
            if !matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
            ) {
                continue;
            }
            let range = span_range(symbol.span)?;
            let id = symbol.id.clone();
            reserve_entry(
                builder,
                size_of::<(Range, SymbolId)>().saturating_add(id.as_str().len()),
            )?;
            index.callables.insert(range, id);
        }
        for ordinal in 0..builder.reference_count() {
            builder.bridge.charge_work(1)?;
            let Some(reference) = builder.reference(ordinal) else {
                continue;
            };
            if reference.owner.is_some() || reference.kind != ReferenceKind::Imports {
                continue;
            }
            let range = span_range(reference.span)?;
            let name = reference.name.clone();
            reserve_entry(
                builder,
                size_of::<(Range, String)>().saturating_add(name.len()),
            )?;
            index.imports.insert(range, name);
        }
        Ok(index)
    }

    pub(super) fn capture(
        &self,
        builder: &mut FrameworkBuilder<'_, '_>,
        call: Node<'_>,
    ) -> Result<(), ExtractError> {
        let Some(name) = self.imports.get(&(call.start_byte(), call.end_byte())) else {
            return Ok(());
        };
        let mut ancestor = call.parent();
        while let Some(node) = ancestor {
            builder.bridge.charge_work(1)?;
            if let Some(owner) = self.callables.get(&(node.start_byte(), node.end_byte())) {
                return builder.add_reference(FrameworkReferenceInput {
                    owner: Some(owner.clone()),
                    name,
                    resolution_name: Some(&format!("framework-commonjs-module::{name}")),
                    kind: ReferenceKind::Imports,
                    start: call.start_byte(),
                    end: call.end_byte(),
                });
            }
            ancestor = node.parent();
        }
        Ok(())
    }
}

fn span_range(span: cartograph_domain::SourceSpan) -> Result<Range, ExtractError> {
    Ok((
        usize::try_from(span.start_byte()).map_err(|_| ExtractError::InvalidSpan)?,
        usize::try_from(span.end_byte()).map_err(|_| ExtractError::InvalidSpan)?,
    ))
}

fn reserve_entry(builder: &mut FrameworkBuilder<'_, '_>, bytes: usize) -> Result<(), ExtractError> {
    builder.bridge.reserve_working_bytes(
        u64::try_from(bytes.saturating_add(MAP_ENTRY_OVERHEAD))
            .map_err(|_| ExtractError::OutputLimit)?,
    )
}
