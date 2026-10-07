//! Bounded Express/React name transforms over existing native facts. Framework
//! detection uses manifest dependencies, explicit imports, or JSX/TSX source.
//! Only a unique best convention in the same package scope binds, at lowered
//! framework confidence; an existing exact-name bucket remains authoritative.

mod context_member;

use std::collections::{HashMap, HashSet};

use super::javascript_member_resolution::{charge_entry, class_member, unique_candidate};
use super::{
    FileId, MAX_SYMBOL_QUALIFIED_NAME_BYTES, ResolutionCandidate, ResolutionIndex,
    ResolutionIndexFileInput, ResolutionRequest, ResolveBudget, ResolvedTarget, SourceSpan,
    StageItemFailure, SymbolKind, Visibility, directory_has_any, framework_convention_target,
    javascript_family_name, reference_kind_candidate, try_clone_text,
};

const MAX_CASE_VARIANTS: usize = 16;

#[derive(Clone, Copy, Default)]
struct Frameworks {
    express: bool,
    react: bool,
}

impl Frameworks {
    fn record(&mut self, name: &str) {
        self.express |= matches!(name, "express" | "fastify" | "koa" | "hapi");
        self.react |= matches!(name, "react" | "next" | "react-native");
    }

    fn merge(self, other: Self) -> Self {
        Self {
            express: self.express || other.express,
            react: self.react || other.react,
        }
    }
}

#[derive(Default)]
pub(super) struct JavascriptFrameworkIndex {
    packages: HashMap<String, Frameworks>,
    files: HashMap<FileId, Frameworks>,
    middleware_names: HashMap<String, Option<Vec<String>>>,
    unbound_context_sites: HashMap<FileId, HashSet<SourceSpan>>,
}

pub(super) fn index_file<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let file = input.file;
    let path = &file.file.normalized_path;
    let manifest = path.rsplit('/').next() == Some("package.json");
    let javascript = javascript_family_name(&input.file.file.language);
    if !manifest && !javascript {
        return Ok(());
    }
    if javascript {
        context_member::index_sites(input)?;
    }
    let flags = index_detection(input)?;
    let index = &mut input.index.javascript_frameworks;
    if manifest {
        let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        charge_entry::<(String, Frameworks)>(input.budget, directory.len())?;
        index
            .packages
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        index.packages.insert(try_clone_text(directory)?, flags);
    } else if flags.express || flags.react {
        charge_entry::<(FileId, Frameworks)>(input.budget, input.file.file.file_id.as_str().len())?;
        index.files.try_reserve(1).map_err(|_| StageItemFailure)?;
        index.files.insert(input.file.file.file_id.clone(), flags);
    }
    Ok(())
}

fn index_detection<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
) -> Result<Frameworks, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut flags = Frameworks {
        react: matches!(input.file.file.language.as_str(), "jsx" | "tsx"),
        ..Frameworks::default()
    };
    for symbol in &input.file.symbols {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.qualified_manifest_dependency() {
            flags.record(
                symbol
                    .input
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or_default(),
            );
        }
        if symbol.input.qualified_name == symbol.name && middleware_name(&symbol.name) {
            insert_middleware_name(
                &mut input.index.javascript_frameworks.middleware_names,
                &symbol.name,
                input.budget,
            )?;
        }
    }
    index_imports(input, flags)
}

fn index_imports<Cancel>(
    input: &mut ResolutionIndexFileInput<'_, '_, '_, '_, Cancel>,
    mut flags: Frameworks,
) -> Result<Frameworks, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for binding in &input.file.import_bindings {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        flags.record(&binding.module_specifier);
    }
    Ok(flags)
}

impl super::NativeSymbolFacts {
    fn qualified_manifest_dependency(&self) -> bool {
        self.kind == SymbolKind::Resource
            && self
                .input
                .qualified_name
                .contains("::manifest-dependency-npm::")
    }
}

fn insert_middleware_name(
    names: &mut HashMap<String, Option<Vec<String>>>,
    name: &str,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    charge_entry::<String>(budget, name.len())?;
    let folded = name.to_ascii_lowercase();
    if !names.contains_key(&folded) {
        charge_entry::<(String, Option<Vec<String>>)>(budget, folded.len())?;
        names.try_reserve(1).map_err(|_| StageItemFailure)?;
        names.insert(folded.clone(), Some(Vec::new()));
    }
    let Some(variants) = names.get_mut(&folded).ok_or(StageItemFailure)? else {
        return Ok(());
    };
    if variants.iter().any(|variant| variant == name) {
        return Ok(());
    }
    if variants.len() == MAX_CASE_VARIANTS {
        names.insert(folded, None);
        return Ok(());
    }
    charge_entry::<String>(budget, name.len())?;
    variants.try_reserve(1).map_err(|_| StageItemFailure)?;
    variants.push(try_clone_text(name)?);
    Ok(())
}

