//! Wave 3 framework targets and provenance through canonical native generation.
use super::*;

#[test]
fn express_route_handlers_preserve_lexical_evidence_or_abstain() {
    let facts = generation(&[
        (
            "src/routes.js",
            "function handler() {} function install(handler) { router.get('/x', handler); }",
        ),
        (
            "src/nested.js",
            "function install() { function handler() {} router.get('/nested', handler); }",
        ),
    ]);
    let route = capability_symbol(&facts, "src/routes.js", "src/routes.js::get::/x");
    assert!(!facts.references().iter().any(|site| {
        site.owner_symbol_id.as_ref() == Some(&route.symbol_id)
            && site.reference_name == "handler"
            && site.target_symbol_id.is_some()
    }));
    let nested = capability_symbol(&facts, "src/nested.js", "src/nested.js::get::/nested");
    targets(
        CapabilityReferenceQuery::new(&facts, nested).named("handler", ReferenceKind::Calls),
        capability_symbol(&facts, "src/nested.js", "install::handler"),
        EXACT_SAME_FILE_PROVENANCE,
    );
}

#[test]
fn angular_components_preserve_lexical_evidence_or_abstain() {
    let facts = generation(&[
        (
            "src/routes.ts",
            "import { Routes } from '@angular/router'; class Home {} function makeRoutes() { class Home {} const routes: Routes = [{path: 'x', component: Home}]; return routes; }",
        ),
        (
            "src/nested.ts",
            "function makeRoutes() { class Home {} const routes: Routes = [{path: 'nested', component: Home}]; return routes; }",
        ),
    ]);
    let route = capability_symbol(&facts, "src/routes.ts", "src/routes.ts::angular::/x");
    let outer = capability_symbol(&facts, "src/routes.ts", "Home");
    assert!(!facts.references().iter().any(|site| {
        site.owner_symbol_id.as_ref() == Some(&route.symbol_id)
            && site.reference_name == "Home"
            && site.target_symbol_id.as_ref() == Some(&outer.symbol_id)
    }));
    let nested = capability_symbol(&facts, "src/nested.ts", "src/nested.ts::angular::/nested");
    targets(
        CapabilityReferenceQuery::new(&facts, nested).named("Home", ReferenceKind::References),
        capability_symbol(&facts, "src/nested.ts", "makeRoutes::Home"),
        EXACT_SAME_FILE_PROVENANCE,
    );
}

#[test]
fn middleware_landmarks_do_not_shadow_recursive_functions() {
    let facts = generation(&[(
        "middleware/auth.ts",
        "export function auth(n: number) { if (n > 0) auth(n - 1); }",
    )]);
    let auth = capability_symbol(&facts, "middleware/auth.ts", "auth");
    targets(
        CapabilityReferenceQuery::new(&facts, auth).named("auth", ReferenceKind::Calls),
        auth,
        EXACT_SAME_FILE_PROVENANCE,
    );
}

#[test]
fn react_context_members_require_the_receivers_visible_binding() {
    let facts = generation(&[
        (
            "src/global.d.ts",
            "declare const SettingsContext: import('react').Context<unknown>;",
        ),
        ("src/unrelated.ts", "export const SettingsContext = {};"),
        (
            "src/App.tsx",
            "export function App() { return <SettingsContext.Provider value={null}/>; }",
        ),
        (
            "src/Bound.tsx",
            "import {SettingsContext} from './unrelated'; export function Bound() { return <SettingsContext.Consumer/>; }",
        ),
        (
            "src/Catch.tsx",
            "const SettingsContext = {}; function App() { try {} catch(SettingsContext) { return <SettingsContext.Provider/>; } }",
        ),
        (
            "src/With.jsx",
            "const SettingsContext = {}; function App() { with (values) { return <SettingsContext.Consumer/>; } }",
        ),
    ]);
    assert_eq!(
        reference(
            &facts,
            (
                "src/App.tsx",
                "SettingsContext.Provider",
                ReferenceKind::References
            )
        )
        .target_symbol_id,
        None,
    );
    for (path, name) in [
        ("src/Catch.tsx", "SettingsContext.Provider"),
        ("src/With.jsx", "SettingsContext.Consumer"),
    ] {
        assert_eq!(
            reference(&facts, (path, name, ReferenceKind::References)).target_symbol_id,
            None,
        );
    }
    targets(
        reference(
            &facts,
            (
                "src/Bound.tsx",
                "SettingsContext.Consumer",
                ReferenceKind::References,
            ),
        ),
        capability_symbol(&facts, "src/unrelated.ts", "SettingsContext"),
        IMPORT_BINDING_PROVENANCE,
    );
}

