//! Integration coverage for Cartograph native extraction contracts.

mod credential_support;
mod dependency_ownership;
#[path = "credential_support/escaped_names.rs"]
mod escaped_names;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{ExtractError, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_LIMIT: usize = 1024 * 1024;
const SECRET: &str = "cartograph_literal_secret_sentinel_7c1f";

struct RouteFixture {
    path: &'static str,
    source: &'static str,
    language: SourceLanguage,
    route: &'static str,
}

const ROUTES: [RouteFixture; 13] = [
    RouteFixture {
        path: "src/server.ts",
        source: "import express from 'express';\nconst app = express();\nexport function listOrders() {}\napp.get('/orders', listOrders);\nconst secret = 'cartograph_literal_secret_sentinel_7c1f';\n",
        language: SourceLanguage::TypeScript,
        route: "GET /orders",
    },
    RouteFixture {
        path: "src/routes.ts",
        source: "import { Routes } from '@angular/router';\nconst routes: Routes = [{ path: 'orders', component: OrdersPage }];\n",
        language: SourceLanguage::TypeScript,
        route: "/orders",
    },
    RouteFixture {
        path: "src/server.js",
        source: "Bun.serve({ routes: { '/orders': listOrders } });\n",
        language: SourceLanguage::JavaScript,
        route: "ANY /orders",
    },
    RouteFixture {
        path: "app/api.py",
        source: "from fastapi import FastAPI\napp = FastAPI()\n@app.get('/orders')\ndef list_orders():\n    return []\n",
        language: SourceLanguage::Python,
        route: "GET /orders",
    },
    RouteFixture {
        path: "routes/web.php",
        source: "<?php\nuse Illuminate\\Support\\Facades\\Route;\nRoute::get('/orders', [OrderController::class, 'index']);\n",
        language: SourceLanguage::Php,
        route: "GET /orders",
    },
    RouteFixture {
        path: "config/routes.rb",
        source: "Rails.application.routes.draw do\n  get '/orders', to: 'orders#index'\nend\n",
        language: SourceLanguage::Ruby,
        route: "GET /orders",
    },
    RouteFixture {
        path: "src/OrderController.java",
        source: "import org.springframework.web.bind.annotation.GetMapping;\nclass OrderController {\n @GetMapping(\"/orders\")\n public void listOrders() {}\n}\n",
        language: SourceLanguage::Java,
        route: "GET /orders",
    },
    RouteFixture {
        path: "src/OrderController.cs",
        source: "using Microsoft.AspNetCore.Mvc;\n[Route(\"api/orders\")]\nclass OrderController {\n [HttpGet(\"/orders\")] public void ListOrders() {}\n}\n",
        language: SourceLanguage::CSharp,
        route: "GET /orders",
    },
    RouteFixture {
        path: "cmd/server.go",
        source: "package main\nimport \"github.com/gin-gonic/gin\"\nfunc listOrders() {}\nfunc main() { r := gin.Default(); r.GET(\"/orders\", listOrders) }\n",
        language: SourceLanguage::Go,
        route: "GET /orders",
    },
    RouteFixture {
        path: "src/routes.rs",
        source: "use actix_web::{get, HttpResponse};\n#[get(\"/orders\")]\nasync fn list_orders() -> HttpResponse { todo!() }\n",
        language: SourceLanguage::Rust,
        route: "GET /orders",
    },
    RouteFixture {
        path: "lib/router.dart",
        source: "final router = GoRouter(routes: [GoRoute(path: '/orders', builder: (context, state) => OrdersPage())]);\n",
        language: SourceLanguage::Dart,
        route: "ANY /orders",
    },
    RouteFixture {
        path: "Sources/App/routes.swift",
        source: "import Vapor\nfunc routes(_ app: Application) throws { app.get(\"/orders\", use: listOrders) }\n",
        language: SourceLanguage::Swift,
        route: "GET /orders",
    },
    RouteFixture {
        path: "conf/routes",
        source: "GET /orders controllers.OrderController.list()\n",
        language: SourceLanguage::Yaml,
        route: "GET /orders",
    },
];

#[test]
fn framework_routes_cover_major_v1_ecosystems_with_typed_searchable_symbols() {
    for fixture in ROUTES {
        let first = extract(fixture.path, fixture.source, fixture.language);
        let second = extract(fixture.path, fixture.source, fixture.language);
        assert_eq!(first, second, "{} was not deterministic", fixture.path);
        let route = first
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Route && symbol.name == fixture.route)
            .unwrap_or_else(|| {
                panic!(
                    "{} missing route {}; routes={:?}",
                    fixture.path,
                    fixture.route,
                    first
                        .symbols
                        .iter()
                        .filter(|symbol| symbol.kind == SymbolKind::Route)
                        .map(|symbol| symbol.name.as_str())
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            route.export.exported,
            "{} route was not public",
            fixture.path
        );
        assert!(
            !format!("{first:?}").contains(SECRET),
            "{} leaked a source literal: {first:?}",
            fixture.path,
        );
    }
}

#[test]
fn framework_signals_add_cli_and_configuration_edges_without_copying_values() {
    let typescript = extract(
        "src/cli.ts",
        "import { Command } from 'commander';\nconst program = new Command();\nprogram.command('serve');\nfunction locate(c: any) { return [process.env.DEPLOY_REGION, process.env['DEPLOY_TOKEN'], c.env.WORKER_REGION, __dirname, import.meta.url]; }\n// process.env.COMMENTED_OUT\nconst secret = 'cartograph_literal_secret_sentinel_7c1f';\n",
        SourceLanguage::TypeScript,
    );
    assert!(
        typescript
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Route && symbol.name == "cmd serve")
    );
    for expected in [
        "DEPLOY_REGION",
        "DEPLOY_TOKEN",
        "WORKER_REGION",
        "__dirname",
        "import.meta.url",
    ] {
        assert!(
            typescript.references.iter().any(|reference| {
                reference.kind == ReferenceKind::References && reference.name == expected
            }),
            "missing TypeScript config/build-context reference {expected}: {:?}",
            typescript.references
        );
    }
    assert!(
        typescript
            .references
            .iter()
            .all(|reference| reference.name != "COMMENTED_OUT")
    );

    let spring = extract(
        "src/OrderService.java",
        "class OrderService {\n @Value(\"${orders.cache.ttl:30}\") String ttl;\n void load() { System.getenv(\"ORDERS_REGION\"); }\n}\n",
        SourceLanguage::Java,
    );
    for expected in ["orders.cache.ttl", "ORDERS_REGION"] {
        assert!(
            spring.references.iter().any(|reference| {
                reference.kind == ReferenceKind::References && reference.name == expected
            }),
            "missing config reference {expected}"
        );
    }
    assert!(
        !format!("{typescript:?}{spring:?}").contains(SECRET),
        "framework signal output leaked: typescript={typescript:?} spring={spring:?}"
    );

    let rust = extract(
        "src/install.rs",
        r#"
fn load() {
    config("APP_MODE");
    static_config(".codex/config.toml");
}
"#,
        SourceLanguage::Rust,
    );
    assert!(rust.references.iter().any(|reference| {
        reference.kind == ReferenceKind::References && reference.name == "APP_MODE"
    }));
    assert!(
        rust.references.iter().all(|reference| {
            reference.kind != ReferenceKind::References || reference.name != ".codex/config.toml"
        }),
        "static_config argument leaked into configuration references: {:?}",
        rust.references
    );
}

#[test]
fn framework_routes_ignore_comments_and_span_multiline_static_calls() {
    let extracted = extract(
        "src/server.ts",
        r"
import express from 'express';
// app.get('/commented-line', ignored);
/*
app.post('/commented-block', ignored);
*/
app.patch(
  '/orders/:id',
  OrderController.update,
);
router.options('/orders', corsHandler);
router.use('/admin', adminRouter);
router.use(authMiddleware);
",
        SourceLanguage::TypeScript,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        routes,
        ["PATCH /orders/:id", "OPTIONS /orders", "USE /admin"]
    );
    assert!(
        extracted.references.iter().any(|reference| {
            reference.kind == ReferenceKind::Calls && reference.name == "update"
        })
    );
    assert!(!format!("{extracted:?}").contains("commented"));
}

#[test]
fn framework_route_markers_do_not_match_identifier_suffixes() {
    let extracted = extract(
        "src/workspace-files.test.ts",
        r#"
import { Router } from '@example/router';
test('omitted path uses realpath(".")', async () => {
  const resolved = await realpath(".");
  return resolved;
});
"#,
        SourceLanguage::TypeScript,
    );
    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Route),
        "realpath invented a framework route: {:?}",
        extracted.symbols
    );
}

