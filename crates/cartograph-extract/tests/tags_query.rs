//! Integration coverage for Cartograph native extraction contracts.

mod credential_support;
mod dependency_ownership;

use std::collections::BTreeMap;

use cartograph_domain::{FileParseStatus, ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractError, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

#[test]
fn elixir_tags_query_matches_v1_facts_and_improves_exact_reference_spans() {
    let extracted = extract(
        "lib/calculator.ex",
        r#"defmodule Calculator do
  @moduledoc "A tiny calculator."

  def add(a, b) do
    sum(a, b)
  end

  defp sum(a, b), do: a + b

  defmacro trace(expr), do: expr

  def countdown(0), do: :done
  def countdown(n), do: countdown(n - 1)

  defmodule Inner do
    def noop, do: :ok
  end
end
"#,
    );

    assert_eq!(
        canonical_facts(&extracted),
        [
            "S|module|Calculator|Calculator|0-286|004e429abd4af74d9ba5d9579b399d92b2b0cc8370e7473d2279fa01d28fc5fa|",
            "S|function|add|Calculator.add|61-97|38ce48a82da9007e69fa5962d671f824c59f48902dfc1df9b7d433d60bdcc8f7|def add(a, b)",
            "S|function|sum|Calculator.sum|101-126|06866e26b947e176dde1d2becba6c1b3a02a7c616b42d0952a606cc1b8311e09|defp sum(a, b)",
            "S|function|trace|Calculator.trace|130-160|a9a1877bbc124a7559f57db77e6866c10b7d846555eb1dba966d1fcea673b9c2|defmacro trace(expr)",
            "S|function|countdown|Calculator.countdown|164-191|fa53feee4d091c7d175086485ba051727c53544c0bc408da5419ec1620cf6fb9|",
            "S|function|countdown|Calculator.countdown|194-232|4cefdd0c4646f14aabebb74510ecfb14fdd185b384ef6a270b8c6ca0655dbef0|def countdown(n)",
            "S|module|Inner|Calculator.Inner|236-282|e290ee87820b6770e8d34c0bd91bb51f27b26df2ac89ae51db61ed88fb416d32|",
            "S|function|noop|Calculator.Inner.noop|259-276|b8f24dffe1f376aec6383c87bb3e97f66d4d4f6cc8c3be284d05793a91a08837|def noop",
            "C|Calculator|Calculator.add",
            "C|Calculator|Calculator.sum",
            "C|Calculator|Calculator.trace",
            "C|Calculator|Calculator.countdown",
            "C|Calculator|Calculator.countdown",
            "C|Calculator|Calculator.Inner",
            "C|Calculator.Inner|Calculator.Inner.noop",
            "R|Calculator.add|sum|calls|82-85",
            "R|Calculator.countdown|countdown|calls|216-225",
        ]
    );
    assert_eq!(extracted.diagnostics, []);
    assert!(extracted.import_bindings.iter().all(|binding| {
        binding.kind == cartograph_extract::ImportBindingKind::Namespace
            && binding.module_specifier == "<elixir-public-function>"
            && binding.imported_name == "*"
    }));
    let public_members: Vec<_> = extracted
        .import_bindings
        .iter()
        .map(|binding| {
            (
                binding.local_name.as_str(),
                binding.span.start_line(),
                binding.span.end_line(),
            )
        })
        .collect();
    assert_eq!(
        public_members,
        [
            ("Calculator.add", 4, 6),
            ("Calculator.trace", 10, 10),
            ("Calculator.countdown", 12, 12),
            ("Calculator.countdown", 13, 13),
            ("Calculator.Inner.noop", 16, 16),
        ]
    );

    let top_level = extract("script.exs", "IO.puts(\"hello\")\n");
    assert_eq!(canonical_facts(&top_level), ["R|<file>|puts|calls|3-7"]);
}

#[test]
fn julia_short_form_functions_declare_module_members_and_own_body_calls() {
    let source = "module Shapes\narea(c::Circle) = helper(c)\ntyped(x)::Float64 = convert(Float64, x)\nidentity(x::T) where T = helper(x)\nbounded(x::T)::T where {T<:Real} = helper(x)\nvalue = area(circle)\narray[index()] = helper(value)\nfunction long(x)\n    helper(x)\nend\nend\n";
    let file = extract("src/Shapes.jl", source);
    let module = file
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Module && symbol.name == "Shapes")
        .unwrap_or_else(|| panic!("missing module: {:?}", file.symbols));
    for (name, line, callee) in [
        ("area", 2, "helper"),
        ("typed", 3, "convert"),
        ("identity", 4, "helper"),
        ("bounded", 5, "helper"),
        ("long", 8, "helper"),
    ] {
        let function = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Function && symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", file.symbols));
        assert_eq!(function.qualified_name, format!("Shapes.{name}"));
        assert_eq!(function.span.start_line(), line);
        if name != "long" {
            assert_eq!(function.span.end_line(), line);
            let declaration = source
                .lines()
                .nth(line as usize - 1)
                .unwrap_or_else(|| panic!("missing fixture line {line}"));
            let start = usize::try_from(function.span.start_byte())
                .unwrap_or_else(|error| panic!("invalid start span: {error}"));
            let end = usize::try_from(function.span.end_byte())
                .unwrap_or_else(|error| panic!("invalid end span: {error}"));
            assert_eq!(&source[start..end], declaration);
        }
        assert!(file.containments.iter().any(|containment| {
            containment.parent == module.id && containment.child == function.id
        }));
        assert!(file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == callee
                && reference.owner.as_ref() == Some(&function.id)
        }));
        assert!(!file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == name
                && reference.span.start_line() == line
        }));
    }
    for (name, line) in [("area", 6), ("index", 7), ("helper", 7)] {
        assert!(file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == name
                && reference.span.start_line() == line
                && reference.owner.as_ref() == Some(&module.id)
        }));
        assert!(!file.symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Function && symbol.span.start_line() == line
        }));
    }
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn julia_parity_corpus_short_form_area_has_its_exact_declaration() {
    let file = extract(
        "src/Shapes.jl",
        include_str!("fixtures/v1_parity/julia/src/Shapes.jl"),
    );
    let area = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "area" && symbol.span.start_line() == 35)
        .unwrap_or_else(|| panic!("missing short-form area: {:?}", file.symbols));
    assert_eq!(area.kind, SymbolKind::Function);
    assert_eq!(area.qualified_name, "Shapes.area");
    assert_eq!(area.span.end_line(), 35);
    assert!(!file.references.iter().any(|reference| {
        reference.kind == ReferenceKind::Calls && reference.span.start_line() == 35
    }));
}