#[test]
fn react_context_members_exclude_non_jsx_reference_names() {
    for source in [
        "const SettingsContext = {}; const value = process.env['SettingsContext.Provider'];",
        "const SettingsContext = {}; const value = requireNativeModule('SettingsContext.Provider');",
        "const SettingsContext = {}; import {'SettingsContext.Provider' as value} from './missing';",
        "const SettingsContext = {}; const {'SettingsContext.Provider': value} = require('./missing');",
    ] {
        let facts = generation(&[("src/App.tsx", source)]);
        let site = reference(
            &facts,
            (
                "src/App.tsx",
                "SettingsContext.Provider",
                ReferenceKind::References,
            ),
        );
        assert_ne!(site.resolution_provenance, "native-react-context-member");
        assert_ne!(
            site.target_symbol_id.as_ref(),
            Some(&capability_symbol(&facts, "src/App.tsx", "SettingsContext").symbol_id),
        );
    }
}

#[test]
fn angular_lazy_named_exports_exclude_default_and_type_only_declarations() {
    let facts = generation(&[
        ("src/Panel.ts", "export default class Panel {}"),
        (
            "src/Types.ts",
            "export interface Shape {} export type Alias = number;",
        ),
        ("src/Ambient.ts", "export declare class Ambient {}"),
        ("src/TypeList.ts", "class Listed {} export type {Listed};"),
        (
            "src/TypeSpecifier.ts",
            "class Specifier {} export {type Specifier};",
        ),
        (
            "src/routes.ts",
            "const routes: Routes = [{path: 'x', loadComponent: () => import('./Panel').then(m => m.Panel)}, {path: 'type', loadComponent: () => import('./Types').then(m => m.Shape)}, {path: 'alias', loadComponent: () => import('./Types').then(m => m.Alias)}, {path: 'ambient', loadComponent: () => import('./Ambient').then(m => m.Ambient)}, {path: 'list', loadComponent: () => import('./TypeList').then(m => m.Listed)}, {path: 'specifier', loadComponent: () => import('./TypeSpecifier').then(m => m.Specifier)}];",
        ),
    ]);
    for (path, name) in [
        ("/x", "Panel"),
        ("/type", "Shape"),
        ("/alias", "Alias"),
        ("/ambient", "Ambient"),
        ("/list", "Listed"),
        ("/specifier", "Specifier"),
    ] {
        let route = capability_symbol(
            &facts,
            "src/routes.ts",
            &format!("src/routes.ts::angular::{path}"),
        );
        let site =
            CapabilityReferenceQuery::new(&facts, route).named(name, ReferenceKind::References);
        assert_eq!(site.target_symbol_id, None, "{path}: {site:?}");
        assert_ne!(site.resolution_provenance, "native-angular-lazy-export");
    }
}

