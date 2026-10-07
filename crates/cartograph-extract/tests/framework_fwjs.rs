//! Wave 3 framework detail through the real native extractor.
mod dependency_ownership;

use cartograph_domain::{ReferenceKind, SourceLanguage, SymbolKind};
use cartograph_extract::{ExtractedFile, NativeExtractor, SourceLimits, SourceSnapshot};

const SOURCE_BYTES: usize = 1_048_576;

fn extract(path: &str, source: &str) -> ExtractedFile {
    let snapshot = SourceSnapshot::from_bytes_for_capability_validation(
        path,
        source.as_bytes(),
        SourceLimits::new(SOURCE_BYTES).unwrap_or_else(|error| panic!("source limits: {error}")),
    )
    .unwrap_or_else(|error| panic!("fixture snapshot {path}: {error}"));
    NativeExtractor::new_for_capability_validation(snapshot.language())
        .and_then(|mut extractor| extractor.extract(&snapshot))
        .unwrap_or_else(|error| panic!("fixture extraction {path}: {error}"))
}

fn routes(file: &ExtractedFile) -> Vec<&str> {
    let mut symbols = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Route)
        .collect::<Vec<_>>();
    symbols.sort_by_key(|symbol| symbol.span.start_byte());
    symbols
        .into_iter()
        .map(|symbol| symbol.name.as_str())
        .collect()
}

#[test]
fn angular_arrays_keep_each_route_and_its_component_and_lazy_imports() {
    let file = extract(
        "src/app.routes.ts",
        "import { Routes, provideRouter } from '@angular/router';\nclass Home {}\nconst routes: Routes = [\n { path : '', component : Home },\n { path: 'admin', loadChildren: () => import('./admin') },\n { path: 'settings', loadComponent: () => import('./settings').then(m => m.Settings) }\n];\nprovideRouter([{path: 'dash', component: Home}]);\nconst other = {path: dynamicPath, component: Home};\nconst text = \"{path: 'fake'}\";\n",
    );
    for route in ["/", "/admin", "/settings", "/dash"] {
        assert!(routes(&file).contains(&route));
    }
    for (route, name, kind, line) in [
        ("/", "Home", ReferenceKind::References, 4),
        ("/admin", "./admin", ReferenceKind::Imports, 5),
        ("/settings", "./settings", ReferenceKind::Imports, 6),
        ("/settings", "Settings", ReferenceKind::References, 6),
    ] {
        let owner = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Route && symbol.name == route)
            .unwrap_or_else(|| panic!("required framework fact is missing"));
        assert_eq!(
            owner.signature.as_deref(),
            Some(format!("Angular route {route}").as_str())
        );
        assert!(
            file.references
                .iter()
                .any(|reference| reference.owner.as_ref() == Some(&owner.id)
                    && reference.name == name
                    && reference.kind == kind
                    && reference.span.start_line() == line)
        );
    }
    assert_eq!(
        routes(&extract(
            "plain.ts",
            "const value = {path: 'fake', component: Home};"
        )),
        Vec::<&str>::new()
    );
    let nested = extract(
        "child.routes.ts",
        "const routes: Routes = [{path: 'parent', children: [{path:'child'}], data: [{path:'metadata'}], loadChildren: () => [{path:'returned'}]}];",
    );
    for path in ["/parent", "/child"] {
        assert!(routes(&nested).contains(&path));
    }
    for path in ["/metadata", "/returned"] {
        assert!(!routes(&nested).contains(&path));
    }
}

#[test]
fn bun_method_keys_and_nested_calls_preserve_routes_without_object_guesses() {
    let file = extract(
        "server.ts",
        "function h() {} function p() {}\nBun.serve({routes:{'/q':{'GET':h,\"POST\":p},'/d':{description:'x', GET:h}, '/object':{body:'x'}}});\nBun.serve({fetch(){return Bun.serve({routes:{'/inner':h}})}});\n",
    );
    assert_eq!(routes(&file), ["GET /q", "POST /q", "GET /d", "ANY /inner"]);
    assert!(
        file.references
            .iter()
            .any(|reference| reference.kind == ReferenceKind::Calls
                && reference.name == "p"
                && reference.owner.is_some())
    );
    assert_eq!(
        routes(&extract(
            "quoted.ts",
            "const s = \"Bun.serve({routes:{'/fake':h}})\";"
        )),
        Vec::<&str>::new()
    );
}

#[test]
fn cli_registers_every_valid_javascript_command_and_keeps_safe_specs() {
    let file = extract(
        "cli.ts",
        "program.command('init [path]').command('hard');\nsetup(program\n .command('serve <port>'), program.command('stop'));\ndb.command('SELECT * FROM t'); program.command('-bad'); program.command('a.b');\nconst text = \"program.command('fake')\";\n",
    );
    assert_eq!(
        routes(&file),
        ["cmd init", "cmd hard", "cmd serve", "cmd stop"]
    );
    assert_eq!(
        file.symbols
            .iter()
            .find(|symbol| symbol.name == "cmd init")
            .unwrap_or_else(|| panic!("required framework fact is missing"))
            .signature
            .as_deref(),
        Some("init [path]")
    );
    assert_eq!(
        routes(&extract("db.py", "client.admin.command('ping')\n")),
        Vec::<&str>::new()
    );
}

#[test]
fn express_local_route_modules_require_receiver_and_literal_path() {
    let file = extract(
        "routes/users.js",
        "const router = require('../lib/router'); const app = require('../app');\nfunction list() {}\nrouter.get('/users', list); app.post('/login', list); app.use('/static', list);\nclient.get('/no', list); router.use(list);\n",
    );
    assert_eq!(routes(&file), ["GET /users", "POST /login", "USE /static"]);
    assert_eq!(
        routes(&extract(
            "commented.js",
            "// express routes\nrouter.get('/commented', list);"
        )),
        ["GET /commented"]
    );
    assert_eq!(
        routes(&extract(
            "plain.js",
            "client.get('/no', h); const s = \"app.get('/fake', h)\";"
        )),
        Vec::<&str>::new()
    );
}