#[test]
fn julia_commented_short_form_signatures_declare_and_own_rhs_calls() {
    for (signature, name) in [
        ("f(x) #= note =#", "f"),
        ("f(x) #= first =# #= second =#", "f"),
        ("#= before =# f(x)", "f"),
        (
            "f(x) #= typed =# ::T #= annotation =# where T #= assignment =#",
            "f",
        ),
        ("f(x) where #= bound =# T where S #= assignment =#", "f"),
        ("Base #= module =#.show(x) #= assignment =#", "Base.show"),
        ("Base. #= field =# show(x)", "Base.show"),
        ("Base.:( #= quote =# == #= trailing =# )(a,b)", "Base.:(==)"),
        (":( #= inner =# == #= trailing =# )(a,b)", ":(==)"),
        (":(( #= nested =# == #= trailing =# ))(a,b)", ":((==))"),
    ] {
        let source = format!("module M\n{signature} = helper(x) #= trailing =#\nend\n");
        let file = extract("src/Commented.jl", &source);
        let module = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Module)
            .unwrap_or_else(|| panic!("missing module: {signature}"));
        let function = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Function)
            .unwrap_or_else(|| panic!("missing declaration: {signature}: {:?}", file.references));
        assert_eq!(function.name, name, "{signature}");
        let qualified = if name.starts_with("Base.") {
            name.to_owned()
        } else {
            format!("M.{name}")
        };
        assert_eq!(function.qualified_name, qualified, "{signature}");
        assert!(
            file.containments
                .iter()
                .any(|edge| edge.parent == module.id && edge.child == function.id),
            "{signature}"
        );
        assert_eq!(
            file.references.len(),
            1,
            "signature is not a call: {signature}"
        );
        let call = &file.references[0];
        assert_eq!(call.name, "helper", "{signature}");
        assert_eq!(call.kind, ReferenceKind::Calls);
        assert_eq!(call.owner.as_ref(), Some(&function.id), "{signature}");
        assert_eq!(file.parse_status, FileParseStatus::Parsed, "{signature}");
    }
}