pub(super) fn middleware_name(name: &str) -> bool {
    [
        "auth",
        "authenticate",
        "authorization",
        "cors",
        "helmet",
        "logger",
        "errorHandler",
        "notFound",
    ]
    .iter()
    .any(|exact| name.eq_ignore_ascii_case(exact))
        || ["validate", "sanitize", "rateLimit"].iter().any(|prefix| {
            name.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        })
        || strip_suffix_case(name, "Middleware").is_some()
}

fn strip_suffix_case<'name>(name: &'name str, suffix: &str) -> Option<&'name str> {
    let split = name.len().checked_sub(suffix.len())?;
    name.get(split..)?
        .eq_ignore_ascii_case(suffix)
        .then(|| name.get(..split))
        .flatten()
}

impl JavascriptFrameworkIndex {
    fn package<'index>(&'index self, path: &str) -> (&'index str, Frameworks) {
        let mut directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        loop {
            if let Some((root, flags)) = self.packages.get_key_value(directory) {
                return (root, *flags);
            }
            if directory.is_empty() {
                return ("", Frameworks::default());
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    fn case_variants(&self, name: &str) -> CaseVariants<'_> {
        let mut buffer = [0_u8; MAX_SYMBOL_QUALIFIED_NAME_BYTES];
        let Some(folded) = buffer.get_mut(..name.len()) else {
            return CaseVariants::Ambiguous;
        };
        folded.copy_from_slice(name.as_bytes());
        folded.make_ascii_lowercase();
        let Ok(folded) = std::str::from_utf8(folded) else {
            return CaseVariants::Ambiguous;
        };
        match self.middleware_names.get(folded) {
            Some(Some(names)) => CaseVariants::Names(names),
            Some(None) => CaseVariants::Ambiguous,
            None => CaseVariants::Missing,
        }
    }
}

enum CaseVariants<'name> {
    Missing,
    Ambiguous,
    Names(&'name [String]),
}

#[derive(Clone, Copy)]
enum Transform<'name> {
    Middleware(&'name str),
    Context(&'name str),
    Member {
        owner: &'name str,
        base: &'name str,
        member: &'name str,
        controller: bool,
    },
}

impl<'name> Transform<'name> {
    fn detect(name: &'name str, flags: Frameworks) -> Option<Self> {
        if flags.express && middleware_name(name) && !name.contains('.') {
            return Some(Self::Middleware(
                strip_suffix_case(name, "Middleware").unwrap_or(name),
            ));
        }
        if flags.react
            && let Some(base) = name
                .strip_suffix("Context")
                .or_else(|| name.strip_suffix("Provider"))
        {
            return (!base.is_empty()).then_some(Self::Context(base));
        }
        if !flags.express {
            return None;
        }
        let (owner, member) = name.split_once('.')?;
        if member.contains('.') || member.is_empty() {
            return None;
        }
        let (base, controller) = if let Some(base) = owner.strip_suffix("Controller") {
            (base, true)
        } else {
            (
                owner
                    .strip_suffix("Service")
                    .or_else(|| owner.strip_suffix("Helper"))
                    .or_else(|| owner.strip_suffix("Utils"))
                    .or_else(|| owner.strip_suffix("Util"))?,
                false,
            )
        };
        (!base.is_empty()).then_some(Self::Member {
            owner,
            base,
            member,
            controller,
        })
    }

    fn name(self) -> &'name str {
        match self {
            Self::Middleware(name) | Self::Context(name) => name,
            Self::Member { member, .. } => member,
        }
    }
}

struct TransformQuery<'index, 'request> {
    index: &'index ResolutionIndex,
    request: &'index ResolutionRequest<'request>,
    transform: Transform<'request>,
    scope: &'index str,
    controller_file: Option<&'index FileId>,
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !javascript_family_name(request.language) {
        return Ok(None);
    }
    let frameworks = &index.javascript_frameworks;
    let (scope, flags) = frameworks.package(request.file_path);
    let flags = flags.merge(
        frameworks
            .files
            .get(request.file_id)
            .copied()
            .unwrap_or_default(),
    );
    if flags.react
        && let Some(target) = context_member::resolve(index, request, cancelled)?
    {
        return Ok(Some(target));
    }
    let Some(transform) = Transform::detect(request.name, flags) else {
        return Ok(None);
    };
    if matches!(transform, Transform::Member { .. })
        && super::javascript_member_resolution::shadowed_receiver(index, request)
    {
        return Ok(None);
    }
    let mut query = TransformQuery {
        index,
        request,
        transform,
        scope,
        controller_file: None,
    };
    query.controller_file = controller_file(&query, cancelled)?;
    select_transformed_candidate(&query, cancelled)
        .map(|candidate| candidate.map(framework_convention_target))
}

fn controller_file<'index, Cancel>(
    query: &TransformQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index FileId>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Transform::Member {
        owner,
        controller: true,
        ..
    } = query.transform
    else {
        return Ok(None);
    };
    let candidates = query
        .index
        .candidates
        .get(owner)
        .map_or(&[][..], |bucket| bucket.as_slice());
    unique_candidate(
        candidates,
        |candidate| {
            candidate.kind == SymbolKind::Class
                && candidate.top_level
                && candidate_in_scope(query, candidate)
        },
        cancelled,
    )
    .map(|candidate| candidate.map(|candidate| &candidate.file_id))
}

