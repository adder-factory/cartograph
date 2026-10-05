//! Pascal and Delphi structural extraction.
//!
//! A unit, program, or library becomes a `Module` that does not qualify its
//! declarations (v1 kept classes top-level). Types, members, routines,
//! constants, and variables are emitted where they are declared. A routine's
//! implementation never becomes a second symbol: the per-file [`index`] pairs
//! it with its declaration up front, so the declared symbol spans and carries
//! the body and owns the body's calls. `uses` clauses emit one import per unit
//! plus a unit-named namespace binding, so `Unit.Member` designators resolve
//! through the unit. Calls the file binds by Pascal scope (nested routines,
//! implicit `Self`, in-file types, and the file's own unit-level routines,
//! matched case-insensitively) resolve only within the file; the rest keep
//! the ordinary project resolution path.

mod body;
mod facts;
mod index;
mod names;
mod routines;

use std::collections::BTreeMap;

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind, Visibility};
use tree_sitter::Node;

use self::{
    body::{same_file_target, self_scope},
    facts::{
        GenericScope, TypeNameOwner, TypeReferences, binding_value, collect_type_names,
        declared_name, emit_heritage, emit_type_names, emit_type_references, emit_uses,
        module_name, owned_identifier, pending, section_visibility,
    },
    index::{FileIndex, MemberBinding, MemberQuery, MemberUse, type_body},
    names::generic_parameters,
    routines::{ImplementationVisit, LexicalScope, RoutineContext},
};
use super::{
    AstVisitBudget, ExtractionBuilder, MAX_AST_DEPTH, PendingSymbol, safe_assignment_signature,
    syntax::{has_child_kind, named_children, span_for},
};
use crate::{ExtractError, ExtractedReference, SymbolExportFlags};

/// Most names one grouped declaration (`A, B, C: T`) shares its structural
/// root among; each rescans the declaration, so the bound keeps that linear.
const SHARED_STRUCTURAL_ROOT_NAMES: usize = 8;

/// Consume the whole Pascal tree from its root; every other node is reached
/// through this family's own bounded traversal.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    if node.kind() != "root" {
        return Ok(false);
    }
    let index = FileIndex::build(node, builder.maximum_ast_depth, &mut builder.context)?;
    let mut walk = PascalWalk {
        index,
        routines: BTreeMap::new(),
        types: BTreeMap::new(),
        scopes: Vec::new(),
        generics: GenericScope::default(),
        budget: AstVisitBudget::default(),
    };
    let scope = DeclarationScope::top_level(false, depth);
    for child in named_children(node) {
        if matches!(child.kind(), "unit" | "program" | "library") {
            walk.emit_module(builder, child, depth)?;
        } else {
            walk.emit_definition(builder, child, &scope)?;
        }
    }
    Ok(true)
}

/// Per-file emission state.
struct PascalWalk<'tree> {
    index: FileIndex<'tree>,
    /// Paired routine declarations by start byte, awaiting their bodies.
    routines: BTreeMap<usize, RoutineContext>,
    /// Emitted in-file types by lower-case dotted key.
    types: BTreeMap<String, OwnerFrame>,
    /// The lexical scopes of the routines being emitted, outermost first.
    scopes: Vec<LexicalScope>,
    /// Generic parameters of the enclosing generic types and routines, which
    /// name no project type.
    generics: GenericScope,
    budget: AstVisitBudget<MAX_AST_DEPTH>,
}

/// An emitted symbol that can own further declarations, with the canonical
/// qualified name its members are emitted under.
#[derive(Clone)]
struct OwnerFrame {
    id: SymbolId,
    qualified: String,
}

/// Where a declaration appears. `type_key` is absent for members of a type
/// too deeply nested to index.
#[derive(Clone)]
struct DeclarationScope {
    exported: bool,
    visibility: Option<Visibility>,
    member: bool,
    type_key: Option<String>,
    depth: usize,
}

