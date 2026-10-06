use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};

use super::{
    BTreeMap, BTreeSet, HashMap, MAXIMUM_TYPESCRIPT_PATH_MAPPINGS,
    MAXIMUM_TYPESCRIPT_PATH_SUBSTITUTIONS, MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES, ModulePathIndex,
    ModuleResolutionAttempt, ModuleResolutionRequest, RESOLUTION_MAP_NODE_ALLOWANCE,
    ResolutionIndexContext, SourceSnapshot, StageItemFailure, Value, fmt, javascript_modules,
    normalize_typescript_alias_target, resolve_normalized_module_file, size_of,
    substitute_module_alias, try_clone_text, typescript_alias_tail, usize_to_u64,
    valid_typescript_alias_text,
};

#[derive(Default)]
pub(super) struct PackageIndex {
    by_name: HashMap<String, Option<WorkspacePackage>>,
}

struct WorkspacePackage {
    directory: String,
    exports: Option<BTreeMap<String, Option<String>>>,
    config_redirect: bool,
    entry: PackageEntry,
    versioned_types: bool,
}

enum PackageEntry {
    Unspecified,
    Target(String),
    Unresolved,
}

#[derive(Deserialize)]
struct ExportsDocument {
    exports: ExportValue,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExportValue {
    Text(String),
    Array(Vec<Self>),
    Object(OrderedObject),
    Unsupported(serde::de::IgnoredAny),
}

struct OrderedObject(Vec<(String, ExportValue)>);

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(OrderedVisitor)
    }
}

struct OrderedVisitor;

impl<'de> Visitor<'de> for OrderedVisitor {
    type Value = OrderedObject;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded ordered exports object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some((key, value)) = access.next_entry::<String, ExportValue>()? {
            if entries.len() >= MAXIMUM_TYPESCRIPT_PATH_MAPPINGS || !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(
                    "invalid or excessive exports keys",
                ));
            }
            entries.push((key, value));
        }
        Ok(OrderedObject(entries))
    }
}

pub(super) fn index_package<Cancel>(
    modules: &mut ModulePathIndex,
    snapshot: &SourceSnapshot,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Ok(parsed) = serde_json::from_str::<Value>(snapshot.source()) else {
        return Ok(());
    };
    let Some(name) = parsed
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| valid_package_name(name))
    else {
        return Ok(());
    };
    let directory = snapshot
        .path()
        .as_str()
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let exports = if parsed.get("exports").is_some() {
        context
            .budget
            .charge(snapshot.byte_size().saturating_mul(8))?;
        let exports = match serde_json::from_str::<ExportsDocument>(snapshot.source()) {
            Ok(document) => export_entries(&document.exports, context)?,
            Err(_) => BTreeMap::new(),
        };
        Some(exports)
    } else {
        None
    };
    let entry = entry_point(&parsed)?;
    let package = WorkspacePackage {
        directory: try_clone_text(directory)?,
        exports,
        config_redirect: parsed.get("main").is_some() || parsed.get("tsconfig").is_some(),
        entry,
        versioned_types: parsed.get("typesVersions").is_some(),
    };
    let entry_bytes = match &package.entry {
        PackageEntry::Target(target) => target.len(),
        PackageEntry::Unspecified | PackageEntry::Unresolved => 0,
    };
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(name.len() + directory.len() + entry_bytes)
            + usize_to_u64(size_of::<WorkspacePackage>()),
    )?;
    if let Some(existing) = modules.javascript_packages.by_name.get_mut(name) {
        *existing = None;
    } else {
        modules
            .javascript_packages
            .by_name
            .insert(try_clone_text(name)?, Some(package));
    }
    Ok(())
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES
        && !name.contains(['\\', '\0', '*', ':'])
        && package_parts(name).is_some_and(|(package, suffix)| package == name && suffix.is_empty())
}

fn entry_point(parsed: &Value) -> Result<PackageEntry, StageItemFailure> {
    if parsed.get("typesVersions").is_some() {
        return Ok(PackageEntry::Unresolved);
    }
    for field in ["types", "typings", "main"] {
        if let Some(value) = parsed.get(field) {
            let target = value
                .as_str()
                .filter(|text| valid_typescript_alias_text(text) && !text.starts_with('/'));
            return target.map_or(Ok(PackageEntry::Unresolved), |target| {
                try_clone_text(target).map(PackageEntry::Target)
            });
        }
    }
    Ok(PackageEntry::Unspecified)
}

