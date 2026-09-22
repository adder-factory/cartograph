# Game scripting language coverage

[Documentation home](../README.md) · [Project overview](../../README.md) ·
[Language matrix](../SUPPORT-MATRIX.md) · [Native extraction](EXTRACTION.md)

Last implementation audit: 2026-09-22 (`v2.1.31`).

Research inventory last reviewed: 2026-08-02 (`v2.1.7`).

Cartograph v2.1.31 recognizes 132 native source-language modes. Seventy-three
remain the byte-for-byte v1.1.33 parity floor, TOML remains an additive v2 mode,
52 dedicated game, modding, and interactive-fiction scripting modes were
introduced in v2.1.7, WGSL and Metal were added in v2.1.12, and Slang and WESL
were added in v2.1.15. Rhai is included in the 52.

This page defines what “all game scripting languages” means for the release. It
is a researched and testable support boundary, not a claim that every private
engine DSL ever created has a public source format.

## Inclusion boundary

A dedicated mode is included when all of these are true:

1. authors or modders edit a textual source form;
2. the engine, toolchain, or a durable public language reference documents a
   canonical filename, extension, or project path;
3. the source can carry declarations, control flow, calls, imports, diverts, or
   executable game rules rather than only opaque asset bytes;
4. Cartograph has bounded, non-executing extraction with deterministic symbols
   and references, malformed-input behavior, cancellation polling, and literal
   masking; and
5. ambiguous extensions are content- or path-gated without changing the frozen
   v1 classifier.

The existing general-purpose modes already cover the dominant game stacks:
C, C++, C#, Java, Kotlin, JavaScript, TypeScript, Lua, Luau, Python, Ruby, Rust,
Pascal/Delphi, GLSL, and HLSL. They are part of game-development coverage but
are not counted again as dedicated game-script additions.

Visual graphs such as Blueprint, Unity Visual Scripting, Construct event
sheets, and node-based shader graphs are excluded because they have no stable
textual source contract. Compiled or binary artifacts such as `.ksm`, `.dmb`,
`.rsc`, `.tic`, packaged maps, and engine caches are also excluded. Pure asset
and localization formats remain data unless their format defines executable
rules. General compiled game-development languages such as Haxe are separate
language-support work; the executable Haxe `hscript` interpreter language is
included here.

## Dedicated modes

| Stable mode | Canonical source boundary | Extraction emphasis |
|---|---|---|
| `action_script` | `.as` | packages, classes, functions, calls, imports |
| `ags_script` | `.asc`, `.ash` | functions, types, includes, calls |
| `angel_script` | `.angelscript`; content-qualified `.as` | classes, functions, imports, calls |
| `boo` | `.boo` | classes, `def` functions, imports, calls |
| `byond_dm` | `.dm` | object-tree procs/verbs, includes, calls |
| `choice_script` | command-bearing `.txt` below `scenes/` | labels, scene calls, branches |
| `daedalus` | Gothic-script `.d` selected by language markers | instances, prototypes, functions, calls |
| `doom_acs` | `.acs` | scripts/functions, includes, calls |
| `doom_decorate` | canonical `DECORATE` / `DECORATE.txt` | actors, states, includes, action calls |
| `enforce_script` | content/path-qualified `.c` | modded classes, methods, calls |
| `galaxy` | `.galaxy` | functions, types, calls, includes |
| `game_maker_language` | `.gml` | functions, constructors, calls |
| `game_monkey` | `.gm` | functions, classes, calls |
| `gdscript` | `.gd` | classes, functions, preload/load imports, calls |
| `gsc` | `.gsc`, `.csc`, `.gsh` | functions, includes, calls/threads |
| `halo_script` | `.hsc` | script declarations and S-expression calls |
| `hscript` | `.hscript` | functions, classes, imports, calls |
| `idtech_script` | `.script` | functions, events, includes, calls |
| `inform6` | content-qualified `.inf` | routines, objects, includes, calls |
| `inform7` | `.ni`, `.i7x` | phrases/rules, includes, rule calls |
| `ink` | `.ink` | knots/stitches, includes, diverts |
| `jass` | `.j` | libraries/scopes, functions, calls |
| `kerboscript` | human-authored `.ks`; never compiled `.ksm` | functions, run targets, calls |
| `lpc` | content/path-qualified `.c` in MUD source | objects, functions, inheritance, calls |
| `lsl` | `.lsl` | states, events, functions, calls |
| `minecraft_function` | `.mcfunction` | path-defined functions and function commands |
| `miniscript` | `.ms` | functions, classes, imports, calls |
| `nwscript` | `.nss` | functions, structs, includes, calls |
| `papyrus` | `.psc` | scripts/classes, states, functions, imports, calls |
| `paradox_script` | executable `.txt` in known script directories plus content markers | event/resources, scripted triggers/effects |
| `pawn` | `.pwn`, `.sma`; generic `.inc` remains PHP-owned | functions, includes, calls |
| `pico8` | Lua section of textual `.p8`; asset sections are ignored | Lua functions and calls only |
| `quakec` | QuakeC-qualified `.qc` | functions, fields, calls |
| `redscript` | `.reds` | classes, functions, calls |
| `renpy` | `.rpy` | labels, calls, jumps |
| `rhai` | `.rhai` | functions, variables, literal imports, exports, calls |
| `skript` | `.sk` | functions, events, calls |
| `sourcepawn` | `.sp`; generic `.inc` remains PHP-owned | functions, includes, calls |
| `sqf` | `.sqf`, `.hqf` | code blocks, calls/spawns, file execution |
| `sqs` | `.sqs` | labels, jumps, file execution |
| `squirrel` | `.nut` | classes, functions, imports, calls |
| `tads` | TADS-qualified `.t` | objects, functions, includes, calls |
| `torque_script` | `.gui`, `.mis`; content-qualified `.cs` | datablocks, functions, exec imports, calls |
| `twee` | `.twee`, `.tw` | passages and links |
| `unrealscript` | `.uc` | classes, states, functions, calls |
| `valve_qc` | `.qci`; directive-qualified `.qc` | model resources, includes, sequences |
| `verse` | `.verse` | classes, functions, `using` modules, calls |
| `witcher_script` | `.ws` | classes, states, functions, calls |
| `wren` | `.wren` | classes, methods, imports, calls |
| `wurstscript` | `.wurst` | packages, classes, functions, imports, calls |
| `yarn_spinner` | `.yarn` | nodes and jumps |
| `zscript` | `.zs`, canonical `zscript.txt` | classes/actors, functions, includes, calls |

