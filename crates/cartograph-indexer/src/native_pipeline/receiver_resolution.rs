//! Members of syntax-proven receiver types. Only lexical/import class bindings
//! participate; unknown, ambiguous, or incomplete inheritance always abstains.

use std::{
    collections::{HashMap, HashSet},
    mem::{size_of, take},
};

use cartograph_extract::{
    EXPLICIT_RECEIVER_IMPORT_PREFIX, EXPLICIT_RECEIVER_RESOLUTION_PREFIX, ExtractedImportBinding,
    ExtractedReceiverBinding, ExtractedReceiverEvidence, ExtractedReceiverLookup,
    ExtractedReference,
};

use super::{
    FileId, FileImportBindingIndex, FileResolutionContext, IMPORT_BINDING_CONFIDENCE,
    ImportBindingScratch, ImportReferenceSite, ImportResolution, ImportResolutionRequest,
    NativeFileFacts, RESOLUTION_MAP_NODE_ALLOWANCE, ReferenceDispatch, ReferenceKind,
    ReferenceResolution, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest, ResolveBudget,
    ResolvedTarget, SourceSpan, StageItemFailure, SymbolId, SymbolKind, Visibility, resolve_import,
    try_clone_text, usize_to_u64, vector_capacity_bytes,
};

const MAX_ANCESTORS: usize = 32;
const EXPLICIT_PROVENANCE: &str = "native-explicit-receiver-type";
const INHERITED_PROVENANCE: &str = "native-inherited-receiver-type";

#[derive(Default)]
pub(super) struct ReceiverIndex {
    classes: HashMap<String, Class>,
    parents: HashMap<String, Vec<Option<String>>>,
    pending: Vec<ParentFile>,
    // Without a proven assignment owner, withhold added receiver resolution for
    // this name. The existing resolver remains authoritative.
    unproven_assignments: HashSet<String>,
}

struct Class {
    symbol_id: SymbolId,
    file_id: FileId,
    members: HashMap<String, IndexedMember>,
    non_methods: HashSet<String>,
    assigned_members: HashSet<String>,
    fenced: bool,
}

enum IndexedMember {
    Unique(MemberDefinition),
    Ambiguous,
}

struct MemberDefinition {
    symbol_id: SymbolId,
    kind: SymbolKind,
    visibility: Option<Visibility>,
    instance_allowed: bool,
}

struct ParentFile {
    file_id: FileId,
    imports: Vec<ExtractedImportBinding>,
    declarations: Vec<ParentDeclaration>,
}

struct ParentDeclaration {
    owner: SymbolId,
    name: String,
    span: SourceSpan,
    blocked: bool,
}

pub(super) struct ReceiverQuery<'context, 'file> {
    pub(super) context: &'context FileResolutionContext<'file>,
    pub(super) reference: &'context ExtractedReference,
    pub(super) import_binding_scratch: &'context mut ImportBindingScratch,
}

pub(super) struct FileLookups<'lookups> {
    sites: HashMap<(u64, u64, ReferenceKind), Option<&'lookups str>>,
}

enum LookupEvidence<'lookup> {
    Missing,
    Ambiguous,
    Proven(&'lookup str),
}

impl<'lookup> LookupEvidence<'lookup> {
    fn known(self) -> Option<&'lookup str> {
        match self {
            Self::Proven(lookup) => Some(lookup),
            Self::Missing | Self::Ambiguous => None,
        }
    }
}

