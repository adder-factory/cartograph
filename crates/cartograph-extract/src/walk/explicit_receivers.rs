//! Explicit Python/Go receiver declarations and direct JavaScript `this` fields.
//! No factory return types, assignment propagation, or name-shape inference.

mod go;
mod javascript;
mod python;

use std::{collections::HashMap, mem::size_of};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};
use tree_sitter::Node;

use super::{AstVisitBudget, ExtractionBuilder, ExtractionContext, syntax::named_children};
use crate::{
    EXPLICIT_RECEIVER_IMPORT_PREFIX, EXPLICIT_RECEIVER_RESOLUTION_PREFIX, ExtractError,
    ExtractedReceiverBinding, ExtractedReceiverEvidence, ExtractedReceiverLookup,
    ExtractedReference,
};

const MAP_ALLOWANCE: u64 = 128;
const MAX_NAME_BYTES: usize = 512;
const MAX_RECEIVER_FIELDS: usize = 2;

pub(super) fn finish(
    builder: &mut ExtractionBuilder<'_, '_>,
) -> Result<Option<Box<ExtractedReceiverEvidence>>, ExtractError> {
    if builder.facts.receiver_lookups.is_empty() && builder.facts.receiver_bindings.is_empty() {
        return Ok(None);
    }
    builder.context.budget.reserve_working_bytes(
        u64::try_from(size_of::<ExtractedReceiverEvidence>())
            .map_err(|_| ExtractError::OutputLimit)?,
    )?;
    Ok(Some(Box::new(ExtractedReceiverEvidence {
        lookups: std::mem::take(&mut builder.facts.receiver_lookups),
        bindings: std::mem::take(&mut builder.facts.receiver_bindings),
    })))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Class,
    Callable,
    Block,
    Opaque,
    Barrier,
}

enum BindingType {
    Unknown,
    Method,
    Import,
    Nominal(SymbolId),
    Receiver(SymbolId),
    Explicit {
        name: String,
        type_scope: usize,
        position: Option<usize>,
    },
}

struct Binding {
    kind: BindingType,
    start: usize,
}

struct Scope {
    parent: Option<usize>,
    kind: ScopeKind,
    nominal: Option<SymbolId>,
    bindings: HashMap<String, Binding>,
    heritage_supported: bool,
    fenced: bool,
}

#[derive(Clone, Copy)]
struct Visit<'tree> {
    node: Node<'tree>,
    scope: usize,
    depth: usize,
}

#[derive(Clone, Copy)]
struct MemberSite<'tree> {
    receiver: Node<'tree>,
    member: Node<'tree>,
    scope: usize,
    start: usize,
    end: usize,
    literal: bool,
}

#[derive(Clone, Copy)]
struct MemberParts<'tree> {
    visit: Visit<'tree>,
    receiver: Option<Node<'tree>>,
    member: Option<Node<'tree>>,
}

const UNKNOWN_BINDING: Binding = Binding {
    kind: BindingType::Unknown,
    start: 0,
};

#[derive(Default)]
struct SyntaxIndex<'tree> {
    scopes: HashMap<usize, Scope>,
    declarations: HashMap<usize, SymbolId>,
    class_scopes: HashMap<String, usize>,
    sites: HashMap<(usize, usize), MemberSite<'tree>>,
    assignments: Vec<MemberSite<'tree>>,
    non_methods: Vec<ExtractedReceiverBinding>,
}

