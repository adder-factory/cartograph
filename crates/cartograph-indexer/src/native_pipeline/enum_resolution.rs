//! Qualified enum members require a source-proven receiver and exact containment.
//! The sealed extractor occurrence stays unchanged; only a bounded identifier
//! beside its exact span is read from a content-verified source snapshot.

use super::{
    FileId, HashMap, LexicalScopeQuery, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ReferenceDispatch, ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionIndexContext,
    ResolutionRequest, StageItemFailure, SymbolKind, qualtype_resolution::Selection,
    resolution_candidates_for_file, resolve_lexical, resolve_lexical_scope, size_of,
    try_clone_text, usize_to_u64,
};

pub(super) const PROVENANCE: &str = "native-enum-qualified-member";
const RECEIVER_CONTEXT_BYTES: usize = 256;

#[derive(Default)]
pub(super) struct Receivers {
    by_file: HashMap<FileId, HashMap<u64, String>>,
}

struct NameUses {
    start: usize,
    end: usize,
    declaration_seen: bool,
    valid: bool,
}

#[derive(Clone, Copy)]
struct WordUse<'a> {
    start: usize,
    word: &'a str,
    previous: Option<(usize, &'a str)>,
}

pub(super) fn index_file<Cancel>(
    index: &mut ResolutionIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !supported(&file.file.language) || !contains_enum(file, context.cancelled)? {
        return Ok(());
    }
    let Some(snapshot) = super::qualtype_source::read(file, context)? else {
        return Ok(());
    };
    // Escaped or non-ASCII identifiers require richer lexical evidence.
    if !snapshot.source().is_ascii() || snapshot.source().contains('\\') {
        return Ok(());
    }
    let names = clean_names((file, snapshot.source()), context)?;
    let mut receivers = HashMap::new();
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::FieldAccess {
            continue;
        }
        let start = usize::try_from(reference.span.start_byte()).map_err(|_| StageItemFailure)?;
        let end = usize::try_from(reference.span.end_byte()).map_err(|_| StageItemFailure)?;
        if snapshot.source().get(start..end) != Some(reference.name.as_str()) {
            continue;
        }
        let Some(receiver) = receiver_before(snapshot.source(), start) else {
            continue;
        };
        if names
            .get(receiver)
            .is_none_or(|uses| !uses.valid || !uses.declaration_seen)
        {
            continue;
        }
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(u64, String)>()))
                .saturating_add(usize_to_u64(receiver.len())),
        )?;
        receivers.try_reserve(1).map_err(|_| StageItemFailure)?;
        receivers.insert(reference.span.start_byte(), try_clone_text(receiver)?);
    }
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(FileId, HashMap<u64, String>)>()))
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    index
        .qualtype
        .enums
        .by_file
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    index
        .qualtype
        .enums
        .by_file
        .insert(file.file.file_id.clone(), receivers);
    Ok(())
}

fn clean_names<'a, Cancel>(
    syntax: (&'a NativeFileFacts, &str),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<HashMap<&'a str, NameUses>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, source) = syntax;
    let mut names = HashMap::new();
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Enum {
            continue;
        }
        if let Some(previous) = names.get_mut(symbol.name.as_str()) {
            let previous: &mut NameUses = previous;
            previous.valid = false;
            continue;
        }
        context.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(&str, NameUses)>())),
        )?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(
            symbol.name.as_str(),
            NameUses {
                start: usize::try_from(symbol.input.start_byte).map_err(|_| StageItemFailure)?,
                end: usize::try_from(symbol.input.end_byte).map_err(|_| StageItemFailure)?,
                declaration_seen: false,
                valid: true,
            },
        );
    }
    let mut previous = None;
    let mut offset = 0;
    for chunk in source.split_inclusive(|character: char| !identifier_character(character)) {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        let start = offset;
        offset += chunk.len();
        let word = chunk.trim_end_matches(|character: char| !identifier_character(character));
        if word.is_empty() {
            continue;
        }
        if let Some(uses) = names.get_mut(word) {
            uses.valid &= allowed_occurrence(
                uses,
                WordUse {
                    start,
                    word,
                    previous,
                },
                source,
            );
        }
        previous = Some((start + word.len(), word));
    }
    Ok(names)
}

