use std::{collections::HashMap, mem::size_of};

use super::super::{DYNAMIC_DISPATCH_RESOLUTION_PREFIX, try_clone_text};
use super::{
    ExtractedReference, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ResolutionIndex,
    ResolveBudget, StageItemFailure, SymbolId, receiver_member, usize_to_u64,
};

struct ReceiverSite {
    owner: Option<SymbolId>,
    member_start: u64,
    receiver_name: String,
    proven: bool,
}

/// An exact span joins JavaScript's short dynamic companion to the full
/// receiver call. It cannot capture calls in arguments or another receiver.
#[derive(Default)]
pub(in super::super) struct ReceiverSites {
    by_end: HashMap<u64, Option<ReceiverSite>>,
}

impl ReceiverSites {
    pub(in super::super) fn new<Cancel>(
        references: &[ExtractedReference],
        (language, index, budget): (&str, &ResolutionIndex, &mut ResolveBudget),
        cancelled: &mut Cancel,
    ) -> Result<Self, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let mut sites = Self::default();
        for reference in references {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if reference.kind != ReferenceKind::Calls {
                continue;
            }
            let Some(member) = receiver_member(language, &reference.name) else {
                continue;
            };
            let end = reference.span.end_byte();
            // Normalized/truncated names and whitespace-bearing spans cannot
            // establish the member's byte position from the name alone.
            if end.saturating_sub(reference.span.start_byte()) != usize_to_u64(reference.name.len())
            {
                continue;
            }
            if let Some(existing) = sites.by_end.get_mut(&end) {
                *existing = None;
                continue;
            }
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<(u64, Option<ReceiverSite>)>()))
                    .saturating_add(usize_to_u64(reference.name.len()))
                    .saturating_add(
                        reference
                            .owner
                            .as_ref()
                            .map_or(0, |owner| usize_to_u64(owner.as_str().len())),
                    ),
            )?;
            sites.by_end.try_reserve(1).map_err(|_| StageItemFailure)?;
            sites.by_end.insert(
                end,
                Some(ReceiverSite {
                    owner: reference.owner.clone(),
                    member_start: end.saturating_sub(usize_to_u64(member.len())),
                    receiver_name: try_clone_text(&reference.name)?,
                    proven: super::class_scope::reference_proven(index, reference),
                }),
            );
        }
        Ok(sites)
    }

    pub(in super::super) fn lookup(&self, reference: &ExtractedReference) -> Option<&str> {
        let dynamic = reference
            .resolution_name
            .as_deref()?
            .strip_prefix(DYNAMIC_DISPATCH_RESOLUTION_PREFIX)?;
        let site = self.by_end.get(&reference.span.end_byte())?.as_ref()?;
        (site.owner == reference.owner
            && site.member_start == reference.span.start_byte()
            && reference
                .span
                .end_byte()
                .saturating_sub(reference.span.start_byte())
                == usize_to_u64(reference.name.len())
            && site.receiver_name.ends_with(&reference.name)
            && dynamic == reference.name)
            .then_some(site.receiver_name.as_str())
    }

    pub(in super::super) fn proven(&self, reference: &ExtractedReference) -> bool {
        self.lookup(reference).is_some()
            && self
                .by_end
                .get(&reference.span.end_byte())
                .and_then(Option::as_ref)
                .is_some_and(|site| site.proven)
    }
}
