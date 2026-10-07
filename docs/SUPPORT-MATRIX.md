# Support matrix

[Documentation home](README.md) · [Project overview](../README.md) ·
[Coverage report](LANGUAGE-COVERAGE-REPORT.md) ·
[Extend support](EXTENDING-EXTRACTORS-RESOLVERS.md)

Last implementation audit: 2026-10-06 (`v2.1.41`).

Use this page to decide whether Cartograph can extract useful graph structure
from a project before you install it. A supported language means files are
recognized and indexed. Framework-aware signals add routes, entry points,
dynamic references, or cross-language bridge edges when Cartograph detects a
known framework shape.

**At a glance**

| Inventory | Modes | Detail |
|---|---:|---|
| Production-admitted total | **132** | Source of truth: `cartograph_domain::SourceLanguage::ALL` |
| v1.1.33 parity modes | 73 | All 163 v1 extensions, plus additive v2 `.pyi` for Python |
| Dedicated game scripting | 52 | Bounded, non-executing scanners; see [below](#dedicated-game-scripting-modes) |
| Additive v2 modes | 7 | Native TOML; WGSL and Metal (v2.1.12); first-class Slang and WESL (v2.1.15); Ada/SPARK and VHDL (v2.1.27) |
| Grammar-backed | 67 | 61 pinned native grammar bindings; some are shared: Metal→C++, WESL→WGSL, Zsh→Bash, JSX→JavaScript, Jupyter→JSON, KHN→Lua |
| Bounded Rust scanners | 65 | 12 custom v1 modes (Vue and Svelte also parse their script regions with the JavaScript/TypeScript grammar), TOML, and the 52 game-scripting modes |
| Covered by the v1 parity oracle | 73 | Every v1.1.33 mode has a fixture corpus and the facts the real v1.1.33 binary extracted from it; see the [coverage report](LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle) |

Cartograph v2 supports all 73 v1.1.33 language modes, native TOML, 52
dedicated textual game-scripting modes, the WGSL and Metal shader modes added
in v2.1.12, first-class Slang and WESL added in v2.1.15, and Ada/SPARK and VHDL
added in v2.1.27: 132 modes total. Native extractor strategy lives in
`crates/cartograph-extract/src/language.rs`, grammar selection in
`crates/cartograph-extract/src/grammars.rs`, and framework/cross-language
enrichment in the focused Rust modules beside them.

> [!NOTE]
> The per-language claims for the 73 v1 modes are backed by the v1 parity
> oracle (`crates/cartograph-extract/tests/v1_parity_oracle.rs`), which runs as
> part of the workspace `cargo test`. Every fact v1.1.33 extracted from a
> mode's fixture corpus needs a unique exact per-file identity, committed exact
> alignment pins, or a pending/intentional ledger entry with shared evidence.
> Ambiguous identities never match and stale rows fail. The
> [coverage report](LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle) owns current
> counts and update instructions. Cross-file target resolution requires
> separate resolver evidence.

**On this page:** [Languages](#languages) ·
[Game scripting](#dedicated-game-scripting-modes) ·
[Special cases](#special-cases) ·
[Family details](#language-family-details) ·
[Framework signals](#framework-aware-signals) ·
[Embedded DSLs](#embedded-dsls-and-derived-signals) ·
[Extending support](#extending-support) ·
[Tree-sitter catalog](#tree-sitter-catalog-notes)

## Languages

The **Extractor path** column uses these terms:

| Extractor path | Meaning |
|---|---|
| Tree-sitter | A pinned, statically linked native grammar plus a structural walker: a dedicated family (JavaScript/TypeScript, Rust/Python/Go, C-family, Objective-C, shell, managed (Java/C#), Apex, VB.NET, JVM-dynamic, shader, Ada/VHDL, Pascal, PHP, Swift, Dart, F#, ArkTS, Astro, Ruby, Lua, R, Nix, Lisp, Lean, ReScript, Solidity, HCL) or the conservative generic structural walker (ABAP, GraphQL, HTML, Prisma, SQL, YAML; GraphQL, Prisma, and SQL have schema-aware slices). Emits declarations and references. |
| Tree-sitter tags query | Grammar-backed, query-driven declaration and call-reference extraction. |
| Tree-sitter parser-only | Cartograph recognizes the file, parses it with the statically linked native grammar, emits the file node, and surfaces syntax diagnostics, but does not yet extract language-specific symbols from that grammar. |
| C-family / shader-family slice, or another mode's grammar | The mode reuses the grammar and walker of a related language (for example Metal uses the C++ grammar, WESL the WGSL grammar, KHN the Lua grammar). |
| Custom extractor | A bounded Rust scanner with no tree-sitter grammar of its own. Vue and Svelte hosts hand their script regions to the JavaScript/TypeScript family (see [embedded component scripts](#language-family-details)). |
| Bounded native structural scanner | TOML only: the scanner emits the file node (a structural-file floor); see the TOML row. |

| Language mode | Extensions / scope | Extractor path |
|---|---|---|
| ABAP | `.abap` | Tree-sitter generic walker |
| Ada / SPARK | `.adb`, `.ads`, `.ada`; SPARK uses the Ada source mode | Tree-sitter Ada/VHDL family with case-insensitive unit, `with`/`use`, declaration, and call extraction |
| Apex | `.cls`, `.trigger` | Tree-sitter Apex family: the Java-shaped managed walker plus `trigger` declarations and SOQL/SOSL object references |
| ArkTS | `.ets` | Tree-sitter ArkTS family: the TypeScript walker plus `struct` components, fields, and decorators as `decorates` references |
| Astro | `.astro` | Tree-sitter Astro family: one file component; frontmatter and template expressions run through the JavaScript/TypeScript walker; capitalized template tags become component-use references |
| Aura | `.cmp`, `.app`, `.evt`, `.intf`, `.design`, `.auradoc` in Aura source paths or Aura markup | Custom extractor |
| Bash | `.sh`, `.bash` | Tree-sitter shell family |
| BG3 Anubis | `.ann`, `.anc`, `Scripts/anubis/node/*.ann`, `Scripts/anubis/config/*.anc` | Custom extractor |
| BG3 Resource Data | `.lsx`, `.lsf`, `.lsfx`, `.lsefx`, `.tbl`, `.stats`, `.mei`, `.lsj`, `Localization/*/*.xml`, `Public/<dir>/**/*.xml`, `Mods/<dir>/**/*.xml` | Custom extractor |
| BG3 Stats DSL | `Stats/Generated/**/*.txt`, `Stats/Generated/*.txt` | Custom extractor |
| C | `.c`, `.h` | Tree-sitter C family |
| Clojure / ClojureScript | `.clj`, `.cljs`, `.cljc`, `.edn`, `.bb` | Tree-sitter Lisp family: `ns` namespaces, `:require` imports, `def`-form declarations, and list-head calls |
| Common Lisp | `.lisp`, `.lsp`, `.l`, `.cl`, `.asd`, `.ros` | Tree-sitter Lisp family: packages, `def`-form declarations, `use-package`/`require`-style imports, and list-head calls |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` | Tree-sitter C family |
| C# | `.cs` | Tree-sitter managed family |
| CUDA | `.cu`, `.cuh` | Tree-sitter C family |
| CSS | `.css` | Tree-sitter parser-only |
| Dart | `.dart` | Tree-sitter Dart family: classes, mixins, extensions, enums, constructors (including factories), fields, URI imports/exports, heritage, and selector-chain calls |
| Elixir | `.ex`, `.exs` | Tree-sitter tags query |
| ERB / EJS | `.erb`, `.ejs`, `.eta`, `.etlua` | Tree-sitter parser-only |
| Fish | `.fish` | Tree-sitter shell family |
| F# | `.fs`, `.fsx` | Tree-sitter F# family: namespaces, modules, `open` imports, records, unions, members, and application calls |
| GLSL | `.glsl`, `.vert`, `.frag`, `.comp`, `.geom`, `.tesc`, `.tese` | Tree-sitter C family |
| HLSL | `.hlsl`, `.hlsli`, `.fx`, `.fxh` | Tree-sitter C family |
| Go | `.go` | Tree-sitter Rust/Python/Go family |
| GraphQL | `.graphql`, `.gql` | Tree-sitter generic walker with a GraphQL schema slice |
| Groovy | `.groovy`, `.gradle` | Tree-sitter JVM-dynamic family |
| Haskell | `.hs` | Tree-sitter tags query |
| HCL / Terraform / OpenTofu | `.tf`, `.tfvars`, `.hcl`, `.tofu` | Tree-sitter HCL family: top-level blocks named by their Terraform address, address references, and static module `source` imports |
| HTML | `.html`, `.htm` | Tree-sitter generic walker: custom elements and PascalCase elements become component symbols |
| Java | `.java` | Tree-sitter managed family |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.xsjs`, `.xsjslib` | Tree-sitter JavaScript/TypeScript family |
| JSDoc | `.jsdoc` | Tree-sitter parser-only |
| JSON | `.json` | Tree-sitter parser-only; `package.json` and `composer.json` add package/workspace manifest facts |
| Jupyter Notebook | `.ipynb` | Tree-sitter parser-only via JSON grammar |
| JSX | `.jsx` | Tree-sitter JavaScript/TypeScript family |
| Julia | `.jl` | Tree-sitter tags query |
| BG3 KHN / Thoth Lua | `.khn` | Lua grammar with the Tree-sitter Lua family |
| Kotlin | `.kt`, `.kts` | Tree-sitter JVM-dynamic family |
| Lean | `.lean` | Tree-sitter Lean family: imports, namespaces, structures with fields, inductives with constructors, `def`/`theorem`, and `abbrev`; no calls |
| Liquid | `.liquid` | Custom extractor |
| Lua | `.lua` | Tree-sitter Lua family: dotted (`M.f`) and colon (`M:f`) definitions, per-name `local` lists, and literal `require` imports |
| Luau | `.luau` | Tree-sitter Lua family, plus type aliases and return types |
| Metal Shading Language | `.metal` | Tree-sitter C-family slice |
| Nix | `.nix` | Tree-sitter Nix family: attribute-path bindings, `inherit`, application calls, and path `import`s |
| Objective-C | `.m`, `.mm` | Tree-sitter Objective-C family: the C subset plus classes, categories, protocols, full-selector methods, `@property` names, and message sends; React Native `RCT_*` method macros are rewritten before parsing |
| OCaml | `.ml` | Tree-sitter tags query |
| OCaml Interface | `.mli` | Tree-sitter tags query |
| Osiris Story | `.div`, `Story/RawFiles/Goals/*.txt` | Custom extractor |
| Pascal / Delphi | `.pas`, `.dpr`, `.dpk`, `.lpr`, `.dfm`, `.fmx` | Tree-sitter Pascal family: units, `uses` imports, types and members, implementations paired with their declarations, nested routines, and scope-bound calls; `.dfm` / `.fmx` text form files use a bounded form scanner |
| PHP | `.php`, `.module`, `.install`, `.theme`, `.inc` | Tree-sitter PHP family: namespace-qualified declarations, `use` imports, literal includes, and compile-time-resolved class, function, member, and static references |
| PowerShell | `.ps1`, `.psm1`, `.psd1` | Tree-sitter shell family |
| Prisma | `.prisma` | Tree-sitter generic walker with a Prisma schema slice |
| Java Properties | `.properties` | Custom extractor |
| Python | `.py`, `.pyw`, plus additive v2 `.pyi` | Tree-sitter Rust/Python/Go family |
| R | `.r` | Tree-sitter R family: assignment-named functions, constants, `library`/`require` package imports, and `source` file imports |
| Regex | `.regex`, `.regexp` | Tree-sitter parser-only |
| ReScript | `.res`, `.resi` | Tree-sitter ReScript family: `open`/`include` imports, variants, records, externals, callable `let`s, module types, and module aliases |
| Ruby | `.rb`, `.rake` | Tree-sitter Ruby family: modules, classes, `attr_*` fields, constants, visibility sections, calls, and `require`/`require_relative` imports |
| Rust | `.rs` | Tree-sitter Rust/Python/Go family |
| Scala | `.scala`, `.sc` | Tree-sitter JVM-dynamic family |
| SQL | `.sql`, `.ddl`, `.dml` | Tree-sitter generic walker with a SQL schema slice |
| Solidity | `.sol` | Tree-sitter Solidity family on top of the generic walker: contract methods and modifiers, enum values, struct members, state variables, and pragmas |
| Slang | `.slang` | Tree-sitter C family with module/import, interface/generic, and shader-stage facts |
| Svelte | `.svelte` | Custom extractor; `<script>` blocks and template expressions run through the JavaScript/TypeScript walker |
| Swift | `.swift` | Tree-sitter Swift family: struct, class, actor, enum, protocol, and extension declarations, members with Swift visibility, and parameter/return type references |
| TOML | `.toml` (additive v2 mode) | Bounded native structural scanner: file node only (structural-file floor); `Cargo.toml` adds package/workspace manifest facts |
| TSX | `.tsx` | Tree-sitter JavaScript/TypeScript family |
| TypeScript | `.ts`, `.mts`, `.cts` | Tree-sitter JavaScript/TypeScript family |
| Visual Basic 6 | `.bas`, `.frm`, `.ctl`, `.dob`, `.dsr`, `.pag`, `.vbp`, VB6 `.cls` by content | Custom extractor |
| VB.NET | `.vb` | Tree-sitter VB.NET family: block declarations, typed members, `Imports`, calls, and `Inherits`/`Implements` on the type line or the line after it |
| Verilog / SystemVerilog | `.v`, `.vh`, `.sv`, `.svh` | Tree-sitter tags query |
| VHDL | `.vhd`, `.vhdl` | Tree-sitter Ada/VHDL family with case-insensitive entity/package/architecture, `library`/`use`, declaration, call, and instantiation extraction |
| Visualforce | `.page`, `.component` | Custom extractor |
| Vue | `.vue` | Custom extractor; `<script>` / `<script setup>` blocks and template expressions run through the JavaScript/TypeScript walker |
| WESL | `.wesl` | WGSL grammar plus bounded WESL import/module extraction |
| WGSL | `.wgsl` | Tree-sitter shader-family slice |
| XML (MyBatis) | `.xml` | Custom extractor |
| YAML | `.yml`, `.yaml` | Tree-sitter generic walker |
| Zsh | `.zsh`, `.zshrc`, `.zshenv`, `.zprofile`, `.zlogin` | Tree-sitter shell family |

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

### Special cases

**Detection and routing**

- Play Framework route files at `conf/routes` and `conf/*.routes` are treated
  as YAML so route declarations can be extracted.
- BG3 Anubis `.ann` / `.anc` files, `Stats/Generated/**/*.txt`,
  `Story/RawFiles/Goals/*.txt`, `Localization/<language>/*.xml`, and `.xml`
  files below a directory under `Public/` or `Mods/` (`Public/<dir>/**/*.xml`,
  `Mods/<dir>/**/*.xml`) use BG3-specific extractors because several
  extensions are otherwise generic text/XML or Lua-derived DSL files. These
  path rules match case-insensitively.
- abapGit-style `*.clas.abap` / `*.intf.abap` paths are covered through the
  `.abap` extension. ABAP classes and methods are declared at their
  `IMPLEMENTATION` blocks; a class `DEFINITION` is declared only when the file
  has no implementation for it.
- Salesforce DX source roots such as `force-app/main/default` are recognized.
  Apex `.cls` / `.trigger` files use a tree-sitter grammar, while Aura and
  Visualforce markup use custom extractors for controller refs, component refs,
  fields, routes, and action calls. Aura/Visualforce extension detection is
  path/content gated so unrelated `.app` or `.component` files are not claimed.
- Visual Basic 6 class modules also use `.cls`; Apex remains the extension
  owner, and VB6 wins only when the first 8 KiB of source has a VB6 IDE header
  such as `VERSION ... CLASS`, `Attribute VB_Name = "..."`, or `Begin VB.`, or
  has `Option Explicit` together with a `Sub`, `Function`, or `Property`
  routine.
- Objective-C header files are detected by content so `.h` can resolve to C,
  C++, or Objective-C.
- Liquid can also be detected from YAML front matter (a `---` block that opens
  on the first line). Any `.html` file with front matter is Liquid, for example
  `layouts/default.html`; `.md` files are considered only under Jekyll
  convention directories such as `_layouts`, `_includes`, `_posts`, and
  `_drafts`.

**Extraction and resolution**

- Astro frontmatter, Vue and Svelte `<script>` blocks, and template
  expressions are extracted by the native JavaScript/TypeScript walker over the
  host file's own byte positions; see
  [embedded component scripts](#language-family-details).
- Haskell files with a dotted `module` name (`module Util.Strings`) and Elixir
  modules that use `defstruct` extract like any other file. Both used to fail
  the whole file with `parse_grammar_unavailable`.
- C# primary constructors are extracted as constructor-shaped method nodes, and
  C# generic/qualified type references are mined from type positions.
- Go receiver methods are associated with same-package structs even when the
  struct and methods live in different files, so implementation and owner edges
  are available. That ownership is computed during project-wide resolution,
  before publication.
- PHP `include`, `include_once`, `require`, and `require_once` expressions
  emit file import edges when they use string-literal paths. The path resolves
  next to the including file first, then from the project root.
- Python `from pkg import module` calls can resolve through the imported module
  to top-level members in `pkg/module.py`.
- SAP HANA XSJS `.xsjs` / `.xsjslib` files use the JavaScript extractor and
  import resolver, including extensionless local imports.
- A member called on the result of a factory call resolves through the
  factory's declared return type only in PHP
  (`ApiClient::for($c)->createOrder()`; see the PHP details below). In the
  other languages such a chained call (`b.build().commit()`,
  `Builder::new().build()`) stays unresolved rather than guessed.

### Language family details

Each dedicated family restores the per-file facts v1.1.33 extracted for its
languages; the [v1 parity oracle](LANGUAGE-COVERAGE-REPORT.md#v1-parity-oracle)
checks them against v1's own output. The blocks below record what each family
emits and where it deliberately stops.

<details>
<summary>Details: embedded component scripts (Astro, Vue, Svelte)</summary>

- Astro frontmatter, Vue and Svelte `<script>` blocks, template expressions
  (`{…}`, `{{ … }}`), and click event-handler attribute values (`@click`,
  `v-on:click`, `on:click`, `onclick`) are parsed by the native
  JavaScript/TypeScript grammar restricted to their byte ranges with
  tree-sitter included ranges over the host file. Every span is therefore an
  exact host position; nothing is remapped.
- Each file is one exported component named after the file. Script
  declarations keep module-scope qualified names; the component is their graph
  parent and owns top-level references. Each `<script>` block is its own module
  for export lists. A `lang="ts"` script is TypeScript.
- Template expressions declare nothing; their references belong to the
  component. A name an expression binds for itself (a handler's parameter or
  local, a callback's named function) hides same-named script declarations
  only inside its own block, function, or clause. A template dynamic import
  keeps its module reference but binds no names.
- Svelte runes and Vue `<script setup>` compiler macros are not calls of
  project code, and Svelte `$store` identifiers reference their store.
  Capitalized template tags are component-use references, and component
  static imports resolve to the imported file. Text inside HTML comments is
  ignored.
- Astro client `<script>` and `<style>` elements are not part of the
  component's server-side module and are not extracted, as in v1.
- Script syntax errors mark the file partial with host-span diagnostics. A
  template whose unterminated structure exhausts the bounded template scan is
  also reported as partial; later scripts and tags are still extracted.
- Known limits: an Astro expression whose regular-expression literal contains
  `}` ends early (an Astro grammar limit, reported as a partial file); Vue
  `v-for` and Svelte `{#each}` names are not treated as template-local, so
  `item.render()` keeps its ordinary member-call facts.

</details>

<details>
<summary>Details: PHP</summary>

- Declarations are qualified by namespace (`App\Models::User::find`). A named
  function or class declared inside a function body is a namespace-level
  declaration. Traits are `trait` symbols, enum cases are enum members, class
  constants are constants, and properties are fields named without `$`.
  `extends`, `implements`, and trait `use` emit heritage references, and
  attributes emit `decorates` references.
- Each `use` clause (simple, aliased, `use function`, `use const`, grouped)
  emits an import symbol named by its fully qualified name, an `imports`
  reference, a named import binding, and a reference to the imported
  declaration.
- `include`/`require` (and their `_once` forms) with a plain string-literal
  target emit file imports that pass the shared credential screen. Interpolated
  or concatenated targets are skipped, and an include import carries only its
  path.
- Function, member (`->`, `?->`), static (`::`), and `new` references follow
  PHP's compile-time name rules and resolve only by exact qualified name and
  symbol space, never by short name. Namespace, class, function, and method
  names ignore ASCII case; constant names do not. Declarations that differ only
  in case are ambiguous.
- A name imported from a vendor package stays unresolved as external, as do
  dynamic receivers and members a class does not declare itself. `$this->` and
  `static::` resolve to the statically known class's declaration with
  dynamic-dispatch confidence.
- A method called on a factory's result (`ApiClient::for($c)->createOrder()`)
  is followed only through the factory's declared return type: `self`/`static`
  (the factory's own class) or exactly one class the file names.
- Laravel: a static call to a member an Eloquent model does not declare
  (`User::where`) binds to the model class with provenance
  `framework-laravel-eloquent-model` and framework-convention confidence
  (0.85). The class must reach `Illuminate\Database\Eloquent\Model`,
  `Illuminate\Foundation\Auth\User`, or an Eloquent `Pivot`/`MorphPivot`
  through exact `extends` ancestry in the project; a member a project ancestor
  or trait supplies is not redirected, and facades such as `Cache::get` stay
  external.

</details>

<details>
<summary>Details: Pascal / Delphi</summary>

- A unit, program, or library is a module; `uses` emits one import per unit
  plus a unit-named binding, so `Unit.Member` resolves through the unit. Units
  resolve by their declared name.
- Classes, records, interfaces, enums, and their fields, properties,
  constants, and methods are extracted where they are declared. An
  implementation (`procedure TShape.Draw`) never becomes a second symbol: it
  is paired with its declaration, which then spans the body and owns its
  calls. Nested routines are extracted.
- Calls the file can bind by Pascal scope (nested routines, implicit `Self`,
  in-file types, and the file's own unit-level routines, matched
  case-insensitively) resolve within the file; the rest use ordinary project
  resolution. Runtime-library names (`Format`, `Length`, `Inc`, `FreeAndNil`,
  `TObject`, …) never bind to a same-named project declaration.
- `.dfm` / `.fmx` text form files use a bounded form scanner: the component
  hierarchy (`object Name: TClass`, `inherited`, `inline`) becomes component
  symbols, and each `OnX = Handler` binds to the form class's method through the
  same-named unit beside the form (`{$R *.dfm}` linkage). Property values are
  never retained.

</details>

<details>
<summary>Details: Objective-C and Swift</summary>

- Objective-C delegates its C subset (includes, functions, structs, typedefs,
  globals, macros) to the C family. `@interface`, categories, and
  `@implementation` reopen one class; protocols are `protocol` symbols; methods
  are named by full selector (`doThing:with:`); the superclass is an `extends`
  reference and adopted protocols are `implements` references. Message sends
  are named `receiver.selector`, with `self` and `super` dropped.
- React Native `RCT_EXPORT_METHOD`, `RCT_REMAP_METHOD`, and their blocking
  synchronous forms are rewritten before parsing without changing any offset,
  so they become selector methods instead of being lost.
- An Objective-C keyword send resolves to the one class that declares its full
  selector, preferring the implementation over a header declaration. A
  protocol requirement is the target only when no class declares the selector
  and exactly one protocol requires it. Unary sends, and selectors declared by
  two classes, stay unresolved.
- Swift maps struct, class, actor, enum, protocol, and extension declarations
  to their kinds; an extension reopens a same-file type. Enum cases are
  members, stored properties are fields, computed properties are properties,
  and top-level `let`/`var` bindings are constants or variables, one symbol per
  name. Visibility defaults to internal. Backtick-escaped names are stored
  without backticks. Parameter types give `type_of` references and return
  types give `returns` references.
- A Swift `import Module` or an Objective-C `#import <Framework/Header.h>`
  never hides the project's own declaration of a name. An unqualified Swift
  call looks among the enclosing type's methods (implicit `self`) first.

</details>

<details>
<summary>Details: JavaScript/TypeScript, Rust, Python, and Go</summary>

- JavaScript/TypeScript: class fields (arrow-function fields as methods, field
  types as `type_of`), plain JavaScript `extends`, decorators as `decorates`
  references owned by the decorated class, method, or field, CommonJS
  `require` and dynamic `import()`, `def_use` edges for lexical bindings, and
  string-literal generic arguments of a type alias as contract properties of
  the alias (when the generic is declared in the same file).
- JavaScript/TypeScript value and `SCREAMING_SNAKE` constant reads follow
  lexical scope: a parameter, local, or nested declaration of the same name
  hides a module symbol, so a shadowed read never binds the top-level
  declaration.
- Python: module-level assignments, `@staticmethod`, `Protocol`/ABC bases as
  `implements`, and decorators as `decorates` references.
- Go: top-level `var`/`const` (including grouped blocks), struct fields with
  their `type_of`, struct and interface embedding as `extends`, composite
  literals as `instantiates` with named keys as field accesses, cgo `C.x`
  calls without the pseudo-receiver, and PascalCase constant reads.
- Rust: supertrait bounds as `extends`, struct expressions as `instantiates`,
  field `type_of`, `SCREAMING_SNAKE` constant reads, and outer attributes
  (`#[derive(..)]`) as `decorates` references owned by the item they precede.
  A `macro_rules!` defined in the same file whose rules bind only expression
  or literal fragments reads constants in its arguments.
- These enrichment facts are optional: a dense generated file whose output
  would exceed its per-file limit with them is extracted again without them
  and carries the non-degrading `optional_facts_omitted` diagnostic; see
  [native extraction](v2/EXTRACTION.md#parsing-and-facts).

</details>

<details>
<summary>Details: JVM, .NET, and Apex</summary>

- Kotlin, Groovy, and Scala annotations are `decorates` references, as for
  Java and C#. A top-level Kotlin extension function (`fun String.shout()`) is
  qualified by its receiver type (`pkg::String::shout`).
- Java and Kotlin heritage no longer names generic type arguments as
  supertypes (`Base<String>` extends `Base` only), and C# tuple element names
  are not type references.
- VB.NET maps class, interface, structure, enum, module, and namespace blocks
  to typed symbols, with methods, constructors, properties, per-declarator
  fields, constants, `Imports`, and invocation/construction references.
  `Friend` is internal and `Shared` is static. The grammar accepts
  `Inherits`/`Implements` only on the type line; the idiomatic next-line form
  is recovered from its source line (the file is still reported partial
  because of the grammar error).
- VB6 `Declare … Lib` routines are import symbols, and paren-less statement
  calls are calls.
- Apex runs the Java-shaped managed walker and adds `trigger` declarations (a
  function whose signature names the sObject and events, owning the trigger
  body) and inline SOQL/SOSL object references (`[SELECT Id FROM Account]`).

</details>

<details>
<summary>Details: Dart, F#, and ArkTS</summary>

- Dart attaches each callable body to its signature, extracts classes, mixins,
  extensions, enum constants, fields, and constructors (a factory constructor
  is the method `Class::Class` and owns its body's calls), records URI
  imports and exports with their bindings, names calls from their selector
  chain, and records `extends`, `with`, and `implements` heritage. A leading
  `_` makes a declaration library-private.
- F# extracts namespaces (`namespace rec X.Y` is `X.Y`), modules, `open`
  imports, records with fields, unions, and members, and names a call by the
  head of its outermost application; a forward pipe names the piped function
  (`xs |> List.map f` calls `List.map`). Declarations are public unless an
  access modifier says otherwise.
- ArkTS runs the TypeScript walker unchanged and adds `struct` components,
  class and struct fields, and decorators (`@Component`, `@State`,
  `@Builder`, …) as `decorates` references.

</details>

<details>
<summary>Details: Ruby, Lua/Luau/KHN, R, and Nix</summary>

- Ruby: modules, classes, and superclass inheritance; methods, with
  `def self.x` static; `attr_reader`/`attr_writer`/`attr_accessor` and
  `class_attribute` fields; constants; `private`/`protected`/`public`
  sections. Statement-level bare identifiers that are not locals are calls,
  and receiver calls keep their receiver (`Factory.run`). `require_relative`
  resolves relative to the file; other requires stay unresolved load-path
  imports.
- Lua, Luau, KHN: dotted (`M.f`) and colon (`M:f`, a method) definitions, a
  function value named by the variable it is assigned to, one binding per
  name in `local a, b = ...`, and literal `require` imports. Lua resolves
  `require("a.b")` to `a/b.lua`, then `a/b/init.lua`, from the project root;
  Luau and KHN resolve relative specifiers only. Luau adds type aliases
  (exported only with `export type`) and return types.
- R: functions named by the left side of `<-`, `=`, or `<<-` (lambdas declare
  nothing), top-level constants and function-local variables,
  `library`/`require` package imports, and `source("p")` resolved to that
  root-relative path.
- Nix: a binding is a function when its value is a lambda and a constant
  otherwise, named by its full attribute path; `inherit` declares constants
  and references its source; an application calls its head
  (`pkgs.stdenv.mkDerivation`); `import ./p` resolves to the exact file or
  `p/default.nix`.

</details>

<details>
<summary>Details: Clojure, Common Lisp, Lean, ReScript, and Solidity</summary>

- Clojure: `ns` is a namespace and each `:require` libspec an import;
  `defn`/`defn-`/`defmacro` are functions, `def`/`defonce` constants,
  `defrecord`/`deftype` classes, and `defprotocol` interfaces. `defn-` and
  `^:private` make a definition private. List heads are calls; special forms,
  bindings, and quoted data are not. Namespace-alias calls
  (`str/upper-case`) are recorded but not resolved through the alias.
- Common Lisp: `defpackage`/`in-package` namespaces; `defun`, `defmacro`,
  `defgeneric`, and `defmethod` functions; `defvar`/`defparameter`/
  `defconstant` constants; `defclass`/`define-condition` classes; `defstruct`
  structs; `use-package`/`require`-style imports (also inside `defpackage`);
  and list-head calls.
- Lean: imports, namespaces, `structure`/`class` with fields, `inductive`
  types with their constructors, `def`/`theorem` as functions, and `abbrev`
  type aliases. `private` declarations are not exported. No calls are
  recorded.
- ReScript: `open`/`include` imports with the full module path, variants as
  enums, records as structs with fields, other types and exceptions as type
  aliases, `external` functions, function-valued `let`s with signatures,
  `module type` as an interface, other modules as namespaces, and module
  aliases as references. Calls include pipes into bare functions.
- Solidity: contract, interface, and library members on top of the generic
  walker: functions, modifiers, the constructor, `fallback`, and `receive` as
  methods (free functions stay functions), enum values, struct members and
  state variables as fields, and `pragma` as an import symbol without a
  dependency edge. A bare call to a member of the enclosing contract resolves
  to it unless a parameter, local, or named return shadows it.
  Inherited-member and `Library.fn` calls are not resolved yet.

</details>

<details>
<summary>Details: HCL/Terraform and import screening</summary>

- Every top-level block is one symbol whose qualified name is its Terraform
  address (`var.x`, `local.x`, `module.x`, `data.TYPE.NAME`, `TYPE.NAME`,
  `output.x`), and attribute expressions reference blocks by that same
  address, so they resolve by exact qualified name.
- A module `source` becomes an `imports` reference only when it is static and
  passes the shared credential screen.
- The screen applies to every import specifier the walkers retain (including
  generic-walker imports, which now also declare `Import` symbols): user info
  other than the conventional `git` SSH user, or a segment shaped like an
  issued provider key, drops the specifier, and URL or query-bearing
  specifiers are also rejected for credential words and high-entropy tokens.

</details>

## Framework-Aware Signals

| Ecosystem | Signals |
|---|---|
| JavaScript / TypeScript | Angular routes, Express routes, Hono routes and mounted sub-routers, Fastify object-form routes, Bun.serve routes, NestJS HTTP/GraphQL/message/WebSocket handlers, React components, Vue/Nuxt aliases/routes, Next.js `pages/` and `app/` file routes, Nuxt `server/api` routes and `middleware/` functions, SvelteKit routes, Commander/Yargs/CAC CLI commands |
| Python | Django, Flask, FastAPI route/controller patterns, and NeuG graph resource landmarks |
| PHP | Laravel `Route::` facade routes and Eloquent model static calls (`User::where`) bound to the model class, Drupal routes/services/hooks/plugins/service tags, Symfony YAML routes with controller references, Symfony `#[Route]` attribute routes (class-level path, name, and HTTP-method prefixes composed as Symfony does; invokable controllers route to `__invoke`), and CodeIgniter 3 routes/controller/model/library conventions |
| Ruby | Rails routes and controller conventions |
| JVM | Spring route mappings, `${…}` placeholder config references (a `@Value` placeholder is owned by the annotated field), and `@ConditionalOnProperty` `prefix`/`name`/`value` configuration keys owned by the annotated declaration; Play routes; MyBatis mapper-interface ↔ XML statement bindings (a packaged mapper binds only to its own package's XML namespace) and `SqlSessionTemplate` statement-id calls; Kotlin/Scala source extraction |
| Go | Gin, Echo, Chi, net/http, Cobra commands, interface implementation edges |
| Rust | Actix Web, Axum, Rocket, and warp route patterns: route attributes such as `#[get("/…")]`, `.route("/…", get(…))` registrations, and literal-path route calls |
| C# | ASP.NET route/controller patterns |
| Dart / Flutter | `MaterialApp.routes` and `GoRoute(path:)` route nodes with widget references |
| Swift / Apple | SwiftUI views and `@main` App entry landmarks; Vapor routes; UIKit (`ViewController`, `View`, `Cell`, `Delegate`/`DataSource`) and Vapor (`Controller`, `Middleware`, model) naming conventions that prefer the conventional directory and kind when a name matches several declarations; Swift/Objective-C bridge edges |
| React Native / Expo | Legacy bridge, TurboModules, Expo Modules, Fabric/Paper view components and native implementation edges; each export belongs to the module or class that declares it (Objective-C categories and Swift extensions included), and in a file that registers several modules a declaration outside every module definition is left unattributed |
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

To add a language, start with [Adding a language](ADDING-A-LANGUAGE.md). The
full registration checklist and required gates are in
[Extending extraction and resolution](EXTENDING-EXTRACTORS-RESOLVERS.md).

## Tree-sitter Catalog Notes

Tree-sitter's homepage and community catalog are not Cartograph's release
contract. Cartograph pins reviewed native grammar crates deliberately rather
than downloading parsers at runtime, so the matrix above is the authoritative
shipped set. A catalog entry is not support until the complete native admission
and live publication gates pass.
