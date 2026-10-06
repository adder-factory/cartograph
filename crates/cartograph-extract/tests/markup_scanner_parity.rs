//! v1 parity for the bounded BG3, Liquid, and Salesforce markup scanners:
//! declaration identity and precedence, reference precision, and the
//! negative cases v1 deliberately did not extract.

mod credential_support;
mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SymbolKind};
use cartograph_extract::{
    DiagnosticCode, ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
    let mut extractor = NativeExtractor::new(snapshot.language())
        .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"));
    let first = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"));
    let second = extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("{path} repeat failed: {error}"));
    assert_eq!(first, second, "{path} was not deterministic");
    first
}

fn symbols(file: &ExtractedFile) -> Vec<(SymbolKind, &str)> {
    file.symbols
        .iter()
        .map(|symbol| (symbol.kind, symbol.name.as_str()))
        .collect()
}

fn references(file: &ExtractedFile) -> Vec<(ReferenceKind, &str)> {
    file.references
        .iter()
        .map(|reference| (reference.kind, reference.name.as_str()))
        .collect()
}

fn has_reference(file: &ExtractedFile, kind: ReferenceKind, name: &str) -> bool {
    references(file).contains(&(kind, name))
}

const SWORD_LSX: &str = r#"
<save>
  <region id="Templates">
    <node id="GameObjects">
      <children>
        <node id="GameObject">
          <attribute id="UUID" type="FixedString" value="11111111-1111-1111-1111-111111111111" />
          <attribute id="Name" type="LSString" value="MAG_Test_Sword" />
          <attribute id="ParentTemplateId" type="FixedString" value="22222222-2222-2222-2222-222222222222" />
          <attribute id="DisplayName" type="TranslatedString" handle="h123456789abc" />
        </node>
      </children>
    </node>
  </region>
</save>
"#;

#[test]
fn bg3_lsx_objects_use_their_own_defining_fields_and_reference_handles() {
    let file = extract("Public/Shared/RootTemplates/Sword.lsx", SWORD_LSX);
    assert_eq!(
        symbols(&file),
        [
            (SymbolKind::Namespace, "Templates"),
            (SymbolKind::Resource, "MAG_Test_Sword")
        ],
        "the GameObjects container must not duplicate its child"
    );
    let sword = &file.symbols[1];
    assert_eq!(sword.qualified_name, "11111111-1111-1111-1111-111111111111");
    assert_eq!(
        references(&file),
        [
            (
                ReferenceKind::References,
                "22222222-2222-2222-2222-222222222222"
            ),
            (ReferenceKind::References, "h123456789abc")
        ],
        "a declaration's own UUID is not a reference to itself"
    );
    assert!(
        file.references
            .iter()
            .all(|reference| reference.owner.as_ref() == Some(&sword.id))
    );
}

#[test]
fn bg3_object_names_skip_generated_and_zero_identities() {
    let file = extract(
        "Public/Shared/Stats/Objects.lsx",
        r#"<save><region id="Stats">
<node id="StatObject"><attribute id="Name" value="New_Stat_12"/><attribute id="Folder" value="Weapons"/></node>
<node id="Rule"><attribute id="UUID" value="00000000-0000-0000-0000-000000000000"/><attribute id="RuleName" value="Rule_Melee"/></node>
<node id="Tags"><attribute id="Object" value="33333333-3333-3333-3333-333333333333"/></node>
<node id="Pointer"><attribute id="Object" value="44444444-4444-4444-4444-444444444444"/></node>
</region></save>"#,
    );
    assert_eq!(
        symbols(&file),
        [
            (SymbolKind::Namespace, "Stats"),
            (SymbolKind::Resource, "Weapons"),
            (SymbolKind::Resource, "Rule_Melee")
        ]
    );
    assert!(
        has_reference(
            &file,
            ReferenceKind::References,
            "44444444-4444-4444-4444-444444444444"
        ),
        "an object-only node lifts its reference to the file"
    );
}

#[test]
fn bg3_generic_tags_and_settings_sidecars_declare_resources() {
    let effect = extract(
        "Public/Shared/Assets/Effects/Test.lsx",
        "<save>\n  <effect Name='FX_Target' class='VisualEffect' Resource='h00a33f75ge607g4aa2ga34ag4e2849aa53f9' />\n</save>\n",
    );
    assert_eq!(symbols(&effect), [(SymbolKind::Resource, "FX_Target")]);
    assert_eq!(
        references(&effect),
        [(
            ReferenceKind::References,
            "h00a33f75ge607g4aa2ga34ag4e2849aa53f9"
        )],
        "the class attribute is identity, not a reference"
    );
    let settings = extract(
        "Public/Demo/Assets/Weapons/WPN_Hand_Cannon_Revolver_03.xml",
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Settings source=\"($SOURCE)\\WPN_Hand_Cannon_Revolver_03.GR2\">\n  <SceneSettings />\n</Settings>\n",
    );
    assert_eq!(
        symbols(&settings),
        [(SymbolKind::Resource, "WPN_Hand_Cannon_Revolver_03")]
    );
}

#[test]
fn bg3_binary_payloads_are_degraded_without_facts() {
    let file = extract("Public/Shared/RootTemplates/Binary.lsf", "LSF\0payload");
    assert!(file.symbols.is_empty() && file.references.is_empty());
    assert_eq!(file.parse_status, FileParseStatus::Partial);
    assert!(
        file.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::ParserStopped)
    );
}

