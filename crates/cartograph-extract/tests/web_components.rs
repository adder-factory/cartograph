//! Astro, Vue, and Svelte component extraction contracts.
//!
//! Script blocks, Astro frontmatter, and template expressions run through the
//! native JavaScript/TypeScript walker as embedded regions of the component
//! file. The scenarios port v1's `extraction.test.ts` (Vue/Svelte),
//! `borrowed-language-support.test.ts` (Astro), and `multi-language-typeof`
//! fixtures; every extraction is also checked for determinism and for exact
//! host-file spans.

mod credential_support;
mod dependency_ownership;

use cartograph_domain::{FileParseStatus, ReferenceKind, SymbolKind};
use cartograph_extract::{
    DiagnosticCode, ExtractError, ExtractedFile, ExtractedReference, ExtractedSymbol,
    ImportBindingKind, NativeExtractor, SourceLimits, SourceSnapshot,
};

const SOURCE_LIMIT: usize = 1024 * 1024;
const SECRET_SENTINEL: &str = "cartograph_literal_secret_sentinel_7c1f";

#[test]
fn separate_script_trees_keep_constant_and_value_parameter_shadowing() {
    let source = "<script context=\"module\">\nconst MAX_RETRY = 3;\nfunction handler() {}\nfunction seed() { configure(handler); return MAX_RETRY; }\n</script>\n<script>\nfunction run(MAX_RETRY, handler) { configure(handler); return MAX_RETRY; }\nfunction free() { return MAX_RETRY; }\n</script>\n";
    for path in ["src/Scopes.svelte", "src/Scopes.vue"] {
        let file = extract(path, source);
        let run = symbol(&file, SymbolKind::Function, "run");
        assert!(
            file.references.iter().all(|reference| {
                reference.owner.as_ref() != Some(&run.id)
                    || reference.kind != ReferenceKind::References
                    || !matches!(reference.name.as_str(), "MAX_RETRY" | "handler")
            }),
            "a second script's parameters must shadow module values: {:?}",
            file.references
        );
        for name in ["seed", "free"] {
            let owner = symbol(&file, SymbolKind::Function, name);
            assert!(
                file.references.iter().any(|reference| {
                    reference.owner.as_ref() == Some(&owner.id)
                        && reference.kind == ReferenceKind::References
                        && reference.name == "MAX_RETRY"
                }),
                "a module read in {name} must remain"
            );
        }
        let seed = symbol(&file, SymbolKind::Function, "seed");
        assert!(
            file.references.iter().any(|reference| {
                reference.owner.as_ref() == Some(&seed.id)
                    && reference.kind == ReferenceKind::References
                    && reference.name == "handler"
            }),
            "enrichment must revisit the first script's own scope"
        );
    }
}

#[test]
fn separate_template_trees_keep_constant_parameter_shadowing() {
    let script = "<script>const MAX_RETRY = 3; function seed() { return MAX_RETRY; }</script>\n";
    for (path, template) in [
        (
            "src/Scopes.svelte",
            "<p>{((MAX_RETRY) => MAX_RETRY)(1)}</p>\n<p>{MAX_RETRY}</p>\n<p>{((MAX_RETRY) => MAX_RETRY)(2)}</p>\n",
        ),
        (
            "src/Scopes.vue",
            "<p>{{ ((MAX_RETRY) => MAX_RETRY)(1) }}</p>\n<p>{{ MAX_RETRY }}</p>\n<p>{{ ((MAX_RETRY) => MAX_RETRY)(2) }}</p>\n",
        ),
        (
            "src/Scopes.astro",
            "<p>{((MAX_RETRY) => MAX_RETRY)(1)}</p>\n<p>{MAX_RETRY}</p>\n<p>{((MAX_RETRY) => MAX_RETRY)(2)}</p>\n",
        ),
    ] {
        let source = if path.ends_with("astro") {
            format!(
                "---\nconst MAX_RETRY = 3; function seed() {{ return MAX_RETRY; }}\n---\n{template}"
            )
        } else {
            format!("{script}{template}")
        };
        let file = extract(path, &source);
        let lines = file
            .references
            .iter()
            .filter(|reference| {
                reference.kind == ReferenceKind::References && reference.name == "MAX_RETRY"
            })
            .map(|reference| reference.span.start_line())
            .collect::<Vec<_>>();
        let expected = if path.ends_with("astro") {
            vec![2, 5]
        } else {
            vec![1, 3]
        };
        assert_eq!(
            lines, expected,
            "only unshadowed script and template reads survive in {path}"
        );
    }
}

#[test]
fn astro_component_frontmatter_and_template_match_the_v1_index_scenario() {
    let source = "---
import Layout from './Layout.astro';
const title = 'Home';
function greet(name: string) { return name; }
---
<Layout title={title}>
  <h1>{greet(title)}</h1>
</Layout>
";
    let file = extract("src/pages/index.astro", source);
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    assert_eq!(file.diagnostics, []);

    let component = symbol(&file, SymbolKind::Component, "index");
    assert_eq!(component.qualified_name, "index");
    assert!(component.export.exported && component.export.default_export);
    assert_eq!(
        (component.span.start_line(), component.span.end_line()),
        (1, 9),
        "the file component spans the whole file"
    );
    for (kind, name) in [
        (SymbolKind::Import, "./Layout.astro"),
        (SymbolKind::Constant, "title"),
        (SymbolKind::Function, "greet"),
    ] {
        let declared = symbol(&file, kind, name);
        assert!(
            contains(&file, &component.id, &declared.id),
            "{kind:?} {name} is not contained by the component"
        );
        assert_eq!(
            declared.qualified_name, name,
            "frontmatter declarations keep module-scope names"
        );
    }
    assert_eq!(
        symbol(&file, SymbolKind::Function, "greet")
            .span
            .start_line(),
        4
    );
    assert!(has_reference(
        &file,
        ReferenceKind::Imports,
        "./Layout.astro"
    ));
    assert_eq!(
        reference(&file, ReferenceKind::References, "Layout")
            .owner
            .as_ref(),
        Some(&component.id)
    );
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "greet")
            .owner
            .as_ref(),
        Some(&component.id)
    );
    assert!(
        !file
            .symbols
            .iter()
            .any(|declared| declared.name == "Layout"),
        "a template tag is a component use, never a declaration"
    );
}

#[test]
fn astro_card_frontmatter_expressions_and_component_uses() {
    let source = "---
import Layout from '../layouts/Layout.astro';
import { formatDate } from '../utils/date';
interface Props { title: string }
const { title } = Astro.props;
function shout(value: string) { return value.toUpperCase(); }
const posts = await getPosts();
---
<Layout title={title}>
  <h1>{shout(title)}</h1>
  <time>{formatDate(new Date())}</time>
  {posts.map((post) => <PostCard post={post} />)}
  <my-element />
</Layout>
<style>
  h1 { color: red; }
  .fake { content: \"{ghostCall()}\"; }
</style>
";
    let file = extract("src/components/Card.astro", source);
    let component = symbol(&file, SymbolKind::Component, "Card");
    let shout = symbol(&file, SymbolKind::Function, "shout");
    symbol(&file, SymbolKind::Interface, "Props");
    symbol(&file, SymbolKind::Constant, "title");
    let posts = symbol(&file, SymbolKind::Constant, "posts");
    for module in ["../layouts/Layout.astro", "../utils/date"] {
        symbol(&file, SymbolKind::Import, module);
        assert!(has_reference(&file, ReferenceKind::Imports, module));
    }
    assert!(file.import_bindings.iter().any(|binding| {
        binding.kind == ImportBindingKind::Default
            && binding.module_specifier == "../layouts/Layout.astro"
            && binding.local_name == "Layout"
    }));
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "value.toUpperCase")
            .owner
            .as_ref(),
        Some(&shout.id)
    );
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "getPosts")
            .owner
            .as_ref(),
        Some(&posts.id)
    );
    for (kind, name) in [
        (ReferenceKind::Calls, "shout"),
        (ReferenceKind::Calls, "formatDate"),
        (ReferenceKind::Calls, "posts.map"),
        (ReferenceKind::Instantiates, "Date"),
        (ReferenceKind::References, "Layout"),
        (ReferenceKind::References, "PostCard"),
    ] {
        assert_eq!(
            reference(&file, kind, name).owner.as_ref(),
            Some(&component.id),
            "{kind:?} {name} must be owned by the component"
        );
    }
    for name in ["Layout", "PostCard", "my-element"] {
        assert!(
            !file
                .symbols
                .iter()
                .any(|declared| declared.kind == SymbolKind::Component && declared.name == name),
            "{name} became a component declaration"
        );
    }
    assert!(
        !file
            .references
            .iter()
            .any(|used| used.name.contains("my-element") || used.name.contains("ghostCall")),
        "custom elements and style text are not references"
    );
}