fn identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '$')
}

fn allowed_occurrence(uses: &mut NameUses, token: WordUse<'_>, source: &str) -> bool {
    let declaration = token.previous.is_some_and(|(end, word)| {
        word == "enum"
            && source[end..token.start]
                .bytes()
                .all(|byte| byte.is_ascii_whitespace())
    }) && token.start >= uses.start
        && token.start + token.word.len() <= uses.end;
    if declaration && !uses.declaration_seen {
        uses.declaration_seen = true;
        return true;
    }
    source[token.start + token.word.len()..]
        .trim_start()
        .starts_with('.')
}

fn supported(language: &str) -> bool {
    matches!(
        language,
        "typescript" | "javascript" | "tsx" | "svelte" | "vue"
    )
}

fn contains_enum<Cancel>(
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if symbol.kind == SymbolKind::Enum {
            return Ok(true);
        }
    }
    Ok(false)
}

fn receiver_before(source: &str, start: usize) -> Option<&str> {
    let context = source
        .as_bytes()
        .get(start.saturating_sub(RECEIVER_CONTEXT_BYTES)..start)?;
    let prefix = context
        .trim_ascii_end()
        .strip_suffix(b".")?
        .trim_ascii_end();
    let split = prefix
        .iter()
        .rposition(|byte| !identifier_byte(*byte))
        .map_or(0, |at| at + 1);
    let receiver = prefix.get(split..)?;
    if receiver.is_empty() || receiver[0].is_ascii_digit() {
        return None;
    }
    let preceding = prefix.get(..split)?.trim_ascii_end();
    if preceding.ends_with(b"::")
        || preceding
            .last()
            .is_some_and(|byte| !byte.is_ascii() || matches!(byte, b'.' | b']' | b')' | b'\\'))
    {
        return None;
    }
    // A context boundary must not truncate a longer identifier or chain.
    if split == 0 && start > RECEIVER_CONTEXT_BYTES {
        return None;
    }
    std::str::from_utf8(receiver).ok()
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if request.kind != ReferenceKind::FieldAccess || request.dispatch != ReferenceDispatch::Static {
        return Ok(None);
    }
    let Some(receiver) = index
        .qualtype
        .enums
        .by_file
        .get(request.file_id)
        .and_then(|sites| sites.get(&request.span.start_byte()))
    else {
        return Ok(None);
    };
    if !local_enum_name(index, (request, receiver), cancelled)? {
        return Ok(None);
    }
    let parent_request = ResolutionRequest {
        name: receiver,
        kind: ReferenceKind::References,
        ..*request
    };
    let scoped = resolve_lexical_scope(
        index,
        LexicalScopeQuery {
            request: &parent_request,
            candidates: resolution_candidates_for_file(index, receiver, request.file_id),
        },
        cancelled,
    )?;
    let parent = match scoped {
        Some(parent) => Some(parent),
        // Embedded script roots can retain a component containment boundary
        // while declaring a file-level, unqualified enum. The whole-file
        // receiver check precedes this root lookup.
        None => resolve_lexical(index, &parent_request, cancelled)?,
    };
    let Some(parent) = parent else {
        return Ok(None);
    };
    if parent.kind != SymbolKind::Enum {
        return Ok(None);
    }
    let mut selected = Selection::default();
    for candidate in resolution_candidates_for_file(index, request.name, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind == SymbolKind::EnumMember
            && candidate.parent_symbol_id.as_ref() == Some(&parent.symbol_id)
        {
            selected.retain(candidate);
        }
    }
    Ok(selected.resolution(PROVENANCE, 1.0))
}

fn local_enum_name<Cancel>(
    index: &ResolutionIndex,
    query: (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (request, receiver) = query;
    for candidate in resolution_candidates_for_file(index, receiver, request.file_id) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if candidate.kind == SymbolKind::Enum {
            return Ok(true);
        }
    }
    Ok(false)
}
