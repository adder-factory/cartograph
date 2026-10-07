//! Parent namespace types precede root using namespaces for a simple type name.

use super::{
    NamespaceUsings, ResolutionIndex, ResolutionRequest, Selection, StageItemFailure,
    qualified_key, retain_qualified,
};

const MAX_NAMESPACE_HOPS: usize = 64;

pub(super) fn select<'index, Cancel>(
    index: &'index ResolutionIndex,
    (request, imports, enclosing, name): (&ResolutionRequest<'_>, &NamespaceUsings, &str, &str),
    cancelled: &mut Cancel,
) -> Result<Selection<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut selected = Selection::default();
    let mut namespace = enclosing;
    for _ in 0..MAX_NAMESPACE_HOPS {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some((parent, _)) = namespace
            .rsplit_once('.')
            .or_else(|| namespace.rsplit_once("::"))
        else {
            return imported(index, (request, imports, name), cancelled);
        };
        namespace = parent;
        let key = qualified_key(namespace, name)?;
        if index.candidates.contains_key(&key) {
            retain_qualified(&mut selected, (index, request, namespace, name), cancelled)?;
            return Ok(selected);
        }
    }
    Ok(selected)
}

fn imported<'index, Cancel>(
    index: &'index ResolutionIndex,
    (request, imports, name): (&ResolutionRequest<'_>, &NamespaceUsings, &str),
    cancelled: &mut Cancel,
) -> Result<Selection<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut selected = Selection::default();
    for namespace in &imports.namespaces {
        retain_qualified(&mut selected, (index, request, namespace, name), cancelled)?;
        if selected.ambiguous {
            break;
        }
    }
    Ok(selected)
}
