//! Integration coverage for Cartograph native extraction contracts.

mod credential_support;
mod dependency_ownership;

use cartograph_domain::{SourceLanguage, SymbolKind};
use cartograph_extract::{
    EMBEDDED_SQL_RESOLUTION_PREFIX, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn embedded_sql_literals_screen_credentials_before_table_projection() {
    credential_support::assert_screened(
        "query.ts",
        "const query = 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    );
    for value in credential_support::CREDENTIAL_INPUTS {
        let source = format!("const query = 'SELECT * FROM users WHERE url = \"{value}\"';\n");
        let file = extract("query.ts", &source);
        credential_support::assert_no_credentials(&file);
        assert_eq!(references(&file, "users", "read"), 0);
    }
    let file = extract("query.ts", "const query = 'SELECT * FROM \"token\"';\n");
    let reference = sql_reference(&file, "token", "read");
    assert_eq!(reference.name, "token");
    assert_eq!(
        reference.resolution_name.as_deref(),
        Some("cartograph.embedded-sql::read::token")
    );
}

#[test]
fn static_sql_literals_link_supported_application_languages_to_qualified_tables() {
    let fixtures = [
        (
            "src/repo.ts",
            "export function loadUsers() { return db.query('SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id'); }\n",
            SourceLanguage::TypeScript,
        ),
        (
            "src/repo.js",
            "export function loadUsers() { return db.query('SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id'); }\n",
            SourceLanguage::JavaScript,
        ),
        (
            "src/repo.tsx",
            "export function loadUsers() { return db.query('SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id'); }\n",
            SourceLanguage::Tsx,
        ),
        (
            "src/repo.jsx",
            "export function loadUsers() { return db.query('SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id'); }\n",
            SourceLanguage::Jsx,
        ),
        (
            "src/repo.py",
            "def load_users():\n    return db.query(\"SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id\")\n",
            SourceLanguage::Python,
        ),
        (
            "src/repo.go",
            "package repo\nfunc loadUsers() { db.Query(`SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id`) }\n",
            SourceLanguage::Go,
        ),
        (
            "src/repo.rs",
            "fn load_users() { db.query(r#\"SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id\"#); }\n",
            SourceLanguage::Rust,
        ),
        (
            "src/Repo.java",
            "class Repo { void loadUsers() { db.query(\"SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id\"); } }\n",
            SourceLanguage::Java,
        ),
        (
            "src/Repo.kt",
            "class Repo { fun loadUsers() { db.query(\"SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id\") } }\n",
            SourceLanguage::Kotlin,
        ),
        (
            "src/Repo.cs",
            "class Repo { void LoadUsers() { db.Query(\"SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id\"); } }\n",
            SourceLanguage::CSharp,
        ),
        (
            "src/repo.php",
            "<?php function loadUsers() { return $db->query('SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id'); }\n",
            SourceLanguage::Php,
        ),
        (
            "src/repo.rb",
            "def load_users\n  db.query(\"SELECT * FROM public.users JOIN orders ON orders.user_id = public.users.id\")\nend\n",
            SourceLanguage::Ruby,
        ),
    ];
    for (path, source, language) in fixtures {
        let first = extract(path, source);
        let second = extract(path, source);
        assert_eq!(first, second, "{path} was not deterministic");
        assert_eq!(first.language, language, "{path}");
        for table in ["public.users", "orders"] {
            let reference = sql_reference(&first, table, "read");
            let owner = reference
                .owner
                .as_ref()
                .unwrap_or_else(|| panic!("{path}:{table} was not attributed to a callable"));
            assert!(
                first.symbols.iter().any(|symbol| {
                    &symbol.id == owner
                        && matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method)
                }),
                "{path}:{table} owner was not callable: {:?}",
                first.symbols
            );
            let start = usize::try_from(reference.span.start_byte())
                .unwrap_or_else(|error| panic!("{path} start overflowed: {error}"));
            let end = usize::try_from(reference.span.end_byte())
                .unwrap_or_else(|error| panic!("{path} end overflowed: {error}"));
            assert!(
                source[start..end].contains(table.rsplit('.').next().unwrap_or(table)),
                "{path}:{table} span was not source-exact: {:?}",
                &source[start..end]
            );
        }
    }
}

#[test]
fn static_sql_operations_are_deduplicated_and_dynamic_or_prose_strings_are_rejected() {
    let source = r#"
export function mutate(table: string) {
  db.exec('INSERT INTO orders(id) VALUES (1); UPDATE orders SET id = 2; DELETE FROM orders WHERE id = 2');
  db.exec('CREATE TABLE IF NOT EXISTS audit_log(id int); ALTER TABLE audit_log ADD COLUMN note text; DROP TABLE audit_log');
  db.query('SELECT * FROM "public"."users" JOIN "public"."users" u2 ON u2.id = users.id');
  db.query(`SELECT * FROM ${table}`);
  db.query('select a value from the list');
}
// SELECT * FROM comments_only
"#;
    let extracted = extract("src/mutations.ts", source);

    assert_eq!(references(&extracted, "orders", "write"), 1);
    assert_eq!(references(&extracted, "audit_log", "ddl"), 1);
    assert_eq!(references(&extracted, "public.users", "read"), 1);
    assert_eq!(references(&extracted, "comments_only", "read"), 0);
    assert_eq!(references(&extracted, "table", "read"), 0);
    assert_eq!(references(&extracted, "the", "read"), 0);
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("embedded SQL snapshot failed for {path}: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("embedded SQL extractor failed for {path}: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("embedded SQL extraction failed for {path}: {error}"))
}

fn sql_reference<'file>(
    file: &'file ExtractedFile,
    table: &str,
    operation: &str,
) -> &'file cartograph_extract::ExtractedReference {
    let marker = format!("{EMBEDDED_SQL_RESOLUTION_PREFIX}{operation}::{table}");
    file.references
        .iter()
        .find(|reference| {
            reference.name == table && reference.resolution_name.as_deref() == Some(&marker)
        })
        .unwrap_or_else(|| {
            panic!(
                "missing embedded SQL {operation} {table}: {:?}",
                file.references
            )
        })
}

fn references(file: &ExtractedFile, table: &str, operation: &str) -> usize {
    let marker = format!("{EMBEDDED_SQL_RESOLUTION_PREFIX}{operation}::{table}");
    file.references
        .iter()
        .filter(|reference| {
            reference.name == table && reference.resolution_name.as_deref() == Some(&marker)
        })
        .count()
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("embedded SQL source limit failed: {error}"))
}
