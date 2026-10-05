//! Pascal routine declarations, implementations, nested routines, and the
//! lexical scopes their bodies are resolved in.
//!
//! Each routine being emitted pushes one [`LexicalScope`] holding only the
//! names it declares itself, so lookups walk the stack innermost-first and the
//! retained state grows with the declarations in the file, never with their
//! nesting depth.

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{SymbolId, SymbolKind};
use tree_sitter::Node;

use super::{
    DeclarationScope, OwnerFrame, PascalWalk,
    body::{BodyCapture, CallScope, capture_calls},
    charge,
    facts::{
        RoutineTypes, declared_name, emit_routine_types, local_declaration_name, local_overloads,
        pending, routine_locals, routine_signature,
    },
    last_qualified,
    names::name_parts,
    own_generics,
};
use crate::{
    ExtractError, SymbolExportFlags,
    walk::{ExtractionBuilder, PendingSymbol, syntax::has_child_kind},
};

/// The names one routine declares, consulted innermost-first: values
/// (parameters, locals, constants, local types, and declared local
/// overloads), routines (nested routines, and the routine itself unless it
/// is a class member) with the qualified name each was emitted under, and
/// local `forward` declarations whose bodies have not been emitted yet
/// (their target is not yet known). Local routines become visible at their
/// declaration; `overloaded` only records which names will be ambiguous
/// once declared.
#[derive(Default)]
pub(super) struct LexicalScope {
    pub(super) values: BTreeSet<String>,
    pub(super) routines: BTreeMap<String, String>,
    pub(super) forward: BTreeSet<String>,
    overloaded: BTreeSet<String>,
}

impl LexicalScope {
    /// Make a local routine name visible: an overload is ambiguous, a
    /// forward declaration has no known target yet, and a body binds exactly
    /// (unless an earlier body already claimed the name).
    fn declare_local(&mut self, name: String, target: Option<String>) {
        if self.overloaded.contains(&name) {
            self.values.insert(name);
            return;
        }
        let Some(target) = target else {
            self.forward.insert(name);
            return;
        };
        self.forward.remove(&name);
        if self.routines.remove(&name).is_some() {
            self.values.insert(name);
        } else {
            self.routines.insert(name, target);
        }
    }
}

/// An emitted routine (or program block) and the facts its body needs.
#[derive(Clone)]
pub(super) struct RoutineContext {
    pub(super) frame: OwnerFrame,
    name: String,
    class_key: Option<String>,
    /// A method's own name is a class member (possibly overloaded), so the
    /// member lookup binds it instead of the routine's own scope.
    method: bool,
    depth: usize,
}

impl RoutineContext {
    /// A routine (or program block) emitted as `frame`.
    pub(super) const fn new(frame: OwnerFrame, name: String, depth: usize) -> Self {
        Self {
            frame,
            name,
            class_key: None,
            method: false,
            depth,
        }
    }

    /// The in-file class whose members the body sees through `Self`.
    fn within_class(mut self, class_key: Option<String>, method: bool) -> Self {
        self.class_key = class_key;
        self.method = method;
        self
    }
}

/// One implementation to emit, nested in `enclosing` when it is a local routine.
#[derive(Clone, Copy)]
pub(super) struct ImplementationVisit<'tree, 'scope> {
    pub(super) node: Node<'tree>,
    pub(super) enclosing: Option<&'scope RoutineContext>,
    pub(super) depth: usize,
}

/// Owner and qualifier stacks saved while an undeclared routine is emitted.
struct SavedFrame {
    owners: Vec<SymbolId>,
    qualifiers: Vec<String>,
}

