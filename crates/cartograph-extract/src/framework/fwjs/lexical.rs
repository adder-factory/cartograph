//! Reuse native value-read lookup identities; never invent a nested binding.
use super::text;
use crate::{ExtractError, framework::FrameworkBuilder};
use cartograph_domain::ReferenceKind;
use std::collections::BTreeMap;
use tree_sitter::Node;

const READ_INDEX_ENTRY_BYTES: u64 = 64;

pub(super) struct Index {
    sites: BTreeMap<(u64, u64), Option<usize>>,
}

pub(super) struct Binding {
    pub(super) resolution_name: Option<String>,
}

impl Index {
    pub(super) fn build(builder: &mut FrameworkBuilder<'_, '_>) -> Result<Self, ExtractError> {
        let mut sites = BTreeMap::new();
        for position in 0..builder.bridge.original_references {
            builder.bridge.charge_work(1)?;
            let reference = &builder.references()[position];
            if reference.kind != ReferenceKind::References {
                continue;
            }
            let key = (reference.span.start_byte(), reference.span.end_byte());
            builder
                .bridge
                .reserve_working_bytes(READ_INDEX_ENTRY_BYTES)?;
            sites
                .entry(key)
                .and_modify(|site| *site = None)
                .or_insert(Some(position));
        }
        Ok(Self { sites })
    }

    pub(super) fn resolution(
        &self,
        builder: &mut FrameworkBuilder<'_, '_>,
        node: Node<'_>,
    ) -> Result<Option<Binding>, ExtractError> {
        builder.bridge.charge_work(1)?;
        let key = (
            u64::try_from(node.start_byte()).map_err(|_| ExtractError::InvalidSpan)?,
            u64::try_from(node.end_byte()).map_err(|_| ExtractError::InvalidSpan)?,
        );
        if let Some(position) = self.sites.get(&key) {
            return Ok(position
                .and_then(|position| builder.reference(position))
                .filter(|reference| reference.name == text(builder, node))
                .map(|reference| Binding {
                    resolution_name: reference.resolution_name.clone(),
                }));
        }
        // Missing native evidence may be an unrepresented parameter, local,
        // or omitted read. The entire nested scope conservatively abstains.
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            builder.bridge.charge_work(1)?;
            if matches!(
                scope.kind(),
                "statement_block" | "class_body" | "catch_clause"
            ) || scope.kind().contains("function")
                || scope.kind() == "method_definition"
            {
                return Ok(None);
            }
            ancestor = scope.parent();
        }
        Ok(Some(Binding {
            resolution_name: None,
        }))
    }
}
