//! Lexical bindings of JavaScript-family scopes.
//!
//! A bare name read inside a function names its nearest enclosing binding: a
//! parameter, a `catch` or declared loop variable, a function or class
//! expression's own name, or a local declaration — `let`/`const`/`using`,
//! nested function, class, enum, and namespace declarations in their block,
//! `var` hoisted to its function or class static block, destructured,
//! imported (`const { x } = require('./m')`), and loop-declared locals
//! included. Reads no enclosing scope binds name a module binding: a
//! top-level declaration or import of the file, or an undeclared global.
//!
//! The first read a syntax tree asks about builds one index of every scope in
//! that tree, in a single bounded pass: each scope's byte range, its enclosing
//! scope, and the names it binds. A read then finds its innermost scope by
//! binary search and walks outward through the scope chain, without
//! re-walking syntax-tree ancestors. A scope whose bindings exceed a scan
//! bound, or a file whose scopes exceed the index bound, binds every name.
//!
//! Hoisted `var`s belong to a function's body, not to its parameter list: a
//! parameter default (`(x = MAX) => { var MAX }`) still reads the outer
//! binding. A function declared in a nested block is also recorded in its
//! function body, the conservative reading of sloppy-mode hoisting.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

mod writes;

use crate::ExtractError;

use super::{
    ExtractionBuilder,
    javascript_bindings::{BindingMatch, scan_bound_names},
    javascript_members, module_system,
    syntax::{descendants_including_root, named_children},
};

/// Most binding-target nodes scanned for one scope's parameters, `catch`,
/// loop, and expression-name bindings. A scope that binds more binds every
/// name.
const MAX_SCOPE_BINDING_NODES: usize = 4 * crate::MAXIMUM_AST_DEPTH;
/// Most binding-target nodes scanned for one scope's local declarations.
const MAX_SCOPE_LOCAL_NODES: usize = 64 * crate::MAXIMUM_AST_DEPTH;
/// Most syntax nodes one file's scope index visits, hoisting scans included;
/// a larger file binds every name in every scope.
const MAX_INDEX_NODES: usize = 4_000_000;
/// Index nodes visited between two cancellation checks.
const INDEX_CANCELLATION_INTERVAL: usize = 256;
/// Most `export`/`declare` wrappers looked through around one declaration.
const MAX_DECLARATION_WRAPPERS: usize = 4;
/// Deepest static member chain (`a.b.c`) followed to its receiver.
const MAX_STATIC_CHAIN_DEPTH: usize = 64;

/// Nodes that open a function scope: its parameters and, for an expression,
/// its own name.
pub(super) const FUNCTION_SCOPE_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "arrow_function",
    "function_expression",
    "generator_function",
];

/// Non-function declarations whose symbol contains the declarations of the
/// blocks inside it (a static block's locals). A namespace is no container:
/// the walker records its members as top-level symbols.
const CONTAINER_SCOPE_KINDS: &[&str] =
    &["class", "class_declaration", "abstract_class_declaration"];

/// Type-level nodes that introduce type parameters but no value bindings:
/// conditional types (`infer X`), mapped types (`[K in ..]`), generic
/// function and constructor types, call and method signatures, interfaces,
/// and type aliases.
const TYPE_SCOPE_KINDS: &[&str] = &[
    "conditional_type",
    "index_signature",
    "function_type",
    "constructor_type",
    "call_signature",
    "construct_signature",
    "method_signature",
    "abstract_method_signature",
    "function_signature",
    "interface_declaration",
    "type_alias_declaration",
];

/// The nearest enclosing binding of the name a bare read names.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NearestBinding {
    /// No enclosing scope binds the name: it names a module binding.
    Module,
    /// A declaration in a block outside every function (`while (..) { const
    /// x = .. }` at module level), which the walker records as a top-level
    /// symbol.
    ModuleBlock,
    /// A parameter that has its own `Parameter` symbol.
    Represented,
    /// A parameter, `catch`, loop, or expression-name binding without a
    /// symbol of its own.
    Anonymous,
    /// A local bound by a `require(..)` or `import(..)` declaration, which
    /// names the imported value rather than a symbol of this file.
    Imported,
    /// A local declaration inside a function (`let`/`const`/`var`, a nested
    /// function or class).
    Local,
    /// A scope too large to scan; any name may be bound there.
    Unknown,
}

/// What a bare read's name is bound to.
#[derive(Clone, Copy)]
pub(super) struct ReadBinding {
    /// The nearest enclosing binding of the name.
    pub(super) nearest: NearestBinding,
    /// Whether the module scope also binds the name (a top-level declaration
    /// or import).
    pub(super) module_bound: bool,
    /// Whether the module scope declares the name (a top-level symbol, which
    /// the resolver prefers to any nested declaration; imports answer only
    /// after the enclosing containers).
    pub(super) module_declared: bool,
    /// Whether a scope beyond the nearest binding declares the name with a
    /// symbol, which the resolver prefers to an import the read names.
    pub(super) outer_declared: bool,
    /// The byte range of the scope that holds the nearest binding, when one
    /// does: a declaration that binds the read lies inside it.
    pub(super) scope: Option<(usize, usize)>,
}