impl DeclarationScope {
    /// A unit-, program-, or library-level declaration.
    const fn top_level(exported: bool, depth: usize) -> Self {
        Self {
            exported,
            visibility: None,
            member: false,
            type_key: None,
            depth,
        }
    }

    /// Members after a visibility keyword; only public and published members
    /// of an exported type are visible to other units.
    fn section(&self, section: Node<'_>) -> Self {
        let visibility = section_visibility(section);
        Self {
            exported: self.exported && visibility == Some(Visibility::Public),
            visibility,
            ..self.nested()
        }
    }
}

impl NestedScope for DeclarationScope {
    fn depth_mut(&mut self) -> &mut usize {
        &mut self.depth
    }
}

/// A traversal scope that records its structural nesting depth.
trait NestedScope: Clone {
    /// The depth this scope records.
    fn depth_mut(&mut self) -> &mut usize;

    /// The same scope one structural level deeper.
    fn nested(&self) -> Self {
        let mut nested = self.clone();
        let depth = nested.depth_mut();
        *depth = depth.saturating_add(1);
        nested
    }
}

/// A binding-shaped declaration: constant, variable, or field.
#[derive(Clone, Copy)]
struct BindingDeclaration<'tree> {
    node: Node<'tree>,
    kind: SymbolKind,
    static_member: bool,
}

/// A class-like body to emit.
#[derive(Clone, Copy)]
struct ClassDeclaration<'tree, 'name> {
    declaration: Node<'tree>,
    body: Node<'tree>,
    name: &'name str,
}

/// A `type` declaration that is not class-like: an enumeration or an alias,
/// with the generic parameters in scope for its shape.
#[derive(Clone, Copy)]
struct ValueTypeDeclaration<'tree, 'name> {
    declaration: Node<'tree>,
    shape: Option<Node<'tree>>,
    name: &'name str,
    generics: &'name GenericScope,
}

/// A property accessor (`read X` / `write Y`) to reference.
#[derive(Clone, Copy)]
struct AccessorReference<'tree, 'scope> {
    accessor: Node<'tree>,
    type_key: Option<&'scope str>,
    property: &'scope SymbolId,
}

impl<'tree> PascalWalk<'tree> {
    /// A unit, program, or library: its module symbol, its sections, and the
    /// calls in its main block or initialization/finalization sections.
    fn emit_module(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'tree>,
        depth: usize,
    ) -> Result<(), ExtractError> {
        let unit = node.kind() == "unit";
        let name = module_name(builder, node)?;
        let main_block = named_children(node).find(|child| child.kind() == "block");
        let id = builder.emit_symbol(PendingSymbol {
            body_node: main_block,
            export: SymbolExportFlags::named(unit),
            ..pending(SymbolKind::Module, name.clone(), node)
        })?;
        let module = RoutineContext::new(
            OwnerFrame {
                id,
                qualified: String::new(),
            },
            name,
            depth,
        );
        let inner = depth.saturating_add(1);
        for child in named_children(node) {
            match child.kind() {
                "initialization" | "finalization" | "block" => {
                    self.capture_body(builder, child, &module)?;
                }
                kind => {
                    let exported = unit && kind == "interface";
                    let scope = DeclarationScope::top_level(exported, inner);
                    self.emit_definition(builder, child, &scope)?;
                }
            }
        }
        Ok(())
    }

    /// One declaration-level node, dispatched by kind.
    fn emit_definition(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'tree>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        self.budget.observe(builder, scope.depth)?;
        match node.kind() {
            "declUses" => emit_uses(builder, node),
            "declType" => self.emit_type(builder, node, scope),
            "declConsts" | "declVars" => self.emit_binding_group(builder, node, scope),
            "declField" => self.emit_bindings(
                builder,
                BindingDeclaration {
                    node,
                    kind: SymbolKind::Field,
                    static_member: false,
                },
                scope,
            ),
            "declProc" => self.emit_declared_routine(builder, node, scope),
            "declProp" => self.emit_property(builder, node, scope),
            "defProc" => self.emit_implementation(
                builder,
                ImplementationVisit {
                    node,
                    enclosing: None,
                    depth: scope.depth,
                },
            ),
            "declSection" => {
                let section = scope.section(node);
                self.emit_children(builder, node, &section)
            }
            "declTypes" | "interface" | "implementation" | "declVariant" | "declVariantClause"
            | "ERROR" => self.emit_children(builder, node, scope),
            _ => Ok(()),
        }
    }