fn export_entries<Cancel>(
    value: &ExportValue,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<BTreeMap<String, Option<String>>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut exports = BTreeMap::new();
    if let ExportValue::Object(OrderedObject(object)) = value
        && object.iter().any(|(key, _)| key.starts_with('.'))
    {
        if object.iter().any(|(key, _)| !key.starts_with('.')) {
            return Ok(exports);
        }
        for (pattern, value) in object {
            if (context.cancelled)() {
                return Err(StageItemFailure);
            }
            if !valid_typescript_alias_text(pattern)
                || !(pattern == "." || pattern.starts_with("./"))
            {
                return Ok(BTreeMap::new());
            }
            insert_export(&mut exports, (pattern, value), context)?;
        }
    } else {
        insert_export(&mut exports, (".", value), context)?;
    }
    Ok(exports)
}

fn insert_export<Cancel>(
    exports: &mut BTreeMap<String, Option<String>>,
    (pattern, value): (&str, &ExportValue),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let target = entry_target(value, 0, context.cancelled)?;
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(pattern.len() + target.as_ref().map_or(0, String::len)),
    )?;
    exports.insert(try_clone_text(pattern)?, target);
    Ok(())
}

fn entry_target<Cancel>(
    value: &ExportValue,
    depth: usize,
    cancelled: &mut Cancel,
) -> Result<Option<String>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    if depth > 8 {
        return Ok(None);
    }
    match value {
        ExportValue::Text(target) if valid_typescript_alias_text(target) => {
            Ok(Some(try_clone_text(target)?))
        }
        ExportValue::Array(array) => array_target(array, depth, cancelled),
        ExportValue::Object(OrderedObject(object)) => condition_target(object, depth, cancelled),
        ExportValue::Text(_) | ExportValue::Unsupported(_) => Ok(None),
    }
}

fn array_target<Cancel>(
    array: &[ExportValue],
    depth: usize,
    cancelled: &mut Cancel,
) -> Result<Option<String>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if array.len() > MAXIMUM_TYPESCRIPT_PATH_SUBSTITUTIONS {
        return Err(StageItemFailure);
    }
    // An unresolved first entry may be conditional or terminal null. Without
    // proving it can be skipped, selecting a later entry would guess a target.
    array
        .first()
        .map_or(Ok(None), |entry| entry_target(entry, depth + 1, cancelled))
}

fn condition_target<Cancel>(
    object: &[(String, ExportValue)],
    depth: usize,
    cancelled: &mut Cancel,
) -> Result<Option<String>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    // Types/default always match TypeScript. Other conditions depend on emit
    // mode/custom conditions not established by these facts, so abstain there.
    let Some((condition, entry)) = object.first() else {
        return Ok(None);
    };
    if !matches!(condition.as_str(), "types" | "default") {
        return Ok(None);
    }
    entry_target(entry, depth + 1, cancelled)
}

fn package_parts(specifier: &str) -> Option<(&str, &str)> {
    if specifier.starts_with(['.', '/', '#']) || specifier.contains(['\\', '\0', ':']) {
        return None;
    }
    let package_end = if specifier.starts_with('@') {
        let first = specifier.find('/')?;
        if first <= 1 {
            return None;
        }
        let rest = &specifier[first + 1..];
        if rest.is_empty() {
            return None;
        }
        rest.find('/')
            .map_or(specifier.len(), |position| first + 1 + position)
    } else {
        specifier.find('/').unwrap_or(specifier.len())
    };
    (package_end > 0).then(|| (&specifier[..package_end], &specifier[package_end..]))
}

fn package<'a>(
    modules: &'a ModulePathIndex,
    specifier: &str,
) -> Option<(&'a WorkspacePackage, String)> {
    let (name, suffix) = package_parts(specifier)?;
    let package = modules.javascript_packages.by_name.get(name)?.as_ref()?;
    let subpath = if suffix.is_empty() {
        ".".to_owned()
    } else {
        format!(".{suffix}")
    };
    Some((package, subpath))
}

fn export_target(exports: &BTreeMap<String, Option<String>>, subpath: &str) -> Option<String> {
    if subpath != "." && !valid_export_target(subpath) {
        return None;
    }
    if let Some(exact) = exports.get(subpath) {
        return exact.clone();
    }
    let (pattern, value) = exports
        .iter()
        .filter(|(pattern, _)| {
            pattern.contains('*') && typescript_alias_tail(subpath, pattern).is_some()
        })
        .max_by_key(|(pattern, _)| (pattern.find('*').unwrap_or(0), pattern.len()))?;
    let capture = typescript_alias_tail(subpath, pattern)?;
    value
        .as_deref()
        .map(|target| substitute_module_alias(target, capture))
}

