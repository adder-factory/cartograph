//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::assert_matches;

use cartograph_domain::{ReferenceKind, SourceLanguage};
use cartograph_extract::{
    DYNAMIC_DISPATCH_RESOLUTION_PREFIX, ExtractedFile, NativeExtractor, SourceLimits,
    SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn object_and_map_dispatch_tables_emit_bounded_source_exact_target_references() {
    let source = r"
function startHandler() {}
function stopHandler() {}
function openHandler() {}
function closeHandler() {}

const HANDLERS = {
  start: startHandler,
  stopHandler,
  ignored: external.member,
};
const ACTIONS = new Map([
  ['open', openHandler],
  ['close', closeHandler],
]);

export function dispatch(kind: string) {
  HANDLERS[kind]?.();
  ACTIONS.get(kind)?.();
}
";
    let first = extract("src/dispatch.ts", source);
    let second = extract("src/dispatch.ts", source);
    assert_eq!(first, second);

    for target in ["startHandler", "stopHandler", "openHandler", "closeHandler"] {
        let reference = first
            .references
            .iter()
            .find(|reference| {
                reference.owner.is_none()
                    && reference.kind == ReferenceKind::Calls
                    && reference.name == target
                    && reference.resolution_name.as_deref()
                        == Some(&format!("{DYNAMIC_DISPATCH_RESOLUTION_PREFIX}{target}"))
            })
            .unwrap_or_else(|| panic!("missing dispatch target {target}: {first:?}"));
        let start = usize::try_from(reference.span.start_byte())
            .unwrap_or_else(|error| panic!("dispatch span start overflowed: {error}"));
        let end = usize::try_from(reference.span.end_byte())
            .unwrap_or_else(|error| panic!("dispatch span end overflowed: {error}"));
        assert_eq!(&source[start..end], target);
    }
    assert!(first.references.iter().all(|reference| {
        reference.name != "member"
            || !reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.starts_with(DYNAMIC_DISPATCH_RESOLUTION_PREFIX))
    }));
}

#[test]
fn unused_and_overwide_tables_abstain_while_literal_member_calls_stay_explicit() {
    let source = r"
function one() {}
function two() {}
function three() {}
function four() {}
function five() {}
function six() {}
function seven() {}
function eight() {}
function nine() {}
function ten() {}
function eleven() {}
const UNUSED = { one };
const WRONG_KIND = new Map([['one', one]]);
const WIDE = { one, two, three, four, five, six, seven, eight, nine, ten, eleven };
UNUSED;
WRONG_KIND['one']();
WIDE['one']();
";
    let extracted = extract("src/negative.ts", source);
    let dynamic = extracted
        .references
        .iter()
        .filter(|reference| {
            reference
                .resolution_name
                .as_deref()
                .is_some_and(|name| name.starts_with(DYNAMIC_DISPATCH_RESOLUTION_PREFIX))
        })
        .collect::<Vec<_>>();
    assert_eq!(dynamic.len(), 2);
    assert!(dynamic.iter().all(|reference| {
        reference.name == "one"
            && reference.resolution_name.as_deref()
                == Some(&format!("{DYNAMIC_DISPATCH_RESOLUTION_PREFIX}one"))
    }));
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("dispatch snapshot failed: {error}"));
    assert_matches!(
        snapshot.language(),
        SourceLanguage::TypeScript
            | SourceLanguage::Tsx
            | SourceLanguage::JavaScript
            | SourceLanguage::Jsx
    );
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("dispatch extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("dispatch extraction failed: {error}"))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("dispatch source limit failed: {error}"))
}
