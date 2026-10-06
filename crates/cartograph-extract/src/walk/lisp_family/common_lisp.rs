//! Common Lisp list forms: packages, `defun`-family definitions, named
//! definitions, imports, binding special forms, and list-head calls.

use cartograph_domain::{ReferenceKind, SymbolId, SymbolKind};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    super::{
        ExtractionBuilder,
        family_support::{
            DeclarationShape, MAX_RETAINED_TEXT_BYTES, OwnedReference, ScopeVisit, ScopedEmission,
            SymbolEmission, bounded_name, emit_declaration, emit_import_reference,
            emit_owned_reference, emit_scoped_declaration, literal_free_signature,
            register_scope_key, source_text, visit_in_scope,
        },
        specifier_safety::specifier_may_carry_credential,
        syntax::named_children,
    },
    AFTER_HEAD, AFTER_NAME, form_children,
};

/// Special operators and binding macros that are not calls.
const NON_CALL_HEADS: &[&str] = &[
    "block",
    "catch",
    "eval-when",
    "function",
    "go",
    "if",
    "load-time-value",
    "locally",
    "loop",
    "prog1",
    "prog2",
    "progn",
    "return-from",
    "setf",
    "setq",
    "tagbody",
    "throw",
    "unless",
    "unwind-protect",
    "when",
];

/// Deepest reader-quote nesting unwrapped while naming a form.
const MAX_QUOTE_NESTING: usize = 4;
/// Prefixes naming the standard `COMMON-LISP` package, which form heads may
/// carry without changing meaning; longest first so `::` is stripped whole.
const STANDARD_PACKAGE_PREFIXES: &[&str] = &["common-lisp::", "common-lisp:", "cl::", "cl:"];
/// The symbol naming the empty list, which is also an empty lambda list.
const EMPTY_LIST: &str = "nil";
/// Reader syntax that preserves the case of the characters it escapes.
const CASE_PRESERVING_MARKERS: &[char] = &['|', '"', '\\'];
/// Scope-map key prefix that keeps package lookups apart from other scope facts.
const NAMESPACE_SCOPE_PREFIX: &str = "lisp-namespace:";
/// Lambda-list keywords after which a nested list carries a default value.
const DEFAULTING_LAMBDA_KEYWORDS: &[&str] = &["&optional", "&key", "&aux"];

/// What a list form does, decided by its head designator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Namespace,
    /// A `defun`-family list; `true` when method qualifiers may precede the
    /// lambda list.
    Function(bool),
    Named(SymbolKind, bool),
    Import(ImportShape),
    Binding(BindingLayout),
    Data,
    NonCall,
    Call,
}

/// Whether an import form names only packages or a package and its symbols.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportShape {
    /// `(use-package :a :b)`: every argument is a package or system.
    Packages,
    /// `(:import-from :package :symbol...)`: one package, then imported symbols.
    From,
}

/// Classify a list by its lower-cased, `cl:`-free head.
fn form(key: &str) -> Form {
    if let Some(layout) = binding_layout(key) {
        return Form::Binding(layout);
    }
    match key {
        "defpackage" | "in-package" => Form::Namespace,
        // The grammar only recognizes lower-case `defun`-family keywords; the
        // reader folds case, so other spellings define functions too.
        "defun" | "defmacro" | "defgeneric" => Form::Function(false),
        "defmethod" => Form::Function(true),
        "defvar" | "defparameter" | "defconstant" => Form::Named(SymbolKind::Constant, true),
        "defclass" | "define-condition" => Form::Named(SymbolKind::Class, false),
        "defstruct" => Form::Named(SymbolKind::Struct, false),
        "import-from" => Form::Import(ImportShape::From),
        "use-package" | "require" | "import" | "use" => Form::Import(ImportShape::Packages),
        // Declarations and proclamations are compiler advice, not calls.
        "quote" | "declare" | "declaim" => Form::Data,
        _ if NON_CALL_HEADS.contains(&key) => Form::NonCall,
        _ => Form::Call,
    }
}

/// Where a binding form introduces names that are not calls.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingLayout {
    /// `(let ((name init)...) body)`: each binding's leading elements are names.
    Bindings(usize),
    /// `(lambda (names...) body)`: the second form only names variables, or
    /// a type that `(the type form)` declares.
    Names,
    /// `(dolist (name list) body)`: the second form starts with a name.
    Spec,
    /// `(case key (keys body)...)`: each clause's leading elements are keys or names.
    Clauses(usize),
    /// `(cond (test body...)...)`: every clause's elements are evaluated forms.
    Tests,
}