impl<'tree> PascalWalk<'tree> {
    /// A routine declared in a class body, a unit `interface`, or a `forward`
    /// declaration. A paired routine spans its implementation: that is where
    /// its code, history, and line-scoped evidence live. The declaration still
    /// supplies the documentation, signature, and visibility.
    pub(super) fn emit_declared_routine(
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
        let implementation = self.index.implementation_of(declaration);
        let kind = if scope.member {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let id = builder.emit_symbol(PendingSymbol {
            span_node: implementation.unwrap_or(declaration),
            structural_node: implementation.unwrap_or(declaration),
            body_node: implementation.and_then(|body| body.child_by_field_name("body")),
            declaration_only: implementation.is_none(),
            signature: routine_signature(builder, declaration)?,
            export: SymbolExportFlags::named(scope.exported),
            static_member: has_child_kind(declaration, "kClass"),
            visibility: scope.visibility,
            ..pending(kind, name.clone(), declaration)
        })?;
        let qualified = last_qualified(builder);
        self.emit_scoped_routine_types(builder, declaration, &id)?;
        if implementation.is_none() {
            return Ok(());
        }
        for value in [Some(&name), Some(&qualified), scope.type_key.as_ref()]
            .into_iter()
            .flatten()
        {
            charge(builder, value)?;
        }
        self.routines.insert(
            declaration.start_byte(),
            RoutineContext::new(OwnerFrame { id, qualified }, name, scope.depth)
                .within_class(scope.type_key.clone(), scope.member),
        );
        Ok(())
    }

    /// Attach one implementation to its declaration (or emit it as its own
    /// symbol), then emit its nested routines and its body's calls inside
    /// the routine's own lexical scope.
    pub(super) fn emit_implementation(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        visit: ImplementationVisit<'tree, '_>,
    ) -> Result<(), ExtractError> {
        self.budget.observe(builder, visit.depth)?;
        let declaration = visit
            .enclosing
            .is_none()
            .then(|| self.index.declaration_of(visit.node))
            .flatten();
        let declared = declaration
            .and_then(|declaration| self.routines.get(&declaration.start_byte()).cloned());
        let routine = match declared {
            Some(routine) => routine,
            None => match self.emit_undeclared_routine(builder, visit)? {
                Some(routine) => routine,
                None => return Ok(()),
            },
        };
        let routine = RoutineContext {
            depth: visit.depth,
            ..routine
        };
        if visit.enclosing.is_some() {
            self.declare_nested_routine(builder, &routine)?;
        }
        let mut scope = LexicalScope {
            values: routine_locals(builder, visit.node, declaration)?,
            overloaded: local_overloads(builder, visit.node)?,
            ..LexicalScope::default()
        };
        let own = routine.name.to_ascii_lowercase();
        // A method's own name is a member, and an overloaded unit-level
        // routine's own name cannot pick an overload without its arguments.
        let overloaded = match visit.enclosing {
            None => self.index.is_overloaded_routine(&own),
            Some(_) => self
                .scopes
                .last()
                .is_some_and(|enclosing| enclosing.values.contains(&own)),
        };
        if !routine.method && !overloaded {
            charge(builder, &own)?;
            scope.routines.insert(own, routine.frame.qualified.clone());
        }
        let generics = self.generics.mark();
        if let Some(header) = visit.node.child_by_field_name("header") {
            let own = own_generics(builder, header)?;
            self.generics.extend(own);
        }
        self.scopes.push(scope);
        let result = self.emit_routine_contents(builder, visit.node, &routine);
        self.scopes.pop();
        self.generics.truncate(generics);
        result
    }

    /// A nested routine is visible by name in its enclosing routine from its
    /// declaration on (Pascal requires declaration before use).
    fn declare_nested_routine(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        routine: &RoutineContext,
    ) -> Result<(), ExtractError> {
        let name = routine.name.to_ascii_lowercase();
        charge(builder, &name)?;
        if let Some(enclosing) = self.scopes.last_mut() {
            enclosing.declare_local(name, Some(routine.frame.qualified.clone()));
        }
        Ok(())
    }

    /// Local routines in declaration order (a `forward` declaration becomes
    /// visible where it is written, a nested body where it is emitted), then
    /// the body's calls.
    fn emit_routine_contents(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        implementation: Node<'tree>,
        routine: &RoutineContext,
    ) -> Result<(), ExtractError> {
        let mut cursor = implementation.walk();
        let locals = implementation
            .children_by_field_name("local", &mut cursor)
            .filter(|local| matches!(local.kind(), "defProc" | "declProc"))
            .collect::<Vec<_>>();
        for local in locals {
            if local.kind() == "declProc" {
                self.declare_local_forward(builder, local)?;
                continue;
            }
            self.emit_implementation(
                builder,
                ImplementationVisit {
                    node: local,
                    enclosing: Some(routine),
                    depth: routine.depth.saturating_add(1),
                },
            )?;
        }
        match implementation.child_by_field_name("body") {
            Some(body) => self.capture_body(builder, body, routine),
            None => Ok(()),
        }
    }

    /// An implementation with no declaration in this file (a program
    /// routine, an implementation-only helper, or a nested routine) owns its
    /// symbol: a method under its in-file class, otherwise under its written
    /// prefix, or a function nested in its enclosing routine.
    fn emit_undeclared_routine(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        visit: ImplementationVisit<'tree, '_>,
    ) -> Result<Option<RoutineContext>, ExtractError> {
        let Some(header) = visit.node.child_by_field_name("header") else {
            return Ok(None);
        };
        let source = builder.context.snapshot.source();
        let Some(parts) = header
            .child_by_field_name("name")
            .and_then(|name| name_parts(source, name))
        else {
            return Ok(None);
        };
        let Some((name, prefix)) = parts.split_last() else {
            return Ok(None);
        };
        let class_key = (!prefix.is_empty())
            .then(|| prefix.join(".").to_ascii_lowercase())
            .filter(|key| self.index.has_type(key));
        let frame = match visit.enclosing {
            Some(outer) => Some(outer.frame.clone()),
            None => class_key
                .as_ref()
                .and_then(|key| self.types.get(key))
                .cloned(),
        };
        let kind = if prefix.is_empty() {
            SymbolKind::Function
        } else {
            SymbolKind::Method
        };
        let symbol = PendingSymbol {
            body_node: visit.node.child_by_field_name("body"),
            signature: routine_signature(builder, header)?,
            static_member: has_child_kind(header, "kClass"),
            ..pending(kind, builder.context.copy_text(name)?, visit.node)
        };
        let saved = enter_frame(builder, frame.as_ref(), prefix);
        let emitted = builder.emit_symbol(symbol);
        restore_frame(builder, saved);
        let id = emitted?;
        let qualified = last_qualified(builder);
        self.emit_scoped_routine_types(builder, header, &id)?;
        let (class_key, method) = if let Some(outer) = visit.enclosing {
            (outer.class_key.clone(), false)
        } else {
            let method = class_key.is_some();
            (class_key, method)
        };
        Ok(Some(
            RoutineContext::new(
                OwnerFrame { id, qualified },
                (*name).to_owned(),
                visit.depth,
            )
            .within_class(class_key, method),
        ))
    }

    /// Parameter and result type references of a routine header, skipping
    /// the generic parameters of the enclosing types and routines and of the
    /// header itself (`TBox<T>.Get<U>`).
    fn emit_scoped_routine_types(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        routine: Node<'tree>,
        owner: &SymbolId,
    ) -> Result<(), ExtractError> {
        let mark = self.generics.mark();
        let own = own_generics(builder, routine)?;
        self.generics.extend(own);
        let result = emit_routine_types(
            builder,
            RoutineTypes {
                routine,
                owner,
                generics: &self.generics,
            },
        );
        self.generics.truncate(mark);
        result
    }

    /// A local `forward` (or external) declaration names a routine whose
    /// target is not known until its body is emitted.
    fn declare_local_forward(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        declaration: Node<'tree>,
    ) -> Result<(), ExtractError> {
        let Some(name) = local_declaration_name(builder, declaration) else {
            return Ok(());
        };
        charge(builder, &name)?;
        if let Some(scope) = self.scopes.last_mut() {
            scope.declare_local(name, None);
        }
        Ok(())
    }

    /// Scan one body for calls in the scope of `routine`.
    pub(super) fn capture_body(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        body: Node<'tree>,
        routine: &RoutineContext,
    ) -> Result<(), ExtractError> {
        capture_calls(
            builder,
            &mut self.budget,
            &BodyCapture {
                root: body,
                depth: routine.depth.saturating_add(1),
                scope: CallScope {
                    owner: &routine.frame.id,
                    class_key: routine.class_key.as_deref(),
                    scopes: &self.scopes,
                    index: &self.index,
                },
            },
        )
    }
}

/// Swap the builder's owner and qualifier stacks for an undeclared routine:
/// inside its in-file owner when known, otherwise under its written prefix.
fn enter_frame(
    builder: &mut ExtractionBuilder<'_, '_>,
    frame: Option<&OwnerFrame>,
    prefix: &[&str],
) -> SavedFrame {
    let (owners, qualifiers) = match frame {
        Some(frame) => (vec![frame.id.clone()], vec![frame.qualified.clone()]),
        None if prefix.is_empty() => (Vec::new(), Vec::new()),
        None => (Vec::new(), vec![prefix.join("::")]),
    };
    SavedFrame {
        owners: std::mem::replace(&mut builder.owners, owners),
        qualifiers: std::mem::replace(&mut builder.qualifiers, qualifiers),
    }
}

/// Restore the stacks saved by [`enter_frame`].
fn restore_frame(builder: &mut ExtractionBuilder<'_, '_>, saved: SavedFrame) {
    builder.owners = saved.owners;
    builder.qualifiers = saved.qualifiers;
}