#[test]
fn framework_routes_do_not_treat_expression_names_and_promise_all_as_express() {
    let extracted = extract(
        "src/generic-work.ts",
        r#"
function expressionCall(source) {
  return source;
}

await Promise.all([
  mkdir("scratch"),
  writeFile("manifest.xml", ""),
]);

await Promise.all([
  client.send("Runtime.enable"),
]);
"#,
        SourceLanguage::TypeScript,
    );

    assert!(
        extracted
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Route),
        "Promise.all invented framework routes: {:?}",
        extracted.symbols
    );
}

#[test]
fn framework_routes_cover_hono_on_and_fastify_object_forms() {
    let extracted = extract(
        "src/server.ts",
        r"
import { Hono } from 'hono';
import Fastify from 'fastify';
app.on('PATCH', '/orders/:id', patchOrder);
fastify.route({
  method: 'POST',
  url: '/orders',
  handler: createOrder,
});
",
        SourceLanguage::TypeScript,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(routes, ["PATCH /orders/:id", "POST /orders"]);
    for handler in ["patchOrder", "createOrder"] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Calls && reference.name == handler
            }),
            "missing handler reference {handler}: {:?}",
            extracted.references
        );
    }
}

#[test]
fn path_convention_routes_cover_next_sveltekit_and_nuxt_even_when_files_are_empty() {
    let fixtures = [
        (
            "src/pages/index.tsx",
            SourceLanguage::Tsx,
            SymbolKind::Route,
            "/",
        ),
        (
            "src/pages/blog/[slug].jsx",
            SourceLanguage::Jsx,
            SymbolKind::Route,
            "/blog/:slug",
        ),
        (
            "src/app/(shop)/orders/[id]/page.ts",
            SourceLanguage::TypeScript,
            SymbolKind::Route,
            "/orders/:id",
        ),
        (
            "src/routes/docs/[...rest]/+server.ts",
            SourceLanguage::TypeScript,
            SymbolKind::Route,
            "/docs/*rest",
        ),
        (
            "src/routes/[[locale]]/+layout.js",
            SourceLanguage::JavaScript,
            SymbolKind::Route,
            "/:locale?",
        ),
        (
            "pages/blog/[slug].vue",
            SourceLanguage::Vue,
            SymbolKind::Route,
            "/blog/:slug",
        ),
        (
            "server/api/users/[id].ts",
            SourceLanguage::TypeScript,
            SymbolKind::Route,
            "/api/users/:id",
        ),
        (
            "middleware/auth.global.ts",
            SourceLanguage::TypeScript,
            SymbolKind::Function,
            "auth.global",
        ),
    ];
    for (path, language, kind, name) in fixtures {
        let extracted = extract(path, "", language);
        let symbol = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.kind == kind && symbol.name == name)
            .unwrap_or_else(|| panic!("{path} missing {kind:?} {name}: {extracted:?}"));
        assert_eq!(symbol.span.start_byte(), 0, "{path}");
        assert_eq!(symbol.span.end_byte(), 0, "{path}");
    }

    let layout = extract(
        "src/app/orders/layout.tsx",
        "export default function Layout() { return null; }",
        SourceLanguage::Tsx,
    );
    assert!(
        layout
            .symbols
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::Route)
    );
}

#[test]
fn framework_routes_cover_resource_axum_chi_and_python_docstring_boundaries() {
    let php = extract(
        "routes/web.php",
        r"<?php
# Route::resource('ignored', IgnoredController::class);
Route::resource('users', UserController::class);
Route::apiResource('teams', TeamController::class);
",
        SourceLanguage::Php,
    );
    for expected in ["resource:users", "resource:teams"] {
        assert!(
            php.symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == expected }),
            "missing {expected}: {php:?}"
        );
    }
    assert!(
        php.symbols
            .iter()
            .all(|symbol| symbol.name != "resource:ignored")
    );

    let rust = extract(
        "src/routes.rs",
        r#"
use axum::{routing::{get, post}, Router};
let app = Router::new()
  .route("/orders", get(list_orders))
  .route("/orders", post(create_order));
"#,
        SourceLanguage::Rust,
    );
    for expected in ["GET /orders", "POST /orders"] {
        assert!(
            rust.symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == expected }),
            "missing {expected}: {rust:?}"
        );
    }

    let go = extract(
        "routes.go",
        r#"
package api
import "github.com/go-chi/chi/v5"
func routes(r chi.Router, req *http.Request) {
  _ = req.Header.Get("Content-Type")
  r.Get("/orders", listOrders)
}
"#,
        SourceLanguage::Go,
    );
    assert!(
        go.symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == "GET /orders" })
    );
    assert!(
        go.symbols
            .iter()
            .all(|symbol| !symbol.name.contains("Content-Type"))
    );

    let python = extract(
        "app.py",
        r#"
from flask import Flask
"""Example only:
@app.route('/not-real')
"""
# @app.route('/also-not-real')
@app.route('/real')
def real():
    return None
"#,
        SourceLanguage::Python,
    );
    let routes = python
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(routes, ["ANY /real"]);
}

#[test]
fn laravel_routes_keep_source_labels_and_qualified_controller_resolution_hints() {
    let source = r"<?php
use App\Http\Controllers\OrderController;
Route::get('/orders', [OrderController::class, 'index']);
Route::post('/legacy', 'OrderController@store');
Route::options('/orders', OrderController::class);
Route::any('/health', healthCheck);
Route::resource('users', UserController::class)->only(['index']);
Route::get('/inline', static fn () => ['ok' => true]);
";
    let php = extract("routes/web.php", source, SourceLanguage::Php);
    let expectations = [
        ("GET /orders", "index", Some("OrderController::index")),
        (
            "POST /legacy",
            "OrderController@store",
            Some("OrderController::store"),
        ),
        (
            "OPTIONS /orders",
            "OrderController",
            Some("OrderController"),
        ),
        ("ANY /health", "healthCheck", None),
        ("resource:users", "UserController", Some("UserController")),
    ];
    for (route_name, reference_name, resolution_name) in expectations {
        let route = php
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Route && symbol.name == route_name)
            .unwrap_or_else(|| panic!("missing Laravel route {route_name}: {php:?}"));
        let reference = php
            .references
            .iter()
            .find(|reference| {
                reference.owner.as_ref() == Some(&route.id)
                    && reference.kind == ReferenceKind::Calls
                    && reference.name == reference_name
            })
            .unwrap_or_else(|| {
                panic!("missing Laravel target {route_name} -> {reference_name}: {php:?}")
            });
        assert_eq!(reference.resolution_name.as_deref(), resolution_name);
        let start = usize::try_from(reference.span.start_byte())
            .unwrap_or_else(|error| panic!("span start does not fit usize: {error}"));
        let end = usize::try_from(reference.span.end_byte())
            .unwrap_or_else(|error| panic!("span end does not fit usize: {error}"));
        assert_eq!(&source[start..end], reference_name);
    }

    let inline = php
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Route && symbol.name == "GET /inline")
        .unwrap_or_else(|| panic!("missing closure route: {php:?}"));
    assert!(
        php.references
            .iter()
            .all(|reference| reference.owner.as_ref() != Some(&inline.id)),
        "inline closure must not invent a callable target: {php:?}"
    );
}

#[test]
fn global_php_route_controllers_retain_their_exact_scope_marker() {
    for (name, lookup) in [
        (r"\OrderController::show", r"\OrderController::show"),
        (
            r"\Other\OrderController::show",
            r"\Other::OrderController::show",
        ),
    ] {
        let source = format!("show:\n  path: /show\n  controller: '{name}'\n");
        let file = extract("config/routes.yaml", &source, SourceLanguage::Yaml);
        let handler = file
            .references
            .iter()
            .find(|reference| reference.kind == ReferenceKind::Calls)
            .unwrap_or_else(|| panic!("missing global route handler"));
        assert_eq!(handler.name, name);
        assert_eq!(handler.resolution_name.as_deref(), Some(lookup));
    }
}