#[test]
fn astro_client_scripts_stay_outside_the_component_module() {
    let source = r#"---
const label = 'x';
---
<Shell>{clientOnly()}</Shell>
<script type="application/ld+json">{"@context": "https://schema.org", "name": "x"}</script>
<script>
import { initNav } from '../lib/nav';
function clientOnly() { initNav(); }
</script>
"#;
    let file = extract("src/components/Shell.astro", source);
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    assert_eq!(file.diagnostics, []);
    symbol(&file, SymbolKind::Constant, "label");
    assert!(
        has_reference(&file, ReferenceKind::Calls, "clientOnly"),
        "the template call is recorded"
    );
    assert!(
        !file
            .symbols
            .iter()
            .any(|declared| declared.name == "clientOnly"),
        "a browser script declaration must not satisfy a server template call"
    );
    for client in ["../lib/nav", "initNav", "schema.org"] {
        assert!(
            !file
                .references
                .iter()
                .any(|used| used.name.contains(client)),
            "client script or data block content leaked: {client}"
        );
    }
}

#[test]
fn astro_expressions_with_void_elements_comments_and_attributes_keep_every_fact() {
    let source = "---
import Child from './Child.astro';
---
<ul class={classes()}>
  {items.map((item) => <li><img src={imageFor(item)}><!-- {ghost()} --><Child item={item} /></li>)}
  {visible ? <Badge /> : null}
</ul>
";
    let file = extract("src/components/List.astro", source);
    let component = symbol(&file, SymbolKind::Component, "List");
    for (kind, name) in [
        (ReferenceKind::Calls, "classes"),
        (ReferenceKind::Calls, "items.map"),
        (ReferenceKind::Calls, "imageFor"),
        (ReferenceKind::References, "Child"),
        (ReferenceKind::References, "Badge"),
    ] {
        assert_eq!(
            reference(&file, kind, name).owner.as_ref(),
            Some(&component.id),
            "{kind:?} {name}"
        );
    }
    assert_eq!(
        file.references
            .iter()
            .filter(|used| used.kind == ReferenceKind::References && used.name == "Child")
            .count(),
        1,
        "a component use inside an expression is recorded once"
    );
    assert!(!file.references.iter().any(|used| used.name == "ghost"));
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn astro_frontmatter_is_typescript_not_tsx() {
    let file = extract(
        "src/pages/cast.astro",
        "---
const user = <User>value;
function load(): void { fetchUser(user); }
---
<p>{load()}</p>
",
    );
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    assert_eq!(file.diagnostics, []);
    let load = symbol(&file, SymbolKind::Function, "load");
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "fetchUser")
            .owner
            .as_ref(),
        Some(&load.id)
    );
}

#[test]
fn embedded_syntax_errors_make_the_file_partial_with_host_spans() {
    let astro = extract(
        "src/pages/broken.astro",
        "---\nconst ok = 1;\nconst = ;\n---\n<p>{ok}</p>\n",
    );
    assert_partial_at_line(&astro, 3);
    symbol(&astro, SymbolKind::Constant, "ok");

    let vue = extract(
        "src/Broken.vue",
        "<template><p>{{ value }}</p></template>\n<script>\nfunction ok() {}\nconst = ;\n</script>\n",
    );
    assert_partial_at_line(&vue, 4);
    symbol(&vue, SymbolKind::Function, "ok");

    let template_only = extract(
        "src/Filters.vue",
        "<template><p>{{ value | currency }}</p><p>{{ ( }}</p></template>\n",
    );
    assert_eq!(
        template_only.parse_status,
        FileParseStatus::Parsed,
        "template expressions are not programs and never make a file partial"
    );
    assert_eq!(template_only.diagnostics, []);
}

#[test]
fn vue_component_node_matches_the_v1_app_scenario() {
    let file = extract(
        "App.vue",
        "<script setup lang=\"ts\">
import { ref } from 'vue'
const count = ref(0)
</script>

<template>
  <button>{{ count }}</button>
</template>
",
    );
    let component = symbol(&file, SymbolKind::Component, "App");
    assert!(component.export.exported && component.export.default_export);
    let count = symbol(&file, SymbolKind::Constant, "count");
    assert!(contains(&file, &component.id, &count.id));
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "ref").owner.as_ref(),
        Some(&count.id)
    );
}

#[test]
fn vue_script_declarations_keep_exact_host_lines() {
    let file = extract(
        "Counter.vue",
        "<template>
  <button>+</button>
</template>

<script setup lang=\"ts\">
function increment(n: number): number {
  return n + 1
}
</script>
",
    );
    let increment = symbol(&file, SymbolKind::Function, "increment");
    assert_eq!(
        (increment.span.start_line(), increment.span.end_line()),
        (6, 8),
        "the declaration spans its whole body in host lines"
    );
    assert!(contains(
        &file,
        &symbol(&file, SymbolKind::Component, "Counter").id,
        &increment.id
    ));
}

#[test]
fn vue_typescript_scripts_emit_type_references_and_javascript_scripts_do_not() {
    let typescript = extract(
        "TS.vue",
        "<script setup lang=\"ts\">
interface Result { ok: boolean }
function f(x: Result): Result { return x }
</script>
",
    );
    let javascript = extract(
        "JS.vue",
        "<script>\nfunction f(x) { return x }\n</script>\n",
    );
    let f = symbol(&typescript, SymbolKind::Function, "f");
    assert!(typescript.references.iter().any(|used| {
        used.kind == ReferenceKind::TypeOf
            && used.name == "Result"
            && used.owner.as_ref() == Some(&f.id)
    }));
    assert!(has_reference(&typescript, ReferenceKind::Returns, "Result"));
    assert!(
        !javascript
            .references
            .iter()
            .any(|used| matches!(used.kind, ReferenceKind::TypeOf | ReferenceKind::Returns))
    );
}

#[test]
fn vue_template_calls_component_tags_and_compiler_macros_follow_v1() {
    let mustache = extract(
        "Mustache.vue",
        "<template>
  <span>{{ format(date) }}</span>
</template>

<script setup>
const date = new Date()
function format(d) { return d.toString() }
</script>
",
    );
    assert!(has_reference(&mustache, ReferenceKind::Calls, "format"));

    let parent = extract(
        "Parent.vue",
        "<template>
  <div>
    <UserCard :user=\"u\" />
    <Modal v-if=\"open\" />
  </div>
</template>

<script setup>
import UserCard from './UserCard.vue'
import Modal from './Modal.vue'
const u = {}
const open = true
</script>
",
    );
    assert!(has_reference(
        &parent,
        ReferenceKind::References,
        "UserCard"
    ));
    assert!(has_reference(&parent, ReferenceKind::References, "Modal"));

    let macros = extract(
        "Macros.vue",
        "<script setup lang=\"ts\">
const props = defineProps<{ msg: string }>()
const emit = defineEmits(['change'])
const x = withDefaults(defineProps<{ n?: number }>(), { n: 0 })
</script>
",
    );
    for name in ["defineProps", "defineEmits", "withDefaults"] {
        assert!(
            !macros
                .references
                .iter()
                .any(|used| used.name.contains(name)),
            "compiler macro {name} leaked as a reference"
        );
    }
    symbol(&macros, SymbolKind::Constant, "props");

    let icon = extract(
        "IconClose.vue",
        "<template>\n  <svg viewBox=\"0 0 24 24\"><path d=\"M0 0L24 24\"/></svg>\n</template>\n",
    );
    symbol(&icon, SymbolKind::Component, "IconClose");
}

