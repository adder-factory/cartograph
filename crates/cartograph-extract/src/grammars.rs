use cartograph_domain::SourceLanguage;
use tree_sitter::Language;

/// Exact-pinned native grammar bindings admitted for the v1 parity program.
///
/// A grammar in this registry proves only that its parser is linked and ABI
/// compatible. Public language support is granted separately, after the
/// language's structural/custom extractor and black-box corpus pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeGrammar {
    /// Represents the abap native grammar.
    Abap,
    /// Represents the Ada native grammar.
    Ada,
    /// Represents the apex native grammar.
    Apex,
    /// Represents the ark ts native grammar.
    ArkTs,
    /// Represents the astro native grammar.
    Astro,
    /// Represents the bash native grammar.
    Bash,
    /// Represents the c native grammar.
    C,
    /// Represents the clojure native grammar.
    Clojure,
    /// Represents the common lisp native grammar.
    CommonLisp,
    /// Represents the cpp native grammar.
    Cpp,
    /// Represents the csharp native grammar.
    CSharp,
    /// Represents the css native grammar.
    Css,
    /// Represents the cuda native grammar.
    Cuda,
    /// Represents the dart native grammar.
    Dart,
    /// Represents the elixir native grammar.
    Elixir,
    /// Represents the embedded template native grammar.
    EmbeddedTemplate,
    /// Represents the fish native grammar.
    Fish,
    /// Represents the fsharp native grammar.
    FSharp,
    /// Represents the glsl native grammar.
    Glsl,
    /// Represents the go native grammar.
    Go,
    /// Represents the graph ql native grammar.
    GraphQl,
    /// Represents the groovy native grammar.
    Groovy,
    /// Represents the haskell native grammar.
    Haskell,
    /// Represents the hcl native grammar.
    Hcl,
    /// Represents the hlsl native grammar.
    Hlsl,
    /// Represents the html native grammar.
    Html,
    /// Represents the java native grammar.
    Java,
    /// Represents the java script native grammar.
    JavaScript,
    /// Represents the js doc native grammar.
    JsDoc,
    /// Represents the JSON native grammar.
    Json,
    /// Represents the julia native grammar.
    Julia,
    /// Represents the kotlin native grammar.
    Kotlin,
    /// Represents the lean native grammar.
    Lean,
    /// Represents the lua native grammar.
    Lua,
    /// Represents the luau native grammar.
    Luau,
    /// Represents the nix native grammar.
    Nix,
    /// Represents the objective c native grammar.
    ObjectiveC,
    /// Represents the ocaml native grammar.
    Ocaml,
    /// Represents the ocaml interface native grammar.
    OcamlInterface,
    /// Represents the pascal native grammar.
    Pascal,
    /// Represents the php native grammar.
    Php,
    /// Represents the power shell native grammar.
    PowerShell,
    /// Represents the prisma native grammar.
    Prisma,
    /// Represents the python native grammar.
    Python,
    /// Represents the r native grammar.
    R,
    /// Represents the regex native grammar.
    Regex,
    /// Represents the re script native grammar.
    ReScript,
    /// Represents the ruby native grammar.
    Ruby,
    /// Represents the rust native grammar.
    Rust,
    /// Represents the scala native grammar.
    Scala,
    /// Represents the Slang shader-language native grammar.
    Slang,
    /// Represents the solidity native grammar.
    Solidity,
    /// Represents the SQL native grammar.
    Sql,
    /// Represents the swift native grammar.
    Swift,
    /// Represents the tsx native grammar.
    Tsx,
    /// Represents the type script native grammar.
    TypeScript,
    /// Represents the visual basic native grammar.
    VisualBasic,
    /// Represents the verilog native grammar.
    Verilog,
    /// Represents the VHDL native grammar.
    Vhdl,
    /// Represents the wgsl native grammar, including `naga_oil` preprocessor imports.
    Wgsl,
    /// Represents the yaml native grammar.
    Yaml,
}

