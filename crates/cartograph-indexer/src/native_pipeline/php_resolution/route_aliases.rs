//! Case-folded route aliases, retained only for typed PHP class imports.

use std::collections::{HashMap, HashSet};

use super::super::SourceSpan;
use cartograph_extract::PHP_NAMESPACE_SCOPE_MODULE;

use super::{
    ExtractedImportBinding, FileId, Intent, NativeFileFacts, PHP_EXACT_RESOLUTION_PREFIX,
    PhpExactLookup, PhpFileIndexInput, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceKind, ResolveBudget,
    StageItemFailure, try_clone_text, usize_to_u64,
};

#[derive(Default)]
pub(super) struct RouteAliasIndex {
    files: HashMap<FileId, HashMap<String, Option<String>>>,
    blocked: HashSet<FileId>,
}

pub(in crate::native_pipeline) fn implicit_binding(
    binding: &ExtractedImportBinding,
    language: &str,
) -> bool {
    language == "php" && binding.module_specifier == PHP_NAMESPACE_SCOPE_MODULE
}

pub(super) enum AliasMatch<'index> {
    Missing,
    Blocked,
    Target(&'index str),
}

impl RouteAliasIndex {
    pub(super) fn lookup(&self, file_id: &FileId, alias: &str) -> AliasMatch<'_> {
        if self.blocked.contains(file_id) {
            return AliasMatch::Blocked;
        }
        match self
            .files
            .get(file_id)
            .and_then(|aliases| aliases.get(&alias.to_ascii_lowercase()))
        {
            None => AliasMatch::Missing,
            Some(None) => AliasMatch::Blocked,
            Some(Some(target)) => AliasMatch::Target(target),
        }
    }

    fn record(
        &mut self,
        input: (&FileId, &ExtractedImportBinding, &str),
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        let (file_id, binding, target) = input;
        if !self.files.contains_key(file_id) {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    + usize_to_u64(
                        size_of::<(FileId, HashMap<String, Option<String>>)>()
                            + file_id.as_str().len(),
                    ),
            )?;
            self.files.insert(file_id.clone(), HashMap::new());
        }
        let aliases = self.files.get_mut(file_id).ok_or(StageItemFailure)?;
        let name = binding.local_name.to_ascii_lowercase();
        if let Some(existing) = aliases.get_mut(&name) {
            *existing = None;
            return Ok(());
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(String, Option<String>)>() + name.len() + target.len()),
        )?;
        aliases.insert(name, Some(try_clone_text(target)?));
        Ok(())
    }
}

pub(super) fn index_file<Cancel>(
    input: PhpFileIndexInput<'_, '_, '_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let PhpFileIndexInput {
        index,
        file,
        budget,
    } = input;
    let bindings = collect_bindings(file, budget, cancelled)?;
    if bindings.namespace_count > 1 {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<FileId>() + file.file.file_id.as_str().len()),
        )?;
        index
            .route_aliases
            .blocked
            .insert(file.file.file_id.clone());
        return Ok(());
    }
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if reference.kind != ReferenceKind::References || reference.owner.is_some() {
            continue;
        }
        let Some(lookup) = reference
            .resolution_name
            .as_deref()
            .and_then(|name| name.strip_prefix(PHP_EXACT_RESOLUTION_PREFIX))
            .and_then(PhpExactLookup::parse)
            .filter(|lookup| lookup.intent == Intent::Class)
        else {
            continue;
        };
        if let Some(binding) = bindings.by_site.get(&reference.span) {
            index
                .route_aliases
                .record((&file.file.file_id, binding, lookup.key), budget)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct BindingSites<'file> {
    by_site: HashMap<SourceSpan, &'file ExtractedImportBinding>,
    namespace_count: u32,
}

fn collect_bindings<'file, Cancel>(
    file: &'file NativeFileFacts,
    budget: &mut ResolveBudget,
    cancelled: &mut Cancel,
) -> Result<BindingSites<'file>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut sites = BindingSites::default();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if implicit_binding(binding, "php") {
            sites.namespace_count = sites.namespace_count.saturating_add(1);
            continue;
        }
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                + usize_to_u64(size_of::<(SourceSpan, &ExtractedImportBinding)>()),
        )?;
        sites.by_site.insert(binding.span, binding);
    }
    Ok(sites)
}