    /// Every named child of a container, one level deeper.
    fn emit_children(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'tree>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        let nested = scope.nested();
        for child in named_children(node) {
            self.emit_definition(builder, child, &nested)?;
        }
        Ok(())
    }

    /// A `const`/`var` section: constants, fields, or variables.
    fn emit_binding_group(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        group: Node<'tree>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        let constants = group.kind() == "declConsts";
        let kind = if constants {
            SymbolKind::Constant
        } else if scope.member {
            SymbolKind::Field
        } else {
            SymbolKind::Variable
        };
        let static_member = scope.member && (constants || has_child_kind(group, "kClass"));
        let declarations = named_children(group)
            .filter(|child| matches!(child.kind(), "declConst" | "declVar"))
            .collect::<Vec<_>>();
        for node in declarations {
            self.emit_bindings(
                builder,
                BindingDeclaration {
                    node,
                    kind,
                    static_member,
                },
                scope,
            )?;
        }
        Ok(())
    }

    /// One binding declaration, which may name several symbols (`A, B: T`).
    /// The declared type is scanned once and referenced from every name. A
    /// small group keeps the whole declaration as each name's structural root,
    /// so a changed type changes every digest; a larger group gives each name
    /// its own root (as managed-language declarators do), so the work stays
    /// linear in the declaration's size.
    fn emit_bindings(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        binding: BindingDeclaration<'tree>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        self.budget.observe(builder, scope.depth)?;
        let signature = match binding_value(binding.node) {
            Some(value) if binding.kind != SymbolKind::Field => {
                safe_assignment_signature(builder, value)?
            }
            _ => None,
        };
        let mut cursor = binding.node.walk();
        let names = binding
            .node
            .children_by_field_name("name", &mut cursor)
            .filter(|name| name.kind() == "identifier")
            .collect::<Vec<_>>();
        let declared_types = match binding.node.child_by_field_name("type") {
            Some(declared_type) => collect_type_names(builder, declared_type, &self.generics)?,
            None => Vec::new(),
        };
        let shared_root = names.len() <= SHARED_STRUCTURAL_ROOT_NAMES;
        for name_node in names {
            let Some(name) = owned_identifier(builder, name_node)? else {
                continue;
            };
            let id = builder.emit_symbol(PendingSymbol {
                span_node: name_node,
                structural_node: if shared_root { binding.node } else { name_node },
                signature: signature.clone(),
                export: SymbolExportFlags::named(scope.exported),
                static_member: binding.static_member,
                visibility: scope.visibility,
                ..pending(binding.kind, name, binding.node)
            })?;
            emit_type_names(
                builder,
                &declared_types,
                TypeNameOwner {
                    owner: &id,
                    kind: ReferenceKind::TypeOf,
                },
            )?;
        }
        Ok(())
    }

    /// One `type` declaration: class-like, enumeration, or alias.
    fn emit_type(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        declaration: Node<'tree>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        let Some(name) = declaration
            .child_by_field_name("name")
            .and_then(|name| declared_name(builder, name))
        else {
            return Ok(());
        };
        if let Some(body) = type_body(declaration) {
            return self.emit_class(
                builder,
                ClassDeclaration {
                    declaration,
                    body,
                    name: &name,
                },
                scope,
            );
        }
        if named_children(declaration)
            .any(|child| matches!(child.kind(), "declClass" | "declIntf" | "declHelper"))
        {
            // A forward declaration only announces the definition that follows.
            return Ok(());
        }
        let shape = named_children(declaration).find(|child| child.kind() == "type");
        let mark = self.generics.mark();
        let own = own_generics(builder, declaration)?;
        self.generics.extend(own);
        let result = emit_value_type(
            builder,
            ValueTypeDeclaration {
                declaration,
                shape,
                name: &name,
                generics: &self.generics,
            },
            scope,
        );
        self.generics.truncate(mark);
        result
    }

