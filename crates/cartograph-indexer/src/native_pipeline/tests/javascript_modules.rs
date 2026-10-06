use super::*;
use std::fmt::Write as _;

async fn generation(fixtures: &[(&str, &str)]) -> NativeGeneration {
    let directory = tempdir().unwrap_or_else(|error| panic!("fixture directory: {error}"));
    fs::create_dir(directory.path().join(".git"))
        .unwrap_or_else(|error| panic!("fixture git directory: {error}"));
    for (path, source) in fixtures {
        let target = directory.path().join(path);
        fs::create_dir_all(target.parent().unwrap_or(directory.path()))
            .unwrap_or_else(|error| panic!("fixture parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("fixture source: {error}"));
    }
    let serial = build(directory.path(), SERIAL_WORKERS).await;
    let parallel = build(directory.path(), PARALLEL_WORKERS).await;
    assert_eq!(serial.facts().digest(), parallel.facts().digest());
    assert_eq!(serial.facts().references(), parallel.facts().references());
    assert_eq!(serial.facts().edges(), parallel.facts().edges());
    serial
}

fn call<'a>(facts: &'a CanonicalGenerationFacts, file: &str, name: &str) -> &'a ReferenceInput {
    let owner = capability_symbol(facts, file, "run");
    CapabilityReferenceQuery::new(facts, owner).named(name, ReferenceKind::Calls)
}

