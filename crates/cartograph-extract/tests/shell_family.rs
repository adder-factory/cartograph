//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::collections::{BTreeMap, BTreeSet};

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractError, ExtractedFile, NativeExtractor, SnapshotError, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn bash_extracts_functions_variables_constants_imports_and_owned_calls_safely() {
    let extracted = extract_fixture(SourceLanguage::Bash);

    assert_eq!(extracted.language, SourceLanguage::Bash);
    assert_clean(&extracted);
    assert_eq!(extracted.symbols.len(), 7);
    assert_symbol(
        &extracted,
        SymbolKind::Import,
        "./lib/common.sh",
        "./lib/common.sh",
    );
    assert_symbol(
        &extracted,
        SymbolKind::Constant,
        "MAX_RETRIES",
        "MAX_RETRIES",
    );
    assert_symbol(
        &extracted,
        SymbolKind::Variable,
        "PUBLIC_LIMIT",
        "PUBLIC_LIMIT",
    );
    assert_eq!(signature(&extracted, "MAX_RETRIES"), Some("= OTHER_LIMIT"));
    assert_eq!(
        signature(&extracted, "PUBLIC_LIMIT"),
        Some("= $DEFAULT_LIMIT")
    );
    assert!(symbol(&extracted, "PUBLIC_LIMIT").export.exported);
    assert_eq!(signature(&extracted, "API_TOKEN"), None);
    assert_eq!(signature(&extracted, "LITERAL_COUNT"), None);
    assert_symbol(&extracted, SymbolKind::Function, "helper", "helper");
    assert_symbol(&extracted, SymbolKind::Function, "run", "run");
    assert_eq!(signature(&extracted, "helper"), Some("helper()"));
    assert_eq!(signature(&extracted, "run"), Some("run()"));
    assert_reference(&extracted, None, "./lib/common.sh", ReferenceKind::Imports);
    assert_reference(&extracted, Some("helper"), "printf", ReferenceKind::Calls);
    assert_reference(&extracted, Some("run"), "helper", ReferenceKind::Calls);
    assert_reference(&extracted, Some("run"), "curl", ReferenceKind::Calls);
    assert_reference(&extracted, None, "run", ReferenceKind::Calls);
    assert_single_literal_import(&extracted, "./lib/common.sh");
}

#[test]
fn zsh_extracts_functions_constants_imports_and_owned_calls_safely() {
    let extracted = extract_fixture(SourceLanguage::Zsh);

    assert_eq!(extracted.language, SourceLanguage::Zsh);
    assert_clean(&extracted);
    assert_eq!(extracted.symbols.len(), 6);
    assert_symbol(
        &extracted,
        SymbolKind::Import,
        "./lib/common.zsh",
        "./lib/common.zsh",
    );
    assert_symbol(&extracted, SymbolKind::Constant, "ZSH_LIMIT", "ZSH_LIMIT");
    assert_eq!(signature(&extracted, "ZSH_LIMIT"), Some("= OTHER_LIMIT"));
    assert_eq!(
        signature(&extracted, "ZSH_PUBLIC"),
        Some("= $DEFAULT_LIMIT")
    );
    assert_eq!(signature(&extracted, "ZSH_TOKEN"), None);
    assert_symbol(&extracted, SymbolKind::Function, "zhelper", "zhelper");
    assert_symbol(&extracted, SymbolKind::Function, "zrun", "zrun");
    assert_eq!(signature(&extracted, "zhelper"), Some("zhelper()"));
    assert_eq!(signature(&extracted, "zrun"), Some("zrun()"));
    assert_reference(&extracted, None, "./lib/common.zsh", ReferenceKind::Imports);
    assert_reference(&extracted, Some("zhelper"), "print", ReferenceKind::Calls);
    assert_reference(&extracted, Some("zrun"), "zhelper", ReferenceKind::Calls);
    assert_reference(&extracted, None, "zrun", ReferenceKind::Calls);
    assert_single_literal_import(&extracted, "./lib/common.zsh");
}

