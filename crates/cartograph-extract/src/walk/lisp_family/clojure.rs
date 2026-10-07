//! Clojure list forms: `ns` declarations and requires, `def`-family
//! definitions, local function bindings, and list-head calls.

use cartograph_domain::{ReferenceKind, SymbolKind, Visibility};
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    super::{
        ExtractionBuilder,
        family_support::{
            DeclarationShape, MAX_RETAINED_TEXT_BYTES, OwnedReference, ScopedEmission,
            SymbolEmission, bounded_name, emit_declaration, emit_import_reference,
            emit_owned_reference, emit_scoped_declaration, literal_free_signature, source_text,
        },
        syntax::{descendants_including_root, named_children},
    },
    AFTER_HEAD, AFTER_NAME, form_children,
};

/// Clojure list heads that bind or are special forms rather than calls.
const NON_CALL_HEADS: &[&str] = &[
    ".",
    "catch",
    "def",
    "do",
    "finally",
    "fn",
    "fn*",
    "if",
    "let",
    "let*",
    "loop",
    "loop*",
    "monitor-enter",
    "monitor-exit",
    "new",
    "recur",
    "set!",
    "throw",
    "try",
    "var",
];

/// Qualifier naming `clojure.core`, whose operators a head may spell in full
/// (`clojure.core/defn`) without changing meaning.
const CORE_NAMESPACE_PREFIX: &str = "clojure.core/";
/// Most multi-arity parameter vectors folded into one signature.
const MAX_ARITIES: usize = 16;
/// Prefix of the implicit argument names of a `#(...)` anonymous function.
const ANONYMOUS_ARGUMENT_PREFIX: char = '%';
/// Suffix of the implicit rest argument, `%&`.
const ANONYMOUS_REST_SUFFIX: &str = "&";

/// What a Clojure list form does, decided by its head symbol.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Namespace,
    Definition(SymbolKind),
    LocalFunctions,
    Data,
    NonCall,
    Call,
}

/// Classify a list by its head symbol.
fn form(head: &str) -> Form {
    match head {
        "ns" => Form::Namespace,
        "defn" | "defn-" | "defmacro" => Form::Definition(SymbolKind::Function),
        "def" | "defonce" => Form::Definition(SymbolKind::Constant),
        "defrecord" | "deftype" => Form::Definition(SymbolKind::Class),
        "defprotocol" => Form::Definition(SymbolKind::Interface),
        "letfn" => Form::LocalFunctions,
        "comment" | "quote" => Form::Data,
        _ if NON_CALL_HEADS.contains(&head) => Form::NonCall,
        _ => Form::Call,
    }
}