fn assert_target(facts: &CanonicalGenerationFacts, site: &ReferenceInput, target: (&str, &str)) {
    let target = capability_symbol(facts, target.0, target.1);
    assert_eq!(
        site.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{site:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tsconfig_extends_uses_the_declaring_base_and_child_overrides() {
    let built = generation(&[
        ("config/base.json", r##"{"compilerOptions":{"baseUrl":"..","paths":{"#shared/*":["src/*"]}}}"##),
        ("tsconfig.json", r#"{"extends":"./config/base"}"#),
        ("app/tsconfig.json", r##"{"extends":"../tsconfig.json","compilerOptions":{"paths":{"#child/*":["app/lib/*"]}}}"##),
        ("src/util.ts", "export function util() {}"),
        ("app/lib/child.ts", "export function child() {}"),
        ("src/main.ts", "import {util} from '#shared/util'; import {missing} from '#shared/missing'; export function run() { util(); missing(); }"),
        ("app/main.ts", "import {child} from '#child/child'; import {util} from '#shared/util'; export function run() { child(); util(); }"),
    ]).await;
    let facts = built.facts();
    let site = call(facts, "src/main.ts", "util");
    assert_target(facts, site, ("src/util.ts", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert_target(
        facts,
        call(facts, "app/main.ts", "child"),
        ("app/lib/child.ts", "child"),
    );
    assert!(
        call(facts, "src/main.ts", "missing")
            .target_symbol_id
            .is_none()
    );
    assert!(
        call(facts, "app/main.ts", "util")
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tsconfig_extends_cycles_and_unindexed_parents_do_not_guess() {
    let built = generation(&[
        ("tsconfig.json", r#"{"extends":"./config/base.json"}"#),
        ("config/base.json", r#"{"extends":"../tsconfig.json"}"#),
        ("src/util.ts", "export function util() {}"),
        (
            "src/main.ts",
            "import {util} from '#shared/util'; export function run() { util(); }",
        ),
    ])
    .await;
    assert!(
        call(built.facts(), "src/main.ts", "util")
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indexed_workspace_config_extends_resolves_and_ignored_configs_are_not_read() {
    let built = generation(&[
        (
            "packages/config/package.json",
            r#"{"name":"@configs/base"}"#,
        ),
        (
            "packages/config/tsconfig.json",
            r##"{"compilerOptions":{"baseUrl":"../..","paths":{"#shared/*":["src/*"]}}}"##,
        ),
        ("tsconfig.json", r#"{"extends":"@configs/base"}"#),
        ("app/tsconfig.json", r#"{"extends":"../hidden.json"}"#),
        (".gitignore", "hidden.json\n"),
        (
            "hidden.json",
            r##"{"compilerOptions":{"paths":{"#shared/*":["src/*"]}}}"##,
        ),
        ("src/util.ts", "export function util() {}"),
        (
            "src/main.ts",
            "import {util} from '#shared/util'; export function run() { util(); }",
        ),
        (
            "app/main.ts",
            "import {util} from '#shared/util'; export function run() { util(); }",
        ),
    ])
    .await;
    let site = call(built.facts(), "src/main.ts", "util");
    assert_target(built.facts(), site, ("src/util.ts", "util"));
    assert_eq!(
        site.resolution_provenance,
        "native-typescript-config-fallback"
    );
    assert_eq!(site.confidence, 0.85);
    assert!(
        call(built.facts(), "app/main.ts", "util")
            .target_symbol_id
            .is_none()
    );
    assert!(
        built
            .facts()
            .files()
            .iter()
            .all(|file| file.normalized_path != "hidden.json")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_config_redirects_abstain_and_explicit_config_subpaths_remain_exact() {
    let mut sources = vec![(
        "src/util.ts".to_owned(),
        "export function util() {}".to_owned(),
    )];
    for (name, extra) in [
        ("main", r#", "main":"./actual.json""#),
        ("exports", r#", "exports":"./actual.json""#),
        ("tsconfig", r#", "tsconfig":"./actual.json""#),
        ("index", ""),
    ] {
        let package = format!("packages/{name}");
        sources.push((
            format!("{package}/package.json"),
            format!(r#"{{"name":"config-{name}"{extra}}}"#),
        ));
        for path in ["tsconfig.json", "actual.json"] {
            sources.push((
                format!("{package}/{path}"),
                r##"{"compilerOptions":{"baseUrl":"../..","paths":{"#shared":["src/util.ts"]}}}"##
                    .to_owned(),
            ));
        }
        sources.push((
            format!("apps/{name}/tsconfig.json"),
            format!(r#"{{"extends":"config-{name}"}}"#),
        ));
        sources.push((
            format!("apps/{name}/main.ts"),
            "import {util} from '#shared'; export function run() { util(); }".to_owned(),
        ));
    }
    sources.push(("packages/index/index.json".to_owned(), "{}".to_owned()));
    sources.push((
        "apps/explicit/tsconfig.json".to_owned(),
        r#"{"extends":"config-main/actual.json"}"#.to_owned(),
    ));
    sources.push((
        "apps/explicit/main.ts".to_owned(),
        "import {util} from '#shared'; export function run() { util(); }".to_owned(),
    ));
    let fixtures = sources
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let built = generation(&fixtures).await;
    for name in ["main", "exports", "tsconfig", "index"] {
        let site = call(built.facts(), &format!("apps/{name}/main.ts"), "util");
        assert!(site.target_symbol_id.is_none(), "{site:?}");
    }
    let site = call(built.facts(), "apps/explicit/main.ts", "util");
    assert_target(built.facts(), site, ("src/util.ts", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_child_extends_blocks_parent_aliases_and_assumed_child_bases() {
    let built = generation(&[
        ("tsconfig.json", r##"{"compilerOptions":{"paths":{"#same/*":["src/*"]}}}"##),
        ("app/tsconfig.json", r##"{"extends":"../not-indexed.json","compilerOptions":{"paths":{"#child/*":["./*"]}}}"##),
        ("src/util.ts", "export function util() {}"),
        ("app/util.ts", "export function util() {}"),
        ("src/main.ts", "import {util} from '#same/util'; export function run() { util(); }"),
        ("app/main.ts", "import {util as parent} from '#same/util'; import {util as child} from '#child/util'; export function run() { parent(); child(); }"),
    ]).await;
    let site = call(built.facts(), "src/main.ts", "util");
    assert_target(built.facts(), site, ("src/util.ts", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    for name in ["parent", "child"] {
        let site = call(built.facts(), "app/main.ts", name);
        assert!(site.target_symbol_id.is_none());
        assert_eq!(
            site.resolution_provenance,
            EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_config_extends_carries_convention_provenance_and_abstains_when_missing() {
    let built = generation(&[
        ("tsconfig.json", r#"{"extends":"./config"}"#),
        (
            "config/tsconfig.json",
            r##"{"compilerOptions":{"baseUrl":"..","paths":{"#shared":["src/util.ts"]}}}"##,
        ),
        ("app/tsconfig.json", r#"{"extends":"./missing-directory"}"#),
        ("src/util.ts", "export function util() {}"),
        (
            "src/main.ts",
            "import {util} from '#shared'; export function run() { util(); }",
        ),
        (
            "app/main.ts",
            "import {util} from '#shared'; export function run() { util(); }",
        ),
    ])
    .await;
    let site = call(built.facts(), "src/main.ts", "util");
    assert_target(built.facts(), site, ("src/util.ts", "util"));
    assert_eq!(
        site.resolution_provenance,
        "native-typescript-config-fallback"
    );
    assert_eq!(site.confidence, 0.85);
    assert!(
        call(built.facts(), "app/main.ts", "util")
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inherited_config_changes_are_hashed_and_change_the_resolved_target() {
    let directory = tempdir().unwrap_or_else(|error| panic!("fixture directory: {error}"));
    for (path, source) in [
        ("tsconfig.json", r#"{"extends":"./base.json"}"#),
        (
            "main.ts",
            "import {util} from '#shared/util'; export function run() { util(); }",
        ),
        ("left.ts", "export function util() {}"),
        ("right.ts", "export function util() {}"),
    ] {
        fs::write(directory.path().join(path), source)
            .unwrap_or_else(|error| panic!("fixture: {error}"));
    }
    let mut hashes = Vec::new();
    for target in ["left", "right"] {
        let config =
            format!(r##"{{"compilerOptions":{{"paths":{{"#shared/util":["{target}.ts"]}}}}}}"##);
        fs::write(directory.path().join("base.json"), config)
            .unwrap_or_else(|error| panic!("config: {error}"));
        let built = build(directory.path(), SERIAL_WORKERS).await;
        assert_target(
            built.facts(),
            call(built.facts(), "main.ts", "util"),
            (&format!("{target}.ts"), "util"),
        );
        let file = built
            .facts()
            .files()
            .iter()
            .find(|file| file.normalized_path == "base.json")
            .unwrap_or_else(|| panic!("parent config missing from manifest"));
        hashes.push(file.content_hash.clone());
    }
    assert_ne!(hashes[0], hashes[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jsonc_trailing_commas_preserve_string_tokens_and_resolve_aliases() {
    let built = generation(&[
        ("tsconfig.json", r##"{
            // typical generated config
            "compilerOptions": {"paths": {"#lib/*": ["src/*",],},},
            "note": "quoted ,} ,] https://example.invalid/*literal*/",
        }"##),
        ("src/util.ts", "export function util() {}"),
        ("src/main.ts", "import {util} from '#lib/util'; import {missing} from '#lib/missing'; export function run() { util(); missing(); }"),
    ]).await;
    let site = call(built.facts(), "src/main.ts", "util");
    assert_target(built.facts(), site, ("src/util.ts", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert!(
        call(built.facts(), "src/main.ts", "missing")
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alias_ties_keep_declaration_order_and_missing_targets_allow_conventions() {
    let built = generation(&[
        ("tsconfig.json", r##"{"compilerOptions":{"paths":{"#x/*/end":["preferred/*"],"#x/*":["other/*"],"@/*":["absent/*"],"direct/*":["absent/*"],"#bad/*":["../outside/*"]}}}"##),
        ("preferred/tool.ts", "export function util() {}"),
        ("other/tool/end.ts", "export function util() {}"),
        ("src/util.ts", "export function util() {}"),
        ("direct/tool.ts", "export function util() {}"),
        ("src/main.ts", "import {util as first} from '#x/tool/end'; import {util as fallback} from '@/util'; import {util as direct} from 'direct/tool'; import {util as missing} from 'direct/missing'; import {util as bad} from '#bad/util'; export function run() { first(); fallback(); direct(); missing(); bad(); }"),
    ]).await;
    let facts = built.facts();
    let first = call(facts, "src/main.ts", "first");
    assert_target(facts, first, ("preferred/tool.ts", "util"));
    assert_eq!(first.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    let fallback = call(facts, "src/main.ts", "fallback");
    assert_target(facts, fallback, ("src/util.ts", "util"));
    assert_eq!(fallback.resolution_provenance, "native-conventional-alias");
    assert_eq!(fallback.confidence, 0.85);
    let direct = call(facts, "src/main.ts", "direct");
    assert_target(facts, direct, ("direct/tool.ts", "util"));
    assert_eq!(direct.resolution_provenance, "native-alias-direct-fallback");
    assert_eq!(direct.confidence, 0.85);
    assert!(
        call(facts, "src/main.ts", "missing")
            .target_symbol_id
            .is_none()
    );
    assert!(call(facts, "src/main.ts", "bad").target_symbol_id.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_alias_patterns_abstain_without_disabling_relative_imports() {
    let built = generation(&[
        ("tsconfig.json", r##"{"compilerOptions":{"paths":{"#same":["left.ts"],"#same":["right.ts"]}}}"##),
        ("left.ts", "export function util() {}"),
        ("right.ts", "export function util() {}"),
        ("main.ts", "import {util as alias} from '#same'; import {util as direct} from './left'; export function run() { alias(); direct(); }"),
    ]).await;
    let site = call(built.facts(), "main.ts", "direct");
    assert_target(built.facts(), site, ("left.ts", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert!(
        call(built.facts(), "main.ts", "alias")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn ambiguous_alias_tiers_do_not_fall_through_to_other_valid_files() {
    let facts = build_capability_generation(
        &[
            (
                "tsconfig.json",
                r##"{"compilerOptions":{"paths":{"#pick/*":["src/*","secondary/*"],"@/*":["missing/*"]}}}"##,
            ),
            ("src/util.ts", "export function util() {}"),
            ("src/util.TS", "export function util() {}"),
            ("secondary/util.ts", "export function util() {}"),
            ("util.ts", "export function util() {}"),
            ("@/util.ts", "export function util() {}"),
            ("src/good.ts", "export function good() {}"),
            ("src/Panel/index.vue", "<template><div /></template>"),
            ("src/Panel/index.svelte", "<div />"),
            ("Panel/index.vue", "<template><div /></template>"),
            ("@/Panel/index.vue", "<template><div /></template>"),
            (
                "src/main.ts",
                "import {util as configured} from '#pick/util'; import {util as conventional} from '@/util'; import {good} from '~/good'; import Panel from '@/Panel'; export function run() { configured(); conventional(); good(); new Panel(); }",
            ),
        ],
        false,
    );
    let site = call(&facts, "src/main.ts", "good");
    assert_target(&facts, site, ("src/good.ts", "good"));
    assert_eq!(site.resolution_provenance, "native-conventional-alias");
    for name in ["configured", "conventional"] {
        let site = call(&facts, "src/main.ts", name);
        assert!(site.target_symbol_id.is_none(), "{site:?}");
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
    let owner = capability_symbol(&facts, "src/main.ts", "run");
    let site =
        CapabilityReferenceQuery::new(&facts, owner).named("Panel", ReferenceKind::Instantiates);
    assert!(site.target_symbol_id.is_none(), "{site:?}");
    assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conventional_aliases_prefer_src_and_do_not_claim_missing_packages() {
    let built = generation(&[
        ("src/util.ts", "export function util() {}"),
        ("util.ts", "export function util() {}"),
        ("app/tool.ts", "export function tool() {}"),
        ("src/main.ts", "import {util as a} from 'src/util'; import {util as b} from '@src/util'; import {util as c} from '~/util'; import {tool as d} from '@app/tool'; import {tool as e} from 'app/tool'; import {missing} from 'app/missing'; export function run() { a(); b(); c(); d(); e(); missing(); }"),
    ]).await;
    for name in ["a", "b", "c", "d", "e"] {
        let site = call(built.facts(), "src/main.ts", name);
        let target = if matches!(name, "d" | "e") {
            ("app/tool.ts", "tool")
        } else {
            ("src/util.ts", "util")
        };
        assert_target(built.facts(), site, target);
        assert_eq!(site.resolution_provenance, "native-conventional-alias");
        assert_eq!(site.confidence, 0.85);
    }
    let missing = call(built.facts(), "src/main.ts", "missing");
    assert!(missing.target_symbol_id.is_none());
    assert_eq!(
        missing.resolution_provenance,
        EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extension_probes_follow_importer_order_and_preserve_missing_exports() {
    let built = generation(&[
        ("tsconfig.json", r#"{"compilerOptions":{"moduleResolution":"bundler"}}"#),
        ("src/tool.ts", "export function util() {}"),
        ("src/tool.tsx", "export function util() {}"),
        ("src/tool.js", "export function util() {} export function absent() {}"),
        ("src/lib/index.ts", "export function nested() {}"),
        ("src/lib/index.tsx", "export function nested() {}"),
        ("src/main.ts", "import {util,absent} from './tool'; import {nested} from './lib'; export function run() { util(); nested(); absent(); }"),
        ("src/main.tsx", "import {util} from './tool'; export function run() { util(); }"),
        ("src/main.js", "import {util} from './tool'; export function run() { util(); }"),
    ]).await;
    for (source, target) in [
        ("src/main.ts", "src/tool.ts"),
        ("src/main.tsx", "src/tool.ts"),
        ("src/main.js", "src/tool.js"),
    ] {
        let site = call(built.facts(), source, "util");
        assert_target(built.facts(), site, (target, "util"));
        assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }
    assert_target(
        built.facts(),
        call(built.facts(), "src/main.ts", "nested"),
        ("src/lib/index.ts", "nested"),
    );
    assert!(
        call(built.facts(), "src/main.ts", "absent")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn javascript_directory_probes_precede_typescript_fallbacks_and_label_them() {
    let facts = build_capability_generation(
        &[
            (
                "src/tool.ts",
                "export function util() {} export function absent() {}",
            ),
            ("src/tool/index.js", "export function util() {}"),
            ("src/onlytype.ts", "export function typed() {}"),
            (
                "src/main.js",
                "import {util,absent} from './tool'; import {typed} from './onlytype'; import {typed as missingJs} from './onlytype.js'; import {typed as explicitTs} from './onlytype.ts'; export function run() { util(); absent(); typed(); missingJs(); explicitTs(); }",
            ),
        ],
        false,
    );
    let site = call(&facts, "src/main.js", "util");
    assert_target(&facts, site, ("src/tool/index.js", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    for name in ["typed", "missingJs"] {
        let site = call(&facts, "src/main.js", name);
        assert_target(&facts, site, ("src/onlytype.ts", "typed"));
        assert_eq!(
            site.resolution_provenance,
            "native-cross-language-module-fallback"
        );
        assert_eq!(site.confidence, 0.85);
    }
    let site = call(&facts, "src/main.js", "explicitTs");
    assert_target(&facts, site, ("src/onlytype.ts", "typed"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert!(
        call(&facts, "src/main.js", "absent")
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_and_alias_js_shims_keep_lower_confidence_than_explicit_ts_targets() {
    let built = generation(&[
        ("tsconfig.json", r##"{"compilerOptions":{"paths":{"#shim":["src/shim.js"],"#exact":["src/shim.ts"],"#missing":["src/missing.js"]}}}"##),
        ("src/shim.ts", "export function shim() {}"),
        ("packages/compiled/package.json", r#"{"name":"compiled","exports":"./index.js"}"#),
        ("packages/compiled/index.ts", "export function core() {}"),
        ("packages/typed/package.json", r#"{"name":"typed","exports":"./index.ts"}"#),
        ("packages/typed/index.ts", "export function core() {}"),
        ("src/main.js", "import {shim} from '#shim'; import {shim as exact} from '#exact'; import {shim as missing} from '#missing'; import {core as compiled} from 'compiled'; import {core as typed} from 'typed'; export function run() { shim(); exact(); missing(); compiled(); typed(); }"),
    ]).await;
    for (name, target) in [
        ("shim", ("src/shim.ts", "shim")),
        ("compiled", ("packages/compiled/index.ts", "core")),
    ] {
        let site = call(built.facts(), "src/main.js", name);
        assert_target(built.facts(), site, target);
        assert_eq!(
            site.resolution_provenance,
            "native-cross-language-module-fallback"
        );
        assert_eq!(site.confidence, 0.85);
    }
    for (name, target) in [
        ("exact", ("src/shim.ts", "shim")),
        ("typed", ("packages/typed/index.ts", "core")),
    ] {
        let site = call(built.facts(), "src/main.js", name);
        assert_target(built.facts(), site, target);
        assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(site.confidence, IMPORT_BINDING_CONFIDENCE);
    }
    assert!(
        call(built.facts(), "src/main.js", "missing")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn same_extension_ties_and_mixed_component_stems_abstain() {
    let fixtures = [
        ("src/duplicate.ts", "export function util() {}"),
        ("src/duplicate.TS", "export function util() {}"),
        ("src/Panel/index.vue", "<template><div /></template>"),
        ("src/Panel/index.svelte", "<div />"),
        (
            "src/main.ts",
            "import {util} from './duplicate'; import Panel from './Panel'; export function run() { util(); new Panel(); }",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let reversed = build_capability_generation(&fixtures, true);
    assert_eq!(facts.digest(), reversed.digest());
    let site = call(&facts, "src/main.ts", "util");
    assert!(site.target_symbol_id.is_none());
    assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    let owner = capability_symbol(&facts, "src/main.ts", "run");
    let site =
        CapabilityReferenceQuery::new(&facts, owner).named("Panel", ReferenceKind::Instantiates);
    assert!(site.target_symbol_id.is_none());
    assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_exports_resolve_exact_conditions_arrays_and_specific_wildcards() {
    let built = generation(&[
        ("packages/core/package.json", r#"{"name":"@acme/core","exports":{".":{"types":"./src/index.ts","import":"./wrong.js"},"./feature/*":["./src/features/*.ts"],"./feature/private/*":{"default":"./src/private/*.ts"},"./blocked":null,"./escape":"../foreign.ts"}}"#),
        ("packages/core/src/index.ts", "export function core() {}"),
        ("packages/core/wrong.js", "export function core() {}"),
        ("packages/core/src/features/a.ts", "export function feature() {}"),
        ("packages/core/src/private/a.ts", "export function feature() {}"),
        ("packages/core/blocked.ts", "export function blocked() {}"),
        ("packages/foreign.ts", "export function escape() {}"),
        ("packages/invalid/package.json", r#"{"name":"invalid","exports":{"./feature":"./index.ts","import":"./index.ts"}}"#),
        ("packages/invalid/index.ts", "export function mixed() {}"),
        ("src/main.ts", "import {core} from '@acme/core'; import {feature as a} from '@acme/core/feature/a'; import {feature as b} from '@acme/core/feature/private/a'; import {blocked} from '@acme/core/blocked'; import {escape} from '@acme/core/escape'; import {mixed} from 'invalid/feature'; export function run() { core(); a(); b(); blocked(); escape(); mixed(); }"),
    ]).await;
    let facts = built.facts();
    for (name, target) in [
        ("core", ("packages/core/src/index.ts", "core")),
        ("a", ("packages/core/src/features/a.ts", "feature")),
        ("b", ("packages/core/src/private/a.ts", "feature")),
    ] {
        let site = call(facts, "src/main.ts", name);
        assert_target(facts, site, target);
        assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    }
    for name in ["blocked", "escape", "mixed"] {
        assert!(call(facts, "src/main.ts", name).target_symbol_id.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_workspace_names_and_missing_subpaths_remain_unresolved() {
    let built = generation(&[
        ("packages/a/package.json", r#"{"name":"duplicate","exports":"./index.ts"}"#),
        ("packages/b/package.json", r#"{"name":"duplicate","exports":"./index.ts"}"#),
        ("packages/a/index.ts", "export function util() {}"),
        ("packages/b/index.ts", "export function util() {}"),
        ("packages/local/package.json", r#"{"name":"local"}"#),
        ("packages/local/index.ts", "export function local() {}"),
        ("src/main.ts", "import {util} from 'duplicate'; import {local} from 'local'; import {missing} from 'local/missing'; export function run() { util(); local(); missing(); }"),
    ]).await;
    let site = call(built.facts(), "src/main.ts", "local");
    assert_target(built.facts(), site, ("packages/local/index.ts", "local"));
    assert_eq!(
        site.resolution_provenance,
        "native-workspace-package-fallback"
    );
    assert!(
        call(built.facts(), "src/main.ts", "util")
            .target_symbol_id
            .is_none()
    );
    assert!(
        call(built.facts(), "src/main.ts", "missing")
            .target_symbol_id
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn component_directory_imports_resolve_without_cross_component_guessing() {
    let built = generation(&[
        ("components/Panel/index.vue", "<template><div /></template>"),
        ("src/lib/Counter/index.svelte", "<div />"),
        ("components/View/index.astro", "---\n---\n<div />"),
        ("src/main.ts", "import Panel from '~/components/Panel'; import Counter from '$lib/Counter'; import View from '../components/View'; import Missing from '../components/Missing'; export function run() { new Panel(); new Counter(); new View(); new Missing(); }"),
    ]).await;
    let facts = built.facts();
    let owner = capability_symbol(facts, "src/main.ts", "run");
    for (name, path) in [
        ("Panel", "components/Panel/index.vue"),
        ("Counter", "src/lib/Counter/index.svelte"),
        ("View", "components/View/index.astro"),
    ] {
        let site =
            CapabilityReferenceQuery::new(facts, owner).named(name, ReferenceKind::Instantiates);
        let file = capability_file_symbol(facts, path);
        let target = capability_symbol_by(facts, &file.file_id, |symbol| {
            symbol.symbol_kind == SymbolKind::Component.as_str()
        });
        assert_eq!(
            site.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{site:?}"
        );
        let expected = if name == "View" {
            IMPORT_BINDING_PROVENANCE
        } else {
            "native-conventional-alias"
        };
        assert_eq!(site.resolution_provenance, expected);
    }
    assert!(
        CapabilityReferenceQuery::new(facts, owner)
            .named("Missing", ReferenceKind::Instantiates)
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn wildcard_barrels_bind_transitively_and_preserve_ambiguity_default_and_private_names() {
    let fixtures = [
        (
            "src/impl.ts",
            "export function util() {} function hidden() {} export default function defaultFn() {}",
        ),
        ("src/other.ts", "export function util() {}"),
        ("src/barrel.ts", "export * from './impl';"),
        ("src/nested.ts", "export * from './barrel';"),
        (
            "src/tied.ts",
            "export * from './impl'; export * from './other';",
        ),
        ("src/cycle-a.ts", "export * from './cycle-b';"),
        ("src/cycle-b.ts", "export * from './cycle-a';"),
        ("src/type-barrel.ts", "export type * from './impl';"),
        (
            "src/unknown-barrel.ts",
            "export * from './impl'; export * from './missing';",
        ),
        (
            "src/mixed-barrel.ts",
            "export * from './impl'; export type * from './other';",
        ),
        (
            "src/main.ts",
            "import {util,hidden} from './nested'; import Default from './barrel'; import {util as tied} from './tied'; import {lost} from './cycle-a'; import {util as typeOnly} from './type-barrel'; import {util as unknown} from './unknown-barrel'; import {util as mixed} from './mixed-barrel'; export function run() { util(); hidden(); Default(); tied(); lost(); typeOnly(); unknown(); mixed(); }",
        ),
    ];
    let facts = build_capability_generation(&fixtures, false);
    let reversed = build_capability_generation(&fixtures, true);
    assert_eq!(facts.digest(), reversed.digest());
    let site = call(&facts, "src/main.ts", "util");
    assert_target(&facts, site, ("src/impl.ts", "util"));
    assert_eq!(site.resolution_provenance, "native-wildcard-import");
    for name in [
        "hidden", "Default", "tied", "lost", "typeOnly", "unknown", "mixed",
    ] {
        let site = call(&facts, "src/main.ts", name);
        assert!(site.target_symbol_id.is_none(), "{site:?}");
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
}

#[test]
fn wildcard_over_named_facades_abstains_until_the_named_chain_is_proven() {
    let facts = build_capability_generation(
        &[
            ("src/impl.ts", "export function util() {}"),
            ("src/named.ts", "export {util as renamed} from './impl';"),
            (
                "src/type-named.ts",
                "export type {util as renamed} from './impl';",
            ),
            (
                "src/barrel.ts",
                "export * from './impl'; export * from './named';",
            ),
            ("src/type-barrel.ts", "export * from './type-named';"),
            (
                "src/main.ts",
                "import {util,renamed} from './barrel'; import {renamed as typeNamed} from './type-barrel'; export function run() { util(); renamed(); typeNamed(); }",
            ),
        ],
        false,
    );
    let site = call(&facts, "src/main.ts", "util");
    assert_target(&facts, site, ("src/impl.ts", "util"));
    assert_eq!(site.resolution_provenance, "native-wildcard-import");
    for name in ["renamed", "typeNamed"] {
        let site = call(&facts, "src/main.ts", name);
        assert!(site.target_symbol_id.is_none(), "{site:?}");
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
}

#[test]
fn wildcard_leaf_visits_share_the_barrel_bound_and_abstain_when_exhausted() {
    let small = "export * from './impl';\n".repeat(250);
    let excessive = "export * from './impl';\n".repeat(257);
    let facts = build_capability_generation(
        &[
            ("src/impl.ts", "export function util() {}"),
            ("src/small.ts", &small),
            ("src/excessive.ts", &excessive),
            (
                "src/main.ts",
                "import {util as small} from './small'; import {util as excessive} from './excessive'; export function run() { small(); excessive(); }",
            ),
        ],
        false,
    );
    let site = call(&facts, "src/main.ts", "small");
    assert_target(&facts, site, ("src/impl.ts", "util"));
    assert_eq!(site.resolution_provenance, "native-wildcard-import");
    let site = call(&facts, "src/main.ts", "excessive");
    assert!(site.target_symbol_id.is_none());
    assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[test]
fn wildcard_depth_limit_resolves_eight_hops_and_abstains_beyond_them() {
    let mut sources = vec![(
        "src/impl.ts".to_owned(),
        "export function util() {}".to_owned(),
    )];
    for depth in 0..9 {
        let target = if depth == 8 {
            "impl".to_owned()
        } else {
            format!("barrel{}", depth + 1)
        };
        sources.push((
            format!("src/barrel{depth}.ts"),
            format!("export * from './{target}';"),
        ));
    }
    sources.push(("src/main.ts".to_owned(), "import {util as bounded} from './barrel1'; import {util as excessive} from './barrel0'; export function run() { bounded(); excessive(); }".to_owned()));
    let fixtures = sources
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let facts = build_capability_generation(&fixtures, false);
    let site = call(&facts, "src/main.ts", "bounded");
    assert_target(&facts, site, ("src/impl.ts", "util"));
    assert_eq!(site.resolution_provenance, "native-wildcard-import");
    let site = call(&facts, "src/main.ts", "excessive");
    assert!(site.target_symbol_id.is_none());
    assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wildcard_imports_preserve_conventional_and_package_fallback_confidence() {
    let built = generation(&[
        ("src/impl.ts", "export function util() {}"),
        ("src/conventional.ts", "export * from '@/impl';"),
        ("src/missing.ts", "export * from '@/impl'; export * from '@/absent';"),
        ("packages/local/package.json", r#"{"name":"local"}"#),
        ("packages/local/index.ts", "export function util() {}"),
        ("src/package.ts", "export * from 'local';"),
        ("src/main.ts", "import {util as conventional} from './conventional'; import {util as packaged} from './package'; import {util as missing} from './missing'; export function run() { conventional(); packaged(); missing(); }"),
    ]).await;
    for (name, file) in [
        ("conventional", "src/impl.ts"),
        ("packaged", "packages/local/index.ts"),
    ] {
        let site = call(built.facts(), "src/main.ts", name);
        assert_target(built.facts(), site, (file, "util"));
        assert_eq!(
            site.resolution_provenance,
            "native-wildcard-import-fallback"
        );
        assert_eq!(site.confidence, 0.85);
    }
    for (source, target) in [
        ("src/conventional.ts", "src/impl.ts"),
        ("src/package.ts", "packages/local/index.ts"),
    ] {
        let source = capability_file_symbol(built.facts(), source);
        let target = capability_symbol(built.facts(), target, "util");
        let edge = built
            .facts()
            .edges()
            .iter()
            .find(|edge| {
                edge.source_symbol_id == source.symbol_id
                    && edge.target_symbol_id == target.symbol_id
                    && edge.kind == EdgeKind::Exports
            })
            .unwrap_or_else(|| panic!("missing fallback export edge"));
        assert_eq!(edge.provenance, "native-reexport-all-fallback");
        assert_eq!(edge.confidence, 0.85);
    }
    let missing_file = capability_file_symbol(built.facts(), "src/missing.ts");
    assert!(
        built
            .facts()
            .edges()
            .iter()
            .all(|edge| edge.source_symbol_id != missing_file.symbol_id
                || edge.kind != EdgeKind::Exports)
    );
    let site = call(built.facts(), "src/main.ts", "missing");
    assert!(site.target_symbol_id.is_none());
    assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[test]
fn namespace_fallback_exports_include_default_and_reject_unknown_modules() {
    let facts = build_capability_generation(
        &[
            (
                "src/impl.ts",
                "export function util() {} export default function defaultFn() {}",
            ),
            ("src/namespace.ts", "export * as api from '@/impl';"),
            (
                "src/unknown-namespace.ts",
                "export * as api from '@/missing';",
            ),
        ],
        false,
    );
    let namespace = capability_symbol(&facts, "src/namespace.ts", "api");
    for name in ["util", "defaultFn"] {
        let target = capability_symbol(&facts, "src/impl.ts", name);
        let edge = facts
            .edges()
            .iter()
            .find(|edge| {
                edge.source_symbol_id == namespace.symbol_id
                    && edge.target_symbol_id == target.symbol_id
                    && edge.kind == EdgeKind::Exports
            })
            .unwrap_or_else(|| panic!("missing namespace export {name}"));
        assert_eq!(edge.provenance, "native-reexport-namespace-fallback");
        assert_eq!(edge.confidence, 0.85);
    }
    let unknown = capability_symbol(&facts, "src/unknown-namespace.ts", "api");
    assert!(
        facts.edges().iter().all(
            |edge| edge.source_symbol_id != unknown.symbol_id || edge.kind != EdgeKind::Exports
        )
    );
}

#[test]
fn external_imports_keep_the_intentional_v2_targetless_boundary() {
    let facts = build_capability_generation(
        &[
            (
                "src/main.ts",
                "import {remote} from 'external-package'; import {missing} from './missing'; export function run() { remote(); missing(); }",
            ),
            (
                "src/other.ts",
                "export function remote() {} export function missing() {}",
            ),
        ],
        false,
    );
    for (name, provenance) in [
        ("remote", EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE),
        ("missing", UNRESOLVED_IMPORT_PROVENANCE),
    ] {
        let site = call(&facts, "src/main.ts", name);
        assert!(site.target_symbol_id.is_none());
        assert_eq!(site.resolution_provenance, provenance);
    }
}

#[test]
fn liquid_and_quoted_include_paths_use_unique_suffixes_and_abstain_on_ties() {
    let facts = build_capability_generation(
        &[
            (
                "theme/templates/main.liquid",
                "{% render 'drawer-menu' %}{% render 'missing' %}{% render 'tied' %}",
            ),
            ("theme/snippets/drawer-menu.liquid", "<div />"),
            ("a/snippets/tied.liquid", "<div />"),
            ("b/snippets/tied.liquid", "<div />"),
            (
                "csrc/main.c",
                "#include \"common/foo.h\"\n#include \"common/tied.h\"\n#include <common/foo.h>\n",
            ),
            ("inc/common/foo.h", "int foo(void);"),
            ("a/common/tied.h", "int tied(void);"),
            ("b/common/tied.h", "int tied(void);"),
        ],
        false,
    );
    for (source, name, target) in [
        (
            "theme/templates/main.liquid",
            "snippets/drawer-menu.liquid",
            "theme/snippets/drawer-menu.liquid",
        ),
        ("csrc/main.c", "common/foo.h", "inc/common/foo.h"),
    ] {
        let owner = capability_file_symbol(&facts, source);
        let site = facts
            .references()
            .iter()
            .find(|site| {
                site.file_id == owner.file_id
                    && site.reference_name == name
                    && site.target_symbol_id.is_some()
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing resolved path {name}; sites={:?}",
                    facts.references()
                )
            });
        let target = capability_file_symbol(&facts, target);
        assert_eq!(site.target_symbol_id.as_ref(), Some(&target.symbol_id));
        assert_eq!(site.resolution_provenance, "native-file-path-suffix");
        assert_eq!(site.confidence, 0.85);
    }
    for name in [
        "snippets/missing.liquid",
        "snippets/tied.liquid",
        "common/tied.h",
    ] {
        let sites = facts
            .references()
            .iter()
            .filter(|site| site.reference_name == name)
            .collect::<Vec<_>>();
        assert_ne!(sites, [] as [&ReferenceInput; 0]);
        assert!(sites.iter().all(|site| site.target_symbol_id.is_none()));
    }
    let owner = capability_file_symbol(&facts, "csrc/main.c");
    let system = facts
        .references()
        .iter()
        .filter(|site| {
            site.file_id == owner.file_id
                && site.reference_name == "common/foo.h"
                && site.target_symbol_id.is_none()
        })
        .collect::<Vec<_>>();
    assert_eq!(system.len(), 1, "system includes must stay unresolved");
    assert_eq!(
        system[0].resolution_provenance,
        UNRESOLVED_IMPORT_PROVENANCE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_paths_preserve_directories_and_abstain_on_unrelated_basenames() {
    let built = generation(&[
        (
            "templates/main.liquid",
            "{% render 'exact' %}{% render 'renamed' %}{% render 'missing' %}",
        ),
        ("snippets/exact.liquid", "<div />"),
        ("elsewhere/renamed.liquid", "<div />"),
        ("src/main.c", "#include \"missing/foo.h\"\n"),
        ("other/foo.h", "int foo(void);"),
    ])
    .await;
    let facts = built.facts();
    let source = capability_file_symbol(facts, "templates/main.liquid");
    let site = facts
        .references()
        .iter()
        .find(|site| {
            site.file_id == source.file_id && site.reference_name == "snippets/exact.liquid"
        })
        .unwrap_or_else(|| panic!("missing exact path reference"));
    let target = capability_file_symbol(facts, "snippets/exact.liquid");
    assert_eq!(site.target_symbol_id.as_ref(), Some(&target.symbol_id));
    assert_eq!(site.resolution_provenance, "native-file-path-exact");
    assert_eq!(site.confidence, 0.95);
    for name in [
        "snippets/renamed.liquid",
        "snippets/missing.liquid",
        "missing/foo.h",
    ] {
        let site = facts
            .references()
            .iter()
            .find(|site| site.reference_name == name)
            .unwrap_or_else(|| panic!("missing path reference {name}"));
        assert!(site.target_symbol_id.is_none(), "{site:?}");
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_defined_paths_without_explicit_base_url_use_child_directory() {
    let built = generation(&[
        ("base.json", "{}"),
        ("app/tsconfig.json", r##"{"extends":"../base.json","compilerOptions":{"paths":{"#util":["lib/util.ts"],"#missing":["lib/missing.ts"]}}}"##),
        ("lib/util.ts", "export function util() {}"),
        ("lib/missing.ts", "export function missing() {}"),
        ("app/lib/util.ts", "export function util() {}"),
        ("app/main.ts", "import {util} from '#util'; import {missing} from '#missing'; export function run() { util(); missing(); }"),
    ]).await;
    let site = call(built.facts(), "app/main.ts", "util");
    assert_target(built.facts(), site, ("app/lib/util.ts", "util"));
    assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
    assert_eq!(site.confidence, IMPORT_BINDING_CONFIDENCE);
    let missing = call(built.facts(), "app/main.ts", "missing");
    assert!(missing.target_symbol_id.is_none());
    assert_eq!(missing.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_aliases_precede_packages_and_ambiguity_is_terminal() {
    let built = generation(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"@acme/core":["src/shim.ts"],"@acme/miss":["src/missing.ts"],"@acme/tied":["src/tied"]}}}"#),
        ("packages/core/package.json", r#"{"name":"@acme/core","exports":"./index.ts"}"#),
        ("packages/miss/package.json", r#"{"name":"@acme/miss","exports":"./index.ts"}"#),
        ("packages/tied/package.json", r#"{"name":"@acme/tied","exports":"./index.ts"}"#),
        ("packages/core/index.ts", "export function core() {}"),
        ("packages/miss/index.ts", "export function core() {}"),
        ("packages/tied/index.ts", "export function core() {}"),
        ("src/shim.ts", "export function core() {}"),
        ("src/tied.vue", "<template><div /></template>"),
        ("src/tied.svelte", "<div />"),
        ("src/main.ts", "import {core} from '@acme/core'; import {core as missed} from '@acme/miss'; import {core as tied} from '@acme/tied'; export function run() { core(); missed(); tied(); }"),
    ]).await;
    for (name, target) in [
        ("core", "src/shim.ts"),
        ("missed", "packages/miss/index.ts"),
    ] {
        let site = call(built.facts(), "src/main.ts", name);
        assert_target(built.facts(), site, (target, "core"));
        assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(site.confidence, IMPORT_BINDING_CONFIDENCE);
    }
    let tied = call(built.facts(), "src/main.ts", "tied");
    assert!(tied.target_symbol_id.is_none());
    assert_eq!(tied.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extension_substitutions_preserve_requested_module_format() {
    let built = generation(&[
        ("src/tool.ts", "export function util() {}"),
        ("src/tool.mts", "export function util() {}"),
        ("src/common.ts", "export function util() {}"),
        ("src/common.cts", "export function util() {}"),
        ("src/decl.ts", "export function util() {}"),
        ("src/decl.d.mts", "export declare function util(): void;"),
        ("src/runtime.ts", "export function util() {}"),
        ("src/runtime.js", "export function util() {}"),
        ("src/wrong.ts", "export function util() {}"),
        ("src/unsupported.tsx", "export function util() {}"),
        ("src/main.ts", "import {util} from './tool.mjs'; import {util as common} from './common.cjs'; import {util as declared} from './decl.mjs'; import {util as runtime} from './runtime.js'; import {util as wrong} from './wrong.mjs'; import {util as unsupported} from './unsupported.vue'; export function run() { util(); common(); declared(); runtime(); wrong(); unsupported(); }"),
    ]).await;
    for (name, target) in [
        ("util", "src/tool.mts"),
        ("common", "src/common.cts"),
        ("declared", "src/decl.d.mts"),
        ("runtime", "src/runtime.ts"),
    ] {
        let site = call(built.facts(), "src/main.ts", name);
        assert_target(built.facts(), site, (target, "util"));
        assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(site.confidence, IMPORT_BINDING_CONFIDENCE);
    }
    for name in ["wrong", "unsupported"] {
        let site = call(built.facts(), "src/main.ts", name);
        assert!(site.target_symbol_id.is_none());
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conditional_exports_preserve_order_and_abstain_on_unknown_conditions() {
    let built = generation(&[
        ("packages/pkg/package.json", r#"{"name":"pkg","exports":{"default":"./first.ts","import":"./second.ts"}}"#),
        ("packages/pkg/first.ts", "export function core() {}"),
        ("packages/pkg/second.ts", "export function core() {}"),
        ("packages/typed/package.json", r#"{"name":"typed","exports":{"types":"./second.ts","default":"./first.ts"}}"#),
        ("packages/typed/first.ts", "export function core() {}"),
        ("packages/typed/second.ts", "export function core() {}"),
        ("packages/unknown/package.json", r#"{"name":"unknown","exports":{"custom":"./first.ts","default":"./second.ts"}}"#),
        ("packages/unknown/first.ts", "export function core() {}"),
        ("packages/unknown/second.ts", "export function core() {}"),
        ("packages/emit/package.json", r#"{"name":"emit","exports":{"import":"./first.ts","default":"./second.ts"}}"#),
        ("packages/emit/first.ts", "export function core() {}"),
        ("packages/emit/second.ts", "export function core() {}"),
        ("packages/array/package.json", r#"{"name":"array","exports":[{"custom":"./first.ts"},"./second.ts"]}"#),
        ("packages/array/first.ts", "export function core() {}"),
        ("packages/array/second.ts", "export function core() {}"),
        ("packages/nullentry/package.json", r#"{"name":"nullentry","exports":[null,"./second.ts"]}"#),
        ("packages/nullentry/second.ts", "export function core() {}"),
        ("packages/segments/package.json", r#"{"name":"segments","exports":"./src/./index.ts"}"#),
        ("packages/segments/src/index.ts", "export function core() {}"),
        ("packages/directory/package.json", r#"{"name":"directory","exports":"./src"}"#),
        ("packages/directory/src/index.ts", "export function core() {}"),
        ("packages/extensionless/package.json", r#"{"name":"extensionless","exports":"./leaf"}"#),
        ("packages/extensionless/leaf.ts", "export function core() {}"),
        ("packages/emptysegment/package.json", r#"{"name":"emptysegment","exports":"./src//index.ts"}"#),
        ("packages/emptysegment/src/index.ts", "export function core() {}"),
        ("packages/encoded/package.json", r#"{"name":"encoded","exports":"./src/%2e/index.ts"}"#),
        ("packages/encoded/src/%2e/index.ts", "export function core() {}"),
        ("packages/fixed/package.json", r#"{"name":"fixed","exports":{"./*":"./fixed.ts"}}"#),
        ("packages/fixed/fixed.ts", "export function core() {}"),
        ("src/main.ts", "import {core} from 'pkg'; import {core as typed} from 'typed'; import {core as unknown} from 'unknown'; import {core as emit} from 'emit'; import {core as array} from 'array'; import {core as nullentry} from 'nullentry'; import {core as segments} from 'segments'; import {core as directory} from 'directory'; import {core as extensionless} from 'extensionless'; import {core as emptysegment} from 'emptysegment'; import {core as encoded} from 'encoded'; import {core as fixed} from 'fixed/valid'; import {core as invalidcapture} from 'fixed/../escape'; export function run() { core(); typed(); unknown(); emit(); array(); nullentry(); segments(); directory(); extensionless(); emptysegment(); encoded(); fixed(); invalidcapture(); }"),
    ]).await;
    for (name, target) in [
        ("core", "packages/pkg/first.ts"),
        ("typed", "packages/typed/second.ts"),
        ("fixed", "packages/fixed/fixed.ts"),
    ] {
        let site = call(built.facts(), "src/main.ts", name);
        assert_target(built.facts(), site, (target, "core"));
        assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
        assert_eq!(site.confidence, IMPORT_BINDING_CONFIDENCE);
    }
    for name in [
        "unknown",
        "emit",
        "array",
        "nullentry",
        "segments",
        "directory",
        "extensionless",
        "emptysegment",
        "encoded",
        "invalidcapture",
    ] {
        let site = call(built.facts(), "src/main.ts", name);
        assert!(site.target_symbol_id.is_none());
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
}

const PACKAGE_ENTRY_CASES: &[(&str, &str, Option<&str>)] = &[
    (
        "pkg",
        r#"{"name":"pkg","main":"./actual.js"}"#,
        Some("actual.js"),
    ),
    (
        "typed",
        r#"{"name":"typed","types":"./types.ts","main":"./actual.js"}"#,
        Some("types.ts"),
    ),
    (
        "typings",
        r#"{"name":"typings","typings":"./types.ts","main":"actual.js"}"#,
        Some("types.ts"),
    ),
    (
        "exported",
        r#"{"name":"exported","exports":"./exported.ts","types":"./types.ts","main":"./actual.js"}"#,
        Some("exported.ts"),
    ),
    (
        "missing",
        r#"{"name":"missing","main":"./absent.js"}"#,
        None,
    ),
    (
        "missingtypes",
        r#"{"name":"missingtypes","types":"./absent.d.ts","main":"./actual.js"}"#,
        None,
    ),
    (
        "versioned",
        r#"{"name":"versioned","main":"./actual.js","typesVersions":{"*":{"*":["other/*"]}}}"#,
        None,
    ),
    ("invalid", r#"{"name":"invalid","main":false}"#, None),
    ("rooted", r#"{"name":"rooted","main":"/actual.js"}"#, None),
    (
        "rootedtypes",
        r#"{"name":"rootedtypes","types":"/types.ts"}"#,
        None,
    ),
    (
        "missingexports",
        r#"{"name":"missingexports","exports":"./absent.ts","main":"./actual.js"}"#,
        None,
    ),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_entries_honor_main_types_and_exports_without_index_fallback() {
    let mut sources = Vec::new();
    let mut imports = String::new();
    let mut calls = String::new();
    for &(name, manifest, _) in PACKAGE_ENTRY_CASES {
        sources.push((format!("packages/{name}/package.json"), manifest.to_owned()));
        for file in ["actual.js", "index.js", "types.ts", "exported.ts"] {
            sources.push((
                format!("packages/{name}/{file}"),
                "export function util() {}".to_owned(),
            ));
        }
        write!(imports, "import {{util as {name}}} from '{name}';")
            .unwrap_or_else(|error| panic!("fixture import: {error}"));
        write!(calls, "{name}();").unwrap_or_else(|error| panic!("fixture call: {error}"));
    }
    sources.push((
        "src/main.ts".to_owned(),
        format!("{imports} export function run() {{ {calls} }}"),
    ));
    let fixtures = sources
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let built = generation(&fixtures).await;
    for &(name, _, target) in PACKAGE_ENTRY_CASES {
        let site = call(built.facts(), "src/main.ts", name);
        if let Some(target) = target {
            assert_target(
                built.facts(),
                site,
                (&format!("packages/{name}/{target}"), "util"),
            );
            assert_eq!(site.resolution_provenance, IMPORT_BINDING_PROVENANCE);
            assert_eq!(site.confidence, IMPORT_BINDING_CONFIDENCE);
        } else {
            assert!(site.target_symbol_id.is_none(), "{site:?}");
            assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_basename_include_misses_abstain_without_cross_directory_binding() {
    let mut sources = Vec::new();
    for number in 0..48 {
        sources.push((format!("d_{number}/foo.h"), "int foo(void);".to_owned()));
        sources.push((
            format!("src/use_{number}.c"),
            format!("#include \"missing_{number}/foo.h\"\n"),
        ));
    }
    let fixtures = sources
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect::<Vec<_>>();
    let built = generation(&fixtures).await;
    let sites = built
        .facts()
        .references()
        .iter()
        .filter(|site| site.reference_kind == ReferenceKind::Imports.as_str())
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 48);
    for site in sites {
        assert!(site.target_symbol_id.is_none(), "{site:?}");
        assert_eq!(site.resolution_provenance, UNRESOLVED_IMPORT_PROVENANCE);
    }
}
