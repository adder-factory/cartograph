//! Root ownership follows declared file-module edges, not directory proximity.
use super::{
    FileId, HashMap, RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndex, ResolutionIndexTarget,
    ResolutionRequest, StageItemFailure, joined_path, resolve_normalized_module_file,
    rust_inline_modules::ModuleScope, size_of, usize_to_u64,
};
use std::collections::{HashSet, VecDeque};

#[derive(Default)]
pub(super) struct RootIndex {
    owners: HashMap<FileId, Option<Owner>>,
}

struct Owner {
    root: FileId,
    parent: Option<FileId>,
}

pub(super) fn index<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let roots = target_roots(target, cancelled)?;
    root_edges(target, &roots, cancelled)?;
    let mut queue = VecDeque::new();
    for file in roots {
        if cancelled() {
            return Err(StageItemFailure);
        }
        merge(target, (&file, None), Some(&file))?;
        enqueue(target, &mut queue, &file)?;
    }
    while let Some(file) = queue.pop_front() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let root = target
            .index
            .rust_paths
            .roots
            .owners
            .get(&file)
            .and_then(Option::as_ref)
            .map(|owner| owner.root.clone());
        target.budget.charge(
            root.as_ref()
                .map_or(0, |file| usize_to_u64(file.as_str().len())),
        )?;
        let Some(edges) = target.index.rust_paths.modules.remove(&file) else {
            continue;
        };
        walk_edges(
            target,
            RootWalk {
                file: &file,
                root: root.as_ref(),
                edges: &edges,
                queue: &mut queue,
            },
            cancelled,
        )?;
        target.index.rust_paths.modules.insert(file, edges);
    }
    Ok(())
}

struct RootWalk<'a> {
    file: &'a FileId,
    root: Option<&'a FileId>,
    edges: &'a HashMap<String, Option<super::rust_path_resolution::ModuleEdge>>,
    queue: &'a mut VecDeque<FileId>,
}

fn walk_edges<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    input: RootWalk<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let RootWalk {
        file,
        root,
        edges,
        queue,
    } = input;
    for edge in edges.values().flatten() {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(child) = edge.file.as_ref() else {
            continue;
        };
        if merge(target, (child, Some(file)), root)? {
            enqueue(target, queue, child)?;
        }
    }
    Ok(())
}

fn target_roots<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<HashSet<FileId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut roots = HashSet::new();
    for (file, context) in &target.index.modules.files {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if context.language != "rust"
            || !super::rust_dependency_paths::target_root(target.index, &context.path)
        {
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<FileId>() + file.as_str().len()),
        )?;
        roots.try_reserve(1).map_err(|_| StageItemFailure)?;
        roots.insert(file.clone());
    }
    Ok(roots)
}

fn root_edges<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    roots: &HashSet<FileId>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for file in roots {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let context = target
            .index
            .modules
            .files
            .get(file)
            .ok_or(StageItemFailure)?;
        let Some(edges) = target.index.rust_paths.modules.get_mut(file) else {
            continue;
        };
        for (name, edge) in edges {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let Some(edge) = edge else {
                continue;
            };
            let path = joined_path(&context.directory, name)?;
            target
                .budget
                .charge(usize_to_u64(path.len() + size_of::<String>()))?;
            let file = resolve_normalized_module_file(&target.index.modules, &path, "rust");
            target
                .budget
                .charge(file.map_or(0, |file| usize_to_u64(file.as_str().len())))?;
            edge.file = file.cloned();
        }
    }
    Ok(())
}

fn merge(
    target: &mut ResolutionIndexTarget<'_>,
    location: (&FileId, Option<&FileId>),
    root: Option<&FileId>,
) -> Result<bool, StageItemFailure> {
    let (file, parent) = location;
    let owners = &mut target.index.rust_paths.roots.owners;
    if let Some(existing) = owners.get_mut(file) {
        let Some(owner) = existing else {
            return Ok(false);
        };
        if root != Some(&owner.root) {
            *existing = None;
            return Ok(true);
        }
        if owner.parent.as_ref() != parent {
            owner.parent = None;
        }
        return Ok(false);
    }
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(
                size_of::<(FileId, Option<Owner>)>()
                    + file.as_str().len()
                    + root.map_or(0, |file| file.as_str().len())
                    + parent.map_or(0, |file| file.as_str().len()),
            ),
    )?;
    owners.try_reserve(1).map_err(|_| StageItemFailure)?;
    owners.insert(
        file.clone(),
        root.map(|root| Owner {
            root: root.clone(),
            parent: parent.cloned(),
        }),
    );
    Ok(true)
}

fn enqueue(
    target: &mut ResolutionIndexTarget<'_>,
    queue: &mut VecDeque<FileId>,
    file: &FileId,
) -> Result<(), StageItemFailure> {
    target
        .budget
        .charge(usize_to_u64(size_of::<FileId>() + file.as_str().len()))?;
    queue.try_reserve(1).map_err(|_| StageItemFailure)?;
    queue.push_back(file.clone());
    Ok(())
}

pub(super) fn root<'a>(index: &'a ResolutionIndex, file: &FileId) -> Option<&'a FileId> {
    Some(&index.rust_paths.roots.owners.get(file)?.as_ref()?.root)
}

pub(super) fn starting_module<'a, 'path, Cancel>(
    index: &'a ResolutionIndex,
    query: (&'a FileId, &ResolutionRequest<'_>, &'path str),
    cancelled: &mut Cancel,
) -> Result<Option<(ModuleScope<'a>, &'path str)>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (file, request, path) = query;
    let Some(root) = root(index, file) else {
        return Ok(None);
    };
    let (first, mut suffix) = path.split_once("::").unwrap_or((path, ""));
    if first == "crate" {
        return Ok(Some((
            ModuleScope {
                file: root,
                inline: "",
                module: None,
            },
            suffix,
        )));
    }
    if !matches!(first, "self" | "super") {
        return Ok(None);
    }
    let Some(mut current) = super::rust_inline_modules::enclosing(index, request, cancelled)?
    else {
        return Ok(None);
    };
    if first == "super" {
        let Some(parent) = parent_module(index, current) else {
            return Ok(None);
        };
        current = parent;
    }
    while let Some(remaining) = suffix
        .strip_prefix("super::")
        .or_else(|| (suffix == "super").then_some(""))
    {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(parent) = parent_module(index, current) else {
            return Ok(None);
        };
        current = parent;
        suffix = remaining;
    }
    Ok(Some((current, suffix)))
}

fn parent_module<'a>(
    index: &'a ResolutionIndex,
    scope: ModuleScope<'a>,
) -> Option<ModuleScope<'a>> {
    if let Some(module) = scope.module {
        let Some(parent) = index.parents.get(module) else {
            return Some(ModuleScope {
                file: scope.file,
                inline: "",
                module: None,
            });
        };
        let (parent, evidence) = index.qualtype.owners.get_key_value(parent)?;
        return (evidence.kind == super::SymbolKind::Module).then_some(ModuleScope {
            file: scope.file,
            inline: &evidence.name,
            module: Some(parent),
        });
    }
    Some(ModuleScope {
        file: index
            .rust_paths
            .roots
            .owners
            .get(scope.file)?
            .as_ref()?
            .parent
            .as_ref()?,
        inline: "",
        module: None,
    })
}