pub(super) fn enrich(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    let language = builder.context.snapshot.language();
    if !supported(language) || root.has_error() || !builder.optional_facts.admit() {
        return Ok(());
    }
    let mut index = prepare_index(builder, root)?;
    index.collect(
        builder,
        Visit {
            node: root,
            scope: root.id(),
            depth: 0,
        },
    )?;
    index.fence_assignments(&mut builder.context)?;
    builder.facts.receiver_bindings = std::mem::take(&mut index.non_methods);
    for reference in &builder.facts.references {
        builder.context.ensure_active()?;
        if let Some(lookup) = index.reference_lookup(&mut builder.context, reference)? {
            builder.context.budget.reserve_fact(
                u64::try_from(size_of::<ExtractedReceiverLookup>())
                    .map_err(|_| ExtractError::OutputLimit)?
                    .saturating_mul(2)
                    .saturating_add(
                        u64::try_from(lookup.lookup.len())
                            .map_err(|_| ExtractError::OutputLimit)?,
                    ),
                [lookup.lookup.as_str()],
            )?;
            builder
                .facts
                .receiver_lookups
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            builder.facts.receiver_lookups.push(lookup);
        }
    }
    Ok(())
}

fn prepare_index<'tree>(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'tree>,
) -> Result<SyntaxIndex<'tree>, ExtractError> {
    let mut index = SyntaxIndex::default();
    for symbol in &builder.facts.symbols {
        builder.context.ensure_active()?;
        if matches!(
            symbol.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface
        ) {
            reserve_map::<(usize, SymbolId)>(&mut builder.context)?;
            reserve_text(&mut builder.context, symbol.id.as_str())?;
            index
                .declarations
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            index.declarations.insert(
                usize::try_from(symbol.span.start_byte()).map_err(|_| ExtractError::OutputLimit)?,
                symbol.id.clone(),
            );
        }
    }
    index.add_scope(
        &mut builder.context,
        Visit {
            node: root,
            scope: root.id(),
            depth: 0,
        },
        ScopeKind::Module,
    )?;
    if builder.context.snapshot.language() == SourceLanguage::Go {
        for binding in &builder.facts.import_bindings {
            builder.context.ensure_active()?;
            index.bind_name(
                &mut builder.context,
                NamedBind {
                    scope: root.id(),
                    name: &binding.local_name,
                    kind: BindingType::Import,
                    start: 0,
                },
            )?;
        }
    }
    Ok(index)
}

impl SyntaxIndex<'_> {
    fn reference_lookup(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        reference: &ExtractedReference,
    ) -> Result<Option<ExtractedReceiverLookup>, ExtractError> {
        if matches!(
            reference.kind,
            ReferenceKind::Extends | ReferenceKind::Implements
        ) && context.snapshot.language() == SourceLanguage::Python
        {
            return self.parent_lookup(context, reference);
        }
        if reference.kind == ReferenceKind::Calls
            && !matches!(
                context.snapshot.language(),
                SourceLanguage::Python | SourceLanguage::Go
            )
        {
            return Ok(None);
        }
        if !matches!(
            reference.kind,
            ReferenceKind::Calls | ReferenceKind::FieldAccess
        ) {
            return Ok(None);
        }
        let key = (
            usize::try_from(reference.span.start_byte()).map_err(|_| ExtractError::OutputLimit)?,
            usize::try_from(reference.span.end_byte()).map_err(|_| ExtractError::OutputLimit)?,
        );
        let Some(site) = self.sites.get(&key) else {
            return Ok(None);
        };
        if let Some(receiver) = self.types().receiver_type(context, *site)? {
            let member = node_text(context, site.member);
            if !identifier(member) {
                return Ok(None);
            }
            let marker = member_marker(context, (&receiver, member))?;
            return Ok(Some(ExtractedReceiverLookup {
                span: reference.span,
                kind: reference.kind,
                lookup: marker,
            }));
        }
        Ok(None)
    }

    fn parent_lookup(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        reference: &ExtractedReference,
    ) -> Result<Option<ExtractedReceiverLookup>, ExtractError> {
        let Some(scope) = reference
            .owner
            .as_ref()
            .and_then(|owner| self.class_scopes.get(owner.as_str()))
            .and_then(|scope| self.scopes.get(scope))
        else {
            return Ok(None);
        };
        let receiver = if scope.heritage_supported {
            self.types().explicit_type(
                context,
                TypeQuery {
                    name: &reference.name,
                    scope: scope.parent.ok_or(ExtractError::InvalidSpan)?,
                    class_bindings: false,
                    position: Some(
                        usize::try_from(reference.span.start_byte())
                            .map_err(|_| ExtractError::OutputLimit)?,
                    ),
                },
            )?
        } else {
            None
        };
        Ok(Some(ExtractedReceiverLookup {
            span: reference.span,
            kind: reference.kind,
            lookup: member_marker(context, (receiver.as_deref().unwrap_or("?"), ""))?,
        }))
    }
}

