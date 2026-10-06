use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};

use super::{
    BTreeMap, BTreeSet, ContentDigest, MAXIMUM_TYPESCRIPT_ALIAS_CONFIGS,
    MAXIMUM_TYPESCRIPT_PATH_MAPPINGS, MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES, ModulePathIndex,
    NativeFileFacts, NormalizedPath, RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndexContext,
    SourceLanguage, SourceReadOptions, SourceSnapshot, StageItemFailure, TypeScriptAliasConfig,
    Value, copy_typescript_config_quoted_byte, exact_limit_ceiling, fmt, javascript_packages,
    normalize_joined_project_path, normalize_typescript_alias_base, parse_typescript_path_mapping,
    size_of, skip_typescript_block_comment, skip_typescript_line_comment, try_clone_text,
    typescript_alias_config_bytes, usize_to_u64,
};

const MAX_EXTENDS_DEPTH: usize = 8;

#[derive(Default)]
pub(super) struct ConfigIndex {
    files: BTreeMap<String, ConfigFile>,
}

struct ConfigFile {
    hash: ContentDigest,
    bytes: u64,
}

#[derive(Default, Deserialize)]
struct RawConfig {
    extends: Option<String>,
    #[serde(rename = "compilerOptions", default)]
    compiler: CompilerOptions,
}

#[derive(Default, Deserialize)]
struct CompilerOptions {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    paths: Option<OrderedPaths>,
}

struct OrderedPaths(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for OrderedPaths {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(PathsVisitor)
    }
}

struct PathsVisitor;

impl<'de> Visitor<'de> for PathsVisitor {
    type Value = OrderedPaths;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded paths object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut paths = Vec::new();
        let mut patterns = BTreeSet::new();
        while let Some(entry) = access.next_entry::<String, Value>()? {
            if !patterns.insert(entry.0.clone()) {
                return Err(serde::de::Error::custom("duplicate paths pattern"));
            }
            if paths.len() >= MAXIMUM_TYPESCRIPT_PATH_MAPPINGS {
                return Err(serde::de::Error::custom("too many paths"));
            }
            paths.push(entry);
        }
        Ok(OrderedPaths(paths))
    }
}

pub(super) fn index_file<Cancel>(
    modules: &mut ModulePathIndex,
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != SourceLanguage::Json.as_str() {
        return Ok(());
    }
    if (context.cancelled)() {
        return Err(StageItemFailure);
    }
    let path = file.file.normalized_path.as_str();
    context.budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            + usize_to_u64(path.len())
            + usize_to_u64(size_of::<ConfigFile>()),
    )?;
    modules.javascript_configs.files.insert(
        try_clone_text(path)?,
        ConfigFile {
            hash: file.file.content_hash.clone(),
            bytes: file.file.byte_size,
        },
    );
    if path.rsplit('/').next() == Some("package.json")
        && !path.split('/').any(|part| part == "node_modules")
    {
        let snapshot = read_config(modules, path, context)?;
        javascript_packages::index_package(modules, &snapshot, context)?;
    }
    Ok(())
}

pub(super) fn finish<Cancel>(
    modules: &mut ModulePathIndex,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut configs = Vec::new();
    for path in modules.javascript_configs.files.keys() {
        if (context.cancelled)() {
            return Err(StageItemFailure);
        }
        if matches!(
            path.rsplit('/').next(),
            Some("tsconfig.json" | "jsconfig.json")
        ) {
            context
                .budget
                .charge(usize_to_u64(path.len()) + usize_to_u64(size_of::<String>()))?;
            configs.push(try_clone_text(path)?);
        }
    }
    for path in configs {
        let tsconfig = path.ends_with("tsconfig.json");
        let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        if modules
            .typescript_aliases
            .by_directory
            .get(directory)
            .is_some_and(|existing| existing.tsconfig || !tsconfig)
        {
            continue;
        }
        let config = load_chain(
            ConfigChain {
                modules,
                path: &path,
                depth: 0,
                seen: &mut BTreeSet::new(),
            },
            context,
        )?;
        // A rejected nearest config blocks ancestor aliases. Its missing base
        // must never be replaced by a parent directory's unrelated config.
        let mut config = config.unwrap_or_else(|| empty_config(&path));
        config.tsconfig = tsconfig;
        if !modules
            .typescript_aliases
            .by_directory
            .contains_key(directory)
            && modules.typescript_aliases.by_directory.len() >= MAXIMUM_TYPESCRIPT_ALIAS_CONFIGS
        {
            return Err(StageItemFailure);
        }
        context
            .budget
            .charge(typescript_alias_config_bytes(directory, &config))?;
        modules
            .typescript_aliases
            .by_directory
            .insert(try_clone_text(directory)?, config);
    }
    Ok(())
}

struct ConfigChain<'a> {
    modules: &'a ModulePathIndex,
    path: &'a str,
    depth: usize,
    seen: &'a mut BTreeSet<String>,
}

