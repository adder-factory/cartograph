//! Integration coverage for Cartograph native extraction contracts.

mod dependency_ownership;

use std::assert_matches;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;
const GRAPHQL_SCHEMA: &str = r#"
"""A graph entity."""
interface Node { id: ID! }

scalar DateTime
enum Role { ADMIN EDITOR }
input UserFilter { role: Role createdAfter: DateTime }
type User implements Node {
  id: ID!
  role: Role!
  friends: [[User!]!]!
}
type Post implements Node { id: ID! author: User! }
union SearchResult = User | Post
directive @cacheControl(maxAge: Int) on FIELD_DEFINITION
extend type User implements Node { lastSeen: DateTime }
extend enum Role { VIEWER }

schema { query: Query }
query RuntimeQuery { user { id } }
"#;

#[test]
fn graphql_sdl_emits_precise_types_members_relationships_extensions_and_docs() {
    let first = extract("schema/api.graphql", GRAPHQL_SCHEMA);
    let second = extract("schema/api.graphql", GRAPHQL_SCHEMA);
    assert_eq!(first, second);

    assert_symbol(
        &first,
        SymbolKind::Interface,
        "Node",
        Some("interface Node"),
    );
    assert_symbol(
        &first,
        SymbolKind::TypeAlias,
        "DateTime",
        Some("scalar DateTime"),
    );
    assert_symbol(&first, SymbolKind::Enum, "Role", Some("enum Role"));
    assert_symbol(
        &first,
        SymbolKind::Class,
        "UserFilter",
        Some("input UserFilter"),
    );
    assert_symbol(&first, SymbolKind::Class, "User", Some("type User"));
    assert_symbol(&first, SymbolKind::Class, "Post", Some("type Post"));
    assert_symbol(
        &first,
        SymbolKind::TypeAlias,
        "SearchResult",
        Some("union SearchResult"),
    );
    assert_symbol(
        &first,
        SymbolKind::Function,
        "@cacheControl",
        Some("directive @cacheControl(maxAge: Int)"),
    );
    assert_symbol(&first, SymbolKind::Class, "User", Some("extend type User"));
    assert_symbol(&first, SymbolKind::Enum, "Role", Some("extend enum Role"));

    let node = symbol_with_signature(&first, SymbolKind::Interface, "Node", "interface Node");
    assert_eq!(node.docstring.as_deref(), Some("A graph entity."));
    let user = symbol_with_signature(&first, SymbolKind::Class, "User", "type User");
    let role = symbol_with_signature(&first, SymbolKind::Enum, "Role", "enum Role");
    for (qualified, signature) in [
        ("User::id", "id: ID!"),
        ("User::role", "role: Role!"),
        ("User::friends", "friends: [[User!]!]!"),
    ] {
        let field = unique_symbol(&first, SymbolKind::Field, qualified);
        assert_eq!(field.signature.as_deref(), Some(signature));
        assert!(contains(&first, &user.id, &field.id));
    }
    for member in ["ADMIN", "EDITOR"] {
        let value = unique_symbol(&first, SymbolKind::EnumMember, &format!("Role::{member}"));
        assert!(contains(&first, &role.id, &value.id));
    }

    for (owner_qualified, kind, target) in [
        ("User", ReferenceKind::Implements, "Node"),
        ("Post", ReferenceKind::Implements, "Node"),
        ("User::role", ReferenceKind::TypeOf, "Role"),
        ("User::friends", ReferenceKind::TypeOf, "User"),
        ("Post::author", ReferenceKind::TypeOf, "User"),
        ("SearchResult", ReferenceKind::References, "User"),
        ("SearchResult", ReferenceKind::References, "Post"),
    ] {
        assert_reference(&first, owner_qualified, kind, target);
    }
    assert!(first.references.iter().all(|reference| {
        !matches!(
            reference.name.as_str(),
            "ID" | "Int" | "Float" | "String" | "Boolean"
        )
    }));

    let extension = symbol_with_signature(&first, SymbolKind::Class, "User", "extend type User");
    assert!(first.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&extension.id)
            && reference.kind == ReferenceKind::Extends
            && reference.name == "User"
    }));
    assert!(first.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&extension.id)
            && reference.kind == ReferenceKind::Implements
            && reference.name == "Node"
    }));

    assert!(
        first
            .symbols
            .iter()
            .all(|symbol| { !matches!(symbol.name.as_str(), "schema" | "RuntimeQuery" | "user") })
    );
}