    /// A class, record, object, interface, or helper with its members.
    fn emit_class(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        class: ClassDeclaration<'tree, '_>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        let kind = if class.body.kind() == "declIntf" {
            SymbolKind::Interface
        } else {
            SymbolKind::Class
        };
        let id = builder.emit_symbol(PendingSymbol {
            body_node: Some(class.body),
            export: SymbolExportFlags::named(scope.exported),
            visibility: scope.visibility,
            ..pending(kind, class.name.to_owned(), class.declaration)
        })?;
        let qualified = last_qualified(builder);
        emit_heritage(builder, class.body, &id)?;
        let key = self.type_key(scope, class.name);
        if let Some(key) = &key {
            charge(builder, key)?;
            charge(builder, &qualified)?;
            self.types.insert(
                key.clone(),
                OwnerFrame {
                    id: id.clone(),
                    qualified,
                },
            );
        }
        let members = DeclarationScope {
            exported: scope.exported,
            visibility: None,
            member: true,
            type_key: key,
            depth: scope.depth.saturating_add(1),
        };
        let generics = self.generics.mark();
        let own = own_generics(builder, class.declaration)?;
        self.generics.extend(own);
        builder.owners.push(id);
        builder.qualifiers.push(class.name.to_owned());
        let result = self.emit_children(builder, class.body, &members);
        builder.qualifiers.pop();
        builder.owners.pop();
        self.generics.truncate(generics);
        result
    }

    /// The lookup key of a type the index recorded, so emission and lookup
    /// agree on which (bounded) types are resolution targets.
    fn type_key(&self, scope: &DeclarationScope, name: &str) -> Option<String> {
        if scope.member && scope.type_key.is_none() {
            return None;
        }
        let lower = name.to_ascii_lowercase();
        let key = scope
            .type_key
            .as_ref()
            .map_or(lower.clone(), |outer| format!("{outer}.{lower}"));
        self.index.has_type(&key).then_some(key)
    }

    /// A property with its type and accessor references.
    fn emit_property(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        node: Node<'tree>,
        scope: &DeclarationScope,
    ) -> Result<(), ExtractError> {
        let Some(name) = node
            .child_by_field_name("name")
            .map(|name| owned_identifier(builder, name))
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        let id = builder.emit_symbol(PendingSymbol {
            export: SymbolExportFlags::named(scope.exported),
            static_member: has_child_kind(node, "kClass"),
            visibility: scope.visibility,
            ..pending(SymbolKind::Property, name, node)
        })?;
        if let Some(declared_type) = node.child_by_field_name("type") {
            emit_type_references(
                builder,
                TypeReferences {
                    root: declared_type,
                    owner: &id,
                    kind: ReferenceKind::TypeOf,
                    generics: &self.generics,
                },
            )?;
        }
        for field in ["getter", "setter"] {
            if let Some(accessor) = node.child_by_field_name(field) {
                self.emit_accessor(
                    builder,
                    AccessorReference {
                        accessor,
                        type_key: scope.type_key.as_deref(),
                        property: &id,
                    },
                )?;
            }
        }
        Ok(())
    }

