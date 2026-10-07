//! Exact PHP name resolution.
//!
//! The PHP extractor resolves statically named references at compile time,
//! exactly as PHP does from the namespace, the `use` aliases of the enclosing
//! block, and the class context, and records an exact lookup: a PHP symbol
//! space and the candidate qualified name. Such a lookup binds only to a PHP
//! declaration of that symbol space whose qualified name is the key; otherwise
//! it stays unresolved. It never falls back to short-name matching, so a vendor
//! import or a missing class cannot bind to a same-named project declaration in
//! another namespace.
//!
//! Names compare as PHP compares them: namespace, class-like, function, and
//! method names ignore ASCII case, while the name of a constant is
//! case-sensitive. Two declarations that differ only in case are ambiguous.
//!
//! `static::` and `$this->` lookups name the declaration in the statically
//! known class, which late static binding or an override can replace at
//! runtime, so they resolve with dynamic-dispatch confidence and provenance.
//! So does a member called on the object a factory method returns, which is
//! followed only through the factory's declared return type: `self`/`static`
//! (the factory's own class) or exactly one class the declaring file named. A
//! static name whose target the extractor could not determine exactly is
//! marked to abstain and is never searched for by short name.
//!
//! A static call to a member that a project class does not declare binds to
//! the class itself only when the class is a Laravel Eloquent model by exact
//! `extends` ancestry, because Eloquent forwards such calls to a query builder
//! (`User::where`). That edge carries framework provenance and confidence.
//!
//! Framework references (Laravel route targets) carry short class names
//! rather than exact lookups. When such a name matches a PHP `use` binding,
//! the binding's fully qualified target is resolved the same exact way.

use std::collections::{HashMap, HashSet};

use cartograph_domain::{FileId, ReferenceKind, SourceLanguage, SymbolId, SymbolKind, Visibility};
use cartograph_extract::{ExtractedImportBinding, ExtractedReference, PHP_EXACT_RESOLUTION_PREFIX};

use super::{
    DYNAMIC_DISPATCH_CONFIDENCE, DYNAMIC_DISPATCH_PROVENANCE,
    DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE, EXACT_SAME_FILE_CONFIDENCE, EXACT_SAME_FILE_PROVENANCE,
    EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE, FRAMEWORK_CONVENTION_CONFIDENCE, ImportReferenceSite,
    ImportResolution, NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceResolution,
    ResolutionCandidate, ResolutionCandidateBucket, ResolutionIndex, ResolutionRequest,
    ResolveBudget, ResolvedTarget, StageItemFailure, UNRESOLVED_PROVENANCE, import_binding_target,
    project_resolved_target, select_candidate, try_clone_text, usize_to_u64,
};

mod members;
mod route_aliases;
mod routes;

pub(super) use route_aliases::implicit_binding as implicit_namespace_binding;
pub(super) use routes::resolve as resolve_route;

/// Separator between candidate-key segments.
const KEY_SEPARATOR: &str = "::";
/// Module specifier of a `use` clause that imports from the global namespace.
const ROOT_NAMESPACE: &str = "\\";
/// Longest synthesized binding or member key; longer keys cannot name a candidate.
const MAX_SYNTHESIZED_KEY_BYTES: usize = 1_536;
/// Most distinct spellings retained for one case-folded qualified name; past
/// it every lookup of that name abstains.
const MAX_CASE_VARIANTS: usize = 16;
/// Most `extends` hops followed from a class towards an Eloquent model base.
const MAX_ANCESTRY_HOPS: usize = 16;
/// Most interfaces and traits recorded for one class, trait, or enum; past
/// it the composition counts as unknown.
const MAX_COMPOSED_TYPES: usize = 64;
/// Most classes and traits whose composed traits are inspected for one
/// member; past it the member counts as possibly trait-supplied.
const MAX_TRAIT_VISITS: usize = 32;
/// Provenance of an undeclared static member bound to its Eloquent model class.
const ELOQUENT_MODEL_PROVENANCE: &str = "framework-laravel-eloquent-model";
/// Candidate keys of the Laravel classes whose subclasses are Eloquent models.
const ELOQUENT_MODEL_BASES: [&str; 4] = [
    "Illuminate\\Database\\Eloquent::Model",
    "Illuminate\\Foundation\\Auth::User",
    "Illuminate\\Database\\Eloquent\\Relations::Pivot",
    "Illuminate\\Database\\Eloquent\\Relations::MorphPivot",
];

/// The PHP symbol space an exact lookup may bind to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Intent {
    Class,
    AdaptedClass,
    DispatchClass,
    Function,
    FunctionFallback,
    Member,
    DispatchMember,
    ReturnedMember,
    OwnReturnedMember,
    Constant,
    Abstain,
}