#[test]
fn framework_configuration_routes_cover_symfony_drupal_and_codeigniter() {
    let symfony = extract(
        "config/routes.yaml",
        r"
orders_show:
  path: /orders/{id}
  controller: App\Controller\OrderController::show
  methods: [GET]
",
        SourceLanguage::Yaml,
    );
    let route = symfony
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Route && symbol.name == "orders_show")
        .unwrap_or_else(|| panic!("missing Symfony route: {symfony:?}"));
    assert!(symfony.references.iter().any(|reference| {
        reference.owner.as_ref() == Some(&route.id)
            && reference.kind == ReferenceKind::Calls
            && reference.name == "App\\Controller\\OrderController::show"
            && reference.resolution_name.as_deref()
                == Some("App\\Controller::OrderController::show")
    }));

    let drupal = extract(
        "modules/custom/orders/orders.routing.yml",
        r"
orders.hello:
  path: '/hello'
  defaults:
    _controller: '\Drupal\orders\Controller\HelloController::build'
  methods: [GET]
",
        SourceLanguage::Yaml,
    );
    assert!(
        drupal
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == "/hello [GET]" })
    );
    assert!(drupal.references.iter().any(|reference| {
        reference.name == "\\Drupal\\orders\\Controller\\HelloController::build"
            && reference.resolution_name.as_deref()
                == Some("\\Drupal\\orders\\Controller::HelloController::build")
    }));

    let codeigniter = extract(
        "application/config/routes.php",
        r"<?php
$route['default_controller'] = 'welcome';
$route['product/(:num)']['DELETE'] = 'catalog/product_lookup_by_id/$1';
",
        SourceLanguage::Php,
    );
    for expected in ["ANY /", "DELETE /product/(:num)"] {
        assert!(
            codeigniter
                .symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == expected }),
            "missing {expected}: {codeigniter:?}"
        );
    }
    assert!(
        codeigniter
            .references
            .iter()
            .any(|reference| { reference.name == "catalog/product_lookup_by_id/$1" })
    );
}

#[test]
fn framework_landmarks_cover_neug_swiftui_flutter_and_grouped_vapor_routes() {
    let neug = extract(
        "graph.py",
        r#"
import neug
graph = neug.Graph("catalog")
users = neug.Vertex("User")
likes = neug.Edge("LIKES")
"#,
        SourceLanguage::Python,
    );
    for expected in ["neug:graph:catalog", "neug:vertex:User", "neug:edge:LIKES"] {
        assert!(
            neug.symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Resource && symbol.name == expected }),
            "missing {expected}: {neug:?}"
        );
    }

    let swiftui = extract(
        "Sources/App.swift",
        "import SwiftUI\nstruct ContentView: View { var body: some View { Text(\"Hi\") } }\n",
        SourceLanguage::Swift,
    );
    assert!(
        swiftui
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Component && symbol.name == "ContentView" }),
        "missing SwiftUI component: {swiftui:?}"
    );

    let flutter = extract(
        "lib/main.dart",
        r"
import 'package:flutter/material.dart';
MaterialApp(routes: {
  '/': (context) => HomePage(),
  '/settings': (context) => const SettingsPage(),
});
",
        SourceLanguage::Dart,
    );
    for expected in ["ANY /", "ANY /settings"] {
        assert!(
            flutter
                .symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == expected }),
            "missing {expected}: {flutter:?}"
        );
    }

    let vapor = extract(
        "Sources/routes.swift",
        "import Vapor\napp.grouped(\"api\").post(\"users\", use: createUser)\n",
        SourceLanguage::Swift,
    );
    assert!(
        vapor
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Route && symbol.name == "POST /api/users" })
    );
}

#[test]
fn nestjs_routes_join_controller_paths_and_gate_http_graphql_and_rpc_styles() {
    let extracted = extract(
        "src/hybrid.controller.ts",
        r"
@Controller('/api/')
@Resolver('Thing')
class HybridController {
  @Get()
  list() {}

  @Post('/:id/')
  update() {}

  @Query(() => Thing)
  thing() {}

  @MessagePattern('sum')
  sum() {}

  @SubscribeMessage('events')
  events() {}

  helper() {}
}

@Resolver('OnlyGraph')
class OnlyGraphResolver {
  @Get('/must-not-exist') wrongStyle() {}
  @Mutation(() => Thing) mutate() {}
}

class NotAController {
  @Get('/orphan') orphan() {}
}
",
        SourceLanguage::TypeScript,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "GET /api",
        "POST /api/:id",
        "GraphQL Query thing",
        "MessagePattern sum",
        "WebSocket events",
        "GraphQL Mutation mutate",
    ] {
        assert!(routes.contains(&expected), "missing {expected}: {routes:?}");
    }
    for absent in ["GET /must-not-exist", "GET /orphan"] {
        assert!(!routes.contains(&absent), "unexpected {absent}: {routes:?}");
    }
    for handler in ["list", "update", "thing", "sum", "events", "mutate"] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Calls && reference.name == handler
            }),
            "missing NestJS handler edge for {handler}: {:?}",
            extracted.references
        );
    }
}

#[test]
fn bun_serve_routes_keep_method_maps_top_level_and_reject_nested_config_shapes() {
    let extracted = extract(
        "src/bun-server.ts",
        r"
function health() {}
function listUsers() {}
function createUser() {}
Bun.serve({
  port: 3000,
  routes: {
    '/health': health,
    '/users': { GET: listUsers, POST: createUser },
    '/not-a-method-map': { description: 'metadata' },
  },
  nested: { routes: { '/not-top-level': health } },
});
// Bun.serve({ routes: { '/commented': health } });
",
        SourceLanguage::TypeScript,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["ANY /health", "GET /users", "POST /users"] {
        assert!(routes.contains(&expected), "missing {expected}: {routes:?}");
    }
    for absent in [
        "ANY /not-a-method-map",
        "ANY /not-top-level",
        "ANY /commented",
    ] {
        assert!(!routes.contains(&absent), "unexpected {absent}: {routes:?}");
    }
    for handler in ["health", "listUsers", "createUser"] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.kind == ReferenceKind::Calls && reference.name == handler
            }),
            "missing Bun handler edge for {handler}: {:?}",
            extracted.references
        );
    }
}

#[test]
fn hono_routes_are_receiver_scoped_and_mount_only_the_named_child_router() {
    let extracted = extract(
        "src/hono.ts",
        r"
import { Hono } from 'hono';
const app = new Hono();
const users = new Hono();
const admin = new OpenAPIHono();
users.get('/users', listUsers);
users.on('purge', '/cache', purgeCache);
admin.post('/admin', createAdmin);
database.get('/must-not-exist', unrelated);
app.route('/api', users);
",
        SourceLanguage::TypeScript,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "GET /users",
        "PURGE /cache",
        "POST /admin",
        "GET /api/users",
        "PURGE /api/cache",
    ] {
        assert!(routes.contains(&expected), "missing {expected}: {routes:?}");
    }
    assert!(
        !routes.contains(&"GET /must-not-exist"),
        "unrelated receiver leaked into Hono routes: {routes:?}"
    );
    assert!(
        !routes.contains(&"POST /api/admin"),
        "unnamed child router was mounted: {routes:?}"
    );
}

#[test]
fn spring_and_aspnet_routes_compose_class_and_method_paths_with_static_tokens() {
    let spring = extract(
        "src/OrdersController.java",
        r#"
@RequestMapping("/api")
public class OrdersController {
  @GetMapping
  public void list() {}

  @PostMapping("/orders")
  public void create() {}

  @RequestMapping(value = "/search", method = RequestMethod.PATCH)
  public void search() {}
}
"#,
        SourceLanguage::Java,
    );
    let spring_routes = spring
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["GET /api", "POST /api/orders", "PATCH /api/search"] {
        assert!(
            spring_routes.contains(&expected),
            "missing {expected}: {spring_routes:?}"
        );
    }

    let aspnet = extract(
        "Controllers/OrdersController.cs",
        r#"
[Route("api/[controller]")]
public class OrdersController {
  [HttpGet("{id}")]
  public void GetOne() {}

  [HttpPost]
  [Route("[action]")]
  public void Create() {}
}
"#,
        SourceLanguage::CSharp,
    );
    let aspnet_routes = aspnet
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["GET /api/Orders/{id}", "POST /api/Orders/Create"] {
        assert!(
            aspnet_routes.contains(&expected),
            "missing {expected}: {aspnet_routes:?}"
        );
    }
}

#[test]
fn class_level_mappings_never_become_constructor_routes() {
    // A Kotlin primary constructor sits between the class annotations and the
    // class body, so the class-level `@RequestMapping` used to be read as the
    // constructor's own mapping and published as `ANY /users/users`.
    let kotlin = extract(
        "src/UserController.kt",
        r#"
import org.springframework.web.bind.annotation.GetMapping
import org.springframework.web.bind.annotation.RequestMapping

@RestController
@RequestMapping("/users")
class UserController(private val userService: UserService) {
    @GetMapping("/{id}")
    fun show(id: String): String = id
}
"#,
        SourceLanguage::Kotlin,
    );
    let kotlin_routes = route_names(&kotlin);
    assert_eq!(
        kotlin_routes,
        ["GET /users/{id}", "BASE /users"],
        "{kotlin_routes:?}"
    );

    let aspnet = extract(
        "Controllers/OrdersController.cs",
        r#"
[Route("api/[controller]")]
public class OrdersController(IOrderService orders) : ControllerBase {
  [HttpGet("{id}")]
  public void GetOne() {}
}
"#,
        SourceLanguage::CSharp,
    );
    let aspnet_routes = route_names(&aspnet);
    assert_eq!(
        aspnet_routes,
        ["GET /api/Orders/{id}", "ROUTE api/[controller]"],
        "{aspnet_routes:?}"
    );

    // A body method that happens to share the class name is still a handler.
    let java = extract(
        "src/Orders.java",
        "@RequestMapping(value = {\"/x\"})\nclass Orders {\n  @GetMapping(\"/same\")\n  public String Orders() { return \"\"; }\n}\n",
        SourceLanguage::Java,
    );
    assert_eq!(route_names(&java), ["GET /same"]);
    let kotlin_body = extract(
        "src/Orders.kt",
        "@RequestMapping(\"/o\")\nclass Orders /* don't { */ {\n    @GetMapping(\"/same\")\n    fun Orders(): String = \"\"\n}\n",
        SourceLanguage::Kotlin,
    );
    assert_eq!(route_names(&kotlin_body), ["GET /o/same", "BASE /o"]);
}