## Collision policy

The detector never chooses a new mode from a shared extension alone when doing
so would replace an existing v1 result:

| Extension/path | Conservative default | Additive selection evidence |
|---|---|---|
| `.as` | ActionScript | AngelScript-only declarations such as `shared`, `mixin`, or `funcdef` |
| `.c` | C | Enforce markers/engine paths or LPC inheritance/types/MUD paths |
| `.cs` | C# | Torque `function`/`datablock` syntax together with `%`, `$`, or `::` |
| `.qc` | QuakeC | Valve model compiler directives such as `$modelname` or `$sequence` |
| `.d` | unsupported rather than guessed | multiple Daedalus declaration markers |
| `.inf` | unsupported rather than guessed | Inform story/parser/routine markers |
| `.t` | unsupported rather than guessed | TADS library or game-main markers |
| `.txt` | unsupported outside existing v1 path rules | ChoiceScript `scenes/` commands, Paradox script directory plus executable block markers, `DECORATE`, or `zscript.txt` |

`for_v1_normalized_path_with_source` uses a separate frozen classifier, so an
Enforce `.c` remains C and a Torque `.cs` remains C# for v1 import parity.

## Research trail

The inventory was assembled from engine/language documentation, canonical
repositories, and the GitHub Linguist extension manifest. Primary sources were
preferred; community references are used only where the engine vendor no
longer maintains a public language manual.

- Engine languages: [Godot GDScript](https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/gdscript_basics.html), [Epic Verse](https://dev.epicgames.com/documentation/en-us/fortnite/verse-language-reference), [GameMaker Language](https://manual.gamemaker.io/lts/en/GameMaker_Language/GML_Reference/GML_Reference.htm), [BYOND DM](https://www.byond.com/docs/ref/info.html), [AngelScript](https://www.angelcode.com/angelscript/), [Wren](https://wren.io/), [Rhai](https://rhai.rs/book/), [MiniScript](https://miniscript.org/), and [TorqueScript](https://torque3d.readthedocs.io/en/latest/script/intro.html).
- Modding and proprietary-engine script: [Papyrus](https://open-papyrus.github.io/docs/Papyrus_Language_Reference/Concepts/Scripts.html), [SQF functions](https://community.bohemia.net/wiki/Functions_-_SQF), [Enforce Script](https://community.bohemia.net/wiki/DayZ:Enforce_Script_Syntax), [SourcePawn](https://sm.alliedmods.net/new-api/), [open.mp Pawn](https://www.open.mp/docs/scripting/language), [REDscript](https://wiki.redmodding.org/redscript), [WitcherScript](https://witcherscript.readthedocs.io/en/latest/), [HaloScript](https://learn.microsoft.com/en-us/halo-master-chief-collection/h2/haloscript/haloscripthome), [kOS/KerboScript](https://ksp-kos.readthedocs.io/en/latest/commands/runprogram.html), and [LDMud LPC](https://www.ldmud.eu/lpc-intro.html).
- id/Valve/Doom families: [id Quake tools source](https://github.com/id-Software/Quake-Tools), [id Doom 3 source](https://github.com/id-Software/DOOM-3), [Valve QC](https://developer.valvesoftware.com/wiki/Qc), [ZDoom ZScript](https://www.zdoom.org/wiki/ZScript), and [ZDoom ACS](https://zdoom.org/wiki/ACS).
- Narrative and interactive fiction: [Ren'Py](https://www.renpy.org/doc/html/quickstart.html), [ink](https://github.com/inkle/ink), [Yarn Spinner](https://docs.yarnspinner.dev/2.5/beginners-guide/syntax-basics), [Twine/Twee](https://twinery.org/cookbook/terms/terms_twee.html), [ChoiceScript](https://www.choiceofgames.com/make-your-own-games/choicescript-intro/), [Inform](https://ganelson.github.io/inform-website/), and [TADS](https://www.tads.org/t3doc/doc/sysman/intro.htm).
- Game/mod ecosystems: [Adventure Game Studio](https://adventuregamestudio.github.io/ags-manual/), [PICO-8 manual](https://www.lexaloffle.com/dl/docs/pico-8_manual.html), [Second Life LSL](https://wiki.secondlife.com/wiki/LSL_Portal), [Minecraft functions](https://learn.microsoft.com/en-us/minecraft/creator/documents/functionsintroduction), [WurstScript](https://wurstlang.org/), [Squirrel](https://squirrel-lang.org/doc/squirrel3.html), and [NWScript/Beamdog](https://nwn.beamdog.net/docs/).
- Canonical extension cross-check: [GitHub Linguist `languages.yml`](https://github.com/github-linguist/linguist/blob/master/lib/linguist/languages.yml).

The audit deliberately does not infer support from a language list alone. Every
row above is represented in the Rust registry and the black-box extractor test
matrix; shared-extension negative controls prove that unrelated files abstain.
