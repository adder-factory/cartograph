use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::{
    ExtractError, ExtractedReference,
    walk::{
        AstVisitBudget, ExtractionBuilder, SingleChildUnwrap,
        javascript_scopes::{NearestBinding, read_binding},
        owner_for_node,
        syntax::{named_children, span_for},
    },
};

const MAX_AST_DEPTH: usize = 256;
const MAX_VALUE_REFERENCES: usize = 8_192;
const VALUE_IDENTIFIER_UNWRAP: SingleChildUnwrap = SingleChildUnwrap::new(
    value_identifier,
    &[
        "jsx_expression",
        "expression",
        "parenthesized_expression",
        "type_assertion",
    ],
);

/// Deepest chain of enclosing symbols walked from a read to the top level;
/// containment never nests deeper than the walker's own depth limit.
const MAX_SCOPE_DEPTH: usize = crate::MAXIMUM_AST_DEPTH;

/// The lexical bindings one scope declares under one name.
enum ScopeBinding {
    Unique(SymbolId),
    Ambiguous,
}

/// Every lexical binding of the file, by name and by the scope (parent
/// symbol, `None` at the top level) that declares it.
struct LexicalTargets {
    by_name: BTreeMap<String, BTreeMap<Option<SymbolId>, ScopeBinding>>,
    parents: BTreeMap<SymbolId, SymbolId>,
    /// The byte range each lexical binding is declared at.
    spans: BTreeMap<SymbolId, (u64, u64)>,
}