#[test]
fn bg3_treasure_table_object_categories_reference_items() {
    let file = extract(
        "Public/Demo/Stats/Generated/TreasureTable.txt",
        "\nnew treasuretable \"TUT_Chest_Potions\"\nCanMerge 1\nnew subtable \"1,1\"\nobject category \"I_MY_COOL_NEW_CIRCLET\",1,0,0,0,0,0,0,0\nobject \"NotACategory\"\n",
    );
    assert_eq!(
        symbols(&file),
        [(SymbolKind::Resource, "TUT_Chest_Potions")]
    );
    assert_eq!(
        references(&file),
        [(ReferenceKind::References, "I_MY_COOL_NEW_CIRCLET")]
    );
}

const GUARD_ANN: &str = r#"
game.states.Guard = State {
  function ()
    local playerApproachTrigger = Entity("S_EventTrigger_0687d319-0436-4091-8389-15f28536a8e8")
    nodes.GuardAction = Selector {
      function(nodes)
        return FindRandomSelectable(nodes)
      end
    }
    nodes.GuardAction.Cower = Action {
      function ()
        DebugText(me, "RaiseAlarm")
        Print("true", "a sentence with spaces", "short")
      end
    }
    nodes.Wander = Proxy {
      params = {
        anchor = [[S_WanderArea_a1017cd3-cbde-44f1-b2d9-00572a2dc9c3]]
      }
    }
  end
}
"#;

#[test]
fn bg3_anubis_keeps_nested_node_paths_and_string_resource_references() {
    let file = extract("Mods/Demo/Scripts/anubis/node/Guard.ann", GUARD_ANN);
    let methods = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(methods, ["GuardAction", "GuardAction.Cower", "Wander"]);
    for name in [
        "S_EventTrigger_0687d319-0436-4091-8389-15f28536a8e8",
        "RaiseAlarm",
        "S_WanderArea_a1017cd3-cbde-44f1-b2d9-00572a2dc9c3",
    ] {
        assert!(
            has_reference(&file, ReferenceKind::References, name),
            "missing {name}: {:?}",
            references(&file)
        );
    }
    for prose in ["true", "a sentence with spaces", "short"] {
        assert!(
            !has_reference(&file, ReferenceKind::References, prose),
            "{prose} is not a resource"
        );
    }
}

#[test]
fn liquid_schema_constants_take_their_json_name() {
    let file = extract(
        "sections/hero.liquid",
        "{% schema %}\n{\"name\": {\"fr\": \"Bonjour\", \"en\": \"Hero banner\"}, \"settings\": [{\"id\": \"secret_value\"}]}\n{% endschema %}\n",
    );
    let schema = &file.symbols[0];
    assert_eq!(
        (
            schema.kind,
            schema.name.as_str(),
            schema.qualified_name.as_str()
        ),
        (
            SymbolKind::Constant,
            "Hero banner",
            "sections/hero.liquid::schema:Hero banner"
        )
    );
    assert_eq!((schema.span.start_line(), schema.span.end_line()), (1, 3));
    assert!(!format!("{file:?}").contains("secret_value"));
    let plain = extract(
        "sections/plain.liquid",
        "{% schema %}{ \"name\": \"Hero Banner\" }{% endschema %}",
    );
    assert_eq!(symbols(&plain), [(SymbolKind::Constant, "Hero Banner")]);
    let invalid = extract(
        "sections/invalid.liquid",
        "{% schema %}not json{% endschema %}",
    );
    assert_eq!(symbols(&invalid), [(SymbolKind::Constant, "schema")]);
}

#[test]
fn liquid_partners_accept_quoted_names_with_spaces() {
    let file = extract(
        "snippets/q.liquid",
        "{% render 'hero card' %}\n{% section 'site header' %}\n{% render '{{ bad }}' %}\n",
    );
    assert!(has_reference(
        &file,
        ReferenceKind::Imports,
        "snippets/hero card.liquid"
    ));
    assert!(has_reference(
        &file,
        ReferenceKind::Imports,
        "sections/site header.liquid"
    ));
    assert_eq!(
        symbols(&file),
        [
            (SymbolKind::Component, "hero card"),
            (SymbolKind::Import, "hero card"),
            (SymbolKind::Component, "site header"),
            (SymbolKind::Import, "site header")
        ]
    );
}

#[test]
fn aura_calls_only_controller_actions_outside_comments() {
    let file = extract(
        "force-app/main/default/aura/OrderPanel/OrderPanel.cmp",
        "<aura:component controller=\"OrderController\"><aura:attribute name=\"rows\" type=\"List<Account>\"/><c:orderCard items=\"{!v.orders}\" onclick=\"{!c.handleClick}\"/><!-- {!c.save} --><span>{!v.record}</span><ui:button press=\"{! controller.doSave }\"/></aura:component>\n",
    );
    let calls = references(&file)
        .into_iter()
        .filter(|(kind, _)| *kind == ReferenceKind::Calls)
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
    assert_eq!(calls, ["handleClick", "doSave"]);
    assert!(has_reference(&file, ReferenceKind::TypeOf, "List"));
    let rows = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "rows")
        .unwrap_or_else(|| panic!("missing rows field"));
    assert_eq!(rows.signature.as_deref(), Some("List<Account>"));
}

