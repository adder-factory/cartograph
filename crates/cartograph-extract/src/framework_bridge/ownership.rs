//! Indexed structural ownership and bridge declaration/annotation sites.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};

use crate::{ExtractError, framework::FrameworkBuilder};

use super::SymbolRange;

// Fixed-size vectors and map nodes; bridge sites borrow their names.
const INDEX_ENTRY_BYTES: usize = 160;
const SYMBOL_ID_BYTES: usize = 36;
const EXPO_MODULE_BASES: [&str; 3] = [
    "Module",
    "expo.modules.kotlin.modules.Module",
    "ExpoModulesCore.Module",
];

#[derive(Clone, Copy, Default)]
enum StructuralOwner {
    #[default]
    Unresolved,
    Resolved(Option<usize>),
}

#[derive(Clone, Copy)]
pub(super) struct BridgeCall {
    pub(super) name: &'static str,
    pub(super) end: usize,
    pub(super) limit: usize,
    pub(super) supported: bool,
}

#[derive(Clone, Copy)]
pub(super) struct AnnotationSite {
    pub(super) symbol: usize,
    pub(super) end: usize,
}

pub(super) struct BridgeOwnership<'source> {
    pub(super) source: &'source str,
    pub(super) classes: Vec<usize>,
    pub(super) expo_classes: BTreeSet<usize>,
    pub(super) view_classes: BTreeSet<usize>,
    pub(super) members: BTreeMap<usize, Vec<usize>>,
    pub(super) definitions: Vec<usize>,
    pub(super) calls: BTreeMap<Option<usize>, Vec<BridgeCall>>,
    pub(super) annotations: BTreeMap<(usize, &'static str), Vec<AnnotationSite>>,
    ranges: Vec<Option<(usize, usize)>>,
    parents: Vec<Option<usize>>,
    owners: Vec<StructuralOwner>,
    definition_owners: Vec<StructuralOwner>,
}

impl<'source> BridgeOwnership<'source> {
    pub(super) fn build(
        builder: &mut FrameworkBuilder<'_, '_>,
        source: &'source str,
    ) -> Result<Self, ExtractError> {
        let count = builder.original_symbol_count();
        let entries = count.saturating_add(builder.references().len());
        let modeled = entries
            .saturating_mul(INDEX_ENTRY_BYTES)
            .saturating_add(count.saturating_mul(SYMBOL_ID_BYTES));
        let bytes = u64::try_from(modeled).map_err(|_| ExtractError::OutputLimit)?;
        builder.bridge.reserve_working_bytes(bytes)?;
        let mut result = Self {
            source,
            classes: Vec::new(),
            expo_classes: BTreeSet::new(),
            view_classes: BTreeSet::new(),
            members: BTreeMap::new(),
            definitions: Vec::new(),
            calls: BTreeMap::new(),
            annotations: BTreeMap::new(),
            ranges: vec![None; count],
            parents: vec![None; count],
            owners: vec![StructuralOwner::Unresolved; count],
            definition_owners: vec![StructuralOwner::Unresolved; count],
        };
        let ids = result.index_symbols(builder)?;
        result.index_parents(builder, &ids)?;
        for index in 0..count {
            result.resolve_identity(builder, (index, false))?;
            result.resolve_identity(builder, (index, true))?;
            if let Some(class) = result.owner(index) {
                result.members.entry(class).or_default().push(index);
            }
        }
        result.index_sites(builder, &ids)?;
        Ok(result)
    }

