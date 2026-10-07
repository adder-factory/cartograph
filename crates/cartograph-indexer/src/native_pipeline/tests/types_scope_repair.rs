//! Receiver proofs must preserve the base resolver across unsupported bindings.

use super::{
    CapabilityReferenceQuery, ReferenceKind, TEST_GENERATION_BYTES, build_capability_generation,
    capability_symbol, generic_repair, types_track,
};

const VERIFIER_CASES: &[(&str, &str, &str, &str)] = &[
    (
        "test.js",
        "class Repo { save() {} } class Other { save() {} } const value = new Repo(); function run(value) { value.save(); } run(new Other());",
        "run",
        "value.save",
    ),
    (
        "test.js",
        "class Repo { save() {} } class Other { save() {} } function run() { const value = new Repo(); for (const value of [new Other()]) { value.save(); } }",
        "run",
        "value.save",
    ),
    (
        "test.dart",
        "class Repo { void save() {} } class Other { void save() {} } void run() { final local = Repo(); for (final local in [Other()]) { local.save(); } }",
        "run",
        "local.save",
    ),
    (
        "test.cpp",
        "struct Other { void save() {} }; struct Repo { void save() {} Other other; Other* operator->() { return &other; } }; void run(Repo value) { value->save(); }",
        "run",
        "value.save",
    ),
];

const LANGUAGE_SHADOW_CASES: &[(&str, &str, &str, &str)] = &[
    (
        "test.java",
        "class Repo { void save() {} } class Other { void save() {} } class Service { Repo value; void run(Object value) { value.save(); } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.kt",
        "class Repo {\n fun save() {}\n}\nclass Other {\n fun save() {}\n}\nclass Service(val value: Repo) {\n fun run(values: List<Other>) {\n for (value in values) { value.save() }\n }\n}\n",
        "Service::run",
        "value.save",
    ),
    (
        "test.cs",
        "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { Repo value; void Run(object value) { value.Save(); } }",
        "Service::Run",
        "value.Save",
    ),
    (
        "test.swift",
        "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { var value: Repo; func run(values: [Other]) { for value in values { value.save() } } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.dart",
        "class Repo { void save() {} } class Other { void save() {} } class Service { Repo value; void run(dynamic value) { value.save(); } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.cpp",
        "struct Repo { void save() {} }; struct Other { void save() {} }; void run(Repo value) { for (auto value : others) { value.save(); } }",
        "run",
        "value.save",
    ),
    (
        "test.scala",
        "class Repo { def save(): Unit = {} }\nclass Other { def save(): Unit = {} }\nclass Service(val value: Repo) { def run(values: List[Other]): Unit = { for (value <- values) value.save() } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.rb",
        "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n value = Repo.new\n others.each do |value|\n value.save\n end\nend",
        "run",
        "value.save",
    ),
    (
        "test.cls",
        "public class Repo { public void save() {} } public class Other { public void save() {} } public class Service { Repo value; void run(Object value) { value.save(); } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.sol",
        "contract Repo { function save() public {} } contract Other { function save() public {} } contract Service { Repo value; function run(uint value) public { value.save(); } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.ml",
        "class repo = object method save = () end\nclass other = object method save = () end\nlet run () = let value = new repo in let nested value = value#save in nested unknown",
        "run.nested",
        "save",
    ),
    (
        "test.ps1",
        "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nfunction Run { $local = [Repo]::new(); & { param($local) $local.Save() } }",
        "Run",
        "Save",
    ),
    (
        "test.pas",
        "unit Test; interface type TRepo = class procedure Save(); end; TOther = class procedure Save(); end; implementation procedure Run(param: TRepo); begin for param in others do param.Save(); end; end.",
        "Run",
        "param.Save",
    ),
    (
        "test.m",
        "@interface Repo\n- (void)save;\n@end\n@interface Other\n- (void)save;\n@end\nvoid run(Repo *value) { void (^block)(id) = ^(id value) { [value save]; }; }",
        "run",
        "value.save",
    ),
    (
        "test.ts",
        "class Repo { save() {} } class Other { save() {} } function run(value: Repo) { consume(value => value.save()); }",
        "run",
        "value.save",
    ),
    (
        "test.js",
        "class Repo { save() {} } class Other { save() {} } function run() { const value = new Repo(); consume(value => value.save()); }",
        "run",
        "value.save",
    ),
];

