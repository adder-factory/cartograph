//! BG3 LSX/XML resource scanning.
//!
//! An LSX `<node>` (or `<stat_object>`) is named by its own defining
//! attributes in the game's precedence order, never by a nested node's fields
//! or a translated-string handle. Nameless container nodes lift their
//! references to the nearest named ancestor. Effect, settings, and timeline
//! tags declare resources directly through their attributes.

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, Visibility};

use crate::{ExtractError, SymbolExportFlags};

use super::{
    Bg3Region, CustomBuilder, CustomReferenceInput, CustomSymbolInput, MarkupTag, SymbolOptions,
    basename_stem,
    bg3_tokens::{
        contains_bg3_identifier, global_identity, is_resource_name, is_zero_uuid, reference_tokens,
    },
    looks_sensitive, markup_tags, matches_ignore_ascii_case, next_markup_attribute,
    scan_bg3_content, scan_bg3_region, tag_attribute,
};

/// Defining attributes in v1 naming precedence.
const NAME_FIELD_ORDER: [&str; 6] = ["NameFS", "Name", "Folder", "RuleName", "SelectorId", "UUID"];
/// Structural LSX node ids that group children rather than declare anything.
const NON_SYMBOL_NODE_IDS: [&str; 6] = [
    "root",
    "children",
    "Tags",
    "SubClasses",
    "SubClass",
    "Object",
];
/// Attributes whose own value is the declaration identity, not a reference.
const DEFINING_FIELD_NAMES: [&str; 9] = [
    "UUID",
    "Name",
    "NameFS",
    "Folder",
    "RuleName",
    "SelectorId",
    "DisplayName",
    "Description",
    "Text",
];
/// Field names whose referenced resource is the declaration's base.
const EXTENDS_FIELD_NAMES: [&str; 3] = ["using", "parent", "parentguid"];
/// Non-LSX tags that declare one resource each (effects, sidecars, timelines).
const GENERIC_RESOURCE_TAGS: [&str; 7] = [
    "Settings",
    "effect",
    "component",
    "MultiEffectInfos",
    "EffectInfo",
    "trackgroup",
    "track",
];
/// Generic-tag attributes that name the resource, in precedence order.
const GENERIC_NAME_ATTRIBUTES: [&str; 4] = ["Name", "instancename", "UUID", "id"];
/// Generic-tag attributes that identify rather than reference.
const GENERIC_IDENTITY_ATTRIBUTES: [&str; 5] = ["Name", "UUID", "id", "instancename", "class"];
/// Field-name fragments that mark a value as a resource reference.
const REFERENCE_FIELD_MARKERS: [&str; 30] = [
    "uuid",
    "guid",
    "template",
    "spell",
    "passive",
    "status",
    "boost",
    "functor",
    "selector",
    "list",
    "table",
    "using",
    "parent",
    "root",
    "resource",
    "effect",
    "icon",
    "tag",
    "equipment",
    "weapon",
    "race",
    "class",
    "actionresource",
    "requirement",
    "condition",
    "event",
    "cost",
    "propert",
    "data",
    "handle",
];
/// Attribute types that always carry references.
const REFERENCE_TYPE_MARKERS: [&str; 4] = [
    "guidobject",
    "statreference",
    "baseclass",
    "translatedstring",
];
/// Value punctuation that marks a functor/boost expression worth scanning.
const EXPRESSION_MARKERS: [char; 4] = ['(', ')', '|', ';'];
/// Prefix of editor-generated placeholder stat names.
const GENERATED_STAT_PREFIX: &str = "New_Stat_";
/// Deepest LSX object nesting scanned before the file is reported degraded.
const MAXIMUM_OBJECT_NESTING: usize = crate::MAXIMUM_AST_DEPTH;

/// Scan a text-converted LSX/XML resource (binary payloads are rejected by
/// the caller before either the JSON or the markup path runs).
pub(super) fn extract_markup(builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
    let tags = markup_tags(builder.source());
    let mut state = MarkupState {
        regions: vec![(None, builder.path().to_owned())],
        objects: Vec::new(),
    };
    for tag in tags {
        builder.check_cancelled()?;
        if scan_bg3_region(builder, &mut state.regions, tag)? {
            continue;
        }
        if state
            .regions
            .last()
            .is_some_and(|(_, name)| name.is_empty())
            || scan_bg3_content(builder, &state.regions, tag)?
        {
            continue;
        }
        scan_tag(builder, &mut state, tag)?;
    }
    while let Some(object) = state.objects.pop() {
        builder.check_cancelled()?;
        finalize_object(builder, &mut state, object)?;
    }
    Ok(())
}