impl<'tree> SyntaxIndex<'tree> {
    /// Borrow the collected scopes for type queries without changing index state.
    fn types(&self) -> ReceiverTypes<'_> {
        ReceiverTypes {
            scopes: &self.scopes,
            class_scopes: &self.class_scopes,
        }
    }

    fn collect(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        root: Visit<'tree>,
    ) -> Result<(), ExtractError> {
        let mut budget = AstVisitBudget::<{ super::MAX_AST_DEPTH }>::default();
        let mut pending = Vec::new();
        push_visit(&mut builder.context, &mut pending, root)?;
        while let Some(mut visit) = pending.pop() {
            budget.observe(builder, visit.depth)?;
            let kind = if builder.context.snapshot.language() == SourceLanguage::Python {
                python::scope_kind(visit.node)
            } else if builder.context.snapshot.language() == SourceLanguage::Go {
                go::scope_kind(visit.node)
            } else {
                javascript::scope_kind(visit.node)
            };
            if let Some(kind) = kind {
                self.add_scope(&mut builder.context, visit, kind)?;
                visit.scope = visit.node.id();
            }
            if builder.context.snapshot.language() == SourceLanguage::Python {
                python::collect(self, &mut builder.context, visit)?;
            } else if builder.context.snapshot.language() == SourceLanguage::Go {
                go::collect(self, &mut builder.context, visit)?;
            } else {
                javascript::collect(self, &mut builder.context, visit)?;
            }
            for child in named_children(visit.node) {
                push_visit(
                    &mut builder.context,
                    &mut pending,
                    Visit {
                        node: child,
                        depth: visit.depth.saturating_add(1),
                        ..visit
                    },
                )?;
            }
        }
        Ok(())
    }

    fn add_scope(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        visit: Visit<'_>,
        kind: ScopeKind,
    ) -> Result<(), ExtractError> {
        let id = visit.node.id();
        let nominal = self.declarations.get(&visit.node.start_byte()).cloned();
        let heritage_supported = context.snapshot.language() != SourceLanguage::Python
            || python::heritage_supported(context, visit.node)?;
        reserve_map::<(usize, Scope)>(context)?;
        if let Some(nominal) = &nominal {
            reserve_text(context, nominal.as_str())?;
        }
        self.scopes
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.scopes.insert(
            id,
            Scope {
                parent: (visit.scope != id).then_some(visit.scope),
                kind,
                nominal: nominal.clone(),
                bindings: HashMap::new(),
                heritage_supported,
                fenced: false,
            },
        );
        if let Some(nominal) = nominal {
            reserve_map::<(String, usize)>(context)?;
            reserve_text(context, nominal.as_str())?;
            self.class_scopes
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            self.class_scopes
                .insert(context.copy_text(nominal.as_str())?, id);
            if let Some(name) = visit.node.child_by_field_name("name") {
                self.bind(
                    context,
                    Bind {
                        scope: visit.scope,
                        name,
                        kind: BindingType::Nominal(nominal),
                        start: if context.snapshot.language() == SourceLanguage::Python
                            || self
                                .scopes
                                .get(&visit.scope)
                                .is_some_and(|scope| scope.kind != ScopeKind::Module)
                        {
                            visit.node.end_byte()
                        } else {
                            0
                        },
                    },
                )?;
            }
        }
        Ok(())
    }

    fn bind(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        input: Bind<'_>,
    ) -> Result<(), ExtractError> {
        let name = node_text(context, input.name);
        self.bind_name(
            context,
            NamedBind {
                scope: input.scope,
                name: name.trim(),
                kind: input.kind,
                start: input.start,
            },
        )
    }

    fn bind_name(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        input: NamedBind<'_>,
    ) -> Result<(), ExtractError> {
        let name = input.name;
        if !identifier(name) {
            return Ok(());
        }
        let scope = self
            .scopes
            .get_mut(&input.scope)
            .ok_or(ExtractError::InvalidSpan)?;
        if let Some(owner) = scope
            .nominal
            .as_ref()
            .filter(|_| scope.kind == ScopeKind::Class)
            && (!matches!(input.kind, BindingType::Method) || scope.bindings.contains_key(name))
        {
            record_non_method(
                context,
                &mut self.non_methods,
                (Some(owner), Some(name), false),
            )?;
        }
        if let Some(existing) = scope.bindings.get_mut(name) {
            existing.kind = BindingType::Unknown;
            return Ok(());
        }
        reserve_map::<(String, Binding)>(context)?;
        reserve_text(context, name)?;
        if let BindingType::Nominal(id) | BindingType::Receiver(id) = &input.kind {
            reserve_text(context, id.as_str())?;
        }
        scope
            .bindings
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        scope.bindings.insert(
            context.copy_text(name)?,
            Binding {
                kind: input.kind,
                start: input.start,
            },
        );
        Ok(())
    }

    fn fence(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        scope: usize,
    ) -> Result<(), ExtractError> {
        let scope = self
            .scopes
            .get_mut(&scope)
            .ok_or(ExtractError::InvalidSpan)?;
        if scope.fenced {
            return Ok(());
        }
        scope.fenced = true;
        if let Some(owner) = scope
            .nominal
            .as_ref()
            .filter(|_| scope.kind == ScopeKind::Class)
        {
            record_non_method(context, &mut self.non_methods, (Some(owner), None, false))?;
        }
        Ok(())
    }

    fn assignment_member(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        parts: MemberParts<'tree>,
    ) -> Result<(), ExtractError> {
        let (Some(receiver), Some(member)) = (parts.receiver, parts.member) else {
            return Ok(());
        };
        context.ensure_active()?;
        context.budget.reserve_working_bytes(
            u64::try_from(size_of::<MemberSite<'_>>())
                .map_err(|_| ExtractError::OutputLimit)?
                .saturating_mul(2),
        )?;
        self.assignments
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.assignments.push(MemberSite {
            receiver,
            member,
            scope: parts.visit.scope,
            start: member.start_byte(),
            end: member.end_byte(),
            literal: false,
        });
        Ok(())
    }

    fn fence_assignments(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
    ) -> Result<(), ExtractError> {
        for site in std::mem::take(&mut self.assignments) {
            context.ensure_active()?;
            let nominal = self.types().receiver_type(context, site)?;
            let owner = nominal
                .as_deref()
                .and_then(|name| name.strip_prefix('@'))
                .and_then(|id| self.class_scopes.get(id))
                .and_then(|scope| self.scopes.get(scope))
                .and_then(|scope| scope.nominal.as_ref());
            let name = node_text(context, site.member);
            if identifier(name) {
                record_non_method(context, &mut self.non_methods, (owner, Some(name), true))?;
            }
        }
        Ok(())
    }

    fn member(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        site: MemberSite<'tree>,
    ) -> Result<(), ExtractError> {
        reserve_map::<((usize, usize), MemberSite<'_>)>(context)?;
        self.sites
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.sites.insert((site.start, site.end), site);
        Ok(())
    }

    fn member_sites(
        &mut self,
        context: &mut ExtractionContext<'_, '_>,
        parts: MemberParts<'tree>,
    ) -> Result<(), ExtractError> {
        let (Some(receiver), Some(member)) = (parts.receiver, parts.member) else {
            return Ok(());
        };
        for node in [parts.visit.node, member] {
            context.ensure_active()?;
            self.member(
                context,
                MemberSite {
                    receiver,
                    member,
                    scope: parts.visit.scope,
                    start: node.start_byte(),
                    end: node.end_byte(),
                    literal: false,
                },
            )?;
        }
        Ok(())
    }
}

/// Immutable receiver-type queries over the scopes populated by the syntax collector.
struct ReceiverTypes<'index> {
    scopes: &'index HashMap<usize, Scope>,
    class_scopes: &'index HashMap<String, usize>,
}