#[test]
fn vue_template_scanning_skips_script_style_and_directive_like_mustaches() {
    let styled = extract(
        "Styled.vue",
        "<script setup>
const literal = '{{ shouldNotCount() }}'
</script>

<template>
  <span>{{ visibleCall(user.name) }}</span>
</template>

<style>
.icon::after { content: \"{{ alsoIgnored() }}\"; }
</style>
",
    );
    assert!(has_reference(&styled, ReferenceKind::Calls, "visibleCall"));
    for ignored in ["shouldNotCount", "alsoIgnored"] {
        assert!(!styled.references.iter().any(|used| used.name == ignored));
    }

    let directive_like = extract(
        "DirectiveLike.vue",
        "<template>
  <span>{{ }}</span>
  <span>{{ #internalCall() }}</span>
  <span>{{ /closingCall() }}</span>
  <span>{{ realCall() }}</span>
</template>
",
    );
    assert!(has_reference(
        &directive_like,
        ReferenceKind::Calls,
        "realCall"
    ));
    for ignored in ["internalCall", "closingCall"] {
        assert!(
            !directive_like
                .references
                .iter()
                .any(|used| used.name == ignored)
        );
    }
}

#[test]
fn vue_offsets_script_refs_types_and_ownership_to_host_lines() {
    let file = extract(
        "Offset.vue",
        "<template>
  <Widget />
</template>

<script setup lang=\"ts\">
import { helper } from './helper'
interface User { name: string }
function render(user: User): string {
  return helper(user.name)
}
</script>
",
    );
    let render = symbol(&file, SymbolKind::Function, "render");
    assert_eq!(render.span.start_line(), 8);
    let helper = reference(&file, ReferenceKind::Calls, "helper");
    assert_eq!(helper.span.start_line(), 9);
    assert_eq!(helper.owner.as_ref(), Some(&render.id));
    let user_type = reference(&file, ReferenceKind::TypeOf, "User");
    assert_eq!(user_type.owner.as_ref(), Some(&render.id));
    assert_eq!(user_type.span.start_line(), 8);
    assert!(has_reference(&file, ReferenceKind::References, "Widget"));
}

#[test]
fn svelte_panel_scenario_extracts_typed_script_template_calls_and_tags() {
    let file = extract(
        "Panel.svelte",
        "<script lang=\"ts\">
  import Child from './Child.svelte';
  export let user: User;
  function formatName(name: string): string {
    return name.toUpperCase();
  }
</script>

<section>
  <Child />
  <p>{formatName(user.name)}</p>
</section>
",
    );
    symbol(&file, SymbolKind::Component, "Panel");
    symbol(&file, SymbolKind::Function, "formatName");
    let user = symbol(&file, SymbolKind::Variable, "user");
    assert!(user.export.exported);
    assert!(file.references.iter().any(|used| {
        used.kind == ReferenceKind::TypeOf
            && used.name == "User"
            && used.owner.as_ref() == Some(&user.id)
    }));
    assert!(has_reference(&file, ReferenceKind::Calls, "formatName"));
    assert!(has_reference(&file, ReferenceKind::References, "Child"));
}

#[test]
fn svelte_runes_and_block_syntax_are_not_calls() {
    let file = extract(
        "Runes.svelte",
        "<script>
  const count = $state(0);
  const doubled = $derived.by(() => count * 2);
  $effect(() => { track(count); });
</script>

{#if count > 0}
  <button>{buttonLabel(count)}</button>
{/if}
",
    );
    assert!(has_reference(&file, ReferenceKind::Calls, "buttonLabel"));
    assert!(
        has_reference(&file, ReferenceKind::Calls, "track"),
        "rune arguments remain ordinary code"
    );
    for ignored in ["$state", "$derived", "$effect", "by", "if"] {
        assert!(
            !file
                .references
                .iter()
                .any(|used| used.name == ignored || used.name.starts_with(&format!("{ignored}."))),
            "{ignored} leaked as a reference"
        );
    }
    symbol(&file, SymbolKind::Constant, "count");
    symbol(&file, SymbolKind::Constant, "doubled");
}

#[test]
fn vue_scripts_keep_class_members_arrow_functions_and_every_import_form() {
    let file = extract(
        "Rich.vue",
        "<script setup lang=\"ts\">
import type { Item } from './types'
import { a as b, c } from './m'
import * as ns from './n'
export class Cart {
  add(item: Item) { this.items.push(item); helper() }
  save() {}
}
const compute = (a: number): number => a * 2
const run2 = async () => svc.format()
</script>
",
    );
    let cart = symbol(&file, SymbolKind::Class, "Cart");
    assert!(cart.export.exported);
    for method in ["add", "save"] {
        let member = symbol(&file, SymbolKind::Method, method);
        assert!(contains(&file, &cart.id, &member.id));
        assert!(
            !has_reference(&file, ReferenceKind::Calls, method),
            "method {method} was mistaken for a call"
        );
    }
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "helper")
            .owner
            .as_ref(),
        Some(&symbol(&file, SymbolKind::Method, "add").id)
    );
    symbol(&file, SymbolKind::Function, "compute");
    let run2 = symbol(&file, SymbolKind::Function, "run2");
    assert!(run2.execution.async_symbol);
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "svc.format")
            .owner
            .as_ref(),
        Some(&run2.id)
    );
    assert!(!has_reference(&file, ReferenceKind::Calls, "async"));
    for (kind, imported, local) in [
        (ImportBindingKind::Named, "Item", "Item"),
        (ImportBindingKind::Named, "a", "b"),
        (ImportBindingKind::Named, "c", "c"),
        (ImportBindingKind::Namespace, "*", "ns"),
    ] {
        assert!(
            file.import_bindings.iter().any(|binding| {
                binding.kind == kind
                    && binding.imported_name == imported
                    && binding.local_name == local
            }),
            "missing {kind:?} import {imported} as {local}: {:?}",
            file.import_bindings
        );
    }
    for keyword in ["type", "as"] {
        assert!(!file.symbols.iter().any(|declared| declared.name == keyword));
    }
}

#[test]
fn svelte_script_declarations_follow_syntax_not_physical_lines() {
    let file = extract(
        "S.svelte",
        "<script>
  import { f as g, h } from './m';
  function after() { g(); }
  function
    split() {}
  const a = 1, b = 2;
  const run = () => helper();
  class C { m() { inner(); } }
  /* function phantom() {} */
</script>
<p>{api.fmt()}</p>
",
    );
    let after = symbol(&file, SymbolKind::Function, "after");
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "g").owner.as_ref(),
        Some(&after.id)
    );
    let split = symbol(&file, SymbolKind::Function, "split");
    assert_eq!((split.span.start_line(), split.span.end_line()), (4, 5));
    symbol(&file, SymbolKind::Constant, "a");
    symbol(&file, SymbolKind::Constant, "b");
    let run = symbol(&file, SymbolKind::Function, "run");
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "helper")
            .owner
            .as_ref(),
        Some(&run.id)
    );
    let method = symbol(&file, SymbolKind::Method, "m");
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "inner")
            .owner
            .as_ref(),
        Some(&method.id)
    );
    for phantom in ["split", "m", "phantom"] {
        assert!(!has_reference(&file, ReferenceKind::Calls, phantom));
    }
    assert!(
        !file
            .symbols
            .iter()
            .any(|declared| declared.name == "phantom")
    );
    assert!(
        file.import_bindings
            .iter()
            .any(|binding| { binding.imported_name == "f" && binding.local_name == "g" })
    );
    assert!(
        file.import_bindings
            .iter()
            .any(|binding| binding.local_name == "h")
    );
    assert!(
        has_reference(&file, ReferenceKind::Calls, "api.fmt"),
        "a template member call keeps its receiver"
    );
}

#[test]
fn template_member_calls_keep_receivers_and_ignore_literals() {
    let file = extract(
        "Receivers.vue",
        "<script setup>\nimport { svc } from './svc'\n</script>\n<template><p>{{ svc.format(order) }}</p><p>{{ 'phantom(1)' }}</p></template>\n",
    );
    assert!(has_reference(&file, ReferenceKind::Calls, "svc.format"));
    assert!(
        !file.references.iter().any(|used| used.name == "phantom"),
        "call-like text inside a template string literal is not a call"
    );
}

#[test]
fn zod_schemas_inside_script_setup_are_recognized() {
    let file = extract(
        "Form.vue",
        "<script setup lang=\"ts\">
import { z } from 'zod'
const UserSchema = z.object({ name: z.string() })
</script>
",
    );
    let schema = symbol(&file, SymbolKind::Struct, "UserSchema");
    let field = symbol(&file, SymbolKind::Field, "name");
    assert!(contains(&file, &schema.id, &field.id));
}

#[test]
fn script_comparisons_cannot_hide_the_closing_script_tag() {
    let file = extract(
        "Compare.svelte",
        "<script>\n  const ok = a < b;\n  function tail() {}\n</script>\n<Child />\n",
    );
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    assert_eq!(file.diagnostics, []);
    symbol(&file, SymbolKind::Function, "tail");
    assert!(has_reference(&file, ReferenceKind::References, "Child"));
    assert!(
        !file.references.iter().any(|used| used.name == "b"),
        "script text after `<` is not a tag"
    );
}