impl<'lookups> FileLookups<'lookups> {
    pub(super) fn new<Cancel>(
        lookups: &'lookups [ExtractedReceiverLookup],
        budget: &mut ResolveBudget,
        cancelled: &mut Cancel,
    ) -> Result<Self, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let mut sites = HashMap::new();
        for lookup in lookups {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let key = (
                lookup.span.start_byte(),
                lookup.span.end_byte(),
                lookup.kind,
            );
            if let Some(prior) = sites.get_mut(&key) {
                if *prior != Some(lookup.lookup.as_str()) {
                    *prior = None;
                }
                continue;
            }
            budget.charge(RESOLUTION_MAP_NODE_ALLOWANCE.saturating_add(usize_to_u64(
                size_of::<((u64, u64, ReferenceKind), Option<&str>)>(),
            )))?;
            sites.try_reserve(1).map_err(|_| StageItemFailure)?;
            sites.insert(key, Some(lookup.lookup.as_str()));
        }
        Ok(Self { sites })
    }

    fn get(&self, reference: &ExtractedReference) -> LookupEvidence<'lookups> {
        match self.sites.get(&(
            reference.span.start_byte(),
            reference.span.end_byte(),
            reference.kind,
        )) {
            None => LookupEvidence::Missing,
            Some(None) => LookupEvidence::Ambiguous,
            Some(Some(lookup)) => LookupEvidence::Proven(lookup),
        }
    }
}

pub(super) fn index_file<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if !matches!(
        file.file.language.as_str(),
        "python" | "go" | "typescript" | "tsx" | "javascript" | "jsx"
    ) {
        return Ok(());
    }
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if !nominal(symbol.kind) {
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(String, Class)>()))
                .saturating_add(
                    usize_to_u64(symbol.input.symbol_id.as_str().len()).saturating_mul(2),
                )
                .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
        )?;
        target
            .index
            .receivers
            .classes
            .try_reserve(1)
            .map_err(|_| StageItemFailure)?;
        target.index.receivers.classes.insert(
            try_clone_text(symbol.input.symbol_id.as_str())?,
            Class {
                symbol_id: symbol.input.symbol_id.clone(),
                file_id: file.file.file_id.clone(),
                members: HashMap::new(),
                non_methods: HashSet::new(),
                assigned_members: HashSet::new(),
                fenced: false,
            },
        );
    }
    index_members(target, file, cancelled)?;
    index_non_methods(target, file, cancelled)?;
    index_parents(target, file, cancelled)
}

fn index_members<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for symbol in &file.symbols {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(class) = target
            .index
            .parents
            .get(&symbol.input.symbol_id)
            .and_then(|owner| target.index.receivers.classes.get_mut(owner.as_str()))
        else {
            continue;
        };
        if let Some(member) = class.members.get_mut(&symbol.name) {
            *member = IndexedMember::Ambiguous;
            continue;
        }
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(String, IndexedMember)>()))
                .saturating_add(usize_to_u64(symbol.name.len()))
                .saturating_add(usize_to_u64(symbol.input.symbol_id.as_str().len())),
        )?;
        class.members.try_reserve(1).map_err(|_| StageItemFailure)?;
        class.members.insert(
            try_clone_text(&symbol.name)?,
            IndexedMember::Unique(MemberDefinition {
                symbol_id: symbol.input.symbol_id.clone(),
                kind: symbol.kind,
                visibility: symbol.visibility,
                instance_allowed: matches!(file.file.language.as_str(), "python" | "go")
                    || !symbol.execution.static_member,
            }),
        );
    }
    Ok(())
}

pub(super) fn evidence_bytes(evidence: Option<&ExtractedReceiverEvidence>) -> u64 {
    let Some(evidence) = evidence else { return 0 };
    let bytes = usize_to_u64(size_of::<ExtractedReceiverEvidence>())
        .saturating_add(vector_capacity_bytes(&evidence.bindings))
        .saturating_add(vector_capacity_bytes(&evidence.lookups));
    evidence
        .bindings
        .iter()
        .fold(bytes, |total, binding| {
            total
                .saturating_add(
                    binding
                        .class_id
                        .as_ref()
                        .map_or(0, |id| usize_to_u64(id.as_str().len())),
                )
                .saturating_add(
                    binding
                        .name
                        .as_ref()
                        .map_or(0, |name| usize_to_u64(name.capacity())),
                )
        })
        .saturating_add(evidence.lookups.iter().fold(0_u64, |total, lookup| {
            total.saturating_add(usize_to_u64(lookup.lookup.capacity()))
        }))
}