#[test]
fn express_hono_and_bun_routes_keep_exact_handler_targets_and_missing_handlers_abstain() {
    let facts = generation(&[
        (
            "src/express.js",
            "const router = require('../router'); function handler() {} router.get('/users', handler); router.get('/missing', absentHandler);",
        ),
        (
            "src/hono.ts",
            "import {Hono} from 'hono'; const app = new Hono(); const child = wrap(new Hono()); function handler() {} child.get('/child/', handler); app.route('/api/', child);",
        ),
        (
            "src/bun.ts",
            "function handler() {} Bun.serve({routes:{'/q':{'GET':handler}}});",
        ),
    ]);
    for (path, qualified) in [
        ("src/express.js", "src/express.js::get::/users"),
        ("src/hono.ts", "src/hono.ts::get::/child/"),
        ("src/hono.ts", "src/hono.ts::get::/api/child/"),
        ("src/bun.ts", "src/bun.ts::bun::get::/q"),
    ] {
        let route = capability_symbol(&facts, path, qualified);
        let site =
            CapabilityReferenceQuery::new(&facts, route).named("handler", ReferenceKind::Calls);
        targets(
            site,
            capability_symbol(&facts, path, "handler"),
            EXACT_SAME_FILE_PROVENANCE,
        );
    }
    let missing = capability_symbol(&facts, "src/express.js", "src/express.js::get::/missing");
    let site =
        CapabilityReferenceQuery::new(&facts, missing).named("absentHandler", ReferenceKind::Calls);
    assert_eq!(site.target_symbol_id, None);
    assert_eq!(site.resolution_provenance, UNRESOLVED_PROVENANCE);
}