#[test]
fn svelte_store_subscriptions_and_framework_modules_are_preserved() {
    let file = extract(
        "src/routes/Store.svelte",
        "<script>
  import { page } from '$app/stores';
  /* $ghost */
  const label = '$fake';
  $count = 5;
</script>
<p>{$page.url} {$$props.x}</p>
",
    );
    let component = symbol(&file, SymbolKind::Component, "Store");
    for (name, store) in [("$count", "count"), ("$page", "page")] {
        let subscription = reference(&file, ReferenceKind::References, name);
        assert_eq!(subscription.resolution_name.as_deref(), Some(store));
        assert_eq!(subscription.owner.as_ref(), Some(&component.id));
    }
    for ignored in ["$ghost", "$fake", "$$props", "$props"] {
        assert!(
            !file.references.iter().any(|used| used.name == ignored),
            "{ignored} is not a store subscription"
        );
    }
    let module = symbol(&file, SymbolKind::Resource, "$app/stores");
    assert_eq!(
        module.qualified_name,
        "Store::framework-module::$app/stores"
    );
    assert!(contains(&file, &component.id, &module.id));
}

#[test]
fn component_literals_never_reach_extracted_facts() {
    for (path, source) in [
        (
            "src/Secret.vue",
            "<script setup lang=\"ts\">\nconst secret = 'cartograph_literal_secret_sentinel_7c1f'\n</script>\n<template>{{ 'cartograph_literal_secret_sentinel_7c1f' }}</template>\n",
        ),
        (
            "src/Secret.svelte",
            "<script>\nconst secret = 'cartograph_literal_secret_sentinel_7c1f';\n</script>\n<p>{'cartograph_literal_secret_sentinel_7c1f'}</p>\n",
        ),
        (
            "src/Secret.astro",
            "---\nconst secret = 'cartograph_literal_secret_sentinel_7c1f';\n---\n<p data-x={'cartograph_literal_secret_sentinel_7c1f'}>{secret}</p>\n",
        ),
    ] {
        let file = extract(path, source);
        symbol(&file, SymbolKind::Constant, "secret");
        assert!(
            !format!("{file:?}").contains(SECRET_SENTINEL),
            "{path} leaked a source literal"
        );
    }
}

#[test]
fn cancellation_inside_embedded_regions_is_reported_not_swallowed() {
    let component = "<script>\nimport { a } from './a';\nfunction one() { a(); }\n</script>\n<p>{one()}</p>{{ one() }}\n<Card />\n";
    for (path, source) in [
        (
            "src/Cancel.astro",
            "---\nimport { a } from './a';\nfunction one() { a(); }\n---\n<p>{one()}</p>\n<Card />\n",
        ),
        ("src/Cancel.vue", component),
        ("src/Cancel.svelte", component),
    ] {
        let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
            .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
        let mut extractor = NativeExtractor::new(snapshot.language())
            .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"));
        let mut polls = 0_usize;
        extractor
            .extract_with_cancellation(&snapshot, || {
                polls = polls.saturating_add(1);
                false
            })
            .unwrap_or_else(|error| panic!("{path} uncancelled extraction failed: {error}"));
        assert!(polls > 2, "{path} never polled cancellation");
        for cancel_at in 1..polls {
            let mut seen = 0_usize;
            let outcome = extractor.extract_with_cancellation(&snapshot, || {
                seen = seen.saturating_add(1);
                seen >= cancel_at
            });
            assert_eq!(
                outcome,
                Err(ExtractError::Cancelled),
                "{path} swallowed cancellation at poll {cancel_at}"
            );
        }
    }
}

#[test]
fn component_module_exports_follow_module_scope_not_the_component_owner() {
    let file = extract(
        "src/lib/Helpers.svelte",
        "<script context=\"module\">
  function helper() {}
  const LIMIT = 3;
  export { helper, LIMIT as limit };
  export * as helpers from './helpers.js';
</script>
<script>
  export let size = 1;
</script>
",
    );
    let helper = symbol(&file, SymbolKind::Function, "helper");
    assert!(
        helper.export.exported,
        "an export list exports the declaration"
    );
    assert_eq!(helper.qualified_name, "helper");
    let namespace = symbol(&file, SymbolKind::Export, "helpers");
    assert_eq!(
        namespace.qualified_name, "helpers",
        "a namespace re-export keeps its public name as its qualified name"
    );
    assert!(symbol(&file, SymbolKind::Variable, "size").export.exported);
    let component = symbol(&file, SymbolKind::Component, "Helpers");
    assert!(component.export.default_export);
    assert_eq!(
        file.symbols
            .iter()
            .filter(|declared| declared.export.default_export)
            .count(),
        1,
        "the component is the file's only default export"
    );
}

#[test]
fn vue_options_api_default_export_does_not_compete_with_the_component() {
    let file = extract(
        "src/Options.vue",
        "<script>
export default {
  name: 'Options',
  methods: { save() { persist(); } },
};
</script>
",
    );
    let save = symbol(&file, SymbolKind::Method, "save");
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "persist")
            .owner
            .as_ref(),
        Some(&save.id)
    );
    let defaults = file
        .symbols
        .iter()
        .filter(|declared| declared.export.default_export)
        .map(|declared| (declared.kind, declared.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(defaults, [(SymbolKind::Component, "Options")]);
}

#[test]
fn zod_schema_fields_keep_resolvable_qualified_names_in_components() {
    let file = extract(
        "src/Form.svelte",
        "<script lang=\"ts\">
import { z } from 'zod';
const UserSchema = z.object({ name: z.string() });
function check() { return UserSchema.shape.name; }
</script>
",
    );
    let field = symbol(&file, SymbolKind::Field, "name");
    assert_eq!(field.qualified_name, "UserSchema::name");
    assert!(file.references.iter().any(|used| {
        used.kind == ReferenceKind::References
            && used.resolution_name.as_deref() == Some("UserSchema::name")
    }));
}

#[test]
fn static_imports_anchor_module_reference_and_bindings_on_the_specifier() {
    let source = "<script>\nimport Card, { helper as run } from './Card.vue';\nimport './styles.css';\n</script>\n";
    let file = extract("src/Imports.vue", source);
    let specifier = source.find("./Card.vue").unwrap_or_default();
    let module = reference(&file, ReferenceKind::Imports, "./Card.vue");
    assert_eq!(
        usize::try_from(module.span.start_byte()).unwrap_or_default(),
        specifier
    );
    let card_bindings = file
        .import_bindings
        .iter()
        .filter(|binding| binding.module_specifier == "./Card.vue")
        .collect::<Vec<_>>();
    assert_eq!(card_bindings.len(), 2);
    assert!(
        card_bindings
            .iter()
            .all(|binding| binding.span == module.span),
        "module-file resolution needs a binding on the module reference's span"
    );
    let side_effect = reference(&file, ReferenceKind::Imports, "./styles.css");
    assert_eq!(
        usize::try_from(side_effect.span.start_byte()).unwrap_or_default(),
        source.find("./styles.css").unwrap_or_default()
    );
}

#[test]
fn template_expressions_end_at_their_real_delimiter() {
    let svelte = extract(
        "src/Braces.svelte",
        "<p>{render(\"}\", after())}</p>\n<p>{pick({ value: make() }, later())}</p>\n",
    );
    for name in ["render", "after", "pick", "make", "later"] {
        assert!(has_reference(&svelte, ReferenceKind::Calls, name), "{name}");
    }
    let vue = extract(
        "src/Braces.vue",
        "<template><p>{{ format(\"}}\", tail()) }}</p><p>{{ wrap({ inner: build() }) }}</p></template>\n",
    );
    for name in ["format", "tail", "wrap", "build"] {
        assert!(has_reference(&vue, ReferenceKind::Calls, name), "{name}");
    }
    let unbalanced = extract(
        "src/Comments.svelte",
        "<p>{render(/* ' */ after())}</p>\n<p>{broken('x)}</p>\n<p>{later()}</p>\n",
    );
    for name in ["render", "after", "later"] {
        assert!(
            has_reference(&unbalanced, ReferenceKind::Calls, name),
            "{name}: comments and unbalanced quotes must not hide later expressions"
        );
    }
}

#[test]
fn event_handler_attributes_are_expressions_or_named_handlers() {
    let vue = extract(
        "src/Handlers.vue",
        "<template>
  <button @click=\"emit('fake()')\">a</button>
  <button @click=\"svc.format()\">b</button>
  <button @click=\"save\">c</button>
  <button v-on:click=\"store.reset\">d</button>
</template>
",
    );
    assert!(has_reference(&vue, ReferenceKind::Calls, "emit"));
    assert!(!vue.references.iter().any(|used| used.name == "fake"));
    assert!(has_reference(&vue, ReferenceKind::Calls, "svc.format"));
    assert!(has_reference(&vue, ReferenceKind::Calls, "save"));
    assert!(has_reference(&vue, ReferenceKind::Calls, "store.reset"));

    let svelte = extract(
        "src/routes/Form.svelte",
        "<button on:click=\"{() => go()}\">x</button>\n<button on:click=\"{save}\">y</button>\n<form method=\"POST\" action=\"?/create\"></form>\n",
    );
    assert!(
        has_reference(&svelte, ReferenceKind::Calls, "save"),
        "a quoted braced handler name calls the handler"
    );
    assert_eq!(
        svelte
            .references
            .iter()
            .filter(|used| used.kind == ReferenceKind::Calls && used.name == "go")
            .count(),
        1,
        "a braced Svelte handler is walked once"
    );
    assert!(
        has_reference(&svelte, ReferenceKind::Calls, "create"),
        "a SvelteKit form action names its server action"
    );
}

#[test]
fn svelte_handler_like_text_inside_other_attribute_values_is_not_a_call() {
    let file = extract(
        "src/AttributeValues.svelte",
        r#"<p title="example on:click={fake}">x</p>
<p title='example onclick={alsoFake}'>x</p>
<p title="example on:click={renderValue()}">x</p>
<p data-value={"example on:click={nestedFake}"}>x</p>
<p data-value={{ text: 'on:click={objectFake}' }}>x</p>
<p data-value={"example on:click='quotedFake'"}>x</p>
<button title="example on:click={ignored}" on:click={live}>x</button>
<button data-value={wrap({ text: 'onclick={ignoredAgain}' })} onclick={store.save}>x</button>
<form data-value={"example action='?/phantomAction'"} action="?/create"></form>
"#,
    );
    let calls = file
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        ["live", "store.save", "create", "renderValue", "wrap"]
    );
}