impl Intent {
    fn parse(marker: &str) -> Option<Self> {
        match marker {
            "class" => Some(Self::Class),
            "adapted-class" => Some(Self::AdaptedClass),
            "dispatch-class" => Some(Self::DispatchClass),
            "function" => Some(Self::Function),
            "function-fallback" => Some(Self::FunctionFallback),
            "member" => Some(Self::Member),
            "dispatch-member" => Some(Self::DispatchMember),
            "returned-member" => Some(Self::ReturnedMember),
            "own-returned-member" => Some(Self::OwnReturnedMember),
            "constant" => Some(Self::Constant),
            "abstain" => Some(Self::Abstain),
            _ => None,
        }
    }

    const fn admits(self, kind: SymbolKind) -> bool {
        match self {
            Self::Class | Self::AdaptedClass | Self::DispatchClass => class_like(kind),
            Self::Function | Self::FunctionFallback => matches!(kind, SymbolKind::Function),
            Self::Member
            | Self::DispatchMember
            | Self::ReturnedMember
            | Self::OwnReturnedMember => matches!(kind, SymbolKind::Method),
            Self::Constant => matches!(kind, SymbolKind::Constant),
            Self::Abstain => false,
        }
    }

    const fn dispatch(self) -> bool {
        matches!(
            self,
            Self::DispatchClass
                | Self::DispatchMember
                | Self::ReturnedMember
                | Self::OwnReturnedMember
        )
    }
}

/// Whether a symbol kind is a PHP class, interface, trait, or enum.
const fn class_like(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait | SymbolKind::Enum
    )
}

/// Whether PHP declarations of this kind can be the target of an exact lookup.
const fn exactly_resolvable(kind: SymbolKind) -> bool {
    class_like(kind)
        || matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Constant
        )
}

/// PHP declaration facts the exact resolver needs beyond the shared
/// candidate map.
#[derive(Default)]
pub(super) struct PhpResolutionIndex {
    /// ASCII-lowercased qualified name → the declared spellings that fold to it.
    folded: HashMap<String, CaseVariants>,
    /// Class-like identities that establish caller scope.
    classes: HashSet<SymbolId>,
    /// Class → candidate key of the class its `extends` clause names; `None`
    /// when the target is unknown or not unique.
    parents: HashMap<SymbolId, Option<String>>,
    /// Callable → candidate key of the one class its return type names;
    /// `None` when the type names no single exactly resolved class.
    returns: HashMap<SymbolId, Option<String>>,
    /// Class, trait, or enum → candidate keys of the interfaces and traits
    /// it implements or uses; `None` when one of them is unknown or is a
    /// trait used with an adaptation block, which can rename its methods.
    composed: HashMap<SymbolId, Option<Vec<String>>>,
    route_aliases: route_aliases::RouteAliasIndex,
}

/// The distinct declared spellings of one case-folded qualified name.
#[derive(Default)]
struct CaseVariants {
    spellings: Vec<String>,
    overflowed: bool,
}

/// One extracted file to add to the PHP resolution index.
pub(super) struct PhpFileIndexInput<'index, 'file, 'budget> {
    pub(super) index: &'index mut PhpResolutionIndex,
    pub(super) file: &'file NativeFileFacts,
    pub(super) budget: &'budget mut ResolveBudget,
}

/// Record the declarations, `extends` targets, return types, and composed
/// interfaces and traits of a PHP file; other languages are ignored.
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
    if file.file.language != SourceLanguage::Php.as_str() {
        return Ok(());
    }
    route_aliases::index_file(
        PhpFileIndexInput {
            index,
            file,
            budget,
        },
        cancelled,
    )?;
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if exactly_resolvable(symbol.kind) {
            index.fold(&symbol.input.qualified_name, budget)?;
        }
        if class_like(symbol.kind) {
            record_class_scope(&mut index.classes, &symbol.input.symbol_id, budget)?;
        }
    }
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        match reference.kind {
            ReferenceKind::Extends => record_class_fact(&mut index.parents, reference, budget)?,
            ReferenceKind::Returns => record_class_fact(&mut index.returns, reference, budget)?,
            ReferenceKind::Implements => {
                record_composed_type(&mut index.composed, reference, budget)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn record_class_scope(
    classes: &mut HashSet<SymbolId>,
    id: &SymbolId,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<SymbolId>()))
            .saturating_add(usize_to_u64(id.as_str().len())),
    )?;
    classes.insert(id.clone());
    Ok(())
}

/// PHP methods and member initializers have their class as the direct parent.
/// Named nested functions establish a separate scope and must not inherit it.
pub(super) fn caller_class<'index>(
    index: &'index ResolutionIndex,
    owner: Option<&SymbolId>,
) -> Option<&'index SymbolId> {
    let owner = owner?;
    index.languages.php.classes.get(owner).or_else(|| {
        index
            .parents
            .get(owner)
            .filter(|parent| index.languages.php.classes.contains(*parent))
    })
}