#[test]
fn command_substitutions_in_assignments_call_their_commands_without_declaring_locals() {
    for (path, language) in [
        ("plugins/sync.plugin.zsh", SourceLanguage::Zsh),
        ("scripts/sync.sh", SourceLanguage::Bash),
    ] {
        let extracted = extract(
            path,
            r#"current_branch() {
  git rev-parse --abbrev-ref HEAD
}

sync() {
  local branch=$(current_branch)
  remote="$(default_remote)"
  export SYNC_TARGET=$(resolve_target "$branch")
}

STARTED=$(start_clock)
"#,
        );
        assert_eq!(extracted.language, language);
        assert_clean(&extracted);
        assert_reference(
            &extracted,
            Some("sync"),
            "current_branch",
            ReferenceKind::Calls,
        );
        assert_reference(
            &extracted,
            Some("sync"),
            "default_remote",
            ReferenceKind::Calls,
        );
        assert_reference(
            &extracted,
            Some("sync"),
            "resolve_target",
            ReferenceKind::Calls,
        );
        assert_reference(&extracted, None, "start_clock", ReferenceKind::Calls);
        let call_line = extracted
            .references
            .iter()
            .find(|reference| reference.name == "current_branch")
            .map(|reference| reference.span.start_line());
        assert_eq!(
            call_line,
            Some(6),
            "{language:?} call is anchored on its line"
        );
        assert!(
            extracted
                .symbols
                .iter()
                .all(|symbol| symbol.name != "branch"),
            "{language:?} declared a function-local variable: {:?}",
            extracted.symbols
        );
        assert_symbol(&extracted, SymbolKind::Variable, "remote", "sync::remote");
    }
}

#[test]
fn dynamic_command_names_abstain_without_leaking_argument_literals() {
    for (path, language) in [
        ("plugins/pick.plugin.zsh", SourceLanguage::Zsh),
        ("scripts/pick.sh", SourceLanguage::Bash),
    ] {
        let extracted = extract(
            path,
            r#"pick() {
  local tool=$($(choose --token "sk_live_literal_probe"))
  "$(locate_runner 'ghp_quoted_probe')" --fast
  $RUNNER build
}
"#,
        );
        assert_eq!(extracted.language, language);
        assert_reference(&extracted, Some("pick"), "choose", ReferenceKind::Calls);
        assert_reference(
            &extracted,
            Some("pick"),
            "locate_runner",
            ReferenceKind::Calls,
        );
        assert_reference(&extracted, Some("pick"), "$RUNNER", ReferenceKind::Calls);
        let debug = format!("{extracted:?}");
        for literal in ["sk_live_literal_probe", "ghp_quoted_probe"] {
            assert!(
                !debug.contains(literal),
                "{language:?} leaked {literal}: {:?}",
                canonical_facts(&extracted)
            );
        }
        assert!(
            extracted
                .references
                .iter()
                .all(|reference| !reference.name.contains('(') && !reference.name.contains('"')),
            "{language:?} named a call by a dynamic expression: {:?}",
            canonical_facts(&extracted)
        );
    }
}

#[test]
fn fish_variable_commands_are_calls_but_dynamic_names_abstain_without_leaking() {
    let extracted = extract(
        "plugins/pick.fish",
        r#"function pick
    $RUNNER build
    $TOOLS[1] --check
    (choose --token "sk_live_fish_probe") --fast
    "$(locate_runner 'ghp_fish_probe')" --slow
end
"#,
    );
    assert_eq!(extracted.language, SourceLanguage::Fish);
    assert_reference(&extracted, Some("pick"), "RUNNER", ReferenceKind::Calls);
    assert_reference(&extracted, Some("pick"), "TOOLS", ReferenceKind::Calls);
    assert_reference(&extracted, Some("pick"), "choose", ReferenceKind::Calls);
    let debug = format!("{extracted:?}");
    for literal in ["sk_live_fish_probe", "ghp_fish_probe"] {
        assert!(
            !debug.contains(literal),
            "Fish leaked {literal}: {:?}",
            canonical_facts(&extracted)
        );
    }
    assert!(
        extracted.references.iter().all(|reference| {
            !reference.name.contains('(')
                && !reference.name.contains('"')
                && !reference.name.contains('[')
        }),
        "Fish named a call by a dynamic expression: {:?}",
        canonical_facts(&extracted)
    );
}

