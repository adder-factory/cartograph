//! Release-wide privacy regression for literal-derived extraction facts.

mod credential_support;
mod dependency_ownership;
#[path = "credential_support/escaped_names.rs"]
mod escaped_names;
#[path = "credential_support/escaped_specifiers.rs"]
mod escaped_specifiers;

use credential_support::assert_screened;

const CASES: &[(&str, &str, &str)] = &[
    (
        "namespace.ts",
        "export * as \"@VALUE@\" from \"./safe\";\n",
        "token",
    ),
    (
        "sql.ts",
        "const query = 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    ),
    (
        "sql.js",
        "const query = 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    ),
    (
        "sql.tsx",
        "const query = 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    ),
    (
        "sql.jsx",
        "const query = 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    ),
    ("sql.py", "query = 'SELECT * FROM \"@VALUE@\"'\n", "token"),
    (
        "sql.go",
        "package main\nvar query = `SELECT * FROM \"@VALUE@\"`\n",
        "token",
    ),
    (
        "sql.rs",
        "const QUERY: &str = r#\"SELECT * FROM \"@VALUE@\"\"#;\n",
        "token",
    ),
    (
        "Sql.java",
        "class Sql { void load() { db.query(\"\"\"\nSELECT * FROM \"@VALUE@\"\n\"\"\"); } }\n",
        "token",
    ),
    (
        "Sql.kt",
        "val query = \"\"\"SELECT * FROM \"@VALUE@\"\"\"\"\n",
        "token",
    ),
    (
        "Sql.cs",
        "class Sql { void Load() { db.Query(\"\"\"\nSELECT * FROM \"@VALUE@\"\n\"\"\"); } }\n",
        "token",
    ),
    (
        "sql.php",
        "<?php $query = 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    ),
    ("sql.rb", "query = 'SELECT * FROM \"@VALUE@\"'\n", "token"),
    (
        "nested.sql",
        "SELECT 'SELECT * FROM \"@VALUE@\"';\n",
        "token",
    ),
    (
        "schema.graphql",
        "\"@VALUE@\" type User { id: ID }\n",
        "A user record.",
    ),
    (
        "block.graphql",
        "\"\"\"@VALUE@\"\"\" type User { id: ID }\n",
        "A user record.",
    ),
    (
        "application/controllers/Users.php",
        "<?php class Users extends CI_Controller { public function show() { $this->load->model('@VALUE@', 'users'); $this->users->find(); } }\n",
        "user_model",
    ),
    ("main.wesl", "import @VALUE@;\n", "token::lib"),
    ("import.wgsl", "#import @VALUE@\n", "token::lib"),
    ("module.wgsl", "#define_import_path @VALUE@\n", "token::lib"),
    (
        "comment.sol",
        "pragma solidity /*@VALUE@*/ ^0.8.0;\n",
        "solidity",
    ),
    (
        "main.R",
        "source(\"@VALUE@\")\n",
        "https://example.invalid/module",
    ),
    ("main.rb", "require \"@VALUE@\"\n", "token"),
    (
        "accessors.rb",
        "class Example; attr_reader :\"@VALUE@\"; attr_reader :token; end\n",
        "token",
    ),
    ("main.lua", "require(\"@VALUE@\")\n", "token"),
    ("main.luau", "require(\"@VALUE@\")\n", "token"),
    ("main.khn", "require(\"@VALUE@\")\n", "token"),
    (
        "default.nix",
        "import \"@VALUE@\"\n",
        "https://example.invalid/module",
    ),
    ("names.nix", "{ \"@VALUE@\" = 1; }\n", "token"),
    ("main.lisp", "(defpackage \"@VALUE@\")\n", "token"),
    ("main.tf", "variable \"@VALUE@\" {}\n", "token"),
    (
        "main.dart",
        "import '@VALUE@';\n",
        "https://example.invalid/module",
    ),
    (
        "export.dart",
        "export '@VALUE@';\n",
        "https://example.invalid/module",
    ),
    (
        "sections/main.liquid",
        "{% schema %}{\"name\":\"@VALUE@\"}{% endschema %}\n",
        "Hero",
    ),
    (
        "main.ts",
        "function f(x: import(\"@VALUE@\").T) {}\n",
        "https://example.invalid/module",
    ),
    (
        "dynamic.ts",
        "const x = import(\"@VALUE@\");\n",
        "https://example.invalid/module",
    ),
    (
        "require.js",
        "const x = require(\"@VALUE@\");\n",
        "https://example.invalid/module",
    ),
    (
        "main.astro",
        "---\nimport X from \"@VALUE@\";\n---\n<X />\n",
        "https://example.invalid/module",
    ),
    (
        "main.vue",
        "<script>import X from \"@VALUE@\";</script><template><X /></template>\n",
        "https://example.invalid/module",
    ),
    (
        "main.svelte",
        "<script>import X from \"@VALUE@\";</script><X />\n",
        "https://example.invalid/module",
    ),
    ("action.vue", "<form action=\"@VALUE@()\"></form>\n", "save"),
    (
        "action.svelte",
        "<form action=\"@VALUE@()\"></form>\n",
        "save",
    ),
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
    (
        "main.sol",
        "import \"@VALUE@\";\n",
        "https://example.invalid/module",
    ),
    (
        "main.php",
        "<?php include \"@VALUE@\";\n",
        "https://example.invalid/module",
    ),
    (
        "main.ets",
        "import X from \"@VALUE@\";\n",
        "https://example.invalid/module",
    ),
    (
        "dispatch.ets",
        "function f(obj:any) { obj[\"@VALUE@\"](); }\n",
        "token",
    ),
    (
        "dispatch.ts",
        "function f(obj:any) { obj[\"@VALUE@\"](); }\n",
        "token",
    ),
    (
        "dispatch.astro",
        "---\nfunction f(obj) { obj[\"@VALUE@\"](); }\n---\n<div />\n",
        "token",
    ),
    (
        "names.ts",
        "const x=1; export { x as \"@VALUE@\" };\n",
        "token",
    ),
    (
        "bindings.ts",
        "import { \"@VALUE@\" as x } from \"./safe\";\n",
        "token",
    ),
    (
        "destructure.js",
        "const {\"@VALUE@\": x} = require(\"./safe\");\n",
        "token",
    ),
    (
        "exports.js",
        "const x=1; module.exports={\"@VALUE@\": x};\n",
        "token",
    ),
    (
        "namespace.ts",
        "export * as \"@VALUE@\" from \"./safe\";\n",
        "token",
    ),
    (
        "pragma.sol",
        "pragma @VALUE@;\n",
        "experimental ABIEncoderV2",
    ),
    (
        "main.go",
        "package main\nimport \"@VALUE@\"\n",
        "https://example.invalid/module",
    ),
    ("main.sh", "source \"@VALUE@\"\n", "token"),
    ("main.ps1", "using module \"@VALUE@\"\n", "token"),
    ("main.m", "#import \"@VALUE@\"\n", "token"),
    ("main.c", "#include \"@VALUE@\"\n", "token"),
    ("main.cpp", "#include \"@VALUE@\"\n", "token"),
    (
        "keys.ts",
        "import { z } from \"zod\"; const S=z.object({\"@VALUE@\":z.string()});\n",
        "token",
    ),
    (
        "enum.ts",
        "import { z } from \"zod\"; const S=z.object({ mode:z.enum([\"@VALUE@\"]) });\n",
        "Ready",
    ),
    (
        "contracts.ts",
        "interface Service<Name extends string> { name: Name } type Config=Service<\"@VALUE@\">;\n",
        "mode",
    ),
    (
        "models.py",
        "from pydantic import BaseModel\nfrom typing import Literal\nclass Config(BaseModel):\n    mode: Literal[\"@VALUE@\"]\n",
        "Ready",
    ),
    (
        "main.sql",
        "CREATE TYPE state AS ENUM ('@VALUE@');\n",
        "Ready",
    ),
    (
        "sections/partner.liquid",
        "{% render '@VALUE@' %}\n",
        "Hero",
    ),
    (
        "view.cmp",
        "<aura:component><aura:attribute name=\"@VALUE@\" type=\"String\"/></aura:component>\n",
        "token",
    ),
    (
        "view.page",
        "<apex:page controller=\"@VALUE@\"></apex:page>\n",
        "Controller",
    ),
    (
        "module.bas",
        "Attribute VB_Name = \"@VALUE@\"\nPublic Sub Run()\nEnd Sub\n",
        "token",
    ),
    (
        "mapper.xml",
        "<mapper namespace=\"@VALUE@\"><select id=\"find\">select 1</select></mapper>\n",
        "token",
    ),
    (
        "ids.xml",
        "<mapper namespace=\"Mapper\"><select id=\"@VALUE@\">select 1</select></mapper>\n",
        "token",
    ),
    (
        "aliases.xml",
        "<configuration><typeAliases><typeAlias alias=\"@VALUE@\" type=\"Thing\"/></typeAliases><mappers/></configuration>\n",
        "token",
    ),
    (
        "Public/Mod/RootTemplates/main.lsx",
        "<save><region id=\"@VALUE@\"><node id=\"GameObject\"><attribute id=\"Name\" value=\"Thing\" /></node></region></save>\n",
        "Region",
    ),
    (
        "Public/Mod/RootTemplates/objects.lsx",
        "<save><region id=\"Templates\"><node id=\"GameObject\"><attribute id=\"Name\" value=\"@VALUE@\" /></node></region></save>\n",
        "Thing",
    ),
    (
        "Mods/Mod/Stats/Generated/Data/main.txt",
        "new entry \"@VALUE@\"\ntype \"Weapon\"\n",
        "Thing",
    ),
    (
        "main.lsj",
        "{\"Name\":\"@VALUE@\",\"UUID\":\"11111111-1111-1111-1111-111111111111\"}\n",
        "Thing",
    ),
    ("config.hcl", "variable \"@VALUE@\" {}\n", "token"),
    (
        "modules.tf",
        "module \"safe\" { source=\"@VALUE@\" }\n",
        "https://example.invalid/module",
    ),
    (
        "server.ts",
        "import express from \"express\"; const app=express(); function handle(){} app.get(\"@VALUE@\",handle);\n",
        "/orders",
    ),
    (
        "api.py",
        "from fastapi import FastAPI\napp=FastAPI()\n@app.get(\"@VALUE@\")\ndef handle():\n    pass\n",
        "/orders",
    ),
    (
        "routes.rs",
        "use actix_web::get;\n#[get(\"@VALUE@\")]\nasync fn handle() {}\n",
        "/orders",
    ),
    (
        "routes.rb",
        "Rails.application.routes.draw do\n get '@VALUE@', to: 'orders#index'\nend\n",
        "/orders",
    ),
    (
        "Controller.java",
        "import org.springframework.web.bind.annotation.GetMapping;\nclass Controller { @GetMapping(\"@VALUE@\") public void handle(){} }\n",
        "/orders",
    ),
    (
        "Controller.kt",
        "import org.springframework.web.bind.annotation.GetMapping\nclass Controller { @GetMapping(\"@VALUE@\") fun handle() {} }\n",
        "/orders",
    ),
    (
        "Controller.cs",
        "using Microsoft.AspNetCore.Mvc;\n[Route(\"api\")] class Controller { [HttpGet(\"@VALUE@\")] public void Handle(){} }\n",
        "/orders",
    ),
    (
        "routes.go",
        "package main\nimport \"github.com/gin-gonic/gin\"\nfunc handle(){}\nfunc main(){ r:=gin.Default(); r.GET(\"@VALUE@\",handle) }\n",
        "/orders",
    ),
    (
        "routes.swift",
        "import Vapor\nfunc handle(){}\napp.get(\"@VALUE@\", use: handle)\n",
        "orders",
    ),
    (
        "app.dart",
        "final router=GoRouter(routes:[GoRoute(path:'@VALUE@',builder:(context,state)=>Home())]);\n",
        "/orders",
    ),
    (
        "Controller.php",
        "<?php\nuse Symfony\\Component\\Routing\\Attribute\\Route;\nclass Controller { #[Route('@VALUE@')] public function handle(){} }\n",
        "/orders",
    ),
    (
        "Events.ts",
        "import { DeviceEventEmitter } from 'react-native';\nDeviceEventEmitter.addListener('@VALUE@', onEvent);\n",
        "orderChanged",
    ),
    (
        "NativeThing.ts",
        "import { TurboModuleRegistry, TurboModule } from 'react-native';\nexport interface Spec extends TurboModule {}\nexport default TurboModuleRegistry.getEnforcing<Spec>('@VALUE@');\n",
        "Thing",
    ),
    (
        "Conditions.java",
        "class Conditions { @ConditionalOnProperty(name=\"@VALUE@\") void start(){} }\n",
        "feature.enabled",
    ),
    (
        "services.yml",
        "services:\n  \"@VALUE@\":\n    class: Demo\\Service\n",
        "ServiceName",
    ),
    ("doc.ts", "// @VALUE@\nfunction run(){}\n", "Documentation"),
    ("doc.dart", "// @VALUE@\nvoid run(){}\n", "Documentation"),
    ("doc.rb", "# @VALUE@\ndef run; end\n", "Documentation"),
    (
        "doc.R",
        "#' @VALUE@\nrun <- function() 1\n",
        "Documentation",
    ),
    ("doc.scala", "// @VALUE@\ndef run() = 1\n", "Documentation"),
    ("doc.rs", "/// @VALUE@\nfn run(){}\n", "Documentation"),
    ("doc.c", "// @VALUE@\nvoid run(){}\n", "Documentation"),
    ("doc.swift", "// @VALUE@\nfunc run(){}\n", "Documentation"),
    ("doc.clj", ";; @VALUE@\n(defn run [x] x)\n", "Documentation"),
    (
        "force-app/main/default/classes/Run.cls",
        "// @VALUE@\nclass Run {}\n",
        "Documentation",
    ),
    ("doc.ets", "// @VALUE@\nfunction run(){}\n", "Documentation"),
    ("doc.go", "// @VALUE@\nfunc run(){}\n", "Documentation"),
    ("doc.cpp", "// @VALUE@\nvoid run(){}\n", "Documentation"),
    ("doc.kt", "// @VALUE@\nfun run(){}\n", "Documentation"),
    ("doc.groovy", "// @VALUE@\ndef run(){}\n", "Documentation"),
    (
        "doc.lua",
        "-- @VALUE@\nfunction run() end\n",
        "Documentation",
    ),
    (
        "doc.luau",
        "-- @VALUE@\nfunction run() end\n",
        "Documentation",
    ),
    (
        "doc.php",
        "<?php\n// @VALUE@\nfunction run(){}\n",
        "Documentation",
    ),
    ("doc.sol", "// @VALUE@\ncontract Run {}\n", "Documentation"),
    ("doc.sh", "# @VALUE@\nrun() { :; }\n", "Documentation"),
    ("doc.res", "// @VALUE@\nlet run = x => x\n", "Documentation"),
    ("keys.yaml", "\"@VALUE@\": 1\n", "token"),
    (
        "keys.sql",
        "CREATE TABLE \"@VALUE@\" (value int);\n",
        "token",
    ),
    (
        "columns.sql",
        "CREATE TABLE Example (\"@VALUE@\" int);\n",
        "token",
    ),
    ("keys.swift", "func `@VALUE@`() {}\n", "token"),
    ("names.fs", "let ``@VALUE@`` = 1\n", "token"),
    ("names.kt", "fun `@VALUE@`() {}\n", "token"),
    ("names.scala", "def `@VALUE@`() = 1\n", "token"),
    ("names.jl", "#= @VALUE@ =#\nBase.:+(x) = x\n", ":+"),
    (
        "nested_names.fs",
        "let outer() =\n    let ``@VALUE@`` = 1\n    1\n",
        "token",
    ),
    (
        "nested_names.kt",
        "fun outer() { val `@VALUE@` = 1 }\n",
        "token",
    ),
    (
        "nested_names.scala",
        "def outer() = { val `@VALUE@` = 1 }\n",
        "token",
    ),
    (
        "fields.swift",
        "class Example { var `@VALUE@`: Int = 1 }\n",
        "token",
    ),
    (
        "Localization/main.lsx",
        "<contentList><content contentuid=\"@VALUE@\">Text</content></contentList>\n",
        "h1234567890abcdef1234567890abcdef",
    ),
    (
        "templates/output.liquid",
        "{{ @VALUE@ | default: 'fallback' }}\n",
        "product",
    ),
    (
        "aura/Type/Type.cmp",
        "<aura:component><aura:attribute name=\"value\" type=\"@VALUE@\"/></aura:component>\n",
        "Account",
    ),
    ("keys.properties", "@VALUE@ = plain\n", "token"),
    (
        "title.test.ts",
        "test(\"@VALUE@\",()=>{});\n",
        "ordinary test",
    ),
    (
        "tests/title.astro",
        "---\ntest(\"@VALUE@\",()=>{});\n---\n<div />\n",
        "ordinary test",
    ),
    (
        "tests/title.vue",
        "<script>test(\"@VALUE@\",()=>{});</script>\n",
        "ordinary test",
    ),
    (
        "tests/title.svelte",
        "<script>test(\"@VALUE@\",()=>{});</script>\n",
        "ordinary test",
    ),
];

const ESCAPED_MODULE_CASES: &[(&str, &str)] = &[
    ("escaped.ts", "import X from \"@VALUE@\";\n"),
    ("escaped.js", "export * from \"@VALUE@\";\n"),
    ("named.ts", "export { X } from \"@VALUE@\";\n"),
    ("type.ts", "function f(x: import(\"@VALUE@\").T) {}\n"),
    ("dynamic.ts", "const x = import(\"@VALUE@\");\n"),
    ("require.js", "const x = require(\"@VALUE@\");\n"),
    (
        "escaped.astro",
        "---\nimport X from \"@VALUE@\";\n---\n<X />\n",
    ),
    (
        "escaped.vue",
        "<script>import X from \"@VALUE@\";</script><template><X /></template>\n",
    ),
    (
        "escaped.svelte",
        "<script>import X from \"@VALUE@\";</script><X />\n",
    ),
    ("import.dart", "import '@VALUE@';\n"),
    ("export.dart", "export '@VALUE@';\n"),
];

#[test]
fn literal_derived_facts_across_families_screen_credentials() {
    let mut failed = Vec::new();
    for &(path, template, ordinary) in CASES {
        if std::panic::catch_unwind(|| assert_screened(path, template, ordinary)).is_err() {
            failed.push(path);
        }
    }
    for &(path, template) in ESCAPED_MODULE_CASES {
        if std::panic::catch_unwind(|| {
            escaped_specifiers::assert_escaped_specifiers_abstain(path, template);
        })
        .is_err()
        {
            failed.push(path);
        }
    }
    for &(path, source) in escaped_names::ESCAPED_NAME_CASES {
        if std::panic::catch_unwind(|| {
            credential_support::assert_no_credentials(&credential_support::extract(path, source));
        })
        .is_err()
        {
            failed.push(path);
        }
    }
    assert!(
        failed.is_empty(),
        "credential screening failed for {failed:?}"
    );
}

#[test]
fn ordinary_mailto_and_escaped_literals_keep_their_facts() {
    let file = credential_support::extract(
        "mail.ts",
        "/** Contact mailto:support@example.invalid?subject=Re:auth */\nfunction f() {}\n",
    );
    let f = file.symbols.iter().find(|symbol| symbol.name == "f");
    assert!(
        f.and_then(|symbol| symbol.docstring.as_deref())
            .is_some_and(|doc| doc.contains("mailto:support@example.invalid")),
        "{file:?}"
    );
    let file = credential_support::extract("token.ts", "type Token = \"\\u0074oken\";\n");
    let alias = file.symbols.iter().find(|symbol| symbol.name == "Token");
    assert!(
        alias
            .and_then(|symbol| symbol.signature.as_deref())
            .is_some(),
        "{file:?}"
    );
    let file = credential_support::extract(
        "schema.graphql",
        "\"A \\\"key\\\" field.\" type User { id: ID }\n",
    );
    let user = file.symbols.iter().find(|symbol| symbol.name == "User");
    assert!(
        user.and_then(|symbol| symbol.docstring.as_deref())
            .is_some(),
        "{file:?}"
    );
}