/// The form a binding target introduces. When one scope binds a name in
/// several forms, the greatest decides: a binding without a symbol shadows
/// any symbol, and a parameter outranks a local declaration.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    /// A declaration the walker records as a symbol.
    Local,
    /// A `require(..)`/`import(..)` declarator, recorded as a symbol at
    /// module level and as an import binding.
    Imported,
    /// An `import` statement or import alias binding, which has no value
    /// symbol of its own.
    Import,
    Represented,
    Anonymous,
}

/// The names one scope binds.
#[derive(Default)]
struct ScopeNames<'source> {
    names: BTreeMap<&'source str, Form>,
    /// The type parameters the scope's declaration introduces (`<T>`).
    type_parameters: Vec<&'source str>,
    /// The types the scope's statements declare: type aliases, interfaces,
    /// classes, enums, and import aliases.
    types: Vec<&'source str>,
    /// Any name may be bound here: a binding target exceeded its scan bound,
    /// or a `with` body resolves names against an object first.
    overflowed: bool,
}

/// One scope of the file: its byte range, its enclosing scope, and its names.
struct IndexedScope<'source> {
    start: usize,
    end: usize,
    parent: Option<usize>,
    /// Where the scope sits.
    level: ScopeLevel,
    /// Whether the scope's declaration holds the symbols of the blocks inside
    /// it (a function or class).
    container: bool,
    names: ScopeNames<'source>,
}

/// Where a scope sits relative to the module and its functions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeLevel {
    /// The module (`program`) scope itself.
    Module,
    /// A block or loop scope outside every function.
    ModuleBlock,
    /// A function scope, or a scope inside a function or static block.
    Function,
}

/// Every scope of one file, in source (pre-)order.
#[derive(Default)]
struct ScopeIndex<'source> {
    writes: writes::Writes<'source>,
    scopes: Vec<IndexedScope<'source>>,
    /// Values and types declared in blocks, by the function or class that
    /// contains the block (`None` outside all of them). The
    /// walker records a block's declarations as members of that container
    /// (or as top-level symbols), so the resolver may bind a same-named read
    /// outside the block to them although they are not visible there.
    block_declarations: BTreeMap<Option<usize>, BlockDeclarations<'source>>,
    /// The file exceeded the index bound; every name counts as bound.
    overflowed: bool,
}

/// What a read's scope chain holds for its name.
#[derive(Default)]
struct ScopeChain {
    /// The nearest binding, its scope's byte range, and its scope's level.
    nearest: Option<(NearestBinding, (usize, usize), ScopeLevel)>,
    /// The module scope's index.
    module: Option<usize>,
    /// A container before the nearest binding has a block declaring the
    /// name, which the resolver's container walk would find first.
    hidden_before_nearest: bool,
    /// A scope beyond the nearest binding declares the name with a symbol,
    /// which the resolver's container walk prefers to any import.
    outer_declared: bool,
}

/// The names the blocks of one function (or of the module) declare.
#[derive(Default)]
struct BlockDeclarations<'source> {
    values: BTreeSet<&'source str>,
    types: BTreeSet<&'source str>,
}

/// Per-tree scope index, reset before a program or template expression is walked.
#[derive(Default)]
pub(super) struct LexicalScopes<'source> {
    index: Option<ScopeIndex<'source>>,
}