impl ReceiverTypes<'_> {
    fn find_binding(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        query: TypeQuery<'_>,
    ) -> Result<Option<&Binding>, ExtractError> {
        let mut scope = query.scope;
        for _ in 0..=super::MAX_AST_DEPTH {
            context.ensure_active()?;
            let Some(entry) = self.scopes.get(&scope) else {
                return Ok(None);
            };
            if entry.fenced || entry.kind == ScopeKind::Barrier {
                return Ok(Some(&UNKNOWN_BINDING));
            }
            if entry.kind == ScopeKind::Opaque {
                return Ok(None);
            }
            if (entry.kind != ScopeKind::Class || query.class_bindings && scope == query.scope)
                && let Some(binding) = entry.bindings.get(query.name)
            {
                return Ok(Some(binding));
            }
            let Some(parent) = entry.parent else {
                return Ok(None);
            };
            scope = parent;
        }
        Ok(None)
    }

    fn receiver_type(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        site: MemberSite<'_>,
    ) -> Result<Option<String>, ExtractError> {
        if site.receiver.kind() == "this" {
            return self.this_type(context, site.scope);
        }
        if site.literal {
            let name = node_text(context, site.receiver);
            return self.explicit_type(
                context,
                TypeQuery {
                    name,
                    scope: site.scope,
                    class_bindings: false,
                    position: None,
                },
            );
        }
        self.expression_type(context, site, 0)
    }

    fn this_type(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        mut scope: usize,
    ) -> Result<Option<String>, ExtractError> {
        for _ in 0..=super::MAX_AST_DEPTH {
            context.ensure_active()?;
            let Some(entry) = self.scopes.get(&scope) else {
                return Ok(None);
            };
            if entry.fenced || entry.kind == ScopeKind::Barrier {
                return Ok(Some("?".into()));
            }
            if entry.kind == ScopeKind::Class {
                return match entry.nominal.as_ref() {
                    Some(nominal) => nominal_marker(context, nominal).map(Some),
                    None => Ok(Some("?".into())),
                };
            }
            let Some(parent) = entry.parent else {
                return Ok(Some("?".into()));
            };
            scope = parent;
        }
        Ok(Some("?".into()))
    }

    fn expression_type(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        site: MemberSite<'_>,
        depth: usize,
    ) -> Result<Option<String>, ExtractError> {
        if depth > MAX_RECEIVER_FIELDS {
            return Ok(Some("?".into()));
        }
        if site.receiver.kind() == "identifier" {
            let name = node_text(context, site.receiver);
            let Some(binding) = self.find_binding(
                context,
                TypeQuery {
                    scope: site.scope,
                    name,
                    class_bindings: false,
                    position: None,
                },
            )?
            else {
                return Ok(None);
            };
            if binding.start > site.start {
                return Ok(Some("?".into()));
            }
            return self.bound_type(context, binding);
        }
        let (Some(receiver), Some(member)) = (
            site.receiver
                .child_by_field_name("object")
                .or_else(|| site.receiver.child_by_field_name("operand")),
            site.receiver
                .child_by_field_name("attribute")
                .or_else(|| site.receiver.child_by_field_name("field")),
        ) else {
            return Ok(None);
        };
        let parent_type = self.expression_type(
            context,
            MemberSite {
                receiver,
                member,
                ..site
            },
            depth.saturating_add(1),
        )?;
        let Some(class) = parent_type
            .as_deref()
            .and_then(|name| name.strip_prefix('@'))
            .and_then(|id| self.class_scopes.get(id))
        else {
            return Ok(parent_type.map(|_| "?".into()));
        };
        let Some(binding) = self
            .scopes
            .get(class)
            .and_then(|scope| scope.bindings.get(context.text(member)))
        else {
            return Ok(Some("?".into()));
        };
        self.bound_type(context, binding)
    }

    fn bound_type(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        binding: &Binding,
    ) -> Result<Option<String>, ExtractError> {
        match &binding.kind {
            BindingType::Import | BindingType::Nominal(_) => Ok(None),
            BindingType::Receiver(id) => nominal_marker(context, id).map(Some),
            BindingType::Explicit {
                name,
                type_scope,
                position,
            } => self.explicit_type(
                context,
                TypeQuery {
                    name,
                    scope: *type_scope,
                    class_bindings: false,
                    position: *position,
                },
            ),
            BindingType::Unknown | BindingType::Method => Ok(Some("?".into())),
        }
    }

    fn explicit_type(
        &self,
        context: &mut ExtractionContext<'_, '_>,
        mut query: TypeQuery<'_>,
    ) -> Result<Option<String>, ExtractError> {
        let name = query.name;
        query.class_bindings = context.snapshot.language() == SourceLanguage::Python
            && self
                .scopes
                .get(&query.scope)
                .is_some_and(|scope| scope.kind == ScopeKind::Class);
        let Some(head) = name.split('.').next().filter(|_| type_path(name)) else {
            return Ok(Some("?".into()));
        };
        let binding = self.find_binding(
            context,
            TypeQuery {
                name: head,
                ..query
            },
        )?;
        if binding.is_some_and(|binding| {
            query
                .position
                .is_some_and(|position| binding.start > position)
        }) {
            return Ok(Some("?".into()));
        }
        match binding.map(|binding| &binding.kind) {
            Some(BindingType::Nominal(id)) if head == name => nominal_marker(context, id).map(Some),
            Some(BindingType::Import) => {
                prefixed_type(context, (EXPLICIT_RECEIVER_IMPORT_PREFIX, name)).map(Some)
            }
            _ => Ok(Some("?".into())),
        }
    }
}