impl PhpResolutionIndex {
    /// Record one declared qualified name under its case-folded form.
    fn fold(
        &mut self,
        qualified_name: &str,
        budget: &mut ResolveBudget,
    ) -> Result<(), StageItemFailure> {
        let mut folded = try_clone_text(qualified_name)?;
        folded.make_ascii_lowercase();
        let spelling_bytes =
            usize_to_u64(size_of::<String>()).saturating_add(usize_to_u64(qualified_name.len()));
        let Some(variants) = self.folded.get_mut(&folded) else {
            budget.charge(
                RESOLUTION_MAP_NODE_ALLOWANCE
                    .saturating_add(usize_to_u64(size_of::<(String, CaseVariants)>()))
                    .saturating_add(usize_to_u64(folded.len()))
                    .saturating_add(spelling_bytes),
            )?;
            let mut spellings = Vec::new();
            spellings
                .try_reserve_exact(1)
                .map_err(|_| StageItemFailure)?;
            spellings.push(try_clone_text(qualified_name)?);
            self.folded.insert(
                folded,
                CaseVariants {
                    spellings,
                    overflowed: false,
                },
            );
            return Ok(());
        };
        if variants.overflowed
            || variants
                .spellings
                .iter()
                .any(|spelling| spelling == qualified_name)
        {
            return Ok(());
        }
        if variants.spellings.len() >= MAX_CASE_VARIANTS {
            variants.overflowed = true;
            return Ok(());
        }
        budget.charge(spelling_bytes)?;
        variants
            .spellings
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure)?;
        variants.spellings.push(try_clone_text(qualified_name)?);
        Ok(())
    }

    /// The declared spellings `key` may match, or `None` when too many
    /// spellings exist to decide.
    fn spellings(&self, key: &str) -> Option<&[String]> {
        match self.folded.get(&key.to_ascii_lowercase()) {
            None => Some(&[]),
            Some(variants) if variants.overflowed => None,
            Some(variants) => Some(&variants.spellings),
        }
    }
}

/// The candidate key a heritage or type reference names, when the extractor
/// resolved it exactly to a class-like name.
fn exact_class_key(reference: &ExtractedReference) -> Option<&str> {
    reference
        .resolution_name
        .as_deref()
        .and_then(|name| name.strip_prefix(PHP_EXACT_RESOLUTION_PREFIX))
        .and_then(PhpExactLookup::parse)
        .filter(|lookup| lookup.intent == Intent::Class)
        .map(PhpExactLookup::key)
}

/// Record one interface or trait a class-like declaration composes. An
/// inexact name, or more types than the bound, makes the composition unknown.
fn record_composed_type(
    facts: &mut HashMap<SymbolId, Option<Vec<String>>>,
    reference: &ExtractedReference,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let Some(owner) = reference.owner.as_ref() else {
        return Ok(());
    };
    let key = exact_class_key(reference);
    let key_bytes =
        usize_to_u64(size_of::<String>()).saturating_add(usize_to_u64(key.map_or(0, str::len)));
    if !facts.contains_key(owner) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(SymbolId, Option<Vec<String>>)>()))
                .saturating_add(usize_to_u64(owner.as_str().len())),
        )?;
        facts.insert(owner.clone(), Some(Vec::new()));
    }
    let Some(entry) = facts.get_mut(owner) else {
        return Err(StageItemFailure);
    };
    let Some(keys) = entry else {
        return Ok(());
    };
    let Some(key) = key else {
        *entry = None;
        return Ok(());
    };
    if keys.iter().any(|known| known == key) {
        return Ok(());
    }
    if keys.len() >= MAX_COMPOSED_TYPES {
        *entry = None;
        return Ok(());
    }
    budget.charge(key_bytes)?;
    keys.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
    keys.push(try_clone_text(key)?);
    Ok(())
}

/// Record the class an `extends` or return-type reference names for its
/// owner. A reference whose target the extractor could not resolve exactly,
/// or a second distinct class, leaves the owner without a known class.
fn record_class_fact(
    facts: &mut HashMap<SymbolId, Option<String>>,
    reference: &ExtractedReference,
    budget: &mut ResolveBudget,
) -> Result<(), StageItemFailure> {
    let Some(owner) = reference.owner.as_ref() else {
        return Ok(());
    };
    let class = exact_class_key(reference);
    if let Some(known) = facts.get_mut(owner) {
        if known.as_deref() != class {
            *known = None;
        }
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<(SymbolId, Option<String>)>()))
            .saturating_add(usize_to_u64(owner.as_str().len()))
            .saturating_add(usize_to_u64(class.map_or(0, str::len))),
    )?;
    facts.insert(owner.clone(), class.map(try_clone_text).transpose()?);
    Ok(())
}

/// One extractor-resolved PHP lookup: `<intent>::<qualified key>`.
#[derive(Clone, Copy)]
pub(super) struct PhpExactLookup<'lookup> {
    intent: Intent,
    key: &'lookup str,
}

impl<'lookup> PhpExactLookup<'lookup> {
    /// Parse the suffix that follows the PHP exact-resolution marker.
    pub(super) fn parse(suffix: &'lookup str) -> Option<Self> {
        let (intent, key) = suffix.split_once(KEY_SEPARATOR)?;
        let intent = Intent::parse(intent)?;
        (!key.is_empty()).then_some(Self { intent, key })
    }