/// What the name the identifier `node` reads is bound to.
pub(super) fn read_binding(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<ReadBinding, ExtractError> {
    let snapshot = builder.context.snapshot;
    let Some(name) = snapshot.source().get(node.start_byte()..node.end_byte()) else {
        return Ok(ReadBinding::UNKNOWN);
    };
    Ok(scope_index(builder, node)?.map_or(ReadBinding::UNKNOWN, |index| index.binding(node, name)))
}

pub(super) fn constructor_binding_proven(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let snapshot = builder.context.snapshot;
    let Some(name) = snapshot.source().get(node.byte_range()) else {
        return Ok(false);
    };
    Ok(scope_index(builder, node)?
        .is_some_and(|index| !index.overflowed && index.writes.proven(name)))
}

/// Whether a bare type name inside a body resolves, by name, to the type it
/// names. A type parameter an enclosing declaration introduces
/// (`<Payload>(x: Payload) => ..`) has no declaration to resolve to, and a
/// type a nearer block declares (`type Payload = number` inside a function)
/// loses to a module type of the same name, which the resolver prefers.
pub(super) fn type_name_resolves(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let snapshot = builder.context.snapshot;
    let Some(name) = snapshot.source().get(node.start_byte()..node.end_byte()) else {
        return Ok(false);
    };
    Ok(scope_index(builder, node)?.is_some_and(|index| index.type_resolves(node, name)))
}

/// The file's scope index, built on first use from the tree containing
/// `node`.
fn scope_index<'builder, 'source>(
    builder: &'builder mut ExtractionBuilder<'source, '_>,
    node: Node<'_>,
) -> Result<Option<&'builder ScopeIndex<'source>>, ExtractError> {
    if builder.javascript.scopes.index.is_none() {
        let index = build_index(builder, tree_root(node))?;
        builder.javascript.scopes.index = Some(index);
    }
    Ok(builder.javascript.scopes.index.as_ref())
}

impl ReadBinding {
    /// A read whose binding could not be determined.
    const UNKNOWN: Self = Self {
        nearest: NearestBinding::Unknown,
        module_bound: true,
        module_declared: true,
        outer_declared: true,
        scope: None,
    };

    /// Whether a reference recorded under the read's bare name resolves to
    /// the binding the read names. The resolver binds a bare name to a
    /// same-file top-level declaration first, then to a declaration of an
    /// enclosing container, and only then to an import. So a read of a local
    /// declaration qualifies when no top-level declaration shares its name,
    /// and a read of a local bound by `require`/`import()` when no module
    /// binding (both imports would answer) and no enclosing declaration (it
    /// would answer first) does. A read of a
    /// parameter, a callback, `catch`, or loop binding, or of a scope too
    /// large to scan, never does.
    pub(super) const fn resolves_by_name(self) -> bool {
        match self.nearest {
            NearestBinding::Module => true,
            NearestBinding::ModuleBlock | NearestBinding::Local => !self.module_declared,
            NearestBinding::Imported => !self.module_bound && !self.outer_declared,
            NearestBinding::Represented | NearestBinding::Anonymous | NearestBinding::Unknown => {
                false
            }
        }
    }
}

/// Whether a reference named by a static chain (`Base`, `ns.Base`) resolves
/// to what the chain reads. A bare name must resolve by name
/// ([`ReadBinding::resolves_by_name`]); a member chain is resolved through
/// a namespace import of its receiver, so its receiver must name a module
/// import or an uncontested local `require`/`import()` namespace. A
/// `this`/`super` receiver binds nothing.
pub(super) fn static_chain_resolves(
    builder: &mut ExtractionBuilder<'_, '_>,
    chain: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut receiver = chain;
    for _ in 0..MAX_STATIC_CHAIN_DEPTH {
        if receiver.kind() != "member_expression" {
            break;
        }
        let Some(object) = receiver.child_by_field_name("object") else {
            return Ok(false);
        };
        receiver = object;
    }
    if receiver.kind() != "identifier" {
        return Ok(receiver.kind() != "member_expression");
    }
    let binding = read_binding(builder, receiver)?;
    Ok(if receiver.id() == chain.id() {
        binding.resolves_by_name()
    } else {
        // A member chain resolves through the namespace import its receiver
        // names: a module one, or a local `require`/`import()` namespace no
        // module binding competes with.
        match binding.nearest {
            NearestBinding::Module => true,
            NearestBinding::Imported => !binding.module_bound && !binding.outer_declared,
            _ => false,
        }
    })
}

/// The root of the syntax tree that contains `node`.
fn tree_root(node: Node<'_>) -> Node<'_> {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    root
}

impl ScopeIndex<'_> {
    /// The index of a file too large to scan: every name counts as bound.
    fn overflowed() -> Self {
        Self {
            writes: writes::Writes::default(),
            scopes: Vec::new(),
            block_declarations: BTreeMap::new(),
            overflowed: true,
        }
    }

    /// The innermost scope containing `node`.
    fn innermost(&self, node: Node<'_>) -> Option<usize> {
        let (start, end) = (node.start_byte(), node.end_byte());
        let mut current = self
            .scopes
            .partition_point(|scope| scope.start <= start)
            .checked_sub(1);
        // The last scope starting at or before the node is the innermost
        // containing one or a descendant of it, so its chain reaches it.
        while let Some(index) = current {
            let scope = &self.scopes[index];
            if end <= scope.end {
                return Some(index);
            }
            current = scope.parent;
        }
        None
    }

    /// Walk outward from the innermost scope containing `read` to the module
    /// scope.
    fn binding(&self, read: Node<'_>, name: &str) -> ReadBinding {
        if self.overflowed {
            return ReadBinding::UNKNOWN;
        }
        let chain = self.chain(read, name);
        let module = chain.module.map(|index| &self.scopes[index].names);
        let module_bound =
            module.is_some_and(|module| module.overflowed || module.names.contains_key(name));
        let module_declared = module.is_some_and(|module| {
            module.overflowed
                || matches!(module.names.get(name), Some(Form::Local | Form::Imported))
        });
        // The resolver binds a bare name to a same-file top-level declaration
        // first, then to a same-named declaration of an enclosing container,
        // and only then to an import. Without a top-level declaration, a
        // block the read is outside of that declares the name could capture
        // the read although its declaration is not visible there: a block of
        // an enclosing container when nothing nearer binds the name, and a
        // module-level block (a top-level symbol) even when something does.
        // A container between the read and its nearest binding whose blocks
        // declare the name is searched by the resolver first.
        if chain.nearest.is_some() && chain.hidden_before_nearest {
            return ReadBinding::UNKNOWN;
        }
        let module_blocks = self.module_blocks_declare(|blocks| blocks.values.contains(name));
        let hidden = match chain.nearest {
            None => module_blocks || chain.hidden_before_nearest,
            // The read's own binding is the module-level block declaration.
            Some((_, _, ScopeLevel::ModuleBlock)) => false,
            Some(_) => module_blocks,
        };
        if hidden && !module_declared {
            return ReadBinding::UNKNOWN;
        }
        ReadBinding {
            nearest: chain
                .nearest
                .map_or(NearestBinding::Module, |(binding, _, _)| binding),
            module_bound,
            module_declared,
            outer_declared: chain.outer_declared,
            scope: chain.nearest.map(|(_, range, _)| range),
        }
    }

    /// The read's scope chain: its nearest binding, the module scope, and
    /// the competing declarations the resolver could prefer.
    fn chain(&self, read: Node<'_>, name: &str) -> ScopeChain {
        let mut chain = ScopeChain::default();
        let mut current = self.innermost(read);
        while let Some(index) = current {
            let scope = &self.scopes[index];
            if scope.level == ScopeLevel::Module {
                chain.module = Some(index);
                break;
            }
            let hides = scope.container
                && self
                    .block_declarations
                    .get(&Some(index))
                    .is_some_and(|blocks| blocks.values.contains(name));
            if chain.nearest.is_none() {
                chain.hidden_before_nearest |= hides;
                chain.nearest = scope
                    .nearest(name)
                    .map(|binding| (binding, (scope.start, scope.end), scope.level));
            } else {
                // The containers' block declarations beyond the nearest
                // binding include that binding itself, so only the outer
                // scopes' own declarations count here.
                chain.outer_declared |= matches!(
                    scope.names.names.get(name),
                    Some(Form::Local | Form::Imported | Form::Represented)
                );
            }
            current = scope.parent;
        }
        chain
    }

    /// Whether the bare type name `name` read at `node` resolves by name to
    /// what it names (see [`type_name_resolves`]).
    fn type_resolves(&self, node: Node<'_>, name: &str) -> bool {
        if self.overflowed {
            return false;
        }
        let mut current = self.innermost(node);
        while let Some(index) = current {
            let scope = &self.scopes[index];
            if scope.level == ScopeLevel::Module {
                break;
            }
            if scope.names.overflowed || scope.names.type_parameters.contains(&name) {
                return false;
            }
            if scope.names.types.contains(&name) {
                return !self.module_declares_type(name)
                    && !self.module_blocks_declare(|blocks| blocks.types.contains(name));
            }
            current = scope.parent;
        }
        self.module_declares_type(name)
            || !self.hidden_in_block(node, |blocks| blocks.types.contains(name))
    }

    /// Whether a block that does not enclose `node` declares a name the
    /// resolver could bind a read at `node` to: a block outside every
    /// container, or a block of a container enclosing `node`.
    fn hidden_in_block(
        &self,
        node: Node<'_>,
        declares: impl Fn(&BlockDeclarations<'_>) -> bool,
    ) -> bool {
        if self.module_blocks_declare(&declares) {
            return true;
        }
        let in_blocks = |function: Option<usize>| {
            self.block_declarations
                .get(&function)
                .is_some_and(&declares)
        };
        let mut current = self.innermost(node);
        while let Some(index) = current {
            let scope = &self.scopes[index];
            if scope.container && in_blocks(Some(index)) {
                return true;
            }
            current = scope.parent;
        }
        false
    }

    /// Whether a block outside every container declares the name; the walker
    /// records such declarations as top-level symbols.
    fn module_blocks_declare(&self, declares: impl Fn(&BlockDeclarations<'_>) -> bool) -> bool {
        self.block_declarations.get(&None).is_some_and(declares)
    }

    /// Whether the module scope declares a type named `name` (a top-level
    /// symbol, which the resolver prefers to any nested declaration; an
    /// import only answers after the enclosing containers).
    fn module_declares_type(&self, name: &str) -> bool {
        self.scopes
            .first()
            .filter(|scope| scope.level == ScopeLevel::Module)
            .is_none_or(|module| {
                module.names.overflowed
                    || module.names.types.contains(&name)
                    || matches!(
                        module.names.names.get(name),
                        Some(Form::Local | Form::Imported)
                    )
            })
    }
}

impl IndexedScope<'_> {
    /// How this (non-module) scope binds `name`, if it does.
    fn nearest(&self, name: &str) -> Option<NearestBinding> {
        if self.names.overflowed {
            return Some(NearestBinding::Unknown);
        }
        Some(match self.names.names.get(name)? {
            Form::Anonymous => NearestBinding::Anonymous,
            Form::Represented => NearestBinding::Represented,
            Form::Imported | Form::Import => NearestBinding::Imported,
            Form::Local if self.level == ScopeLevel::ModuleBlock => NearestBinding::ModuleBlock,
            Form::Local => NearestBinding::Local,
        })
    }
}

/// One bounded pre-order pass over the whole tree, recording every scope.
fn build_index<'source>(
    builder: &mut ExtractionBuilder<'source, '_>,
    root: Node<'_>,
) -> Result<ScopeIndex<'source>, ExtractError> {
    let mut build = IndexBuild {
        index: ScopeIndex::default(),
        open: Vec::new(),
        kinds: Vec::new(),
        visits: 0,
        maximum_depth: builder.maximum_ast_depth.min(crate::MAXIMUM_AST_DEPTH),
    };
    let mut cursor = root.walk();
    let mut depth = 0_usize;
    loop {
        let visited = CursorNode {
            node: cursor.node(),
            depth,
            field: cursor.field_name(),
        };
        if !build.visit(builder, visited)? {
            return Ok(ScopeIndex::overflowed());
        }
        if cursor.goto_first_child() {
            depth = depth.saturating_add(1);
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(build.index);
            }
            depth = depth.saturating_sub(1);
        }
    }
}