fn index_non_methods<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let Some(evidence) = file.receiver_evidence.as_deref() else {
        return Ok(());
    };
    for binding in &evidence.bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let names = binding_names(&mut target.index.receivers, binding)?;
        let (Some(names), Some(name)) = (names, binding.name.as_deref()) else {
            continue;
        };
        index_member_name(names, target.budget, name)?;
    }
    Ok(())
}

fn binding_names<'index>(
    index: &'index mut ReceiverIndex,
    binding: &ExtractedReceiverBinding,
) -> Result<Option<&'index mut HashSet<String>>, StageItemFailure> {
    let Some(owner) = &binding.class_id else {
        return Ok(binding.assigned.then_some(&mut index.unproven_assignments));
    };
    let class = index
        .classes
        .get_mut(owner.as_str())
        .ok_or(StageItemFailure)?;
    if binding.name.is_none() {
        class.fenced = true;
        return Ok(None);
    }
    Ok(Some(if binding.assigned {
        &mut class.assigned_members
    } else {
        &mut class.non_methods
    }))
}

fn index_member_name(
    names: &mut HashSet<String>,
    budget: &mut ResolveBudget,
    name: &str,
) -> Result<(), StageItemFailure> {
    if names.contains(name) {
        return Ok(());
    }
    budget.charge(
        RESOLUTION_MAP_NODE_ALLOWANCE
            .saturating_add(usize_to_u64(size_of::<String>()))
            .saturating_add(usize_to_u64(name.len())),
    )?;
    names.try_reserve(1).map_err(|_| StageItemFailure)?;
    names.insert(try_clone_text(name)?);
    Ok(())
}

fn index_parents<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    file: &NativeFileFacts,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if file.file.language != "python" {
        return Ok(());
    }
    let lookups = FileLookups::new(
        file.receiver_evidence
            .as_deref()
            .map_or(&[], |evidence| evidence.lookups.as_slice()),
        target.budget,
        cancelled,
    )?;
    let mut declarations = Vec::new();
    for reference in &file.references {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(owner) = reference
            .owner
            .as_ref()
            .filter(|owner| target.index.receivers.classes.contains_key(owner.as_str()))
        else {
            continue;
        };
        if !matches!(
            reference.kind,
            ReferenceKind::Extends | ReferenceKind::Implements
        ) {
            continue;
        }
        let name = parent_name(&lookups, reference);
        target.budget.charge(
            usize_to_u64(size_of::<ParentDeclaration>())
                .saturating_add(usize_to_u64(name.len()))
                .saturating_add(usize_to_u64(owner.as_str().len())),
        )?;
        declarations
            .try_reserve_exact(1)
            .map_err(|_| StageItemFailure)?;
        declarations.push(ParentDeclaration {
            owner: owner.clone(),
            name: try_clone_text(name)?,
            span: reference.span,
            blocked: reference.resolution_name.as_deref().is_some_and(|name| {
                name.starts_with(super::PYTHON_UNBOUND_IMPORT_RESOLUTION_PREFIX)
            }),
        });
    }
    if declarations.is_empty() {
        return Ok(());
    }
    let mut imports = Vec::new();
    for binding in &file.import_bindings {
        if cancelled() {
            return Err(StageItemFailure);
        }
        target.budget.charge(
            usize_to_u64(size_of::<ExtractedImportBinding>())
                .saturating_add(usize_to_u64(binding.module_specifier.len()))
                .saturating_add(usize_to_u64(binding.imported_name.len()))
                .saturating_add(usize_to_u64(binding.local_name.len())),
        )?;
        imports.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
        imports.push(ExtractedImportBinding {
            kind: binding.kind,
            module_specifier: try_clone_text(&binding.module_specifier)?,
            imported_name: try_clone_text(&binding.imported_name)?,
            local_name: try_clone_text(&binding.local_name)?,
            span: binding.span,
        });
    }
    target.budget.charge(
        usize_to_u64(size_of::<ParentFile>())
            .saturating_add(usize_to_u64(file.file.file_id.as_str().len())),
    )?;
    target
        .index
        .receivers
        .pending
        .try_reserve_exact(1)
        .map_err(|_| StageItemFailure)?;
    target.index.receivers.pending.push(ParentFile {
        file_id: file.file.file_id.clone(),
        imports,
        declarations,
    });
    Ok(())
}