    /// `read FValue` references the member it names. Only a member of the
    /// declaring type (or its in-file ancestry) is a valid target, so the
    /// reference never widens to a project-wide lookup.
    fn emit_accessor(
        &self,
        builder: &mut ExtractionBuilder<'_, '_>,
        reference: AccessorReference<'_, '_>,
    ) -> Result<(), ExtractError> {
        let Some(name) = owned_identifier(builder, reference.accessor)? else {
            return Ok(());
        };
        let binding = reference.type_key.map(|type_key| {
            self.index.member_binding(MemberQuery {
                type_key,
                member: &name,
                usage: MemberUse::Accessor,
            })
        });
        let resolution_name = match binding {
            Some(MemberBinding::Exact(qualified)) => self_scope(&same_file_target(&qualified)),
            _ => self_scope(&name),
        };
        builder.emit_reference(ExtractedReference {
            owner: Some(reference.property.clone()),
            name,
            resolution_name: Some(resolution_name),
            kind: ReferenceKind::References,
            span: span_for(reference.accessor)?,
        })
    }
}

/// The canonical (possibly shortened) qualified name of the symbol the
/// builder emitted last.
fn last_qualified(builder: &ExtractionBuilder<'_, '_>) -> String {
    builder
        .facts
        .symbols
        .last()
        .map(|symbol| symbol.qualified_name.clone())
        .unwrap_or_default()
}

/// Charge a string the walk retains beyond the emitted facts to the
/// extraction's working-memory budget.
fn charge(builder: &mut ExtractionBuilder<'_, '_>, value: &str) -> Result<(), ExtractError> {
    builder.context.budget.reserve_additional_string(value)
}

/// The generic parameters a type or routine declaration (or implementation
/// header) introduces through its `name`. Each is charged twice: the
/// generic scope's stack and its index both retain a copy.
fn own_generics(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: Node<'_>,
) -> Result<Vec<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let parameters = declaration
        .child_by_field_name("name")
        .map(|name| generic_parameters(source, name))
        .unwrap_or_default();
    for parameter in &parameters {
        charge(builder, parameter)?;
        charge(builder, parameter)?;
    }
    Ok(parameters)
}

/// An enumeration with its members, or an alias with references to the
/// named types it is built from.
fn emit_value_type(
    builder: &mut ExtractionBuilder<'_, '_>,
    declaration: ValueTypeDeclaration<'_, '_>,
    scope: &DeclarationScope,
) -> Result<(), ExtractError> {
    let enumeration = declaration
        .shape
        .and_then(|shape| named_children(shape).find(|child| child.kind() == "declEnum"));
    let kind = if enumeration.is_some() {
        SymbolKind::Enum
    } else {
        SymbolKind::TypeAlias
    };
    let id = builder.emit_symbol(PendingSymbol {
        export: SymbolExportFlags::named(scope.exported),
        visibility: scope.visibility,
        ..pending(kind, declaration.name.to_owned(), declaration.declaration)
    })?;
    let Some(enumeration) = enumeration else {
        return match declaration.shape {
            Some(shape) => emit_type_references(
                builder,
                TypeReferences {
                    root: shape,
                    owner: &id,
                    kind: ReferenceKind::TypeOf,
                    generics: declaration.generics,
                },
            ),
            None => Ok(()),
        };
    };
    builder.owners.push(id);
    builder.qualifiers.push(declaration.name.to_owned());
    let result = emit_enum_members(builder, enumeration, scope);
    builder.qualifiers.pop();
    builder.owners.pop();
    result
}

/// The members of an enumeration, contained by it.
fn emit_enum_members(
    builder: &mut ExtractionBuilder<'_, '_>,
    enumeration: Node<'_>,
    scope: &DeclarationScope,
) -> Result<(), ExtractError> {
    for value in named_children(enumeration).filter(|child| child.kind() == "declEnumValue") {
        let Some(name) = value
            .child_by_field_name("name")
            .map(|name| owned_identifier(builder, name))
            .transpose()?
            .flatten()
        else {
            continue;
        };
        builder.emit_symbol(PendingSymbol {
            export: SymbolExportFlags::named(scope.exported),
            ..pending(SymbolKind::EnumMember, name, value)
        })?;
    }
    Ok(())
}