/// The node an index pass is at, with its depth and its field in its parent.
#[derive(Clone, Copy)]
struct CursorNode<'tree> {
    node: Node<'tree>,
    depth: usize,
    field: Option<&'tree str>,
}

/// State of one index pass.
struct IndexBuild<'source, 'tree> {
    index: ScopeIndex<'source>,
    /// Scopes enclosing the current node, with the depth of each.
    open: Vec<(usize, usize)>,
    /// Node kinds from the root to the current node, by depth.
    kinds: Vec<&'tree str>,
    visits: usize,
    maximum_depth: usize,
}

impl<'source, 'tree> IndexBuild<'source, 'tree> {
    /// Record `node` if it opens a scope; `false` when a bound is exceeded.
    fn visit(
        &mut self,
        builder: &mut ExtractionBuilder<'source, '_>,
        CursorNode { node, depth, field }: CursorNode<'tree>,
    ) -> Result<bool, ExtractError> {
        if !self.charge(builder, 1)? || depth > self.maximum_depth {
            return Ok(false);
        }
        self.index.writes.observe(builder, node)?;
        while self.open.last().is_some_and(|(_, open)| *open >= depth) {
            self.open.pop();
        }
        self.kinds.truncate(depth);
        let parent_kind = self.kinds.last().copied();
        self.kinds
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.kinds.push(node.kind());
        // `with (object) body` resolves the body's names against `object`
        // first, so any name may be bound there; the object operand itself
        // is evaluated outside.
        let with_body = parent_kind == Some("with_statement") && field == Some("body");
        if !with_body && !is_scope(node) {
            return Ok(true);
        }
        let parent = self.open.last().map(|(index, _)| *index);
        let level = self.level(node, parent, parent_kind);
        let hoists = hoists_vars(node, level, parent_kind);
        let Some(mut names) = self.scope_names(builder, (node, hoists))? else {
            return Ok(false);
        };
        names.overflowed |= with_body;
        self.index
            .scopes
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.open
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.open.push((self.index.scopes.len(), depth));
        let container = FUNCTION_SCOPE_KINDS.contains(&node.kind())
            || CONTAINER_SCOPE_KINDS.contains(&node.kind());
        if !container && level != ScopeLevel::Module {
            self.record_block_declarations(&names);
        }
        self.index.scopes.push(IndexedScope {
            start: node.start_byte(),
            end: node.end_byte(),
            parent,
            level,
            container,
            names,
        });
        Ok(true)
    }

