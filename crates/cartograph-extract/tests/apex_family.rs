//! Salesforce Apex extraction contracts (v1 `apex` extractor parity).

mod dependency_ownership;

use std::collections::BTreeSet;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind, Visibility};
use cartograph_extract::{
    ExtractError, ExtractedFile, ExtractedSymbol, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
const LITERAL_SENTINEL: &str = "apex_literal_sentinel_91be";

#[test]
fn apex_classes_keep_v1_annotations_returns_and_signatures() {
    // v1 __tests__/salesforce.test.ts: decorators contain AuraEnabled and the
    // `List<Account>` return type yields `returns Account`.
    let extracted = extract(
        "force-app/main/default/classes/AccountService.cls",
        "public with sharing class AccountService { @AuraEnabled(cacheable=true) public static List<Account> listAccounts() { return new List<Account>(); } }",
    );
    let class = symbol(&extracted, SymbolKind::Class, "AccountService");
    assert_eq!(class.visibility, Some(Visibility::Public));
    let method = symbol(
        &extracted,
        SymbolKind::Method,
        "AccountService::listAccounts",
    );
    assert!(method.execution.static_member);
    assert_eq!(method.signature.as_deref(), Some("List<Account> ()"));
    assert_reference(
        &extracted,
        method,
        ("AuraEnabled", ReferenceKind::Decorates),
    );
    assert_reference(&extracted, method, ("Account", ReferenceKind::Returns));
}

#[test]
fn apex_constructors_fields_enums_calls_inheritance_and_queries() {
    let source = format!(
        "public with sharing class AccountService extends BaseService implements Queueable, Database.Batchable<SObject> {{\n    public enum Mode {{ FAST, SLOW }}\n    Integer a, b;\n    global AccountService() {{}}\n    PUBLIC STATIC List<Account> load(String name) {{\n        List<Account> rows = [SELECT Id, (SELECT Id FROM Contacts) FROM Account WHERE Name = :name];\n        List<List<SObject>> found = [FIND '{LITERAL_SENTINEL}' IN ALL FIELDS RETURNING Contact, Lead];\n        other.run();\n        helper();\n        return rows;\n    }}\n    private void hidden() {{ Integer local = 1; }}\n}}\n"
    );
    let extracted = extract("force-app/main/default/classes/AccountService.cls", &source);
    let class = symbol(&extracted, SymbolKind::Class, "AccountService");
    assert_reference(&extracted, class, ("BaseService", ReferenceKind::Extends));
    assert_reference(&extracted, class, ("Queueable", ReferenceKind::Implements));
    assert_reference(
        &extracted,
        class,
        ("Database.Batchable", ReferenceKind::Implements),
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.name != "SObject"
                || reference.kind != ReferenceKind::Implements),
        "generic argument became an implemented interface"
    );

    symbol(&extracted, SymbolKind::Enum, "AccountService::Mode");
    symbol(
        &extracted,
        SymbolKind::EnumMember,
        "AccountService::Mode::FAST",
    );
    symbol(
        &extracted,
        SymbolKind::EnumMember,
        "AccountService::Mode::SLOW",
    );
    symbol(&extracted, SymbolKind::Field, "AccountService::a");
    symbol(&extracted, SymbolKind::Field, "AccountService::b");
    let constructor = symbol(
        &extracted,
        SymbolKind::Method,
        "AccountService::AccountService",
    );
    assert_eq!(constructor.visibility, Some(Visibility::Public));
    assert_eq!(constructor.signature.as_deref(), Some("()"));

    let load = symbol(&extracted, SymbolKind::Method, "AccountService::load");
    assert_eq!(load.visibility, Some(Visibility::Public));
    assert!(load.execution.static_member);
    assert_eq!(
        load.signature.as_deref(),
        Some("List<Account> (String name)")
    );
    assert_reference(&extracted, load, ("other.run", ReferenceKind::Calls));
    assert_reference(&extracted, load, ("helper", ReferenceKind::Calls));
    for object in ["Account", "Contacts", "Contact", "Lead"] {
        assert_reference(&extracted, load, (object, ReferenceKind::References));
    }
    let account_references = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.name == "Account" && reference.kind == ReferenceKind::References
        })
        .count();
    assert_eq!(
        account_references, 1,
        "one query object reference per query"
    );
    let hidden = symbol(&extracted, SymbolKind::Method, "AccountService::hidden");
    assert_eq!(hidden.visibility, Some(Visibility::Private));
    symbol(
        &extracted,
        SymbolKind::Variable,
        "AccountService::hidden::local",
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Property),
        "Apex fields must be fields, not properties"
    );
    assert!(!format!("{extracted:?}").contains(LITERAL_SENTINEL));
}

