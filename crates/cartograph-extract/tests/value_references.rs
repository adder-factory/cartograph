//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn callback_object_array_jsx_and_ternary_values_emit_exact_static_references() {
    let source = r"
export function saveHandler() {}
export function cancelHandler() {}
function configure(value: unknown) { return value; }

export function wire(pretty: boolean) {
  configure(saveHandler);
  const options = { onSave: saveHandler, cancelHandler };
  const steps = [saveHandler, cancelHandler];
  const form = <form onSubmit={saveHandler} />;
  (pretty ? saveHandler : cancelHandler)();
  return { options, steps, form };
}
";
    let first = extract("src/wire.tsx", source);
    let second = extract("src/wire.tsx", source);
    assert_eq!(first, second);

    let wire = symbol(&first, SymbolKind::Function, "wire");
    for (target, minimum_sites) in [("saveHandler", 5), ("cancelHandler", 3)] {
        let target_symbol = symbol(&first, SymbolKind::Function, target);
        let references = first
            .references
            .iter()
            .filter(|reference| {
                reference.kind == ReferenceKind::References
                    && reference.name == target
                    && reference.owner.as_ref().is_some_and(|owner| {
                        owner == &wire.id
                            || first.containments.iter().any(|containment| {
                                containment.parent == wire.id && &containment.child == owner
                            })
                    })
            })
            .collect::<Vec<_>>();
        assert!(
            references.len() >= minimum_sites,
            "missing value positions for {target}: {:?}",
            first.references
        );
        for reference in references {
            let start = usize::try_from(reference.span.start_byte())
                .unwrap_or_else(|error| panic!("value-ref start overflowed: {error}"));
            let end = usize::try_from(reference.span.end_byte())
                .unwrap_or_else(|error| panic!("value-ref end overflowed: {error}"));
            assert_eq!(&source[start..end], target);
        }
        assert_ne!(wire.id, target_symbol.id);
    }
}

#[test]
fn member_values_strings_and_ambiguous_same_file_names_do_not_invent_value_edges() {
    // `duplicated` names two top-level bindings, so the read in `wire`
    // cannot choose one. Neither a class member namesake nor another scope's
    // local would make it ambiguous: the nearest enclosing scope decides.
    let source = r"
function duplicated() {}
var duplicated = 1;
function uniqueHandler() {}
function wire() {
  configure(obj.uniqueHandler);
  configure('uniqueHandler');
  configure(duplicated);
}
";
    let extracted = extract("src/negative.ts", source);
    assert!(
        extracted.references.iter().all(|reference| {
            reference.kind != ReferenceKind::References
                || !matches!(reference.name.as_str(), "uniqueHandler" | "duplicated")
        }),
        "invented ambiguous/member/string value reference: {:?}",
        extracted.references
    );
}

#[test]
fn nested_functions_passed_as_values_survive_namesakes_in_other_scopes() {
    // No top-level binding names `toggle`, `handler`, `later`, `Widget` or
    // `pick`; each is bound in several nested scopes. A read whose nearest
    // binding is a function or class its own scope declares, hoisted or not,
    // names that declaration (v1.1.33 recorded the `[value, toggle]` hook
    // result as a value reference) and carries its qualified name, so
    // resolution cannot bind a same-named file-level candidate instead. A
    // read whose nearest binding is a plain local, a function only a nested
    // scope declares, or one of two same-scope declarations names nothing.
    let source = r"
export function useToggle(initial: boolean) {
  const [value, setValue] = useState(initial);
  const toggle = () => setValue(!value);
  return [value, toggle];
}
export function Button() {
  const [open, toggle] = useToggle(false);
  return consume(open, toggle);
}
function outer() {
  const handler = 1;
  function inner() { function handler() {} return handler; }
  return consume([handler, inner]);
}
function other() {
  function handler() {}
  return [handler];
}
function hoisted() {
  register([later]);
  function later() {}
}
function shadowed() { const later = 0; const Widget = 1; return later + Widget; }
function factory() {
  class Widget {}
  return [Widget];
}
function sibling(flag: boolean) {
  if (flag) { const pick = () => 1; register([pick]); } else { const pick = () => 2; }
}
";
    let first = extract("src/components/Button.tsx", source);
    assert_eq!(first, extract("src/components/Button.tsx", source));
    let value_reads = |owner: &str, name: &str| {
        let owner = &symbol(&first, SymbolKind::Function, owner).id;
        first
            .references
            .iter()
            .filter(|reference| {
                reference.kind == ReferenceKind::References
                    && reference.name == name
                    && reference.owner.as_ref() == Some(owner)
            })
            .map(|reference| reference.resolution_name.as_deref())
            .collect::<Vec<_>>()
    };
    for (owner, name, identity) in [
        ("useToggle", "toggle", "useToggle::toggle"),
        ("other", "handler", "other::handler"),
        ("hoisted", "later", "hoisted::later"),
        ("factory", "Widget", "factory::Widget"),
    ] {
        assert_eq!(
            value_reads(owner, name),
            [Some(identity)],
            "{owner} -> {name}: {:?}",
            first.references
        );
    }
    for (owner, name) in [
        ("Button", "toggle"),
        ("outer", "handler"),
        ("sibling", "pick"),
    ] {
        assert!(
            value_reads(owner, name).is_empty(),
            "{owner} recorded a read of {name}: {:?}",
            first.references
        );
    }
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("value-ref snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("value-ref extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("value-ref extraction failed: {error}"))
}

fn symbol<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("missing {kind:?} {qualified_name}: {:?}", file.symbols))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("value-ref source limit failed: {error}"))
}