    /// Record a block's declared values and types under the container that
    /// holds their symbols (the nearest open function or class).
    fn record_block_declarations(&mut self, names: &ScopeNames<'source>) {
        let container = self
            .open
            .iter()
            .rev()
            .map(|(index, _)| *index)
            .find(|index| {
                self.index
                    .scopes
                    .get(*index)
                    .is_some_and(|scope| scope.container)
            });
        let blocks = self.index.block_declarations.entry(container).or_default();
        blocks.values.extend(
            names
                .names
                .iter()
                .filter(|(_, form)| matches!(**form, Form::Local | Form::Imported | Form::Import))
                .map(|(name, _)| *name),
        );
        blocks.types.extend(names.types.iter().copied());
    }

    /// Where a new scope sits: function scopes, static-block bodies, and
    /// everything inside them are function level.
    fn level(
        &self,
        node: Node<'_>,
        parent: Option<usize>,
        parent_kind: Option<&str>,
    ) -> ScopeLevel {
        if node.kind() == "program" {
            return ScopeLevel::Module;
        }
        let inside_function = parent
            .and_then(|parent| self.index.scopes.get(parent))
            .is_some_and(|parent| parent.level == ScopeLevel::Function);
        if inside_function
            || FUNCTION_SCOPE_KINDS.contains(&node.kind())
            || parent_kind == Some("class_static_block")
        {
            ScopeLevel::Function
        } else {
            ScopeLevel::ModuleBlock
        }
    }