#[test]
fn fish_extracts_functions_variables_imports_and_owned_calls_safely() {
    let extracted = extract_fixture(SourceLanguage::Fish);

    assert_eq!(extracted.language, SourceLanguage::Fish);
    assert_clean(&extracted);
    assert_eq!(extracted.symbols.len(), 6);
    assert_symbol(
        &extracted,
        SymbolKind::Import,
        "./lib/common.fish",
        "./lib/common.fish",
    );
    assert_symbol(&extracted, SymbolKind::Variable, "FISH_LIMIT", "FISH_LIMIT");
    assert_eq!(
        signature(&extracted, "FISH_LIMIT"),
        Some("= $DEFAULT_LIMIT")
    );
    assert!(symbol(&extracted, "FISH_LIMIT").export.exported);
    assert_eq!(signature(&extracted, "FISH_OTHER"), Some("= OTHER_LIMIT"));
    assert_eq!(signature(&extracted, "FISH_SECRET"), None);
    assert_symbol(
        &extracted,
        SymbolKind::Function,
        "fish_helper",
        "fish_helper",
    );
    assert_symbol(&extracted, SymbolKind::Function, "fish_run", "fish_run");
    assert_eq!(
        signature(&extracted, "fish_helper"),
        Some("fish_helper --description")
    );
    assert_eq!(signature(&extracted, "fish_run"), Some("fish_run"));
    assert_reference(
        &extracted,
        None,
        "./lib/common.fish",
        ReferenceKind::Imports,
    );
    assert_reference(
        &extracted,
        Some("fish_helper"),
        "echo",
        ReferenceKind::Calls,
    );
    assert_reference(
        &extracted,
        Some("fish_run"),
        "fish_helper",
        ReferenceKind::Calls,
    );
    assert_reference(&extracted, None, "fish_run", ReferenceKind::Calls);
    assert_single_literal_import(&extracted, "./lib/common.fish");
}

#[test]
fn powershell_extracts_class_method_function_variables_imports_and_owned_calls_safely() {
    let extracted = extract_fixture(SourceLanguage::PowerShell);

    assert_eq!(extracted.language, SourceLanguage::PowerShell);
    assert_clean(&extracted);
    assert_eq!(extracted.symbols.len(), 7);
    assert_symbol(
        &extracted,
        SymbolKind::Import,
        "./Modules/Common.psm1",
        "./Modules/Common.psm1",
    );
    assert_symbol(
        &extracted,
        SymbolKind::Variable,
        "GlobalLimit",
        "GlobalLimit",
    );
    assert_eq!(
        signature(&extracted, "GlobalLimit"),
        Some("= $DEFAULT_LIMIT")
    );
    assert_eq!(signature(&extracted, "ApiToken"), None);
    assert_symbol(&extracted, SymbolKind::Class, "Worker", "Worker");
    assert_eq!(signature(&extracted, "Worker::Name"), Some("[string]"));
    assert_symbol(&extracted, SymbolKind::Method, "Run", "Worker::Run");
    assert_symbol(
        &extracted,
        SymbolKind::Function,
        "Invoke-Workflow",
        "Invoke-Workflow",
    );
    assert_containment(&extracted, "Worker", "Worker::Name");
    assert_containment(&extracted, "Worker", "Worker::Run");
    assert_reference(
        &extracted,
        None,
        "./Modules/Common.psm1",
        ReferenceKind::Imports,
    );
    assert_reference(
        &extracted,
        Some("Worker::Run"),
        "Invoke-Helper",
        ReferenceKind::Calls,
    );
    assert_reference(
        &extracted,
        Some("Invoke-Workflow"),
        "Invoke-Helper",
        ReferenceKind::Calls,
    );
    assert_reference(&extracted, None, "Invoke-Workflow", ReferenceKind::Calls);
    assert_single_literal_import(&extracted, "./Modules/Common.psm1");
}

#[test]
fn shell_languages_are_production_admitted_through_the_reviewed_family() {
    for (path, source, language) in fixture_cases() {
        let production = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
            .unwrap_or_else(|error| panic!("{path} production snapshot failed: {error}"));
        assert_eq!(production.language(), language);
        assert!(NativeExtractor::new(language).is_ok());
        let snapshot = capability_snapshot(path, source);
        assert_eq!(snapshot.language(), language);
        assert!(NativeExtractor::new_for_capability_validation(language).is_ok());
    }
}