/// Binding forms whose name positions would otherwise read as list-head calls.
const BINDING_LAYOUTS: &[(&str, BindingLayout)] = &[
    ("case", BindingLayout::Clauses(1)),
    ("ccase", BindingLayout::Clauses(1)),
    ("cond", BindingLayout::Tests),
    ("ctypecase", BindingLayout::Clauses(1)),
    ("destructuring-bind", BindingLayout::Names),
    ("do", BindingLayout::Bindings(1)),
    ("do*", BindingLayout::Bindings(1)),
    ("dolist", BindingLayout::Spec),
    ("dotimes", BindingLayout::Spec),
    ("ecase", BindingLayout::Clauses(1)),
    ("etypecase", BindingLayout::Clauses(1)),
    ("flet", BindingLayout::Bindings(2)),
    ("handler-bind", BindingLayout::Bindings(1)),
    ("handler-case", BindingLayout::Clauses(2)),
    ("labels", BindingLayout::Bindings(2)),
    ("lambda", BindingLayout::Names),
    ("let", BindingLayout::Bindings(1)),
    ("let*", BindingLayout::Bindings(1)),
    ("macrolet", BindingLayout::Bindings(2)),
    ("multiple-value-bind", BindingLayout::Names),
    ("restart-bind", BindingLayout::Bindings(1)),
    ("restart-case", BindingLayout::Clauses(2)),
    ("symbol-macrolet", BindingLayout::Bindings(1)),
    ("the", BindingLayout::Names),
    ("typecase", BindingLayout::Clauses(1)),
];

/// The binding layout of a binding form, if `key` names one.
fn binding_layout(key: &str) -> Option<BindingLayout> {
    BINDING_LAYOUTS
        .iter()
        .find(|(head, _)| *head == key)
        .map(|(_, layout)| *layout)
}

/// Where a designator appears, which decides the spellings it may take.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Designator {
    /// A declared or invoked symbol: never a string literal.
    Symbol,
    /// A package or system name, which may also be spelled as a string.
    Package,
}

/// Clean a designator: `|name|`, `#:name`, and `:name` designate `name`, a
/// package may also be `"name"`, and quoted designators name their symbol.
fn designator<'source>(
    source: &'source str,
    node: Node<'_>,
    usage: Designator,
) -> Option<&'source str> {
    let mut current = node;
    for _ in 0..MAX_QUOTE_NESTING {
        match current.kind() {
            "quoting_lit" | "var_quoting_lit" => current = named_children(current).next()?,
            "fancy_literal" => return escaped_symbol(source_text(source, current)),
            "sym_lit" | "package_lit" | "kwd_lit" => {
                return clean_designator(source_text(source, current));
            }
            "str_lit" if usage == Designator::Package => {
                let text = source_text(source, current).trim();
                return clean_designator(text.strip_prefix('"')?.strip_suffix('"')?);
            }
            _ => return None,
        }
    }
    None
}

/// The name a `|...|` escaped symbol spells, which may contain spaces.
fn escaped_symbol(text: &str) -> Option<&str> {
    let name = text.trim().strip_prefix('|')?.strip_suffix('|')?;
    let valid = !name.trim().is_empty()
        && name.len() <= MAX_RETAINED_TEXT_BYTES
        && !specifier_may_carry_credential(name)
        && !name.chars().any(|character| {
            character.is_control() || matches!(character, '\\' | '|' | '"' | '\'' | '`')
        });
    valid.then_some(name)
}

/// Strip a package marker from designator text; a whole `|...|` symbol keeps
/// its escaped spelling.
fn clean_designator(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.contains('\\') {
        return None;
    }
    if text.len() > 1 && text.starts_with('|') && text.ends_with('|') {
        return escaped_symbol(text);
    }
    let text = text
        .strip_prefix("#:")
        .or_else(|| text.strip_prefix(':'))
        .unwrap_or(text);
    bounded_name(text)
}

