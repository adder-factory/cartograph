use cartograph_domain::SourceLanguage;

use crate::NativeGrammar;

/// Executable extraction family selected by the production language registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractionStrategy {
    /// Existing JavaScript/TypeScript structural walker.
    JavaScriptFamily,
    /// Existing Rust/Python/Go structural walkers.
    PolyglotStructural,
    /// Parse and diagnose a file while intentionally emitting no language-level facts.
    ParserOnly,
    /// Query-driven declaration and call-reference extraction.
    TagsQuery,
    /// C/C++-grammar family with structural declarations, types, includes, calls, and fields.
    CFamily,
    /// Shell-family structural extraction with literal-safe variables, imports, and calls.
    ShellFamily,
    /// Java/C# structural extraction with managed-language declarations and references.
    ManagedFamily,
    /// Kotlin/Scala/Groovy structural extraction with JVM and dynamic-language semantics.
    JvmDynamicFamily,
    /// WGSL shader extraction with stage-typed entry points, bindings, and
    /// `naga_oil` module imports.
    ShaderFamily,
    /// Ada and VHDL declarations, case-insensitive references, and compilation imports.
    AdaFamily,
    /// Dart declarations with sibling bodies, URI imports, heritage, and selector calls.
    DartFamily,
    /// F# namespaces, modules, `open` imports, typed definitions, and application calls.
    FSharpFamily,
    /// `ArkTS` on the TypeScript walker plus structs, fields, and decorators.
    ArkTsFamily,
    /// Clojure and Common Lisp list-form declarations, imports, and list-head calls.
    LispFamily,
    /// Lean imports, structures, inductives, definitions, theorems, and abbreviations.
    LeanFamily,
    /// `ReScript` opens, variants, records, externals, callable lets, and modules.
    ReScriptFamily,
    /// Solidity contract members, modifiers, enum values, state variables, and pragmas.
    SolidityFamily,
    /// Ruby modules, classes, accessors, constants, visibility sections, and load imports.
    RubyFamily,
    /// Lua, Luau, and KHN tables, colon methods, local lists, and `require` imports.
    LuaFamily,
    /// R assignment-named functions, constants, and package/source imports.
    RFamily,
    /// Nix attribute bindings, inherits, applications, and path imports.
    NixFamily,
    /// HCL/Terraform blocks addressed by their Terraform names, with address references.
    HclFamily,
    /// VB.NET block declarations, typed members, `Imports`, and invocation references.
    VbNetFamily,
    /// Salesforce Apex: the Java-shaped managed family plus triggers and SOQL/SOSL objects.
    ApexFamily,
    /// Pascal and Delphi units, types, members, implementation bodies, and `uses` imports.
    PascalFamily,
    /// Objective-C classes, protocols, full-selector methods, message sends, and the C subset.
    ObjcFamily,
    /// Swift type kinds, members, visibility, and parameter/return type references.
    SwiftFamily,
    /// PHP declarations, namespace `use` imports, literal includes, and
    /// compile-time-resolved class, member, and static call references.
    PhpFamily,
    /// Grammar-backed conservative structural extraction for the remaining v1 language modes.
    GenericStructural,
    /// Bounded native scanners for custom, mixed-markup, and domain-specific v1 modes.
    CustomStructural,
}

impl ExtractionStrategy {
    /// Whether this strategy is currently executable end to end.
    #[must_use]
    pub const fn is_executable(self) -> bool {
        matches!(
            self,
            Self::JavaScriptFamily
                | Self::PolyglotStructural
                | Self::ParserOnly
                | Self::TagsQuery
                | Self::CFamily
                | Self::ShellFamily
                | Self::ManagedFamily
                | Self::JvmDynamicFamily
                | Self::ShaderFamily
                | Self::AdaFamily
                | Self::DartFamily
                | Self::FSharpFamily
                | Self::ArkTsFamily
                | Self::LispFamily
                | Self::LeanFamily
                | Self::ReScriptFamily
                | Self::SolidityFamily
                | Self::RubyFamily
                | Self::LuaFamily
                | Self::RFamily
                | Self::NixFamily
                | Self::HclFamily
                | Self::VbNetFamily
                | Self::ApexFamily
                | Self::PascalFamily
                | Self::ObjcFamily
                | Self::SwiftFamily
                | Self::PhpFamily
                | Self::GenericStructural
                | Self::CustomStructural
        )
    }
}