    /// The names one scope binds (with the `var`s hoisted into it when
    /// `hoists`), or `None` when the file bound ran out.
    fn scope_names(
        &mut self,
        builder: &mut ExtractionBuilder<'source, '_>,
        (scope, hoists): (Node<'_>, bool),
    ) -> Result<Option<ScopeNames<'source>>, ExtractError> {
        let mut collector = ScopeCollector {
            source: builder.context.snapshot.source(),
            names: ScopeNames::default(),
            binding_budget: MAX_SCOPE_BINDING_NODES,
            local_budget: MAX_SCOPE_LOCAL_NODES,
        };
        collector.collect(builder, scope);
        if hoists {
            let Some(visited) = collector.hoist(builder, scope, self.remaining())? else {
                return Ok(None);
            };
            if !self.charge(builder, visited)? {
                return Ok(None);
            }
        }
        Ok(Some(collector.names))
    }

    /// Nodes the index may still visit.
    const fn remaining(&self) -> usize {
        MAX_INDEX_NODES.saturating_sub(self.visits)
    }

    /// Spend `visits` index nodes, polling cancellation; `false` when the
    /// file bound is exceeded.
    fn charge(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        visits: usize,
    ) -> Result<bool, ExtractError> {
        let before = self.visits / INDEX_CANCELLATION_INTERVAL;
        self.visits = self.visits.saturating_add(visits);
        if self.visits / INDEX_CANCELLATION_INTERVAL != before {
            builder.context.ensure_active()?;
        }
        Ok(self.visits <= MAX_INDEX_NODES)
    }
}

/// Whether a scope collects hoisted `var`s: the module, and the bodies of
/// functions and class static blocks.
fn hoists_vars(scope: Node<'_>, level: ScopeLevel, parent_kind: Option<&str>) -> bool {
    level == ScopeLevel::Module
        || (scope.kind() == "statement_block"
            && parent_kind.is_some_and(|kind| {
                FUNCTION_SCOPE_KINDS.contains(&kind) || kind == "class_static_block"
            }))
}

/// Whether a node can bind names for the code inside it.
fn is_scope(node: Node<'_>) -> bool {
    FUNCTION_SCOPE_KINDS.contains(&node.kind())
        || TYPE_SCOPE_KINDS.contains(&node.kind())
        || matches!(
            node.kind(),
            "program"
                | "class"
                | "class_declaration"
                | "abstract_class_declaration"
                | "internal_module"
                | "module"
                | "catch_clause"
                | "for_in_statement"
                | "for_statement"
                | "statement_block"
                | "switch_body"
        )
}

/// Bounded collection of one scope's bindings.
struct ScopeCollector<'source> {
    source: &'source str,
    names: ScopeNames<'source>,
    binding_budget: usize,
    local_budget: usize,
}