#[test]
fn julia_parameter_comments_never_enter_declaration_facts() {
    const CANARY: &str = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    for declaration in [
        format!("f(x # {CANARY}\n) #= note =# = helper(x)"),
        format!("f(x # {CANARY} )\n) #= note =# = helper(x)"),
        format!("f(x #= {CANARY} =#) #= note =# = helper(x)"),
        format!("function f(x # {CANARY}\n)\nhelper(x)\nend"),
    ] {
        let source = format!("module M\n{declaration}\nend\n");
        let file = extract("src/Comments.jl", &source);
        let function = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Function && symbol.name == "f")
            .unwrap_or_else(|| panic!("missing declaration: {:?}", file.symbols));
        assert_eq!(function.qualified_name, "M.f");
        let helper = file
            .references
            .iter()
            .find(|reference| reference.kind == ReferenceKind::Calls && reference.name == "helper")
            .unwrap_or_else(|| panic!("missing RHS call: {:?}", file.references));
        assert_eq!(helper.owner.as_ref(), Some(&function.id));
        assert!(!format!("{file:?}").contains("ghp_"));
        assert_eq!(function.signature, None);
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn julia_commented_non_signatures_keep_module_call_owners() {
    for body in [
        "Base.:(a #= tuple =#,b)(x) = helper(x)",
        "f(x) #= operator =# += helper(x)",
        "expr = quote; f(x) #= quoted =# = helper(x); end",
    ] {
        let source = format!("module M\n{body}\nend\n");
        let file = extract("src/Commented.jl", &source);
        let module = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Module)
            .unwrap_or_else(|| panic!("missing module: {body}"));
        assert!(
            file.symbols
                .iter()
                .all(|symbol| symbol.kind != SymbolKind::Function),
            "{body}"
        );
        let call = file
            .references
            .iter()
            .find(|reference| reference.name == "helper")
            .unwrap_or_else(|| panic!("missing RHS call: {body}"));
        assert_eq!(call.kind, ReferenceKind::Calls);
        assert_eq!(call.owner.as_ref(), Some(&module.id), "{body}");
        assert_eq!(file.parse_status, FileParseStatus::Parsed, "{body}");
    }
}

