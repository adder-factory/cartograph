//! A Cargo path dependency identifies the local crate; package names alone do not.
use super::{
    FileId, HashMap, ImportBindingKind, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE,
    ResolutionIndex, ResolutionIndexTarget, StageItemFailure, joined_path,
    normalize_joined_project_path, rust_crate_identifier, rust_crate_module_directory, size_of,
    try_clone_text, usize_to_u64,
};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct DependencyIndex {
    by_source: HashMap<String, HashMap<String, Option<Dependency>>>,
    conventional_libraries: HashSet<String>,
    manifests: HashSet<String>,
    target_paths: HashSet<String>,
    auto_bins: HashSet<String>,
}

struct Dependency {
    directory: String,
    package: String,
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "toml"
        || file.file.normalized_path.rsplit('/').next() != Some("Cargo.toml")
    {
        return Ok(());
    }
    let directory = file
        .file
        .normalized_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let source = joined_path(directory, "src")?;
    register_manifest(target, directory)?;
    let mut dependencies = HashMap::<String, Option<Dependency>>::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        index_binding(
            target,
            ((directory, &file.file.normalized_path), &mut dependencies),
            binding,
        )?;
    }
    if !dependencies.is_empty() {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(
                    String,
                    HashMap<String, Option<Dependency>>,
                )>()))
                .saturating_add(usize_to_u64(source.len())),
        )?;
        target
            .index
            .rust_paths
            .dependencies
            .by_source
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target
            .index
            .rust_paths
            .dependencies
            .by_source
            .insert(source, dependencies);
    }
    Ok(())
}

fn index_binding(
    target: &mut ResolutionIndexTarget<'_>,
    state: ((&str, &str), &mut HashMap<String, Option<Dependency>>),
    binding: &super::ExtractedImportBinding,
) -> Result<(), StageItemFailure> {
    let ((directory, manifest), dependencies) = state;
    if binding.kind != ImportBindingKind::Namespace {
        return Ok(());
    }
    if binding.module_specifier == "<cargo-conventional-library>" {
        register_library(target, directory)?;
        return Ok(());
    }
    if binding.local_name.starts_with("<cargo-") {
        register_target(target, (directory, manifest), binding)?;
        return Ok(());
    }
    if let Some(existing) = dependencies.get_mut(&binding.local_name) {
        *existing = None;
        return Ok(());
    }
    let Some(directory) = normalize_joined_project_path(manifest, &binding.module_specifier) else {
        return Ok(());
    };
    let Some(package) = rust_crate_identifier(&binding.imported_name) else {
        return Ok(());
    };
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(String, Option<Dependency>)>()))
            .saturating_add(usize_to_u64(
                directory.len() + package.len() + binding.local_name.len(),
            )),
    )?;
    dependencies.try_reserve(1).map_err(|_| StageItemFailure)?;
    dependencies.insert(
        try_clone_text(&binding.local_name)?,
        Some(Dependency { directory, package }),
    );
    Ok(())
}

fn register_manifest(
    target: &mut ResolutionIndexTarget<'_>,
    directory: &str,
) -> Result<(), StageItemFailure> {
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(directory.len() + size_of::<String>()),
    )?;
    let manifests = &mut target.index.rust_paths.dependencies.manifests;
    manifests.try_reserve(1).map_err(|_| StageItemFailure)?;
    manifests.insert(try_clone_text(directory)?);
    Ok(())
}

fn register_library(
    target: &mut ResolutionIndexTarget<'_>,
    directory: &str,
) -> Result<(), StageItemFailure> {
    target.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(directory.len() + size_of::<String>())),
    )?;
    let libraries = &mut target.index.rust_paths.dependencies.conventional_libraries;
    libraries.try_reserve(1).map_err(|_| StageItemFailure)?;
    libraries.insert(try_clone_text(directory)?);
    Ok(())
}