    pub(super) const fn key(self) -> &'lookup str {
        self.key
    }
}

/// An exact lookup together with its file and proven caller class scope.
#[derive(Clone, Copy)]
pub(super) struct PhpExactRequest<'request> {
    pub(super) file_id: &'request FileId,
    pub(super) caller_class: Option<&'request SymbolId>,
    pub(super) lookup: PhpExactLookup<'request>,
}

/// Resolve one exact PHP lookup, or leave it unresolved with provenance that
/// says whether the named declaration exists in the project at all.
pub(super) fn resolve_exact<Cancel>(
    index: &ResolutionIndex,
    request: PhpExactRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let PhpExactRequest {
        file_id,
        caller_class,
        lookup,
    } = request;
    let exact = ExactQuery {
        index,
        file_id,
        caller_class,
        intent: lookup.intent,
        key: lookup.key,
    };
    match lookup.intent {
        Intent::Abstain => return Ok(ReferenceResolution::unresolved(UNRESOLVED_PROVENANCE)),
        Intent::ReturnedMember | Intent::OwnReturnedMember => {
            return resolve_returned_member(exact, cancelled);
        }
        _ => {}
    }
    if let Some(target) = exact.target(cancelled)? {
        return Ok(ReferenceResolution::resolved(if lookup.intent.dispatch() {
            dispatched(target)
        } else {
            target
        }));
    }
    if let Some(candidate) = members::resolve(exact, cancelled)? {
        let target = exact_target(candidate, exact.file_id);
        return Ok(ReferenceResolution::resolved(if lookup.intent.dispatch() {
            dispatched(target)
        } else {
            target
        }));
    }
    if let Some(target) = undeclared_target(exact, cancelled)? {
        return Ok(ReferenceResolution::resolved(target));
    }
    Ok(ReferenceResolution::unresolved(unresolved_provenance(
        exact, cancelled,
    )?))
}

/// The target PHP itself reaches when the named declaration does not exist:
/// the global function for an unqualified call in a namespace, and the model
/// class for a static call on a Laravel Eloquent model.
fn undeclared_target<Cancel>(
    exact: ExactQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(exact.intent, Intent::FunctionFallback | Intent::Member)
        || exact.declared(cancelled)?
    {
        return Ok(None);
    }
    if exact.intent == Intent::FunctionFallback {
        exact.global_function().target(cancelled)
    } else {
        eloquent_model_target(exact, cancelled)
    }
}

/// A target reached through runtime dispatch on a statically known class.
fn dispatched(target: ResolvedTarget) -> ResolvedTarget {
    ResolvedTarget {
        confidence: DYNAMIC_DISPATCH_CONFIDENCE,
        provenance: DYNAMIC_DISPATCH_PROVENANCE,
        ..target
    }
}

/// A missing member of a declared class is an unresolved project reference;
/// a name with no PHP declaration anywhere in the project is external.
fn unresolved_provenance<Cancel>(
    query: ExactQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<&'static str, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if query.intent.dispatch() {
        return Ok(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE);
    }
    let owner_declared = match query.intent {
        Intent::Member => match query.key.rsplit_once(KEY_SEPARATOR) {
            Some((owner, _)) => ExactQuery {
                intent: Intent::Class,
                key: owner,
                ..query
            }
            .declared(cancelled)?,
            None => false,
        },
        Intent::FunctionFallback => {
            query.declared(cancelled)? || query.global_function().declared(cancelled)?
        }
        Intent::Class
        | Intent::AdaptedClass
        | Intent::DispatchClass
        | Intent::Function
        | Intent::DispatchMember
        | Intent::ReturnedMember
        | Intent::OwnReturnedMember
        | Intent::Constant
        | Intent::Abstain => query.declared(cancelled)?,
    };
    Ok(if owner_declared {
        UNRESOLVED_PROVENANCE
    } else {
        EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
    })
}

/// Whether a declared qualified name is the name a lookup spells, as PHP
/// compares them: ignoring ASCII case, except for a constant's own name.
fn names_match(intent: Intent, declared: &str, key: &str) -> bool {
    if intent != Intent::Constant {
        return declared.eq_ignore_ascii_case(key);
    }
    let (declared_scope, declared_name) = split_last_segment(declared);
    let (key_scope, key_name) = split_last_segment(key);
    declared_name == key_name && declared_scope.eq_ignore_ascii_case(key_scope)
}

/// `(scope, name)` of a candidate key; global names have an empty scope.
fn split_last_segment(key: &str) -> (&str, &str) {
    key.rsplit_once(KEY_SEPARATOR).unwrap_or(("", key))
}

/// One exact candidate query.
#[derive(Clone, Copy)]
struct ExactQuery<'query, 'lookup> {
    index: &'query ResolutionIndex,
    file_id: &'lookup FileId,
    caller_class: Option<&'lookup SymbolId>,
    intent: Intent,
    key: &'lookup str,
}