#[test]
fn julia_chained_where_signatures_are_declarations_not_calls() {
    let file = extract(
        "src/Shapes.jl",
        "module Shapes\nnested(x::T, y::S) where T where S = helper(x, y)\nrecursive(x) where T where S where U = recursive(helper(x))\nend\n",
    );
    let module = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Shapes")
        .unwrap_or_else(|| panic!("missing Shapes module"));
    for (name, line) in [("nested", 2), ("recursive", 3)] {
        let function = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name} declaration"));
        assert_eq!(function.kind, SymbolKind::Function);
        assert_eq!(function.qualified_name, format!("Shapes.{name}"));
        assert_eq!(function.span.start_line(), line);
        assert_eq!(function.span.end_line(), line);
        assert!(file.containments.iter().any(|containment| {
            containment.parent == module.id && containment.child == function.id
        }));
        assert!(file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == "helper"
                && reference.owner.as_ref() == Some(&function.id)
        }));
    }
    let calls = file
        .references
        .iter()
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(calls, ["helper", "recursive", "helper"]);
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn julia_qualified_and_operator_short_forms_own_their_rhs_calls() {
    let source = "module Extensions\nBase.show(io, x) = helper(x)\nBase.:+(a,b) = combine(a,b)\n+(a,b) = combine(a,b)\nBase.Math.show(io, x)::String where T = helper(x)\nend\n";
    let file = extract("src/Extensions.jl", source);
    let module = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Extensions")
        .unwrap_or_else(|| panic!("missing Extensions module"));
    for (name, qualified, line, callee) in [
        ("Base.show", "Base.show", 2, "helper"),
        ("Base.:+", "Base.:+", 3, "combine"),
        ("+", "Extensions.+", 4, "combine"),
        ("Base.Math.show", "Base.Math.show", 5, "helper"),
    ] {
        let function = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Function && symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", file.symbols));
        assert_eq!(function.qualified_name, qualified);
        assert_eq!(function.span.start_line(), line);
        assert_eq!(function.span.end_line(), line);
        let start = usize::try_from(function.span.start_byte())
            .unwrap_or_else(|error| panic!("invalid span: {error}"));
        let end = usize::try_from(function.span.end_byte())
            .unwrap_or_else(|error| panic!("invalid span: {error}"));
        assert_eq!(
            &source[start..end],
            source.lines().nth(line as usize - 1).unwrap_or_default()
        );
        assert!(file.containments.iter().any(|containment| {
            containment.parent == module.id && containment.child == function.id
        }));
        assert!(file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == callee
                && reference.owner.as_ref() == Some(&function.id)
        }));
    }
    assert_eq!(file.references.len(), 4, "signatures are not call sites");
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn julia_quoted_parenthesized_operator_short_forms_own_their_rhs_calls() {
    let source = "module Extensions\nBase.:(==)(a,b) = equal(a,b)\n:(==)(a,b) = equal(a,b)\nBase.:((==))(a,b) = equal(a,b)\n:((==))(a,b) = equal(a,b)\nend\n";
    let file = extract("src/Extensions.jl", source);
    let module = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Extensions")
        .unwrap_or_else(|| panic!("missing Extensions module"));
    for (name, qualified, line) in [
        ("Base.:(==)", "Base.:(==)", 2),
        (":(==)", "Extensions.:(==)", 3),
        ("Base.:((==))", "Base.:((==))", 4),
        (":((==))", "Extensions.:((==))", 5),
    ] {
        let function = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Function && symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", file.symbols));
        assert_eq!(function.qualified_name, qualified);
        assert_eq!(function.span.start_line(), line);
        assert_eq!(function.span.end_line(), line);
        let start = usize::try_from(function.span.start_byte())
            .unwrap_or_else(|error| panic!("invalid span: {error}"));
        let end = usize::try_from(function.span.end_byte())
            .unwrap_or_else(|error| panic!("invalid span: {error}"));
        assert_eq!(
            &source[start..end],
            source.lines().nth(line as usize - 1).unwrap_or_default()
        );
        assert!(file.containments.iter().any(|containment| {
            containment.parent == module.id && containment.child == function.id
        }));
        assert!(file.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls
                && reference.name == "equal"
                && reference.owner.as_ref() == Some(&function.id)
        }));
    }
    assert_eq!(file.references.len(), 4);
    assert_eq!(file.parse_status, FileParseStatus::Parsed);

    let rejected = extract(
        "src/Extensions.jl",
        "module Extensions\nBase.:(a,b)(x) = helper(x)\n:(a,b)(x) = helper(x)\nend\n",
    );
    assert_eq!(
        rejected.symbols.len(),
        1,
        "quoted tuples do not name functions"
    );
    assert!(rejected.references.iter().all(|reference| {
        reference.name == "helper" && reference.owner.as_ref() == Some(&rejected.symbols[0].id)
    }));
    assert_eq!(rejected.references.len(), 2);
    assert_eq!(rejected.parse_status, FileParseStatus::Parsed);
}