/// The operator a head designator names, lower-cased and free of a standard
/// package prefix, as the reader interns it.
///
/// The reader upcases an unescaped symbol, so its spelling is irrelevant. An
/// escaped symbol (`|defun|`) keeps its exact case and names a standard
/// operator only when spelled in upper case; any other escaped spelling is an
/// ordinary function, so it has no operator key. Escaped text never carries a
/// package prefix, since its colons are literal.
fn operator_key(raw: &str, head: &str) -> Option<String> {
    let lowered = head.to_ascii_lowercase();
    if raw.contains(CASE_PRESERVING_MARKERS) {
        // An escaped colon is part of the name, never a package separator.
        return (head == head.to_ascii_uppercase()).then_some(lowered);
    }
    let key = STANDARD_PACKAGE_PREFIXES
        .iter()
        .find_map(|prefix| lowered.strip_prefix(prefix))
        .unwrap_or(&lowered);
    Some(key.to_owned())
}

/// Declare, import, or record the call a list form makes.
pub(super) fn visit_list(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let source = builder.context.snapshot.source();
    let forms = form_children(node);
    let Some((&head_node, head)) = forms
        .first()
        .and_then(|head| designator(source, *head, Designator::Symbol).map(|name| (head, name)))
    else {
        return Ok(false);
    };
    let key = operator_key(source_text(source, head_node), head);
    visit_form(
        builder,
        ListForm {
            named: NamedForm {
                node,
                forms: &forms,
                kind: SymbolKind::Namespace,
                visit_rest: true,
                depth,
            },
            head_node,
            head,
        },
        key.as_deref().map_or(Form::Call, form),
    )
}

/// A list form with its designator head.
#[derive(Clone, Copy)]
struct ListForm<'tree, 'forms, 'head> {
    named: NamedForm<'tree, 'forms>,
    head_node: Node<'tree>,
    head: &'head str,
}

/// Visit a list as the form its head operator selects; true when the list's
/// children need no further traversal.
fn visit_form(
    builder: &mut ExtractionBuilder<'_, '_>,
    list: ListForm<'_, '_, '_>,
    form: Form,
) -> Result<bool, ExtractError> {
    let named = list.named;
    match form {
        Form::Namespace => visit_namespace(builder, named),
        Form::Function(qualified) => {
            visit_function_list(builder, named, qualified)?;
            Ok(true)
        }
        Form::Named(kind, visit_rest) => visit_named(
            builder,
            NamedForm {
                kind,
                visit_rest,
                ..named
            },
        ),
        Form::Import(shape) => {
            visit_imports(builder, named.forms, shape)?;
            Ok(true)
        }
        Form::Binding(layout) => {
            visit_binding_form(
                builder,
                BindingFormVisit {
                    forms: named.forms,
                    layout,
                    depth: named.depth,
                },
            )?;
            Ok(true)
        }
        Form::Data => Ok(true),
        Form::NonCall => Ok(false),
        Form::Call => {
            emit_head_call(builder, list)?;
            Ok(false)
        }
    }
}

/// Record a call to the list's head when it is a plain symbol designator.
fn emit_head_call(
    builder: &mut ExtractionBuilder<'_, '_>,
    list: ListForm<'_, '_, '_>,
) -> Result<(), ExtractError> {
    if !matches!(
        list.head_node.kind(),
        "sym_lit" | "package_lit" | "fancy_literal"
    ) {
        return Ok(());
    }
    emit_owned_reference(
        builder,
        OwnedReference {
            name: list.head,
            kind: ReferenceKind::Calls,
            node: list.head_node,
        },
    )
}

/// A binding form together with its layout and traversal depth.
#[derive(Clone, Copy)]
struct BindingFormVisit<'tree, 'forms> {
    forms: &'forms [Node<'tree>],
    layout: BindingLayout,
    depth: usize,
}

/// Visit a binding form's initializers and body without its introduced names.
fn visit_binding_form(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: BindingFormVisit<'_, '_>,
) -> Result<(), ExtractError> {
    let child_depth = input.depth.saturating_add(1);
    let introducer = input.forms.get(AFTER_HEAD).copied();
    let (body, clause_skip) = match input.layout {
        BindingLayout::Bindings(skip) => {
            for binding in introducer.map(form_children).unwrap_or_default() {
                if binding.kind() == "list_lit" {
                    visit_forms_after(builder, FormsAfter::new(binding, skip, child_depth))?;
                }
            }
            (input.forms.get(AFTER_NAME..), None)
        }
        BindingLayout::Names => (input.forms.get(AFTER_NAME..), None),
        BindingLayout::Spec => {
            if let Some(spec) = introducer {
                visit_forms_after(builder, FormsAfter::new(spec, 1, child_depth))?;
            }
            (input.forms.get(AFTER_NAME..), None)
        }
        BindingLayout::Clauses(skip) => {
            if let Some(key) = introducer {
                builder.visit(key, child_depth)?;
            }
            (input.forms.get(AFTER_NAME..), Some(skip))
        }
        BindingLayout::Tests => (input.forms.get(AFTER_HEAD..), Some(0)),
    };
    for form in body.unwrap_or_default() {
        match clause_skip {
            Some(skip) if form.kind() == "list_lit" => {
                visit_forms_after(builder, FormsAfter::new(*form, skip, child_depth))?;
            }
            _ => builder.visit(*form, child_depth)?,
        }
    }
    Ok(())
}