impl<'query> ExactQuery<'query, '_> {
    /// The global function PHP tries when no namespaced function of an
    /// unqualified call's name exists.
    fn global_function(self) -> Self {
        Self {
            intent: Intent::Function,
            key: split_last_segment(self.key).1,
            ..self
        }
    }

    /// Every candidate filed under one of the declared spellings of the key;
    /// `admits` decides which of them this query names.
    fn filed(
        self,
        spellings: &'query [String],
    ) -> impl Iterator<Item = &'query ResolutionCandidate> + 'query {
        let index = self.index;
        spellings.iter().flat_map(move |spelling| {
            index.candidates.get(spelling).map_or(
                &[] as &[ResolutionCandidate],
                ResolutionCandidateBucket::as_slice,
            )
        })
    }

    /// Whether a candidate is a PHP declaration of this symbol space whose
    /// qualified name is the key under PHP's comparison, visible or not.
    fn admits(self, candidate: &ResolutionCandidate) -> bool {
        names_match(self.intent, &candidate.qualified_name, self.key)
            && self.intent.admits(candidate.kind)
            && php_candidate(self.index, candidate)
    }

    /// Whether any PHP declaration of this symbol space has this qualified
    /// name, visible or not; a name with too many spellings to decide counts
    /// as declared so no fallback can bypass it.
    fn declared<Cancel>(self, cancelled: &mut Cancel) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let Some(spellings) = self.index.languages.php.spellings(self.key) else {
            return Ok(true);
        };
        for candidate in self.filed(spellings) {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if self.admits(candidate) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The single visible declaration this query names.
    fn candidate<Cancel>(
        self,
        cancelled: &mut Cancel,
    ) -> Result<Option<&'query ResolutionCandidate>, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let candidate = self.declaration(cancelled)?;
        match candidate {
            Some(candidate) if self.visible_from(candidate, cancelled)? => Ok(Some(candidate)),
            _ => Ok(None),
        }
    }

    /// The single declaration of this name, before caller access is checked.
    fn declaration<Cancel>(
        self,
        cancelled: &mut Cancel,
    ) -> Result<Option<&'query ResolutionCandidate>, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let Some(spellings) = self.index.languages.php.spellings(self.key) else {
            return Ok(None);
        };
        select_candidate(
            self.filed(spellings),
            |candidate| self.admits(candidate) && !candidate.augmentation,
            cancelled,
        )
    }

    fn target<Cancel>(
        self,
        cancelled: &mut Cancel,
    ) -> Result<Option<ResolvedTarget>, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        Ok(self
            .candidate(cancelled)?
            .map(|candidate| exact_target(candidate, self.file_id)))
    }

    /// The single class-like declaration named `key`.
    fn class_like<Cancel>(
        self,
        key: &str,
        cancelled: &mut Cancel,
    ) -> Result<Option<&'query ResolutionCandidate>, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        ExactQuery {
            intent: Intent::Class,
            key,
            ..self
        }
        .candidate(cancelled)
    }

    /// Private access requires the declaring class; protected access requires
    /// that class or a uniquely resolved chain relating it to the caller.
    fn visible_from<Cancel>(
        self,
        candidate: &ResolutionCandidate,
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        match candidate.visibility {
            None | Some(Visibility::Public) => return Ok(true),
            Some(Visibility::Internal) => return Ok(false),
            Some(Visibility::Private | Visibility::Protected) => {}
        }
        let (Some(caller), Some(declaring)) =
            (self.caller_class, candidate.parent_symbol_id.as_ref())
        else {
            return Ok(false);
        };
        if caller == declaring {
            return Ok(true);
        }
        if candidate.visibility == Some(Visibility::Private) {
            return Ok(false);
        }
        Ok(self.descends_from((caller, declaring), cancelled)?
            || self.descends_from((declaring, caller), cancelled)?)
    }

    fn descends_from<Cancel>(
        self,
        lineage: (&SymbolId, &SymbolId),
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (mut descendant, ancestor) = lineage;
        for _ in 0..MAX_ANCESTRY_HOPS {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let Some(Some(parent)) = self.index.languages.php.parents.get(descendant) else {
                return Ok(false);
            };
            let Some(class) = self
                .class_like(parent, cancelled)?
                .filter(|class| class.kind == SymbolKind::Class)
            else {
                return Ok(false);
            };
            if &class.symbol_id == ancestor {
                return Ok(true);
            }
            descendant = &class.symbol_id;
        }
        Ok(false)
    }
}

/// The exact-resolution target of a candidate resolved from `file_id`.
fn exact_target(candidate: &ResolutionCandidate, file_id: &FileId) -> ResolvedTarget {
    if &candidate.file_id == file_id {
        ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: EXACT_SAME_FILE_CONFIDENCE,
            provenance: EXACT_SAME_FILE_PROVENANCE,
        }
    } else {
        project_resolved_target(candidate)
    }
}