/// One typed production language/extractor registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageSpec {
    language: SourceLanguage,
    grammar: Option<NativeGrammar>,
    strategy: ExtractionStrategy,
}

impl LanguageSpec {
    /// Resolve the one authoritative extractor registration for a language.
    #[must_use]
    pub const fn for_language(language: SourceLanguage) -> Self {
        let strategy = strategy_for_language(language);
        Self {
            language,
            grammar: NativeGrammar::for_source_language(language),
            strategy,
        }
    }

    /// Stable language identifier represented by this spec.
    #[must_use]
    pub const fn language(self) -> SourceLanguage {
        self.language
    }

    /// Statically linked grammar, absent only for bounded custom modes.
    #[must_use]
    pub const fn grammar(self) -> Option<NativeGrammar> {
        self.grammar
    }

    /// Runtime extraction family.
    #[must_use]
    pub const fn strategy(self) -> ExtractionStrategy {
        self.strategy
    }
}

const fn strategy_for_language(language: SourceLanguage) -> ExtractionStrategy {
    if is_game_scripting_language(language) {
        return ExtractionStrategy::CustomStructural;
    }
    match grammar_strategy(language) {
        Some(strategy) => strategy,
        None => fallback_strategy(language),
    }
}

const fn grammar_strategy(language: SourceLanguage) -> Option<ExtractionStrategy> {
    match language {
        SourceLanguage::TypeScript
        | SourceLanguage::Tsx
        | SourceLanguage::JavaScript
        | SourceLanguage::Jsx => Some(ExtractionStrategy::JavaScriptFamily),
        SourceLanguage::Rust | SourceLanguage::Python | SourceLanguage::Go => {
            Some(ExtractionStrategy::PolyglotStructural)
        }
        SourceLanguage::Css
        | SourceLanguage::EmbeddedTemplate
        | SourceLanguage::JsDoc
        | SourceLanguage::Json
        | SourceLanguage::Jupyter
        | SourceLanguage::Regex => Some(ExtractionStrategy::ParserOnly),
        SourceLanguage::Elixir
        | SourceLanguage::Haskell
        | SourceLanguage::Julia
        | SourceLanguage::Ocaml
        | SourceLanguage::OcamlInterface
        | SourceLanguage::Verilog => Some(ExtractionStrategy::TagsQuery),
        SourceLanguage::C
        | SourceLanguage::Cpp
        | SourceLanguage::Cuda
        | SourceLanguage::Glsl
        | SourceLanguage::Hlsl
        | SourceLanguage::Metal
        | SourceLanguage::Slang => Some(ExtractionStrategy::CFamily),
        SourceLanguage::Wesl | SourceLanguage::Wgsl => Some(ExtractionStrategy::ShaderFamily),
        SourceLanguage::Ada | SourceLanguage::Vhdl => Some(ExtractionStrategy::AdaFamily),
        SourceLanguage::Dart => Some(ExtractionStrategy::DartFamily),
        SourceLanguage::FSharp => Some(ExtractionStrategy::FSharpFamily),
        SourceLanguage::ArkTs => Some(ExtractionStrategy::ArkTsFamily),
        SourceLanguage::Clojure | SourceLanguage::CommonLisp => {
            Some(ExtractionStrategy::LispFamily)
        }
        SourceLanguage::Lean => Some(ExtractionStrategy::LeanFamily),
        SourceLanguage::ReScript => Some(ExtractionStrategy::ReScriptFamily),
        SourceLanguage::Solidity => Some(ExtractionStrategy::SolidityFamily),
        SourceLanguage::Ruby => Some(ExtractionStrategy::RubyFamily),
        SourceLanguage::Lua | SourceLanguage::Luau | SourceLanguage::Khn => {
            Some(ExtractionStrategy::LuaFamily)
        }
        SourceLanguage::R => Some(ExtractionStrategy::RFamily),
        SourceLanguage::Nix => Some(ExtractionStrategy::NixFamily),
        SourceLanguage::Hcl => Some(ExtractionStrategy::HclFamily),
        SourceLanguage::VbNet => Some(ExtractionStrategy::VbNetFamily),
        SourceLanguage::Apex => Some(ExtractionStrategy::ApexFamily),
        SourceLanguage::Pascal => Some(ExtractionStrategy::PascalFamily),
        SourceLanguage::ObjectiveC => Some(ExtractionStrategy::ObjcFamily),
        SourceLanguage::Swift => Some(ExtractionStrategy::SwiftFamily),
        SourceLanguage::Php => Some(ExtractionStrategy::PhpFamily),
        SourceLanguage::Bash
        | SourceLanguage::Fish
        | SourceLanguage::PowerShell
        | SourceLanguage::Zsh => Some(ExtractionStrategy::ShellFamily),
        SourceLanguage::Java | SourceLanguage::CSharp => Some(ExtractionStrategy::ManagedFamily),
        SourceLanguage::Kotlin | SourceLanguage::Scala | SourceLanguage::Groovy => {
            Some(ExtractionStrategy::JvmDynamicFamily)
        }
        _ => None,
    }
}