#[test]
fn shell_family_reports_syntax_damage_and_honors_cancellation() {
    let damaged = [
        ("broken.sh", "broken() {\n  echo value\n"),
        ("broken.zsh", "broken() {\n  print value\n"),
        ("broken.fish", "function broken\n  echo value\n"),
        ("broken.ps1", "function Broken {\n  Write-Output value\n"),
    ];
    for (path, source) in damaged {
        let extracted = extract(path, source);
        assert_eq!(extracted.parse_status, FileParseStatus::Partial, "{path}");
        assert!(!extracted.diagnostics.is_empty(), "{path}");
    }

    for (path, source, language) in fixture_cases() {
        let snapshot = capability_snapshot(path, source);
        let mut extractor = NativeExtractor::new_for_capability_validation(language)
            .unwrap_or_else(|error| panic!("{language:?} extractor failed: {error}"));
        assert_eq!(
            extractor.extract_with_cancellation(&snapshot, || true),
            Err(ExtractError::Cancelled),
            "{path}",
        );
    }
}

#[test]
fn shell_family_enforces_source_nesting_and_output_bounds() {
    assert_eq!(
        SourceSnapshot::from_bytes_for_capability_validation(
            "oversized.sh",
            b"value=OTHER_LIMIT\n",
            SourceLimits::new(8)
                .unwrap_or_else(|error| panic!("small source limit failed: {error}")),
        )
        .err(),
        Some(SnapshotError::SourceTooLarge),
    );

    let mut nested = String::from("deep() {\n");
    for _ in 0..300 {
        nested.push_str("if true; then\n");
    }
    for _ in 0..300 {
        nested.push_str("fi\n");
    }
    nested.push_str("}\n");
    let recovered = extract_result("deep.sh", &nested)
        .unwrap_or_else(|error| panic!("deep shell extraction did not recover: {error}"));
    assert_eq!(
        recovered.parse_status,
        cartograph_domain::FileParseStatus::Partial
    );
    assert_eq!(
        recovered.diagnostics[0].code,
        cartograph_extract::DiagnosticCode::NestingLimitExceeded
    );

    let mut excessive = String::new();
    for index in 0..20_000 {
        excessive.push_str("VALUE_");
        excessive.push_str(&index.to_string());
        excessive.push_str("=OTHER_LIMIT\n");
    }
    assert_eq!(
        extract_result("many.sh", &excessive),
        Err(ExtractError::OutputLimit)
    );
}

#[test]
fn shell_family_facts_are_repeatable_and_locked() {
    let expected = [
        "5b1cf0e734e8f4031dd206d9672592893da1c7bdea09e131a0b318c5a30ef23f",
        "e53a03cf5c66e3c092a1967fc0e920a0dd0ce7ce054a1403ee16469687715512",
        "dd5b99c1807dc8d6ae1a99ffe3a0e3aa910a39e0c2d46b9eaca8147ad87a22d0",
        "5b33024a0ec233992fcd36eb8ad1ff0f6fc44121497b5dfdb97ffeb0266307d9",
    ];
    for ((path, source, _), expected) in fixture_cases().into_iter().zip(expected) {
        let first = extract(path, source);
        let second = extract(path, source);
        assert_eq!(canonical_facts(&first), canonical_facts(&second), "{path}");
        assert_unique_ids(&first);
        assert_eq!(locked_digest(&first), expected, "{path}");
    }
}