/// Whether a candidate was declared in a PHP file.
fn php_candidate(index: &ResolutionIndex, candidate: &ResolutionCandidate) -> bool {
    index
        .modules
        .files
        .get(&candidate.file_id)
        .is_some_and(|file| file.language == SourceLanguage::Php.as_str())
}

/// Bind `Model::member` to the model class when the class is, by exact
/// `extends` ancestry through unique project classes, a Laravel Eloquent
/// model and no project declaration supplies the member: not the class, not
/// a project ancestor, and not a project trait either of them uses. The call
/// then reaches Eloquent's own static API or its `__callStatic` forwarding to
/// a query builder, both on behalf of the model class.
fn eloquent_model_target<Cancel>(
    exact: ExactQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some((owner, member)) = exact.key.rsplit_once(KEY_SEPARATOR) else {
        return Ok(None);
    };
    let Some(model) = exact
        .class_like(owner, cancelled)?
        .filter(|model| model.kind == SymbolKind::Class)
    else {
        return Ok(None);
    };
    let mut current = model;
    for _ in 0..MAX_ANCESTRY_HOPS {
        if exact.composes_member(
            Composition {
                class: current,
                member,
            },
            cancelled,
        )? {
            return Ok(None);
        }
        let Some(Some(parent)) = exact.index.languages.php.parents.get(&current.symbol_id) else {
            return Ok(None);
        };
        if exact.declares_member([parent, member], cancelled)? {
            return Ok(None);
        }
        if ELOQUENT_MODEL_BASES
            .iter()
            .any(|base| base.eq_ignore_ascii_case(parent))
        {
            return Ok(Some(ResolvedTarget {
                symbol_id: model.symbol_id.clone(),
                kind: model.kind,
                confidence: FRAMEWORK_CONVENTION_CONFIDENCE,
                provenance: ELOQUENT_MODEL_PROVENANCE,
            }));
        }
        let Some(ancestor) = exact
            .class_like(parent, cancelled)?
            .filter(|ancestor| ancestor.kind == SymbolKind::Class)
        else {
            return Ok(None);
        };
        current = ancestor;
    }
    Ok(None)
}

/// A class or trait and a member that its composed traits may supply.
#[derive(Clone, Copy)]
struct Composition<'query> {
    class: &'query ResolutionCandidate,
    member: &'query str,
}