const NATIVE_GRAMMAR_IDS: [&str; 61] = [
    "abap",
    "ada",
    "apex",
    "arkts",
    "astro",
    "bash",
    "c",
    "clojure",
    "common_lisp",
    "cpp",
    "csharp",
    "css",
    "cuda",
    "dart",
    "elixir",
    "embedded_template",
    "fish",
    "fsharp",
    "glsl",
    "go",
    "graphql",
    "groovy",
    "haskell",
    "hcl",
    "hlsl",
    "html",
    "java",
    "javascript",
    "jsdoc",
    "json",
    "julia",
    "kotlin",
    "lean",
    "lua",
    "luau",
    "nix",
    "objc",
    "ocaml",
    "ocaml_interface",
    "pascal",
    "php",
    "powershell",
    "prisma",
    "python",
    "r",
    "regex",
    "rescript",
    "ruby",
    "rust",
    "scala",
    "slang",
    "solidity",
    "sql",
    "swift",
    "tsx",
    "typescript",
    "vbnet",
    "verilog",
    "vhdl",
    "wgsl",
    "yaml",
];

const NATIVE_GRAMMAR_FACTORIES: [fn() -> Language; 61] = [
    || tree_sitter_abap_sqry::language(),
    || tree_sitter_ada::LANGUAGE.into(),
    || tree_sitter_sfapex::apex::LANGUAGE.into(),
    || tree_sitter_arkts::LANGUAGE.into(),
    || tree_sitter_astro_next::LANGUAGE.into(),
    || arborium_bash::language().into(),
    || arborium_c::language().into(),
    || arborium_clojure::language().into(),
    || arborium_commonlisp::language().into(),
    || arborium_cpp::language().into(),
    || arborium_c_sharp::language().into(),
    || arborium_css::language().into(),
    || tree_sitter_cuda::LANGUAGE.into(),
    || arborium_dart::language().into(),
    || arborium_elixir::language().into(),
    || tree_sitter_embedded_template::LANGUAGE.into(),
    || arborium_fish::language().into(),
    || arborium_fsharp::language().into(),
    || arborium_glsl::language().into(),
    || tree_sitter_go::LANGUAGE.into(),
    || arborium_graphql::language().into(),
    || arborium_groovy::language().into(),
    || arborium_haskell::language().into(),
    || arborium_hcl::language().into(),
    || arborium_hlsl::language().into(),
    || arborium_html::language().into(),
    || arborium_java::language().into(),
    || tree_sitter_javascript::LANGUAGE.into(),
    || arborium_jsdoc::language().into(),
    || arborium_json::language().into(),
    || arborium_julia::language().into(),
    || arborium_kotlin::language().into(),
    || arborium_lean::language().into(),
    || arborium_lua::language().into(),
    || tree_sitter_luau::LANGUAGE.into(),
    || arborium_nix::language().into(),
    || arborium_objc::language().into(),
    || tree_sitter_ocaml::LANGUAGE_OCAML.into(),
    || tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into(),
    || tree_sitter_pascal::LANGUAGE.into(),
    || arborium_php::language().into(),
    || arborium_powershell::language().into(),
    || tree_sitter_prisma_io::LANGUAGE.into(),
    || tree_sitter_python::LANGUAGE.into(),
    || arborium_r::language().into(),
    || arborium_regex::language().into(),
    || arborium_rescript::language().into(),
    || arborium_ruby::language().into(),
    || tree_sitter_rust::LANGUAGE.into(),
    || arborium_scala::language().into(),
    || tree_sitter_slang::LANGUAGE_SLANG.into(),
    || arborium_solidity::language().into(),
    || arborium_sql::language().into(),
    || arborium_swift::language().into(),
    || tree_sitter_typescript::LANGUAGE_TSX.into(),
    || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    || arborium_vb::language().into(),
    || arborium_verilog::language().into(),
    || arborium_vhdl::language().into(),
    || tree_sitter_wgsl_bevy::LANGUAGE.into(),
    || arborium_yaml::language().into(),
];

impl NativeGrammar {
    /// Every admitted binding in stable language-id order.
    pub const ALL: [Self; 61] = [
        Self::Abap,
        Self::Ada,
        Self::Apex,
        Self::ArkTs,
        Self::Astro,
        Self::Bash,
        Self::C,
        Self::Clojure,
        Self::CommonLisp,
        Self::Cpp,
        Self::CSharp,
        Self::Css,
        Self::Cuda,
        Self::Dart,
        Self::Elixir,
        Self::EmbeddedTemplate,
        Self::Fish,
        Self::FSharp,
        Self::Glsl,
        Self::Go,
        Self::GraphQl,
        Self::Groovy,
        Self::Haskell,
        Self::Hcl,
        Self::Hlsl,
        Self::Html,
        Self::Java,
        Self::JavaScript,
        Self::JsDoc,
        Self::Json,
        Self::Julia,
        Self::Kotlin,
        Self::Lean,
        Self::Lua,
        Self::Luau,
        Self::Nix,
        Self::ObjectiveC,
        Self::Ocaml,
        Self::OcamlInterface,
        Self::Pascal,
        Self::Php,
        Self::PowerShell,
        Self::Prisma,
        Self::Python,
        Self::R,
        Self::Regex,
        Self::ReScript,
        Self::Ruby,
        Self::Rust,
        Self::Scala,
        Self::Slang,
        Self::Solidity,
        Self::Sql,
        Self::Swift,
        Self::Tsx,
        Self::TypeScript,
        Self::VisualBasic,
        Self::Verilog,
        Self::Vhdl,
        Self::Wgsl,
        Self::Yaml,
    ];