#[test]
fn compiler_names_are_dropped_in_every_invocation_shape() {
    let file = extract(
        "src/Shapes.svelte",
        "<script>
  const raw = $state.raw([]);
  const boxed = new $state();
  const total = $derived(sum(raw));
  import { $fake } from './fake';
</script>
",
    );
    assert!(has_reference(&file, ReferenceKind::Calls, "sum"));
    for leaked in ["$state", "$state.raw", "raw", "$derived"] {
        assert!(
            !file.references.iter().any(|used| {
                used.name == leaked
                    && matches!(
                        used.kind,
                        ReferenceKind::Calls | ReferenceKind::Instantiates
                    )
            }),
            "{leaked} leaked as an invocation"
        );
    }
    assert!(
        !file
            .references
            .iter()
            .any(|used| { used.resolution_name.as_deref() == Some("fake") }),
        "an import binding is not a store subscription"
    );
    symbol(&file, SymbolKind::Constant, "boxed");
}

#[test]
fn crlf_and_unicode_sources_keep_exact_byte_columns() {
    for (path, source) in [
        (
            "src/Ünïcode.vue",
            "<template>\r\n  <p title=\"café\">{{ naïve(größe) }}</p>\r\n</template>\r\n<script setup lang=\"ts\">\r\nconst größe = 1\r\nfunction naïve(wert: number): number { return wert }\r\n</script>\r\n",
        ),
        (
            "src/Ünïcode.astro",
            "---\r\nconst größe = 'é';\r\n---\r\n<p>é {größe.toFixed()}</p>\r\n",
        ),
    ] {
        let file = extract(path, source);
        assert_eq!(file.parse_status, FileParseStatus::Parsed, "{path}");
        symbol(&file, SymbolKind::Constant, "größe");
    }
}

/// Output contract at scale; the scan-work bound itself is asserted by the
/// `template_scan` unit tests, which count the bytes the scanner examines.
#[test]
fn many_tiny_template_expressions_each_keep_their_call() {
    let mut source = String::from("<script setup>\nconst total = 1\n</script>\n<template>\n");
    for _ in 0..5_000 {
        source.push_str("<i>{{ total }}</i>{{ tick() }}\n");
    }
    source.push_str("</template>\n");
    let file = extract("src/Many.vue", &source);
    assert_eq!(
        file.references
            .iter()
            .filter(|used| used.kind == ReferenceKind::Calls && used.name == "tick")
            .count(),
        5_000
    );
    assert_eq!(file.symbols.len(), 2, "expressions declare nothing");
}

#[test]
fn embedded_diagnostics_are_globally_capped_and_empty_components_emit_nothing() {
    let mut broken = String::from("<script>\n");
    for _ in 0..200 {
        broken.push_str("const = ;\n");
    }
    broken.push_str("</script>\n");
    let file = extract("src/Broken.svelte", &broken);
    assert_eq!(file.parse_status, FileParseStatus::Partial);
    assert!(!file.diagnostics.is_empty() && file.diagnostics.len() <= 32);

    for path in ["src/Empty.vue", "src/Empty.svelte", "src/Empty.astro"] {
        let empty = extract(path, "");
        assert_eq!(empty.symbols, [], "{path}");
        assert_eq!(empty.references, [], "{path}");
    }
}

#[test]
fn v1_type_reference_fixtures_keep_their_type_of_and_returns_counts() {
    for (path, source, minimum_type_of, expects_box) in [
        (
            "docs/test-beds/vue/fixture.vue",
            include_str!("../../../docs/test-beds/vue/fixture.vue"),
            2,
            false,
        ),
        (
            "docs/test-beds/svelte/fixture.svelte",
            include_str!("../../../docs/test-beds/svelte/fixture.svelte"),
            1,
            true,
        ),
    ] {
        let file = extract(path, source);
        let type_of = file
            .references
            .iter()
            .filter(|used| used.kind == ReferenceKind::TypeOf)
            .collect::<Vec<_>>();
        assert!(type_of.len() >= minimum_type_of, "{path}: {type_of:?}");
        assert!(has_returns(&file), "{path} lost its return-type reference");
        if expects_box {
            assert!(type_of.iter().any(|used| used.name == "Box"), "{path}");
        }
    }
    let astro = extract(
        "docs/test-beds/astro/fixture.astro",
        include_str!("../../../docs/test-beds/astro/fixture.astro"),
    );
    symbol(&astro, SymbolKind::Component, "fixture");
    symbol(&astro, SymbolKind::Function, "greet");
    assert!(has_reference(&astro, ReferenceKind::Calls, "greet"));
    assert!(has_reference(&astro, ReferenceKind::References, "Layout"));
}

fn has_returns(file: &ExtractedFile) -> bool {
    file.references
        .iter()
        .any(|used| used.kind == ReferenceKind::Returns)
}

#[test]
fn raw_tag_text_inside_attributes_and_expressions_is_not_a_script() {
    let file = extract(
        "src/Quoted.vue",
        "<template>
  <p title=\"<script>function ghost() {} ghost();</script>\">{{ real() }}</p>
  <p>{{ '<script>' }}</p>
</template>
<script setup>
function later() {}
</script>
",
    );
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
    assert_eq!(file.diagnostics, []);
    assert!(has_reference(&file, ReferenceKind::Calls, "real"));
    symbol(&file, SymbolKind::Function, "later");
    assert!(
        !file.symbols.iter().any(|declared| declared.name == "ghost")
            && !file.references.iter().any(|used| used.name == "ghost"),
        "script text inside an attribute value is not code"
    );
}

#[test]
fn astro_object_braces_inside_expressions_stay_script() {
    let file = extract(
        "src/components/Braces.astro",
        "<p>{render({ format() { return labelFor(); } })}</p>\n",
    );
    for name in ["render", "labelFor"] {
        assert!(has_reference(&file, ReferenceKind::Calls, name), "{name}");
    }
    assert!(
        !has_reference(&file, ReferenceKind::Calls, "format"),
        "an object method in an expression is not a call"
    );
    assert_eq!(file.parse_status, FileParseStatus::Parsed);
}

#[test]
fn regular_expressions_do_not_end_template_expressions() {
    let file = extract(
        "src/Regex.svelte",
        "<p>{render(/}/.test(x), after())}</p>\n<p>{ratio(a / b, c / d)}</p>\n<p>{later()}</p>\n",
    );
    for name in ["render", "after", "ratio", "later"] {
        assert!(has_reference(&file, ReferenceKind::Calls, name), "{name}");
    }
}