struct Bind<'tree> {
    scope: usize,
    name: Node<'tree>,
    kind: BindingType,
    start: usize,
}

struct NamedBind<'name> {
    scope: usize,
    name: &'name str,
    kind: BindingType,
    start: usize,
}

#[derive(Clone, Copy)]
struct TypeQuery<'name> {
    name: &'name str,
    scope: usize,
    class_bindings: bool,
    position: Option<usize>,
}

fn nominal_marker(
    context: &mut ExtractionContext<'_, '_>,
    id: &SymbolId,
) -> Result<String, ExtractError> {
    prefixed_type(context, ("@", id.as_str()))
}

pub(super) fn record_non_method(
    context: &mut ExtractionContext<'_, '_>,
    facts: &mut Vec<ExtractedReceiverBinding>,
    binding: (Option<&SymbolId>, Option<&str>, bool),
) -> Result<(), ExtractError> {
    context.ensure_active()?;
    context.budget.reserve_fact(
        u64::try_from(size_of::<ExtractedReceiverBinding>())
            .map_err(|_| ExtractError::OutputLimit)?
            .saturating_mul(2)
            .saturating_add(
                u64::try_from(
                    binding.0.map_or(0, |id| id.as_str().len()) + binding.1.map_or(0, str::len),
                )
                .map_err(|_| ExtractError::OutputLimit)?,
            ),
        binding.1,
    )?;
    facts
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    facts.push(ExtractedReceiverBinding {
        class_id: binding.0.cloned(),
        name: binding.1.map(|name| context.copy_text(name)).transpose()?,
        assigned: binding.2,
    });
    Ok(())
}

