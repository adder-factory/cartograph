//! A local Java enum constant requires the actual nominal expression receiver.

use super::super::{
    ReferenceKind, ReferenceResolution, ResolutionIndex, ResolutionRequest, ResolvedTarget,
    StageItemFailure, SymbolKind,
    qualified_member_resolution::{ReceiverLookup, resolve_receiver},
    resolution_candidates_for_file, select_candidate,
};

const PROVENANCE: &str = "native-java-enum-member";
const MAX_MEMBER_HEADER_BYTES: usize = 1_024;

pub(super) fn type_sites<Cancel>(
    (file, source): (&super::NativeFileFacts, &str),
    context: &mut super::ResolutionIndexContext<'_, Cancel>,
) -> Result<std::collections::HashSet<(usize, usize)>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut sites = std::collections::HashSet::new();
    if file.file.language != "java" {
        return Ok(sites);
    }
    for reference in &file.references {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if !matches!(
            reference.kind,
            ReferenceKind::TypeOf | ReferenceKind::Returns
        ) {
            continue;
        }
        context.budget.charge(
            super::RESOLUTION_MAP_NODE_ALLOWANCE + super::usize_to_u64(size_of::<(usize, usize)>()),
        )?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert((
            usize::try_from(reference.span.start_byte()).map_err(|_| StageItemFailure)?,
            usize::try_from(reference.span.end_byte()).map_err(|_| StageItemFailure)?,
        ));
    }
    method_names(&mut sites, (file, source), context)?;
    Ok(sites)
}

fn method_names<Cancel>(
    sites: &mut std::collections::HashSet<(usize, usize)>,
    (file, source): (&super::NativeFileFacts, &str),
    context: &mut super::ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for symbol in &file.symbols {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.kind != SymbolKind::Method {
            continue;
        }
        let start = usize::try_from(symbol.input.start_byte).map_err(|_| StageItemFailure)?;
        let end = usize::try_from(symbol.input.end_byte).map_err(|_| StageItemFailure)?;
        let Some(header) = source
            .get(start..end.min(start.saturating_add(MAX_MEMBER_HEADER_BYTES)))
            .and_then(|header| header.split_once('(').map(|(header, _)| header.trim_end()))
        else {
            continue;
        };
        if !header.ends_with(&symbol.name) {
            continue;
        }
        let end = start + header.len();
        context.budget.charge(
            super::RESOLUTION_MAP_NODE_ALLOWANCE + super::usize_to_u64(size_of::<(usize, usize)>()),
        )?;
        sites.try_reserve(1).map_err(|_| StageItemFailure)?;
        sites.insert((end - symbol.name.len(), end));
    }
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    (request, receiver): (&ResolutionRequest<'_>, &str),
    cancelled: &mut Cancel,
) -> Result<Option<ReferenceResolution>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let receiver_request = ResolutionRequest {
        name: receiver,
        kind: ReferenceKind::FieldAccess,
        ..*request
    };
    let ReceiverLookup::Resolved(parent) = resolve_receiver(index, &receiver_request, cancelled)?
    else {
        return Ok(None);
    };
    if parent.instance || parent.target.kind != SymbolKind::Enum {
        return Ok(None);
    }
    let candidate = select_candidate(
        resolution_candidates_for_file(index, request.name, request.file_id),
        |candidate| {
            candidate.kind == SymbolKind::EnumMember
                && candidate.parent_symbol_id.as_ref() == Some(&parent.target.symbol_id)
        },
        cancelled,
    )?;
    Ok(candidate.map(|candidate| {
        ReferenceResolution::resolved(ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: super::super::EXACT_LEXICAL_CONFIDENCE,
            provenance: PROVENANCE,
        })
    }))
}
