//! Compact resolved subscriptions survive spill and call-site retention policy.

use std::collections::{BTreeMap, BTreeSet};

use super::super::{
    ExtractedReference, ReferenceKind, ReferenceResolution, ResolutionIndexTarget, ResolveBudget,
    StageItemFailure, SymbolId, ordered_resolution_candidates, usize_to_u64,
};

const ENTRY_BYTES: u64 = 128;

#[derive(Default)]
pub(in crate::native_pipeline) struct ConsumerIndex(BTreeSet<SymbolId>);

#[derive(Default)]
pub(in crate::native_pipeline) struct HandlerIndex {
    pub(super) targets: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
}

pub(in crate::native_pipeline) struct HandlerObservation<'observation> {
    pub(in crate::native_pipeline) consumers: &'observation ConsumerIndex,
    pub(in crate::native_pipeline) reference: &'observation ExtractedReference,
    pub(in crate::native_pipeline) resolution: &'observation ReferenceResolution,
    pub(in crate::native_pipeline) budget: &'observation mut ResolveBudget,
}

pub(in crate::native_pipeline) fn prepare<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut consumers = ConsumerIndex::default();
    for (name, candidates) in ordered_resolution_candidates(target.index) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if name.contains("::") {
            continue;
        }
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if candidate
                .qualified_name
                .contains("::react-native-event-consumer::")
            {
                target
                    .budget
                    .charge(ENTRY_BYTES + usize_to_u64(candidate.symbol_id.as_str().len()))?;
                consumers.0.insert(candidate.symbol_id.clone());
            }
        }
    }
    target.index.frameworks.native_event_consumers = consumers;
    Ok(())
}

impl HandlerIndex {
    pub(in crate::native_pipeline) fn record(
        &mut self,
        observation: HandlerObservation<'_>,
    ) -> Result<(), StageItemFailure> {
        let HandlerObservation {
            consumers,
            reference,
            resolution,
            budget,
        } = observation;
        let Some(owner) = reference
            .owner
            .as_ref()
            .filter(|id| consumers.0.contains(id))
        else {
            return Ok(());
        };
        if reference.kind != ReferenceKind::Calls {
            return Ok(());
        }
        if let Some(target) = resolution.target.as_ref() {
            self.insert((owner, &target.symbol_id), budget)?;
        }
        Ok(())
    }

    pub(in crate::native_pipeline) fn merge<Cancel>(
        &mut self,
        input: (Self, &mut ResolveBudget),
        cancelled: &mut Cancel,
    ) -> Result<(), StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (other, budget) = input;
        for (owner, targets) in other.targets {
            for target in targets {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                self.insert((&owner, &target), budget)?;
            }
        }
        Ok(())
    }

    fn insert(
        &mut self,
        (owner, target): (&SymbolId, &SymbolId),
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        if self
            .targets
            .get(owner)
            .is_some_and(|targets| targets.contains(target))
        {
            return Ok(());
        }
        budget.charge(ENTRY_BYTES + usize_to_u64(owner.as_str().len() + target.as_str().len()))?;
        self.targets
            .entry(owner.clone())
            .or_default()
            .insert(target.clone());
        Ok(())
    }
}
