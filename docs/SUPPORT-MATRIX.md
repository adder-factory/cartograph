# Support matrix

[Documentation home](README.md) · [Project overview](../README.md) ·
[Coverage report](LANGUAGE-COVERAGE-REPORT.md) ·
[Extend support](EXTENDING-EXTRACTORS-RESOLVERS.md)

Last implementation audit: 2026-10-01 (`v2.1.33`).

Cartograph v2 supports all 73 v1.1.33 language modes, native TOML, 52
dedicated textual game-scripting modes, the WGSL and Metal shader modes added
in v2.1.12, first-class Slang and WESL added in v2.1.15, and Ada/SPARK and VHDL
added in v2.1.27: 132 modes total.
The source of truth is `cartograph_domain::SourceLanguage::ALL`; native
extractor strategy lives in `crates/cartograph-extract/src/language.rs`, and
framework/cross-language enrichment lives in the focused Rust modules beside
it.

Use this page to decide whether Cartograph can extract useful graph structure
from a project before you install it. A supported language means files are
recognized and indexed. Framework-aware signals add routes, entry points,
dynamic references, or cross-language bridge edges when Cartograph detects a
known framework shape.

`Tree-sitter parser-only` means Cartograph recognizes the file, parses it with
the statically linked native grammar, emits the file node, and surfaces syntax
diagnostics, but does not yet extract language-specific symbols from that
grammar.

