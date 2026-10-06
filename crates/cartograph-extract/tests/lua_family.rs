//! Lua, Luau, and KHN extraction contracts restored from the v1 Lua extractors.

mod credential_support;
mod dependency_ownership;
mod script_family_support;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::ImportBindingKind;
use script_family_support::{
    ReferenceQuery, assert_linear_work, extract, names_of_kind, numbered_names, symbol,
};

const SECRET_SENTINEL: &str = "sk_live_lua_family_secret";
/// Longest source name the scripting families retain.
const LONGEST_NAME_BYTES: usize = 512;

#[test]
fn lua_colon_definitions_are_methods_and_dotted_definitions_keep_full_names() {
    let extracted = extract(
        "src/thing.lua",
        "local M = {}\n\nfunction M.create(name)\n  return name\nend\n\nfunction M:speak()\n  return self.name\nend\n\nlocal function helper()\n  return 1\nend\n\nfunction plain()\n  return 2\nend\n",
    );
    assert_eq!(extracted.language, SourceLanguage::Lua);
    assert_eq!(names_of_kind(&extracted, SymbolKind::Method), ["M:speak"]);
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "M:speak")
            .signature
            .as_deref(),
        Some("function M:speak()")
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Function),
        ["M.create", "helper", "plain"]
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "M.create")
            .signature
            .as_deref(),
        Some("function M.create(name)")
    );
    assert_eq!(names_of_kind(&extracted, SymbolKind::Variable), ["M"]);
    assert!(
        !symbol(&extracted, SymbolKind::Function, "helper")
            .export
            .exported
    );
    assert!(
        !symbol(&extracted, SymbolKind::Variable, "M")
            .export
            .exported
    );
}

#[test]
fn lua_colon_method_bodies_own_their_calls() {
    let extracted = extract(
        "src/render.lua",
        "local M = {}\n\nfunction M:render()\n  return tostring(self.value)\nend\n",
    );
    let method = symbol(&extracted, SymbolKind::Method, "M:render");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "tostring")
            .owned_by(&method.id)
            .found_in(&extracted)
    );
}

#[test]
fn lua_multi_local_lists_and_function_assignments_are_named_by_their_binding() {
    let extracted = extract(
        "src/codec.lua",
        "local a, b = 1, function(q) qux(q) end\nlocal encode\nencode = function(v) return json.encode(v) end\nlocal pair_left, pair_right = makepair()\nM.field = function(w) go(w) end\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Variable),
        ["a", "encode", "pair_left", "pair_right"]
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Function),
        ["M.field", "b", "encode"]
    );
    let b = symbol(&extracted, SymbolKind::Function, "b");
    assert_eq!(b.signature.as_deref(), Some("function b(q)"));
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "qux")
            .owned_by(&b.id)
            .found_in(&extracted)
    );
    let encode = symbol(&extracted, SymbolKind::Function, "encode");
    assert!(
        !encode.export.exported,
        "an assigned function may target a forward-declared local"
    );
    let field = symbol(&extracted, SymbolKind::Function, "M.field");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "go")
            .owned_by(&field.id)
            .found_in(&extracted)
    );
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "json.encode")
            .owned_by(&encode.id)
            .found_in(&extracted)
    );
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| matches!(symbol.name.as_str(), "q" | "v" | "w")),
        "function values are never named after their parameters: {:?}",
        extracted.symbols
    );
}

#[test]
fn lua_string_requires_emit_imports_and_keep_the_require_call() {
    let extracted = extract(
        "src/main.lua",
        "local json = require(\"json\")\nlocal strings = require \"util.strings\"\nrequire('side.effect')\nlocal dynamic = require(name)\n",
    );
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Import),
        ["json", "side.effect", "util.strings"]
    );
    for specifier in ["json", "util.strings", "side.effect"] {
        assert!(ReferenceQuery::new(ReferenceKind::Imports, specifier).found_in(&extracted));
        let binding = extracted
            .import_bindings
            .iter()
            .find(|binding| binding.module_specifier == specifier)
            .unwrap_or_else(|| panic!("missing binding {specifier}"));
        assert_eq!(binding.kind, ImportBindingKind::Namespace);
        assert_eq!(
            binding.local_name, "<load>",
            "a require alias is not a module binding: the returned value is unknown"
        );
    }
    assert_eq!(extracted.import_bindings.len(), 3);
    assert_eq!(
        ReferenceQuery::new(ReferenceKind::Calls, "require").count_in(&extracted)
            + extracted
                .references
                .iter()
                .filter(|reference| {
                    reference.kind == ReferenceKind::Calls
                        && reference.name == "require"
                        && reference.owner.is_some()
                })
                .count(),
        4
    );
}

#[test]
fn luau_type_aliases_export_only_with_export_and_signatures_keep_return_types() {
    let extracted = extract(
        "src/service.luau",
        "export type User = {\n  name: string,\n}\ntype Priv = number\n\nlocal M = {}\n\nfunction M:greet(user: User): string\n  return user.name\nend\n\nlocal function helper(value: number)\n  return value\nend\n",
    );
    assert_eq!(extracted.language, SourceLanguage::Luau);
    assert!(
        symbol(&extracted, SymbolKind::TypeAlias, "User")
            .export
            .exported
    );
    assert!(
        !symbol(&extracted, SymbolKind::TypeAlias, "Priv")
            .export
            .exported
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Method, "M:greet")
            .signature
            .as_deref(),
        Some("function M:greet(user: User): string")
    );
    assert_eq!(
        symbol(&extracted, SymbolKind::Function, "helper")
            .signature
            .as_deref(),
        Some("function helper(value: number)")
    );
    assert!(
        !symbol(&extracted, SymbolKind::Method, "M:greet")
            .export
            .exported
    );
}

