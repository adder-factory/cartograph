use super::*;
use cartograph_domain::DocumentId;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn symbol(number: u64) -> SymbolId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&number.to_be_bytes());
    SymbolId::from_uuid_v8(bytes)
}

fn document(number: u64, natural_text: &str) -> CanonicalSearchDocument {
    CanonicalSearchDocument {
        document_id: DocumentId::from_uuid_v8([1; 16]),
        file_id: None,
        symbol_id: Some(symbol(number)),
        path: "src/lib.rs".into(),
        language: "rust".into(),
        kind: DocumentKind::Symbol,
        qualified_name: "fixture".into(),
        code: "fn fixture() {}".into(),
        natural_text: natural_text.into(),
        metadata_json: "{}".into(),
    }
}

#[test]
fn empty_documentation_does_not_exhaust_readiness_workspace() -> TestResult {
    let mut lengths = BTreeMap::new();
    for number in 1..=131_073 {
        record_documentation_length(&mut lengths, document(number, ""))?;
    }
    assert!(lengths.is_empty());
    record_documentation_length(&mut lengths, document(1, "Résumé 🦀"))?;
    assert_eq!(lengths.get(&symbol(1)), Some(&8));
    Ok(())
}

#[test]
fn documentation_lengths_preserve_last_document_and_positive_bound() -> TestResult {
    let mut lengths = BTreeMap::new();
    record_documentation_length(&mut lengths, document(1, "first"))?;
    record_documentation_length(&mut lengths, document(1, ""))?;
    assert!(!lengths.contains_key(&symbol(1)));
    record_documentation_length(&mut lengths, document(1, "é🦀"))?;
    assert_eq!(lengths.get(&symbol(1)), Some(&2));

    let mut other_kind = document(1, "ignored");
    other_kind.kind = DocumentKind::File;
    record_documentation_length(&mut lengths, other_kind)?;
    let mut no_symbol = document(1, "ignored");
    no_symbol.symbol_id = None;
    record_documentation_length(&mut lengths, no_symbol)?;
    assert_eq!(lengths.get(&symbol(1)), Some(&2));

    for number in 2..=131_072 {
        record_documentation_length(&mut lengths, document(number, "x"))?;
    }
    assert!(matches!(
        record_documentation_length(&mut lengths, document(131_073, "x")),
        Err(RocksError::Limit {
            resource: "artifact-workspace"
        })
    ));
    Ok(())
}