fn parent_name<'lookups>(
    lookups: &FileLookups<'lookups>,
    reference: &ExtractedReference,
) -> &'lookups str {
    lookups
        .get(reference)
        .known()
        .and_then(|lookup| lookup.strip_prefix(EXPLICIT_RECEIVER_RESOLUTION_PREFIX))
        .and_then(|lookup| lookup.split_once('#'))
        .filter(|(_, member)| member.is_empty())
        .map_or("?", |(receiver, _)| receiver)
}

pub(super) fn finish_index<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    for file in take(&mut target.index.receivers.pending) {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let language = target
            .index
            .modules
            .files
            .get(&file.file_id)
            .ok_or(StageItemFailure)?
            .language
            .clone();
        let bindings = FileImportBindingIndex::new(&file.imports, target.budget, &language)?;
        let mut scratch = ImportBindingScratch::new(file.imports.len(), target.budget)?;
        for declaration in file.declarations {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let metadata = target
                .index
                .modules
                .files
                .get(&file.file_id)
                .ok_or(StageItemFailure)?;
            let imported_name = declaration
                .name
                .strip_prefix(EXPLICIT_RECEIVER_IMPORT_PREFIX);
            let name = imported_name.unwrap_or(&declaration.name);
            let request = ResolutionRequest {
                file_id: &file.file_id,
                file_path: &metadata.path,
                language: &metadata.language,
                import_bindings: scratch.select(&bindings, name),
                owner: None,
                name,
                dispatch: ReferenceDispatch::Static,
                kind: ReferenceKind::TypeOf,
                span: declaration.span,
            };
            let parent = parent_class(target.index, (&declaration, &request), cancelled)?
                .map(|class| try_clone_text(class.symbol_id.as_str()))
                .transpose()?;
            record_parent(target, &declaration.owner, parent)?;
        }
    }
    Ok(())
}

fn parent_class<'index, Cancel>(
    index: &'index ResolutionIndex,
    query: (&ParentDeclaration, &ResolutionRequest<'_>),
    cancelled: &mut Cancel,
) -> Result<Option<&'index Class>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (declaration, request) = query;
    // Nested class bases need their declaration namespace, not the receiver's
    // method scope. That broader namespace model is deferred.
    if declaration.blocked || index.parents.contains_key(&declaration.owner) {
        return Ok(None);
    }
    if let Some(id) = declaration.name.strip_prefix('@') {
        return Ok(local_type(index, (id, request.file_id)));
    }
    if declaration
        .name
        .starts_with(EXPLICIT_RECEIVER_IMPORT_PREFIX)
    {
        return imported_type(index, request, cancelled);
    }
    Ok(None)
}