impl LexicalTargets {
    fn from_builder(builder: &ExtractionBuilder<'_, '_>) -> Result<Self, ExtractError> {
        // An embedded component script's module scope is its file-level
        // component: declarations it contains are top-level bindings, exactly
        // as in a standalone file.
        let module_owner = builder.embedded.module_owner(&builder.owners);
        let parents = builder
            .facts
            .containments
            .iter()
            .filter(|containment| Some(&containment.parent) != module_owner.as_ref())
            .map(|containment| (containment.child.clone(), containment.parent.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut by_name = BTreeMap::<String, BTreeMap<Option<SymbolId>, ScopeBinding>>::new();
        let mut spans = BTreeMap::new();
        for symbol in &builder.facts.symbols {
            let scope = parents.get(&symbol.id).cloned();
            if !names_a_lexical_binding(symbol.kind, scope.as_ref()) {
                continue;
            }
            spans.insert(
                symbol.id.clone(),
                (symbol.span.start_byte(), symbol.span.end_byte()),
            );
            if let Some(scopes) = by_name.get_mut(&symbol.name) {
                scopes
                    .entry(scope)
                    .and_modify(|binding| *binding = ScopeBinding::Ambiguous)
                    .or_insert_with(|| ScopeBinding::Unique(symbol.id.clone()));
                continue;
            }
            let mut name = String::new();
            name.try_reserve(symbol.name.len())
                .map_err(|_| ExtractError::OutputLimit)?;
            name.push_str(&symbol.name);
            by_name.insert(
                name,
                BTreeMap::from([(scope, ScopeBinding::Unique(symbol.id.clone()))]),
            );
        }
        Ok(Self {
            by_name,
            parents,
            spans,
        })
    }

    /// The binding a bare read of `name` may name, before its scope is known.
    ///
    /// A name bound exactly once in the file names that binding. Otherwise
    /// only a unique top-level binding can be a target — the resolver binds a
    /// bare name to a same-named top-level declaration before it walks
    /// enclosing scopes. Which of the two a read names is then settled by its
    /// nearest enclosing binding (see `ValueScanner::admitted_target`). A
    /// name bound only in several nested scopes
    /// names nothing, which keeps parameter and local reads at one binding
    /// per name, so the per-file value-reference cap stays a ceiling real
    /// files do not reach.
    fn candidate(&self, name: &str) -> Option<ValueTarget<'_>> {
        let scopes = self.by_name.get(name)?;
        if let (1, Some(ScopeBinding::Unique(id))) = (scopes.len(), scopes.values().next()) {
            return Some(ValueTarget::Unique(id));
        }
        match scopes.get(&None) {
            Some(ScopeBinding::Unique(id)) => Some(ValueTarget::TopLevel(id)),
            _ => None,
        }
    }

    /// Whether a binding is declared at the top level of the file.
    fn is_top_level(&self, target: &SymbolId) -> bool {
        !self.parents.contains_key(target)
    }

    /// Whether a binding is declared inside the scope that binds a read, so
    /// the read names that declaration and not another one of the same name
    /// (an outer local shadowed by a `using` resource, a block local read
    /// outside its block).
    fn declared_within(&self, target: &SymbolId, scope: Option<(usize, usize)>) -> bool {
        let Some((start, end)) = scope else {
            return false;
        };
        self.spans
            .get(target)
            .is_some_and(|(declared, declared_end)| {
                u64::try_from(start).is_ok_and(|start| start <= *declared)
                    && u64::try_from(end).is_ok_and(|end| *declared_end <= end)
            })
    }

    /// Whether a binding is in scope for a read owned by `owner`: a top-level
    /// binding always is, a nested one (a parameter, a local, a nested
    /// function) only inside the scope that declares it. Another function's
    /// parameter is never what a bare read names. An unknown or too deep
    /// chain counts as out of scope.
    fn visible_from(&self, target: &SymbolId, owner: Option<&SymbolId>) -> bool {
        let Some(declaring) = self.parents.get(target) else {
            return true;
        };
        let mut scope = owner;
        for _ in 0..=MAX_SCOPE_DEPTH {
            let Some(current) = scope else {
                return false;
            };
            if current == declaring {
                return true;
            }
            scope = self.parents.get(current);
        }
        false
    }
}

/// What a bare read may name.
#[derive(Clone, Copy)]
enum ValueTarget<'targets> {
    /// The only lexical binding of the name in the file.
    Unique(&'targets SymbolId),
    /// The unique top-level binding of a name nested scopes also bind.
    TopLevel(&'targets SymbolId),
}

/// Whether the resolver can bind a bare name to this symbol, declared in
/// `scope`. A member (method, field, property, enum member) of a containing
/// declaration is reached only through a receiver, so it neither becomes a
/// value target nor makes a module binding of the same name ambiguous — the
/// rule the resolver's lexical scope walk applies to bare names. An
/// uncontained member, such as an inline object-literal method
/// (`register({ handler() {} })`), has its bare name as its qualified name,
/// and the resolver's exact-name lookup binds a same-named read to it, so it
/// stays a top-level binding here.
fn names_a_lexical_binding(kind: SymbolKind, scope: Option<&SymbolId>) -> bool {
    match kind {
        SymbolKind::File | SymbolKind::Import => false,
        SymbolKind::Method | SymbolKind::Property | SymbolKind::Field | SymbolKind::EnumMember => {
            scope.is_none()
        }
        _ => true,
    }
}

#[derive(Default)]
struct ScanBudget {
    visits: AstVisitBudget<MAX_AST_DEPTH>,
    references: usize,
    emitted: BTreeSet<(Option<SymbolId>, SymbolId, usize, usize)>,
}

impl ScanBudget {
    fn admit(
        &mut self,
        owner: Option<&SymbolId>,
        target: &SymbolId,
        node: Node<'_>,
    ) -> Result<bool, ExtractError> {
        let key = (
            owner.cloned(),
            target.clone(),
            node.start_byte(),
            node.end_byte(),
        );
        if !self.emitted.insert(key) {
            return Ok(false);
        }
        self.references = self
            .references
            .checked_add(1)
            .ok_or(ExtractError::OutputLimit)?;
        if self.references > MAX_VALUE_REFERENCES {
            Err(ExtractError::OutputLimit)
        } else {
            Ok(true)
        }
    }
}

pub(super) fn enrich(
    builder: &mut ExtractionBuilder<'_, '_>,
    root: Node<'_>,
) -> Result<(), ExtractError> {
    if !matches!(
        builder.context.snapshot.language(),
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    ) {
        return Ok(());
    }
    let targets = LexicalTargets::from_builder(builder)?;
    if targets.by_name.is_empty() {
        return Ok(());
    }
    let existing = super::javascript_reads::existing_reference_spans(builder);
    ValueScanner {
        builder,
        targets: &targets,
        existing: &existing,
        budget: ScanBudget::default(),
    }
    .scan(root, 0)
}

struct ValueScanner<'builder, 'source, 'cancel, 'targets> {
    builder: &'builder mut ExtractionBuilder<'source, 'cancel>,
    targets: &'targets LexicalTargets,
    /// Spans the walker already recorded as reads (constant reads), which this
    /// pass must not record a second time.
    existing: &'targets BTreeSet<(u64, u64)>,
    budget: ScanBudget,
}

impl ValueScanner<'_, '_, '_, '_> {
    fn scan(&mut self, node: Node<'_>, depth: usize) -> Result<(), ExtractError> {
        self.budget.visits.observe(self.builder, depth)?;
        match node.kind() {
            "arguments" | "array" => {
                for candidate in named_children(node) {
                    self.emit_identifier(candidate)?;
                }
            }
            "pair" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.emit_identifier(value)?;
                }
            }
            "shorthand_property_identifier" => {
                self.emit_identifier(node)?;
            }
            "jsx_attribute" => {
                if let Some(value) = node
                    .child_by_field_name("value")
                    .or_else(|| named_children(node).find(|child| child.kind() == "jsx_expression"))
                    && let Some(identifier) =
                        super::unwrap_single_child(value, 0, VALUE_IDENTIFIER_UNWRAP)
                {
                    self.emit_identifier(identifier)?;
                }
            }
            "call_expression" => self.emit_ternary_callee_arms(node)?,
            _ => {}
        }
        for child in named_children(node) {
            self.scan(child, depth.saturating_add(1))?;
        }
        Ok(())
    }

