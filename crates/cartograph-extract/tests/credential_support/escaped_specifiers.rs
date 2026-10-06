//! Escape assertions compiled only by module-specifier regression targets.

use cartograph_domain::{ReferenceKind, SymbolKind};

use crate::credential_support::{assert_no_credentials, extract};

pub fn assert_escaped_specifiers_abstain(path: &str, template: &str) {
    for value in [
        "https:\\/\\/reader:FAKEPASSWORDxyz@example.invalid/module",
        "sk_l\\u0069ve_FAKE1234567890abcdef",
        "./glp\\u0061t-aaaaaaaaaaaaaaaaaaaa/module",
    ] {
        let file = extract(path, &template.replace("@VALUE@", value));
        assert_no_credentials(&file);
        assert_eq!(file.import_bindings.len(), 0);
        assert!(
            file.symbols
                .iter()
                .all(|symbol| symbol.kind != SymbolKind::Import)
        );
        assert!(
            file.references
                .iter()
                .all(|reference| reference.kind != ReferenceKind::Imports)
        );
    }
    // An ordinary escaped spelling (`tok\u0065n` is "token") keeps its import facts.
    if !template.trim_start().starts_with("import ") {
        return;
    }
    let file = extract(path, &template.replace("@VALUE@", "tok\\u0065n"));
    assert!(
        file.references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::Imports),
        "ordinary escaped module must keep its import: {file:?}"
    );
}