#[test]
fn file_routes_preserve_index_directories_and_embedded_params() {
    for (path, source, route, line) in [
        (
            "pages/index/detail.tsx",
            "import React from 'react';\n\nexport default function Detail() {}",
            "/index/detail",
            3,
        ),
        (
            "pages/user-[id].tsx",
            "\nexport default function User() {}",
            "/user-:id",
            2,
        ),
        ("src/routes/index/+page.svelte", "<p>hi</p>", "/index", 1),
        (
            "server/api/users.cjs",
            "module.exports = () => {};",
            "/api/users",
            1,
        ),
    ] {
        let file = extract(path, source);
        let symbol = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Route)
            .unwrap_or_else(|| panic!("required framework fact is missing"));
        assert_eq!(symbol.name, route);
        assert_eq!(symbol.span.start_line(), line);
    }
    assert_eq!(
        routes(&extract(
            "pages/_app.tsx",
            "export default function App() {}"
        )),
        Vec::<&str>::new()
    );
    let middleware = extract(
        "middleware/auth.cjs",
        "function auth() {}\nmodule.exports = auth;",
    );
    assert_eq!(
        middleware
            .symbols
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Function && symbol.name == "auth")
            .count(),
        1
    );
    let quoted = extract(
        "pages/quoted.tsx",
        "const s = 'export default';\nexport default function Page() {}",
    );
    assert_eq!(
        quoted
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Route)
            .unwrap_or_else(|| panic!("required framework fact is missing"))
            .span
            .start_line(),
        2
    );
    assert_eq!(
        routes(&extract("pages/fake.tsx", "const s = 'export default';")),
        Vec::<&str>::new()
    );
}

#[test]
fn hono_wrapped_and_nested_receivers_keep_child_sites_and_slashes() {
    let file = extract(
        "routes.ts",
        "import {Hono} from 'hono';\nconst app = new Hono(); const child = wrap(new Hono());\nfunction h() {}\napp.get('/a', () => app.get('/b', h));\nchild.get('/c//', h);\napp.route('/api/', child);\nobj.api = new Hono(); obj.api.get('/wrong', h);\n",
    );
    assert_eq!(
        routes(&file),
        ["GET /a", "GET /b", "GET /c//", "GET /api/c//"]
    );
    let mounted = file
        .symbols
        .iter()
        .find(|symbol| symbol.name == "GET /api/c//")
        .unwrap_or_else(|| panic!("required framework fact is missing"));
    assert_eq!(mounted.span.start_line(), 5);
    assert_eq!(
        routes(&extract(
            "plain.ts",
            "obj.api = new Hono(); obj.api.get('/wrong', h);"
        )),
        Vec::<&str>::new()
    );
    assert_eq!(
        routes(&extract(
            "fake.ts",
            "const child = 'new Hono()'; child.get('/fake', h);"
        )),
        Vec::<&str>::new()
    );
}

#[test]
fn neug_imported_constructors_are_unique_resources_with_safe_signatures() {
    let file = extract(
        "graph.py",
        "import neug as ng\nfrom neug import Node, Relationship\ndb = neug.Database ('shop')\nn = ng.Vertex('User')\nRelationship('OWNS')\nNode('Product')\nng.Graph('g')\nng.Graph('g')\nother.Node('wrong')\n",
    );
    let resources = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Resource)
        .collect::<Vec<_>>();
    assert_eq!(
        resources
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>(),
        [
            "neug:database:shop",
            "neug:vertex:User",
            "neug:relationship:OWNS",
            "neug:node:Product",
            "neug:graph:g"
        ]
    );
    assert_eq!(resources[0].signature.as_deref(), Some("NeuG Database"));
    assert!(
        !extract("other.py", "Node('wrong')\n")
            .symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Resource)
    );
    for source in [
        "from neug import Node\ndef make(Node): return Node('ordinary')\n",
        "import neug as ng\ndef make(ng): return ng.Node('ordinary')\n",
        "from neug import Node\ndef make():\n Node = ordinary\n return Node('ordinary')\n",
        "import neug as ng\ndef make():\n ng = ordinary\n return ng.Node('ordinary')\n",
        "import neug as ng\nwith ordinary as ng: ng.Node('ordinary')\n",
        "from neug import Node\nfrom other import Node\nNode('ordinary')\n",
        "def setup():\n from neug import Node\nNode('ordinary')\n",
    ] {
        assert!(
            !extract("shadowed.py", source)
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Resource)
        );
    }
}

#[test]
fn commonjs_imports_keep_the_innermost_callable_owner() {
    let file = extract(
        "models.js",
        "const root = require('./root');\nfunction load() { const cfg = require('./config'); function nested() { const inside = require('./nested'); } }\n",
    );
    for (module, owner_name) in [
        ("./root", None),
        ("./config", Some("load")),
        ("./nested", Some("nested")),
    ] {
        let reference = file
            .references
            .iter()
            .find(|reference| {
                reference.kind == ReferenceKind::Imports
                    && reference.name == module
                    && reference.owner.is_some() == owner_name.is_some()
            })
            .unwrap_or_else(|| panic!("required framework fact is missing"));
        let owner = reference
            .owner
            .as_ref()
            .and_then(|id| file.symbols.iter().find(|symbol| &symbol.id == id));
        assert_eq!(owner.map(|symbol| symbol.name.as_str()), owner_name);
    }
    assert_eq!(file.language, SourceLanguage::JavaScript);
}