const SIBLING_BINDER_CASES: &[(&str, &str, &str, &str)] = &[
    (
        "test.java",
        "class Repo { void save() {} } class Other { void save() {} } class Service { Repo value; void run(Object input) { if (input instanceof Other value) { value.save(); } } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.dart",
        "class Repo { void save() {} } class Other { void save() {} } class Service { Repo value; void run(dynamic payload) { var (value,) = payload; value.save(); } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.dart",
        "class Repo { void save() {} } class Other { void save() {} } void run(dynamic payload) { var value = Repo(); (value,) = payload; value.save(); }",
        "run",
        "value.save",
    ),
    (
        "test.rb",
        "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n value = Repo.new\n other => value\n value.save\nend",
        "run",
        "value.save",
    ),
    (
        "test.cs",
        "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { Repo value; void Run(object input) { if (input is Other {} value) { value.Save(); } } }",
        "Service::Run",
        "value.Save",
    ),
    (
        "test.cs",
        "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { Repo value; void Run() { Get(out var value); value.Save(); } }",
        "Service::Run",
        "value.Save",
    ),
    (
        "test.cs",
        "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { Repo value; void Run(object values) { var query = from value in values select value.Save(); } }",
        "Service::Run",
        "value.Save",
    ),
    (
        "test.swift",
        "class Repo { func save() {} }\nclass Other { func save() {} }\nclass Service { var value: Repo; func run(input: Other) { switch input { case let value: value.save() } } }",
        "Service::run",
        "value.save",
    ),
    (
        "test.ps1",
        "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nclass Service { [Repo]$value; [void] Run([object]$value) { $value.Save() } }",
        "Service::Run",
        "Save",
    ),
    (
        "test.kt",
        "class Repo {\n fun save() {}\n}\nclass Other {\n fun save() {}\n}\nclass Service(val value: Repo) {\n fun run(payload: Pair<Other, Other>) {\n val (value, unused) = payload\n value.save()\n }\n}\n",
        "Service::run",
        "value.save",
    ),
    (
        "test.pas",
        "unit Test; interface type TRepo = class procedure Save(); end; TOther = class procedure Save(); end; implementation procedure Run(param: TRepo); begin try Work(); except on param: Exception do param.Save(); end; end; end.",
        "Run",
        "param.Save",
    ),
    (
        "test.cpp",
        "struct Repo { void save() {} }; struct Other { void save() {} }; void run(Repo value) { { Other (value); value.save(); } }",
        "run",
        "value.save",
    ),
    (
        "test.cpp",
        "namespace ns { struct Other { void save() {} }; } struct Repo { void save() {} }; void run(Repo value) { { ns::Other(value); value.save(); } }",
        "run",
        "value.save",
    ),
    (
        "test.cpp",
        "template<typename T> struct Other { void save() {} }; struct Repo { void save() {} }; void run(Repo value) { { Other<int>(value); value.save(); } }",
        "run",
        "value.save",
    ),
    (
        "test.cpp",
        "struct Other { void save() {} }; using Alias = Other; struct Repo { void save() {} }; void run(Repo value) { { Alias(value); value.save(); } }",
        "run",
        "value.save",
    ),
    (
        "test.cpp",
        "struct Other { void save() {} }; struct Repo { void save() {} }; void run(Repo value) { { Other((value)); value.save(); } }",
        "run",
        "value.save",
    ),
];

#[test]
fn types_scope_repair_verifier_counterexamples_preserve_base_resolution() {
    for &(path, source, caller, name) in VERIFIER_CASES {
        assert_preserves_base((path, source), (caller, name));
    }
}

#[test]
fn types_scope_repair_each_adapter_stops_at_nearer_parameters_or_loop_binders() {
    for &(path, source, caller, name) in LANGUAGE_SHADOW_CASES {
        assert_preserves_base((path, source), (caller, name));
    }
}

#[test]
fn types_scope_repair_patterns_parameters_and_unsupported_declarators_preserve_base() {
    for &(path, source, caller, name) in SIBLING_BINDER_CASES {
        assert_preserves_base((path, source), (caller, name));
    }
}

#[test]
fn types_scope_repair_known_annotations_survive_ordinary_writes() {
    let source = "unit Test; interface type TRepo = class procedure Save(); end; TOther = class procedure Save(); end; implementation procedure Run(param: TRepo); begin param := Unknown; param.Save(); end; end.";
    let facts = build_capability_generation(&[("test.pas", source)], false);
    types_track::assert_target(
        &facts,
        ("test.pas", "Run", "param.Save"),
        ("test.pas", "TRepo::Save", "native-explicit-receiver-type"),
    );
    let source = "struct Repo { void save() {} }; struct Holder { Repo *field; }; void run(Holder value) { value.field->save(); }";
    let facts = build_capability_generation(&[("test.cpp", source)], false);
    types_track::assert_target(
        &facts,
        ("test.cpp", "run", "value.field.save"),
        ("test.cpp", "Repo::save", "native-explicit-receiver-type"),
    );
    let source = "struct Repo { void save() {} }; void Other(Repo input) {} void run(Repo value) { { Other(value); value.save(); } }";
    assert_preserves_base(("test.cpp", source), ("run", "value.save"));
}

#[test]
fn types_scope_repair_iteration_assignments_and_destructuring_invalidate_constructors() {
    let prefix = "class Repo { save() {} } class Other { save() {} } function run() { let value = new Repo(); ";
    for body in [
        "for (value of [new Other()]) { value.save(); }",
        "for (value of [new Other()]) {} value.save();",
        "for (var value of [new Other()]) {} value.save();",
        "({value} = payload); value.save();",
        "consume(({value}) => value.save());",
        "consume((value = new Other()) => value.save());",
        "for (const {value} of payload) { value.save(); }",
    ] {
        let source = format!("{prefix}{body} }}");
        assert_preserves_base(("test.js", &source), ("run", "value.save"));
    }
    let dart = "class Repo { void save() {} } class Other { void save() {} } void run() { var local = Repo(); for (local in [Other()]) {} local.save(); }";
    assert_preserves_base(("test.dart", dart), ("run", "local.save"));
}