fn package_candidate(package: &WorkspacePackage, subpath: &str) -> Option<String> {
    if package.exports.is_none() && package.versioned_types {
        return None;
    }
    let target = match &package.exports {
        Some(exports) => export_target(exports, subpath)?,
        None if subpath == "." => package_root_target(package)?,
        None => subpath.to_owned(),
    };
    if !target.starts_with("./")
        || target.contains(['\\', '\0', ':'])
        || target
            .split('/')
            .any(|part| matches!(part, ".." | "node_modules"))
    {
        return None;
    }
    if package.exports.is_some() && !valid_export_target(&target) {
        return None;
    }
    let normalized = normalize_typescript_alias_target(&package.directory, &target)?;
    (package.directory.is_empty() || normalized.starts_with(&format!("{}/", package.directory)))
        .then_some(normalized)
}

fn package_root_target(package: &WorkspacePackage) -> Option<String> {
    let entry = match &package.entry {
        PackageEntry::Unspecified => return Some("./index".to_owned()),
        PackageEntry::Target(entry) => entry,
        PackageEntry::Unresolved => return None,
    };
    Some(if entry.starts_with("./") {
        entry.to_owned()
    } else {
        format!("./{entry}")
    })
}

fn valid_export_target(target: &str) -> bool {
    let Some(tail) = target.strip_prefix("./") else {
        return false;
    };
    !tail.contains('%')
        && tail.split('/').all(|part| {
            !matches!(part, "" | "." | "..") && !part.eq_ignore_ascii_case("node_modules")
        })
}

pub(super) fn resolve<'a>(
    modules: &'a ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> ModuleResolutionAttempt<'a> {
    if !javascript_modules::module_language(request.importing_language) {
        return ModuleResolutionAttempt::NotMatched;
    }
    let Some((name, _)) = package_parts(request.specifier) else {
        return ModuleResolutionAttempt::NotMatched;
    };
    if !modules.javascript_packages.by_name.contains_key(name) {
        return ModuleResolutionAttempt::NotMatched;
    }
    let Some((package, subpath)) = package(modules, request.specifier) else {
        return ModuleResolutionAttempt::Rejected;
    };
    let Some(candidate) = package_candidate(package, &subpath) else {
        return ModuleResolutionAttempt::Rejected;
    };
    if package.exports.is_some() && super::strip_any_module_extension(&candidate).is_none() {
        return ModuleResolutionAttempt::Rejected;
    }
    resolve_normalized_module_file(modules, &candidate, request.importing_language).map_or(
        ModuleResolutionAttempt::Rejected,
        ModuleResolutionAttempt::Resolved,
    )
}

pub(super) fn is_local(modules: &ModulePathIndex, specifier: &str) -> bool {
    package_parts(specifier)
        .is_some_and(|(name, _)| modules.javascript_packages.by_name.contains_key(name))
}

pub(super) fn uses_fallback(
    modules: &ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> bool {
    package(modules, request.specifier).is_some_and(|(package, _)| {
        package.exports.is_none() && matches!(&package.entry, PackageEntry::Unspecified)
    })
}

pub(super) fn uses_cross_language_fallback(
    modules: &ModulePathIndex,
    request: ModuleResolutionRequest<'_>,
) -> bool {
    package(modules, request.specifier)
        .and_then(|(package, subpath)| package_candidate(package, &subpath))
        .is_some_and(|candidate| {
            javascript_modules::uses_cross_language_fallback(
                modules,
                &candidate,
                request.importing_language,
            )
        })
}

pub(super) struct ConfigPath {
    pub(super) path: String,
    pub(super) fallback: bool,
}

pub(super) fn config_path(modules: &ModulePathIndex, specifier: &str) -> Option<ConfigPath> {
    let (package, subpath) = package(modules, specifier)?;
    if package.exports.is_some() {
        return None;
    }
    if subpath == "." {
        if package.config_redirect || has_default_entry(modules, package) {
            return None;
        }
        return Some(ConfigPath {
            path: normalize_typescript_alias_target(&package.directory, "./tsconfig.json")?,
            fallback: true,
        });
    }
    let candidate = normalize_typescript_alias_target(&package.directory, &subpath)?;
    (package.directory.is_empty() || candidate.starts_with(&format!("{}/", package.directory)))
        .then_some(ConfigPath {
            path: candidate,
            fallback: false,
        })
}

fn has_default_entry(modules: &ModulePathIndex, package: &WorkspacePackage) -> bool {
    ["index.js", "index.json", "index.node"]
        .iter()
        .any(|entry| {
            normalize_typescript_alias_target(&package.directory, &format!("./{entry}"))
                .is_some_and(|path| modules.exact.contains_key(&path))
        })
}