#[test]
fn pathless_method_mappings_are_located_at_their_own_annotation() {
    // `[HttpPost]` / `@GetMapping` without a path inherit the class path, but
    // the route is declared by the method's annotation, not the class's.
    let aspnet = extract(
        "Controllers/OrdersController.cs",
        "[Route(\"api/[controller]\")]\npublic class OrdersController : ControllerBase {\n  [HttpGet(\"{id}\")]\n  public void GetOne() {}\n\n  [HttpPost]\n  public void Create() {}\n}\n",
        SourceLanguage::CSharp,
    );
    assert_eq!(
        route_lines(&aspnet),
        [
            ("GET /api/Orders/{id}", 3),
            ("POST /api/Orders", 6),
            ("ROUTE api/[controller]", 1)
        ]
    );
    let spring = extract(
        "src/OrdersController.java",
        "@RequestMapping(\"/api\")\npublic class OrdersController {\n  @GetMapping\n  public void list() {}\n\n  @PostMapping()\n  public void create() {}\n}\n",
        SourceLanguage::Java,
    );
    assert_eq!(
        route_lines(&spring),
        [("GET /api", 3), ("POST /api", 6), ("BASE /api", 1)]
    );
}

#[test]
fn play_route_handlers_keep_their_controller_action_with_arguments() {
    // Play handlers may declare their parameters (`show(id: Long)`, which also
    // splits on the space) or an empty list (`create()`); the action is the
    // same per-file handler reference either way.
    let extracted = extract(
        "conf/routes",
        "GET     /                       controllers.HomeController.index\nGET     /users/:id              controllers.Users.show(id: Long)\nPOST    /users                  controllers.Users.create()\n",
        SourceLanguage::Yaml,
    );
    let handlers = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Calls)
        .map(|reference| (reference.name.as_str(), reference.span.start_line()))
        .collect::<Vec<_>>();
    assert_eq!(
        handlers,
        [
            ("controllers.HomeController.index", 1),
            ("controllers.Users.show", 2),
            ("controllers.Users.create", 3),
        ]
    );
}

/// Route symbol names and start lines of one extraction, in source order.
fn route_lines(extracted: &cartograph_extract::ExtractedFile) -> Vec<(&str, u32)> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| (symbol.name.as_str(), symbol.span.start_line()))
        .collect()
}

/// Route symbol names of one extraction, in source order.
fn route_names(extracted: &cartograph_extract::ExtractedFile) -> Vec<&str> {
    extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect()
}

#[test]
fn rails_routes_expand_resources_compose_namespaces_and_keep_handler_identity() {
    let extracted = extract(
        "config/routes.rb",
        r#"
Rails.application.routes.draw do
  root "home#index"
  resources :orders
  post "/checkout", to: "orders#create"
  namespace :api do
    get "/orders", to: "orders#index"
  end
end
"#,
        SourceLanguage::Ruby,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "/ -> home#index",
        "resource:orders",
        "GET /orders",
        "POST /orders",
        "GET /orders/:id",
        "PATCH /orders/:id",
        "DELETE /orders/:id",
        "POST /checkout",
        "GET /api/orders",
    ] {
        assert!(routes.contains(&expected), "missing {expected}: {routes:?}");
    }
    for (name, resolution) in [
        ("index", "HomeController::index"),
        ("create", "OrdersController::create"),
        ("index", "Api::OrdersController::index"),
    ] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.name == name && reference.resolution_name.as_deref() == Some(resolution)
            }),
            "missing Rails handler lookup {resolution}: {:?}",
            extracted.references
        );
    }
}

#[test]
fn drupal_services_hooks_plugins_and_tags_are_graph_visible_without_source_reparse() {
    let services = extract(
        "modules/custom/demo/demo.services.yml",
        r"
services:
  _defaults:
    autowire: true
  demo.listener:
    class: Drupal\demo\Event\DemoListener
    arguments: ['@logger.factory', '@?demo.optional']
    tags:
      - { name: event_subscriber }
  demo.alias:
    alias: demo.listener
  demo.consumer:
    arguments:
      - !tagged_iterator event_subscriber
",
        SourceLanguage::Yaml,
    );
    for service in ["demo.listener", "demo.alias", "demo.consumer"] {
        assert!(
            services
                .symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Resource && symbol.name == service }),
            "missing Drupal service {service}: {services:?}"
        );
    }
    assert!(services.symbols.iter().all(|symbol| {
        symbol.name != "_defaults" || !symbol.qualified_name.contains("::drupal-service::")
    }));
    for reference in [
        "Drupal\\demo\\Event\\DemoListener",
        "logger.factory",
        "demo.optional",
        "demo.listener",
    ] {
        assert!(
            services
                .references
                .iter()
                .any(|candidate| candidate.name == reference),
            "missing Drupal service reference {reference}: {:?}",
            services.references
        );
    }
    for role in ["::drupal-tag-provider::", "::drupal-tag-consumer::"] {
        assert!(
            services.symbols.iter().any(|symbol| {
                symbol.name == "drupal-tag:event_subscriber" && symbol.qualified_name.contains(role)
            }),
            "missing Drupal tag role {role}: {:?}",
            services.symbols
        );
    }

    let hooks = extract(
        "modules/custom/demo/demo.module",
        r"<?php
/** @implements hook_form_alter(). */
function demo_form_alter(&$form) {}
function demo_help() {}
function unrelated_helper() {}
",
        SourceLanguage::Php,
    );
    for contract in ["hook_form_alter", "hook_help"] {
        assert!(
            hooks
                .symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Resource && symbol.name == contract }),
            "missing Drupal hook {contract}: {hooks:?}"
        );
    }
    assert!(
        hooks
            .symbols
            .iter()
            .all(|symbol| symbol.name != "hook_helper")
    );

    let plugins = extract(
        "modules/custom/demo/src/Plugin/Block/HeroBlock.php",
        r#"<?php
/** @Block(id = "hero_block") */
class HeroBlock {}

#[Block(id: 'modern_block')]
class ModernBlock {}
"#,
        SourceLanguage::Php,
    );
    for plugin in ["hero_block", "modern_block"] {
        assert!(
            plugins
                .symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Resource && symbol.name == plugin }),
            "missing Drupal plugin {plugin}: {plugins:?}"
        );
    }
}