fn fixture_cases() -> [(&'static str, &'static str, SourceLanguage); 4] {
    [
        (
            "scripts/build.sh",
            r#"#!/usr/bin/env bash
source "./lib/common.sh"
source "$DYNAMIC_MODULE"
readonly MAX_RETRIES=OTHER_LIMIT
export PUBLIC_LIMIT=$DEFAULT_LIMIT
API_TOKEN=sk_live_fixture_secret
LITERAL_COUNT=42

helper() {
  printf '%s' "$1"
}

run() {
  helper "$PUBLIC_LIMIT"
  curl "$ENDPOINT"
}

run
"#,
            SourceLanguage::Bash,
        ),
        (
            "scripts/plugin.zsh",
            r#"source "./lib/common.zsh"
source "${DYNAMIC_MODULE}"
readonly ZSH_LIMIT=OTHER_LIMIT
ZSH_PUBLIC=$DEFAULT_LIMIT
ZSH_TOKEN=github_pat_fixture_credential

zhelper() {
  print -- "$1"
}

zrun() {
  zhelper "$ZSH_PUBLIC"
}

zrun
"#,
            SourceLanguage::Zsh,
        ),
        (
            "scripts/session.fish",
            r#"source "./lib/common.fish"
source $DYNAMIC_MODULE
set -x FISH_LIMIT $DEFAULT_LIMIT
set FISH_OTHER OTHER_LIMIT
set FISH_SECRET database_password

function fish_helper --description helper
  echo $argv[1]
end

function fish_run
  fish_helper $FISH_LIMIT
end

fish_run
"#,
            SourceLanguage::Fish,
        ),
        (
            "scripts/Worker.psm1",
            r"using module './Modules/Common.psm1'
using module $DynamicModule
$GlobalLimit = $DEFAULT_LIMIT
$ApiToken = 'sk_live_fixture_secret'

class Worker {
  [string] $Name

  [void] Run() {
    Invoke-Helper
  }
}

function Invoke-Workflow {
  Invoke-Helper
}

Invoke-Workflow
",
            SourceLanguage::PowerShell,
        ),
    ]
}

fn extract_fixture(language: SourceLanguage) -> ExtractedFile {
    fixture_cases()
        .into_iter()
        .find(|(_, _, candidate)| *candidate == language)
        .map_or_else(
            || panic!("missing {language:?} shell fixture"),
            |(path, source, _)| extract(path, source),
        )
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    extract_result(path, source)
        .unwrap_or_else(|error| panic!("extraction failed for {path}: {error}"))
}

fn extract_result(path: &str, source: &str) -> Result<ExtractedFile, ExtractError> {
    let snapshot = capability_snapshot(path, source);
    let mut extractor = NativeExtractor::new_for_capability_validation(snapshot.language())?;
    extractor.extract(&snapshot)
}

fn capability_snapshot(path: &str, source: &str) -> SourceSnapshot {
    SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("snapshot failed for {path}: {error}"))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("shell-family limits failed: {error}"))
}

fn symbol_names(extracted: &ExtractedFile) -> BTreeMap<&str, &str> {
    extracted
        .symbols
        .iter()
        .map(|symbol| (symbol.id.as_str(), symbol.qualified_name.as_str()))
        .collect()
}

fn canonical_facts(extracted: &ExtractedFile) -> Vec<String> {
    let names = symbol_names(extracted);
    let mut facts = vec![format!(
        "F|{}|{:?}|{}|{}|{}|{}",
        extracted.language.as_str(),
        extracted.parse_status,
        extracted.symbols.len(),
        extracted.references.len(),
        extracted.containments.len(),
        extracted.import_bindings.len(),
    )];
    facts.extend(extracted.symbols.iter().map(|symbol| {
        format!(
            "S|{}|{}|{}|{}-{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
            symbol.kind.as_str(),
            symbol.name,
            symbol.qualified_name,
            symbol.span.start_byte(),
            symbol.span.end_byte(),
            symbol.structural_digest,
            symbol.signature.as_deref().unwrap_or_default(),
            symbol.docstring.as_deref().unwrap_or_default(),
            symbol.body_search_text,
            symbol.implementation.declaration_only,
            symbol.export.exported,
            symbol.execution.async_symbol,
            symbol.execution.static_member,
            symbol.visibility,
        )
    }));
    facts.extend(extracted.containments.iter().map(|edge| {
        format!(
            "C|{}|{}",
            names
                .get(edge.parent.as_str())
                .copied()
                .unwrap_or("<missing>"),
            names
                .get(edge.child.as_str())
                .copied()
                .unwrap_or("<missing>"),
        )
    }));
    facts.extend(extracted.references.iter().map(|reference| {
        format!(
            "R|{}|{}|{}|{}-{}",
            reference
                .owner
                .as_ref()
                .and_then(|owner| names.get(owner.as_str()).copied())
                .unwrap_or("<file>"),
            reference.kind.as_str(),
            reference.name,
            reference.span.start_byte(),
            reference.span.end_byte(),
        )
    }));
    facts.extend(extracted.import_bindings.iter().map(|binding| {
        format!(
            "I|{:?}|{}|{}|{}|{}-{}",
            binding.kind,
            binding.module_specifier,
            binding.imported_name,
            binding.local_name,
            binding.span.start_byte(),
            binding.span.end_byte(),
        )
    }));
    facts.extend(
        extracted
            .diagnostics
            .iter()
            .map(|diagnostic| format!("D|{:?}|{:?}", diagnostic.code, diagnostic.span)),
    );
    facts
}