impl ScopeCollector<'_> {
    /// Record what a scope node itself binds; a function body's hoisted
    /// `var`s are collected separately.
    fn collect(&mut self, builder: &ExtractionBuilder<'_, '_>, scope: Node<'_>) {
        self.type_parameters(scope.child_by_field_name("type_parameters"));
        if !self.collect_signature(scope) {
            self.collect_block(builder, scope);
        }
    }

    /// Record what a type-level or callable scope's signature binds; false
    /// when the scope has no such signature.
    fn collect_signature(&mut self, scope: Node<'_>) -> bool {
        let field = |name| scope.child_by_field_name(name);
        match scope.kind() {
            "conditional_type" => self.inferred_types(field("right")),
            "index_signature" => self.mapped_type_parameter(scope),
            kind if FUNCTION_SCOPE_KINDS.contains(&kind) => {
                let parameters = field("parameters").or_else(|| field("parameter"));
                self.bind(parameters, parameter_form(scope, self.source));
                if matches!(kind, "function_expression" | "generator_function") {
                    self.bind(field("name"), Form::Anonymous);
                }
            }
            // A function type's or signature's parameters bind values only
            // for its own `typeof` queries.
            kind if TYPE_SCOPE_KINDS.contains(&kind) && field("parameters").is_some() => {
                self.bind(field("parameters"), Form::Anonymous);
            }
            _ => return false,
        }
        true
    }

    /// Record the key name a mapped type clause (`[K in Keys]`) introduces.
    fn mapped_type_parameter(&mut self, scope: Node<'_>) {
        let mapped = named_children(scope).find(|child| child.kind() == "mapped_type_clause");
        if let Some(name) = mapped
            .and_then(|clause| clause.child_by_field_name("name"))
            .and_then(|name| self.source.get(name.start_byte()..name.end_byte()))
        {
            self.names.type_parameters.push(name);
        }
    }

    /// Record what a class, clause, loop, or statement-list scope binds.
    fn collect_block(&mut self, builder: &ExtractionBuilder<'_, '_>, scope: Node<'_>) {
        let field = |name| scope.child_by_field_name(name);
        match scope.kind() {
            "class" => self.bind_name(field("name"), Form::Anonymous),
            "catch_clause" => self.bind(field("parameter"), Form::Anonymous),
            "for_in_statement" if field("kind").is_some() => {
                self.bind(field("left"), Form::Anonymous);
            }
            "for_statement" => {
                if let Some(initializer) = field("initializer") {
                    self.declare(builder, initializer);
                }
            }
            "program" | "statement_block" => self.declare_statements(builder, scope),
            "switch_body" => {
                for case in named_children(scope) {
                    self.declare_statements(builder, case);
                }
            }
            _ => {}
        }
    }

    /// Record a type a block statement declares.
    fn declare_type(&mut self, name: Option<Node<'_>>) {
        if let Some(name) =
            name.and_then(|name| self.source.get(name.start_byte()..name.end_byte()))
        {
            self.names.types.push(name);
        }
    }

    /// Record the names of a declaration's type parameters (`<T, U>`).
    fn type_parameters(&mut self, parameters: Option<Node<'_>>) {
        let Some(parameters) = parameters else {
            return;
        };
        for parameter in named_children(parameters) {
            if let Some(name) = parameter
                .child_by_field_name("name")
                .and_then(|name| self.source.get(name.start_byte()..name.end_byte()))
            {
                self.names.type_parameters.push(name);
            }
        }
    }

    /// Record the `infer X` names a conditional type's `extends` clause
    /// introduces (for its branches; over-approximated to both).
    fn inferred_types(&mut self, clause: Option<Node<'_>>) {
        let Some(clause) = clause else {
            return;
        };
        for node in descendants_including_root(clause) {
            let Some(remaining) = self.binding_budget.checked_sub(1) else {
                self.names.overflowed = true;
                return;
            };
            self.binding_budget = remaining;
            if node.kind() == "infer_type"
                && let Some(name) = named_children(node)
                    .find(|child| child.kind() == "type_identifier")
                    .and_then(|name| self.source.get(name.start_byte()..name.end_byte()))
            {
                self.names.type_parameters.push(name);
            }
        }
    }

    /// Record every name a binding target binds.
    fn bind(&mut self, target: Option<Node<'_>>, form: Form) {
        let Some(target) = target else {
            return;
        };
        let source = self.source;
        let names = &mut self.names.names;
        let budget = if matches!(form, Form::Local | Form::Imported | Form::Import) {
            &mut self.local_budget
        } else {
            &mut self.binding_budget
        };
        let outcome = scan_bound_names(target, budget, |bound| {
            if let Some(name) = source.get(bound.start_byte()..bound.end_byte()) {
                record(names, name, form);
            }
            false
        });
        if outcome == BindingMatch::Exhausted {
            self.names.overflowed = true;
        }
    }

    /// Record a declaration's own name (`class C`, `enum E`, `namespace N`,
    /// whose leading identifier `N` of `N.M` is the bound value).
    fn bind_name(&mut self, name: Option<Node<'_>>, form: Form) {
        let Some(mut name) = name else {
            return;
        };
        if name.kind() == "nested_identifier"
            && let Some(first) = named_children(name).next()
        {
            name = first;
        }
        if matches!(name.kind(), "identifier" | "type_identifier")
            && let Some(text) = self.source.get(name.start_byte()..name.end_byte())
        {
            record(&mut self.names.names, text, form);
        }
    }

    /// Record the names a `let`/`const`/`using`/`var` declaration declares;
    /// a `require(..)` or `import(..)` initializer binds imported values.
    fn declare(&mut self, builder: &ExtractionBuilder<'_, '_>, declaration: Node<'_>) {
        if !matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration" | "using_declaration"
        ) {
            return;
        }
        for declarator in
            named_children(declaration).filter(|child| child.kind() == "variable_declarator")
        {
            let value = declarator.child_by_field_name("value");
            let form = if declaration.kind() == "using_declaration" {
                // The walker records no symbol for a `using` resource.
                Form::Anonymous
            } else if module_system::is_static_module_binding_value(builder, value) {
                Form::Imported
            } else {
                Form::Local
            };
            self.bind(declarator.child_by_field_name("name"), form);
        }
    }

    /// Record the block-scoped declarations among a block's (or the
    /// module's) direct statements, `export`ed and `declare`d ones (a
    /// namespace body) included. A `var` belongs to the enclosing function
    /// body instead.
    fn declare_statements(&mut self, builder: &ExtractionBuilder<'_, '_>, block: Node<'_>) {
        for statement in named_children(block) {
            let Some(declaration) = unwrap_declaration(statement) else {
                continue;
            };
            match declaration.kind() {
                "lexical_declaration" | "using_declaration" => {
                    self.declare(builder, declaration);
                }
                "function_declaration"
                | "generator_function_declaration"
                | "function_signature"
                | "internal_module"
                | "module" => {
                    self.bind_name(declaration.child_by_field_name("name"), Form::Local);
                }
                "class_declaration" | "abstract_class_declaration" | "enum_declaration" => {
                    self.bind_name(declaration.child_by_field_name("name"), Form::Local);
                    self.declare_type(declaration.child_by_field_name("name"));
                }
                "type_alias_declaration" | "interface_declaration" => {
                    self.declare_type(declaration.child_by_field_name("name"));
                }
                "import_alias" => {
                    self.bind_name(declaration.named_child(0), Form::Import);
                    self.declare_type(declaration.named_child(0));
                }
                "import_statement" => self.import(declaration),
                _ => {}
            }
        }
    }

    /// Record the locals an `import` statement binds: the default, the
    /// namespace, and each (possibly renamed) named import.
    fn import(&mut self, statement: Node<'_>) {
        let Some(clause) = named_children(statement).find(|child| child.kind() == "import_clause")
        else {
            return;
        };
        for child in named_children(clause) {
            match child.kind() {
                "identifier" => self.bind_name(Some(child), Form::Import),
                "namespace_import" => self.bind_name(child.named_child(0), Form::Import),
                "named_imports" => {
                    for specifier in named_children(child)
                        .filter(|specifier| specifier.kind() == "import_specifier")
                    {
                        let local = specifier
                            .child_by_field_name("alias")
                            .or_else(|| specifier.child_by_field_name("name"));
                        self.bind_name(local, Form::Import);
                    }
                }
                _ => {}
            }
        }
    }

    /// Record the `var` declarations, `for (var x ..)` variables, and nested
    /// block function declarations hoisted into a function, static-block, or
    /// module body, not crossing into nested function scopes or static
    /// blocks, which hoist their own. Returns the nodes visited, or `None`
    /// past `limit`.
    fn hoist(
        &mut self,
        builder: &mut ExtractionBuilder<'_, '_>,
        body: Node<'_>,
        limit: usize,
    ) -> Result<Option<usize>, ExtractError> {
        let mut pending = vec![body];
        let mut visits = 0_usize;
        while let Some(node) = pending.pop() {
            visits = visits.saturating_add(1);
            if visits > limit {
                return Ok(None);
            }
            if visits.is_multiple_of(INDEX_CANCELLATION_INTERVAL) {
                builder.context.ensure_active()?;
            }
            match node.kind() {
                "variable_declaration" => self.declare(builder, node),
                "for_in_statement" if declares_var(node, self.source) => {
                    self.bind(node.child_by_field_name("left"), Form::Local);
                }
                _ => {}
            }
            for child in named_children(node) {
                if !FUNCTION_SCOPE_KINDS.contains(&child.kind())
                    && child.kind() != "class_static_block"
                {
                    pending
                        .try_reserve(1)
                        .map_err(|_| ExtractError::OutputLimit)?;
                    pending.push(child);
                }
            }
        }
        Ok(Some(visits))
    }
}