pub(super) fn entry<'a>(index: &'a ResolutionIndex, query: (&str, &str)) -> Option<&'a FileId> {
    let (source, name) = query;
    let directory = source_directory(index, source)?;
    let dependency = index
        .rust_paths
        .dependencies
        .by_source
        .get(&directory)
        .and_then(|dependencies| dependencies.get(name));
    let package = if let Some(dependency) = dependency {
        let dependency = dependency.as_ref()?;
        index
            .modules
            .rust_packages
            .get(&dependency.package)?
            .as_ref()
            .filter(|package| package.directory == dependency.directory)?
    } else {
        let package = index.modules.rust_packages.get(name)?.as_ref()?;
        if directory != joined_path(&package.directory, "src").ok()? {
            return None;
        }
        let tests = joined_path(&package.directory, "tests").ok()?;
        source
            .strip_prefix(&tests)
            .filter(|suffix| suffix.starts_with('/'))?;
        package
    };
    index
        .rust_paths
        .dependencies
        .conventional_libraries
        .contains(&package.directory)
        .then_some(&package.entry_file_id)
}

fn source_directory(index: &ResolutionIndex, source: &str) -> Option<String> {
    nearest_manifest(index, source).map_or_else(
        || rust_crate_module_directory(&index.modules, source),
        |directory| joined_path(directory, "src").ok(),
    )
}

fn nearest_manifest<'a>(index: &ResolutionIndex, source: &'a str) -> Option<&'a str> {
    let mut directory = source
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    loop {
        if index.rust_paths.dependencies.manifests.contains(directory) {
            return Some(directory);
        }
        if directory.is_empty() {
            return None;
        }
        directory = directory
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
    }
}

pub(super) fn metadata_binding(language: &str, binding: &super::ExtractedImportBinding) -> bool {
    language == "toml" && binding.kind == ImportBindingKind::Namespace
}

fn register_target(
    target: &mut ResolutionIndexTarget<'_>,
    source: (&str, &str),
    binding: &super::ExtractedImportBinding,
) -> Result<(), StageItemFailure> {
    let (directory, manifest) = source;
    let (paths, path) = if binding.local_name == "<cargo-auto-bins>" {
        (
            &mut target.index.rust_paths.dependencies.auto_bins,
            try_clone_text(directory)?,
        )
    } else if binding.local_name == "<cargo-target-root>" {
        let Some(path) = normalize_joined_project_path(manifest, &binding.module_specifier) else {
            return Ok(());
        };
        (&mut target.index.rust_paths.dependencies.target_paths, path)
    } else {
        return Ok(());
    };
    target
        .budget
        .charge(RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<String>() + path.len()))?;
    paths.try_reserve(1).map_err(|_| StageItemFailure)?;
    paths.insert(path);
    Ok(())
}

pub(super) fn target_root(index: &ResolutionIndex, path: &str) -> bool {
    let dependencies = &index.rust_paths.dependencies;
    if dependencies.target_paths.contains(path) {
        return true;
    }
    if let Some(manifest) = nearest_manifest(index, path) {
        let Some(source) = joined_path(manifest, "src").ok() else {
            return false;
        };
        let Some(relative) = path
            .strip_prefix(&source)
            .and_then(|suffix| suffix.strip_prefix('/'))
        else {
            return false;
        };
        return relative == "lib.rs" && dependencies.conventional_libraries.contains(manifest)
            || (relative == "main.rs" || binary_root(relative))
                && dependencies.auto_bins.contains(manifest);
    }
    let filename = path.rsplit('/').next().unwrap_or(path);
    matches!(filename, "lib.rs" | "main.rs") || binary_root(path)
}

fn binary_root(path: &str) -> bool {
    let relative = path
        .strip_prefix("bin/")
        .or_else(|| path.strip_prefix("src/bin/"));
    relative.is_some_and(|path| {
        !path.contains('/')
            && std::path::Path::new(path).extension() == Some(std::ffi::OsStr::new("rs"))
            || path
                .strip_suffix("/main.rs")
                .is_some_and(|name| !name.contains('/'))
    })
}