/// A flow-mapping tag (`{ name: x, priority: 10 }`) ends its `name` at the
/// next comma, and a scalar `'@service:method'` factory names the service
/// once: the suffix after `:` is the factory method. In a `[@service, method]`
/// sequence the colon belongs to the service id, quoted scalars are decoded,
/// and a quote escape never truncates a value into a different name. A
/// non-ASCII service id is never cut to an ASCII prefix, and a rejected `factory` scalar names
/// nothing.
#[test]
fn drupal_service_flow_tags_and_scalar_factories_name_only_their_target() {
    let services = extract(
        "modules/custom/demo/demo.services.yml",
        r#"
services:
  demo.listener:
    class: Drupal\demo\Listener
    tags:
      - { name: event_subscriber, priority: 10 }
      - { name: 'cache.bin', default_backend: cache.backend.memory }
  demo.made:
    factory: '@demo.factory:create'
  demo.static:
    factory: 'Drupal\demo\Factory::create'
  demo.tenant:
    factory: ['@tenant:cache', 'create']
    arguments:
      - { factory: '@tenant:cache' }
  demo.escaped_factory:
    factory: "Drupal\\demo\\Escaped::create"
  demo.escaped:
    class: "Drupal\\demo\\Escaped"
    tags:
      - { name: 'kernel''listener' }
      - { name: "bad\tescape" }
  demo.unicode:
    factory: '@café:create'
    arguments: ['@café', '@plain', '@caf€']
  demo.unterminated:
    factory: '@svc:create
  demo.bad_escape:
    configurator: "@svc\x:configure"
"#,
        SourceLanguage::Yaml,
    );
    let tags = services
        .symbols
        .iter()
        .filter(|symbol| symbol.qualified_name.contains("::drupal-tag-provider::"))
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        tags,
        [
            "drupal-tag:kernel'listener",
            "drupal-tag:cache.bin",
            "drupal-tag:event_subscriber"
        ],
        "provider tags are ordered by service id"
    );
    let names_on = |line: u32| {
        services
            .references
            .iter()
            .filter(|reference| reference.span.start_line() == line)
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(names_on(9), ["demo.factory"]);
    assert_eq!(names_on(11), ["Drupal\\demo\\Factory"]);
    assert_eq!(names_on(13), ["tenant:cache"]);
    assert_eq!(names_on(15), ["tenant:cache"]);
    let escaped_factory = services
        .references
        .iter()
        .find(|reference| reference.span.start_line() == 17)
        .unwrap_or_else(|| panic!("missing escaped factory: {:?}", services.references));
    assert_eq!(escaped_factory.name, "Drupal\\demo\\Escaped");
    let raw_class = r"Drupal\\demo\\Escaped";
    assert_eq!(
        escaped_factory.span.end_byte() - escaped_factory.span.start_byte(),
        u64::try_from(raw_class.len()).unwrap_or(u64::MAX),
        "the span covers the raw escaped class text"
    );
    assert_eq!(names_on(19), ["Drupal\\demo\\Escaped"]);
    // A non-ASCII service id is never named as a shorter ASCII prefix
    // (`caf`) that would bind a different service; framework signals are
    // ASCII-only, so it abstains.
    assert_eq!(names_on(24), Vec::<&str>::new());
    assert_eq!(names_on(25), ["plain"]);
    // A rejected `factory`/`configurator` scalar names nothing.
    assert_eq!(names_on(27), Vec::<&str>::new());
    assert_eq!(names_on(29), Vec::<&str>::new());
}

#[test]
fn codeigniter_escaped_load_operands_abstain_before_resource_state() {
    for &(path, source) in escaped_names::ESCAPED_NAME_CASES {
        if path.starts_with("application/") {
            let file = credential_support::extract(path, source);
            credential_support::assert_no_credentials(&file);
            assert!(
                file.references
                    .iter()
                    .all(|reference| reference.name != "user_model")
            );
        }
    }
    let source = r"<?php class Users extends CI_Controller { function show() { $this->load->model('App\Models\user_model', 'users'); $this->users->find(); } }";
    let file = credential_support::extract("application/controllers/Users.php", source);
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == r"App\Models\user_model")
    );
}

#[test]
fn codeigniter_load_names_screen_credentials_before_resource_and_alias_projection() {
    for value in credential_support::CREDENTIAL_INPUTS {
        for (resource, alias) in [(value, "users"), ("user_model", value)] {
            let source = format!(
                "<?php class Users extends CI_Controller {{ public function show() {{ $this->load->model('{resource}', '{alias}'); $this->users->find(); }} }}\n"
            );
            let file = credential_support::extract("application/controllers/Users.php", &source);
            credential_support::assert_no_credentials(&file);
            assert!(
                file.references
                    .iter()
                    .all(|reference| reference.resolution_name.as_deref()
                        != Some("ci-loaded::model::User_model::find"))
            );
        }
    }
    credential_support::assert_screened(
        "application/controllers/Users.php",
        "<?php class Users extends CI_Controller { public function show() { $this->load->model('@VALUE@', 'users'); $this->users->find(); } }\n",
        "user_model",
    );
    let file = credential_support::extract(
        "application/controllers/Users.php",
        "<?php class Users extends CI_Controller { public function show() { $this->load->model('user_model', 'users'); $this->users->find(); } }\n",
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "find"
                && reference.resolution_name.as_deref()
                    == Some("ci-loaded::model::User_model::find"))
    );
}

#[test]
fn codeigniter_controller_routes_and_loaded_resource_calls_keep_convention_identity() {
    let extracted = extract(
        "application/controllers/admin/Users.php",
        r"<?php
class Users extends CI_Controller {
  public function index() {}
  public function show($id) {
    $this->load->model('user_model');
    $this->load->model('blog/queries', 'queryModel');
    $this->load->library('email');
    $this->user_model->active();
    $this->queryModel->find();
    $this->email->send();
    $this->db->get();
  }
  public function _remap($method) {}
  protected function hidden() {}
}
",
        SourceLanguage::Php,
    );
    let routes = extracted
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["ANY /admin/users", "ANY /admin/users/show"] {
        assert!(routes.contains(&expected), "missing {expected}: {routes:?}");
    }
    for absent in ["ANY /admin/users/_remap", "ANY /admin/users/hidden"] {
        assert!(!routes.contains(&absent), "unexpected {absent}: {routes:?}");
    }
    for (name, resolution) in [
        ("user_model", "ci-loaded::model::User_model"),
        ("blog/queries", "ci-loaded::model::blog/Queries"),
        ("email", "ci-loaded::library::Email"),
        ("active", "ci-loaded::model::User_model::active"),
        ("find", "ci-loaded::model::blog/Queries::find"),
        ("send", "ci-loaded::library::Email::send"),
    ] {
        assert!(
            extracted.references.iter().any(|reference| {
                reference.name == name && reference.resolution_name.as_deref() == Some(resolution)
            }),
            "missing CodeIgniter lookup {name} -> {resolution}: {:?}",
            extracted.references
        );
    }
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| reference.resolution_name.as_deref() != Some("Db::get"))
    );

    let routes_config = extract(
        "application/config/routes.php",
        "<?php\n$route['default_controller'] = 'welcome';\n$route['admin/users/show']['GET'] = 'admin/users/show';\n$route['translate_uri_dashes'] = FALSE;\n",
        SourceLanguage::Php,
    );
    for resolution in [
        "ci-route-root::Welcome::index",
        "ci-route-path::admin/users/show",
    ] {
        assert!(
            routes_config
                .references
                .iter()
                .any(|reference| { reference.resolution_name.as_deref() == Some(resolution) }),
            "missing CodeIgniter route lookup {resolution}: {:?}",
            routes_config.references
        );
    }
    assert!(routes_config.symbols.iter().all(|symbol| {
        symbol.kind != SymbolKind::Route || !symbol.name.contains("translate_uri_dashes")
    }));
}

#[test]
fn framework_enrichment_honors_cancellation() {
    let fixture = &ROUTES[0];
    let snapshot = snapshot(fixture.path, fixture.source, fixture.language);
    let mut extractor = NativeExtractor::new(fixture.language)
        .unwrap_or_else(|error| panic!("framework extractor failed: {error}"));
    let mut polls = 0_u8;
    assert_eq!(
        extractor.extract_with_cancellation(&snapshot, || {
            polls = polls.saturating_add(1);
            polls > 2
        }),
        Err(ExtractError::Cancelled)
    );
}

#[test]
fn dependency_manifests_are_graph_visible_workspace_aware_and_literal_safe() {
    let package = extract(
        "package.json",
        r#"{
  "name": "@acme/app",
  "workspaces": { "packages": ["packages/*"] },
  "dependencies": {
    "@acme/core": "workspace:*",
    "react": "cartograph_literal_secret_sentinel_7c1f"
  },
  "devDependencies": { "vitest": "^3" },
  "tool": { "dependencies": { "nested-false-positive": "1" } }
}"#,
        SourceLanguage::Json,
    );
    for expected in [
        "@acme/app",
        "npm dependency @acme/core",
        "npm dependency react",
        "npm dependency vitest",
        "npm workspace member packages/*",
    ] {
        assert!(
            package.symbols.iter().any(|symbol| symbol.name == expected),
            "missing package signal {expected}: {:?}",
            package.symbols
        );
    }
    assert!(
        package
            .symbols
            .iter()
            .all(|symbol| !symbol.name.contains("nested-false-positive"))
    );
    assert!(package.references.iter().any(|reference| {
        reference.name == "@acme/core" && reference.kind == ReferenceKind::References
    }));
    assert!(package.symbols.iter().any(|symbol| {
        symbol.name == "@acme/app" && symbol.qualified_name.ends_with("::manifest-dir::__root__")
    }));
    assert!(package.symbols.iter().any(|symbol| {
        symbol.name == "npm workspace" && symbol.qualified_name.ends_with("::__root__")
    }));

    let composer = extract(
        "services/api/composer.json",
        r#"{
  "name": "acme/api",
  "require": { "php": "^8.4", "acme/domain": "dev-main" },
  "require-dev": { "phpunit/phpunit": "^12" }
}"#,
        SourceLanguage::Json,
    );
    for expected in ["php", "acme/domain", "phpunit/phpunit"] {
        assert!(
            composer
                .references
                .iter()
                .any(|reference| reference.name == expected),
            "missing Composer dependency {expected}: {:?}",
            composer.references
        );
    }

    let cargo = extract(
        "Cargo.toml",
        r#"[package]
name = "cartograph"
version = "cartograph_literal_secret_sentinel_7c1f"

[workspace]
members = ["crates/*"]

[dependencies]
tokio = "1"
serde_alias = { package = "serde", version = "1" }

[workspace.dependencies]
sqlx = "0.8"

[target.'cfg(unix)'.dependencies]
nix = "0.30"
"#,
        SourceLanguage::Toml,
    );
    for expected in ["cartograph", "cargo workspace member crates/*"] {
        assert!(
            cargo.symbols.iter().any(|symbol| symbol.name == expected),
            "missing Cargo signal {expected}: {:?}",
            cargo.symbols
        );
    }
    for expected in ["tokio", "sqlx", "nix"] {
        assert!(
            cargo
                .references
                .iter()
                .any(|reference| reference.name == expected),
            "missing Cargo dependency {expected}: {:?}",
            cargo.references
        );
    }
    assert!(cargo.references.iter().any(|reference| {
        reference.name == "serde_alias" && reference.resolution_name.as_deref() == Some("serde")
    }));
    assert!(cargo.symbols.iter().any(|symbol| {
        symbol.name == "cartograph" && symbol.qualified_name.ends_with("::manifest-dir::__root__")
    }));
    assert!(cargo.symbols.iter().any(|symbol| {
        symbol.name == "cargo workspace" && symbol.qualified_name.ends_with("::__root__")
    }));

    for extracted in [&package, &composer, &cargo] {
        assert!(!format!("{extracted:?}").contains(SECRET));
    }
}