#[test]
fn svelte_module_export_lists_do_not_export_instance_declarations() {
    let file = extract(
        "src/Scopes.svelte",
        "<script context=\"module\">
  function helper() { moduleOnly(); }
  export { helper };
</script>
<script>
  function helper() { instanceOnly(); }
</script>
",
    );
    let helpers = file
        .symbols
        .iter()
        .filter(|declared| declared.kind == SymbolKind::Function && declared.name == "helper")
        .collect::<Vec<_>>();
    assert_eq!(helpers.len(), 2);
    assert_eq!(
        helpers
            .iter()
            .filter(|declared| declared.export.exported)
            .count(),
        1,
        "only the module script's helper is exported"
    );
    let exported = helpers
        .iter()
        .find(|declared| declared.export.exported)
        .unwrap_or_else(|| panic!("no exported helper"));
    assert_eq!(
        reference(&file, ReferenceKind::Calls, "moduleOnly")
            .owner
            .as_ref(),
        Some(&exported.id)
    );
}

#[test]
fn store_reads_in_parameter_defaults_subscribe_in_both_dialects() {
    for (path, lang) in [
        ("src/StoreTs.svelte", " lang=\"ts\""),
        ("src/StoreJs.svelte", ""),
    ] {
        let source =
            format!("<script{lang}>\nfunction read(n = $count) {{ return n; }}\n</script>\n");
        let file = extract(path, &source);
        let subscription = reference(&file, ReferenceKind::References, "$count");
        assert_eq!(
            subscription.resolution_name.as_deref(),
            Some("count"),
            "{path}"
        );
    }
}

#[test]
fn compiler_invocation_filtering_scales_with_many_runes() {
    let mut source = String::from("<script>\n");
    for _ in 0..10_000 {
        source.push_str("$state(0); ordinary();\n");
    }
    source.push_str("</script>\n");
    let file = extract("src/ManyRunes.svelte", &source);
    assert_eq!(
        file.references
            .iter()
            .filter(|used| used.kind == ReferenceKind::Calls && used.name == "ordinary")
            .count(),
        10_000
    );
    assert!(!file.references.iter().any(|used| used.name == "$state"));
}

/// Output contract; the scan-work bound of unterminated expressions is asserted
/// by the `template_scan` unit tests.
#[test]
fn unterminated_expressions_inside_an_html_comment_are_not_code() {
    let mut source = String::from("<template><!--");
    for _ in 0..20_000 {
        source.push_str("{{/*}}");
    }
    source.push_str("--><p>{{ real() }}</p></template>\n");
    let file = extract("src/Comments.vue", &source);
    assert!(has_reference(&file, ReferenceKind::Calls, "real"));
    assert_eq!(
        file.references
            .iter()
            .filter(|used| used.kind == ReferenceKind::Calls)
            .count(),
        1,
        "commented-out template text is not code"
    );
}

#[test]
fn braced_svelte_attributes_hide_script_text_and_name_handlers() {
    let file = extract(
        "src/Braced.svelte",
        "<script>\n  function save() {}\n</script>\n<button on:click={() => log(\"<script>function ghost() {} ghost();</script>\")}>\n  {real()}\n</button>\n<button on:click={save}>a</button>\n<button onclick={store.reset}>b</button>\n",
    );
    assert!(has_reference(&file, ReferenceKind::Calls, "real"));
    assert!(has_reference(&file, ReferenceKind::Calls, "log"));
    assert!(has_reference(&file, ReferenceKind::Calls, "save"));
    assert!(has_reference(&file, ReferenceKind::Calls, "store.reset"));
    assert!(
        !file.symbols.iter().any(|declared| declared.name == "ghost")
            && !file.references.iter().any(|used| used.name == "ghost"),
        "script text inside a braced attribute string is not code"
    );
}

#[test]
fn comments_before_regular_expressions_keep_the_expression_whole() {
    let file = extract(
        "src/CommentRegex.svelte",
        "<p>{render(/* note */ /}/.test(value), after())}</p>\n<p>{render(// note\n /}/.test(value), later())}</p>\n",
    );
    for name in ["render", "after", "later"] {
        assert!(has_reference(&file, ReferenceKind::Calls, name), "{name}");
    }
    let vue = extract(
        "src/LeadingRegex.vue",
        "<template><p>{{ /x/.test(value) ? matched() : missed() }}</p><p>{{ /* note */ format(value) }}</p></template>\n",
    );
    for name in ["matched", "missed", "format"] {
        assert!(has_reference(&vue, ReferenceKind::Calls, name), "{name}");
    }
}

#[test]
fn each_program_enriches_with_its_own_export_list() {
    let file = extract(
        "src/Models.svelte",
        "<script context=\"module\">
  import { z } from 'zod';
  const Model = z.object({ id: z.string() });
  export { Model };
</script>
<script>
  let local = 0;
</script>
",
    );
    assert!(symbol(&file, SymbolKind::Constant, "Model").export.exported);
    assert!(
        symbol(&file, SymbolKind::Struct, "Model").export.exported,
        "the schema of an exported constant is exported"
    );
    assert!(!symbol(&file, SymbolKind::Variable, "local").export.exported);
}

#[test]
fn svelte_inline_handlers_declare_nothing_and_their_calls_belong_to_the_component() {
    let file = extract(
        "src/Counter.svelte",
        "<script>
  let count = 0;
  const next = 1;
  function save(value) {}
</script>
<button on:click={() => { const next = count + 1; save(next); }}>a</button>
<button on:click={function pick() { const count = 2; save(count); }}>b</button>
<p>{(() => { const temporary = work(); return temporary; })()}</p>
<p>{exports.alias = save}</p>
",
    );
    let component = symbol(&file, SymbolKind::Component, "Counter");
    assert_template_declares_nothing(&file, &["count", "next", "save"]);
    for name in ["pick", "temporary", "alias"] {
        assert!(
            !file.symbols.iter().any(|declared| declared.name == name),
            "the template declared {name}"
        );
    }
    assert_calls_owned_by(&file, &component.id, &[("save", 2), ("work", 1)]);
}

#[test]
fn vue_inline_handlers_and_template_callbacks_declare_nothing() {
    let file = extract(
        "src/Labels.vue",
        "<script setup>
const label = 'x'
function compute() { return 1 }
</script>
<template>
  <button @click=\"() => { const v = compute(); emit('x', v) }\">go</button>
  <p>{{ list.map(function f(x) { const label = x.name; return label }) }}</p>
</template>
",
    );
    let component = symbol(&file, SymbolKind::Component, "Labels");
    assert_template_declares_nothing(&file, &["label", "compute"]);
    for name in ["v", "f"] {
        assert!(
            !file.symbols.iter().any(|declared| declared.name == name),
            "the template declared {name}"
        );
    }
    assert_calls_owned_by(
        &file,
        &component.id,
        &[("compute", 1), ("emit", 1), ("list.map", 1)],
    );
}

#[test]
fn astro_template_callbacks_declare_nothing() {
    let file = extract(
        "src/List.astro",
        "---
const label = 'x';
const items: string[] = [];
---
<ul>{items.map((item) => { const label = format(item); return <li>{label}</li>; })}</ul>
",
    );
    let component = symbol(&file, SymbolKind::Component, "List");
    assert_template_declares_nothing(&file, &["label", "items"]);
    assert_calls_owned_by(&file, &component.id, &[("format", 1), ("items.map", 1)]);
}

#[test]
fn one_template_expression_too_deep_to_walk_keeps_the_rest_of_the_component() {
    let deep = format!("{}x{}", "(".repeat(600), ")".repeat(600));
    for (path, source) in [
        (
            "src/Deep.svelte",
            format!(
                "<script>\n  function keep() {{}}\n</script>\n<p>{{{deep}}}</p>\n<p>{{after()}}</p>\n<Card />\n"
            ),
        ),
        (
            "src/Deep.vue",
            format!(
                "<script setup>\nfunction keep() {{}}\n</script>\n<template><p>{{{{ {deep} }}}}</p><p>{{{{ after() }}}}</p><Card /></template>\n"
            ),
        ),
        (
            "src/Deep.astro",
            format!(
                "---\nfunction keep() {{}}\n---\n<p>{{{deep}}}</p>\n<p>{{after()}}</p>\n<Card />\n"
            ),
        ),
    ] {
        let file = extract(path, &source);
        symbol(&file, SymbolKind::Component, "Deep");
        symbol(&file, SymbolKind::Function, "keep");
        assert!(
            has_reference(&file, ReferenceKind::Calls, "after"),
            "{path}"
        );
        assert!(
            has_reference(&file, ReferenceKind::References, "Card"),
            "{path}"
        );
        assert_eq!(file.parse_status, FileParseStatus::Partial, "{path}");
        assert_eq!(
            file.diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == DiagnosticCode::NestingLimitExceeded)
                .count(),
            1,
            "{path}: {:?}",
            file.diagnostics
        );
    }
}