fn candidate_in_scope(query: &TransformQuery<'_, '_>, candidate: &ResolutionCandidate) -> bool {
    query
        .index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|file| {
            javascript_family_name(&file.language)
                && query.index.javascript_frameworks.package(&file.path).0 == query.scope
        })
}

fn candidate_score(query: &TransformQuery<'_, '_>, candidate: &ResolutionCandidate) -> u8 {
    if candidate.augmentation
        || matches!(
            candidate.visibility,
            Some(Visibility::Private | Visibility::Protected)
        )
        || !candidate_in_scope(query, candidate)
        || !reference_kind_candidate(query.request.kind, candidate)
    {
        return 0;
    }
    let Some(file) = query.index.modules.files.get(&candidate.file_id) else {
        return 0;
    };
    let path = &file.path;
    match query.transform {
        Transform::Middleware(_) if candidate.top_level => {
            1 + u8::from(directory_has_any(path, &["middleware", "middlewares"]))
        }
        Transform::Context(_) if candidate.top_level => 1,
        Transform::Member { base, .. } if member_target(query.index, candidate) => {
            if contains_case(path, base) {
                2
            } else {
                u8::from(query.controller_file == Some(&candidate.file_id))
            }
        }
        _ => 0,
    }
}

fn member_target(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    (candidate.kind == SymbolKind::Function && candidate.top_level)
        || class_member(index, candidate)
}

fn contains_case(text: &str, needle: &str) -> bool {
    !needle.is_empty()
        && text
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[derive(Default)]
struct PreferredCandidate<'candidate> {
    selected: Option<&'candidate ResolutionCandidate>,
    score: u8,
    ambiguous: bool,
}

impl<'candidate> PreferredCandidate<'candidate> {
    fn offer(&mut self, candidate: &'candidate ResolutionCandidate, score: u8) {
        if score == 0 {
            return;
        }
        if score > self.score {
            self.selected = Some(candidate);
            self.score = score;
            self.ambiguous = false;
        } else if score == self.score
            && self
                .selected
                .is_some_and(|selected| selected.symbol_id != candidate.symbol_id)
        {
            self.ambiguous = true;
        }
    }
}

fn select_transformed_candidate<'index, Cancel>(
    query: &TransformQuery<'index, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index ResolutionCandidate>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let name = query.transform.name();
    let variants = match query.transform {
        Transform::Middleware(_) => query.index.javascript_frameworks.case_variants(name),
        Transform::Context(_) | Transform::Member { .. } => CaseVariants::Missing,
    };
    let variants = match variants {
        CaseVariants::Ambiguous => return Ok(None),
        CaseVariants::Missing => &[][..],
        CaseVariants::Names(names) => names,
    };
    let names = std::iter::once(name).chain(
        variants
            .iter()
            .map(String::as_str)
            .filter(|variant| *variant != name),
    );
    let mut best = PreferredCandidate::default();
    for name in names {
        let candidates = query
            .index
            .candidates
            .get(name)
            .map_or(&[][..], |bucket| bucket.as_slice());
        for candidate in candidates {
            if cancelled() {
                return Err(StageItemFailure);
            }
            best.offer(candidate, candidate_score(query, candidate));
        }
    }
    Ok(best.selected.filter(|_| !best.ambiguous))
}