fn load_chain<Cancel>(
    input: ConfigChain<'_>,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<Option<TypeScriptAliasConfig>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ConfigChain {
        modules,
        path,
        depth,
        seen,
    } = input;
    if depth > MAX_EXTENDS_DEPTH || !seen.insert(try_clone_text(path)?) {
        return Ok(None);
    }
    let snapshot = read_config(modules, path, context)?;
    let stripped = strip_typescript_config_comments(snapshot.source());
    let Ok(raw) = serde_json::from_str::<RawConfig>(&stripped) else {
        return Ok(None);
    };
    let inherited = if let Some(parent) = raw.extends.as_deref() {
        let Some(parent) = extended_path(modules, path, parent) else {
            return Ok(None);
        };
        let Some(mut inherited) = load_chain(
            ConfigChain {
                modules,
                path: &parent.path,
                depth: depth + 1,
                seen,
            },
            context,
        )?
        else {
            return Ok(None);
        };
        inherited.fallback |= parent.fallback;
        Some(inherited)
    } else {
        None
    };
    merge_config(raw.compiler, (path, inherited), context)
}

pub(super) fn normalize_base(directory: &str, base: &str) -> Option<String> {
    if base.len() > MAXIMUM_TYPESCRIPT_PATH_TEXT_BYTES
        || base.contains(['\\', '\0'])
        || base.starts_with('/')
    {
        return None;
    }
    let anchor = format!("{directory}/__cartograph_config__.json");
    let target = format!("{base}/__cartograph_base__.json");
    let normalized = normalize_joined_project_path(&anchor, &target)?;
    Some(
        normalized
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .to_owned(),
    )
}

fn read_config<Cancel>(
    modules: &ModulePathIndex,
    path: &str,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<SourceSnapshot, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let file = modules
        .javascript_configs
        .files
        .get(path)
        .ok_or(StageItemFailure)?;
    // The snapshot and JSON parser both retain bounded source-sized working data.
    context.budget.charge(file.bytes.saturating_mul(8))?;
    let normalized = NormalizedPath::parse(path).map_err(|_| StageItemFailure)?;
    let snapshot = context
        .source_root
        .read_with_cancellation(
            &normalized,
            SourceReadOptions::new(exact_limit_ceiling(file.bytes)?, &mut *context.cancelled),
        )
        .map_err(|_| StageItemFailure)?;
    if snapshot.content_hash() != &file.hash || snapshot.byte_size() != file.bytes {
        return Err(StageItemFailure);
    }
    Ok(snapshot)
}

fn extended_path(
    modules: &ModulePathIndex,
    from: &str,
    parent: &str,
) -> Option<javascript_packages::ConfigPath> {
    let parent = parent.trim();
    if !super::valid_typescript_alias_text(parent) || parent.contains('*') {
        return None;
    }
    let candidate = if parent.starts_with('.') {
        javascript_packages::ConfigPath {
            path: normalize_joined_project_path(from, parent)?,
            fallback: false,
        }
    } else {
        javascript_packages::config_path(modules, parent)?
    };
    [
        (candidate.path.clone(), false),
        (format!("{}.json", candidate.path), false),
        (format!("{}/tsconfig.json", candidate.path), true),
    ]
    .into_iter()
    .find(|(path, _)| modules.javascript_configs.files.contains_key(path))
    .map(|(path, fallback)| javascript_packages::ConfigPath {
        path,
        fallback: candidate.fallback || fallback,
    })
}

fn empty_config(path: &str) -> TypeScriptAliasConfig {
    let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
    TypeScriptAliasConfig {
        base_path: directory.to_owned(),
        explicit_base_url: None,
        mappings: Vec::new(),
        tsconfig: false,
        fallback: false,
    }
}

fn merge_config<Cancel>(
    compiler: CompilerOptions,
    (path, inherited): (&str, Option<TypeScriptAliasConfig>),
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<Option<TypeScriptAliasConfig>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
    let mut config = inherited.unwrap_or_else(|| empty_config(path));
    if let Some(base) = compiler.base_url {
        let Some(base) = normalize_typescript_alias_base(directory, &base) else {
            return Ok(None);
        };
        config.base_path = try_clone_text(&base)?;
        config.explicit_base_url = Some(base);
    }
    if let Some(paths) = compiler.paths {
        config.base_path =
            try_clone_text(config.explicit_base_url.as_deref().unwrap_or(directory))?;
        config.mappings.clear();
        for (pattern, substitutions) in paths.0 {
            if (context.cancelled)() {
                return Err(StageItemFailure);
            }
            if let Some(mapping) = parse_typescript_path_mapping(&pattern, &substitutions)? {
                config.mappings.push(mapping);
            }
        }
        // Stable sorting retains the declaration order for equal specificity.
        config.mappings.sort_by_key(|mapping| {
            (
                std::cmp::Reverse(mapping.pattern.find('*').unwrap_or(mapping.pattern.len())),
                mapping.pattern.contains('*'),
            )
        });
    }
    Ok(Some(config))
}

pub(super) fn strip_typescript_config_comments(source: &str) -> String {
    let mut output = Vec::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut quoted = false;
    let mut significant = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            (index, quoted) = copy_typescript_config_quoted_byte(bytes, index, &mut output);
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            index = skip_typescript_line_comment(bytes, index);
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index = skip_typescript_block_comment(bytes, index);
            continue;
        }
        if matches!(byte, b'}' | b']')
            && let Some((position, b',')) = significant
        {
            output[position] = b' ';
        }
        if !byte.is_ascii_whitespace() {
            significant = Some((output.len(), byte));
        }
        quoted = byte == b'"';
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| source.to_owned())
}
