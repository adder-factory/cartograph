//! Schema-recognizer naming parity with the retired v1 recognizers.
//!
//! v1 kept quoted Zod keys and enum values with spaces or punctuation and
//! Unicode Pydantic model/field names. v2 admits the same names while still
//! refusing credential-shaped literals and names that would corrupt `::`
//! qualification.

mod dependency_ownership;

use cartograph_domain::SymbolKind;
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn zod_quoted_keys_and_spaced_enum_values_keep_their_names() {
    let source = r#"
import { z } from 'zod';
export const S = z.object({
  "x y": z.number(),
  "content-type": z.string(),
  _ok: z.boolean(),
  "a::b": z.string(),
  "  padded": z.string(),
  s: z.enum(["c d", "a-b", "café", "ghp_0123456789abcdefABCDEF0123456789", "a::b", "ok/ghp_abcé"]),
});
"#;
    let file = extract("src/schema.ts", source);
    assert_eq!(file, extract("src/schema.ts", source));
    for name in ["S::x y", "S::content-type", "S::_ok", "S::s"] {
        symbol(&file, SymbolKind::Field, name);
    }
    for name in ["S::s::c d", "S::s::a-b", "S::s::café"] {
        symbol(&file, SymbolKind::EnumMember, name);
    }
    assert!(
        file.symbols.iter().all(|symbol| {
            !symbol.name.contains("::")
                && !symbol.name.starts_with(' ')
                && !symbol.name.contains("ghp_")
        }),
        "unsafe schema names escaped: {:?}",
        names(&file)
    );
}

#[test]
fn pydantic_models_keep_unicode_names_and_reduce_literal_signatures() {
    let source = "from typing import Literal\nfrom pydantic import BaseModel\nclass Café(BaseModel):\n    naïve: str\n    mode: Literal[\"a\", \"b c\"]\n";
    let file = extract("src/models.py", source);
    symbol(&file, SymbolKind::Struct, "Café");
    symbol(&file, SymbolKind::Field, "Café::naïve");
    let mode = symbol(&file, SymbolKind::Field, "Café::mode");
    assert_eq!(
        mode.signature.as_deref(),
        Some("Literal[...]"),
        "a literal-bearing annotation must not be retained verbatim"
    );
    for member in ["Café::mode::a", "Café::mode::b c"] {
        symbol(&file, SymbolKind::EnumMember, member);
    }
}

#[test]
fn literal_schema_names_never_carry_credentials() {
    // Quoted keys, enum values, and `Literal[...]` members are source
    // literals: a connection URI, user info, an e-mail address, a provider
    // token, or a credential phrase never becomes a symbol name, while
    // ordinary quoted names (with spaces, dashes, or digits, or naming a
    // field such as `password`) still do.
    let source = r#"
import { z } from 'zod';
export const S = z.object({
  "postgres://admin:hunter2@db.internal/prod": z.string(),
  "user:Tr0ub4dor@host": z.string(),
  "my password hunter2": z.string(),
  'Bearer sk_live_AB12CD34EF56GH78': z.string(),
  "Bearer abc123": z.string(),
  "ops@example.com": z.string(),
  "x y": z.number(),
  "content-type": z.string(),
  "200": z.string(),
  "password": z.string(),
  "sk_live_AB12CD34EF56GH78": z.string(),
  "Bearer-abc123": z.string(),
  mode: z.enum(["https://u:p4ss@x.io", "a+b", "c d", "Bearer"]),
});
register({ "redis://:hunter2@cache:6379": z.object({ a: z.string() }) });
"#;
    let file = extract("src/secrets.ts", source);
    let mut schema_names = file
        .symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                SymbolKind::Field | SymbolKind::EnumMember | SymbolKind::Struct
            )
        })
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    schema_names.sort_unstable();
    assert_eq!(
        schema_names,
        [
            "S",
            "S::200",
            "S::content-type",
            "S::mode",
            "S::mode::Bearer",
            "S::mode::c d",
            "S::password",
            "S::x y",
        ],
        "{:?}",
        names(&file)
    );
    let python = "from typing import Literal\nfrom pydantic import BaseModel\nclass Conn(BaseModel):\n    url: Literal[\"postgres://admin:hunter2@db/prod\", \"my secret value\", \"read only\"]\n";
    let file = extract("src/conn.py", python);
    let members = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::EnumMember)
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(members, ["Conn::url::read only"], "{:?}", names(&file));
}

fn symbol<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("missing {kind:?} {qualified_name}: {:?}", names(file)))
}

fn names(file: &ExtractedFile) -> Vec<(SymbolKind, &str)> {
    file.symbols
        .iter()
        .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
        .collect()
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("schema snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("schema extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("schema extraction failed: {error}"))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("schema source limit failed: {error}"))
}
