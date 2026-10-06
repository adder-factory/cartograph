//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;
const SCHEMA_SOURCE: &str = r#"
CREATE SCHEMA reporting;
CREATE TYPE order_status AS ENUM ('pending', 'shipped', 'sk_live_must_not_escape');
CREATE TYPE point AS (x FLOAT, y FLOAT);

CREATE TABLE "public"."users" (
  id BIGINT PRIMARY KEY,
  status order_status NOT NULL
);
CREATE TABLE reporting.orders (
  id BIGINT PRIMARY KEY,
  user_id BIGINT REFERENCES "public"."users"(id)
);
CREATE VIEW reporting.active_orders AS
  SELECT o.id FROM reporting.orders o
  JOIN "public"."users" u ON u.id = o.user_id;

CREATE FUNCTION reporting.find_orders(user_id BIGINT) RETURNS BIGINT AS 'SELECT user_id' LANGUAGE SQL;
CREATE TRIGGER orders_audit AFTER INSERT ON reporting.orders
  FOR EACH ROW EXECUTE FUNCTION reporting.audit_orders();

SELECT * FROM reporting.orders;
CREATE INDEX idx_orders_user ON reporting.orders(user_id);
"#;

#[test]
fn sql_ddl_emits_schema_objects_columns_and_cross_object_relations() {
    let first = extract("db/schema.sql", SCHEMA_SOURCE);
    let second = extract("db/schema.sql", SCHEMA_SOURCE);
    assert_eq!(first, second);

    for (kind, qualified_name, signature) in [
        (
            SymbolKind::Namespace,
            "reporting",
            "CREATE SCHEMA reporting",
        ),
        (SymbolKind::Enum, "order_status", "CREATE TYPE order_status"),
        (SymbolKind::TypeAlias, "point", "CREATE TYPE point"),
        (
            SymbolKind::Table,
            "public.users",
            "CREATE TABLE public.users",
        ),
        (
            SymbolKind::Table,
            "reporting.orders",
            "CREATE TABLE reporting.orders",
        ),
        (
            SymbolKind::Table,
            "reporting.active_orders",
            "CREATE VIEW reporting.active_orders",
        ),
        (
            SymbolKind::Function,
            "reporting.find_orders",
            "CREATE FUNCTION reporting.find_orders(user_id BIGINT)",
        ),
        (
            SymbolKind::Function,
            "orders_audit",
            "CREATE TRIGGER orders_audit",
        ),
    ] {
        assert_symbol(&first, kind, qualified_name, signature);
    }

    for (qualified_name, signature) in [
        ("public.users::id", "BIGINT"),
        ("public.users::status", "order_status"),
        ("reporting.orders::id", "BIGINT"),
        ("reporting.orders::user_id", "BIGINT"),
    ] {
        assert_symbol(&first, SymbolKind::Field, qualified_name, signature);
    }

    let orders = symbol(&first, SymbolKind::Table, "reporting.orders");
    assert_reference(
        &first,
        &orders.id,
        "public.users",
        ReferenceKind::References,
    );
    let view = symbol(&first, SymbolKind::Table, "reporting.active_orders");
    assert_reference(
        &first,
        &view.id,
        "reporting.orders",
        ReferenceKind::References,
    );
    assert_reference(&first, &view.id, "public.users", ReferenceKind::References);
    let trigger = symbol(&first, SymbolKind::Function, "orders_audit");
    assert_reference(
        &first,
        &trigger.id,
        "reporting.orders",
        ReferenceKind::References,
    );
    assert_reference(
        &first,
        &trigger.id,
        "reporting.audit_orders",
        ReferenceKind::Calls,
    );

    assert!(first.symbols.iter().all(|symbol| {
        symbol.name != "idx_orders_user"
            && !symbol.name.contains("sk_live")
            && !symbol.qualified_name.contains("sk_live")
            && !symbol
                .signature
                .as_deref()
                .is_some_and(|signature| signature.contains("sk_live"))
    }));
}

#[test]
fn function_body_dml_targets_reference_their_tables_but_invoked_functions_do_not() {
    let extracted = extract(
        "db/functions.sql",
        "CREATE FUNCTION audit_orders() RETURNS TRIGGER AS $$
BEGIN
  INSERT INTO audit_log (id, message) VALUES (1, 'order changed');
  UPDATE stats SET n = n + 1;
  DELETE FROM reporting.old_rows WHERE id = 1;
  INSERT INTO AUDIT_LOG (id) VALUES (2);
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE FUNCTION order_count(uid INT) RETURNS INT AS $$
  SELECT COUNT(*) FROM orders WHERE user_id = uid;
$$ LANGUAGE SQL;
",
    );
    let audit = symbol(&extracted, SymbolKind::Function, "audit_orders");
    for (target, line) in [("audit_log", 3), ("stats", 4), ("reporting.old_rows", 5)] {
        assert_reference(&extracted, &audit.id, target, ReferenceKind::References);
        assert!(
            extracted
                .references
                .iter()
                .any(|reference| reference.name == target && reference.span.start_line() == line),
            "{target} is not anchored on line {line}: {:?}",
            extracted.references
        );
    }
    assert_eq!(
        extracted
            .references
            .iter()
            .filter(|reference| reference.name.eq_ignore_ascii_case("audit_log"))
            .count(),
        1,
        "a body references each table once regardless of case: {:?}",
        extracted.references
    );
    let count = symbol(&extracted, SymbolKind::Function, "order_count");
    assert_reference(&extracted, &count.id, "orders", ReferenceKind::References);
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.eq_ignore_ascii_case("count")),
        "an invoked function is not a table: {:?}",
        extracted.references
    );
}

#[test]
fn plain_sql_dml_and_malformed_create_statements_do_not_invent_declarations() {
    let extracted = extract(
        "db/queries.sql",
        "SELECT * FROM users; INSERT INTO orders(id) VALUES (1); UPDATE users SET id = 2; CREATE TABLE;",
    );
    assert!(
        extracted.symbols.is_empty(),
        "unexpected symbols: {:?}",
        extracted.symbols
    );
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("SQL snapshot failed: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::Sql);
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("SQL extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("SQL extraction failed: {error}"))
}

fn assert_symbol(file: &ExtractedFile, kind: SymbolKind, qualified_name: &str, signature: &str) {
    let symbol = symbol(file, kind, qualified_name);
    assert_eq!(symbol.signature.as_deref(), Some(signature));
}

fn symbol<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    qualified_name: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    let matches = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == kind && symbol.qualified_name == qualified_name)
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one {kind:?} {qualified_name}: {:?}",
        file.symbols
    );
    matches[0]
}

fn assert_reference(
    file: &ExtractedFile,
    owner: &cartograph_domain::SymbolId,
    target: &str,
    kind: ReferenceKind,
) {
    assert!(
        file.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(owner)
                && reference.name == target
                && reference.kind == kind
        }),
        "missing {kind:?} reference to {target}: {:?}",
        file.references
    );
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("SQL source limit failed: {error}"))
}