fn member_marker(
    context: &mut ExtractionContext<'_, '_>,
    parts: (&str, &str),
) -> Result<String, ExtractError> {
    let length = EXPLICIT_RECEIVER_RESOLUTION_PREFIX
        .len()
        .saturating_add(parts.0.len())
        .saturating_add(1)
        .saturating_add(parts.1.len());
    context
        .budget
        .reserve_working_bytes(u64::try_from(length).map_err(|_| ExtractError::OutputLimit)?)?;
    let mut marker = String::new();
    marker
        .try_reserve_exact(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    marker.push_str(EXPLICIT_RECEIVER_RESOLUTION_PREFIX);
    marker.push_str(parts.0);
    marker.push('#');
    marker.push_str(parts.1);
    Ok(marker)
}

fn prefixed_type(
    context: &mut ExtractionContext<'_, '_>,
    parts: (&str, &str),
) -> Result<String, ExtractError> {
    let length = parts.0.len().saturating_add(parts.1.len());
    context
        .budget
        .reserve_working_bytes(u64::try_from(length).map_err(|_| ExtractError::OutputLimit)?)?;
    let mut marker = String::new();
    marker
        .try_reserve_exact(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    marker.push_str(parts.0);
    marker.push_str(parts.1);
    Ok(marker)
}

fn explicit(
    context: &mut ExtractionContext<'_, '_>,
    name: Option<Node<'_>>,
    scope: usize,
) -> Result<BindingType, ExtractError> {
    let position = (context.snapshot.language() == SourceLanguage::Go)
        .then(|| name.map(|node| node.start_byte()))
        .flatten();
    let Some(name) = name
        .map(|name| node_text(context, name))
        .filter(|name| type_path(name))
    else {
        return Ok(BindingType::Unknown);
    };
    reserve_text(context, name)?;
    Ok(BindingType::Explicit {
        name: context.copy_text(name)?,
        type_scope: scope,
        position,
    })
}

fn initializer(
    context: &mut ExtractionContext<'_, '_>,
    name: Option<Node<'_>>,
    scope: usize,
) -> Result<BindingType, ExtractError> {
    let mut kind = explicit(context, name, scope)?;
    if let BindingType::Explicit { position, .. } = &mut kind {
        *position = name.map(|node| node.start_byte());
    }
    Ok(kind)
}

fn identifier(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES
        && name
            .as_bytes()
            .first()
            .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn type_path(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES && name.split('.').all(identifier)
}

fn supported(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::Python
            | SourceLanguage::Go
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    )
}

fn reserve_map<T>(context: &mut ExtractionContext<'_, '_>) -> Result<(), ExtractError> {
    context.budget.reserve_working_bytes(
        MAP_ALLOWANCE
            .saturating_add(u64::try_from(size_of::<T>()).map_err(|_| ExtractError::OutputLimit)?),
    )
}

fn reserve_text(context: &mut ExtractionContext<'_, '_>, value: &str) -> Result<(), ExtractError> {
    context
        .budget
        .reserve_working_bytes(u64::try_from(value.len()).map_err(|_| ExtractError::OutputLimit)?)
}

fn node_text<'source>(context: &ExtractionContext<'source, '_>, node: Node<'_>) -> &'source str {
    context
        .source()
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
        .trim()
}

fn push_visit<'tree>(
    context: &mut ExtractionContext<'_, '_>,
    pending: &mut Vec<Visit<'tree>>,
    visit: Visit<'tree>,
) -> Result<(), ExtractError> {
    context.ensure_active()?;
    context.budget.reserve_working_bytes(
        u64::try_from(size_of::<Visit<'_>>())
            .map_err(|_| ExtractError::OutputLimit)?
            .saturating_mul(2),
    )?;
    pending
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    pending.push(visit);
    Ok(())
}