/// Mark `name` as bound in `form`, keeping the greatest form bound so far.
fn record<'source>(names: &mut BTreeMap<&'source str, Form>, name: &'source str, form: Form) {
    names
        .entry(name)
        .and_modify(|bound| *bound = (*bound).max(form))
        .or_insert(form);
}

/// Whether the walker gives a function's parameters symbols of their own.
fn parameter_form(function: Node<'_>, source: &str) -> Form {
    let represented = matches!(
        function.kind(),
        "function_declaration" | "generator_function_declaration" | "method_definition"
    ) || javascript_members::function_has_parameter_symbols(function, source);
    if represented {
        Form::Represented
    } else {
        Form::Anonymous
    }
}

/// The declaration a block statement makes, looking through `export` and
/// `declare` (`export declare const X`).
fn unwrap_declaration(statement: Node<'_>) -> Option<Node<'_>> {
    let mut declaration = statement;
    for _ in 0..MAX_DECLARATION_WRAPPERS {
        declaration = match declaration.kind() {
            "export_statement" => declaration.child_by_field_name("declaration")?,
            "ambient_declaration" => {
                named_children(declaration).find(|child| child.kind() != "comment")?
            }
            _ => return Some(declaration),
        };
    }
    None
}

/// Whether a `for (.. in/of ..)` loop declares its variable with `var`.
fn declares_var(loop_statement: Node<'_>, source: &str) -> bool {
    loop_statement
        .child_by_field_name("kind")
        .and_then(|kind| source.get(kind.start_byte()..kind.end_byte()))
        == Some("var")
}