#[test]
fn types_scope_repair_cpp_arrow_requires_a_declared_raw_pointer() {
    let prefix = "struct Other { void save() {} }; struct Repo { void save() {} Other other; Other* operator->() { return &other; } }; ";
    for parameter in ["Repo value", "Repo &value", "Repo &&value", "Repo **value"] {
        let source = format!("{prefix}void run({parameter}) {{ value->save(); }}");
        assert_preserves_base(("test.cpp", &source), ("run", "value.save"));
    }
    for (parameter, access) in [
        ("Repo value", "."),
        ("Repo &value", "."),
        ("Repo *value", "->"),
        ("Repo *&value", "->"),
    ] {
        let source = format!("{prefix}void run({parameter}) {{ value{access}save(); }}");
        let facts = build_capability_generation(&[("test.cpp", &source)], false);
        types_track::assert_target(
            &facts,
            ("test.cpp", "run", "value.save"),
            ("test.cpp", "Repo::save", "native-explicit-receiver-type"),
        );
    }
}

#[test]
fn types_scope_repair_increment_grammar_variants_invalidate_constructors() {
    let fixtures = [
        (
            "test.cs",
            "class Repo { public void Save() {} public static Repo operator ++(Repo v) { return v; } } class Other { public void Save() {} } class Service { void Run() { var value = new Repo(); value++; value.Save(); } }",
            "Service::Run",
            "value.Save",
        ),
        (
            "test.ps1",
            "class Repo { [void] Save() {} }\nclass Other { [void] Save() {} }\nfunction Run { $local = [Repo]::new(); $local++; $local.Save() }",
            "Run",
            "Save",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Other { void save() {} } void run() { var local = Repo(); local++; local.save(); }",
            "run",
            "local.save",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Other { void save() {} } void run() { var local = Repo(); { ++local; } local.save(); }",
            "run",
            "local.save",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Other { void save() {} } void run() { var local = Repo(); { local++; } local.save(); }",
            "run",
            "local.save",
        ),
    ];
    for (path, source, caller, name) in fixtures {
        assert_preserves_base((path, source), (caller, name));
    }
}

#[test]
fn types_scope_repair_nested_destructuring_writes_invalidate_outer_constructors() {
    let fixtures = [
        (
            "test.rb",
            "class Repo\n def save; end\nend\nclass Other\n def save; end\nend\ndef run\n value = Repo.new\n once { value, unused = [Other.new, nil] }\n value.save\nend",
            "run",
            "value.save",
        ),
        (
            "test.cs",
            "class Repo { public void Save() {} } class Other { public void Save() {} } class Service { void Run() { var value = new Repo(); { (value, unused) = Payload(); } value.Save(); } }",
            "Service::Run",
            "value.Save",
        ),
        (
            "test.dart",
            "class Repo { void save() {} } class Other { void save() {} } void run(dynamic payload) { var value = Repo(); { (value,) = payload; } value.save(); }",
            "run",
            "value.save",
        ),
    ];
    for (path, source, caller, name) in fixtures {
        assert_preserves_base((path, source), (caller, name));
    }
}

#[test]
fn types_scope_repair_loop_declarations_preserve_outer_constructor_after_the_loop() {
    for declaration in ["const", "let"] {
        let source = format!(
            "class Repo {{ save() {{}} }} class Other {{ save() {{}} }} function run() {{ const value = new Repo(); for ({declaration} value of payload) {{ consume(value); }} value.save(); }}"
        );
        let facts = build_capability_generation(&[("test.js", &source)], false);
        types_track::assert_target(
            &facts,
            ("test.js", "run", "value.save"),
            ("test.js", "Repo::save", "native-explicit-receiver-type"),
        );
    }
}

fn assert_preserves_base(fixture: (&str, &str), call: (&str, &str)) {
    let fixtures = [fixture];
    let facts = build_capability_generation(&fixtures, false);
    assert!(
        facts
            .files()
            .iter()
            .all(|file| { file.parse_status == cartograph_domain::FileParseStatus::Parsed }),
        "fixture must parse completely: {}",
        fixture.0
    );
    types_track::assert_abstains(&facts, (fixture.0, call.0, call.1));
    let base = generic_repair::build_generation(
        generic_repair::CapabilityGenerationRequest {
            fixtures: &fixtures,
            reverse: false,
            wider_partial_band: false,
            maximum_bytes: TEST_GENERATION_BYTES,
        },
        |file| file.receiver_evidence = None,
        || false,
    );
    let owner = capability_symbol(&facts, fixture.0, call.0);
    let reference =
        CapabilityReferenceQuery::new(&facts, owner).named(call.1, ReferenceKind::Calls);
    generic_repair::assert_base_reference(&base, reference);
}