struct MarkupState<'source> {
    regions: Vec<Bg3Region>,
    objects: Vec<Bg3Object<'source>>,
}

/// One open LSX object, its own fields, and references lifted from nameless
/// descendants.
struct Bg3Object<'source> {
    tag: MarkupTag<'source>,
    fields: Vec<Bg3Field<'source>>,
    lifted: Vec<PendingReference<'source>>,
}

#[derive(Clone, Copy)]
struct Bg3Field<'source> {
    name: &'source str,
    value: Option<(usize, &'source str)>,
    handle: Option<(usize, &'source str)>,
    value_type: Option<&'source str>,
}

impl<'source> Bg3Field<'source> {
    /// The field's effective value: its `value`, or else its `handle`.
    fn effective_value(self) -> Option<(usize, &'source str)> {
        self.value.or(self.handle)
    }
}

#[derive(Clone, Copy)]
struct PendingReference<'source> {
    name: &'source str,
    kind: ReferenceKind,
    start: usize,
}

fn scan_tag<'source>(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut MarkupState<'source>,
    tag: MarkupTag<'source>,
) -> Result<(), ExtractError> {
    if matches_ignore_ascii_case(tag.name, &["node", "stat_object"]) {
        if tag.closing {
            if let Some(object) = state.objects.pop() {
                finalize_object(builder, state, object)?;
            }
            return Ok(());
        }
        let object = Bg3Object {
            tag,
            fields: Vec::new(),
            lifted: Vec::new(),
        };
        if tag.self_closing {
            return finalize_object(builder, state, object);
        }
        if state.objects.len() >= MAXIMUM_OBJECT_NESTING {
            return Err(ExtractError::NestingLimit);
        }
        state.objects.push(object);
        return Ok(());
    }
    if tag.closing {
        return Ok(());
    }
    if matches_ignore_ascii_case(tag.name, &["attribute", "field"]) {
        if let (Some(object), Some(field)) = (state.objects.last_mut(), field_from_tag(tag)) {
            object.fields.push(field);
        }
        return Ok(());
    }
    if GENERIC_RESOURCE_TAGS.contains(&tag.name) {
        let parent = state.regions.last().and_then(|(id, _)| id.clone());
        return emit_generic_resource(builder, tag, parent);
    }
    Ok(())
}

fn field_from_tag(tag: MarkupTag<'_>) -> Option<Bg3Field<'_>> {
    let key = if tag.name.eq_ignore_ascii_case("field") {
        "name"
    } else {
        "id"
    };
    let (_, name) = tag_attribute(tag, key).filter(|(_, name)| !name.is_empty())?;
    Some(Bg3Field {
        name,
        value: tag_attribute(tag, "value"),
        handle: tag_attribute(tag, "handle"),
        value_type: tag_attribute(tag, "type").map(|(_, value_type)| value_type),
    })
}

fn finalize_object<'source>(
    builder: &mut CustomBuilder<'_, '_>,
    state: &mut MarkupState<'source>,
    object: Bg3Object<'source>,
) -> Result<(), ExtractError> {
    let Bg3Object {
        tag,
        fields,
        lifted: mut references,
    } = object;
    for field in &fields {
        push_field_references(&mut references, *field);
    }
    let Some((name_offset, name)) = definition_name(tag, &fields) else {
        if let Some(parent) = state.objects.last_mut() {
            lift_references(&mut parent.lifted, references);
            return Ok(());
        }
        return emit_references(builder, None, references);
    };
    let uuid = field_value(&fields, "UUID").and_then(|(_, uuid)| global_identity(uuid));
    let prefix = state
        .regions
        .last()
        .map_or(builder.path(), |(_, name)| name);
    let qualified_name = uuid.map_or_else(|| format!("{prefix}::{name}"), str::to_owned);
    // A UUID is a game-global identity: other files reference it directly.
    let addressable = state.regions.len() == 1 || uuid.is_some();
    // As in v1, the declaration starts at its own `<node>` tag. v1 recorded
    // no end (it defaulted to the start line); the span here runs through
    // the attribute value that names it.
    let id = builder.add_symbol(
        CustomSymbolInput::new(SymbolKind::Resource, name, qualified_name)
            .at(tag.start.min(name_offset), name_offset + name.len())
            .with_options(SymbolOptions {
                body_search_text: format!("bg3 resource {name}"),
                export: SymbolExportFlags::named(addressable),
                visibility: addressable.then_some(Visibility::Public),
                parent: state.regions.last().and_then(|(id, _)| id.clone()),
                ..SymbolOptions::default()
            }),
    )?;
    emit_references(builder, Some(&id), references)
}