fn locked_digest(extracted: &ExtractedFile) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("cartograph.v2.shell-family-corpus.2026-07-24");
    for fact in canonical_facts(extracted) {
        hasher.update(&u64::try_from(fact.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(fact.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn assert_unique_ids(extracted: &ExtractedFile) {
    let ids = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), extracted.symbols.len());
}

fn assert_reference(
    extracted: &ExtractedFile,
    owner: Option<&str>,
    name: &str,
    kind: ReferenceKind,
) {
    let names = symbol_names(extracted);
    let matched = extracted.references.iter().any(|reference| {
        reference.name == name
            && reference.kind == kind
            && match (owner, reference.owner.as_ref()) {
                (None, _) => true,
                (Some(expected), Some(owner)) => names.get(owner.as_str()) == Some(&expected),
                (Some(_), None) => false,
            }
    });
    assert!(
        matched,
        "missing {kind:?} reference {name}; facts={:?}",
        canonical_facts(extracted),
    );
}

fn assert_symbol<'file>(
    extracted: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
    qualified_name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| {
            symbol.kind == kind && symbol.name == name && symbol.qualified_name == qualified_name
        })
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} {qualified_name}; facts={:?}",
                canonical_facts(extracted),
            )
        })
}

fn symbol<'file>(
    extracted: &'file ExtractedFile,
    qualified_name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    extracted
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing symbol {qualified_name}; facts={:?}",
                canonical_facts(extracted),
            )
        })
}

fn signature<'file>(extracted: &'file ExtractedFile, qualified_name: &str) -> Option<&'file str> {
    symbol(extracted, qualified_name).signature.as_deref()
}

fn assert_containment(extracted: &ExtractedFile, parent: &str, child: &str) {
    let parent = symbol(extracted, parent);
    let child = symbol(extracted, child);
    assert!(
        extracted
            .containments
            .iter()
            .any(|edge| edge.parent == parent.id && edge.child == child.id),
        "missing containment {} -> {}; facts={:?}",
        parent.qualified_name,
        child.qualified_name,
        canonical_facts(extracted),
    );
}

fn assert_single_literal_import(extracted: &ExtractedFile, expected: &str) {
    let imports = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Import)
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 1, "facts={:?}", canonical_facts(extracted));
    assert_eq!(imports[0].name, expected);
    let references = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Imports)
        .collect::<Vec<_>>();
    assert_eq!(
        references.len(),
        1,
        "facts={:?}",
        canonical_facts(extracted),
    );
    assert_eq!(references[0].name, expected);
    assert!(
        imports
            .iter()
            .all(|symbol| !symbol.name.to_ascii_lowercase().contains("dynamic"))
    );
    assert!(
        references
            .iter()
            .all(|reference| !reference.name.to_ascii_lowercase().contains("dynamic"))
    );
}

fn assert_clean(extracted: &ExtractedFile) {
    assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
    assert!(
        extracted.diagnostics.is_empty(),
        "facts={:?}",
        canonical_facts(extracted),
    );
    assert_eq!(extracted.import_bindings, []);
    assert_unique_ids(extracted);
    let rendered = format!("{extracted:?}");
    for forbidden in [
        "sk_live_fixture_secret",
        "github_pat_fixture_credential",
        "database_password",
        "= 42",
        "<redacted>",
    ] {
        assert!(!rendered.contains(forbidden), "retained {forbidden}");
    }
    assert!(extracted.symbols.iter().all(|symbol| {
        symbol
            .signature
            .as_deref()
            .is_none_or(|signature| !signature.contains("<redacted>"))
    }));
}