#[test]
fn khn_scripts_share_the_lua_family() {
    let extracted = extract(
        "Scripts/thoth/helpers/Thing.khn",
        "local Thing = {}\nfunction Thing:Apply(target)\n  Osi.ApplyStatus(target)\nend\n",
    );
    assert_eq!(extracted.language, SourceLanguage::Khn);
    let method = symbol(&extracted, SymbolKind::Method, "Thing:Apply");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "Osi.ApplyStatus")
            .owned_by(&method.id)
            .found_in(&extracted)
    );
}

#[test]
fn lua_computed_callees_and_interpolated_requires_are_not_named() {
    let source = format!(
        "local function run()\n  get([[{SECRET_SENTINEL}]]).go()\n  get(1):go()\n  a.b.c()\n  obj:method()\n  spaced . call()\n  return (wrapped).call()\nend\n"
    );
    let extracted = extract("src/computed.lua", &source);
    assert!(!format!("{extracted:?}").contains(SECRET_SENTINEL));
    let run = symbol(&extracted, SymbolKind::Function, "run");
    for name in ["get", "a.b.c", "obj:method", "spaced.call", "wrapped.call"] {
        assert!(
            ReferenceQuery::new(ReferenceKind::Calls, name)
                .owned_by(&run.id)
                .found_in(&extracted),
            "{name}"
        );
    }
    assert_eq!(
        ReferenceQuery::new(ReferenceKind::Calls, "get")
            .owned_by(&run.id)
            .count_in(&extracted),
        2
    );
    assert!(
        !extracted
            .references
            .iter()
            .any(|reference| reference.name.contains("go")),
        "a call on a computed table has no stable name: {:?}",
        extracted.references
    );
    let luau = extract("src/dynamic.luau", "local m = require(`pkg/{suffix}`)\n");
    assert_eq!(names_of_kind(&luau, SymbolKind::Import), Vec::<&str>::new());
    assert_eq!(luau.import_bindings.len(), 0);
    assert!(
        !luau
            .references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::Imports)
    );
}

#[test]
fn lua_extraction_is_deterministic_and_literal_free() {
    let source = format!(
        "local token = \"{SECRET_SENTINEL}\"\nlocal long = [[{SECRET_SENTINEL}]]\nlocal function open(v) return check(\"{SECRET_SENTINEL}\") end\n"
    );
    let first = extract("src/vault.lua", &source);
    let second = extract("src/vault.lua", &source);
    assert_eq!(first, second);
    assert!(!format!("{first:?}").contains(SECRET_SENTINEL));
    let typed = format!(
        "local function pick(mode: \"{SECRET_SENTINEL}\"): \"{SECRET_SENTINEL}\" return mode end\n"
    );
    let luau = extract("src/vault.luau", &typed);
    assert!(!format!("{luau:?}").contains(SECRET_SENTINEL));
    assert!(
        symbol(&luau, SymbolKind::Function, "pick")
            .signature
            .is_none()
    );
}

#[test]
fn lua_function_assignments_name_only_static_targets() {
    let extracted = extract(
        "src/computed.lua",
        "make().f = function() body() end\nt[123].g = function() end\n(M).h = function() inner() end\na.b.c = function() end\na.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r = function() deep() end\n",
    );
    let deep_path = "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r";
    assert_eq!(
        names_of_kind(&extracted, SymbolKind::Function),
        ["M.h", "a.b.c", deep_path]
    );
    let deep = symbol(&extracted, SymbolKind::Function, deep_path);
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "deep")
            .owned_by(&deep.id)
            .found_in(&extracted),
        "a long static path still declares its function"
    );
    assert!(
        !extracted
            .symbols
            .iter()
            .any(|symbol| symbol.name.contains("123") || symbol.name.contains('(')),
        "computed targets declare nothing: {:?}",
        extracted.symbols
    );
    // A name exactly at the retained-name bound is still a declaration.
    let longest = "x".repeat(LONGEST_NAME_BYTES);
    let bounded = extract(
        "src/longest.lua",
        &format!("{longest} = function() body() end\n{longest}()\n"),
    );
    let declared = symbol(&bounded, SymbolKind::Function, &longest);
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "body")
            .owned_by(&declared.id)
            .found_in(&bounded)
    );
    assert!(ReferenceQuery::new(ReferenceKind::Calls, &longest).found_in(&bounded));
    for call in ["make", "body"] {
        assert!(
            ReferenceQuery::new(ReferenceKind::Calls, call).found_in(&extracted),
            "{call} still executes at file scope: {:?}",
            extracted.references
        );
    }
    let assigned = symbol(&extracted, SymbolKind::Function, "M.h");
    assert!(
        ReferenceQuery::new(ReferenceKind::Calls, "inner")
            .owned_by(&assigned.id)
            .found_in(&extracted)
    );
}

#[test]
fn lua_wide_local_lists_take_linear_work() {
    assert_linear_work("src/wide.lua", |width| {
        format!("local {} = nil\n", numbered_names("name", width, ", "))
    });
}

#[test]
fn lua_and_luau_loads_screen_credentials_before_emitting_import_facts() {
    for path in ["main.lua", "main.luau", "main.khn"] {
        credential_support::assert_screened(path, "require(\"@VALUE@\")\n", "token");
    }
}