/// Merge a nameless object's references into its parent's, appending the
/// smaller batch to the larger so deep wrapper chains stay near-linear.
fn lift_references<'source>(
    parent: &mut Vec<PendingReference<'source>>,
    mut references: Vec<PendingReference<'source>>,
) {
    if parent.len() < references.len() {
        std::mem::swap(parent, &mut references);
    }
    parent.append(&mut references);
}

/// The object's declared name, or `None` for structural containers and
/// objects that only point at another object.
fn definition_name<'source>(
    tag: MarkupTag<'source>,
    fields: &[Bg3Field<'source>],
) -> Option<(usize, &'source str)> {
    let only_object_reference = fields.len() == 1
        && fields
            .iter()
            .all(|field| field.name.eq_ignore_ascii_case("object"));
    if only_object_reference {
        return None;
    }
    let declared = NAME_FIELD_ORDER.into_iter().find_map(|field_name| {
        field_value(fields, field_name)
            .map(|(offset, value)| trimmed_at(offset, value))
            .filter(|(_, value)| !value.is_empty())
            .filter(|(_, value)| field_name != "Name" || !is_generated_stat_name(value))
            .filter(|(_, value)| field_name != "UUID" || !is_zero_uuid(value))
    });
    let fallback =
        || tag_attribute(tag, "id").filter(|(_, id)| !fields.is_empty() && !id.is_empty());
    declared
        .or_else(fallback)
        .filter(|(_, name)| !NON_SYMBOL_NODE_IDS.contains(name) && !looks_sensitive(name))
}

fn field_value<'source>(fields: &[Bg3Field<'source>], name: &str) -> Option<(usize, &'source str)> {
    fields
        .iter()
        .rev()
        .find(|field| field.name.eq_ignore_ascii_case(name))
        .and_then(|field| field.effective_value())
}

fn trimmed_at(offset: usize, value: &str) -> (usize, &str) {
    let trimmed = value.trim_start();
    let leading = value.len() - trimmed.len();
    (offset + leading, trimmed.trim_end())
}

fn is_generated_stat_name(value: &str) -> bool {
    value
        .strip_prefix(GENERATED_STAT_PREFIX)
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn push_field_references<'source>(
    references: &mut Vec<PendingReference<'source>>,
    field: Bg3Field<'source>,
) {
    if let Some((start, handle)) = field.handle {
        references.push(PendingReference {
            name: handle,
            kind: ReferenceKind::References,
            start,
        });
    }
    let Some((offset, value)) = field.effective_value() else {
        return;
    };
    if !field_carries_references(field, value) {
        return;
    }
    let kind = if matches_ignore_ascii_case(field.name, &EXTENDS_FIELD_NAMES) {
        ReferenceKind::Extends
    } else {
        ReferenceKind::References
    };
    let defining = DEFINING_FIELD_NAMES.contains(&field.name);
    for (relative, token) in reference_tokens(value) {
        if defining && token == value.trim() {
            continue;
        }
        references.push(PendingReference {
            name: token,
            kind,
            start: offset + relative,
        });
    }
}

/// References of one stats `data "Field" "Value"` pair, under the same field
/// rules as LSX attributes; offsets are relative to `value`.
pub(super) fn field_references<'source>(
    name: &'source str,
    value: &'source str,
) -> Vec<(usize, &'source str, ReferenceKind)> {
    let mut references = Vec::new();
    push_field_references(
        &mut references,
        Bg3Field {
            name,
            value: Some((0, value)),
            handle: None,
            value_type: None,
        },
    );
    references
        .into_iter()
        .map(|reference| (reference.start, reference.name, reference.kind))
        .collect()
}