fn record_parent(
    target: &mut ResolutionIndexTarget<'_>,
    owner: &SymbolId,
    parent: Option<String>,
) -> Result<(), StageItemFailure> {
    let parents = &mut target.index.receivers.parents;
    if !parents.contains_key(owner.as_str()) {
        target.budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE
                .saturating_add(usize_to_u64(size_of::<(String, Vec<Option<String>>)>()))
                .saturating_add(usize_to_u64(owner.as_str().len())),
        )?;
        parents.try_reserve(1).map_err(|_| StageItemFailure)?;
        parents.insert(try_clone_text(owner.as_str())?, Vec::new());
    }
    target.budget.charge(
        usize_to_u64(size_of::<Option<String>>()).saturating_add(
            parent
                .as_ref()
                .map_or(0, |parent| usize_to_u64(parent.len())),
        ),
    )?;
    let values = parents.get_mut(owner.as_str()).ok_or(StageItemFailure)?;
    if values.len() == MAX_ANCESTORS {
        values[0] = None;
        return Ok(());
    }
    values.try_reserve_exact(1).map_err(|_| StageItemFailure)?;
    values.push(parent);
    Ok(())
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    query: ReceiverQuery<'_, '_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let ReceiverQuery {
        context,
        reference,
        import_binding_scratch,
    } = query;
    if matches!(
        reference.kind,
        ReferenceKind::Extends | ReferenceKind::Implements
    ) {
        return Ok(None);
    }
    let marker = context.receiver_lookups.get(reference);
    if matches!(marker, LookupEvidence::Missing) {
        return Ok(None);
    }
    if cancelled() {
        return Err(StageItemFailure);
    }
    let Some(payload) = marker
        .known()
        .and_then(|name| name.strip_prefix(EXPLICIT_RECEIVER_RESOLUTION_PREFIX))
    else {
        return Ok(None);
    };
    let Some((receiver, member)) = payload.split_once('#') else {
        return Ok(None);
    };
    if context.identity.language == "python" && member.starts_with("__") && !member.ends_with("__")
    {
        return Ok(None);
    }
    let class = if let Some(id) = receiver.strip_prefix('@') {
        local_type(index, (id, &context.identity.file_id))
    } else if let Some(receiver) = receiver.strip_prefix(EXPLICIT_RECEIVER_IMPORT_PREFIX) {
        let request = ResolutionRequest {
            file_id: &context.identity.file_id,
            file_path: &context.identity.path,
            language: &context.identity.language,
            import_bindings: import_binding_scratch.select(context.import_bindings, receiver),
            owner: reference.owner.as_ref(),
            name: receiver,
            dispatch: ReferenceDispatch::Static,
            kind: ReferenceKind::TypeOf,
            span: reference.span,
        };
        imported_type(index, &request, cancelled)?
    } else {
        None
    };
    let Some(class) = class else {
        return Ok(None);
    };
    let mut search = MemberSearch {
        index,
        class,
        receiver: class,
        name: member,
        kind: reference.kind,
    };
    let target = match direct_member(&mut search, cancelled)? {
        Member::Unique(candidate) => Some(member_target(candidate, false)),
        Member::Missing => {
            inherited_member(search, cancelled)?.map(|candidate| member_target(candidate, true))
        }
        Member::Ambiguous => None,
    };
    Ok(target)
}

pub(super) fn prefer_base(
    base: ReferenceResolution,
    receiver: Option<ResolvedTarget>,
) -> ReferenceResolution {
    match receiver {
        Some(target)
            if base
                .target
                .as_ref()
                .is_none_or(|base| base.symbol_id != target.symbol_id) =>
        {
            ReferenceResolution::resolved(target)
        }
        _ => base,
    }
}

fn local_type<'index>(
    index: &'index ResolutionIndex,
    lookup: (&str, &FileId),
) -> Option<&'index Class> {
    index
        .receivers
        .classes
        .get(lookup.0)
        .filter(|class| &class.file_id == lookup.1)
}

fn imported_type<'index, Cancel>(
    index: &'index ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index Class>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let imported = resolve_import(
        index,
        ImportResolutionRequest {
            reference: request,
            site: ImportReferenceSite::Usage,
        },
        cancelled,
    )?;
    let target = match imported {
        ImportResolution::Resolved(target) if target.confidence >= IMPORT_BINDING_CONFIDENCE => {
            Some(target)
        }
        ImportResolution::NotBound
        | ImportResolution::Unresolved
        | ImportResolution::Resolved(_) => None,
    };
    let target = match target {
        Some(target) => Some(target),
        None => super::go_path_resolution::resolve(index, request, cancelled)?
            .and_then(|resolution| resolution.target),
    };
    Ok(target.and_then(|target| index.receivers.classes.get(target.symbol_id.as_str())))
}