    fn emit_ternary_callee_arms(&mut self, call: Node<'_>) -> Result<(), ExtractError> {
        let Some(function) = call.child_by_field_name("function") else {
            return Ok(());
        };
        let Some(ternary) = unwrap_to_ternary(function, 0) else {
            return Ok(());
        };
        for field in ["consequence", "alternative"] {
            if let Some(arm) = ternary.child_by_field_name(field)
                && let Some(identifier) =
                    super::unwrap_single_child(arm, 0, VALUE_IDENTIFIER_UNWRAP)
            {
                self.emit_identifier(identifier)?;
            }
        }
        Ok(())
    }

    fn emit_identifier(&mut self, node: Node<'_>) -> Result<(), ExtractError> {
        if !value_identifier(node) {
            return Ok(());
        }
        let targets = self.targets;
        let Some(candidate) = targets.candidate(self.builder.context.text(node)) else {
            return Ok(());
        };
        let name = self.builder.context.owned_text(node)?;
        let span = span_for(node)?;
        // A value already recorded at this site is not recorded twice.
        if self
            .existing
            .contains(&(span.start_byte(), span.end_byte()))
        {
            return Ok(());
        }
        let Some((target, owner)) = self.admitted_target(node, candidate)? else {
            return Ok(());
        };
        if owner.as_ref() == Some(target) || !self.budget.admit(owner.as_ref(), target, node)? {
            return Ok(());
        }
        self.builder.emit_reference(ExtractedReference {
            owner,
            name,
            resolution_name: None,
            kind: ReferenceKind::References,
            span,
        })
    }
}

impl<'targets> ValueScanner<'_, '_, '_, 'targets> {
    /// The symbol a read names and the read's owner, settled by the read's
    /// nearest enclosing binding. A read no function scope rebinds names a
    /// top-level symbol (a module-level block's declarations are top-level
    /// symbols too); a read of a parameter or function local names a nested
    /// symbol, and only one declared in the scope that binds the read. A callback,
    /// `catch`, loop, or expression-name binding has no symbol of its own
    /// (`items.map(save => run(save))`), so such a read names nothing; a
    /// local bound by `require` or `import()` names its declarator's symbol
    /// when it has one.
    fn admitted_target(
        &mut self,
        node: Node<'_>,
        candidate: ValueTarget<'targets>,
    ) -> Result<Option<(&'targets SymbolId, Option<SymbolId>)>, ExtractError> {
        let targets = self.targets;
        let (ValueTarget::Unique(target) | ValueTarget::TopLevel(target)) = candidate;
        let top_level = targets.is_top_level(target);
        let binding = read_binding(self.builder, node)?;
        let admissible = match binding.nearest {
            NearestBinding::Module => top_level,
            NearestBinding::ModuleBlock => {
                top_level && targets.declared_within(target, binding.scope)
            }
            NearestBinding::Represented | NearestBinding::Local => {
                !top_level && targets.declared_within(target, binding.scope)
            }
            // A `require`/`import()` declarator that has its own symbol.
            NearestBinding::Imported => targets.declared_within(target, binding.scope),
            NearestBinding::Anonymous | NearestBinding::Unknown => false,
        };
        if !admissible {
            return Ok(None);
        }
        let owner = owner_for_node(self.builder, node);
        if !top_level && !targets.visible_from(target, owner.as_ref()) {
            return Ok(None);
        }
        Ok(Some((target, owner)))
    }
}

fn unwrap_to_ternary(node: Node<'_>, depth: usize) -> Option<Node<'_>> {
    if depth > 8 {
        return None;
    }
    if matches!(node.kind(), "ternary_expression" | "conditional_expression") {
        return Some(node);
    }
    if matches!(node.kind(), "parenthesized_expression" | "expression") {
        let mut children = named_children(node);
        let child = children.next()?;
        if children.next().is_none() {
            return unwrap_to_ternary(child, depth.saturating_add(1));
        }
    }
    None
}

fn value_identifier(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier" | "shorthand_property_identifier" | "shorthand_property_identifier_pattern"
    )
}