/// The child forms of `list` after its first `skip` forms.
#[derive(Clone, Copy)]
struct FormsAfter<'tree> {
    list: Node<'tree>,
    skip: usize,
    depth: usize,
}

impl<'tree> FormsAfter<'tree> {
    const fn new(list: Node<'tree>, skip: usize, depth: usize) -> Self {
        Self { list, skip, depth }
    }
}

/// Visit the selected trailing forms of one list.
fn visit_forms_after(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: FormsAfter<'_>,
) -> Result<(), ExtractError> {
    for form in form_children(input.list).into_iter().skip(input.skip) {
        builder.visit(form, input.depth.saturating_add(1))?;
    }
    Ok(())
}

/// One named declaration form and its traversal context.
#[derive(Clone, Copy)]
struct NamedForm<'tree, 'forms> {
    node: Node<'tree>,
    forms: &'forms [Node<'tree>],
    kind: SymbolKind,
    visit_rest: bool,
    depth: usize,
}

/// A `defpackage` or `in-package` form and its package options.
fn visit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedForm<'_, '_>,
) -> Result<bool, ExtractError> {
    let NamedForm {
        node, forms, depth, ..
    } = input;
    let source = builder.context.snapshot.source();
    let Some(name) = forms
        .get(AFTER_HEAD)
        .and_then(|name| designator(source, *name, Designator::Package))
    else {
        return Ok(false);
    };
    let options = forms.get(AFTER_NAME..).unwrap_or_default();
    let package = PackageForm {
        node,
        raw: forms
            .get(AFTER_HEAD)
            .map_or("", |name| source_text(source, *name)),
        name,
    };
    match package_occurrence(builder, package)? {
        PackageOccurrence::Declared(owner) => visit_in_scope(
            builder,
            ScopeVisit {
                owner: &owner,
                kind: SymbolKind::Namespace,
                name,
                children: options,
                depth,
            },
        )?,
        // A later form naming a known package must not claim ownership of
        // facts outside the declaring form's span.
        PackageOccurrence::Reentered => {
            for option in options {
                builder.visit(*option, depth.saturating_add(1))?;
            }
        }
    }
    Ok(true)
}

/// Whether a package form declared its package or re-entered a known one.
enum PackageOccurrence {
    Declared(SymbolId),
    Reentered,
}

/// A package form, the source spelling of its designator, and the name it
/// designates.
#[derive(Clone, Copy)]
struct PackageForm<'tree, 'text> {
    node: Node<'tree>,
    raw: &'text str,
    name: &'text str,
}

/// The package a `defpackage` or `in-package` form names, emitted once per file.
fn package_occurrence(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: PackageForm<'_, '_>,
) -> Result<PackageOccurrence, ExtractError> {
    let PackageForm { node, raw, name } = input;
    // Package names compare exactly, but the reader upcases an unescaped
    // symbol designator: `:app` and `"APP"` name one package, `"app"` another.
    let key = if raw.contains(CASE_PRESERVING_MARKERS) {
        format!("{NAMESPACE_SCOPE_PREFIX}{name}")
    } else {
        format!("{NAMESPACE_SCOPE_PREFIX}{}", name.to_ascii_uppercase())
    };
    if builder.native_scope_symbols.contains_key(&key) {
        return Ok(PackageOccurrence::Reentered);
    }
    let owned_name = builder.context.copy_text(name)?;
    let id = emit_declaration(
        builder,
        SymbolEmission {
            node,
            name: owned_name,
            body: None,
            signature: None,
            shape: DeclarationShape::plain(SymbolKind::Namespace, true),
        },
    )?;
    register_scope_key(builder, &key, Some((id.clone(), SymbolKind::Namespace)))?;
    Ok(PackageOccurrence::Declared(id))
}

