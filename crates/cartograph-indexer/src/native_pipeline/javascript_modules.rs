use super::{
    FRAMEWORK_CONVENTION_CONFIDENCE, ModuleFileMatch, ModulePathIndex, ModuleResolutionAttempt,
    ModuleResolutionRequest, ResolvedTarget, TypeScriptAliasModuleResolution,
    javascript_family_name, javascript_packages, module_file_match, normalize_root_module_path,
    resolve_framework_alias_file, resolve_typescript_alias_module, strip_any_module_extension,
    strip_module_extension,
};

const TS_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".d.ts", ".js", ".jsx", ".xsjs", ".xsjslib"];
const JS_EXTENSIONS: &[&str] = &[".js", ".jsx", ".xsjs", ".xsjslib"];
const JSX_EXTENSIONS: &[&str] = &[".jsx", ".js", ".xsjs", ".xsjslib"];
const TYPESCRIPT_EXTENSIONS: &[&str] =
    &[".ts", ".tsx", ".mts", ".cts", ".d.ts", ".d.mts", ".d.cts"];
const MJS_SUBSTITUTIONS: &[&str] = &[".mts", ".d.mts", ".mjs"];
const CJS_SUBSTITUTIONS: &[&str] = &[".cts", ".d.cts", ".cjs"];
const JS_SUBSTITUTIONS: &[&str] = &[".ts", ".tsx", ".d.ts", ".js", ".jsx"];

pub(super) fn module_language(language: &str) -> bool {
    javascript_family_name(language) || matches!(language, "vue" | "svelte" | "astro")
}

pub(super) fn resolve_normalized<'a>(
    modules: &'a ModulePathIndex,
    normalized: &str,
    language: &str,
) -> ModuleResolutionAttempt<'a> {
    if !module_language(language) {
        return ModuleResolutionAttempt::NotMatched;
    }
    if let Some(stem) = strip_any_module_extension(normalized) {
        return resolve_explicit(modules, (normalized, stem), language);
    }
    let extensions = match language {
        "javascript" => JS_EXTENSIONS,
        "jsx" => JSX_EXTENSIONS,
        _ => TS_EXTENSIONS,
    };
    let mut selected = extension_tier(modules, (normalized, language), extensions);
    if matches!(selected, ModuleFileMatch::Missing) && matches!(language, "javascript" | "jsx") {
        selected = extension_tier(modules, (normalized, language), &TS_EXTENSIONS[..3]);
    }
    module_attempt(&selected)
}

fn resolve_explicit<'a>(
    modules: &'a ModulePathIndex,
    (normalized, stem): (&str, &str),
    language: &str,
) -> ModuleResolutionAttempt<'a> {
    let extension = &normalized[stem.len()..];
    let substitutions = if extension.eq_ignore_ascii_case(".mjs") {
        Some(MJS_SUBSTITUTIONS)
    } else if extension.eq_ignore_ascii_case(".cjs") {
        Some(CJS_SUBSTITUTIONS)
    } else if extension.eq_ignore_ascii_case(".js") {
        Some(JS_SUBSTITUTIONS)
    } else {
        None
    };
    if let Some(substitutions) = substitutions {
        if matches!(language, "javascript" | "jsx") {
            let exact = module_file_match(modules.exact.get(normalized), &modules.files, language);
            if !matches!(exact, ModuleFileMatch::Missing) {
                return module_attempt(&exact);
            }
        }
        return module_attempt(&ranked_file_match(
            modules,
            (stem, language, false),
            substitutions,
        ));
    }
    let exact = module_file_match(modules.exact.get(normalized), &modules.files, language);
    module_attempt(&exact)
}

fn module_attempt<'a>(selected: &ModuleFileMatch<'a>) -> ModuleResolutionAttempt<'a> {
    match selected {
        ModuleFileMatch::Unique(file) => ModuleResolutionAttempt::Resolved(file),
        ModuleFileMatch::Missing => ModuleResolutionAttempt::NotMatched,
        ModuleFileMatch::Ambiguous => ModuleResolutionAttempt::Rejected,
    }
}

fn extension_tier<'a>(
    modules: &'a ModulePathIndex,
    (stem, language): (&str, &str),
    extensions: &[&str],
) -> ModuleFileMatch<'a> {
    match ranked_file_match(modules, (stem, language, true), extensions) {
        ModuleFileMatch::Missing => ranked_file_match(
            modules,
            (&format!("{stem}/index"), language, true),
            extensions,
        ),
        selected => selected,
    }
}

fn ranked_file_match<'a>(
    modules: &'a ModulePathIndex,
    (stem, language, components): (&str, &str, bool),
    extensions: &[&str],
) -> ModuleFileMatch<'a> {
    let candidates = modules
        .stem
        .get(stem)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut rank = usize::MAX;
    let mut selected = ModuleFileMatch::Missing;
    for file in candidates {
        let Some(target) = modules
            .files
            .get(file)
            .filter(|target| super::resolution_languages_compatible(language, &target.language))
        else {
            continue;
        };
        let Some(extension) = strip_module_extension(&target.path, &target.language)
            .and_then(|stem| target.path.get(stem.len()..))
        else {
            continue;
        };
        let Some(priority) = extension_priority(extension, extensions, components) else {
            continue;
        };
        if priority < rank {
            rank = priority;
            selected = ModuleFileMatch::Unique(file);
        } else if priority == rank {
            selected = ModuleFileMatch::Ambiguous;
        }
    }
    selected
}