[Languages](#languages) ·
[Game scripting](#dedicated-game-scripting-modes) ·
[Framework signals](#framework-aware-signals) ·
[Embedded DSLs](#embedded-dsls-and-derived-signals) ·
[Extending support](#extending-support)

## Languages

| Language mode | Extensions / scope | Extractor path |
|---|---|---|
| ABAP | `.abap` | Tree-sitter |
| Ada / SPARK | `.adb`, `.ads`, `.ada`; SPARK uses the Ada source mode | Tree-sitter with case-insensitive unit, `with`/`use`, declaration, and call extraction |
| Apex | `.cls`, `.trigger` | Tree-sitter |
| ArkTS | `.ets` | Tree-sitter |
| Astro | `.astro` | Tree-sitter plus frontmatter/template extractor |
| Aura | `.cmp`, `.app`, `.evt`, `.intf`, `.design`, `.auradoc` in Aura source paths or Aura markup | Custom extractor |
| Bash | `.sh`, `.bash` | Tree-sitter |
| BG3 Anubis | `.ann`, `.anc`, `Scripts/anubis/node/*.ann`, `Scripts/anubis/config/*.anc` | Custom extractor |
| BG3 Resource Data | `.lsx`, `.lsf`, `.lsfx`, `.lsefx`, `.tbl`, `.stats`, `.mei`, `.lsj`, `Localization/*/*.xml` | Custom extractor |
| BG3 Stats DSL | `Stats/Generated/**/*.txt`, `Stats/Generated/*.txt` | Custom extractor |
| C | `.c`, `.h` | Tree-sitter |
| Clojure / ClojureScript | `.clj`, `.cljs`, `.cljc`, `.edn`, `.bb` | Tree-sitter |
| Common Lisp | `.lisp`, `.lsp`, `.l`, `.cl`, `.asd`, `.ros` | Tree-sitter |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` | Tree-sitter |
| C# | `.cs` | Tree-sitter |
| CUDA | `.cu`, `.cuh` | Tree-sitter |
| CSS | `.css` | Tree-sitter parser-only |
| Dart | `.dart` | Tree-sitter |
| Elixir | `.ex`, `.exs` | Tree-sitter |
| ERB / EJS | `.erb`, `.ejs`, `.eta`, `.etlua` | Tree-sitter parser-only |
| Fish | `.fish` | Tree-sitter |
| F# | `.fs`, `.fsx` | Tree-sitter |
| GLSL | `.glsl`, `.vert`, `.frag`, `.comp`, `.geom`, `.tesc`, `.tese` | Tree-sitter |
| HLSL | `.hlsl`, `.hlsli`, `.fx`, `.fxh` | Tree-sitter |
| Go | `.go` | Tree-sitter |
| GraphQL | `.graphql`, `.gql` | Tree-sitter |
| Groovy | `.groovy`, `.gradle` | Tree-sitter |
| Haskell | `.hs` | Tree-sitter tags query |
| HCL / Terraform / OpenTofu | `.tf`, `.tfvars`, `.hcl`, `.tofu` | Tree-sitter |
| HTML | `.html`, `.htm` | Tree-sitter parser-only |
| Java | `.java` | Tree-sitter |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.xsjs`, `.xsjslib` | Tree-sitter |
| JSDoc | `.jsdoc` | Tree-sitter parser-only |
| JSON | `.json` | Tree-sitter parser-only |
| Jupyter Notebook | `.ipynb` | Tree-sitter parser-only via JSON grammar |
| JSX | `.jsx` | Tree-sitter |
| Julia | `.jl` | Tree-sitter tags query |
| BG3 KHN / Thoth Lua | `.khn` | Lua grammar |
| Kotlin | `.kt`, `.kts` | Tree-sitter |
| Lean | `.lean` | Tree-sitter |
| Liquid | `.liquid` | Custom extractor |
| Lua | `.lua` | Tree-sitter |
| Luau | `.luau` | Tree-sitter |
| Metal Shading Language | `.metal` | Tree-sitter C-family slice |
| Nix | `.nix` | Tree-sitter |
| Objective-C | `.m`, `.mm` | Tree-sitter |
| OCaml | `.ml` | Tree-sitter tags query |
| OCaml Interface | `.mli` | Tree-sitter tags query |
| Osiris Story | `.div`, `Story/RawFiles/Goals/*.txt` | Custom extractor |
| Pascal / Delphi | `.pas`, `.dpr`, `.dpk`, `.lpr`, `.dfm`, `.fmx` | Tree-sitter plus form-file extractor |
| PHP | `.php`, `.module`, `.install`, `.theme`, `.inc` | Tree-sitter |
| PowerShell | `.ps1`, `.psm1`, `.psd1` | Tree-sitter |
| Prisma | `.prisma` | Tree-sitter |
| Java Properties | `.properties` | Custom extractor |
| Python | `.py`, `.pyw`, plus additive v2 `.pyi` | Tree-sitter |
| R | `.r` | Tree-sitter |
| Regex | `.regex`, `.regexp` | Tree-sitter parser-only |
| ReScript | `.res`, `.resi` | Tree-sitter |
| Ruby | `.rb`, `.rake` | Tree-sitter |
| Rust | `.rs` | Tree-sitter |
| Scala | `.scala`, `.sc` | Tree-sitter |
| SQL | `.sql`, `.ddl`, `.dml` | Tree-sitter |
| Solidity | `.sol` | Tree-sitter |
| Slang | `.slang` | Tree-sitter with module/import, interface/generic, and shader-stage facts |
| Svelte | `.svelte` | Custom extractor |
| Swift | `.swift` | Tree-sitter |
| TOML | `.toml` (additive v2 mode) | Bounded native structural scanner |
| TSX | `.tsx` | Tree-sitter |
| TypeScript | `.ts`, `.mts`, `.cts` | Tree-sitter |
| Visual Basic 6 | `.bas`, `.frm`, `.ctl`, `.dob`, `.dsr`, `.pag`, `.vbp`, VB6 `.cls` by content | Custom extractor |
| VB.NET | `.vb` | Tree-sitter |
| Verilog / SystemVerilog | `.v`, `.vh`, `.sv`, `.svh` | Tree-sitter tags query |
| VHDL | `.vhd`, `.vhdl` | Tree-sitter with case-insensitive entity/package/architecture, `library`/`use`, declaration, call, and instantiation extraction |
| Visualforce | `.page`, `.component` | Custom extractor |
| Vue | `.vue` | Custom extractor |
| WESL | `.wesl` | WGSL grammar plus bounded WESL import/module extraction |
| WGSL | `.wgsl` | Tree-sitter shader-family slice |
| XML (MyBatis) | `.xml` | Custom extractor |
| YAML | `.yml`, `.yaml` | Tree-sitter |
| Zsh | `.zsh`, `.zshrc`, `.zshenv`, `.zprofile`, `.zlogin` | Tree-sitter |

### Dedicated game scripting modes

These additive modes use bounded, non-executing Rust scanners. The researched
scope, collision policy, exclusions, and primary-source trail are in
[Game scripting language coverage](v2/GAME-SCRIPTING-LANGUAGES.md).

| Language mode | Extensions / scope |
|---|---|
| ActionScript | `.as` |
| AGS Script | `.asc`, `.ash` |
| AngelScript | `.angelscript`; content-qualified `.as` |
| Boo | `.boo` |
| BYOND Dream Maker | `.dm` |
| ChoiceScript | command-bearing `scenes/**/*.txt` |
| Daedalus | content-qualified `.d` |
| Doom ACS | `.acs` |
| Doom DECORATE | `DECORATE`, `DECORATE.txt` |
| Enforce Script | content/path-qualified `.c` |
| Galaxy | `.galaxy` |
| GameMaker Language | `.gml` |
| GameMonkey | `.gm` |
| GDScript | `.gd` |
| GSC | `.gsc`, `.csc`, `.gsh` |
| HaloScript | `.hsc` |
| hscript | `.hscript` |
| id Tech Script | `.script` |
| Inform 6 | content-qualified `.inf` |
| Inform 7 | `.ni`, `.i7x` |
| ink | `.ink` |
| JASS | `.j` |
| KerboScript | `.ks` source; `.ksm` remains excluded binary code |
| LPC | content/path-qualified `.c` |
| Linden Scripting Language | `.lsl` |
| Minecraft Function | `.mcfunction` |
| MiniScript | `.ms` |
| NWScript | `.nss` |
| Papyrus | `.psc` |
| Paradox Script | executable `.txt` in known game script paths plus content markers |
| Pawn | `.pwn`, `.sma` |
| PICO-8 Lua cartridge source | Lua section of `.p8` only |
| QuakeC | `.qc` unless Valve directives qualify it as Valve QC |
| REDscript | `.reds` |
| Ren'Py | `.rpy` |
| Rhai | `.rhai` |
| Skript | `.sk` |
| SourcePawn | `.sp` |
| SQF | `.sqf`, `.hqf` |
| SQS | `.sqs` |
| Squirrel | `.nut` |
| TADS | content-qualified `.t` |
| TorqueScript | `.gui`, `.mis`; content-qualified `.cs` |
| Twee | `.twee`, `.tw` |
| UnrealScript | `.uc` |
| Valve QC | `.qci`; directive-qualified `.qc` |
| Verse | `.verse` |
| WitcherScript | `.ws` |
| Wren | `.wren` |
| WurstScript | `.wurst` |
| Yarn Spinner | `.yarn` |
| ZScript | `.zs`, `zscript.txt` |

Special cases:

- Play Framework route files at `conf/routes` and `conf/*.routes` are treated
  as YAML so route declarations can be extracted.
- BG3 Anubis `.ann` / `.anc` files, `Stats/Generated/**/*.txt`,
  `Story/RawFiles/Goals/*.txt`, and `Localization/<language>/*.xml` paths use
  BG3-specific extractors because several extensions are otherwise generic
  text/XML or Lua-derived DSL files.
- abapGit-style `*.clas.abap` / `*.intf.abap` paths are covered through the
  `.abap` extension; implementation blocks provide ABAP method containment.
- Astro files emit a component node, extract TypeScript frontmatter, and mine
  PascalCase template component references plus expression calls.
- Salesforce DX source roots such as `force-app/main/default` are recognized.
  Apex `.cls` / `.trigger` files use a tree-sitter grammar, while Aura and
  Visualforce markup use custom extractors for controller refs, component refs,
  fields, routes, and action calls. Aura/Visualforce extension detection is
  path/content gated so unrelated `.app` or `.component` files are not claimed.
- Visual Basic 6 class modules also use `.cls`; Apex remains the extension
  owner, and VB6 wins only when the source has VB6 IDE headers such as
  `VERSION ... CLASS` or `Attribute VB_Name = "..."`.
- Objective-C header files are detected by content so `.h` can resolve to C,
  C++, or Objective-C.
- Liquid can also be detected from YAML front matter in `.html` / `.md` files
  under Jekyll convention directories such as `_layouts`, `_includes`,
  `_posts`, and `_drafts`.
- C# primary constructors are extracted as constructor-shaped method nodes, and
  C# generic/qualified type references are mined from type positions.
- TypeScript type aliases index direct string-literal generic arguments as
  alias-contained property symbols, which covers typed RPC/service contract
  tuples such as `Service<'method_name', Req, Resp>`.
- Go receiver methods are associated with same-package structs even when the
  struct and methods live in different files, so implementation and owner edges
  are available after the post-index hooks run.
- PHP `include`, `include_once`, `require`, and `require_once` statements emit
  file import edges when they use string-literal paths.
- Python `from pkg import module` calls can resolve through the imported module
  to top-level members in `pkg/module.py`.
- SAP HANA XSJS `.xsjs` / `.xsjslib` files use the JavaScript extractor and
  import resolver, including extensionless local imports.
- Chained member calls can resolve through an intermediate method return type
  when Cartograph has a signature such as `build(): Committer` or
  `Committer build()`.

## Framework-Aware Signals

| Ecosystem | Signals |
|---|---|
| JavaScript / TypeScript | Angular routes, Express routes, Hono routes and mounted sub-routers, Fastify object-form routes, Bun.serve routes, NestJS HTTP/GraphQL/message/WebSocket handlers, React components, Vue/Nuxt aliases/routes, SvelteKit routes, Commander/Yargs/CAC CLI commands |
| Python | Django, Flask, FastAPI route/controller patterns, and NeuG graph resource landmarks |
| PHP | Laravel facades/routes, Drupal routes/services/hooks/plugins/service tags, Symfony routes/controllers, and CodeIgniter 3 routes/controller/model/library conventions |
| Ruby | Rails routes and controller conventions |
| JVM | Spring route/config references including `@Value` and `@ConditionalOnProperty`, Play routes, MyBatis Java/XML bindings including `SqlSessionTemplate` statement ids, Kotlin/Scala source extraction |
| Go | Gin, Echo, Chi, net/http, Cobra commands, interface implementation edges |
| Rust | Framework and route patterns recognized by the Rust resolver |
| C# | ASP.NET route/controller patterns |
| Dart / Flutter | `MaterialApp.routes` and `GoRoute(path:)` route nodes with widget references |
| Swift / Apple | SwiftUI, UIKit, Vapor, Swift/Objective-C bridge edges |
| React Native / Expo | Legacy bridge, TurboModules, Expo Modules, Fabric/Paper view components and native implementation edges |
| Salesforce | LWC `@salesforce/apex` imports, LWC/Aura component refs, Aura client/server actions, Visualforce controller/actions |

Framework resolvers run only when the project looks like it uses that
framework, so generic codebases do not pay the full resolver cost.

## Embedded DSLs And Derived Signals

| Signal | What gets added |
|---|---|
| Zod / Pydantic | Schema nodes, fields, and enum-like members |
| GraphQL SDL | Types, fields, enums, interfaces, and references |
| Prisma / SQL | Models, tables, views, functions, triggers, schemas, and table references |
| Package/workspace manifests | npm `package.json`, Composer `composer.json`, and Cargo `Cargo.toml` package/workspace landmarks; dependency sections; npm workspace patterns; Cargo members, exclusions, workspace dependencies, and target-specific dependencies |
| Env/config refs | Env-var, config-key, feature-flag, and build-context reference edges |
| Dynamic imports | String import and dynamic import edges |
| Dynamic dispatch | Bounded TS/JS object/Map dispatch-table call edges marked `INFERRED` |
| Re-exports | Barrel-file and re-export edges |
| Tests | Import-based test edges and test-name signals |
| Coverage | LCOV joins to file and symbol records |
| History | Churn, issue attribution, co-change, and hotspot signals |
| Code Health | Biomarker findings such as complexity, long methods, duplicate code, risky security patterns, and incomplete markers |

## Extending Support

To add a language, start with [Adding a language](ADDING-A-LANGUAGE.md).

## Tree-sitter Catalog Notes

Tree-sitter's homepage and community catalog are not Cartograph's release
contract. Cartograph pins reviewed native grammar crates deliberately rather
than downloading parsers at runtime, so the matrix above is the authoritative
shipped set. A catalog entry is not support until the complete native admission
and live publication gates pass.