/// Emit a constant, class, condition, or structure definition.
fn visit_named(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedForm<'_, '_>,
) -> Result<bool, ExtractError> {
    let source = builder.context.snapshot.source();
    let Some(&name_form) = input.forms.get(AFTER_HEAD) else {
        return Ok(false);
    };
    // `(defstruct (name options...) slots...)` names the structure first.
    let name_node = if input.kind == SymbolKind::Struct && name_form.kind() == "list_lit" {
        form_children(name_form).first().copied()
    } else {
        Some(name_form)
    };
    let Some(name) = name_node.and_then(|node| designator(source, node, Designator::Symbol)) else {
        return Ok(false);
    };
    let children = if input.visit_rest {
        input.forms.get(AFTER_NAME..).unwrap_or_default()
    } else {
        &[]
    };
    let name = builder.context.copy_text(name)?;
    let exported = builder.owners.is_empty();
    emit_scoped_declaration(
        builder,
        ScopedEmission {
            symbol: SymbolEmission {
                node: input.node,
                name,
                body: Some(input.node),
                signature: None,
                shape: DeclarationShape::plain(input.kind, exported),
            },
            children,
            depth: input.depth,
        },
    )?;
    Ok(true)
}

/// Every package or system an import form depends on.
///
/// `import-from` imports only its package: its symbols are not bound, because
/// no resolver maps a package designator to a project file yet, and a named
/// binding would leave those calls unresolved instead of letting project-wide
/// resolution find them.
fn visit_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
    forms: &[Node<'_>],
    shape: ImportShape,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    let arguments = forms.get(AFTER_HEAD..).unwrap_or_default();
    let packages = match shape {
        ImportShape::Packages => arguments,
        ImportShape::From => arguments.get(..1).unwrap_or_default(),
    };
    for package in packages {
        let Some(name) = designator(source, *package, Designator::Package) else {
            continue;
        };
        let name = builder.context.copy_text(name)?;
        emit_import_reference(builder, *package, name)?;
    }
    Ok(())
}

/// A `defun`, `defmacro`, `defgeneric`, or `defmethod` the grammar parsed as
/// a `defun` node; anonymous lambdas only walk their body.
pub(super) fn visit_defun(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    let header = named_children(node)
        .find(|child| child.kind() == "defun_header")
        .unwrap_or(node);
    let mut cursor = node.walk();
    let body: Vec<Node<'_>> = node.children_by_field_name("value", &mut cursor).collect();
    emit_function(
        builder,
        FunctionForm {
            node,
            name: header.child_by_field_name("function_name"),
            lambda_list: header.child_by_field_name("lambda_list"),
            body: &body,
            depth,
        },
    )?;
    Ok(true)
}

/// A `defun`-family list in a spelling the grammar does not recognize, such as
/// `(DEFUN NAME (PARAMS) BODY...)`: the name, any method qualifiers when
/// `qualified`, the lambda list, then the body.
fn visit_function_list(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: NamedForm<'_, '_>,
    qualified: bool,
) -> Result<(), ExtractError> {
    let NamedForm {
        node, forms, depth, ..
    } = input;
    let rest = forms.get(AFTER_NAME..).unwrap_or_default();
    // A method's qualifiers (`:before`, `+`, ...) are atoms, so its lambda list
    // is the first list (`NIL` included); any other definition's comes first.
    let source = builder.context.snapshot.source();
    let lambda_index = if qualified {
        rest.iter().position(|form| is_list_form(source, *form))
    } else {
        (!rest.is_empty()).then_some(0)
    };
    let lambda_list = lambda_index
        .and_then(|index| rest.get(index))
        .copied()
        .filter(|form| form.kind() == "list_lit");
    let body_start = lambda_index.map_or(rest.len(), |index| index.saturating_add(1));
    emit_function(
        builder,
        FunctionForm {
            node,
            name: forms.get(AFTER_HEAD).copied(),
            lambda_list,
            body: rest.get(body_start..).unwrap_or_default(),
            depth,
        },
    )
}

/// Whether a form is a list, including the empty list spelled `NIL`.
fn is_list_form(source: &str, form: Node<'_>) -> bool {
    form.kind() == "list_lit"
        || (form.kind() == "sym_lit" && source_text(source, form).eq_ignore_ascii_case(EMPTY_LIST))
}