fn extension_priority(extension: &str, extensions: &[&str], components: bool) -> Option<usize> {
    extensions
        .iter()
        .position(|candidate| extension.eq_ignore_ascii_case(candidate))
        .or_else(|| {
            components
                .then_some([".vue", ".svelte", ".astro"])?
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                .then_some(extensions.len())
        })
}

pub(super) fn conventional_paths(language: &str, specifier: &str) -> [Option<String>; 2] {
    if !module_language(language) {
        return [None, None];
    }
    if let Some(tail) = specifier.strip_prefix("$lib/") {
        return [normalize_root_module_path(&format!("src/lib/{tail}")), None];
    }
    if let Some(tail) = specifier.strip_prefix("@src/") {
        return [normalize_root_module_path(&format!("src/{tail}")), None];
    }
    for prefix in ["@/", "~/"] {
        if let Some(tail) = specifier.strip_prefix(prefix) {
            return [
                normalize_root_module_path(&format!("src/{tail}")),
                normalize_root_module_path(tail),
            ];
        }
    }
    if let Some(tail) = specifier.strip_prefix("@app/") {
        return [normalize_root_module_path(&format!("app/{tail}")), None];
    }
    if specifier.starts_with("src/") || specifier.starts_with("app/") {
        return [normalize_root_module_path(specifier), None];
    }
    [None, None]
}

pub(super) fn lower_fallback_target(
    target: &mut ResolvedTarget,
    modules: &ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) {
    if let Some(provenance) = fallback_provenance(modules, request) {
        target.confidence = FRAMEWORK_CONVENTION_CONFIDENCE;
        target.provenance = provenance;
    }
}

pub(super) fn fallback_provenance(
    modules: &ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<&'static str> {
    if !module_language(request.importing_language) {
        return None;
    }
    if let TypeScriptAliasModuleResolution::Resolved(_, fallback) =
        resolve_typescript_alias_module(modules, request)
    {
        return if super::nearest_typescript_alias_config(modules, request.importing_path)
            .is_some_and(|config| config.fallback)
        {
            Some("native-typescript-config-fallback")
        } else {
            fallback.then_some("native-cross-language-module-fallback")
        };
    }
    if javascript_packages::is_local(modules, request.specifier) {
        if javascript_packages::uses_fallback(modules, request) {
            return Some("native-workspace-package-fallback");
        }
        return javascript_packages::uses_cross_language_fallback(modules, request)
            .then_some("native-cross-language-module-fallback");
    }
    if resolve_framework_alias_file(modules, request).is_some() {
        return Some("native-conventional-alias");
    }
    if super::typescript_alias_matches(
        modules,
        super::TypeScriptAliasMatch {
            importing_path: request.importing_path,
            specifier: request.specifier,
            importing_language: request.importing_language,
        },
    ) {
        return Some("native-alias-direct-fallback");
    }
    super::normalize_relative_module_path(request.importing_path, request.specifier)
        .is_some_and(|candidate| {
            uses_cross_language_fallback(modules, &candidate, request.importing_language)
        })
        .then_some("native-cross-language-module-fallback")
}

pub(super) fn uses_cross_language_fallback(
    modules: &ModulePathIndex,
    candidate: &str,
    language: &str,
) -> bool {
    if !matches!(language, "javascript" | "jsx") || explicit_typescript_extension(candidate) {
        return false;
    }
    let ModuleResolutionAttempt::Resolved(file) = resolve_normalized(modules, candidate, language)
    else {
        return false;
    };
    modules
        .files
        .get(file)
        .is_some_and(|target| matches!(target.language.as_str(), "typescript" | "tsx"))
}

fn explicit_typescript_extension(specifier: &str) -> bool {
    strip_any_module_extension(specifier)
        .and_then(|stem| specifier.get(stem.len()..))
        .is_some_and(|extension| {
            TYPESCRIPT_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

pub(super) fn resolve_alias_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> Option<&'a super::FileId> {
    match resolve_typescript_alias_module(modules, request) {
        TypeScriptAliasModuleResolution::Resolved(file, _) => return Some(file),
        TypeScriptAliasModuleResolution::Ambiguous => return None,
        TypeScriptAliasModuleResolution::Unresolved
        | TypeScriptAliasModuleResolution::NotMatched => {}
    }
    match javascript_packages::resolve(modules, request) {
        ModuleResolutionAttempt::Resolved(file) => return Some(file),
        ModuleResolutionAttempt::Rejected => return None,
        ModuleResolutionAttempt::NotMatched => {}
    }
    match conventional_file(modules, request) {
        ModuleResolutionAttempt::Resolved(file) => return Some(file),
        ModuleResolutionAttempt::Rejected => return None,
        ModuleResolutionAttempt::NotMatched => {}
    }
    super::typescript_alias_matches(
        modules,
        super::TypeScriptAliasMatch {
            importing_path: request.importing_path,
            specifier: request.specifier,
            importing_language: request.importing_language,
        },
    )
    .then(|| normalize_root_module_path(request.specifier))?
    .and_then(|candidate| {
        super::resolve_normalized_module_file(modules, &candidate, request.importing_language)
    })
}

pub(super) fn conventional_file<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    for candidate in conventional_paths(request.importing_language, request.specifier)
        .into_iter()
        .flatten()
    {
        match resolve_normalized(modules, &candidate, request.importing_language) {
            ModuleResolutionAttempt::Resolved(file) => {
                return ModuleResolutionAttempt::Resolved(file);
            }
            ModuleResolutionAttempt::Rejected => return ModuleResolutionAttempt::Rejected,
            ModuleResolutionAttempt::NotMatched => {}
        }
    }
    ModuleResolutionAttempt::NotMatched
}