    /// Stable v1 storage/protocol language id represented by this binding.
    #[must_use]
    pub const fn stable_id(self) -> &'static str {
        NATIVE_GRAMMAR_IDS[self as usize]
    }

    /// Statically linked Tree-sitter grammar with no runtime loading or network.
    #[must_use]
    pub fn language(self) -> Language {
        NATIVE_GRAMMAR_FACTORIES[self as usize]()
    }

    /// Native grammar binding selected for every grammar-backed mode.
    /// Bounded custom languages intentionally return `None`.
    #[must_use]
    pub const fn for_source_language(language: SourceLanguage) -> Option<Self> {
        if let Some(grammar) = grammar_group_a(language) {
            return Some(grammar);
        }
        if let Some(grammar) = grammar_group_b(language) {
            return Some(grammar);
        }
        if let Some(grammar) = grammar_group_c(language) {
            return Some(grammar);
        }
        if let Some(grammar) = grammar_group_d(language) {
            return Some(grammar);
        }
        if let Some(grammar) = grammar_group_e(language) {
            return Some(grammar);
        }
        grammar_group_f(language)
    }
}

const fn grammar_group_a(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Abap => Some(NativeGrammar::Abap),
        SourceLanguage::Ada => Some(NativeGrammar::Ada),
        SourceLanguage::Apex => Some(NativeGrammar::Apex),
        SourceLanguage::ArkTs => Some(NativeGrammar::ArkTs),
        SourceLanguage::Astro => Some(NativeGrammar::Astro),
        SourceLanguage::Bash | SourceLanguage::Zsh => Some(NativeGrammar::Bash),
        SourceLanguage::C => Some(NativeGrammar::C),
        SourceLanguage::Clojure => Some(NativeGrammar::Clojure),
        SourceLanguage::CommonLisp => Some(NativeGrammar::CommonLisp),
        SourceLanguage::Cpp => Some(NativeGrammar::Cpp),
        SourceLanguage::CSharp => Some(NativeGrammar::CSharp),
        _ => None,
    }
}

const fn grammar_group_b(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Css => Some(NativeGrammar::Css),
        SourceLanguage::Cuda => Some(NativeGrammar::Cuda),
        SourceLanguage::Dart => Some(NativeGrammar::Dart),
        SourceLanguage::Elixir => Some(NativeGrammar::Elixir),
        SourceLanguage::EmbeddedTemplate => Some(NativeGrammar::EmbeddedTemplate),
        SourceLanguage::Fish => Some(NativeGrammar::Fish),
        SourceLanguage::FSharp => Some(NativeGrammar::FSharp),
        SourceLanguage::Glsl => Some(NativeGrammar::Glsl),
        SourceLanguage::Go => Some(NativeGrammar::Go),
        SourceLanguage::GraphQl => Some(NativeGrammar::GraphQl),
        SourceLanguage::Groovy => Some(NativeGrammar::Groovy),
        _ => None,
    }
}

const fn grammar_group_c(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Haskell => Some(NativeGrammar::Haskell),
        SourceLanguage::Hcl => Some(NativeGrammar::Hcl),
        SourceLanguage::Hlsl => Some(NativeGrammar::Hlsl),
        SourceLanguage::Html => Some(NativeGrammar::Html),
        SourceLanguage::Java => Some(NativeGrammar::Java),
        SourceLanguage::JavaScript | SourceLanguage::Jsx => Some(NativeGrammar::JavaScript),
        SourceLanguage::JsDoc => Some(NativeGrammar::JsDoc),
        SourceLanguage::Json | SourceLanguage::Jupyter => Some(NativeGrammar::Json),
        SourceLanguage::Julia => Some(NativeGrammar::Julia),
        SourceLanguage::Khn | SourceLanguage::Lua => Some(NativeGrammar::Lua),
        _ => None,
    }
}