impl<'query> ExactQuery<'query, '_> {
    /// Whether `[class, member]` names a method some PHP declaration has,
    /// visible or not; an unrepresentable key counts as declared.
    fn declares_member<Cancel>(
        self,
        method: [&str; 2],
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let [class, member] = method;
        let Some(key) = synthesized_key(&[class, KEY_SEPARATOR, member]) else {
            return Ok(true);
        };
        ExactQuery {
            intent: Intent::Member,
            key: &key,
            ..self
        }
        .declared(cancelled)
    }

    /// Whether a project trait the class uses, directly or through other
    /// project traits, may supply the member. Unknown or ambiguous composed
    /// types, and compositions too large to inspect, count as supplying it;
    /// types the project does not declare are vendor types whose members are
    /// part of the framework's own API.
    fn composes_member<Cancel>(
        self,
        composition: Composition<'query>,
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let mut pending = vec![composition.class];
        let mut visits = 0_usize;
        while let Some(class) = pending.pop() {
            visits = visits.saturating_add(1);
            if visits > MAX_TRAIT_VISITS {
                return Ok(true);
            }
            let current = Composition {
                class,
                ..composition
            };
            if self.visit_composed_traits((current, &mut pending), cancelled)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Inspect one class's composed types, queuing project traits to visit.
    fn visit_composed_traits<Cancel>(
        self,
        traversal: (Composition<'query>, &mut Vec<&'query ResolutionCandidate>),
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let (composition, pending) = traversal;
        let keys = match self
            .index
            .languages
            .php
            .composed
            .get(&composition.class.symbol_id)
        {
            None => return Ok(false),
            Some(None) => return Ok(true),
            Some(Some(keys)) => keys,
        };
        for key in keys {
            match self.class_like(key, cancelled)? {
                Some(used) if used.kind == SymbolKind::Trait => {
                    if self.declares_member([key, composition.member], cancelled)? {
                        return Ok(true);
                    }
                    pending.push(used);
                }
                Some(_) => {}
                None => {
                    let composed = ExactQuery {
                        intent: Intent::Class,
                        key,
                        ..self
                    };
                    if composed.declared(cancelled)? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

/// Resolve `member` called on the object `Class::factory()` returns, through
/// the factory's declared return type only. A non-public factory, or a
/// non-public method of the returned class, is followed only from inside that
/// class, where PHP grants access; elsewhere the call could reach
/// `__callStatic` or `__call` instead.
fn resolve_returned_member<Cancel>(
    exact: ExactQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ReferenceResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let unresolved = || ReferenceResolution::unresolved(DYNAMIC_DISPATCH_UNRESOLVED_PROVENANCE);
    let Some((factory, member)) = exact.key.rsplit_once(KEY_SEPARATOR) else {
        return Ok(unresolved());
    };
    let factory = ExactQuery {
        intent: Intent::Member,
        key: factory,
        ..exact
    };
    let Some(method) = factory.candidate(cancelled)?.filter(|method| {
        exact.intent == Intent::OwnReturnedMember || method.visibility == Some(Visibility::Public)
    }) else {
        return Ok(unresolved());
    };
    let Some(class) = returned_class(factory, method, cancelled)? else {
        return Ok(unresolved());
    };
    let Some(key) = synthesized_key(&[class, KEY_SEPARATOR, member]) else {
        return Ok(unresolved());
    };
    let inside_returned_class = exact.intent == Intent::OwnReturnedMember
        && split_last_segment(&method.qualified_name)
            .0
            .eq_ignore_ascii_case(class);
    let receiver = ExactQuery {
        intent: Intent::DispatchMember,
        key: &key,
        ..exact
    };
    Ok(receiver
        .candidate(cancelled)?
        .filter(|called| inside_returned_class || called.visibility == Some(Visibility::Public))
        .map_or_else(unresolved, |called| {
            ReferenceResolution::resolved(dispatched(exact_target(called, exact.file_id)))
        }))
}

/// The candidate key of the class a factory method's declared return type
/// names: the factory's own class for `self` or `static`, else the one class
/// its declaring file resolved the type to. Unions, intersections, and
/// builtins name none. The factory's class must be a single non-trait class
/// (a trait's methods can be replaced by the using class), and the returned
/// class must be a single declaration that is not a trait.
fn returned_class<'query, Cancel>(
    factory: ExactQuery<'query, '_>,
    method: &'query ResolutionCandidate,
    cancelled: &mut Cancel,
) -> Result<Option<&'query str>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (Some((class, _)), Some(returned)) = (
        method.qualified_name.rsplit_once(KEY_SEPARATOR),
        single_return_type(&method.signature),
    ) else {
        return Ok(None);
    };
    let returned =
        if returned.eq_ignore_ascii_case("self") || returned.eq_ignore_ascii_case("static") {
            Some(class)
        } else {
            factory
                .index
                .languages
                .php
                .returns
                .get(&method.symbol_id)
                .and_then(Option::as_deref)
        };
    let Some(returned) = returned else {
        return Ok(None);
    };
    for declared in [class, returned] {
        if factory
            .class_like(declared, cancelled)?
            .is_none_or(|declaration| declaration.kind == SymbolKind::Trait)
        {
            return Ok(None);
        }
    }
    Ok(Some(returned))
}

/// The one non-`null` type of a `(params): type` signature's return type.
fn single_return_type(signature: &str) -> Option<&str> {
    let returned = declared_return_type(signature)?;
    if returned.contains(['(', ')', '&']) {
        return None;
    }
    let returned = returned.strip_prefix('?').unwrap_or(returned);
    let mut types = returned
        .split('|')
        .map(str::trim)
        .filter(|part| !part.eq_ignore_ascii_case("null"));
    let single = types.next().filter(|part| !part.is_empty())?;
    types.next().is_none().then_some(single)
}

/// The text after the parameter list of a `(params): type` signature.
fn declared_return_type(signature: &str) -> Option<&str> {
    if !signature.starts_with('(') {
        return None;
    }
    let mut depth = 0_usize;
    for (offset, byte) in signature.bytes().enumerate() {
        match byte {
            b'(' => depth = depth.checked_add(1)?,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let (_, rest) = signature.split_at(offset);
                    return rest.strip_prefix("): ").map(str::trim);
                }
            }
            _ => {}
        }
    }
    None
}

/// A PHP `use` binding matched by a non-exact reference name.
#[derive(Clone, Copy)]
pub(super) struct PhpUseBinding<'context, 'request> {
    pub(super) index: &'context ResolutionIndex,
    pub(super) reference: &'context ResolutionRequest<'request>,
    pub(super) binding: &'context ExtractedImportBinding,
    pub(super) site: ImportReferenceSite,
}

/// Resolve a reference that names a PHP `use` alias (optionally followed by
/// `::member`) through the binding's fully qualified target: the alias alone
/// names a class-like declaration, and `Alias::member` names its method.
///
/// An import of a project declaration whose member is missing stays
/// unresolved; an import that names no project declaration is not bound, so
/// the existing external-binding rule keeps it from matching project names.
pub(super) fn resolve_use_binding<Cancel>(
    query: PhpUseBinding<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<ImportResolution, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let PhpUseBinding {
        index,
        reference,
        binding,
        site,
    } = query;
    let suffix = match site {
        ImportReferenceSite::Declaration => "",
        ImportReferenceSite::Usage => reference
            .name
            .strip_prefix(binding.local_name.as_str())
            .unwrap_or_default(),
    };
    if !(suffix.is_empty() || suffix.starts_with(KEY_SEPARATOR)) {
        return Ok(ImportResolution::NotBound);
    }
    let Some(imported) = binding_key(binding, "") else {
        return Ok(ImportResolution::Unresolved);
    };
    let Some(key) = binding_key(binding, suffix) else {
        return Ok(ImportResolution::Unresolved);
    };
    let imported = ExactQuery {
        index,
        file_id: reference.file_id,
        caller_class: caller_class(index, reference.owner),
        intent: Intent::Class,
        key: &imported,
    };
    let target = ExactQuery {
        intent: if suffix.is_empty() {
            Intent::Class
        } else {
            Intent::Member
        },
        key: &key,
        ..imported
    };
    if let Some(candidate) = target.candidate(cancelled)? {
        return Ok(ImportResolution::Resolved(import_binding_target(candidate)));
    }
    let mut imported_declared = false;
    for intent in [Intent::Class, Intent::Function, Intent::Constant] {
        if (ExactQuery { intent, ..imported }).declared(cancelled)? {
            imported_declared = true;
            break;
        }
    }
    Ok(if imported_declared {
        ImportResolution::Unresolved
    } else {
        ImportResolution::NotBound
    })
}

/// `Namespace::Name` (or `Name` for a global import) followed by `suffix`.
fn binding_key(binding: &ExtractedImportBinding, suffix: &str) -> Option<String> {
    if binding.imported_name.is_empty() {
        return None;
    }
    if binding.module_specifier == ROOT_NAMESPACE {
        synthesized_key(&[&binding.imported_name, suffix])
    } else {
        synthesized_key(&[
            &binding.module_specifier,
            KEY_SEPARATOR,
            &binding.imported_name,
            suffix,
        ])
    }
}

/// Concatenate key parts, refusing keys no candidate can have.
fn synthesized_key(parts: &[&str]) -> Option<String> {
    let length = parts
        .iter()
        .try_fold(0_usize, |length, part| length.checked_add(part.len()))?;
    if length > MAX_SYNTHESIZED_KEY_BYTES {
        return None;
    }
    let mut key = String::new();
    key.try_reserve_exact(length).ok()?;
    for part in parts {
        key.push_str(part);
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_lookups_parse_only_known_symbol_spaces() {
        let Some(lookup) = PhpExactLookup::parse(r"member::App\Models::User::find") else {
            panic!("member lookup must parse");
        };
        assert!(lookup.intent == Intent::Member);
        assert_eq!(lookup.key(), r"App\Models::User::find");
        let Some(returned) = PhpExactLookup::parse(r"returned-member::App::C::make::run") else {
            panic!("returned-member lookup must parse");
        };
        assert!(returned.intent == Intent::ReturnedMember && returned.intent.dispatch());
        assert!(PhpExactLookup::parse("namespace::App").is_none());
        assert!(PhpExactLookup::parse("class::").is_none());
        assert!(PhpExactLookup::parse("class").is_none());
    }

    #[test]
    fn symbol_spaces_never_cross_between_classes_and_functions() {
        assert!(Intent::Class.admits(SymbolKind::Trait));
        assert!(!Intent::Class.admits(SymbolKind::Function));
        assert!(!Intent::Function.admits(SymbolKind::Class));
        assert!(!Intent::Member.admits(SymbolKind::Property));
        assert!(Intent::FunctionFallback.admits(SymbolKind::Function));
        assert!(Intent::DispatchMember.admits(SymbolKind::Method));
        assert!(!Intent::ReturnedMember.admits(SymbolKind::Property));
        assert!(!Intent::Abstain.admits(SymbolKind::Class));
        assert!(Intent::DispatchClass.dispatch() && !Intent::Member.dispatch());
    }

    #[test]
    fn names_ignore_ascii_case_except_constant_names() {
        assert!(names_match(
            Intent::Function,
            r"App\Util::Ping",
            r"app\util::PING"
        ));
        assert!(!names_match(Intent::Class, "Caf\u{c9}", "caf\u{e9}"));
        assert!(names_match(
            Intent::Constant,
            r"App\Config::LIMIT",
            r"app\CONFIG::LIMIT"
        ));
        assert!(!names_match(
            Intent::Constant,
            r"App\Config::limit",
            r"App\Config::LIMIT"
        ));
        assert!(!names_match(Intent::Constant, "limit", "LIMIT"));
    }

    #[test]
    fn return_types_name_one_class_or_none() {
        assert_eq!(single_return_type("(string $c): ?self"), Some("self"));
        assert_eq!(single_return_type("(): static|null"), Some("static"));
        assert_eq!(
            single_return_type(r"((A&B)|null $x): \App\Client"),
            Some(r"\App\Client"),
            "parentheses in a parameter type do not end the parameter list"
        );
        assert_eq!(single_return_type("(): Client|Other"), None);
        assert_eq!(single_return_type("(): (A&B)|null"), None);
        assert_eq!(single_return_type("(): A&B"), None);
        assert_eq!(single_return_type("(): null"), None);
        assert_eq!(single_return_type("(int $x)"), None);
        assert_eq!(single_return_type(""), None);
        assert_eq!(single_return_type("(()"), None);
    }
}