#[test]
fn unquoted_script_attributes_select_the_dialect_and_skip_data_blocks() {
    let file = extract(
        "src/Unquoted.vue",
        "<script lang=ts>
interface User { name: string }
export function greet(user: User): string { return user.name }
</script>
<script type=application/ld+json>
{\"@context\": go()}
</script>
",
    );
    assert_eq!(
        file.parse_status,
        FileParseStatus::Parsed,
        "{:?}",
        file.diagnostics
    );
    symbol(&file, SymbolKind::Interface, "User");
    assert!(has_reference(&file, ReferenceKind::TypeOf, "User"));
    assert!(
        !has_reference(&file, ReferenceKind::Calls, "go"),
        "a JSON data block is not code"
    );

    let spaced = extract(
        "src/Spaced.vue",
        "<script lang = ts>
interface Account { id: string }
</script>
<script type = application/ld+json>
{\"@context\": go()}
</script>
",
    );
    assert_eq!(
        spaced.parse_status,
        FileParseStatus::Parsed,
        "{:?}",
        spaced.diagnostics
    );
    symbol(&spaced, SymbolKind::Interface, "Account");
    assert!(!has_reference(&spaced, ReferenceKind::Calls, "go"));
}

#[test]
fn spaced_property_names_are_not_regular_expression_keywords() {
    for (path, source) in [
        (
            "src/Spaced.svelte",
            "{value . in / 2}<script>function kept(){return /x/}</script><p>{after()}</p>\n",
        ),
        (
            "src/Commented.svelte",
            "{value . /*c*/ in / 2}<script>const value={in:4}; function kept(){return /x/}</script><p>{after()}</p>\n",
        ),
        (
            "src/Postfix.svelte",
            "{count++ / 2}<script>let count = 0; function kept(){return /x/}</script><p>{after()}</p>\n",
        ),
    ] {
        let file = extract(path, source);
        symbol(&file, SymbolKind::Function, "kept");
        assert!(
            has_reference(&file, ReferenceKind::Calls, "after"),
            "{path}"
        );
        assert_eq!(file.parse_status, FileParseStatus::Parsed, "{path}");
    }
}

/// A long single line of well-formed expressions whose divisions could be
/// misread as regular expressions never exhausts the scan budget.
#[test]
fn well_formed_divisions_never_exhaust_the_template_scan() {
    let mut source = "{value . /*c*/ in / 2}[".repeat(20_000);
    source.push_str(
        "<script>const value={in:4}; function inner(){} function after(){return 0}</script><p>{((() => { inner() })(), after())}</p>\n",
    );
    let file = extract("src/Divisions.svelte", &source);
    assert_eq!(
        file.parse_status,
        FileParseStatus::Parsed,
        "{:?}",
        file.diagnostics
    );
    assert!(has_reference(&file, ReferenceKind::Calls, "after"));
    assert!(has_reference(&file, ReferenceKind::Calls, "inner"));
}

#[test]
fn inline_handler_locals_never_resolve_to_script_declarations() {
    let svelte = extract(
        "src/Shadow.svelte",
        "<script>
  function save() {}
  function handler() {}
  const item = { render() {} };
</script>
<button on:click={() => { const save = () => {}; save(); }}>a</button>
<button on:click={(handler) => handler()}>b</button>
<p>{items.map((item) => item.render())}</p>
<p>{[1].map(function Local() { return new Local(); })}</p>
<p>{save()}</p>
",
    );
    let component = symbol(&svelte, SymbolKind::Component, "Shadow");
    assert_calls_owned_by(
        &svelte,
        &component.id,
        &[
            ("save", 1),
            ("handler", 0),
            ("item.render", 0),
            ("items.map", 1),
        ],
    );
    assert!(!has_reference(
        &svelte,
        ReferenceKind::Instantiates,
        "Local"
    ));

    let vue = extract(
        "src/Shadow.vue",
        "<script setup>
function pick(event) { return event }
function fn() {}
</script>
<template>
  <button @click=\"(event) => { const fn = pick(event); fn() }\">a</button>
  <p>{{ fn() }}</p>
</template>
",
    );
    let component = symbol(&vue, SymbolKind::Component, "Shadow");
    assert_calls_owned_by(&vue, &component.id, &[("pick", 1), ("fn", 1)]);
}

#[test]
fn template_local_names_hide_script_names_only_in_their_own_scope() {
    let file = extract(
        "src/Scopes.svelte",
        "<script>
  function save() {}
  function load() {}
  function run() {}
</script>
<button on:click={() => { if (ready) { const save = () => {}; save(); } save(); }}>a</button>
<button on:click={() => { load(); for (load of loaders) load(); }}>b</button>
<p>{[1].map((run) => run) && run()}</p>
<p>{(function outer() { var run = 1; return () => run(); })()}</p>
",
    );
    let component = symbol(&file, SymbolKind::Component, "Scopes");
    // The block's `save` hides only the call inside the block; `for (load of
    // …)` assigns the script binding instead of declaring one; a callback's
    // parameter ends with the callback; a hoisted `var` reaches nested
    // functions.
    assert_calls_owned_by(
        &file,
        &component.id,
        &[("save", 1), ("load", 2), ("run", 1)],
    );
}

#[test]
fn template_scopes_follow_javascript_binding_rules() {
    // Each line is one template expression; the comment says whether its
    // `save` call reaches the script function.
    let source = "<script>
  function save() {}
</script>
<p>{(function (value = save()) { var save; })}</p>
<p>{({ [save()](save) {} })}</p>
<p>{(() => { class C { static { var save; } } save(); })}</p>
<p>{(() => { save(); for (var save of []) {} save(); })}</p>
<p>{(() => { switch (save()) { case 0: const save = () => {}; save(); } })}</p>
<p>{(save => (save)())}</p>
";
    // A default parameter does not see the body's `var`; a computed method
    // name is outside the method; a static block's `var` stays in the block;
    // `for (var …)` hoists to the whole function; a `case` declaration does
    // not cover the discriminant; parentheses do not hide a parameter.
    let reaching_lines = [4, 5, 6, 8];
    let file = extract("src/Scoping.svelte", source);
    let mut lines = file
        .references
        .iter()
        .filter(|used| used.kind == ReferenceKind::Calls && used.name == "save")
        .map(|used| used.span.start_line())
        .collect::<Vec<_>>();
    lines.sort_unstable();
    assert_eq!(lines, reaching_lines);

    let typed = extract(
        "src/Typed.svelte",
        "<script lang=\"ts\">
  class Local {}
</script>
<p>{(class Local { method() { return new Local(); } })}</p>
",
    );
    assert!(!has_reference(&typed, ReferenceKind::Instantiates, "Local"));
}

#[test]
fn spreads_and_decimal_points_are_not_member_access() {
    let file = extract(
        "src/Punctuation.svelte",
        "<p>{[... /*c*/ typeof /}/, after()]}</p>\n<p>{1. /*c*/ in /}/ ? yes() : no()}</p>\n<p>{value?.in / 2 > limit() ? big() : small()}</p>\n",
    );
    for name in ["after", "yes", "no", "limit", "big", "small"] {
        assert!(has_reference(&file, ReferenceKind::Calls, name), "{name}");
    }
}

#[test]
fn template_dynamic_imports_keep_the_module_reference_but_bind_nothing() {
    let file = extract(
        "src/Lazy.svelte",
        "<script>
  const loaded = import('./script.js');
</script>
<p>{import('./m.js').then(done)}</p>
",
    );
    assert!(has_reference(&file, ReferenceKind::Imports, "./m.js"));
    assert!(
        !file
            .import_bindings
            .iter()
            .any(|binding| binding.module_specifier == "./m.js"),
        "a template binds no module names: {:?}",
        file.import_bindings
    );
    assert!(!has_reference(&file, ReferenceKind::References, "then"));
    assert!(
        file.import_bindings
            .iter()
            .any(|binding| binding.module_specifier == "./script.js"),
        "script dynamic imports keep their bindings"
    );
}

#[test]
fn unterminated_template_structure_is_partial_and_keeps_later_scripts_and_tags() {
    let mut source = "{/*}".repeat(20_000);
    source.push_str("<script>\n  function keep() {}\n</script>\n<Card />\n<p>{after()}</p>\n");
    let file = extract("src/Damaged.svelte", &source);
    symbol(&file, SymbolKind::Function, "keep");
    assert!(has_reference(&file, ReferenceKind::References, "Card"));
    assert!(has_reference(&file, ReferenceKind::Calls, "after"));
    assert_eq!(file.parse_status, FileParseStatus::Partial);
    assert!(
        file.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::SyntaxError),
        "{:?}",
        file.diagnostics
    );
}

#[test]
fn keyword_regexes_and_nested_template_literals_keep_the_expression_whole() {
    let file = extract(
        "src/Lexical.svelte",
        "<p>{typeof /}/.test(x) ? yes() : no()}</p>\n<p>{format(`x ${inner(`}`)}`, after())}</p>\n<p>{value.in / 2 > limit() ? big() : small()}</p>\n",
    );
    for name in [
        "yes", "no", "format", "inner", "after", "limit", "big", "small",
    ] {
        assert!(has_reference(&file, ReferenceKind::Calls, name), "{name}");
    }
}

#[test]
fn markup_inside_expressions_strings_and_directives_is_never_a_component_use() {
    let vue = extract(
        "src/Strings.vue",
        "<template>{{ '< Café />' }}<p>< Café</p><p>{{ '<Ghost/>' }}</p><Card /></template>\n",
    );
    assert!(has_reference(&vue, ReferenceKind::References, "Card"));
    for ghost in ["Café", "Ghost"] {
        assert!(
            !vue.references.iter().any(|used| used.name.contains(ghost)),
            "{ghost} is text, not a tag"
        );
    }
    let svelte = extract(
        "src/Directives.svelte",
        "<script>\n  let count = 0;\n</script>\n{#if count <Max}<Row />{/if}\n{@html '<Ghost />'}\n",
    );
    assert!(has_reference(&svelte, ReferenceKind::References, "Row"));
    assert!(
        !svelte
            .references
            .iter()
            .any(|used| used.name.starts_with("Max") || used.name == "Ghost"),
        "directive and string text is not a tag"
    );
}

#[test]
fn braced_svelte_form_actions_are_walked_once_without_literal_calls() {
    let file = extract(
        "src/routes/Actions.svelte",
        "<form action=\"{resolveAction('phantom()')}\"></form>\n<form method=\"POST\" action=\"?/create\"></form>\n",
    );
    assert_eq!(
        file.references
            .iter()
            .filter(|used| used.kind == ReferenceKind::Calls && used.name == "resolveAction")
            .count(),
        1
    );
    assert!(!file.references.iter().any(|used| used.name == "phantom"));
    assert!(has_reference(&file, ReferenceKind::Calls, "create"));
}

#[test]
fn top_level_value_references_belong_to_the_component() {
    let file = extract(
        "src/Values.svelte",
        "<script>\n  const item = 1;\n  sink(item);\n</script>\n",
    );
    let component = symbol(&file, SymbolKind::Component, "Values");
    let value = reference(&file, ReferenceKind::References, "item");
    assert_eq!(value.owner.as_ref(), Some(&component.id));
}

/// Every symbol named in `script_names` is declared once, by the script.
fn assert_template_declares_nothing(file: &ExtractedFile, script_names: &[&str]) {
    for name in script_names {
        assert_eq!(
            file.symbols
                .iter()
                .filter(|declared| declared.name == *name)
                .count(),
            1,
            "{name} is declared more than once: {:?}",
            file.symbols
                .iter()
                .map(|declared| (declared.kind, declared.qualified_name.as_str()))
                .collect::<Vec<_>>()
        );
    }
}

/// Each expected `(name, count)` is called exactly `count` times, and every
/// one of those calls is owned by `owner`.
fn assert_calls_owned_by(
    file: &ExtractedFile,
    owner: &cartograph_domain::SymbolId,
    expected: &[(&str, usize)],
) {
    for &(name, count) in expected {
        let calls = file
            .references
            .iter()
            .filter(|used| used.kind == ReferenceKind::Calls && used.name == name)
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), count, "{name}: {calls:?}");
        assert!(
            calls.iter().all(|used| used.owner.as_ref() == Some(owner)),
            "{name} is not owned by the component: {calls:?}"
        );
    }
}