#[test]
fn component_framework_builtins_stores_and_template_boundaries_are_precise() {
    let vue = extract(
        "pages/index.vue",
        r#"<script setup lang="ts">
import { useRoute } from '#imports'
const props = defineProps<{ message: string }>()
</script>
<template><OrderCard @click="submitOrder()" />{{ formatOrder(order) }}</template>
<style>.fake { content: "{{ styleGhost() }}"; }</style>
"#,
        SourceLanguage::Vue,
    );
    let vue_component = vue
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Component && symbol.name == "index")
        .unwrap_or_else(|| panic!("missing Vue component: {:?}", vue.symbols));
    assert!(vue_component.export.exported && vue_component.export.default_export);
    assert!(
        vue.symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Resource && symbol.name == "#imports" })
    );
    assert!(
        vue.references
            .iter()
            .all(|reference| { !matches!(reference.name.as_str(), "defineProps" | "styleGhost") })
    );
    for expected in ["OrderCard", "submitOrder", "formatOrder"] {
        assert!(
            vue.references
                .iter()
                .any(|reference| reference.name == expected),
            "missing Vue reference {expected}: {:?}",
            vue.references
        );
    }

    let svelte = extract(
        "src/routes/+page.svelte",
        r#"<script>
import { goto } from '$app/navigation';
let count = 0;
const doubled = $count * 2;
const rune = $state(0);
</script>
<button on:click="increment()">{$count}</button>
<style>.fake { content: "$styleGhost"; }</style>
"#,
        SourceLanguage::Svelte,
    );
    let stores = svelte
        .references
        .iter()
        .filter(|reference| reference.name == "$count")
        .collect::<Vec<_>>();
    assert_eq!(stores.len(), 2, "Svelte store sites drifted: {stores:?}");
    assert!(stores.iter().all(|reference| {
        reference.resolution_name.as_deref() == Some("count")
            && reference.kind == ReferenceKind::References
    }));
    assert!(
        svelte.symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Resource && symbol.name == "$app/navigation"
        })
    );
    assert!(
        svelte
            .references
            .iter()
            .all(|reference| { !matches!(reference.name.as_str(), "$state" | "$styleGhost") })
    );
}

#[test]
fn spring_conditional_on_property_keys_are_owned_by_the_annotated_declaration() {
    let java = extract(
        "src/PaymentsAutoConfig.java",
        "import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;\n\n@ConditionalOnProperty(prefix = \"feature.payments.\", name = \"enabled\", havingValue = \"cartograph_literal_secret_sentinel_7c1f\")\npublic class PaymentsAutoConfig {\n  @Bean\n  @ConditionalOnProperty(\"feature.audit\")\n  public Audit audit() { return null; }\n  @ConditionalOnProperty(value = \".feature.legacy\", matchIfMissing = true)\n  public Legacy legacy() { return null; }\n  @ConditionalOnProperty(name = {\"feature.array\"})\n  public Arrayed arrayed() { return null; }\n  @Value(\"${app.cache.ttl}\")\n  private int cacheTtl;\n  @Value(\"${app.pair:1}\") String left, right;\n  @Value /* documented */ (\"${app.noted}\") private String noted;\n  @ConditionalOnProperty(prefix = PREFIX, name = \"hidden\")\n  public Hidden hidden() { return null; }\n}\n",
        SourceLanguage::Java,
    );
    let owner_of = |name: &str| {
        java.references
            .iter()
            .filter(|reference| {
                reference.kind == ReferenceKind::References && reference.name == name
            })
            .map(|reference| {
                reference.owner.as_ref().and_then(|owner| {
                    java.symbols
                        .iter()
                        .find(|symbol| &symbol.id == owner)
                        .map(|symbol| symbol.qualified_name.as_str())
                })
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        owner_of("feature.payments.enabled"),
        vec![Some("PaymentsAutoConfig")]
    );
    assert_eq!(
        owner_of("feature.audit"),
        vec![Some("PaymentsAutoConfig::audit")]
    );
    assert_eq!(
        owner_of("feature.legacy"),
        vec![Some("PaymentsAutoConfig::legacy")]
    );
    assert_eq!(
        owner_of("app.cache.ttl"),
        vec![Some("PaymentsAutoConfig::cacheTtl")],
        "@Value keys belong to the annotated field"
    );
    assert_eq!(
        owner_of("app.pair"),
        vec![
            Some("PaymentsAutoConfig::left"),
            Some("PaymentsAutoConfig::right")
        ],
        "every declarator of a multi-declarator @Value field depends on the key"
    );
    assert_eq!(
        owner_of("app.noted"),
        vec![Some("PaymentsAutoConfig::noted")],
        "a comment between @Value and its arguments keeps the field owner"
    );
    assert!(
        owner_of("hidden").is_empty(),
        "a non-literal prefix makes the key unknown"
    );
    for absent in ["feature.array", "true", "enabled", "feature.payments."] {
        assert!(
            owner_of(absent).is_empty(),
            "unexpected configuration key {absent}: {:?}",
            java.references
        );
    }
    assert!(!format!("{java:?}").contains(SECRET));

    let kotlin = extract(
        "src/Payments.kt",
        "@ConditionalOnProperty(prefix = \"feature.payments\", name = \"enabled\")\nclass PaymentsAutoConfig\n@ConditionalOnProperty(prefix = \"$ROOT.flags\", name = \"templated\")\nclass Templated\n",
        SourceLanguage::Kotlin,
    );
    assert!(kotlin.references.iter().any(|reference| {
        reference.kind == ReferenceKind::References && reference.name == "feature.payments.enabled"
    }));
    assert!(
        kotlin
            .references
            .iter()
            .all(|reference| !reference.name.contains("templated")),
        "a Kotlin string template is not a literal prefix: {:?}",
        kotlin.references
    );

    let unrelated = extract(
        "src/Other.java",
        "@ConditionalOnBean(name = \"feature.payments\")\nclass Other {}\n",
        SourceLanguage::Java,
    );
    assert!(
        unrelated
            .references
            .iter()
            .all(|reference| reference.kind != ReferenceKind::References),
        "only ConditionalOnProperty names configuration keys: {:?}",
        unrelated.references
    );
}

#[test]
fn mybatis_template_statement_ids_reference_mapper_statements_from_the_calling_method() {
    let extracted = extract(
        "src/main/java/com/example/order/dao/impl/OrderAttributeDaoImpl.java",
        "package com.example.order.dao.impl;\n\nimport com.example.order.dao.OrderAttributeDao;\n\npublic class OrderAttributeDaoImpl {\n  private static final String SQL_NS = OrderAttributeDao.class.getName() + \"Mapper\";\n  private static final String ORDER_NS = \"com.example.OrderMapper\";\n\n  public void deleteByOrderId(String orderId) {\n    getSqlSessionTemplate().delete(SQL_NS + \".deleteByOrderId\", orderId);\n  }\n\n  public Object find(String id) {\n    Object row = sqlSessionTemplate.selectOne(ORDER_NS + \".findOrder\", id);\n    this.sqlSessionTemplate.selectList(\"ns.Literal.findAll\");\n    sqlSessionTemplate.update(dynamicNamespace() + \".skip\", id);\n    sqlSessionTemplate.insert(unknown + \".skip\", id);\n    // sqlSessionTemplate.delete(\"Commented.out\");\n    return row;\n  }\n}\n",
        SourceLanguage::Java,
    );
    let owned = |owner: &str| {
        let owner = extracted
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == owner)
            .unwrap_or_else(|| panic!("missing {owner}: {:?}", extracted.symbols));
        extracted
            .references
            .iter()
            .filter(|reference| {
                reference.kind == ReferenceKind::References
                    && reference.owner.as_ref() == Some(&owner.id)
            })
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        owned("com.example.order.dao.impl::OrderAttributeDaoImpl::deleteByOrderId"),
        vec!["OrderAttributeDaoMapper::deleteByOrderId"]
    );
    assert_eq!(
        owned("com.example.order.dao.impl::OrderAttributeDaoImpl::find"),
        vec!["OrderMapper::findOrder", "Literal::findAll"]
    );
    assert!(
        extracted
            .references
            .iter()
            .all(|reference| !reference.name.ends_with("::skip")
                && reference.name != "Commented::out"),
        "unevaluable or commented statement ids must not be guessed: {:?}",
        extracted.references
    );

    let ambiguous = extract(
        "src/main/java/com/example/AmbiguousDao.java",
        "package com.example;\npublic class AmbiguousDao {\n  private String reassigned = \"first.Mapper\";\n  private static final String SHADOWED = \"a.Mapper\";\n  private static final String KNOWN = \"ok.Mapper\";\n  void reset() { reassigned = \"second.Mapper\"; }\n  void load() {\n    sqlSessionTemplate.selectOne(reassigned + \".find\");\n    sqlSessionTemplate.selectOne(SHADOWED + \".find\");\n    String doc = \"sqlSessionTemplate.selectOne(KNOWN + \\\".fake\\\")\";\n    sqlSessionTemplate.selectOne(KNOWN + \".real\");\n  }\n  class Inner { private static final String SHADOWED = \"b.Mapper\"; }\n}\n",
        SourceLanguage::Java,
    );
    let statements = ambiguous
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        statements,
        vec!["Mapper::real"],
        "reassigned or shadowed constants and calls inside string literals abstain"
    );

    let scoped = extract(
        "src/main/java/com/example/ScopedDao.java",
        "package com.example;\npublic class ScopedDao {\n  private static final String NS = \"field.Mapper\";\n  void initialize() {\n    final String LOCAL = \"LocalMapper\";\n  }\n  Object load(String NS) {\n    return sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n  Object local() {\n    return sqlSessionTemplate.selectOne(LOCAL + \".find\");\n  }\n  Object quoted() {\n    String block = \"\"\"\n      sqlSessionTemplate.selectOne(\"Block.fake\");\n      \"\"\";\n    char quote = '\"'; return sqlSessionTemplate.selectOne(\"Quoted.real\");\n  }\n  Object annotated(@Param(\"ns\") String NS) {\n    return sqlSessionTemplate.selectOne((NS) + \".find\");\n  }\n  void loop(java.util.List<String> names) {\n    for (String NS : names) sqlSessionTemplate.selectOne(NS + \".find\");\n    names.forEach(NS -> sqlSessionTemplate.selectOne(NS + \".find\"));\n  }\n  Object fieldUse() {\n    return sqlSessionTemplate.selectOne(NS + \".ok\");\n  }\n}\n",
        SourceLanguage::Java,
    );
    let statements = scoped
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        statements,
        vec!["Quoted::real", "Mapper::ok"],
        "parameters shadow fields, locals never answer for other methods, text blocks are literals, and a char literal quote does not hide a real call"
    );
}