#[test]
fn prisma_emits_models_composites_enums_fields_and_relation_types_only() {
    let source = r#"
datasource db {
  provider = "postgresql"
  url = env("DATABASE_URL")
}
generator client { provider = "prisma-client-js" }

enum Role { ADMIN EDITOR }
type Photo { height Int width Int }
model User {
  id Int @id
  role Role
  posts Post[]
  avatar Photo?
  metadata Json
  native Unsupported("tsvector")
}
model Post {
  id Int @id
  author User @relation(fields: [authorId], references: [id])
  authorId Int
}
"#;
    let first = extract("prisma/schema.prisma", source);
    let second = extract("prisma/schema.prisma", source);
    assert_eq!(first, second);

    for (kind, name, signature) in [
        (SymbolKind::Enum, "Role", "enum Role"),
        (SymbolKind::Struct, "Photo", "type Photo"),
        (SymbolKind::Struct, "User", "model User"),
        (SymbolKind::Struct, "Post", "model Post"),
    ] {
        assert_symbol(&first, kind, name, Some(signature));
    }
    for (qualified, signature) in [
        ("Role::ADMIN", "ADMIN"),
        ("Role::EDITOR", "EDITOR"),
        ("Photo::height", "Int"),
        ("Photo::width", "Int"),
        ("User::id", "Int"),
        ("User::role", "Role"),
        ("User::posts", "Post[]"),
        ("User::avatar", "Photo?"),
        ("User::metadata", "Json"),
        ("User::native", "Unsupported"),
        ("Post::author", "User"),
    ] {
        let kind = if qualified.starts_with("Role::") {
            SymbolKind::EnumMember
        } else {
            SymbolKind::Field
        };
        let symbol = unique_symbol(&first, kind, qualified);
        assert_eq!(symbol.signature.as_deref(), Some(signature));
    }

    for (owner, target) in [
        ("User::role", "Role"),
        ("User::posts", "Post"),
        ("User::avatar", "Photo"),
        ("Post::author", "User"),
    ] {
        assert_reference(&first, owner, ReferenceKind::TypeOf, target);
    }
    for scalar_owner in [
        "User::id",
        "User::metadata",
        "User::native",
        "Post::authorId",
    ] {
        let owner = unique_symbol(&first, SymbolKind::Field, scalar_owner);
        assert!(first.references.iter().all(|reference| {
            reference.owner.as_ref() != Some(&owner.id) || reference.kind != ReferenceKind::TypeOf
        }));
    }
    assert!(first.symbols.iter().all(|symbol| {
        !matches!(symbol.name.as_str(), "db" | "client" | "provider" | "url")
            && !symbol
                .signature
                .as_deref()
                .is_some_and(|signature| signature.contains("DATABASE_URL"))
    }));
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("declarative schema snapshot failed: {error}"));
    assert_matches!(
        snapshot.language(),
        SourceLanguage::GraphQl | SourceLanguage::Prisma
    );
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("declarative schema extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("declarative schema extraction failed: {error}"))
}

fn assert_symbol(file: &ExtractedFile, kind: SymbolKind, name: &str, signature: Option<&str>) {
    assert!(
        file.symbols.iter().any(|symbol| {
            symbol.kind == kind && symbol.name == name && symbol.signature.as_deref() == signature
        }),
        "missing {kind:?} {name} {signature:?}: {:?}",
        file.symbols
    );
}

fn unique_symbol<'file>(
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

fn symbol_with_signature<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
    signature: &str,
) -> &'file cartograph_extract::ExtractedSymbol {
    file.symbols
        .iter()
        .find(|symbol| {
            symbol.kind == kind
                && symbol.name == name
                && symbol.signature.as_deref() == Some(signature)
        })
        .unwrap_or_else(|| panic!("missing {kind:?} {name} {signature}: {:?}", file.symbols))
}

fn assert_reference(
    file: &ExtractedFile,
    owner_qualified_name: &str,
    kind: ReferenceKind,
    target: &str,
) {
    let owner = file
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == owner_qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "missing reference owner {owner_qualified_name}: {:?}",
                file.symbols
            )
        });
    assert!(
        file.references.iter().any(|reference| {
            reference.owner.as_ref() == Some(&owner.id)
                && reference.kind == kind
                && reference.name == target
        }),
        "missing {kind:?} {owner_qualified_name} -> {target}: {:?}",
        file.references
    );
}

fn contains(
    file: &ExtractedFile,
    parent: &cartograph_domain::SymbolId,
    child: &cartograph_domain::SymbolId,
) -> bool {
    file.containments
        .iter()
        .any(|containment| &containment.parent == parent && &containment.child == child)
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("declarative schema source limit failed: {error}"))
}