const fn grammar_group_d(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Kotlin => Some(NativeGrammar::Kotlin),
        SourceLanguage::Lean => Some(NativeGrammar::Lean),
        SourceLanguage::Luau => Some(NativeGrammar::Luau),
        SourceLanguage::Nix => Some(NativeGrammar::Nix),
        SourceLanguage::ObjectiveC => Some(NativeGrammar::ObjectiveC),
        SourceLanguage::Ocaml => Some(NativeGrammar::Ocaml),
        SourceLanguage::OcamlInterface => Some(NativeGrammar::OcamlInterface),
        SourceLanguage::Pascal => Some(NativeGrammar::Pascal),
        SourceLanguage::Php => Some(NativeGrammar::Php),
        SourceLanguage::PowerShell => Some(NativeGrammar::PowerShell),
        _ => None,
    }
}

const fn grammar_group_e(language: SourceLanguage) -> Option<NativeGrammar> {
    if let Some(grammar) = grammar_group_e_data(language) {
        return Some(grammar);
    }
    grammar_group_e_systems(language)
}

const fn grammar_group_e_data(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Prisma => Some(NativeGrammar::Prisma),
        SourceLanguage::Python => Some(NativeGrammar::Python),
        SourceLanguage::R => Some(NativeGrammar::R),
        SourceLanguage::Regex => Some(NativeGrammar::Regex),
        SourceLanguage::ReScript => Some(NativeGrammar::ReScript),
        _ => None,
    }
}

const fn grammar_group_e_systems(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Ruby => Some(NativeGrammar::Ruby),
        SourceLanguage::Rust => Some(NativeGrammar::Rust),
        SourceLanguage::Scala => Some(NativeGrammar::Scala),
        SourceLanguage::Slang => Some(NativeGrammar::Slang),
        SourceLanguage::Solidity => Some(NativeGrammar::Solidity),
        SourceLanguage::Sql => Some(NativeGrammar::Sql),
        _ => None,
    }
}

const fn grammar_group_f(language: SourceLanguage) -> Option<NativeGrammar> {
    match language {
        SourceLanguage::Swift => Some(NativeGrammar::Swift),
        SourceLanguage::Tsx => Some(NativeGrammar::Tsx),
        SourceLanguage::TypeScript => Some(NativeGrammar::TypeScript),
        SourceLanguage::VbNet => Some(NativeGrammar::VisualBasic),
        SourceLanguage::Metal => Some(NativeGrammar::Cpp),
        SourceLanguage::Verilog => Some(NativeGrammar::Verilog),
        SourceLanguage::Vhdl => Some(NativeGrammar::Vhdl),
        SourceLanguage::Wesl | SourceLanguage::Wgsl => Some(NativeGrammar::Wgsl),
        SourceLanguage::Yaml => Some(NativeGrammar::Yaml),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_admitted_native_grammar_is_unique_sorted_and_abi_compatible() {
        let ids = NativeGrammar::ALL.map(NativeGrammar::stable_id);
        assert!(ids.is_sorted_by(|previous, next| previous < next));
        for (ordinal, grammar) in NativeGrammar::ALL.into_iter().enumerate() {
            assert_eq!(grammar as usize, ordinal, "grammar registry order drifted");
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&grammar.language())
                .unwrap_or_else(|error| {
                    panic!("{} grammar is incompatible: {error}", grammar.stable_id())
                });
        }
    }

    #[test]
    fn grammar_mapping_covers_exact_v1_grammar_and_custom_counts() {
        let v1_languages = SourceLanguage::ALL
            .into_iter()
            .filter(|language| language.is_v1_language())
            .collect::<Vec<_>>();
        let grammar_backed = v1_languages
            .iter()
            .copied()
            .filter(|language| NativeGrammar::for_source_language(*language).is_some())
            .count();
        assert_eq!(grammar_backed, 61);
        assert_eq!(v1_languages.len() - grammar_backed, 12);
        assert_eq!(
            NativeGrammar::for_source_language(SourceLanguage::Toml),
            None
        );

        for (ordinal, language) in SourceLanguage::ALL.into_iter().enumerate() {
            assert_eq!(
                language as usize, ordinal,
                "language registry order drifted"
            );
            let spec = crate::LanguageSpec::for_language(language);
            assert_eq!(
                NativeGrammar::for_source_language(language).is_some(),
                spec.strategy() != crate::ExtractionStrategy::CustomStructural,
                "{} grammar/custom routing drifted",
                language.as_str()
            );
        }
    }
}
