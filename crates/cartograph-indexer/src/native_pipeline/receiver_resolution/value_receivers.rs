//! An inherited or unknown value binding cannot prove a nominal receiver.

use super::{AncestryWalk, Class, MAX_ANCESTORS, ResolutionIndex, StageItemFailure, SymbolId};

pub(in super::super) fn unshadowed<Cancel>(
    index: &ResolutionIndex,
    (owner, name): (Option<&SymbolId>, &str),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(owner) = owner else {
        return Ok(true);
    };
    let Some(root) = index.receivers.classes.get(owner.as_str()) else {
        return Ok(false);
    };
    let mut walk = AncestryWalk::new(root);
    while walk.count > 0 {
        if cancelled() {
            return Err(StageItemFailure);
        }
        walk.count -= 1;
        let class = walk.pending[walk.count].take().ok_or(StageItemFailure)?;
        if !walk.observe(class) || uncertain(class, (&root.symbol_id, name)) {
            return Ok(false);
        }
        if !queue_parents(index, (class, &mut walk), cancelled)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn uncertain(class: &Class, (root, name): (&SymbolId, &str)) -> bool {
    class.fenced
        || (class.symbol_id != *root
            && (class.members.contains_key(name)
                || class.non_methods.contains(name)
                || class.assigned_members.contains(name)))
}

fn queue_parents<'index, Cancel>(
    index: &'index ResolutionIndex,
    (class, walk): (&'index Class, &mut AncestryWalk<'index>),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let parents = index.receivers.parents.get(class.symbol_id.as_str());
    for parent in parents.into_iter().flatten() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(parent) = parent
            .as_deref()
            .and_then(|id| index.receivers.classes.get(id))
        else {
            return Ok(false);
        };
        if walk.count == MAX_ANCESTORS {
            return Ok(false);
        }
        walk.pending[walk.count] = Some(parent);
        walk.count += 1;
    }
    Ok(true)
}