#[test]
fn apex_triggers_are_functions_with_object_and_event_signatures() {
    let extracted = extract(
        "force-app/main/default/triggers/AccountTrigger.trigger",
        "trigger AccountTrigger on Account (before insert, after update) {\n    AccountService.load('x');\n    for (Contact c : [SELECT Id FROM Contact]) {}\n}\n",
    );
    let trigger = symbol(&extracted, SymbolKind::Function, "AccountTrigger");
    assert_eq!(
        trigger.signature.as_deref(),
        Some("on Account (before insert, after update)")
    );
    assert!(trigger.export.exported);
    assert_reference(
        &extracted,
        trigger,
        ("AccountService.load", ReferenceKind::Calls),
    );
    assert_reference(&extracted, trigger, ("Contact", ReferenceKind::References));
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.owner.is_some()),
        "trigger body references must be owned by the trigger"
    );
}

#[test]
fn apex_property_accessor_bodies_keep_their_calls_and_queries() {
    let extracted = extract(
        "force-app/main/default/classes/Prop.cls",
        "public class Prop {\n    public Account row { get { return load([SELECT Id FROM Account]); } set { save(value); } }\n    public Integer count { get; private set; }\n}\n",
    );
    let row = symbol(&extracted, SymbolKind::Field, "Prop::row");
    assert_reference(&extracted, row, ("load", ReferenceKind::Calls));
    assert_reference(&extracted, row, ("save", ReferenceKind::Calls));
    assert_reference(&extracted, row, ("Account", ReferenceKind::References));
    symbol(&extracted, SymbolKind::Field, "Prop::count");
}

#[test]
fn apex_extraction_is_repeatable_and_cancellable() {
    let source = "public class Repeat { public void run() { helper(); } }\n";
    let path = "force-app/main/default/classes/Repeat.cls";
    assert_eq!(extract(path, source), extract(path, source));
    let snapshot = snapshot(path, source);
    let mut extractor = NativeExtractor::new(SourceLanguage::Apex)
        .unwrap_or_else(|error| panic!("Apex extractor failed: {error}"));
    assert_eq!(
        extractor
            .extract_with_cancellation(&snapshot, || true)
            .err(),
        Some(ExtractError::Cancelled)
    );
    let names = extract(path, source)
        .symbols
        .iter()
        .map(|symbol| symbol.qualified_name.clone())
        .collect::<BTreeSet<_>>();
    assert!(names.contains("Repeat::run"), "{names:?}");
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = snapshot(path, source);
    assert_eq!(snapshot.language(), SourceLanguage::Apex);
    NativeExtractor::new(SourceLanguage::Apex)
        .unwrap_or_else(|error| panic!("Apex extractor failed: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("Apex extraction failed for {path}: {error}"))
}

fn snapshot(path: &str, source: &str) -> SourceSnapshot {
    let limits = SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("source limits failed: {error}"));
    SourceSnapshot::from_bytes(path, source.as_bytes(), limits)
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"))
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name}; symbols={:?}",
                extracted
                    .symbols
                    .iter()
                    .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
                    .collect::<Vec<_>>()
            )
        })
}

fn assert_reference(
    extracted: &ExtractedFile,
    owner: &ExtractedSymbol,
    (name, kind): (&str, ReferenceKind),
) {
    assert!(
        extracted.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&owner.id)
                && reference.name == name
                && reference.kind == kind
        }),
        "missing {kind:?} {name} owned by {}; references={:?}",
        owner.qualified_name,
        extracted
            .references
            .iter()
            .map(|reference| (reference.kind, reference.name.as_str()))
            .collect::<Vec<_>>()
    );
}
