//! Imported nominal lookup does not infer bindings of generic parameters.
//! Only a bounded, verified declaration header can establish their absence.

use std::collections::HashSet;

use super::{
    FileId, NativeFileFacts, NativeSymbolFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndex,
    ResolutionIndexContext, ResolutionRequest, StageItemFailure, SymbolId, SymbolKind, size_of,
    usize_to_u64,
};

const HEADER_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct Scopes {
    owners: HashSet<SymbolId>,
    unreadable: HashSet<FileId>,
}

pub(super) fn index_file<Cancel>(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(file.file.language.as_str(), "csharp" | "rust") {
        return Ok(());
    }
    let Some(snapshot) = super::qualtype_source::read(file, context)? else {
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<FileId>()))
                .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
        )?;
        index
            .qualtype
            .generics
            .unreadable
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        index
            .qualtype
            .generics
            .unreadable
            .insert(file.file.file_id.clone());
        return Ok(());
    };
    super::csharp_constructors::index_syntax(index, (file, snapshot.source()), context)?;
    super::namespace_types::index_syntax(index, (file, snapshot.source()), context)?;
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if !scope_kind(symbol.kind)
            || plain_header(symbol, (&file.file.language, snapshot.source()))
        {
            continue;
        }
        let id = &symbol.input.symbol_id;
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<SymbolId>()))
                .saturating_add(usize_to_u64(id.as_str().len())),
        )?;
        index
            .qualtype
            .generics
            .owners
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        index.qualtype.generics.owners.insert(id.clone());
    }
    Ok(())
}

fn scope_kind(kind: SymbolKind) -> bool {
    super::qualtype_resolution::nominal_candidate(kind)
        || matches!(kind, SymbolKind::Function | SymbolKind::Method)
}

fn plain_header(symbol: &NativeSymbolFacts, syntax: (&str, &str)) -> bool {
    let (language, source) = syntax;
    let Ok(start) = usize::try_from(symbol.input.start_byte) else {
        return false;
    };
    let Ok(end) = usize::try_from(symbol.input.end_byte) else {
        return false;
    };
    let Some(bytes) = source
        .as_bytes()
        .get(start..end.min(start.saturating_add(HEADER_BYTES)))
    else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some(text) = strip_attributes(text, language) else {
        return false;
    };
    let Some(stop) = text
        .as_bytes()
        .iter()
        .position(|byte| matches!(byte, b'{' | b'(' | b';' | b'=' | b':'))
    else {
        return false;
    };
    let Some(prefix) = text
        .get(..stop)
        .and_then(|header| header.trim_end().strip_suffix(&symbol.name))
    else {
        return false;
    };
    !prefix.contains(['[', ']', '"', '\'', '/'])
        && (language != "rust" || !prefix.contains('<'))
        && prefix.chars().last().is_some_and(char::is_whitespace)
}

fn strip_attributes<'a>(mut text: &'a str, language: &str) -> Option<&'a str> {
    // Rust attribute token trees may contain lifetimes and raw literals. Their
    // headers need AST evidence; this bounded C# attribute subset abstains.
    if language == "rust" {
        return (!text.trim_start().starts_with('#')).then_some(text);
    }
    loop {
        text = text.trim_start();
        let attribute = text.strip_prefix('#').unwrap_or(text);
        if !attribute.starts_with('[') {
            return Some(text);
        }
        text = attribute.get(attribute_end(attribute)?..)?;
    }
}

fn attribute_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut offset = 0;
    let mut depth = 0_usize;
    while let Some(byte) = bytes.get(offset) {
        match byte {
            b'[' => depth = depth.checked_add(1)?,
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return offset.checked_add(1);
                }
            }
            b'\'' | b'"' => {
                offset = quoted_end(bytes, offset)?;
                continue;
            }
            b'r' if bytes
                .get(offset.checked_add(1)?)
                .is_some_and(|next| matches!(next, b'#' | b'"')) =>
            {
                return None;
            }
            // Comments and interpolated/verbatim strings need richer syntax.
            b'/' | b'$' | b'@' => return None,
            _ => {}
        }
        offset = offset.checked_add(1)?;
    }
    None
}

fn quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = *bytes.get(start)?;
    if bytes.get(start..start.checked_add(3)?) == Some(&[quote, quote, quote]) {
        return None;
    }
    let mut offset = start.checked_add(1)?;
    while let Some(byte) = bytes.get(offset) {
        if *byte == quote {
            return offset.checked_add(1);
        }
        let step = if *byte == b'\\' { 2 } else { 1 };
        offset = offset.checked_add(step)?;
    }
    None
}

pub(super) fn blocked<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if index.qualtype.generics.unreadable.contains(request.file_id) {
        return Ok(true);
    }
    let mut owner = request.owner;
    for _ in 0..=index.parents.len() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(id) = owner else {
            return Ok(false);
        };
        if index.qualtype.generics.owners.contains(id) {
            return Ok(true);
        }
        owner = index.parents.get(id);
    }
    Ok(true)
}