fn assert_partial_at_line(file: &ExtractedFile, line: u32) {
    assert_eq!(
        file.parse_status,
        FileParseStatus::Partial,
        "{}",
        file.path.as_str()
    );
    assert!(
        file.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::SyntaxError
                && diagnostic
                    .span
                    .is_some_and(|span| span.start_line() == line)
        }),
        "{} has no host-line {line} syntax diagnostic: {:?}",
        file.path.as_str(),
        file.diagnostics
    );
}

/// Extract twice, require identical output, and require every span to be an
/// exact host-file range whose bytes contain the fact's name.
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
    assert_eq!(first, second, "{path} extraction was not deterministic");
    for declared in &first.symbols {
        assert_exact_span(source, declared.span, None);
    }
    for used in &first.references {
        assert_exact_span(source, used.span, Some(reference_text(used)));
    }
    for binding in &first.import_bindings {
        assert_exact_span(source, binding.span, None);
    }
    first
}

/// The name a reference's own span must spell: its last member segment.
fn reference_text(used: &ExtractedReference) -> &str {
    used.name.rsplit('.').next().unwrap_or(&used.name)
}

fn assert_exact_span(source: &str, span: cartograph_domain::SourceSpan, text: Option<&str>) {
    let start = usize::try_from(span.start_byte()).unwrap_or(usize::MAX);
    let end = usize::try_from(span.end_byte()).unwrap_or(usize::MAX);
    let covered = source
        .get(start..end)
        .unwrap_or_else(|| panic!("span {start}..{end} is not a host range"));
    for (byte, line, column) in [
        (start, span.start_line(), span.start_column()),
        (end, span.end_line(), span.end_column()),
    ] {
        assert_eq!(
            host_position(source, byte),
            (line, column),
            "span {start}..{end} ({covered:?}) has a remapped position"
        );
    }
    if let Some(text) = text {
        assert!(
            covered.contains(text),
            "span {start}..{end} ({covered:?}) does not spell {text}"
        );
    }
}

fn host_position(source: &str, byte: usize) -> (u32, u32) {
    let before = &source[..byte];
    let line = before.matches('\n').count() + 1;
    let column = byte - before.rfind('\n').map_or(0, |newline| newline + 1);
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    )
}

fn symbol<'file>(
    file: &'file ExtractedFile,
    kind: SymbolKind,
    name: &str,
) -> &'file ExtractedSymbol {
    file.symbols
        .iter()
        .find(|declared| declared.kind == kind && declared.name == name)
        .unwrap_or_else(|| {
            panic!(
                "{} is missing {kind:?} {name}; symbols={:?}",
                file.path.as_str(),
                file.symbols
                    .iter()
                    .map(|declared| (declared.kind, declared.qualified_name.as_str()))
                    .collect::<Vec<_>>()
            )
        })
}

fn reference<'file>(
    file: &'file ExtractedFile,
    kind: ReferenceKind,
    name: &str,
) -> &'file ExtractedReference {
    file.references
        .iter()
        .find(|used| used.kind == kind && used.name == name)
        .unwrap_or_else(|| {
            panic!(
                "{} is missing {kind:?} {name}; references={:?}",
                file.path.as_str(),
                file.references
                    .iter()
                    .map(|used| (used.kind, used.name.as_str()))
                    .collect::<Vec<_>>()
            )
        })
}

fn has_reference(file: &ExtractedFile, kind: ReferenceKind, name: &str) -> bool {
    file.references
        .iter()
        .any(|used| used.kind == kind && used.name == name)
}

fn contains(
    file: &ExtractedFile,
    parent: &cartograph_domain::SymbolId,
    child: &cartograph_domain::SymbolId,
) -> bool {
    file.containments
        .iter()
        .any(|edge| &edge.parent == parent && &edge.child == child)
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("web-component source limit failed: {error}"))
}

#[test]
fn embedded_static_imports_screen_credentials() {
    credential_support::assert_screened(
        "main.astro",
        "---\nimport X from \"@VALUE@\";\n---\n<X />\n",
        "https://example.invalid/module",
    );
}

#[test]
fn component_form_actions_screen_before_call_name_projection() {
    for path in ["action.vue", "action.svelte"] {
        credential_support::assert_screened(path, "<form action=\"@VALUE@()\"></form>\n", "save");
    }
}