const fn fallback_strategy(language: SourceLanguage) -> ExtractionStrategy {
    match language {
        SourceLanguage::Abap
        | SourceLanguage::Astro
        | SourceLanguage::GraphQl
        | SourceLanguage::Html
        | SourceLanguage::Prisma
        | SourceLanguage::Sql
        | SourceLanguage::Yaml => ExtractionStrategy::GenericStructural,
        SourceLanguage::Aura
        | SourceLanguage::Bg3Anubis
        | SourceLanguage::Bg3Resource
        | SourceLanguage::Bg3Stats
        | SourceLanguage::Liquid
        | SourceLanguage::Osiris
        | SourceLanguage::Properties
        | SourceLanguage::Svelte
        | SourceLanguage::Toml
        | SourceLanguage::Vb6
        | SourceLanguage::Visualforce
        | SourceLanguage::Vue
        | SourceLanguage::Xml => ExtractionStrategy::CustomStructural,
        _ => panic!("game scripting registry drifted"),
    }
}

const fn is_game_scripting_language(language: SourceLanguage) -> bool {
    language.is_game_scripting()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_admission_requires_an_executable_strategy() {
        for language in SourceLanguage::ALL {
            let spec = LanguageSpec::for_language(language);
            assert_eq!(spec.language(), language);
            assert!(
                !language.is_native_indexable() || spec.strategy().is_executable(),
                "{} was admitted without an executable extractor",
                language.as_str()
            );
            if spec.strategy() == ExtractionStrategy::ParserOnly {
                assert!(spec.grammar().is_some());
            }
        }
    }

    #[test]
    fn implemented_families_can_be_validated_before_production_admission() {
        for language in [
            SourceLanguage::C,
            SourceLanguage::Cpp,
            SourceLanguage::Cuda,
            SourceLanguage::Glsl,
            SourceLanguage::Hlsl,
            SourceLanguage::Metal,
            SourceLanguage::Slang,
            SourceLanguage::Wesl,
            SourceLanguage::Bash,
            SourceLanguage::Fish,
            SourceLanguage::PowerShell,
            SourceLanguage::Zsh,
            SourceLanguage::Java,
            SourceLanguage::CSharp,
            SourceLanguage::Kotlin,
            SourceLanguage::Scala,
            SourceLanguage::Groovy,
            SourceLanguage::VbNet,
            SourceLanguage::Apex,
            SourceLanguage::Php,
        ] {
            assert!(
                LanguageSpec::for_language(language)
                    .strategy()
                    .is_executable()
            );
            assert!(language.is_native_indexable());
        }
    }
}
