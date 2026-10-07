//! Receiver finalization consumes only complete wildcard and re-export indexes.

use super::{ResolutionIndexTarget, StageItemFailure};

pub(in super::super) fn finish<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    super::super::jvm_resolution::prepare_wildcards(target, cancelled)?;
    super::super::javascript_exports::prepare(target.index, target.budget, cancelled)?;
    // The receiver owner finishes inheritance before declared returns.
    super::finish_index(target, cancelled)
}