fn field_carries_references(field: Bg3Field<'_>, value: &str) -> bool {
    field.handle.is_some()
        || contains_bg3_identifier(value)
        || field
            .value_type
            .is_some_and(|value_type| contains_marker(value_type, &REFERENCE_TYPE_MARKERS))
        || is_reference_field_name(field.name)
        || value.contains(EXPRESSION_MARKERS)
}

fn is_reference_field_name(name: &str) -> bool {
    contains_marker(name, &REFERENCE_FIELD_MARKERS)
}

fn contains_marker(value: &str, markers: &[&str]) -> bool {
    let lower = value.to_ascii_lowercase();
    markers.iter().any(|marker| lower.contains(marker))
}

/// Emit references in source order, each distinct site once.
fn emit_references(
    builder: &mut CustomBuilder<'_, '_>,
    owner: Option<&SymbolId>,
    mut references: Vec<PendingReference<'_>>,
) -> Result<(), ExtractError> {
    references.sort_by_key(|reference| reference.start);
    let mut seen = BTreeSet::new();
    for reference in &references {
        if !seen.insert((reference.start, reference.kind, reference.name)) {
            continue;
        }
        builder.add_reference(
            CustomReferenceInput::new(owner.cloned(), reference.name, reference.kind)
                .at(reference.start, reference.start + reference.name.len()),
        )?;
    }
    Ok(())
}

/// `<effect Name="FX" Resource="h..."/>`, `<Settings source=...>` and similar
/// tags declare one resource named by an identity attribute or, for settings
/// sidecars, by the file itself.
fn emit_generic_resource(
    builder: &mut CustomBuilder<'_, '_>,
    tag: MarkupTag<'_>,
    parent: Option<SymbolId>,
) -> Result<(), ExtractError> {
    let declared = GENERIC_NAME_ATTRIBUTES
        .into_iter()
        .find_map(|key| tag_attribute(tag, key))
        .map(|(offset, value)| trimmed_at(offset, value))
        .filter(|(_, value)| is_resource_name(value));
    let path = builder.path().to_owned();
    // A file-stem fallback names the sidecar after its own (already public)
    // path, so only declared attribute values need the secret screen.
    let (start, end, name) = match declared {
        Some((offset, name)) if !looks_sensitive(name) => (offset, offset + name.len(), name),
        Some(_) => return Ok(()),
        None => (tag.start, tag.end, basename_stem(&path)),
    };
    if name.is_empty() {
        return Ok(());
    }
    let uuid = ["UUID", "id"]
        .into_iter()
        .find_map(|key| tag_attribute(tag, key))
        .and_then(|(_, uuid)| global_identity(uuid));
    let addressable = parent.is_none() || uuid.is_some();
    let qualified_name = uuid.map_or_else(|| format!("{path}::{name}"), str::to_owned);
    let id = builder.add_symbol(
        CustomSymbolInput::new(SymbolKind::Resource, name, qualified_name)
            .at(start, end)
            .with_options(SymbolOptions {
                body_search_text: format!("bg3 resource {name}"),
                export: SymbolExportFlags::named(addressable),
                visibility: addressable.then_some(Visibility::Public),
                parent,
                ..SymbolOptions::default()
            }),
    )?;
    emit_references(builder, Some(&id), generic_tag_references(tag))
}

fn generic_tag_references(tag: MarkupTag<'_>) -> Vec<PendingReference<'_>> {
    let mut references = Vec::new();
    let mut cursor = 0;
    while let Some(attribute) = next_markup_attribute(tag.raw, &mut cursor) {
        if matches_ignore_ascii_case(attribute.name, &GENERIC_IDENTITY_ATTRIBUTES) {
            continue;
        }
        let value = &tag.raw[attribute.value_start..attribute.value_end];
        if !is_reference_field_name(attribute.name) && !contains_bg3_identifier(value) {
            continue;
        }
        let offset = tag.start + 1 + attribute.value_start;
        for (relative, token) in reference_tokens(value) {
            references.push(PendingReference {
                name: token,
                kind: ReferenceKind::References,
                start: offset + relative,
            });
        }
    }
    references
}
