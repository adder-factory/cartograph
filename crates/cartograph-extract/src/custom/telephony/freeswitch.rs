use std::mem::size_of;

use cartograph_domain::{FileParseStatus, ReferenceKind, SymbolKind};

use super::super::{
    CustomImportInput, CustomReferenceInput, MarkupTag, markup_tags, parse_markup_tag,
    poll_cancellation, tag_attribute, tag_name_eq,
};
use super::{CustomBuilder, ExtractError, safe_name, symbol};
use crate::ImportBindingKind;

const MAXIMUM_TAGS: usize = 100_000;
const DETECTION_BYTES: usize = 8_192;

pub(super) fn extract(
    builder: &mut CustomBuilder<'_, '_>,
) -> Result<Option<FileParseStatus>, ExtractError> {
    if !detected(builder)? {
        return Ok(None);
    }
    let (tags, mut complete) = scan_tags(builder)?;
    let mut stack = ElementStack { frames: Vec::new() };
    for tag in tags {
        builder.check_cancelled()?;
        if !stack.read(builder, tag)? {
            complete = false;
            break;
        }
    }
    complete &= stack.frames.is_empty();
    Ok(Some(if complete {
        FileParseStatus::Parsed
    } else {
        FileParseStatus::Partial
    }))
}

fn detected(builder: &mut CustomBuilder<'_, '_>) -> Result<bool, ExtractError> {
    // Leave unrelated XML on its existing path without charging it for a
    // complete second token stream. Content-only detection is bounded.
    if !supports(builder.path(), &[]) {
        let source = builder.source();
        let prefix = &source[..source.floor_char_boundary(source.len().min(DETECTION_BYTES))];
        let markers = markup_tags(prefix);
        if !supports(builder.path(), &markers) {
            return Ok(false);
        }
        builder.budget.reserve_working_bytes(
            u64::try_from(2 * size_of::<MarkupTag<'_>>() * markers.len())
                .map_err(|_| ExtractError::OutputLimit)?,
        )?;
    }
    Ok(true)
}

struct ElementStack<'source> {
    frames: Vec<(MarkupTag<'source>, Option<usize>)>,
}

impl<'source> ElementStack<'source> {
    fn read(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        tag: MarkupTag<'source>,
    ) -> Result<bool, ExtractError> {
        if tag.closing {
            return self.close(builder, tag);
        }
        let parent = self.frames.iter().rev().find_map(|(_, position)| *position);
        let position = declaration(builder, tag, parent)?;
        references(builder, tag, position.or(parent))?;
        if !tag.self_closing {
            if self.frames.len() >= builder.maximum_ast_depth {
                return Err(ExtractError::NestingLimit);
            }
            builder.budget.reserve_working_bytes(
                u64::try_from(2 * size_of::<(MarkupTag<'_>, Option<usize>)>())
                    .map_err(|_| ExtractError::OutputLimit)?,
            )?;
            self.frames.push((tag, position));
        }
        Ok(true)
    }

    fn close(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        tag: MarkupTag<'_>,
    ) -> Result<bool, ExtractError> {
        let Some((opening, position)) = self.frames.pop() else {
            return Ok(false);
        };
        if opening.name != tag.name {
            return Ok(false);
        }
        if let Some(position) = position {
            builder.symbols[position].span = builder.span(opening.start, tag.end)?;
        }
        Ok(true)
    }
}

fn supports(path: &str, tags: &[MarkupTag<'_>]) -> bool {
    if path.replace('\\', "/").split('/').any(|part| {
        ["freeswitch", "dialplan", "autoload_configs", "sip_profiles"]
            .iter()
            .any(|name| part.eq_ignore_ascii_case(name))
    }) {
        return true;
    }
    let has = |name| {
        tags.iter()
            .any(|tag| !tag.closing && tag_name_eq(*tag, name))
    };
    tags.iter().any(|tag| {
        !tag.closing
            && (tag_name_eq(*tag, "X-PRE-PROCESS")
                || tag_name_eq(*tag, "document")
                    && tag_attribute(*tag, "type")
                        .is_some_and(|(_, value)| value == "freeswitch/xml")
                || tag_name_eq(*tag, "configuration")
                    && tag_attribute(*tag, "name").is_some_and(|(_, value)| {
                        value
                            .rsplit_once('.')
                            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("conf"))
                    }))
    }) || has("context") && has("extension")
        || has("profile") && has("settings")
        || has("gateway") && has("param")
        || has("domain") && has("user")
}

fn declaration(
    builder: &mut CustomBuilder<'_, '_>,
    tag: MarkupTag<'_>,
    parent: Option<usize>,
) -> Result<Option<usize>, ExtractError> {
    let (kind, attribute) = match tag.name {
        "section" | "context" | "domain" => (SymbolKind::Namespace, "name"),
        "extension" => (SymbolKind::Function, "name"),
        "configuration" | "profile" | "gateway" | "group" => (SymbolKind::Resource, "name"),
        "user" => (SymbolKind::Resource, "id"),
        "param" => (SymbolKind::Property, "name"),
        "variable" => (SymbolKind::Variable, "name"),
        _ => return Ok(None),
    };
    let Some((_, name)) = tag_attribute(tag, attribute).filter(|(_, name)| safe_name(name)) else {
        return Ok(None);
    };
    let prefix = parent.map_or("freeswitch", |position| {
        builder.symbols[position].qualified_name.as_str()
    });
    let qualified = format!("{prefix}::{}::{name}", tag.name);
    let parent = parent.map(|position| builder.symbols[position].id.clone());
    let position = builder.symbols.len();
    symbol(
        builder,
        (kind, name, qualified),
        (tag.start, tag.end),
        parent,
    )?;
    Ok(Some(position))
}

fn references(
    builder: &mut CustomBuilder<'_, '_>,
    tag: MarkupTag<'_>,
    owner: Option<usize>,
) -> Result<(), ExtractError> {
    let owner_id = owner.map(|position| builder.symbols[position].id.clone());
    if matches!(tag.name, "action" | "anti-action")
        && let Some((offset, name)) =
            tag_attribute(tag, "application").filter(|(_, name)| safe_name(name))
    {
        builder.add_reference(
            CustomReferenceInput::new(owner_id.clone(), name, ReferenceKind::Calls)
                .with_resolution(&format!("freeswitch::application::{name}"))
                .at(offset, offset + name.len()),
        )?;
    }
    if tag.name == "load"
        && let Some((offset, name)) =
            tag_attribute(tag, "module").filter(|(_, name)| safe_name(name))
    {
        builder.add_reference(
            CustomReferenceInput::new(owner_id.clone(), name, ReferenceKind::Imports)
                .with_resolution(&format!("freeswitch::module::{name}"))
                .at(offset, offset + name.len()),
        )?;
    }
    if tag.name != "X-PRE-PROCESS" {
        return Ok(());
    }
    let Some((_, command)) = tag_attribute(tag, "cmd") else {
        return Ok(());
    };
    let Some((offset, data)) = tag_attribute(tag, "data") else {
        return Ok(());
    };
    match command {
        "include" if !data.is_empty() && !data.contains(['$', '&']) => {
            builder.add_reference(
                CustomReferenceInput::new(owner_id.clone(), data, ReferenceKind::Imports)
                    .with_resolution(&format!("freeswitch::include::{data}"))
                    .at(offset, offset + data.len()),
            )?;
            builder.add_import_binding(
                &CustomImportInput::new(owner_id, data)
                    .with_kind(ImportBindingKind::IncludeQuoted)
                    .at(offset, offset + data.len()),
            )?;
        }
        "set" | "env-set" => {
            if let Some((name, _)) = data.split_once('=') {
                let name = name.trim();
                if safe_name(name) {
                    symbol(
                        builder,
                        (
                            SymbolKind::Constant,
                            name,
                            format!("freeswitch::variable::{name}"),
                        ),
                        (tag.start, tag.end),
                        owner_id,
                    )?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Tokenize once with an allocation ceiling and cancellation even inside huge
/// comments, CDATA and quoted attributes. DTDs are opaque and remain partial.
fn scan_tags<'source>(
    builder: &mut CustomBuilder<'source, '_>,
) -> Result<(Vec<MarkupTag<'source>>, bool), ExtractError> {
    let source = builder.source();
    let bytes = source.as_bytes();
    let mut tags = Vec::new();
    let mut cursor = MarkupCursor {
        bytes,
        position: 0,
        next_poll: 0,
    };
    while cursor.position < bytes.len() {
        cursor.poll(builder)?;
        if bytes[cursor.position] != b'<' {
            cursor.position += 1;
            continue;
        }
        let start = cursor.position;
        if let Some((terminator, prefix_length)) = cursor.opaque_region() {
            if !cursor.skip_opaque(builder, terminator, prefix_length)? {
                return Ok((tags, false));
            }
            continue;
        }
        if bytes[start..].starts_with(b"<!") || !cursor.tag_end(builder)? {
            return Ok((tags, false));
        }
        if let Some(tag) = parse_markup_tag(source, start, cursor.position) {
            retain_tag(builder, &mut tags, tag)?;
        }
        cursor.position += 1;
    }
    Ok((tags, true))
}

fn retain_tag<'source>(
    builder: &mut CustomBuilder<'_, '_>,
    tags: &mut Vec<MarkupTag<'source>>,
    tag: MarkupTag<'source>,
) -> Result<(), ExtractError> {
    if tags.len() >= MAXIMUM_TAGS {
        return Err(ExtractError::OutputLimit);
    }
    builder.budget.reserve_working_bytes(
        u64::try_from(2 * size_of::<MarkupTag<'_>>()).map_err(|_| ExtractError::OutputLimit)?,
    )?;
    tags.push(tag);
    Ok(())
}

struct MarkupCursor<'source> {
    bytes: &'source [u8],
    position: usize,
    next_poll: usize,
}

impl MarkupCursor<'_> {
    fn poll(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<(), ExtractError> {
        poll_cancellation(builder.cancelled, self.position, &mut self.next_poll)
    }

    fn opaque_region(&self) -> Option<(&'static [u8], usize)> {
        let tail = &self.bytes[self.position..];
        if tail.starts_with(b"<!--") {
            Some((b"-->", 4))
        } else if tail.starts_with(b"<![CDATA[") {
            Some((b"]]>", 9))
        } else {
            None
        }
    }

    fn skip_opaque(
        &mut self,
        builder: &mut CustomBuilder<'_, '_>,
        terminator: &[u8],
        prefix_length: usize,
    ) -> Result<bool, ExtractError> {
        self.position += prefix_length;
        while self.position < self.bytes.len()
            && !self.bytes[self.position..].starts_with(terminator)
        {
            self.poll(builder)?;
            self.position += 1;
        }
        if self.position == self.bytes.len() {
            return Ok(false);
        }
        self.position += terminator.len();
        Ok(true)
    }

    fn tag_end(&mut self, builder: &mut CustomBuilder<'_, '_>) -> Result<bool, ExtractError> {
        self.position += 1;
        let mut quote = None;
        while self.position < self.bytes.len() {
            self.poll(builder)?;
            match (quote, self.bytes[self.position]) {
                (Some(active), byte) if active == byte => quote = None,
                (None, b'\'' | b'"') => quote = Some(self.bytes[self.position]),
                (None, b'>') => return Ok(true),
                _ => {}
            }
            self.position += 1;
        }
        Ok(false)
    }
}