// Expectations captured from the native HEAD extractor at 5220c3a1.
#[test]
fn julia_quoted_short_form_assignments_keep_head_declarations_and_call_owners() {
    for (source, expected) in [
        (
            "module M\nexpr = :(Base.:(==)(a,b) = equal(a,b))\nend\n",
            &[
                "S|module|M|M|0-51|44f05c4ec8f7792abf193f5d678050ce9a6016df1307edd8e8c3a81ea3d627e8|",
                "R|M|equal|calls|36-41",
            ][..],
        ),
        (
            "module M\nexpr = quote; :(==)(a,b) = equal(a,b); end\nend\n",
            &[
                "S|module|M|M|0-55|6b2300a24175210dea8ce14626844048ef271adc4dae8152f9316ffc06a85a2a|",
                "R|M|equal|calls|36-41",
            ][..],
        ),
        (
            "module M\nexpr = quote; area(x) = equal(x,x); end\nend\n",
            &[
                "S|module|M|M|0-52|7aac7a3991105ba33702871164d92db05209661c4ca56af7e9a5d9b12880ee7d|",
                "R|M|area|calls|23-27",
                "R|M|equal|calls|33-38",
            ][..],
        ),
    ] {
        let file = extract("src/Quoted.jl", source);
        assert_eq!(canonical_facts(&file), expected);
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn remaining_tags_modes_have_locked_exact_structural_facts() {
    let cases = [
        (
            "Main.hs",
            "module M where\nfoo x = x + 1\ndata User = User Int\nclass C a where\n  run :: a -> Int\n",
            &[
                "S|module|M|M|0-14|e0cdae71da59afab791e9549beed4c7d2e58dd6fb4a05366dd3ee10739075ee1|",
                "S|function|foo|foo|15-28|f3371600832886141f320066dcc7800a92ad0c762a253d90e536be58c4d31b7b|foo x",
                "S|type_alias|User|User|29-49|b4ea1a811fae192bfaee939f44b1cbb3593f6b068de738f6e7b56d4b0ac1f7f0|",
                "S|class|C|C|50-83|b0fa7fb38d6da3fbcdfb260f1e71f70a290df6b8cb36a887fd1c9e56a53b919e|",
                "S|function|run|C.run|68-83|67f7349891f18ef69a1a2e1a4d3c7b9f10b9a9e8bee79c805ae430c5cb37f99a|run :: a -> Int",
                "C|C|C.run",
            ][..],
        ),
        (
            "main.jl",
            "module M\nstruct User\n  name::String\nend\nfunction greet(name)\n  show(name)\nend\nmacro m(x) x end\nend\n",
            &[
                "S|module|M|M|0-98|995b4090c669b90dab3ff615a56b1f530d2576c5d31d398697729696e2779092|",
                "S|struct|User|M.User|9-39|4e552a42a4baf064b2834c6c87ac561b4caa3b1da8eaa028da75e8a979db586a|",
                "S|function|greet|M.greet|40-77|6b51d314350cbbe7dc8c17527d85cd7722dc95aaab7a1c09a260d6d478ce6e58|function greet(name)",
                "S|function|m|M.m|78-94|84262ae5f9ae1370527fbfae088882594e21fd221f6500662419fd60260881e0|macro m(x)",
                "C|M|M.User",
                "C|M|M.greet",
                "C|M|M.m",
                "R|M.greet|show|calls|63-67",
            ][..],
        ),
        (
            "lib.ml",
            "let f x = x + 1\ntype user = { name : string }\n",
            &[
                "S|function|f|f|4-15|df52b74cb5b9c9a2fe0cc067597023154596505cec74ac763cdd966e972f9eed|f x",
                "S|type_alias|user|user|21-45|1db0b0d8dc38115d6d40c34a954f18c55e001009c7e6d484dc022d14252efce3|",
                "S|field|name|user.name|30-43|ae12573ba18ec637cd071f89976f73642b3a211f802e92bc0cc536fe49803663|",
                "C|user|user.name",
                "R|f|+|calls|12-13",
            ][..],
        ),
        (
            "lib.mli",
            "val f : int -> int\ntype user\n",
            &[
                "S|function|f|f|0-18|78eb46902467cd4d3650a87a965b19221d5d78d14954d91857347dedd9594faf|val f : int -> int",
                "S|type_alias|user|user|24-28|b021cbfadc103df81a86dcdfd6cbc1919314276355945bdeb71793452ba07be8|",
            ][..],
        ),
        (
            "top.sv",
            "module top;\nfunction int add; endfunction\ntask run; endtask\nendmodule\npackage P; endpackage\ninterface I; endinterface\nclass C; endclass\n",
            &[
                "S|module|top|top|0-69|42c5ccb84d05e304457aa6b248f545ee2c49f547148a696181a7f6c690d2d4aa|",
                "S|function|add|top.add|12-41|69029336541f3f8661c52763b39f8832648598787471ad6bc6e8c5ee4d70c0c3|function int add",
                "S|function|run|top.run|42-59|b4cc2c97cf342fadca34ca8a20a9a31a324f40ce039b7d3d8986e8bc7104dade|task run",
                "S|module|P|P|70-91|960c9245b9fb9e7ef7991f5e64033293c4805e0d84e037e67c080b5add2b4c2c|",
                "S|interface|I|I|92-117|eaa9191123fafd61381ddd32577963c2f60d0732a742b677bb89fc33f090861f|",
                "S|class|C|C|118-135|1015093741a208de31b36b3d374ed0de5fe6ecaaf8d6ed75bbf924326423487a|",
                "C|top|top.add",
                "C|top|top.run",
            ][..],
        ),
    ];

    for (path, source, expected) in cases {
        let extracted = extract(path, source);
        assert_eq!(canonical_facts(&extracted), expected, "{path}");
        assert!(extracted.diagnostics.is_empty(), "{path}");
        assert!(extracted.import_bindings.is_empty(), "{path}");
    }
}

#[test]
fn tags_queries_bound_failure_cancel_and_sensitive_text_paths() {
    let comment_only = extract("lib/empty.ex", "# just a comment\n");
    assert_eq!(comment_only.parse_status, FileParseStatus::Parsed);
    assert_eq!(canonical_facts(&comment_only), [] as [String; 0]);

    let damaged = extract("lib/broken.ex", "defmodule Broken do\n  def value(\n");
    assert_eq!(damaged.parse_status, FileParseStatus::Partial);
    assert_ne!(damaged.diagnostics, []);

    let documented = extract("lib.ml", "(** Adds one. *)\nlet f x = x + 1\n");
    assert_eq!(
        documented.symbols[0].docstring.as_deref(),
        Some("(** Adds one. *)")
    );
    assert!(
        canonical_facts(&documented)
            .iter()
            .any(|fact| fact == "D|f|(** Adds one. *)")
    );

    let sensitive = extract(
        "lib/secrets.ex",
        "defmodule Secrets do\n  def api_key, do: :sk_live_secret\nend\n",
    );
    let api_key = sensitive
        .symbols
        .iter()
        .find(|symbol| symbol.name == "api_key")
        .unwrap_or_else(|| panic!("api_key symbol was not extracted"));
    assert_eq!(api_key.signature.as_deref(), Some("def api_key"));
    assert!(!format!("{sensitive:?}").contains("sk_live_secret"));

    let limits = limits();
    let snapshot = SourceSnapshot::from_bytes("lib/cancel.ex", b"def value, do: :ok\n", limits)
        .unwrap_or_else(|error| panic!("cancel fixture snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(SourceLanguage::Elixir)
        .unwrap_or_else(|error| panic!("Elixir extractor failed: {error}"));
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || true),
        Err(ExtractError::Cancelled)
    );

    let mut adversarial = String::new();
    for index in 0..17_000 {
        adversarial.push_str("def value_");
        adversarial.push_str(&index.to_string());
        adversarial.push_str(", do: :ok\n");
    }
    assert_eq!(
        extract_result("lib/too_many.ex", &adversarial),
        Err(ExtractError::OutputLimit)
    );

    let mut nested = String::from("defmodule Deep do\n  def value, do: ");
    for _ in 0..300 {
        nested.push('[');
    }
    nested.push_str(":ok");
    for _ in 0..300 {
        nested.push(']');
    }
    nested.push_str("\nend\n");
    let recovered = extract_result("lib/too_deep.ex", &nested)
        .unwrap_or_else(|error| panic!("deep Elixir extraction did not recover: {error}"));
    assert_eq!(
        recovered.parse_status,
        cartograph_domain::FileParseStatus::Partial
    );
    assert_eq!(
        recovered.diagnostics[0].code,
        cartograph_extract::DiagnosticCode::NestingLimitExceeded
    );
}

#[test]
fn definitions_sharing_one_syntax_node_all_extract_instead_of_failing_the_file() {
    let haskell = extract(
        "src/Util/Strings.hs",
        "module Util.Strings (shout) where\n\nshout :: String -> String\nshout = map toUpper\n",
    );
    let elixir = extract(
        "lib/shop/cart.ex",
        "defmodule Shop.Cart do\n  defstruct items: []\n  def new, do: %__MODULE__{}\nend\n",
    );

    assert_eq!(
        top_level_symbols(&haskell),
        [
            ("module", "Strings"),
            ("module", "Util"),
            ("function", "shout"),
        ]
    );
    assert_eq!(
        top_level_symbols(&elixir),
        [("module", "Shop.Cart"), ("struct", "Shop.Cart")]
    );
    let new = elixir
        .symbols
        .iter()
        .find(|symbol| symbol.name == "new")
        .unwrap_or_else(|| panic!("Shop.Cart.new was not extracted: {:?}", elixir.symbols));
    assert_eq!(new.qualified_name, "Shop.Cart.new");
    for extracted in [&haskell, &elixir] {
        assert_eq!(extracted.parse_status, FileParseStatus::Parsed);
        let mut ids = extracted
            .symbols
            .iter()
            .map(|symbol| symbol.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), extracted.symbols.len(), "symbol ids collide");
    }
}

/// Kind and qualified name of every symbol no containment nests.
fn top_level_symbols(extracted: &ExtractedFile) -> Vec<(&str, &str)> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| {
            extracted
                .containments
                .iter()
                .all(|containment| containment.child != symbol.id)
        })
        .map(|symbol| (symbol.kind.as_str(), symbol.qualified_name.as_str()))
        .collect()
}