struct MemberSearch<'index> {
    index: &'index ResolutionIndex,
    class: &'index Class,
    receiver: &'index Class,
    name: &'index str,
    kind: ReferenceKind,
}

enum Member<'candidate> {
    Missing,
    Unique(&'candidate MemberDefinition),
    Ambiguous,
}

fn direct_member<'index, Cancel>(
    search: &mut MemberSearch<'index>,
    cancelled: &mut Cancel,
) -> Result<Member<'index>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    if search.class.fenced
        || search.class.assigned_members.contains(search.name)
        || search
            .index
            .receivers
            .unproven_assignments
            .contains(search.name)
    {
        return Ok(Member::Ambiguous);
    }
    let blocked = search.class.non_methods.contains(search.name);
    if blocked && search.kind == ReferenceKind::Calls {
        return Ok(Member::Ambiguous);
    }
    Ok(match search.class.members.get(search.name) {
        Some(IndexedMember::Unique(candidate)) if member_matches(search, candidate) => {
            Member::Unique(candidate)
        }
        Some(_) => Member::Ambiguous,
        None if blocked => Member::Ambiguous,
        None => Member::Missing,
    })
}

fn member_matches(search: &MemberSearch<'_>, candidate: &MemberDefinition) -> bool {
    if !member_kind(search.kind, candidate.kind)
        || !candidate.instance_allowed
        || (search.class.non_methods.contains(search.name) && candidate.kind == SymbolKind::Method)
    {
        return false;
    }
    search.class.symbol_id == search.receiver.symbol_id
        || candidate.visibility != Some(Visibility::Private)
}

fn inherited_member<'index, Cancel>(
    mut search: MemberSearch<'index>,
    cancelled: &mut Cancel,
) -> Result<Option<&'index MemberDefinition>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut walk = AncestryWalk::new(search.class);
    while walk.count > 0 {
        if cancelled() {
            return Err(StageItemFailure);
        }
        walk.count -= 1;
        let class = walk.pending[walk.count].take().ok_or(StageItemFailure)?;
        if !walk.observe(class) {
            return Ok(None);
        }
        let parents = search
            .index
            .receivers
            .parents
            .get(class.symbol_id.as_str())
            .map_or(&[] as &[_], Vec::as_slice);
        for parent in parents {
            if cancelled() {
                return Err(StageItemFailure);
            }
            let Some(parent) = parent
                .as_deref()
                .and_then(|id| search.index.receivers.classes.get(id))
            else {
                return Ok(None);
            };
            search.class = parent;
            if !walk.visit(&mut search, cancelled)? {
                return Ok(None);
            }
        }
    }
    Ok(walk.matched)
}

struct AncestryWalk<'index> {
    pending: [Option<&'index Class>; MAX_ANCESTORS],
    seen: [Option<&'index SymbolId>; MAX_ANCESTORS],
    count: usize,
    seen_count: usize,
    matched: Option<&'index MemberDefinition>,
}

impl<'index> AncestryWalk<'index> {
    fn new(class: &'index Class) -> Self {
        let mut pending = [None; MAX_ANCESTORS];
        pending[0] = Some(class);
        Self {
            pending,
            seen: [None; MAX_ANCESTORS],
            count: 1,
            seen_count: 0,
            matched: None,
        }
    }

    fn observe(&mut self, class: &'index Class) -> bool {
        if self.seen_count == MAX_ANCESTORS
            || self.seen[..self.seen_count].contains(&Some(&class.symbol_id))
        {
            return false;
        }
        self.seen[self.seen_count] = Some(&class.symbol_id);
        self.seen_count += 1;
        true
    }

    fn visit<Cancel>(
        &mut self,
        search: &mut MemberSearch<'index>,
        cancelled: &mut Cancel,
    ) -> Result<bool, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        match direct_member(search, cancelled)? {
            Member::Unique(candidate) => {
                if self
                    .matched
                    .is_some_and(|prior| prior.symbol_id != candidate.symbol_id)
                {
                    return Ok(false);
                }
                self.matched = Some(candidate);
                Ok(true)
            }
            Member::Missing if self.count < MAX_ANCESTORS => {
                self.pending[self.count] = Some(search.class);
                self.count += 1;
                Ok(true)
            }
            Member::Missing | Member::Ambiguous => Ok(false),
        }
    }
}