#[test]
fn visualforce_calls_only_action_attributes() {
    let file = extract(
        "force-app/main/default/pages/Orders.page",
        "<apex:page controller=\"OrderController\" action=\"{!init}\"><!-- <apex:commandButton action=\"{!save}\"/> --><span>{!account.Name}</span><apex:commandButton action=\"{!doSave}\" value=\"Save\"/></apex:page>\n",
    );
    let calls = references(&file)
        .into_iter()
        .filter(|(kind, _)| *kind == ReferenceKind::Calls)
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
    assert_eq!(calls, ["init", "doSave"]);
}

#[test]
fn empty_salesforce_markup_still_declares_its_component_and_route() {
    let aura = extract("force-app/main/default/aura/Panel/Panel.cmp", "");
    assert_eq!(symbols(&aura), [(SymbolKind::Component, "Panel")]);
    let page = extract("force-app/main/default/pages/Empty.page", "");
    assert_eq!(
        symbols(&page),
        [
            (SymbolKind::Component, "Empty"),
            (SymbolKind::Route, "/apex/Empty")
        ]
    );
}

#[test]
fn liquid_schema_locale_choice_follows_document_order_and_fails_closed() {
    for (body, expected) in [
        (r#"{"name": {"zz": "First", "aa": "Second"}}"#, "First"),
        (
            r#"{"name": {"fr": "Produit", "en": "Product card"}}"#,
            "Product card",
        ),
        (r#"{"name": {"en": "", "fr": "Bonjour"}}"#, "Bonjour"),
        (r#"{"name": {"en": 404, "fr": false}}"#, "schema"),
        (r#"{"name": ["Hero"]}"#, "schema"),
        (r#"{"settings": []}"#, "schema"),
    ] {
        let source = format!("{{%- schema -%}}\n{body}\n{{%- endschema -%}}\n");
        let file = extract("sections/locale.liquid", &source);
        assert_eq!(symbols(&file), [(SymbolKind::Constant, expected)], "{body}");
    }
}

#[test]
fn liquid_schema_json_is_not_scanned_for_output_references() {
    let file = extract(
        "sections/hero.liquid",
        "{{ section.title }}\n{% schema %}\n{\"name\": \"Hero\", \"default\": \"{{ product.title }}\"}\n{% endschema %}\n{{ shop.name }}\n",
    );
    let referenced = references(&file)
        .into_iter()
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
    assert!(referenced.contains(&"section") && referenced.contains(&"shop"));
    assert!(!referenced.contains(&"product"), "{referenced:?}");
}

/// v1.1.33 declared every rendered partner as a component next to its import
/// node, so `{% render %}` and `{% include %}` sites are found by component
/// searches exactly like `{% section %}` sites, Jekyll includes included.
#[test]
fn liquid_render_and_include_partners_are_components_like_sections() {
    let theme = extract(
        "layout/theme.liquid",
        "{% section 'header' %}\n{%- render 'drawer-menu' -%}\n{% include 'icon-cart' %}\n",
    );
    let declared = theme
        .symbols
        .iter()
        .map(|symbol| {
            (
                symbol.kind,
                symbol.qualified_name.as_str(),
                symbol.span.start_line(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        [
            (
                SymbolKind::Component,
                "layout/theme.liquid::section:header",
                1
            ),
            (SymbolKind::Import, "layout/theme.liquid::section:header", 1),
            (
                SymbolKind::Component,
                "layout/theme.liquid::render:drawer-menu",
                2
            ),
            (
                SymbolKind::Import,
                "layout/theme.liquid::render:drawer-menu",
                2
            ),
            (
                SymbolKind::Component,
                "layout/theme.liquid::include:icon-cart",
                3
            ),
            (
                SymbolKind::Import,
                "layout/theme.liquid::include:icon-cart",
                3
            ),
        ]
    );
    let jekyll = extract(
        "blog/_layouts/default.html",
        "---\nlayout: none\n---\n{% include 'footer.html' %}\n",
    );
    assert_eq!(
        symbols(&jekyll),
        [
            (SymbolKind::Component, "footer.html"),
            (SymbolKind::Import, "footer.html")
        ]
    );
}

#[test]
fn liquid_partner_tags_keep_v1_whitespace_control_and_multiplicity() {
    let file = extract(
        "layout/theme.liquid",
        "{% section 'header' %}\n{%- render 'loading-spinner' -%}\n{% include 'cart-drawer' %}\n",
    );
    let imports = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Import)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(imports, ["header", "loading-spinner", "cart-drawer"]);
}

#[test]
fn bg3_lsj_names_define_resources_without_use_references() {
    let file = extract(
        "Public/Demo/RootTemplates/JsonItem.lsj",
        include_str!("fixtures/v1_parity/bg3_resource/Public/Demo/RootTemplates/JsonItem.lsj"),
    );
    // Native HEAD facts, with only the three consumed identity uses removed.
    assert_eq!(
        bg3_head_facts(&file),
        [
            "S|resource|MAG_JSON_Item|44444444-4444-4444-4444-444444444444|13-26|true",
            "S|resource|MAG_JSON_Child|Public/Demo/RootTemplates/JsonItem.lsj::MAG_JSON_Child|178-192|false",
            "C|44444444-4444-4444-4444-444444444444|Public/Demo/RootTemplates/JsonItem.lsj::MAG_JSON_Child",
            "R|Public/Demo/RootTemplates/JsonItem.lsj::MAG_JSON_Child|UnlockSpell|references|212-223",
            "R|Public/Demo/RootTemplates/JsonItem.lsj::MAG_JSON_Child|Target_Spell|references|224-236",
            "R|44444444-4444-4444-4444-444444444444|22222222-2222-2222-2222-222222222222|references|102-138",
            "R|44444444-4444-4444-4444-444444444444|Target_Tag|references|267-277",
        ]
    );
    assert_eq!(file.symbols[0].span.start_line(), 2);
    assert_eq!(file.symbols[1].span.start_line(), 7);
}

#[test]
fn bg3_lsj_identity_key_siblings_keep_declarations_and_nested_uses() {
    for key in ["Name", "NameFS", "name", "UUID", "Guid", "id"] {
        let file = extract(
            "Public/Demo/RootTemplates/Names.lsj",
            &format!(
                r#"{{"{key}": "MAG_Resource", "Children": [{{"Name": "MAG_Child", "Uses": ["Target_Spell"]}}], "ParentTemplateId": "Target_Parent"}}"#
            ),
        );
        assert_eq!(
            symbols(&file),
            [
                (SymbolKind::Resource, "MAG_Resource"),
                (SymbolKind::Resource, "MAG_Child")
            ],
            "{key}"
        );
        assert_eq!(
            references(&file),
            [
                (ReferenceKind::References, "Target_Spell"),
                (ReferenceKind::References, "Target_Parent")
            ],
            "{key}"
        );
    }
}

// HEAD 5220c3a1 projections retain declaration identity, spans, export,
// containment, and exact reference spans/owners. Expectations below were
// captured with the native HEAD extractor, before applying the subtraction.
fn bg3_head_facts(file: &ExtractedFile) -> Vec<String> {
    let names = file
        .symbols
        .iter()
        .map(|symbol| (symbol.id.as_str(), symbol.qualified_name.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut facts = Vec::new();
    for symbol in &file.symbols {
        facts.push(format!(
            "S|{}|{}|{}|{}-{}|{}",
            symbol.kind.as_str(),
            symbol.name,
            symbol.qualified_name,
            symbol.span.start_byte(),
            symbol.span.end_byte(),
            symbol.export.exported
        ));
    }
    for containment in &file.containments {
        facts.push(format!(
            "C|{}|{}",
            names[containment.parent.as_str()],
            names[containment.child.as_str()]
        ));
    }
    for reference in &file.references {
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
            reference.span.end_byte()
        ));
    }
    facts
}

#[test]
fn bg3_lsj_wrapped_children_and_module_info_keep_head_definitions() {
    for (path, source, expected) in [
        (
            "Public/Demo/RootTemplates/Wrapped.lsj",
            "{\"Children\":[{\n  \"Name\": \"MAG_JSON_Item\",\n  \"UUID\": \"44444444-4444-4444-4444-444444444444\",\n  \"ParentTemplateId\": \"22222222-2222-2222-2222-222222222222\",\n  \"Children\": [\n    {\n      \"Name\": \"MAG_JSON_Child\",\n      \"Boosts\": \"UnlockSpell(Target_Spell)\"\n    }\n  ],\n  \"Tags\": [\n    \"Target_Tag\"\n  ]\n}\n]}",
            &[
                "S|resource|MAG_JSON_Item|44444444-4444-4444-4444-444444444444|26-39|true",
                "S|resource|MAG_JSON_Child|Public/Demo/RootTemplates/Wrapped.lsj::MAG_JSON_Child|191-205|false",
                "C|44444444-4444-4444-4444-444444444444|Public/Demo/RootTemplates/Wrapped.lsj::MAG_JSON_Child",
                "R|Public/Demo/RootTemplates/Wrapped.lsj::MAG_JSON_Child|UnlockSpell|references|225-236",
                "R|Public/Demo/RootTemplates/Wrapped.lsj::MAG_JSON_Child|Target_Spell|references|237-249",
                "R|44444444-4444-4444-4444-444444444444|22222222-2222-2222-2222-222222222222|references|115-151",
                "R|44444444-4444-4444-4444-444444444444|Target_Tag|references|280-290",
            ][..],
        ),
        (
            "Mods/Demo/meta.lsj",
            "{\"save\":{\"regions\":{\"Config\":{\"root\":[{\"ModuleInfo\":[{\"Name\":\"Demo_Module\",\"UUID\":\"33333333-3333-3333-3333-333333333333\",\"ParentTemplateId\":\"Target_Module\"}]}]}}}}",
            &[
                "S|resource|Demo_Module|33333333-3333-3333-3333-333333333333|62-73|true",
                "R|33333333-3333-3333-3333-333333333333|Target_Module|references|141-154",
            ][..],
        ),
        (
            "Mods/Demo/meta.lsj",
            "{\"Dependencies\":[{\"ModuleShortDesc\":[{\"Name\":\"GustavX\",\"UUID\":\"cb555efe-2d9e-131f-8195-a89329d218ea\"}]}]}",
            &["S|resource|GustavX|cb555efe-2d9e-131f-8195-a89329d218ea|46-53|true"][..],
        ),
    ] {
        let file = extract(path, source);
        assert_eq!(bg3_head_facts(&file), expected, "{path}");
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn bg3_lsj_typed_dependencies_keep_head_facts_through_gameobject_wrappers() {
    for (source, expected) in [
        (
            "{\"save\":{\"regions\":{\"Config\":{\"root\":[{\"Dependencies\":[{\"ModuleShortDesc\":[{\"Name\":{\"type\":\"FixedString\",\"value\":\"GustavX\"},\"UUID\":{\"type\":\"guid\",\"value\":\"cb555efe-2d9e-131f-8195-a89329d218ea\"}}]}]}]}}}}",
            &[
                "R|<file>|FixedString|references|92-103",
                "R|<file>|GustavX|references|114-121",
                "R|<file>|cb555efe-2d9e-131f-8195-a89329d218ea|references|155-191",
            ][..],
        ),
        (
            "{\"save\":{\"regions\":{\"Config\":{\"root\":[{\"Dependencies\":[{\"ModuleShortDesc\":[{\"GameObject\":[{\"Name\":{\"type\":\"FixedString\",\"value\":\"GustavX\"},\"UUID\":{\"type\":\"guid\",\"value\":\"cb555efe-2d9e-131f-8195-a89329d218ea\"}}]}]}]}]}}}}",
            &[
                "R|<file>|FixedString|references|107-118",
                "R|<file>|GustavX|references|129-136",
                "R|<file>|cb555efe-2d9e-131f-8195-a89329d218ea|references|170-206",
            ][..],
        ),
        (
            "{\"save\":{\"regions\":{\"Config\":{\"root\":[{\"Dependencies\":[{\"ModuleShortDesc\":[{\"GameObjects\":[{\"Name\":{\"type\":\"FixedString\",\"value\":\"GustavX\"},\"UUID\":{\"type\":\"guid\",\"value\":\"cb555efe-2d9e-131f-8195-a89329d218ea\"}}]}]}]}]}}}}",
            &[
                "R|<file>|FixedString|references|108-119",
                "R|<file>|GustavX|references|130-137",
                "R|<file>|cb555efe-2d9e-131f-8195-a89329d218ea|references|171-207",
            ][..],
        ),
    ] {
        let file = extract("Mods/Demo/meta.lsj", source);
        assert_eq!(bg3_head_facts(&file), expected, "{source}");
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn bg3_lsj_only_consumed_scalar_fields_lose_head_references() {
    for (source, expected) in [
        (
            "{\"Name\":\"MAG_Primary\",\"NameFS\":\"MAG_Secondary\",\"UUID\":\"33333333-3333-3333-3333-333333333333\",\"Guid\":\"44444444-4444-4444-4444-444444444444\",\"ParentTemplateId\":\"MAG_Primary\",\"Children\":[{\"Name\":{\"type\":\"FixedString\",\"value\":\"GustavX\"},\"UUID\":{\"type\":\"guid\",\"value\":\"cb555efe-2d9e-131f-8195-a89329d218ea\"}}]}",
            &[
                "S|resource|MAG_Primary|33333333-3333-3333-3333-333333333333|9-20|true",
                "R|33333333-3333-3333-3333-333333333333|FixedString|references|201-212",
                "R|33333333-3333-3333-3333-333333333333|GustavX|references|223-230",
                "R|33333333-3333-3333-3333-333333333333|cb555efe-2d9e-131f-8195-a89329d218ea|references|264-300",
                "R|33333333-3333-3333-3333-333333333333|44444444-4444-4444-4444-444444444444|references|101-137",
                "R|33333333-3333-3333-3333-333333333333|MAG_Secondary|references|32-45",
                "R|33333333-3333-3333-3333-333333333333|MAG_Primary|references|9-20",
            ][..],
        ),
        (
            "{\"Name\":\"MAG_Primary\",\"UUID\":\"Target_One Target_Two\",\"Guid\":\"44444444-4444-4444-4444-444444444444\"}",
            &[
                "S|resource|MAG_Primary|Mods/Demo/meta.lsj::MAG_Primary|9-20|true",
                "R|Mods/Demo/meta.lsj::MAG_Primary|44444444-4444-4444-4444-444444444444|references|61-97",
                "R|Mods/Demo/meta.lsj::MAG_Primary|Target_One|references|30-40",
                "R|Mods/Demo/meta.lsj::MAG_Primary|Target_Two|references|41-51",
            ][..],
        ),
    ] {
        let file = extract("Mods/Demo/meta.lsj", source);
        assert_eq!(bg3_head_facts(&file), expected, "{source}");
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn bg3_lsj_typed_attributes_keep_head_references_without_declarations() {
    for key in ["Name", "NameFS", "name", "UUID", "Guid", "id"] {
        let file = extract(
            "Public/Demo/RootTemplates/Typed.lsj",
            &format!(
                r#"{{"{key}":{{"type":"FixedString","value":"MAG_Typed"}},"ParentTemplateId":{{"type":"guid","value":"44444444-4444-4444-4444-444444444444"}}}}"#
            ),
        );
        assert_eq!(symbols(&file), []);
        let mut uses = references(&file);
        uses.sort_unstable();
        assert_eq!(
            uses,
            [
                "44444444-4444-4444-4444-444444444444",
                "FixedString",
                "MAG_Typed",
            ]
            .map(|name| (ReferenceKind::References, name)),
            "{key}"
        );
        assert!(
            file.references
                .iter()
                .all(|reference| reference.owner.is_none())
        );
        assert_eq!(file.parse_status, FileParseStatus::Parsed);
    }
}

#[test]
fn bg3_uuid_resources_are_addressable_and_lsj_follows_v1_naming() {
    let lsx = extract("Public/Shared/RootTemplates/Sword.lsx", SWORD_LSX);
    let sword = &lsx.symbols[1];
    assert!(
        sword.export.exported,
        "a UUID-identified template is a game-global target"
    );
    let lsj = extract(
        "Public/Shared/RootTemplates/JsonItem.lsj",
        r#"{"NameFS": "", "Name": "MAG_JSON_Item", "UUID": "33333333-3333-3333-3333-333333333333", "ParentTemplateId": "44444444-4444-4444-4444-444444444444", "Children": [{"Name": "MAG_JSON_Child", "Boosts": "UnlockSpell(Target_Spell)"}], "Tags": ["Target_Tag"]}"#,
    );
    let item = lsj
        .symbols
        .iter()
        .find(|symbol| symbol.name == "MAG_JSON_Item")
        .unwrap_or_else(|| panic!("missing LSJ item: {:?}", lsj.symbols));
    assert_eq!(item.qualified_name, "33333333-3333-3333-3333-333333333333");
    let child = lsj
        .symbols
        .iter()
        .find(|symbol| symbol.name == "MAG_JSON_Child")
        .unwrap_or_else(|| panic!("missing LSJ child: {:?}", lsj.symbols));
    assert_eq!(
        child.qualified_name,
        "Public/Shared/RootTemplates/JsonItem.lsj::MAG_JSON_Child"
    );
    for name in [
        "44444444-4444-4444-4444-444444444444",
        "Target_Spell",
        "Target_Tag",
    ] {
        assert!(
            has_reference(&lsj, ReferenceKind::References, name),
            "{name}"
        );
    }
}

#[test]
fn bg3_lsx_fields_bind_to_the_innermost_open_object() {
    let file = extract(
        "Public/Shared/Stats/Wrapped.lsx",
        r#"<save><region id="Data"><node id="Outer">
<attribute id="Name" value="Outer_Object"/>
<children><node id="Inner"><attribute id="Name" value="Inner_Object"/><attribute id="TemplateId" value="Inner_Template_1"/></node></children>
<attribute id="RootTemplate" value="Outer_Template_2"/>
</node></region></save>"#,
    );
    let owner_of = |name: &str| {
        let reference = file
            .references
            .iter()
            .find(|reference| reference.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        file.symbols
            .iter()
            .find(|symbol| Some(&symbol.id) == reference.owner.as_ref())
            .map(|symbol| symbol.name.as_str())
    };
    assert_eq!(owner_of("Inner_Template_1"), Some("Inner_Object"));
    assert_eq!(
        owner_of("Outer_Template_2"),
        Some("Outer_Object"),
        "a field after a nested object closes belongs to the enclosing object"
    );
}

#[test]
fn bg3_anubis_event_handlers_end_the_preceding_behavior_scope() {
    let file = extract(
        "Mods/Demo/Scripts/anubis/node/Events.ann",
        "game.states.Guard = State {\n  nodes.Wander = Proxy {\n    Wander()\n  }\n  events.EnteredTrigger = function(e)\n    SetEntityEvent(me, \"RaiseAlarm\")\n  end\n}\n",
    );
    let call = file
        .references
        .iter()
        .find(|reference| reference.name == "SetEntityEvent")
        .unwrap_or_else(|| panic!("missing SetEntityEvent"));
    let owner = file
        .symbols
        .iter()
        .find(|symbol| Some(&symbol.id) == call.owner.as_ref())
        .map(|symbol| symbol.qualified_name.as_str());
    assert_eq!(owner, Some("game.states.Guard"));
}

#[test]
fn aura_comment_blanking_preserves_utf8_byte_offsets_and_lines() {
    let source = "<aura:component>\r\n<!-- caf\u{e9} \u{1f600} {!c.ignored} -->\r\n<ui:button press=\"{!c.save}\"/></aura:component>\n";
    let file = extract("force-app/main/default/aura/Cafe/Cafe.cmp", source);
    let save = file
        .references
        .iter()
        .find(|reference| reference.kind == ReferenceKind::Calls)
        .unwrap_or_else(|| panic!("missing save call: {:?}", file.references));
    assert_eq!(save.name, "save");
    let start = usize::try_from(save.span.start_byte())
        .unwrap_or_else(|error| panic!("span overflow: {error}"));
    assert_eq!(&source[start..start + "save".len()], "save");
    assert_eq!(save.span.start_line(), 3);
    assert_eq!(
        references(&file)
            .into_iter()
            .filter(|(kind, _)| *kind == ReferenceKind::Calls)
            .count(),
        1
    );
}

#[test]
fn bg3_identities_are_screened_trimmed_and_never_degrade_the_file() {
    let file = extract(
        "Public/Shared/RootTemplates/Identity.lsx",
        r#"<save><region id="Templates">
<node id="GameObject"><attribute id="UUID" value="ghp_examplecredential12345AB"/><attribute id="Name" value="Leaky_Object"/></node>
<node id="GameObject"><attribute id="UUID" value=""/><attribute id="Name" value="Empty_Identity"/></node>
<node id="GameObject"><attribute id="UUID" value=" 66666666-6666-6666-6666-666666666666 "/><attribute id="Name" value="Spaced_Identity"/></node>
</region></save>"#,
    );
    let qualified = |name: &str| {
        file.symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .map_or_else(
                || panic!("missing {name}: {:?}", file.symbols),
                |symbol| symbol.qualified_name.as_str(),
            )
    };
    assert_eq!(
        qualified("Leaky_Object"),
        "Public/Shared/RootTemplates/Identity.lsx::Templates::Leaky_Object"
    );
    assert_eq!(
        qualified("Empty_Identity"),
        "Public/Shared/RootTemplates/Identity.lsx::Templates::Empty_Identity"
    );
    assert_eq!(
        qualified("Spaced_Identity"),
        "66666666-6666-6666-6666-666666666666"
    );
    assert!(!format!("{file:?}").contains("examplecredential"));
}

#[test]
fn bg3_field_values_reference_plain_identifiers_at_every_site() {
    let file = extract(
        "Public/Shared/Stats/Fields.lsx",
        r#"<save><region id="Stats"><node id="Item">
<attribute id="Name" value="MAG_Ring"/>
<attribute id="Passives" value="Darkvision;Darkvision"/>
<attribute id="Boosts" value="UnlockSpell(Target_Spell)"/>
<attribute id="ParentTemplateId" value="77777777-7777-7777-7777-777777777777"/>
<attribute id="RootTemplate" value="77777777-7777-7777-7777-777777777777"/>
<attribute id="Weight" value="1.5"/>
</node></region></save>"#,
    );
    let count = |name: &str| {
        file.references
            .iter()
            .filter(|reference| reference.name == name)
            .count()
    };
    assert_eq!(count("Darkvision"), 2, "{:?}", references(&file));
    assert_eq!(count("UnlockSpell"), 1);
    assert_eq!(count("Target_Spell"), 1);
    assert_eq!(count("77777777-7777-7777-7777-777777777777"), 2);
    assert_eq!(count("1.5"), 0);
    let stats = extract(
        "Mods/Demo/Stats/Generated/Data/Weapons.txt",
        "new entry \"WPN_Ring\"\ndata \"Passives\" \"Darkvision\"\ndata \"Damage\" \"1d8\"\n",
    );
    assert_eq!(
        references(&stats),
        [(ReferenceKind::References, "Darkvision")]
    );
    // A `/` in a dice expression is ordinary BG3 syntax, not a URL.
    let functors = extract(
        "Mods/Demo/Stats/Generated/Data/Spells.txt",
        "new entry \"Target_Fireball\"\ndata \"SpellFail\" \"DealDamage(8d6/2,Fire,Magical)\"\n",
    );
    assert_eq!(
        references(&functors),
        [
            (ReferenceKind::References, "DealDamage"),
            (ReferenceKind::References, "Fire"),
            (ReferenceKind::References, "Magical"),
        ]
    );
    // URL-shaped and key-bearing values stay opaque: neither the location nor
    // the body of an issued key split off at its `-` becomes a reference.
    // GitLab-shaped tokens are assembled at runtime so no complete key-shaped
    // literal sits in the source (secret scanners flag those).
    let gitlab = |body: &str| ["glpat", body].join("-");
    let locations = extract(
        "Mods/Demo/Stats/Generated/Data/Leaky.txt",
        &format!(
            "new entry \"SafeEntry\"\ndata \"Boosts\" \"//example.invalid/{}/m\"\ndata \"Passives\" \"Assets/{}\"\ndata \"Icon\" \"https://example.invalid/IconSword\"\n",
            gitlab("AbCdEf01234567890123"),
            gitlab("QwErTy98765432109876"),
        ),
    );
    assert_eq!(references(&locations), []);
    let rendered = format!("{locations:?}");
    for secret in ["AbCdEf01234567890123", "QwErTy98765432109876", "IconSword"] {
        assert!(!rendered.contains(secret), "{secret} leaked");
    }
}

#[test]
fn bg3_nameless_wrapper_chains_lift_references_to_the_file() {
    let depth = 200;
    let mut source = String::from("<save>");
    for _ in 0..depth {
        source.push_str(
            "<node id=\"Object\"><attribute id=\"TemplateId\" value=\"Wrapped_Target_1\"/>",
        );
    }
    for _ in 0..depth {
        source.push_str("</node>");
    }
    source.push_str("</save>");
    let file = extract("Public/Shared/Deep/Wrappers.lsx", &source);
    assert_eq!(file.symbols.len(), 0, "{:?}", file.symbols);
    assert_eq!(file.references.len(), depth);
    assert!(
        file.references
            .iter()
            .all(|reference| reference.owner.is_none())
    );
}

#[test]
fn liquid_rejects_dynamic_partners_root_arrays_and_unterminated_schemas() {
    let file = extract(
        "snippets/dynamic.liquid",
        "{% render chosen, label: 'hero card' %}\n{% schema %}[\"Hero\"]{% endschema %}\n{% schema %}\n{% schema %}\n{% render 'footer' %}\n",
    );
    assert_eq!(
        symbols(&file),
        [
            (SymbolKind::Constant, "schema"),
            (SymbolKind::Component, "footer"),
            (SymbolKind::Import, "footer")
        ]
    );
    assert!(!has_reference(
        &file,
        ReferenceKind::Imports,
        "snippets/hero card.liquid"
    ));
}

#[test]
fn aura_action_spans_point_after_the_provider_and_types_are_screened() {
    let source = "<aura:component><aura:attribute name=\"token\" type=\"ghp_examplecredential12345AB\"/><a onclick=\"{!c.c}\"/><b onclick=\"{! controller.roll }\"/></aura:component>";
    let file = extract("force-app/main/default/aura/Spans/Spans.cmp", source);
    for name in ["c", "roll"] {
        let call = file
            .references
            .iter()
            .find(|reference| reference.kind == ReferenceKind::Calls && reference.name == name)
            .unwrap_or_else(|| panic!("missing {name}: {:?}", file.references));
        let start = usize::try_from(call.span.start_byte())
            .unwrap_or_else(|error| panic!("span overflow: {error}"));
        assert_eq!(&source[start..start + name.len()], name);
        assert_eq!(
            &source[start - 1..start],
            ".",
            "{name} must follow the provider dot"
        );
    }
    assert!(!format!("{file:?}").contains("examplecredential"));
}

#[test]
fn literal_derived_bg3_and_aura_names_never_carry_credentials() {
    let lsx = extract(
        "Public/Shared/RootTemplates/Url.lsx",
        r#"<save><region id="Templates"><node id="GameObject"><attribute id="UUID" value="https://alice:hunter2@example.invalid/11111111-1111-1111-1111-111111111111"/><attribute id="Name" value="Url_Identity"/><attribute id="Boosts" value="IF(IsTagged('TAG_1')):UnlockSpell(Target_Spell)"/></node></region></save>"#,
    );
    assert_eq!(
        lsx.symbols
            .iter()
            .find(|symbol| symbol.name == "Url_Identity")
            .map(|symbol| symbol.qualified_name.as_str()),
        Some("Public/Shared/RootTemplates/Url.lsx::Templates::Url_Identity")
    );
    assert!(has_reference(
        &lsx,
        ReferenceKind::References,
        "UnlockSpell"
    ));
    let anubis = extract(
        "Mods/Demo/Scripts/anubis/node/Leak.ann",
        "game.states.Leak = State {\n  Entity(\"ghp_examplecredential_11111111-1111-1111-1111-111111111111\")\n  Entity(\"S_Door_22222222-2222-2222-2222-222222222222\")\n}\n",
    );
    assert!(has_reference(
        &anubis,
        ReferenceKind::References,
        "S_Door_22222222-2222-2222-2222-222222222222"
    ));
    let aura = extract(
        "force-app/main/default/aura/Typed/Typed.cmp",
        "<aura:component><aura:attribute name=\"rows\" type=\"List<ghp_examplecredential>\"/></aura:component>",
    );
    for rendered in [
        format!("{lsx:?}"),
        format!("{anubis:?}"),
        format!("{aura:?}"),
    ] {
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("examplecredential"));
    }
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("markup source limit failed: {error}"))
}

#[test]
fn liquid_schema_display_email_retains_exact_name_and_search_text() {
    let name = "Contact support@example.invalid";
    let file = extract(
        "sections/contact.liquid",
        &format!("{{% schema %}}{{\"name\":\"{name}\"}}{{% endschema %}}"),
    );
    assert_eq!(file.symbols.len(), 1);
    let schema = &file.symbols[0];
    assert_eq!(schema.name, name);
    assert_eq!(
        schema.qualified_name,
        "sections/contact.liquid::schema:Contact support@example.invalid"
    );
    assert_eq!(
        schema.body_search_text,
        "schema Contact support@example.invalid"
    );
}

#[test]
fn liquid_schema_credential_display_names_use_the_schema_fallback() {
    for value in credential_support::CREDENTIAL_INPUTS
        .into_iter()
        .chain(["https://alice:hunter2@example.invalid"])
    {
        let source = format!("{{% schema %}}{{\"name\":\"{value}\"}}{{% endschema %}}");
        let file = extract("sections/main.liquid", &source);
        credential_support::assert_no_credentials(&file);
        assert!(!format!("{file:?}").contains("hunter2"));
        assert!(file.symbols.iter().any(|symbol| symbol.name == "schema"));
    }
    credential_support::assert_screened(
        "sections/main.liquid",
        "{% schema %}{\"name\":\"@VALUE@\"}{% endschema %}",
        "Hero",
    );
}

#[test]
fn aura_attribute_types_screen_credentials_before_type_head_projection() {
    for value in credential_support::CREDENTIAL_INPUTS {
        let source = format!(
            "<aura:component><aura:attribute name=\"value\" type=\"{value}\"/></aura:component>"
        );
        let file = extract("aura/Type/Type.cmp", &source);
        credential_support::assert_no_credentials(&file);
        assert!(
            file.references
                .iter()
                .all(|reference| reference.kind != ReferenceKind::TypeOf)
        );
        let field = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Field)
            .unwrap_or_else(|| panic!("safe field is still extracted: {file:?}"));
        assert_eq!(field.name, "value");
        assert!(field.signature.is_none());
    }
    credential_support::assert_screened(
        "aura/Type/Type.cmp",
        "<aura:component><aura:attribute name=\"value\" type=\"@VALUE@\"/></aura:component>",
        "Account",
    );
}