#[test]
fn mybatis_lambda_generic_arguments_do_not_shadow_statement_constants() {
    let source = "class GenericDao {\n  static final String NS = \"pkg.Mapper\";\n  static class NS {}\n  void load() {\n    Consumer<Map<String, NS>> c = (Map<String, NS> values) -> sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n}\n";
    let extracted = extract("src/GenericDao.java", source, SourceLanguage::Java);
    let statements = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(statements, ["Mapper::find"]);
}

#[test]
fn mybatis_lambda_type_names_do_not_shadow_statement_constants() {
    let source = "class TypeDao {\n  static final String Map = \"pkg.OtherMapper\";\n  void load() {\n    Consumer<Map<String, Integer>> c = (Map<String, Integer> values) -> sqlSessionTemplate.selectOne(Map + \".find\");\n  }\n}\n";
    let extracted = extract("src/TypeDao.java", source, SourceLanguage::Java);
    let statements = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(statements, ["OtherMapper::find"]);
}

#[test]
fn mybatis_switch_constant_labels_keep_statement_constants() {
    for label in ["NS", "ConstantSwitchDao.NS", "NS, OTHER", "\"other\""] {
        let source = format!(
            "class ConstantSwitchDao {{\n  static final String NS = \"pkg.Mapper\";\n  static final String OTHER = \"other\";\n  void load(String input) {{\n    switch (input) {{\n      case {label} -> sqlSessionTemplate.selectOne(NS + \".find\");\n      default -> sqlSessionTemplate.selectOne(NS + \".fallback\");\n    }}\n  }}\n}}\n"
        );
        let extracted = extract("src/ConstantSwitchDao.java", &source, SourceLanguage::Java);
        let statements = extracted
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::References)
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(statements, ["Mapper::find", "Mapper::fallback"], "{label}");
    }
}

#[test]
fn mybatis_switch_type_patterns_and_real_lambdas_shadow_statement_constants() {
    for arm in [
        "case String NS -> sqlSessionTemplate.selectOne(NS + \".find\");",
        "case final String NS -> sqlSessionTemplate.selectOne(NS + \".find\");",
        "case @Marker(value = {1, 2}) final java.lang.String NS -> sqlSessionTemplate.selectOne(NS + \".find\");",
        "case String value -> map.forEach(NS -> sqlSessionTemplate.selectOne(NS + \".find\"));",
    ] {
        let source = format!(
            "class PatternSwitchDao {{\n  static final String NS = \"pkg.Mapper\";\n  void load(Object input) {{ switch (input) {{ {arm} default -> {{}} }} }}\n  void field() {{ sqlSessionTemplate.selectOne(NS + \".real\"); }}\n}}\n"
        );
        let extracted = extract("src/PatternSwitchDao.java", &source, SourceLanguage::Java);
        let statements = extracted
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::References)
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(statements, ["Mapper::real"], "{arm}");
    }
}

#[test]
fn mybatis_switch_record_patterns_abstain_for_statement_constants() {
    let source = "class RecordSwitchDao {\n  static final String NS = \"pkg.Mapper\";\n  record Box(String value) {}\n  void load(Object input) {\n    switch (input) {\n      case Box(String value) -> sqlSessionTemplate.selectOne(NS + \".find\");\n      default -> {}\n    }\n  }\n  void field() { sqlSessionTemplate.selectOne(NS + \".real\"); }\n}\n";
    let extracted = extract("src/RecordSwitchDao.java", source, SourceLanguage::Java);
    let statements = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(statements, ["Mapper::real"]);
}

#[test]
fn mybatis_nested_switch_record_patterns_shadow_statement_constants() {
    let source = "class SwitchDao {\n  static final String NS = \"pkg.Mapper\";\n  record Inner(String value) {}\n  record Outer(Inner value) {}\n  void load(Object input) {\n    switch (input) {\n      case Outer(Inner(String NS)) -> sqlSessionTemplate.selectOne(NS + \".find\");\n      default -> {}\n    }\n  }\n  void field() { sqlSessionTemplate.selectOne(NS + \".real\"); }\n}\n";
    let extracted = extract("src/SwitchDao.java", source, SourceLanguage::Java);
    let statements = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(statements, ["Mapper::real"]);
}

#[test]
fn mybatis_varargs_parameters_shadow_statement_constants() {
    for parameter in [
        "String... NS",
        "java.lang.String /* type */ ... NS",
        "String[]... NS",
        "Façade... NS",
    ] {
        let source = format!(
            "class VarargsDao {{\n  static final String NS = \"pkg.Mapper\";\n  void load({parameter}) {{ sqlSessionTemplate.selectOne(NS + \".find\"); }}\n  void field() {{ sqlSessionTemplate.selectOne(NS + \".real\"); }}\n}}\n"
        );
        let extracted = extract("src/VarargsDao.java", &source, SourceLanguage::Java);
        let statements = extracted
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::References)
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(statements, ["Mapper::real"], "{parameter}");
    }
}