/// One function definition, however the grammar parsed it.
#[derive(Clone, Copy)]
struct FunctionForm<'tree, 'forms> {
    /// The whole definition, which spans the emitted symbol.
    node: Node<'tree>,
    /// The name designator; a missing or non-symbol name declares nothing.
    name: Option<Node<'tree>>,
    lambda_list: Option<Node<'tree>>,
    /// The evaluated forms, visited inside the function's scope.
    body: &'forms [Node<'tree>],
    depth: usize,
}

/// Emit a function with its lambda-list signature and visit its body; an
/// unnamed definition only visits its body.
fn emit_function(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: FunctionForm<'_, '_>,
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    // Default-value forms run when the function is called, before its body.
    let mut evaluated = input
        .lambda_list
        .map(|list| lambda_list_defaults(source, list))
        .unwrap_or_default();
    evaluated.extend_from_slice(input.body);
    let Some(name) = input
        .name
        .and_then(|name| designator(source, name, Designator::Symbol))
    else {
        for form in &evaluated {
            builder.visit(*form, input.depth.saturating_add(1))?;
        }
        return Ok(());
    };
    let signature = match input.lambda_list {
        Some(parameters) if plain_lambda_list(source, parameters) => {
            literal_free_signature(builder, source_text(source, parameters))?
        }
        _ => None,
    };
    let name = builder.context.copy_text(name)?;
    let exported = builder.owners.is_empty();
    emit_scoped_declaration(
        builder,
        ScopedEmission {
            symbol: SymbolEmission {
                node: input.node,
                name,
                body: Some(input.node),
                signature,
                shape: DeclarationShape::plain(SymbolKind::Function, exported),
            },
            children: &evaluated,
            depth: input.depth,
        },
    )?;
    Ok(())
}

/// The default-value forms of a lambda list: everything after the variable
/// in a `(var init supplied-p)` parameter that follows `&optional`, `&key`, or
/// `&aux` (in any case). The variable itself, or a `((:key var) ...)` pair,
/// only names a binding.
fn lambda_list_defaults<'tree>(source: &str, list: Node<'tree>) -> Vec<Node<'tree>> {
    if list.kind() != "list_lit" {
        return Vec::new();
    }
    let mut defaults = false;
    let mut forms = Vec::new();
    for parameter in form_children(list) {
        match parameter.kind() {
            "sym_lit" => {
                if let Some(keyword) = lambda_keyword(source_text(source, parameter)) {
                    defaults = keyword;
                }
            }
            "list_lit" if defaults => {
                forms.extend(form_children(parameter).into_iter().skip(AFTER_HEAD));
            }
            _ => {}
        }
    }
    forms
}

/// For a lambda-list keyword, whether the parameters after it may carry
/// default values; `None` for an ordinary parameter name.
fn lambda_keyword(text: &str) -> Option<bool> {
    text.starts_with('&').then(|| {
        DEFAULTING_LAMBDA_KEYWORDS
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(text))
    })
}

/// Whether a lambda list names only parameters and specializers.
///
/// A list after `&optional`, `&key`, or `&aux` (in any case) carries a default
/// value, which may be a literal, so such lambda lists are never retained as
/// signatures.
fn plain_lambda_list(source: &str, list: Node<'_>) -> bool {
    if list.kind() != "list_lit" || list.byte_range().len() > MAX_RETAINED_TEXT_BYTES {
        return false;
    }
    let mut defaults = false;
    for parameter in form_children(list) {
        match parameter.kind() {
            "sym_lit" => {
                if let Some(keyword) = lambda_keyword(source_text(source, parameter)) {
                    defaults = keyword;
                }
            }
            "list_lit"
                if !defaults
                    && form_children(parameter)
                        .iter()
                        .all(|part| part.kind() == "sym_lit") => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn designators_lose_reader_prefixes_and_escapes_keep_their_spelling() {
        assert_eq!(clean_designator("#:demo.core"), Some("demo.core"));
        assert_eq!(clean_designator(":cl"), Some("cl"));
        assert_eq!(clean_designator("*default-name*"), Some("*default-name*"));
        assert_eq!(clean_designator("|escaped|"), Some("escaped"));
        assert_eq!(escaped_symbol("|Weird Name|"), Some("Weird Name"));
        assert_eq!(escaped_symbol("|  |"), None);
        assert_eq!(escaped_symbol("|a\nb|"), None);
        assert_eq!(escaped_symbol("plain"), None);
    }
}
