//! Module-file rules for the Lua, R, and Nix load conventions.
//!
//! Each rule maps a load specifier to project files by that language's own
//! search convention and nothing else: Lua `require("a.b")` (or `"a/b"`) follows the
//! default `package.path` templates `./?.lua;./?/init.lua` from the project
//! root, R `source("p")` evaluates the exact path from the working directory
//! taken to be the project root, and Nix `import ./p` loads the exact path or,
//! for a directory, `p/default.nix`. Ruby's file-relative loads, Luau and KHN
//! relative requires, and every other language keep the shared resolution;
//! Ruby load-path features and R packages are never guessed as files.

use super::{
    ModuleFileMatch, ModulePathIndex, ModuleResolutionAttempt, ModuleResolutionRequest,
    module_file_match, normalize_relative_module_path, normalize_root_module_path,
    resolve_normalized_module_file,
};

/// Lua `package.path` templates relative to the project root, in search order.
const LUA_MODULE_TEMPLATES: [&str; 2] = ["", "/init"];
/// File loaded when a Nix path names a directory.
const NIX_DIRECTORY_ENTRY: &str = "/default.nix";
/// Longest specifier a script-load rule will rewrite.
const MAXIMUM_SCRIPT_SPECIFIER_BYTES: usize = 1_024;

/// Whether a bound load in `language` resolves only through its module file.
///
/// These loads name a module, not a declaration, so a same-file symbol that
/// happens to share the specifier (`local json = require("json")`) must never
/// become the import's target through lexical lookup.
pub(super) fn binds_loads_exactly(language: &str) -> bool {
    matches!(language, "ruby" | "lua" | "luau" | "khn" | "nix" | "r")
}

/// Resolve `request` by its language's load convention, or report that no
/// script rule applies so the shared resolution continues.
pub(super) fn resolve_script_module<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    if request.specifier.is_empty()
        || request.specifier.len() > MAXIMUM_SCRIPT_SPECIFIER_BYTES
        || request.specifier.contains(['\\', '\0'])
    {
        return ModuleResolutionAttempt::NotMatched;
    }
    match request.importing_language {
        "lua" => resolve_lua_module(modules, request),
        "r" => resolve_r_source(modules, request),
        "nix" => resolve_nix_path(modules, request),
        _ => ModuleResolutionAttempt::NotMatched,
    }
}

fn resolve_r_source<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    let Some(path) = is_root_path(request.specifier)
        .then(|| normalize_root_module_path(request.specifier))
        .flatten()
    else {
        return ModuleResolutionAttempt::Rejected;
    };
    match exact_file(modules, &path, request.importing_language) {
        ModuleFileMatch::Unique(file_id) => ModuleResolutionAttempt::Resolved(file_id),
        ModuleFileMatch::Missing | ModuleFileMatch::Ambiguous => ModuleResolutionAttempt::Rejected,
    }
}

fn resolve_lua_module<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    let specifier = request.specifier;
    // `package.searchpath` turns each `.` into a directory separator and
    // keeps an explicit `/`, so `a.b` and `a/b` both name `a/b.lua`; an empty
    // segment (`./a`, `a..b`, `/a`) has no module-name form.
    let named = specifier
        .split(['.', '/'])
        .all(|segment| !segment.is_empty());
    if !named {
        return ModuleResolutionAttempt::Rejected;
    }
    let path = specifier.replace('.', "/");
    for template in LUA_MODULE_TEMPLATES {
        let Some(candidate) = normalize_root_module_path(&format!("{path}{template}")) else {
            return ModuleResolutionAttempt::Rejected;
        };
        if let Some(file_id) =
            resolve_normalized_module_file(modules, &candidate, request.importing_language)
        {
            return ModuleResolutionAttempt::Resolved(file_id);
        }
    }
    ModuleResolutionAttempt::Rejected
}

fn resolve_nix_path<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    if !(request.specifier.starts_with("./") || request.specifier.starts_with("../")) {
        return ModuleResolutionAttempt::NotMatched;
    }
    // A path naming the project root itself (`../.` from `nix/`) has no file
    // form, so only its directory entry is probed.
    if let Some(path) = normalize_relative_module_path(request.importing_path, request.specifier) {
        match exact_file(modules, &path, request.importing_language) {
            ModuleFileMatch::Unique(file_id) => return ModuleResolutionAttempt::Resolved(file_id),
            ModuleFileMatch::Ambiguous => return ModuleResolutionAttempt::Rejected,
            ModuleFileMatch::Missing => {}
        }
    }
    let Some(entry) = normalize_relative_module_path(
        request.importing_path,
        &format!("{}{NIX_DIRECTORY_ENTRY}", request.specifier),
    ) else {
        return ModuleResolutionAttempt::Rejected;
    };
    match exact_file(modules, &entry, request.importing_language) {
        ModuleFileMatch::Unique(file_id) => ModuleResolutionAttempt::Resolved(file_id),
        ModuleFileMatch::Missing | ModuleFileMatch::Ambiguous => ModuleResolutionAttempt::Rejected,
    }
}

fn exact_file<'a>(
    modules: &'a ModulePathIndex,
    path: &str,
    importing_language: &str,
) -> ModuleFileMatch<'a> {
    module_file_match(modules.exact.get(path), &modules.files, importing_language)
}

/// A path inside the project root, never an absolute, home, or URL path.
fn is_root_path(specifier: &str) -> bool {
    !specifier.starts_with(['/', '~']) && !specifier.contains("://")
}