    pub(super) fn range(&self, index: usize) -> Option<SymbolRange<'source>> {
        let (start, end) = self.ranges.get(index).copied().flatten()?;
        Some(SymbolRange {
            source: self.source,
            start,
            end,
        })
    }

    pub(super) fn owner(&self, index: usize) -> Option<usize> {
        match self.owners.get(index)? {
            StructuralOwner::Unresolved => None,
            StructuralOwner::Resolved(owner) => *owner,
        }
    }

    pub(super) fn native_symbol_name(
        &self,
        builder: &mut FrameworkBuilder<'_, '_>,
        index: usize,
    ) -> Result<Option<(&'source str, usize, usize)>, ExtractError> {
        let Some(range) = self.range(index) else {
            return Ok(None);
        };
        let Some(node) = builder
            .syntax_root()
            .and_then(|root| root.named_descendant_for_byte_range(range.start, range.end))
        else {
            return Ok(None);
        };
        let mut cursor = node.walk();
        let mut name = node.child_by_field_name("name");
        if name.is_none() {
            for child in node.named_children(&mut cursor) {
                builder.bridge.charge_work(1)?;
                if matches!(
                    child.kind(),
                    "simple_identifier" | "identifier" | "type_identifier"
                ) {
                    name = Some(child);
                    break;
                }
            }
        }
        let Some(name) = name else { return Ok(None) };
        builder
            .bridge
            .charge_work(name.end_byte() - name.start_byte())?;
        let text = self
            .source
            .get(name.byte_range())
            .ok_or(ExtractError::InvalidSpan)?;
        let value = text.trim_matches('`');
        let start = name.start_byte() + usize::from(text.starts_with('`'));
        Ok(Some((value, start, start + value.len())))
    }

    fn index_symbols(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
    ) -> Result<BTreeMap<SymbolId, usize>, ExtractError> {
        let mut ids = BTreeMap::new();
        for index in 0..builder.original_symbol_count() {
            builder.bridge.charge_work(1)?;
            let Some(symbol) = builder.original_symbol(index) else {
                continue;
            };
            ids.insert(symbol.id.clone(), index);
            let start =
                usize::try_from(symbol.span.start_byte()).map_err(|_| ExtractError::InvalidSpan)?;
            let end =
                usize::try_from(symbol.span.end_byte()).map_err(|_| ExtractError::InvalidSpan)?;
            self.ranges[index] = self.source.get(start..end).map(|_| (start, end));
            if symbol.kind == SymbolKind::Class {
                self.classes.push(index);
                self.owners[index] = StructuralOwner::Resolved(Some(index));
            }
            if symbol.name == "definition"
                && matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function)
            {
                self.definitions.push(index);
                self.definition_owners[index] = StructuralOwner::Resolved(Some(index));
            }
        }
        Ok(ids)
    }

    fn index_parents(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        ids: &BTreeMap<SymbolId, usize>,
    ) -> Result<(), ExtractError> {
        for offset in 0..builder.containments().len() {
            builder.bridge.charge_work(1)?;
            let containment = &builder.containments()[offset];
            if let (Some(child), Some(parent)) =
                (ids.get(&containment.child), ids.get(&containment.parent))
            {
                self.parents[*child] = Some(*parent);
            }
        }
        Ok(())
    }

    fn resolve_identity(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        query: (usize, bool),
    ) -> Result<(), ExtractError> {
        let owners = if query.1 {
            &mut self.definition_owners
        } else {
            &mut self.owners
        };
        let mut path = Vec::new();
        let mut cursor = query.0;
        let owner = loop {
            builder.bridge.charge_work(1)?;
            if let StructuralOwner::Resolved(owner) = owners[cursor] {
                break owner;
            }
            // Structural cycles cannot consume unbounded memory or work.
            if path.len() == self.parents.len() {
                return Err(ExtractError::OutputLimit);
            }
            path.push(cursor);
            let Some(parent) = self.parents[cursor] else {
                break None;
            };
            cursor = parent;
        };
        for index in path {
            owners[index] = StructuralOwner::Resolved(owner);
        }
        Ok(())
    }

    fn index_sites(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        ids: &BTreeMap<SymbolId, usize>,
    ) -> Result<(), ExtractError> {
        for offset in 0..builder.references().len() {
            builder
                .bridge
                .charge_work(builder.references()[offset].name.len().saturating_add(1))?;
            let reference = &builder.references()[offset];
            let owner = reference.owner.as_ref().and_then(|id| ids.get(id)).copied();
            let end = usize::try_from(reference.span.end_byte())
                .map_err(|_| ExtractError::InvalidSpan)?;
            if matches!(
                reference.kind,
                ReferenceKind::Calls | ReferenceKind::Instantiates
            ) {
                let Some((name, supported)) = expo_callee(builder.language(), &reference.name)
                else {
                    continue;
                };
                let definition = owner.and_then(|index| match self.definition_owners[index] {
                    StructuralOwner::Resolved(definition) => definition,
                    StructuralOwner::Unresolved => None,
                });
                let limit = owner
                    .and_then(|index| self.range(index))
                    .map_or(self.source.len(), |range| range.end);
                self.calls.entry(definition).or_default().push(BridgeCall {
                    name,
                    end,
                    limit,
                    supported,
                });
                continue;
            }
            let Some(owner) = owner else {
                continue;
            };
            if reference.kind == ReferenceKind::Inherits && reference.name == "ExpoModulesCore" {
                self.index_qualified_expo_base(builder, (owner, end))?;
            } else {
                self.index_owned_site((reference.kind, reference.name.as_str()), (owner, end));
            }
        }
        Ok(())
    }

    fn index_qualified_expo_base(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        (owner, end): (usize, usize),
    ) -> Result<(), ExtractError> {
        if self.qualified_expo_base(builder, end)?
            && let Some(class) = self.owner(owner)
        {
            self.expo_classes.insert(class);
        }
        Ok(())
    }

    // Swift's ordinary inheritance fact retains the first type identifier.
    // Read only its bounded heritage node to establish the qualified base.
    fn qualified_expo_base(
        &self,
        builder: &mut FrameworkBuilder<'_, '_>,
        end: usize,
    ) -> Result<bool, ExtractError> {
        builder.bridge.charge_work(1)?;
        let Some(node) = builder
            .syntax_root()
            .and_then(|root| {
                root.named_descendant_for_byte_range(end - "ExpoModulesCore".len(), end)
            })
            .and_then(|name| name.parent())
            .filter(|node| node.kind() == "user_type")
        else {
            return Ok(false);
        };
        let bytes = node.end_byte() - node.start_byte();
        if bytes > super::MAX_BRIDGE_SCAN_BYTES {
            return Ok(false);
        }
        builder.bridge.charge_work(bytes)?;
        let mut cursor = node.walk();
        let mut identifiers = node
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra());
        let first = identifiers
            .next()
            .and_then(|node| self.source.get(node.byte_range()));
        let second = identifiers
            .next()
            .and_then(|node| self.source.get(node.byte_range()));
        Ok(first == Some("ExpoModulesCore")
            && second == Some("Module")
            && identifiers.next().is_none())
    }

    fn index_owned_site(&mut self, reference: (ReferenceKind, &str), site: (usize, usize)) {
        let Some(class) = self.owner(site.0) else {
            return;
        };
        match reference {
            (ReferenceKind::Extends | ReferenceKind::Inherits, name)
                if EXPO_MODULE_BASES.contains(&name) =>
            {
                self.expo_classes.insert(class);
            }
            (ReferenceKind::Extends | ReferenceKind::Inherits, name)
                if name.contains("ViewManager") =>
            {
                self.view_classes.insert(class);
            }
            (ReferenceKind::Decorates, name) => {
                let name = match name.rsplit('.').next() {
                    Some("ReactModule") => "ReactModule",
                    Some("ReactMethod") => "ReactMethod",
                    Some("ReactProp") => "ReactProp",
                    Some("ReactPropGroup") => "ReactPropGroup",
                    _ => return,
                };
                self.annotations
                    .entry((class, name))
                    .or_default()
                    .push(AnnotationSite {
                        symbol: site.0,
                        end: site.1,
                    });
            }
            _ => {}
        }
    }
}

// A known receiver spelling is sufficient for a marker, without resolving
// helper calls. Other qualified Name calls veto a guessed fallback identity.
fn expo_callee(language: SourceLanguage, name: &str) -> Option<(&'static str, bool)> {
    Some(match (language, name) {
        (_, "Name")
        | (SourceLanguage::Kotlin, "this.Name")
        | (SourceLanguage::Swift, "ExpoModulesCore.Name") => ("Name", true),
        (_, "Function") => ("Function", true),
        (_, "AsyncFunction") => ("AsyncFunction", true),
        (_, "Property") => ("Property", true),
        (_, name) if name.ends_with(".Name") => ("Name", false),
        _ => return None,
    })
}