#[test]
fn mybatis_large_argument_lists_keep_unshadowed_statement_constants() {
    let arguments = vec!["NS"; 16_000].join(",");
    let source = format!(
        "class LargeDao {{\n  static final String NS = \"pkg.Mapper\";\n  void load() {{ consume({arguments}); sqlSessionTemplate.selectOne(NS + \".find\"); }}\n  void shadowed() {{ map.forEach((NS, value) -> sqlSessionTemplate.selectOne(NS + \".fake\")); }}\n}}\n"
    );
    let extracted = extract("src/LargeDao.java", &source, SourceLanguage::Java);
    let statements = extracted
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::References)
        .map(|reference| reference.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(statements, ["Mapper::find"]);
}

#[test]
fn mybatis_inferred_lambda_parameters_shadow_statement_constants() {
    for parameters in [
        "NS, value",
        "key, NS, value",
        "key, NS",
        "NS, café",
        "key, NS, café",
        "café, NS",
        "NS /* first */,\n value",
        "var NS, var value",
        "String NS, String value",
        "Map<String, Integer> NS, String value",
        "Map<String, Integer> values, String NS",
        "@Marker(value = {1, 2}) final Map<String, Integer> NS, String value",
        "String NS[], String value",
    ] {
        let source = format!(
            "class LambdaDao {{\n  static final String NS = \"pkg.Mapper\";\n  void shadowed() {{ map.forEach(({parameters}) -> sqlSessionTemplate.selectOne(NS + \".find\")); }}\n  void field() {{ map.forEach((key, value) -> sqlSessionTemplate.selectOne(NS + \".real\")); }}\n}}\n"
        );
        let extracted = extract("src/LambdaDao.java", &source, SourceLanguage::Java);
        let statements = extracted
            .references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::References)
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(statements, ["Mapper::real"], "{parameters}");
    }
}

#[test]
fn mybatis_statement_ids_screen_credentials_before_namespace_projection() {
    for value in credential_support::CREDENTIAL_INPUTS {
        let source = format!(
            "class Dao {{ void run() {{ sqlSessionTemplate.selectOne(\"{value}.Mapper.token\"); }} }}\n"
        );
        let file = credential_support::extract("Dao.java", &source);
        credential_support::assert_no_credentials(&file);
        assert!(
            file.references
                .iter()
                .all(|reference| reference.name != "Mapper::token")
        );
    }
    credential_support::assert_screened(
        "Dao.java",
        "class Dao { void run() { sqlSessionTemplate.selectOne(\"@VALUE@\"); } }\n",
        "Mapper::token",
    );
    let file = credential_support::extract(
        "Dao.java",
        "class Dao { void run() { sqlSessionTemplate.selectOne(\"pkg.Mapper.token\"); } }\n",
    );
    assert!(
        file.references
            .iter()
            .any(|reference| reference.name == "Mapper::token"
                && reference.kind == ReferenceKind::References)
    );
}

#[test]
fn mybatis_template_scanning_is_literal_and_unicode_safe() {
    let statements = |path: &str, source: &str| {
        extract(path, source, SourceLanguage::Java)
            .references
            .into_iter()
            .filter(|reference| reference.kind == ReferenceKind::References)
            .map(|reference| reference.name)
            .collect::<Vec<_>>()
    };
    // A multibyte identifier ending in a constant's name neither panics nor
    // shadows it; a non-ASCII type before the name is a shadowing local.
    assert_eq!(
        statements(
            "src/main/java/UnicodeDao.java",
            "class UnicodeDao {\n  static final String NS = \"a.Mapper\";\n  void f() { String caféNS; sqlSessionTemplate.selectOne(NS + \".find\"); }\n  void g() { Façade NS; sqlSessionTemplate.selectOne(NS + \".shadowed\"); }\n  void h() { sqlSessionTemplate.selectOne(\"é.Ünicode\" + \".x\"); }\n}\n",
        ),
        vec!["Mapper::find"],
    );
    // Declarations and bindings spelled inside literals are not code: a
    // text-block line never defines a constant, and a string mentioning
    // `String KNOWN` does not shadow the field.
    let leaked = statements(
        "src/main/java/BlockDao.java",
        "class BlockDao extends BaseDao {\n  static final String DOC = \"\"\"\n    final String NS = \"literal_secret_sentinel\";\n    \"\"\";\n  static final String KNOWN = \"known.KnownMapper\";\n  void load() {\n    sqlSessionTemplate.selectOne(NS + \".find\");\n    String doc = \"String KNOWN\";\n    sqlSessionTemplate.selectOne(KNOWN + \".real\");\n  }\n}\n",
    );
    assert_eq!(leaked, vec!["KnownMapper::real"]);
    assert!(leaked.iter().all(|name| !name.contains("sentinel")));
    // v1 accepts non-final fields assigned exactly once; a local declared in
    // a one-line method body is still not a class-level constant.
    assert_eq!(
        statements(
            "src/main/java/LegacyDao.java",
            "class LegacyDao {\n  private static String LEGACY = \"legacy.LegacyMapper\";\n  void init() { String LOCAL = \"LocalMapper\"; }\n  void load() {\n    sqlSessionTemplate.selectOne(LEGACY + \".find\");\n    sqlSessionTemplate.selectOne(LOCAL + \".skip\");\n  }\n}\n",
        ),
        vec!["LegacyMapper::find"],
    );
    // A local of a static or instance initializer block lies outside every
    // method but is not a field: it never answers for the field it shadows.
    for (path, block) in [
        ("src/main/java/StaticInit.java", "static"),
        ("src/main/java/InstanceInit.java", ""),
    ] {
        let source = format!(
            "class Init {{\n  static String NS;\n  {block} {{\n    String NS = \"fake.FakeMapper\";\n  }}\n  void f() {{\n    sqlSessionTemplate.selectOne(NS + \".find\");\n  }}\n}}\n"
        );
        assert!(statements(path, &source).is_empty(), "{path}");
    }
    // Java translates `\uXXXX` before it finds comments, literals, or
    // identifiers, so an escape can open a text block, end a comment, or
    // respell an identifier (even with an ignorable non-ASCII character):
    // any file containing one abstains, and nothing it spells is stored. A
    // literal ignorable character in code respells identifiers the same way.
    for (path, source) in [
        (
            "src/main/java/EscapedBlock.java",
            "class C {\n  static String NS;\n  static final String DOC = \\u0022\\u0022\\u0022\n    final String NS = \"literal_secret_sentinel\";\n    \\u0022\\u0022\\u0022;\n  void f() {\n    sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n}\n",
        ),
        (
            "src/main/java/EscapedAssignment.java",
            "class C {\n  static String NS = \"a.Mapper\";\n  void f() {\n    N\\u0053 = \"b.OtherMapper\";\n    sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n}\n",
        ),
        (
            "src/main/java/EscapedComment.java",
            "class C {\n  static String NS = \"a.Mapper\";\n  void f() {\n    //\\u000a NS = \"b.OtherMapper\";\n    sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n}\n",
        ),
        (
            "src/main/java/EscapedIgnorable.java",
            "class C {\n  static String NS = \"a.Mapper\";\n  void f() {\n    N\\u200bS = \"b.OtherMapper\";\n    sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n}\n",
        ),
        (
            "src/main/java/LiteralIgnorable.java",
            "class C {\n  static String NS = \"a.Mapper\";\n  void f() {\n    N\u{200b}S = \"b.OtherMapper\";\n    sqlSessionTemplate.selectOne(NS + \".find\");\n  }\n}\n",
        ),
    ] {
        assert!(statements(path, source).is_empty(), "{path}");
    }
    // A field's initializer can contain an anonymous class whose initializer
    // block declares a same-named local; that local is not the field.
    assert_eq!(
        statements(
            "src/main/java/AnonymousInit.java",
            "class C {\n  String NS[] = { new Object() {\n    {\n      String NS = \"fake.FakeMapper\";\n    }\n  }.toString() };\n  void f() { sqlSessionTemplate.selectOne(NS + \".find\"); }\n}\n",
        ),
        Vec::<String>::new()
    );
}

fn extract(
    path: &str,
    source: &str,
    language: SourceLanguage,
) -> cartograph_extract::ExtractedFile {
    let snapshot = snapshot(path, source, language);
    let mut extractor = NativeExtractor::new(language)
        .unwrap_or_else(|error| panic!("{path} extractor failed: {error}"));
    extractor
        .extract(&snapshot)
        .unwrap_or_else(|error| panic!("{path} extraction failed: {error}"))
}

fn snapshot(path: &str, source: &str, language: SourceLanguage) -> SourceSnapshot {
    let snapshot = SourceSnapshot::from_bytes(path, source.as_bytes(), limits())
        .unwrap_or_else(|error| panic!("{path} snapshot failed: {error}"));
    assert_eq!(snapshot.language(), language, "{path}");
    snapshot
}

fn limits() -> SourceLimits {
    SourceLimits::new(SOURCE_LIMIT)
        .unwrap_or_else(|error| panic!("framework source limit failed: {error}"))
}