/// Whether `head` is an implicit `#(...)` argument: `%`, `%&`, or `%N` (an
/// empty digit run is the bare `%`).
/// Any other symbol starting with `%`, such as `%helper`, is an ordinary name.
fn is_anonymous_argument(head: &str) -> bool {
    head.strip_prefix(ANONYMOUS_ARGUMENT_PREFIX)
        .is_some_and(|suffix| {
            suffix == ANONYMOUS_REST_SUFFIX || suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Whether `node` lies inside a `#(...)` anonymous function. The walk is
/// bounded by the tree depth the walker already accepted.
fn within_anonymous_function(node: Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(ancestor) = current {
        if ancestor.kind() == "anon_fn_lit" {
            return true;
        }
        current = ancestor.parent();
    }
    false
}

/// Symbol text without reader metadata, keeping any `ns/` qualifier.
fn symbol<'source>(source: &'source str, node: Node<'_>) -> Option<&'source str> {
    if node.kind() != "sym_lit" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    let start = node
        .child_by_field_name("namespace")
        .unwrap_or(name)
        .start_byte();
    source.get(start..name.end_byte()).and_then(bounded_name)
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
        .and_then(|head| symbol(source, *head).map(|name| (head, name)))
    else {
        return Ok(false);
    };
    // An implicit argument is a parameter only inside `#(...)`; elsewhere `%`
    // is an ordinary symbol. A call keeps the head as written; only the
    // operator classification ignores a `clojure.core/` qualifier.
    let operator = head.strip_prefix(CORE_NAMESPACE_PREFIX).unwrap_or(head);
    let form = if is_anonymous_argument(head) && within_anonymous_function(node) {
        Form::NonCall
    } else {
        form(operator)
    };
    match form {
        Form::Namespace => {
            visit_namespace(builder, node, &forms)?;
            Ok(true)
        }
        Form::Definition(kind) => visit_definition(
            builder,
            Definition {
                node,
                forms: &forms,
                private_head: operator == "defn-",
                kind,
                depth,
            },
        ),
        Form::LocalFunctions => {
            visit_local_functions(builder, &forms, depth)?;
            Ok(true)
        }
        Form::Data => Ok(true),
        Form::NonCall => Ok(false),
        Form::Call => {
            recursion::capture(builder, (node, head_node, head))?;
            emit_owned_reference(
                builder,
                OwnedReference {
                    name: head,
                    kind: ReferenceKind::Calls,
                    node: head_node,
                },
            )?;
            Ok(false)
        }
    }
}

/// An `ns` form: the namespace declaration and its `:require` imports.
fn visit_namespace(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    forms: &[Node<'_>],
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    if let Some(name) = forms.get(AFTER_HEAD).and_then(|name| symbol(source, *name)) {
        let name = builder.context.copy_text(name)?;
        emit_declaration(
            builder,
            SymbolEmission {
                node,
                name,
                body: None,
                signature: None,
                shape: DeclarationShape::plain(SymbolKind::Namespace, true),
            },
        )?;
    }
    for clause in forms.iter().skip(AFTER_NAME) {
        let clause_forms = form_children(*clause);
        let requires = clause.kind() == "list_lit"
            && clause_forms.first().is_some_and(|head| {
                head.kind() == "kwd_lit" && source_text(source, *head) == ":require"
            });
        if requires {
            visit_requires(builder, &clause_forms)?;
        }
    }
    Ok(())
}

/// Every library a `(:require ...)` clause loads.
///
/// `:refer [names]` binds nothing: no resolver maps a Clojure namespace to a
/// project file yet, so a named binding would leave the referred calls
/// unresolved instead of letting project-wide resolution find them.
fn visit_requires(
    builder: &mut ExtractionBuilder<'_, '_>,
    clause_forms: &[Node<'_>],
) -> Result<(), ExtractError> {
    let source = builder.context.snapshot.source();
    for spec in clause_forms.iter().skip(AFTER_HEAD) {
        let name = match spec.kind() {
            "vec_lit" => required_namespace(builder, *spec)?,
            _ => symbol(source, *spec)
                .map(|name| builder.context.copy_text(name))
                .transpose()?,
        };
        let Some(name) = name else {
            continue;
        };
        emit_import_reference(builder, *spec, name)?;
    }
    Ok(())
}

/// Namespace named by a `[ns.name :as alias]` libspec, joining a `prefix.` symbol
/// with the symbol that follows it.
fn required_namespace(
    builder: &ExtractionBuilder<'_, '_>,
    spec: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let mut symbols = named_children(spec).filter_map(|child| symbol(source, child));
    let Some(first) = symbols.next() else {
        return Ok(None);
    };
    let suffix = first.ends_with('.').then(|| symbols.next()).flatten();
    let length = first.len().saturating_add(suffix.map_or(0, str::len));
    if length > MAX_RETAINED_TEXT_BYTES {
        return Ok(None);
    }
    let mut name = builder.context.copy_text(first)?;
    if let Some(suffix) = suffix {
        name.try_reserve(suffix.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        name.push_str(suffix);
    }
    Ok(Some(name))
}

/// `(letfn [(name [params] body...)...] body...)` binds local functions whose
/// names and parameters are not calls; their bodies and the form body are.
fn visit_local_functions(
    builder: &mut ExtractionBuilder<'_, '_>,
    forms: &[Node<'_>],
    depth: usize,
) -> Result<(), ExtractError> {
    let child_depth = depth.saturating_add(1);
    let bindings = forms
        .get(AFTER_HEAD)
        .filter(|bindings| bindings.kind() == "vec_lit")
        .map(|bindings| form_children(*bindings))
        .unwrap_or_default();
    for binding in bindings
        .iter()
        .filter(|binding| binding.kind() == "list_lit")
    {
        for form in local_function_bodies(*binding) {
            builder.visit(form, child_depth)?;
        }
    }
    for form in forms.iter().skip(AFTER_NAME) {
        builder.visit(*form, child_depth)?;
    }
    Ok(())
}

/// The evaluated body forms of one `letfn` binding: after `name [params]`
/// for a single arity, or after each arity's parameter vector otherwise.
fn local_function_bodies(binding: Node<'_>) -> Vec<Node<'_>> {
    let parts = form_children(binding);
    if parts
        .get(AFTER_HEAD)
        .is_some_and(|parameters| parameters.kind() == "vec_lit")
    {
        return parts.into_iter().skip(AFTER_NAME).collect();
    }
    parts
        .into_iter()
        .skip(AFTER_HEAD)
        .filter(|arity| arity.kind() == "list_lit")
        .flat_map(|arity| form_children(arity).into_iter().skip(AFTER_HEAD))
        .collect()
}

/// One `def`-family form and the context needed to emit it.
#[derive(Clone, Copy)]
struct Definition<'tree, 'forms> {
    node: Node<'tree>,
    forms: &'forms [Node<'tree>],
    private_head: bool,
    kind: SymbolKind,
    depth: usize,
}

/// Emit a definition and visit the forms it evaluates.
fn visit_definition(
    builder: &mut ExtractionBuilder<'_, '_>,
    input: Definition<'_, '_>,
) -> Result<bool, ExtractError> {
    let source = builder.context.snapshot.source();
    let Some((&name_node, name)) = input
        .forms
        .get(AFTER_HEAD)
        .and_then(|node| symbol(source, *node).map(|name| (node, name)))
    else {
        return Ok(false);
    };
    let private = input.private_head || has_private_metadata(source, name_node);
    let rest = input.forms.get(AFTER_NAME..).unwrap_or_default();
    let signature = if input.kind == SymbolKind::Function {
        signature(builder, rest)?
    } else {
        None
    };
    let name = builder.context.copy_text(name)?;
    let children = scope_children(input.kind, rest);
    // A definition nested in another one is local to it, never a namespace export.
    let exported = !private && builder.owners.is_empty();
    emit_scoped_declaration(
        builder,
        ScopedEmission {
            symbol: SymbolEmission {
                node: input.node,
                name,
                body: Some(input.node),
                signature,
                shape: DeclarationShape {
                    kind: input.kind,
                    exported,
                    visibility: Some(if private {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    }),
                    async_symbol: false,
                    declaration_only: input.kind == SymbolKind::Interface,
                },
            },
            children: &children,
            depth: input.depth,
        },
    )?;
    Ok(true)
}

/// Forms evaluated inside a definition. A protocol only declares method
/// signatures; a record or type's method implementations are `(name [params]
/// body...)` lists whose name and parameters are not calls.
fn scope_children<'tree>(kind: SymbolKind, rest: &[Node<'tree>]) -> Vec<Node<'tree>> {
    match kind {
        SymbolKind::Interface => Vec::new(),
        SymbolKind::Class => rest
            .iter()
            .filter(|form| form.kind() == "list_lit")
            .flat_map(|method| form_children(*method).into_iter().skip(AFTER_NAME))
            .collect(),
        _ => rest.to_vec(),
    }
}

/// Whether a definition name carries `^:private` or `{:private true}` metadata.
fn has_private_metadata(source: &str, name: Node<'_>) -> bool {
    named_children(name)
        .filter(|child| matches!(child.kind(), "meta_lit" | "old_meta_lit"))
        .filter_map(|meta| named_children(meta).next())
        .any(|value| match value.kind() {
            "kwd_lit" => source_text(source, value) == ":private",
            "map_lit" => map_marks_private(source, value),
            _ => false,
        })
}

/// Whether a metadata map holds the entry `:private true`.
fn map_marks_private(source: &str, map: Node<'_>) -> bool {
    let entries = form_children(map);
    let (entries, _) = entries.as_chunks::<2>();
    entries.iter().any(|[key, value]| {
        source_text(source, *key) == ":private" && source_text(source, *value) == "true"
    })
}

/// Parameter vector of a single-arity definition, or every arity's vector.
fn signature(
    builder: &ExtractionBuilder<'_, '_>,
    rest: &[Node<'_>],
) -> Result<Option<String>, ExtractError> {
    let source = builder.context.snapshot.source();
    let vectors: Vec<Node<'_>> = match rest.iter().find(|form| form.kind() == "vec_lit") {
        Some(vector) => vec![*vector],
        None => rest
            .iter()
            .filter(|form| form.kind() == "list_lit")
            .filter_map(|arity| {
                form_children(*arity)
                    .first()
                    .copied()
                    .filter(|parameters| parameters.kind() == "vec_lit")
            })
            .take(MAX_ARITIES)
            .collect(),
    };
    let length = vectors
        .iter()
        .map(|vector| vector.byte_range().len().saturating_add(1))
        .fold(0_usize, usize::saturating_add);
    if vectors.is_empty()
        || length > MAX_RETAINED_TEXT_BYTES
        || !vectors.iter().all(|vector| binds_only_symbols(*vector))
    {
        return Ok(None);
    }
    let parts: Vec<&str> = vectors
        .iter()
        .map(|vector| source_text(source, *vector))
        .collect();
    literal_free_signature(builder, &parts.join(" "))
}

/// Whether a parameter vector binds only symbols, possibly in nested vectors.
///
/// Map destructuring, metadata, and defaults can carry literal values, so a
/// vector containing anything else is never retained as a signature.
fn binds_only_symbols(vector: Node<'_>) -> bool {
    descendants_including_root(vector)
        .filter(Node::is_named)
        .all(|node| matches!(node.kind(), "vec_lit" | "sym_lit" | "sym_name" | "sym_ns"))
}
mod recursion;
