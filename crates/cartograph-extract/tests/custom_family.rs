//! Integration coverage for Cartograph native extraction contracts.

mod credential_support;
mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{
    ExtractError, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
const SECRET_SENTINEL: &str = "cartograph_literal_secret_sentinel_7c1f";

struct Fixture {
    language: SourceLanguage,
    path: &'static str,
    source: &'static str,
    symbol_kind: SymbolKind,
    symbol_name: &'static str,
    reference: Option<(&'static str, ReferenceKind)>,
}

const FIXTURES: [Fixture; 13] = [
    Fixture {
        language: SourceLanguage::Aura,
        path: "force-app/main/default/aura/OrderPanel/OrderPanel.cmp",
        source: "<aura:component controller=\"OrderController\"><aura:attribute name=\"orderId\" type=\"Id\"/><c:orderCard onclick=\"{!c.loadOrder}\"/><span data-secret=\"cartograph_literal_secret_sentinel_7c1f\"/></aura:component>\n",
        symbol_kind: SymbolKind::Field,
        symbol_name: "orderId",
        reference: Some(("loadOrder", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Bg3Anubis,
        path: "Game/AI/order.ann",
        source: "game.states.OrderState = State {\n nodes.LoadOrder = Action {\n OnEnter = function()\n   StartOrder()\n end\n}\n",
        symbol_kind: SymbolKind::Module,
        symbol_name: "OrderState",
        reference: Some(("StartOrder", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Bg3Resource,
        path: "Mods/Orders/Public/Data/order.lsx",
        source: "<save><region id=\"Orders\"><node id=\"OrderDefinition\"><attribute id=\"Name\" value=\"OrderBeacon\"/><attribute id=\"ParentTemplateId\" value=\"OrderParent_123\"/><attribute id=\"Secret\" value=\"cartograph_literal_secret_sentinel_7c1f\"/></node></region></save>\n",
        symbol_kind: SymbolKind::Resource,
        symbol_name: "OrderBeacon",
        reference: Some(("OrderParent_123", ReferenceKind::References)),
    },
    Fixture {
        language: SourceLanguage::Bg3Stats,
        path: "Game/Stats/Generated/Data/orders.txt",
        source: "new entry \"OrderStatsBeacon\"\ntype \"StatusData\"\nusing \"BaseOrderStats\"\ndata \"Boosts\" \"OrderBoost_123;cartograph_literal_secret_sentinel_7c1f\"\n",
        symbol_kind: SymbolKind::Resource,
        symbol_name: "OrderStatsBeacon",
        reference: Some(("BaseOrderStats", ReferenceKind::Extends)),
    },
    Fixture {
        language: SourceLanguage::Liquid,
        path: "sections/order-panel.liquid",
        source: "{% assign order_total = cart.total %}\n{% render 'order-card' %}\n{{ format_order(order_total) }}\n{% comment %}cartograph_literal_secret_sentinel_7c1f{% endcomment %}\n",
        symbol_kind: SymbolKind::Variable,
        symbol_name: "order_total",
        reference: Some(("snippets/order-card.liquid", ReferenceKind::Imports)),
    },
    Fixture {
        language: SourceLanguage::Osiris,
        path: "Story/RawFiles/Goals/OrderGoal.txt",
        source: "INITSECTION\nsyscall StartOrder((GUIDSTRING)_Order)\nKBSECTION\nIF\nDB_OrderReady(_Order)\nTHEN\nStartOrder(_Order);\n",
        symbol_kind: SymbolKind::Function,
        symbol_name: "StartOrder",
        reference: Some(("DB_OrderReady", ReferenceKind::References)),
    },
    Fixture {
        language: SourceLanguage::Properties,
        path: "config/application.properties",
        source: "orders.cache.ttl=${orders.default.ttl}\norders.secret=cartograph_literal_secret_sentinel_7c1f\n",
        symbol_kind: SymbolKind::Constant,
        symbol_name: "orders.cache.ttl",
        reference: Some(("orders.default.ttl", ReferenceKind::References)),
    },
    Fixture {
        language: SourceLanguage::Rhai,
        path: "scripts/order-policy.rhai",
        source: "import \"./orders\" as orders;\nfn load_order(id) { orders::fetch(id) }\nconst secret = \"cartograph_literal_secret_sentinel_7c1f\";\n",
        symbol_kind: SymbolKind::Function,
        symbol_name: "load_order",
        reference: Some(("orders::fetch", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Svelte,
        path: "src/OrderPanel.svelte",
        source: "<script lang=\"ts\">\nimport OrderCard from './OrderCard.svelte';\nexport function loadOrder() { fetchOrder(); }\nconst secret = 'cartograph_literal_secret_sentinel_7c1f';\n</script>\n<OrderCard on:click=\"loadOrder()\" />\n{formatOrder(order)}\n",
        symbol_kind: SymbolKind::Component,
        symbol_name: "OrderPanel",
        reference: Some(("fetchOrder", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Vb6,
        path: "legacy/OrderModule.bas",
        source: "Attribute VB_Name = \"OrderModule\"\nPublic Type OrderRecord\n  Id As Long\nEnd Type\nPublic Sub LoadOrder()\n  FetchOrder (1)\n  secret = \"cartograph_literal_secret_sentinel_7c1f\"\nEnd Sub\n",
        symbol_kind: SymbolKind::Struct,
        symbol_name: "OrderRecord",
        reference: Some(("FetchOrder", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Visualforce,
        path: "force-app/main/default/pages/Orders.page",
        source: "<apex:page controller=\"OrderController\" action=\"{!loadOrders}\"><c:orderTable/><span title=\"cartograph_literal_secret_sentinel_7c1f\"/></apex:page>\n",
        symbol_kind: SymbolKind::Route,
        symbol_name: "/apex/Orders",
        reference: Some(("loadOrders", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Vue,
        path: "src/OrderPanel.vue",
        source: "<script setup lang=\"ts\">\nimport OrderCard from './OrderCard.vue';\nexport function loadOrder() { fetchOrder(); }\nconst secret = 'cartograph_literal_secret_sentinel_7c1f';\n</script>\n<template><OrderCard @click=\"loadOrder()\" />{{ formatOrder(order) }}</template>\n",
        symbol_kind: SymbolKind::Component,
        symbol_name: "OrderPanel",
        reference: Some(("fetchOrder", ReferenceKind::Calls)),
    },
    Fixture {
        language: SourceLanguage::Xml,
        path: "src/main/resources/OrderMapper.xml",
        source: "<mapper namespace=\"com.example.OrderMapper\"><resultMap id=\"orderMap\" type=\"com.example.Order\"/><sql id=\"orderColumns\">id,total</sql><select id=\"findOrder\" resultMap=\"orderMap\">SELECT <include refid=\"orderColumns\"/> FROM orders WHERE id = #{orderId} AND secret != 'cartograph_literal_secret_sentinel_7c1f'</select></mapper>\n",
        symbol_kind: SymbolKind::Method,
        symbol_name: "findOrder",
        reference: Some(("OrderMapper::orderColumns", ReferenceKind::References)),
    },
];

#[test]
fn custom_modes_extract_real_structures_deterministically_without_literal_leaks() {
    for fixture in FIXTURES {
        let snapshot =
            SourceSnapshot::from_bytes(fixture.path, fixture.source.as_bytes(), limits())
                .unwrap_or_else(|error| panic!("{} snapshot failed: {error}", fixture.path));
        assert_eq!(snapshot.language(), fixture.language, "{}", fixture.path);
        assert!(fixture.language.is_native_indexable(), "{}", fixture.path);
        let mut extractor = NativeExtractor::new(fixture.language)
            .unwrap_or_else(|error| panic!("{} extractor failed: {error}", fixture.path));
        let first = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("{} extraction failed: {error}", fixture.path));
        let second = extractor
            .extract(&snapshot)
            .unwrap_or_else(|error| panic!("{} repeat failed: {error}", fixture.path));
        assert_eq!(first, second, "{} was not deterministic", fixture.path);
        assert!(
            first.symbols.iter().any(|symbol| {
                symbol.kind == fixture.symbol_kind && symbol.name == fixture.symbol_name
            }),
            "{} missing {:?} {}; symbols={:?}",
            fixture.path,
            fixture.symbol_kind,
            fixture.symbol_name,
            first
                .symbols
                .iter()
                .map(|symbol| (
                    symbol.kind,
                    symbol.name.as_str(),
                    symbol.qualified_name.as_str()
                ))
                .collect::<Vec<_>>()
        );
        if let Some((name, kind)) = fixture.reference {
            assert!(
                first
                    .references
                    .iter()
                    .any(|reference| reference.name == name && reference.kind == kind),
                "{} missing {kind:?} {name}; refs={:?}",
                fixture.path,
                first.references
            );
        }
        assert!(
            !format!("{first:?}").contains(SECRET_SENTINEL),
            "{} leaked a source literal",
            fixture.path
        );
    }
}

#[test]
fn custom_modes_poll_cancellation_before_retaining_facts() {
    for fixture in FIXTURES {
        let snapshot =
            SourceSnapshot::from_bytes(fixture.path, fixture.source.as_bytes(), limits())
                .unwrap_or_else(|error| panic!("{} snapshot failed: {error}", fixture.path));
        let mut extractor = NativeExtractor::new(fixture.language)
            .unwrap_or_else(|error| panic!("{} extractor failed: {error}", fixture.path));
        assert_eq!(
            extractor.extract_with_cancellation(&snapshot, || true),
            Err(ExtractError::Cancelled),
            "{}",
            fixture.path
        );
    }
}

#[test]
fn vb6_declare_routines_are_imports_and_bare_statements_are_calls() {
    // v1 languages/vb6.ts: `Declare` -> import node; inside routines a statement
    // line calls its first identifier unless it is a VB keyword.
    let source = "VERSION 5.00\nBegin VB.Form frmMain\nEnd\nAttribute VB_Name = \"frmMain\"\nPrivate Declare PtrSafe Function GetTickCount Lib \"kernel32\" () As Long\nDeclare Sub Sleep Lib \"kernel32\" (ByVal ms As Long)\nPrivate customerName As String\n\nPublic Sub Load()\n    Dim i As Integer\n    Helper i\n    MsgBox \"cartograph_literal_secret_sentinel_7c1f\"\n    Call DoWork(1)\n    FormatName customerName\n    customerName = \"x\"\n    total = Compute(2)\n    Me.Caption = \"y\"\n    If i > 0 Then\n    End If\n    Exit Sub\n    Set obj = Nothing\nEnd Sub\n\nPublic Function FormatName(ByVal value As String) As String\n    FormatName = value\nEnd Function\n";
    let snapshot = SourceSnapshot::from_bytes("legacy/Main.frm", source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("VB6 snapshot failed: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::Vb6);
    let extracted = NativeExtractor::new(SourceLanguage::Vb6)
        .unwrap_or_else(|error| panic!("VB6 extractor failed: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("VB6 extraction failed: {error}"));
    let symbols = extracted
        .symbols
        .iter()
        .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
        .collect::<Vec<_>>();
    for name in ["frmMain::GetTickCount", "frmMain::Sleep"] {
        assert!(
            symbols.contains(&(SymbolKind::Import, name)),
            "missing Declare import {name}: {symbols:?}"
        );
    }
    assert!(
        symbols.iter().all(|(_, name)| !name.ends_with("::Declare")),
        "Declare keyword became a symbol: {symbols:?}"
    );
    assert!(symbols.contains(&(SymbolKind::Field, "frmMain::customerName")));
    let load = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == "frmMain::Load")
        .unwrap_or_else(|| panic!("missing Load: {symbols:?}"));
    let calls = extracted
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::Calls && reference.owner.as_ref() == Some(&load.id)
        })
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["Helper", "MsgBox", "DoWork", "FormatName", "Compute"] {
        assert!(
            calls.contains(&expected),
            "missing call {expected}: {calls:?}"
        );
    }
    for keyword_or_assignment in [
        "customerName",
        "total",
        "Me",
        "Caption",
        "If",
        "End",
        "Exit",
        "Set",
        "obj",
        "Dim",
    ] {
        assert!(
            !calls.contains(&keyword_or_assignment),
            "{keyword_or_assignment} is not a call: {calls:?}"
        );
    }
    assert!(!format!("{extracted:?}").contains(SECRET_SENTINEL));
    assert!(!format!("{extracted:?}").contains("kernel32"));
}

#[test]
fn vb6_containers_start_with_the_file_and_withevents_fields_keep_their_names() {
    // The module/class/form is the whole file: its symbol starts on line 1
    // (the designer header belongs to it), so every member lies inside it.
    let form = "VERSION 5.00\nBegin VB.Form frmMain\n   Caption = \"Billing\"\nEnd\nAttribute VB_Name = \"frmMain\"\nOption Explicit\n\nPrivate WithEvents mCustomer As Customer\nDim WithEvents mTimer As Timer\n\nPrivate Sub Form_Load()\n    mCustomer.Load\nEnd Sub\n";
    let extracted = extract_vb6("legacy/frmMain.frm", form);
    let symbol = |kind: SymbolKind, name: &str| {
        extracted
            .symbols
            .iter()
            .find(|symbol| symbol.kind == kind && symbol.qualified_name == name)
            .unwrap_or_else(|| panic!("missing {kind:?} {name}: {:?}", extracted.symbols))
    };
    let container = symbol(SymbolKind::Component, "frmMain");
    assert_eq!(container.span.start_line(), 1);
    let field = symbol(SymbolKind::Field, "frmMain::mCustomer");
    assert_eq!(field.span.start_line(), 8);
    symbol(SymbolKind::Field, "frmMain::mTimer");
    for member in extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.id != container.id)
    {
        assert!(
            container.span.start_byte() <= member.span.start_byte()
                && member.span.end_byte() <= container.span.end_byte(),
            "{} lies outside its container",
            member.qualified_name
        );
    }
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| !symbol.name.eq_ignore_ascii_case("WithEvents")),
        "the WithEvents modifier became a symbol: {:?}",
        extracted.symbols
    );

    let class = extract_vb6(
        "legacy/Customer.cls",
        "VERSION 1.0 CLASS\nBEGIN\n  MultiUse = -1  'True\nEND\nAttribute VB_Name = \"Customer\"\nPublic Balance As Currency\n",
    );
    let customer = class
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Class && symbol.name == "Customer")
        .unwrap_or_else(|| panic!("missing class: {:?}", class.symbols));
    assert_eq!(customer.span.start_line(), 1);
}

/// Extract one VB6 source through the production entry points.
fn extract_vb6(path: &str, source: &str) -> cartograph_extract::ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("VB6 snapshot failed: {error}"));
    assert_eq!(snapshot.language(), SourceLanguage::Vb6);
    NativeExtractor::new(SourceLanguage::Vb6)
        .unwrap_or_else(|error| panic!("VB6 extractor failed: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("VB6 extraction failed: {error}"))
}

#[test]
fn vb6_calls_ignore_string_contents_and_keep_member_and_rhs_calls() {
    let source = "Attribute VB_Name = \"Module1\"\nPublic Sub Run()\n    MsgBox \"Use (Fake) it's quoted\"\n    obj.Save arg\n    total = Other(i)\n    Me.Caption = \"y\"\n    Retry:\n    Notify \"x\" ' Trailing(comment)\n    Rem Remark(ignored) entirely\n    Primary Nested(i)\n    Call form.Submit(1)\nEnd Sub\n";
    let snapshot = SourceSnapshot::from_bytes("legacy/Module1.bas", source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("VB6 snapshot failed: {error}"));
    let extracted = NativeExtractor::new(SourceLanguage::Vb6)
        .unwrap_or_else(|error| panic!("VB6 extractor failed: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("VB6 extraction failed: {error}"));
    let calls = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "MsgBox", "Save", "Other", "Notify", "Primary", "Nested", "Submit",
    ] {
        assert!(
            calls.contains(&expected),
            "missing call {expected}: {calls:?}"
        );
    }
    for absent in [
        "Use", "Fake", "obj", "total", "Me", "Caption", "Retry", "Trailing", "Rem", "Remark",
        "form",
    ] {
        assert!(
            !calls.contains(&absent),
            "{absent} is not a call: {calls:?}"
        );
    }
}

#[test]
fn osiris_blocks_start_at_their_control_line_and_are_named_by_their_head_line() {
    // v1 languages/bg3/osiris.ts: a rule/proc/query node starts on its
    // `IF`/`PROC`/`QRY` line and is disambiguated by its head predicate's line.
    let source = "KBSECTION\nIF\nCharacterUsedSkill(_Player, \"Skill\", _)\nTHEN\nDB_Seen(_Player);\n\n  PROC\n  PROC_Target((CHARACTER)_Character)\nTHEN\nDebugText(_Character, \"apply Target_Status now\");\nEXITSECTION\nSysCompleteGoal(\"Blocks\");\n";
    let extracted = extract_custom(
        "Story/RawFiles/Goals/Blocks.txt",
        source,
        SourceLanguage::Osiris,
    );
    for (name, qualified_name, start_line) in [
        ("rule:CharacterUsedSkill", "Blocks::rule:3", 2),
        ("proc:PROC_Target", "Blocks::proc:8", 7),
    ] {
        let block = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Method && symbol.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", extracted.symbols));
        assert_eq!(block.qualified_name, qualified_name);
        assert_eq!(block.span.start_line(), start_line, "{name}");
    }
    // A string that is one identifier names it whole, as in v1.
    let string_references = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["Skill", "Target_Status", "Blocks"] {
        assert!(
            string_references.contains(&expected),
            "missing string reference {expected}: {string_references:?}"
        );
    }
    for prose in ["apply", "now", "x"] {
        assert!(
            !string_references.contains(&prose),
            "{prose} is prose: {string_references:?}"
        );
    }
}

#[test]
fn anubis_handlers_are_disambiguated_by_their_line() {
    // v1 languages/bg3/anubis.ts names a handler `<root>::<label>:<name>:<line>`
    // so same-named callbacks of different behavior nodes stay distinct.
    let source = "game.states.Guard = State {\n  function ()\n    nodes.A = Action {\n      OnEnter = function() end\n    }\n    nodes.B = Action {\n      OnEnter = function() end\n    }\n    events.Alarm = function(e) end\n  end\n}\n";
    let extracted = extract_custom(
        "Mods/Demo/Scripts/anubis/node/Guard.ann",
        source,
        SourceLanguage::Bg3Anubis,
    );
    let handlers = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.name.contains(':'))
        .map(|symbol| (symbol.qualified_name.as_str(), symbol.span.start_line()))
        .collect::<Vec<_>>();
    assert_eq!(
        handlers,
        [
            ("game.states.Guard::callback:OnEnter:4", 4),
            ("game.states.Guard::callback:OnEnter:7", 7),
            ("game.states.Guard::event:Alarm:9", 9),
        ]
    );
}

#[test]
fn bg3_resources_start_at_their_node_tag() {
    // v1 languages/bg3/resource.ts starts an LSX object's node at its `<node>`
    // tag, not at the attribute that names it.
    let source = "<save>\n  <region id=\"Rules\">\n    <node id=\"Ruleset\">\n      <attribute id=\"Secret\" type=\"LSString\" value=\"x\" />\n      <attribute id=\"RuleName\" type=\"LSString\" value=\"Demo_Ruleset\" />\n    </node>\n  </region>\n</save>\n";
    let extracted = extract_custom(
        "Public/Demo/Content/Rules.lsx",
        source,
        SourceLanguage::Bg3Resource,
    );
    let resource = extracted
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Resource && symbol.name == "Demo_Ruleset")
        .unwrap_or_else(|| panic!("missing Demo_Ruleset: {:?}", extracted.symbols));
    assert_eq!(resource.span.start_line(), 3);
    assert_eq!(resource.span.end_line(), 5);
}

fn extract_custom(path: &str, source: &str, language: SourceLanguage) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
    assert_eq!(snapshot.language(), language, "{path}");
    NativeExtractor::new(language)
        .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"))
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"))
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("custom-family source limit failed: {error}"))
}

#[test]
fn screened_bg3_region_abstains_without_changing_the_following_sibling_owner() {
    let template = "<region id=\"Outer\"><region id=\"@VALUE@\"><node id=\"Object\"><attribute id=\"Name\" value=\"Hidden\"/></node></region><node id=\"Object\"><attribute id=\"Name\" value=\"Following\"/></node></region>";
    for value in credential_support::CREDENTIAL_INPUTS {
        let file = credential_support::extract(
            "Public/Mod/RootTemplates/regions.lsx",
            &template.replace("@VALUE@", value),
        );
        credential_support::assert_no_credentials(&file);
        assert!(!file.symbols.iter().any(|symbol| symbol.name == "Hidden"));
        assert!(file.symbols.iter().any(|symbol| symbol.name == "Following"
            && symbol.qualified_name.ends_with("::Outer::Following")));
    }
    credential_support::assert_screened("Public/Mod/RootTemplates/regions.lsx", template, "Inner");
}

#[test]
fn literal_loads_and_script_calls_screen_before_name_projection() {
    for (path, source, ordinary) in [
        ("main.vbp", "Module=@VALUE@\n", "./Module.bas"),
        (
            "Mods/Demo/Scripts/anubis/node/Guard.ann",
            "SetEntityEvent(me, \"@VALUE@()\");\n",
            "RaiseAlarm",
        ),
        (
            "Story/RawFiles/Goals/OrderGoal.txt",
            "Version 1\nINITSECTION\nSysCompleteGoal(\"@VALUE@()\");\nKBSECTION\nEXITSECTION\n",
            "Init",
        ),
    ] {
        credential_support::assert_screened(path, source, ordinary);
    }
}