fn canonical_facts(extracted: &ExtractedFile) -> Vec<String> {
    let mut facts = Vec::new();
    let names = extracted
        .symbols
        .iter()
        .map(|symbol| (symbol.id.as_str(), symbol.qualified_name.as_str()))
        .collect::<BTreeMap<_, _>>();
    for symbol in &extracted.symbols {
        facts.push(format!(
            "S|{}|{}|{}|{}-{}|{}|{}",
            symbol.kind.as_str(),
            symbol.name,
            symbol.qualified_name,
            symbol.span.start_byte(),
            symbol.span.end_byte(),
            symbol.structural_digest.as_str(),
            symbol.signature.as_deref().unwrap_or_default(),
        ));
        if let Some(docstring) = &symbol.docstring {
            facts.push(format!("D|{}|{}", symbol.qualified_name, docstring));
        }
    }
    for containment in &extracted.containments {
        facts.push(format!(
            "C|{}|{}",
            names
                .get(containment.parent.as_str())
                .copied()
                .unwrap_or("<missing>"),
            names
                .get(containment.child.as_str())
                .copied()
                .unwrap_or("<missing>"),
        ));
    }
    for reference in &extracted.references {
        let owner = reference
            .owner
            .as_ref()
            .and_then(|owner| names.get(owner.as_str()).copied())
            .unwrap_or("<file>");
        facts.push(format!(
            "R|{}|{}|{}|{}-{}",
            owner,
            reference.name,
            reference.kind.as_str(),
            reference.span.start_byte(),
            reference.span.end_byte(),
        ));
    }
    facts
}

fn extract(path: &str, source: &str) -> ExtractedFile {
    let extracted = extract_result(path, source)
        .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"));
    let unique_ids = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique_ids.len(), extracted.symbols.len(), "{path}");
    extracted
}

fn extract_result(path: &str, source: &str) -> Result<ExtractedFile, ExtractError> {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())?;
    extractor.extract(&snapshot)
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT).unwrap_or_else(|error| panic!("test limit failed: {error}"))
}

#[test]
fn julia_retained_tag_names_screen_provider_keys() {
    for key in ["sk_live_FAKE1234567890abcdef", "ghp_aaaaaaaaaaaaaaaaaaaa"] {
        let file = credential_support::extract("names.jl", &format!("Base.{key}(x) = x\n"));
        credential_support::assert_no_credentials(&file);
        assert_eq!(file.symbols, []);
    }
    credential_support::assert_screened("comments.jl", "#= @VALUE@ =#\nBase.:+(x) = x\n", ":+");
    let file = credential_support::extract("names.jl", "Base.token(x) = x\n");
    assert!(
        file.symbols
            .iter()
            .any(|symbol| symbol.name == "Base.token")
    );
}