fn member_target(candidate: &MemberDefinition, inherited: bool) -> ResolvedTarget {
    ResolvedTarget {
        symbol_id: candidate.symbol_id.clone(),
        kind: candidate.kind,
        confidence: if inherited { 0.9 } else { 0.95 },
        provenance: if inherited {
            INHERITED_PROVENANCE
        } else {
            EXPLICIT_PROVENANCE
        },
    }
}

const fn nominal(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface
    )
}

const fn member_kind(reference: ReferenceKind, kind: SymbolKind) -> bool {
    match reference {
        ReferenceKind::Calls => matches!(kind, SymbolKind::Method),
        ReferenceKind::FieldAccess => matches!(
            kind,
            SymbolKind::Field | SymbolKind::Property | SymbolKind::Method
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_pipeline::{
        NativeFactAccumulator, ResolutionIndexContext, build_resolution_index,
    };
    use cartograph_extract::{NativeExtractor, SourceLimits, SourceRoot, SourceSnapshot};
    use std::cell::Cell;

    fn repeated_class_index(count: usize) -> ResolutionIndex {
        const MAXIMUM_BYTES: u64 = 128 * 1024 * 1024;
        let source = "class C:\n    def m(self): pass\n    def f(self): self.m()\n".repeat(count);
        let limits =
            SourceLimits::new(1024 * 1024).unwrap_or_else(|error| panic!("source limits: {error}"));
        let snapshot = SourceSnapshot::from_bytes("main.py", source.as_bytes(), limits)
            .unwrap_or_else(|error| panic!("source snapshot: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("Python grammar: {error}"));
        let extracted = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("Python extraction: {error}"));
        let mut accumulator = NativeFactAccumulator::new(MAXIMUM_BYTES);
        accumulator
            .push(extracted)
            .unwrap_or_else(|_| panic!("native fact bound"));
        let root = SourceRoot::open(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|error| panic!("test source root: {error}"));
        let mut budget =
            ResolveBudget::new(0, MAXIMUM_BYTES).unwrap_or_else(|_| panic!("resolve budget"));
        let index = build_resolution_index(
            &accumulator,
            ResolutionIndexContext {
                source_root: &root,
                budget: &mut budget,
                cancelled: &mut || false,
            },
        )
        .unwrap_or_else(|_| panic!("resolution index"));
        assert!(budget.charged_bytes <= MAXIMUM_BYTES);
        index
    }

    #[test]
    fn repeated_class_members_have_a_linear_lookup_work_bound() {
        for count in [32_usize, 128] {
            let index = repeated_class_index(count);
            assert_eq!(index.receivers.classes.len(), count);
            let polls = Cell::new(0_usize);
            let mut cancelled = || {
                polls.set(polls.get() + 1);
                false
            };
            for class in index.receivers.classes.values() {
                let mut search = MemberSearch {
                    index: &index,
                    class,
                    receiver: class,
                    name: "m",
                    kind: ReferenceKind::Calls,
                };
                let Member::Unique(target) = direct_member(&mut search, &mut cancelled)
                    .unwrap_or_else(|_| panic!("member lookup"))
                else {
                    panic!("distinct owner lost its method");
                };
                assert_eq!(index.parents.get(&target.symbol_id), Some(&class.symbol_id));
            }
            assert!(
                polls.get() <= 2 * count,
                "{count} owners used {} polls",
                polls.get()
            );
        }
    }
}