#[test]
fn default_component_module_imports_target_components_and_named_imports_keep_modules() {
    let facts = generation(&[
        (
            "src/lib/Counter.svelte",
            "<script>export let count = 0;</script><p>{count}</p>",
        ),
        (
            "src/Panel/index.vue",
            "<script setup>const value = 0;</script><p>{{value}}</p>",
        ),
        (
            "src/use.ts",
            "import Counter from '$lib/Counter.svelte';\nimport Panel from '~/src/Panel';\nimport * as namespace from '$lib/Counter.svelte';\nimport Missing from './Missing.svelte';\nexport function run() { Panel(); Counter(); }\n",
        ),
        (
            "src/View.vue",
            "<script setup>import Panel from '~/src/Panel';</script><Panel/>",
        ),
        (
            "src/Namespace.vue",
            "<script setup>import * as namespace from '~/src/Panel';</script><p/>",
        ),
        (
            "src/Mixed.vue",
            "<script setup>import Panel, {usePanel} from '~/src/Panel';</script><Panel/>",
        ),
    ]);
    targets(
        reference(
            &facts,
            ("src/View.vue", "~/src/Panel", ReferenceKind::Imports),
        ),
        capability_symbol(&facts, "src/Panel/index.vue", "index"),
        "native-conventional-alias",
    );
    for (name, target_file, qualified) in [
        ("Panel", "src/Panel/index.vue", "index"),
        ("Counter", "src/lib/Counter.svelte", "Counter"),
    ] {
        targets(
            reference(&facts, ("src/use.ts", name, ReferenceKind::Calls)),
            capability_symbol(&facts, target_file, qualified),
            "native-conventional-alias",
        );
    }
    for path in ["src/Namespace.vue", "src/Mixed.vue"] {
        targets(
            reference(&facts, (path, "~/src/Panel", ReferenceKind::Imports)),
            capability_symbol(&facts, "src/Panel/index.vue", "src/Panel/index.vue"),
            "native-conventional-alias",
        );
    }

    assert!(
        reference(
            &facts,
            ("src/use.ts", "./Missing.svelte", ReferenceKind::Imports)
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn framework_provided_names_are_targetless_and_local_code_stays_authoritative() {
    let facts = generation(&[
        (
            "src/use.ts",
            "import {navigateTo} from '#app';\nimport {goto} from '$app/navigation';\nexport function run() { useFetch(); definePageMeta(); $state.raw(0); }\n",
        ),
        (
            "src/View.vue",
            "<script setup>useRuntimeConfig(); navigateTo('/');</script><p/>",
        ),
        (
            "src/local.ts",
            "function useFetch() {}\nexport function run() { useFetch(); }\n",
        ),
    ]);
    for (path, name, kind) in [
        ("src/use.ts", "#app", ReferenceKind::Imports),
        ("src/use.ts", "$app/navigation", ReferenceKind::Imports),
        ("src/use.ts", "definePageMeta", ReferenceKind::Calls),
        ("src/use.ts", "$state.raw", ReferenceKind::Calls),
        ("src/View.vue", "useRuntimeConfig", ReferenceKind::Calls),
    ] {
        let site = reference(&facts, (path, name, kind));
        assert_eq!(site.target_symbol_id, None);
        assert_eq!(site.resolution_provenance, framework_provided::PROVENANCE);
    }
    targets(
        reference(&facts, ("src/local.ts", "useFetch", ReferenceKind::Calls)),
        capability_symbol(&facts, "src/local.ts", "useFetch"),
        EXACT_SAME_FILE_PROVENANCE,
    );
    let svelte = generation(&[(
        "src/View.svelte",
        "<script>let x = $state.raw(0); let y = $derived.by(() => x); $effect.pre(() => y);</script><p>{x}</p>",
    )]);
    assert!(!svelte.references().iter().any(|site| {
        site.reference_kind == "calls"
            && [
                "raw",
                "by",
                "pre",
                "$state.raw",
                "$derived.by",
                "$effect.pre",
            ]
            .contains(&site.reference_name.as_str())
    }));
}

#[test]
fn framework_provided_classification_preserves_explicit_import_boundaries() {
    let facts = generation(&[
        ("src/bare.ts", "export function run() { useFetch(); }"),
        (
            "src/external.ts",
            "import { useFetch } from 'external-sdk'; export function run() { useFetch(); }",
        ),
        (
            "src/missing.ts",
            "import { useFetch } from './absent'; export function run() { useFetch(); }",
        ),
        (
            "src/virtual.ts",
            "import { navigateTo } from '#app'; export function run() { navigateTo('/'); }",
        ),
    ]);
    for (path, name, provenance) in [
        ("src/bare.ts", "useFetch", framework_provided::PROVENANCE),
        (
            "src/external.ts",
            "useFetch",
            EXTERNAL_REFERENCE_UNRESOLVED_PROVENANCE,
        ),
        ("src/missing.ts", "useFetch", UNRESOLVED_IMPORT_PROVENANCE),
        (
            "src/virtual.ts",
            "navigateTo",
            framework_provided::PROVENANCE,
        ),
    ] {
        let site = reference(&facts, (path, name, ReferenceKind::Calls));
        assert_eq!(site.target_symbol_id, None);
        assert_eq!(site.resolution_provenance, provenance);
    }
}

#[test]
fn react_context_member_targets_the_context_and_abstains_for_unknown_members() {
    let facts = generation(&[(
        "src/App.jsx",
        "import React from 'react';\nexport const SettingsContext = React.createContext({});\nexport function App() { return <SettingsContext.Provider/>; }\nexport function Other() { return <SettingsContext.Unknown/>; }\n",
    )]);
    let context = capability_symbol(&facts, "src/App.jsx", "SettingsContext");
    let site = reference(
        &facts,
        (
            "src/App.jsx",
            "SettingsContext.Provider",
            ReferenceKind::References,
        ),
    );
    targets(site, context, "native-react-context-member");
    assert!((site.confidence - FRAMEWORK_CONVENTION_CONFIDENCE).abs() < f32::EPSILON);
    assert!(
        reference(
            &facts,
            (
                "src/App.jsx",
                "SettingsContext.Unknown",
                ReferenceKind::References
            )
        )
        .target_symbol_id
        .is_none()
    );
    let shadowed = generation(&[(
        "src/Shadow.jsx",
        "export const SettingsContext = {};\nexport function Shadow(SettingsContext) { return <SettingsContext.Provider/>; }",
    )]);
    assert!(
        reference(
            &shadowed,
            (
                "src/Shadow.jsx",
                "SettingsContext.Provider",
                ReferenceKind::References
            )
        )
        .target_symbol_id
        .is_none()
    );
}

#[test]
fn angular_lazy_imports_keep_the_route_owner_and_real_module_target() {
    let facts = generation(&[
        (
            "src/routes.ts",
            "import { Routes } from '@angular/router';\nexport class Home {}\nconst routes: Routes = [{path: '', component: Home}, {path: 'lazy', loadComponent: () => import('./Panel').then(m => m.Panel)}, {path: 'missing', loadComponent: () => import('./absent').then(m => m.Panel)}, {path: 'rejection', loadComponent: () => import('./Panel').then(undefined, e => e.Panel)}];",
        ),
        ("src/Panel.ts", "export class Panel {}"),
        ("src/Other.ts", "export class Panel {}"),
    ]);
    let route = capability_symbol(&facts, "src/routes.ts", "src/routes.ts::angular::/");
    let home = capability_symbol(&facts, "src/routes.ts", "Home");
    let component =
        CapabilityReferenceQuery::new(&facts, route).named("Home", ReferenceKind::References);
    targets(component, home, EXACT_SAME_FILE_PROVENANCE);
    let lazy = capability_symbol(&facts, "src/routes.ts", "src/routes.ts::angular::/lazy");
    let site = CapabilityReferenceQuery::new(&facts, lazy).named("./Panel", ReferenceKind::Imports);
    targets(
        site,
        capability_symbol(&facts, "src/Panel.ts", "src/Panel.ts"),
        "native-angular-lazy-module",
    );
    let export =
        CapabilityReferenceQuery::new(&facts, lazy).named("Panel", ReferenceKind::References);
    // Export flags alone cannot prove a named runtime export. Module
    // resolution remains exact; the new lazy-export fallback abstains.
    assert_eq!(export.target_symbol_id, None);
    assert_ne!(export.resolution_provenance, "native-angular-lazy-export");
    let missing = capability_symbol(&facts, "src/routes.ts", "src/routes.ts::angular::/missing");
    for site in facts
        .references()
        .iter()
        .filter(|reference| reference.owner_symbol_id.as_ref() == Some(&missing.symbol_id))
    {
        assert!(site.target_symbol_id.is_none(), "{site:?}");
    }
    let rejection = capability_symbol(
        &facts,
        "src/routes.ts",
        "src/routes.ts::angular::/rejection",
    );
    assert!(!facts.references().iter().any(|site| {
        site.owner_symbol_id.as_ref() == Some(&rejection.symbol_id)
            && site.reference_name == "Panel"
    }));
    assert!(
        !facts
            .references()
            .iter()
            .any(
                |reference| reference.owner_symbol_id.as_ref() == Some(&route.symbol_id)
                    && reference.reference_name == "Panel"
            )
    );
}

#[test]
fn commonjs_callable_imports_target_the_real_module_and_shadowed_loaders_abstain() {
    let facts = generation(&[
        (
            "src/models.js",
            "function load() { const cfg = require('./cfg'); }",
        ),
        ("src/cfg.js", "module.exports = {};"),
        (
            "src/shadow.js",
            "function shadow(require) { const cfg = require('./cfg'); }",
        ),
    ]);
    let load = capability_symbol(&facts, "src/models.js", "load");
    let site = CapabilityReferenceQuery::new(&facts, load).named("./cfg", ReferenceKind::Imports);
    targets(
        site,
        capability_symbol(&facts, "src/cfg.js", "src/cfg.js"),
        "native-commonjs-callable-import",
    );
    let shadow = capability_symbol(&facts, "src/shadow.js", "shadow");
    assert!(
        !facts
            .references()
            .iter()
            .any(
                |reference| reference.owner_symbol_id.as_ref() == Some(&shadow.symbol_id)
                    && reference.reference_kind == "imports"
            )
    );
}
