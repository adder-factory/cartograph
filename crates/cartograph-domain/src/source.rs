//! Storage-independent source-code contracts shared by extraction and indexing.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

mod go_manifest;

const MAX_NORMALIZED_PATH_BYTES: usize = 4_096;
const LITERAL_SIGNATURE_KEYWORDS: &[&str] = &[
    "default",
    "false",
    "nil",
    "none",
    "null",
    "true",
    "undefined",
];

/// Return whether a callable signature is safe to copy into a search document.
///
/// Extractors may retain declaration syntax in structural storage, but search text must not
/// retain literal defaults. This conservative, language-independent classifier rejects quoted
/// values, assignment/default syntax, numeric tokens, and common literal keywords. Identifiers
/// that merely contain digits, such as `u32` and `UTF8`, remain admissible.
#[must_use]
pub fn callable_signature_is_literal_free(signature: &str) -> bool {
    if signature
        .bytes()
        .any(|byte| matches!(byte, b'=' | b'\'' | b'"' | b'`'))
    {
        return false;
    }
    signature
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .all(|token| {
            !token.as_bytes()[0].is_ascii_digit()
                && !LITERAL_SIGNATURE_KEYWORDS
                    .iter()
                    .any(|keyword| token.eq_ignore_ascii_case(keyword))
        })
}

/// Return whether a declaration value is safe and useful to retain in search text.
///
/// The accepted shape is an identifier/reference expression, not an arbitrary literal. Common
/// credential names and provider token prefixes are rejected even when unquoted, as are
/// high-entropy token-shaped values. Callers still own a strict byte limit before copying.
#[must_use]
pub fn declaration_value_is_search_safe(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty()
        || value.contains("$(")
        || !callable_signature_is_literal_free(value)
        || !value.bytes().all(is_safe_reference_expression_byte)
    {
        return false;
    }
    let mut token_count = 0_usize;
    for token in value
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
    {
        token_count = token_count.saturating_add(1);
        if value_token_is_sensitive(token) {
            return false;
        }
    }
    token_count == 1 || (token_count > 1 && value.bytes().any(is_reference_expression_operator))
}

/// Return whether a symbol signature is safe and semantically useful to persist.
///
/// Callable declarations retain only literal-free type/parameter syntax. Variables and constants
/// retain only a conservative identifier/reference initializer. Field/property signatures retain
/// type-only syntax. Type aliases retain their declaration only when the right-hand side is
/// literal-free type syntax. Other symbol categories use their qualified name as search code.
#[must_use]
pub fn symbol_signature_is_search_safe(kind: SymbolKind, signature: &str) -> bool {
    let signature = signature.trim();
    if signature.is_empty() {
        return false;
    }
    match kind {
        SymbolKind::Function
        | SymbolKind::Method
        | SymbolKind::Component
        | SymbolKind::Field
        | SymbolKind::Property
        | SymbolKind::Parameter => callable_signature_is_literal_free(signature),
        SymbolKind::Variable | SymbolKind::Constant => signature
            .strip_prefix('=')
            .is_some_and(|value| declaration_value_is_search_safe(value.trim())),
        SymbolKind::TypeAlias => signature
            .split_once('=')
            .is_some_and(|(declaration, value)| {
                type_alias_name(declaration)
                    .is_some_and(|name| name.bytes().all(is_identifier_byte))
                    && callable_signature_is_literal_free(value.trim().trim_end_matches(';'))
            }),
        _ => false,
    }
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn type_alias_name(declaration: &str) -> Option<&str> {
    let mut tokens = declaration.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "type" {
            return tokens
                .next()
                .and_then(|name| name.split('<').next())
                .filter(|name| !name.is_empty());
        }
    }
    None
}

const fn is_safe_reference_expression_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'_' | b'$'
                | b'{'
                | b'}'
                | b'.'
                | b':'
                | b'('
                | b')'
                | b'['
                | b']'
                | b'*'
                | b'&'
                | b'|'
                | b'!'
                | b'~'
                | b'+'
                | b'-'
                | b'/'
                | b'%'
                | b'<'
                | b'>'
                | b'?'
                | b','
                | b' '
                | b'\t'
                | b'\r'
                | b'\n'
        )
}

const fn is_reference_expression_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'$' | b'.'
            | b':'
            | b'('
            | b')'
            | b'['
            | b']'
            | b'*'
            | b'&'
            | b'|'
            | b'!'
            | b'~'
            | b'+'
            | b'-'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'?'
            | b','
    )
}

/// Prefixes of provider-issued access keys and tokens (AWS, GitHub, GitLab,
/// Stripe, Slack), in the case each provider issues them. Hyphenated forms
/// only match callers that keep `-` inside a segment; identifier tokens never
/// contain it.
const CREDENTIAL_TOKEN_PREFIXES: &[&str] = &[
    "AKIA",
    "ASIA",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk_live_",
    "sk_test_",
    "xoxb_",
    "xoxb-",
    "xoxp_",
    "xoxp-",
];

/// Shortest key body after a provider prefix; issued keys carry 16 or more.
const MINIMUM_PROVIDER_KEY_BODY_BYTES: usize = 8;

/// Return whether one token starts with a provider access-key prefix in any
/// case. This is the conservative half of the declaration-value screen.
fn token_has_credential_prefix(token: &str) -> bool {
    CREDENTIAL_TOKEN_PREFIXES
        .iter()
        .any(|prefix| starts_with_ignore_ascii_case(token, prefix))
}

/// Return whether one path segment is shaped like an issued provider access
/// key: a case-insensitive provider prefix (`AKIA`, `sk_live_`, `ghp_`,
/// `glpat-`) followed by a key body of at least eight `[A-Za-z0-9_-]` bytes.
///
/// Unlike the declaration screen this never matches an ordinary directory or
/// module name such as `asia-east1`, `AsiaToken.sol`, or `token`, so it is the
/// screen for path-like specifiers that may name real directories.
#[must_use]
pub fn token_has_provider_key_shape(segment: &str) -> bool {
    CREDENTIAL_TOKEN_PREFIXES.iter().any(|prefix| {
        starts_with_ignore_ascii_case(segment, prefix)
            && segment.get(prefix.len()..).is_some_and(|body| {
                body.len() >= MINIMUM_PROVIDER_KEY_BODY_BYTES
                    && body
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
    })
}

/// Return whether one identifier token (`[A-Za-z0-9_]`) is a credential word
/// (`password`, `api_key`, ...), a provider access key, or a high-entropy
/// secret — the same screen [`declaration_value_is_search_safe`] applies.
#[must_use]
pub fn value_token_is_sensitive(token: &str) -> bool {
    is_sensitive_value_token(token) || looks_like_high_entropy_token(token)
}

/// Whether a name identifies credential material, without the entropy heuristic.
/// Query and fragment parameter names share this declaration-value vocabulary.
#[must_use]
pub fn is_sensitive_value_token(token: &str) -> bool {
    const EXACT: &[&str] = &[
        "apikey",
        "authtoken",
        "clientsecret",
        "credential",
        "credentials",
        "password",
        "passwd",
        "privatekey",
        "secret",
        "token",
    ];
    if EXACT.iter().any(|word| token.eq_ignore_ascii_case(word))
        || token_has_credential_prefix(token)
    {
        return true;
    }
    let mut prior_key_prefix = false;
    for component in token.split('_').filter(|component| !component.is_empty()) {
        if matches_ignore_ascii_case(
            component,
            &[
                "credential",
                "credentials",
                "password",
                "passwd",
                "secret",
                "token",
            ],
        ) || (prior_key_prefix
            && matches_ignore_ascii_case(component, &["key", "secret", "token"]))
        {
            return true;
        }
        prior_key_prefix =
            matches_ignore_ascii_case(component, &["api", "access", "private", "client"]);
    }
    false
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn looks_like_high_entropy_token(token: &str) -> bool {
    token.len() >= 24
        && token.bytes().any(|byte| byte.is_ascii_lowercase())
        && token.bytes().any(|byte| byte.is_ascii_uppercase())
        && token.bytes().any(|byte| byte.is_ascii_digit())
}

macro_rules! impl_stable_as_str {
    ($type:ty, $($variant:pat => $value:literal),+ $(,)?) => {
        impl $type {
            /// Stable storage, search, and protocol representation.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($variant => $value),+
                }
            }
        }
    };
}

/// Parser outcome recorded for one source file in an immutable generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileParseStatus {
    /// The complete file was parsed and extracted.
    Parsed,
    /// Useful facts were extracted despite recoverable parse gaps.
    Partial,
    /// Parsing failed and no structural facts are trusted.
    Failed,
    /// Project policy deliberately excluded the file from parsing.
    Skipped,
}

impl FileParseStatus {
    /// Stable PostgreSQL representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

macro_rules! source_languages {
    (
        $(
            $variant:ident => {
                stable: $stable:literal,
                v1_extensions: [$($v1_extension:literal),* $(,)?],
                additions: [$($additional_extension:literal),* $(,)?],
                native: $native:literal
            }
        ),+ $(,)?
    ) => {
        /// Every canonical language identifier carried forward from v1.1.33,
        /// plus explicitly tracked additive v2 language modes.
        ///
        /// `native: true` is deliberately part of the same declaration. It is
        /// granted only after extraction, publication, and retrieval are all
        /// executable; grammar linkage by itself never enables import/indexing.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum SourceLanguage {
            $(
                #[doc = concat!("The `", $stable, "` source language.")]
                #[serde(rename = $stable)]
                $variant,
            )+
        }

        impl SourceLanguage {
            /// Exhaustive stable language set. Generated with the enum so a
            /// future variant cannot be omitted from the registry.
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] = [$(Self::$variant),+];

            /// Stable storage, search, and protocol representation.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $stable),+
                }
            }

            /// Extensions declared by the immutable v1.1.33 registry.
            #[must_use]
            pub const fn v1_extensions(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => &[$($v1_extension),*]),+
                }
            }

            /// Additive v2 extensions which do not inflate the v1 parity count.
            #[must_use]
            pub const fn additional_extensions(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => &[$($additional_extension),*]),+
                }
            }

            /// Whether this language currently has an end-to-end native path.
            /// This must be true for every variant before the v2.0.0 release.
            #[must_use]
            pub const fn is_native_indexable(self) -> bool {
                match self {
                    $(Self::$variant => $native),+
                }
            }

            /// Whether this mode belongs to the immutable v1.1.33 parity floor.
            /// Additive v2 modes are deliberately excluded from the frozen v1 digest.
            #[must_use]
            pub const fn is_v1_language(self) -> bool {
                !self.v1_extensions().is_empty() || matches!(self, Self::Bg3Stats)
            }

            /// Parse any stable language identifier known to the v2 registry.
            #[must_use]
            pub fn from_stable_str(value: &str) -> Option<Self> {
                match value {
                    $($stable => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

// Stable ids are sorted so deterministic iteration is useful to storage,
// protocol, release-manifest, and test consumers. The 163 v1 extension entries
// below are preserved exactly; `.pyi` is an additive v2 improvement.
source_languages!(
    Abap => { stable: "abap", v1_extensions: [".abap"], additions: [], native: true },
    ActionScript => { stable: "action_script", v1_extensions: [], additions: [".as"], native: true },
    Ada => { stable: "ada", v1_extensions: [], additions: [".adb", ".ads", ".ada"], native: true },
    AgsScript => { stable: "ags_script", v1_extensions: [], additions: [".asc", ".ash"], native: true },
    AngelScript => { stable: "angel_script", v1_extensions: [], additions: [".angelscript", ".as"], native: true },
    Apex => { stable: "apex", v1_extensions: [".cls", ".trigger"], additions: [], native: true },
    ArkTs => { stable: "arkts", v1_extensions: [".ets"], additions: [], native: true },
    Astro => { stable: "astro", v1_extensions: [".astro"], additions: [], native: true },
    Aura => { stable: "aura", v1_extensions: [".cmp", ".app", ".evt", ".intf", ".design", ".auradoc"], additions: [], native: true },
    Bash => { stable: "bash", v1_extensions: [".sh", ".bash"], additions: [], native: true },
    Bg3Anubis => { stable: "bg3_anubis", v1_extensions: [".ann", ".anc"], additions: [], native: true },
    Bg3Resource => { stable: "bg3_resource", v1_extensions: [".lsx", ".lsf", ".lsfx", ".lsefx", ".tbl", ".stats", ".mei", ".lsj"], additions: [], native: true },
    Bg3Stats => { stable: "bg3_stats", v1_extensions: [], additions: [], native: true },
    Boo => { stable: "boo", v1_extensions: [], additions: [".boo"], native: true },
    ByondDm => { stable: "byond_dm", v1_extensions: [], additions: [".dm"], native: true },
    C => { stable: "c", v1_extensions: [".c", ".h"], additions: [], native: true },
    ChoiceScript => { stable: "choice_script", v1_extensions: [], additions: [], native: true },
    Clojure => { stable: "clojure", v1_extensions: [".clj", ".cljs", ".cljc", ".edn", ".bb"], additions: [], native: true },
    CommonLisp => { stable: "common_lisp", v1_extensions: [".lisp", ".lsp", ".l", ".cl", ".asd", ".ros"], additions: [], native: true },
    Cpp => { stable: "cpp", v1_extensions: [".cpp", ".cc", ".cxx", ".hpp", ".hxx"], additions: [], native: true },
    CSharp => { stable: "csharp", v1_extensions: [".cs"], additions: [], native: true },
    Css => { stable: "css", v1_extensions: [".css"], additions: [], native: true },
    Cuda => { stable: "cuda", v1_extensions: [".cu", ".cuh"], additions: [], native: true },
    Daedalus => { stable: "daedalus", v1_extensions: [], additions: [".d"], native: true },
    Dart => { stable: "dart", v1_extensions: [".dart"], additions: [], native: true },
    DoomAcs => { stable: "doom_acs", v1_extensions: [], additions: [".acs"], native: true },
    DoomDecorate => { stable: "doom_decorate", v1_extensions: [], additions: [], native: true },
    Elixir => { stable: "elixir", v1_extensions: [".ex", ".exs"], additions: [], native: true },
    EmbeddedTemplate => { stable: "embedded_template", v1_extensions: [".erb", ".ejs", ".eta", ".etlua"], additions: [], native: true },
    EnforceScript => { stable: "enforce_script", v1_extensions: [], additions: [".c"], native: true },
    Fish => { stable: "fish", v1_extensions: [".fish"], additions: [], native: true },
    FSharp => { stable: "fsharp", v1_extensions: [".fs", ".fsx"], additions: [], native: true },
    Galaxy => { stable: "galaxy", v1_extensions: [], additions: [".galaxy"], native: true },
    GameMakerLanguage => { stable: "game_maker_language", v1_extensions: [], additions: [".gml"], native: true },
    GameMonkey => { stable: "game_monkey", v1_extensions: [], additions: [".gm"], native: true },
    GdScript => { stable: "gdscript", v1_extensions: [], additions: [".gd"], native: true },
    Glsl => { stable: "glsl", v1_extensions: [".glsl", ".vert", ".frag", ".comp", ".geom", ".tesc", ".tese"], additions: [], native: true },
    Go => { stable: "go", v1_extensions: [".go"], additions: [], native: true },
    GraphQl => { stable: "graphql", v1_extensions: [".graphql", ".gql"], additions: [], native: true },
    Groovy => { stable: "groovy", v1_extensions: [".groovy", ".gradle"], additions: [], native: true },
    Gsc => { stable: "gsc", v1_extensions: [], additions: [".gsc", ".csc", ".gsh"], native: true },
    HaloScript => { stable: "halo_script", v1_extensions: [], additions: [".hsc"], native: true },
    Haskell => { stable: "haskell", v1_extensions: [".hs"], additions: [], native: true },
    Hcl => { stable: "hcl", v1_extensions: [".tf", ".tfvars", ".hcl", ".tofu"], additions: [], native: true },
    Hlsl => { stable: "hlsl", v1_extensions: [".hlsl", ".hlsli", ".fx", ".fxh"], additions: [], native: true },
    Hscript => { stable: "hscript", v1_extensions: [], additions: [".hscript"], native: true },
    Html => { stable: "html", v1_extensions: [".html", ".htm"], additions: [], native: true },
    IdTechScript => { stable: "idtech_script", v1_extensions: [], additions: [".script"], native: true },
    Inform6 => { stable: "inform6", v1_extensions: [], additions: [".inf"], native: true },
    Inform7 => { stable: "inform7", v1_extensions: [], additions: [".ni", ".i7x"], native: true },
    Ink => { stable: "ink", v1_extensions: [], additions: [".ink"], native: true },
    Jass => { stable: "jass", v1_extensions: [], additions: [".j"], native: true },
    Java => { stable: "java", v1_extensions: [".java"], additions: [], native: true },
    JavaScript => { stable: "javascript", v1_extensions: [".js", ".mjs", ".cjs", ".xsjs", ".xsjslib"], additions: [], native: true },
    JsDoc => { stable: "jsdoc", v1_extensions: [".jsdoc"], additions: [], native: true },
    Json => { stable: "json", v1_extensions: [".json"], additions: [], native: true },
    Jsx => { stable: "jsx", v1_extensions: [".jsx"], additions: [], native: true },
    Julia => { stable: "julia", v1_extensions: [".jl"], additions: [], native: true },
    Jupyter => { stable: "jupyter", v1_extensions: [".ipynb"], additions: [], native: true },
    KerboScript => { stable: "kerboscript", v1_extensions: [], additions: [".ks"], native: true },
    Khn => { stable: "khn", v1_extensions: [".khn"], additions: [], native: true },
    Kotlin => { stable: "kotlin", v1_extensions: [".kt", ".kts"], additions: [], native: true },
    Lean => { stable: "lean", v1_extensions: [".lean"], additions: [], native: true },
    Liquid => { stable: "liquid", v1_extensions: [".liquid"], additions: [], native: true },
    Lpc => { stable: "lpc", v1_extensions: [], additions: [".c"], native: true },
    Lsl => { stable: "lsl", v1_extensions: [], additions: [".lsl"], native: true },
    Lua => { stable: "lua", v1_extensions: [".lua"], additions: [], native: true },
    Luau => { stable: "luau", v1_extensions: [".luau"], additions: [], native: true },
    Metal => { stable: "metal", v1_extensions: [], additions: [".metal"], native: true },
    MinecraftFunction => { stable: "minecraft_function", v1_extensions: [], additions: [".mcfunction"], native: true },
    MiniScript => { stable: "miniscript", v1_extensions: [], additions: [".ms"], native: true },
    Nix => { stable: "nix", v1_extensions: [".nix"], additions: [], native: true },
    NwScript => { stable: "nwscript", v1_extensions: [], additions: [".nss"], native: true },
    ObjectiveC => { stable: "objc", v1_extensions: [".m", ".mm"], additions: [], native: true },
    Ocaml => { stable: "ocaml", v1_extensions: [".ml"], additions: [], native: true },
    OcamlInterface => { stable: "ocaml_interface", v1_extensions: [".mli"], additions: [], native: true },
    Osiris => { stable: "osiris", v1_extensions: [".div"], additions: [], native: true },
    Papyrus => { stable: "papyrus", v1_extensions: [], additions: [".psc"], native: true },
    ParadoxScript => { stable: "paradox_script", v1_extensions: [], additions: [], native: true },
    Pascal => { stable: "pascal", v1_extensions: [".pas", ".dpr", ".dpk", ".lpr", ".dfm", ".fmx"], additions: [], native: true },
    Pawn => { stable: "pawn", v1_extensions: [], additions: [".pwn", ".sma"], native: true },
    Php => { stable: "php", v1_extensions: [".php", ".module", ".install", ".theme", ".inc"], additions: [], native: true },
    Pico8 => { stable: "pico8", v1_extensions: [], additions: [".p8"], native: true },
    PowerShell => { stable: "powershell", v1_extensions: [".ps1", ".psm1", ".psd1"], additions: [], native: true },
    Prisma => { stable: "prisma", v1_extensions: [".prisma"], additions: [], native: true },
    Properties => { stable: "properties", v1_extensions: [".properties"], additions: [], native: true },
    Python => { stable: "python", v1_extensions: [".py", ".pyw"], additions: [".pyi"], native: true },
    QuakeC => { stable: "quakec", v1_extensions: [], additions: [".qc"], native: true },
    R => { stable: "r", v1_extensions: [".r"], additions: [], native: true },
    Redscript => { stable: "redscript", v1_extensions: [], additions: [".reds"], native: true },
    Regex => { stable: "regex", v1_extensions: [".regex", ".regexp"], additions: [], native: true },
    Renpy => { stable: "renpy", v1_extensions: [], additions: [".rpy"], native: true },
    ReScript => { stable: "rescript", v1_extensions: [".res", ".resi"], additions: [], native: true },
    Rhai => { stable: "rhai", v1_extensions: [], additions: [".rhai"], native: true },
    Ruby => { stable: "ruby", v1_extensions: [".rb", ".rake"], additions: [], native: true },
    Rust => { stable: "rust", v1_extensions: [".rs"], additions: [], native: true },
    Scala => { stable: "scala", v1_extensions: [".scala", ".sc"], additions: [], native: true },
    Skript => { stable: "skript", v1_extensions: [], additions: [".sk"], native: true },
    Slang => { stable: "slang", v1_extensions: [], additions: [".slang"], native: true },
    Solidity => { stable: "solidity", v1_extensions: [".sol"], additions: [], native: true },
    SourcePawn => { stable: "sourcepawn", v1_extensions: [], additions: [".sp"], native: true },
    Sqf => { stable: "sqf", v1_extensions: [], additions: [".sqf", ".hqf"], native: true },
    Sql => { stable: "sql", v1_extensions: [".sql", ".ddl", ".dml"], additions: [], native: true },
    Sqs => { stable: "sqs", v1_extensions: [], additions: [".sqs"], native: true },
    Squirrel => { stable: "squirrel", v1_extensions: [], additions: [".nut"], native: true },
    Svelte => { stable: "svelte", v1_extensions: [".svelte"], additions: [], native: true },
    Swift => { stable: "swift", v1_extensions: [".swift"], additions: [], native: true },
    Tads => { stable: "tads", v1_extensions: [], additions: [".t"], native: true },
    Toml => { stable: "toml", v1_extensions: [], additions: [".toml"], native: true },
    TorqueScript => { stable: "torque_script", v1_extensions: [], additions: [".cs", ".gui", ".mis"], native: true },
    Tsx => { stable: "tsx", v1_extensions: [".tsx"], additions: [], native: true },
    Twee => { stable: "twee", v1_extensions: [], additions: [".twee", ".tw"], native: true },
    TypeScript => { stable: "typescript", v1_extensions: [".ts", ".mts", ".cts"], additions: [], native: true },
    UnrealScript => { stable: "unrealscript", v1_extensions: [], additions: [".uc"], native: true },
    ValveQc => { stable: "valve_qc", v1_extensions: [], additions: [".qci", ".qc"], native: true },
    Vb6 => { stable: "vb6", v1_extensions: [".bas", ".frm", ".ctl", ".dob", ".dsr", ".pag", ".vbp"], additions: [], native: true },
    VbNet => { stable: "vbnet", v1_extensions: [".vb"], additions: [], native: true },
    Verilog => { stable: "verilog", v1_extensions: [".v", ".vh", ".sv", ".svh"], additions: [], native: true },
    Verse => { stable: "verse", v1_extensions: [], additions: [".verse"], native: true },
    Vhdl => { stable: "vhdl", v1_extensions: [], additions: [".vhd", ".vhdl"], native: true },
    Visualforce => { stable: "visualforce", v1_extensions: [".page", ".component"], additions: [], native: true },
    Vue => { stable: "vue", v1_extensions: [".vue"], additions: [], native: true },
    Wesl => { stable: "wesl", v1_extensions: [], additions: [".wesl"], native: true },
    Wgsl => { stable: "wgsl", v1_extensions: [], additions: [".wgsl"], native: true },
    WitcherScript => { stable: "witcher_script", v1_extensions: [], additions: [".ws"], native: true },
    Wren => { stable: "wren", v1_extensions: [], additions: [".wren"], native: true },
    WurstScript => { stable: "wurstscript", v1_extensions: [], additions: [".wurst"], native: true },
    Xml => { stable: "xml", v1_extensions: [".xml"], additions: [], native: true },
    Yaml => { stable: "yaml", v1_extensions: [".yml", ".yaml"], additions: [], native: true },
    YarnSpinner => { stable: "yarn_spinner", v1_extensions: [], additions: [".yarn"], native: true },
    Zscript => { stable: "zscript", v1_extensions: [], additions: [".zs"], native: true },
    Zsh => { stable: "zsh", v1_extensions: [".zsh", ".zshrc", ".zshenv", ".zprofile", ".zlogin"], additions: [], native: true },
);

impl SourceLanguage {
    /// Whether this is a dedicated textual game, modding, or interactive-fiction
    /// scripting mode added beyond the frozen v1 language floor.
    #[must_use]
    pub const fn is_game_scripting(self) -> bool {
        matches!(
            self,
            Self::ActionScript
                | Self::AgsScript
                | Self::AngelScript
                | Self::Boo
                | Self::ByondDm
                | Self::ChoiceScript
                | Self::Daedalus
                | Self::DoomAcs
                | Self::DoomDecorate
                | Self::EnforceScript
                | Self::Galaxy
                | Self::GameMakerLanguage
                | Self::GameMonkey
                | Self::GdScript
                | Self::Gsc
                | Self::HaloScript
                | Self::Hscript
                | Self::IdTechScript
                | Self::Inform6
                | Self::Inform7
                | Self::Ink
                | Self::Jass
                | Self::KerboScript
                | Self::Lpc
                | Self::Lsl
                | Self::MinecraftFunction
                | Self::MiniScript
                | Self::NwScript
                | Self::Papyrus
                | Self::ParadoxScript
                | Self::Pawn
                | Self::Pico8
                | Self::QuakeC
                | Self::Redscript
                | Self::Renpy
                | Self::Rhai
                | Self::Skript
                | Self::SourcePawn
                | Self::Sqf
                | Self::Sqs
                | Self::Squirrel
                | Self::Tads
                | Self::TorqueScript
                | Self::Twee
                | Self::UnrealScript
                | Self::ValveQc
                | Self::Verse
                | Self::WitcherScript
                | Self::Wren
                | Self::WurstScript
                | Self::YarnSpinner
                | Self::Zscript
        )
    }

    /// Detect a known language using the bounded v1 path/content contract.
    /// The result may still be pending native extraction.
    #[must_use]
    pub fn detect(path: &str, source: Option<&str>) -> Option<Self> {
        detect_source_language(path, source)
    }

    /// Classify a native-indexable normalized path without reading content.
    /// Ambiguous paths use the same conservative defaults as v1.
    #[must_use]
    pub fn for_normalized_path(path: &str) -> Option<Self> {
        Self::detect(path, None).filter(|language| language.is_native_indexable())
    }

    /// Classify a native-indexable normalized path with bounded content clues.
    #[must_use]
    pub fn for_normalized_path_with_source(path: &str, source: &str) -> Option<Self> {
        Self::detect(path, Some(source)).filter(|language| language.is_native_indexable())
    }

    /// Classify a normalized path/content pair through the immutable v1.1.33
    /// language and extension boundary. Additive v2 paths such as `.pyi` and
    /// `.toml` remain excluded even when their language has a native extractor.
    #[must_use]
    pub fn for_v1_normalized_path_with_source(path: &str, source: &str) -> Option<Self> {
        if !Self::is_v1_candidate_path(path) {
            return None;
        }
        detect_v1_source_language(path, Some(source))
            .filter(|language| language.is_native_indexable() && language.is_v1_language())
    }

    /// Whether a path could become supported after bounded content inspection.
    #[must_use]
    pub fn is_known_candidate_path(path: &str) -> bool {
        is_candidate_path_with(path, |_| true)
    }

    /// Whether bounded content inspection could select a currently executable
    /// native extractor for this path.
    #[must_use]
    pub fn is_native_candidate_path(path: &str) -> bool {
        is_candidate_path_with(path, Self::is_native_indexable)
    }

    /// Whether bounded content inspection could select one of the exact
    /// v1.1.33 source modes. Paths admitted only by v2 additions are excluded.
    #[must_use]
    pub fn is_v1_candidate_path(path: &str) -> bool {
        is_v1_candidate_path(path)
    }

    /// Whether a path could resolve to a native language admitted by a
    /// caller-owned allow-list. Ambiguous extensions remain candidates until
    /// bounded content classification chooses the exact language.
    #[must_use]
    pub fn is_native_candidate_path_where(
        path: &str,
        enabled: impl Fn(SourceLanguage) -> bool,
    ) -> bool {
        is_candidate_path_with(path, |language| {
            language.is_native_indexable() && enabled(language)
        })
    }
}

fn is_candidate_path_with(path: &str, enabled: impl Fn(SourceLanguage) -> bool) -> bool {
    if let Some(language) = go_manifest::language(path) {
        return enabled(language);
    }
    if is_play_routes_file(path) {
        return enabled(SourceLanguage::Yaml);
    }
    if let Some(language) = v1_path_specific_language(path) {
        return enabled(language);
    }
    if game_path_candidates(path).into_iter().any(&enabled) {
        return true;
    }
    let extension = path_extension(path);
    if matches!(extension, Some(value) if value.eq_ignore_ascii_case(".md")) {
        return enabled(SourceLanguage::Liquid) && is_jekyll_liquid_path(path);
    }
    if matches!(extension, Some(value) if value.eq_ignore_ascii_case(".h"))
        && [
            SourceLanguage::C,
            SourceLanguage::Cpp,
            SourceLanguage::ObjectiveC,
        ]
        .into_iter()
        .any(&enabled)
    {
        return true;
    }
    if matches!(extension, Some(value) if value.eq_ignore_ascii_case(".cls"))
        && [SourceLanguage::Apex, SourceLanguage::Vb6]
            .into_iter()
            .any(&enabled)
    {
        return true;
    }
    if matches!(extension, Some(value) if value.eq_ignore_ascii_case(".html")) {
        return enabled(SourceLanguage::Html)
            || enabled(SourceLanguage::Liquid) && is_jekyll_liquid_path(path);
    }
    SourceLanguage::ALL.into_iter().any(|language| {
        enabled(language)
            && extension.is_some_and(|value| language_owns_extension(language, value, true))
            && candidate_path_admits_language(path, language)
    })
}

fn is_v1_candidate_path(path: &str) -> bool {
    if is_play_routes_file(path) || v1_path_specific_language(path).is_some() {
        return true;
    }
    let extension = path_extension(path);
    if matches!(extension, Some(value) if value.eq_ignore_ascii_case(".md")) {
        return is_jekyll_liquid_path(path);
    }
    SourceLanguage::ALL.into_iter().any(|language| {
        language.is_v1_language()
            && language.is_native_indexable()
            && extension.is_some_and(|value| language_owns_extension(language, value, false))
            && candidate_path_admits_language(path, language)
    })
}

fn candidate_path_admits_language(path: &str, language: SourceLanguage) -> bool {
    match language {
        SourceLanguage::Aura => is_salesforce_aura_file(path, None),
        SourceLanguage::Visualforce => is_salesforce_visualforce_file(path, None),
        _ => true,
    }
}

fn is_jekyll_liquid_path(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase();
    normalized
        .split('/')
        .any(|component| matches!(component, "_layouts" | "_includes" | "_posts" | "_drafts"))
}

const CONTENT_CLASSIFIER_BYTES: usize = 8_192;
const FRONT_MATTER_CLASSIFIER_BYTES: usize = 4_096;
const V1_LANGUAGE_REGISTRY_DIGEST_DOMAIN: &[u8] = b"cartograph-v2-v1.1.33-language-registry-v1";
const V2_LANGUAGE_ADDITIONS_DIGEST_DOMAIN: &[u8] = b"cartograph-v2-language-extension-additions-v1";

/// BLAKE3 digest of the canonical v1.1.33 stable-id/extension manifest.
/// Release verification freezes this independently from the registry source.
#[must_use]
pub fn v1_language_registry_digest() -> [u8; 32] {
    language_registry_digest(
        V1_LANGUAGE_REGISTRY_DIGEST_DOMAIN,
        SourceLanguage::is_v1_language,
        SourceLanguage::v1_extensions,
    )
}

/// BLAKE3 digest of additive v2 extension aliases, kept separate from parity.
#[must_use]
pub fn v2_language_additions_digest() -> [u8; 32] {
    language_registry_digest(
        V2_LANGUAGE_ADDITIONS_DIGEST_DOMAIN,
        |_| true,
        SourceLanguage::additional_extensions,
    )
}

fn language_registry_digest(
    domain: &[u8],
    include: impl Fn(SourceLanguage) -> bool,
    extensions: impl Fn(SourceLanguage) -> &'static [&'static str],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&[0]);
    for language in SourceLanguage::ALL {
        if !include(language) {
            continue;
        }
        hasher.update(language.as_str().as_bytes());
        hasher.update(&[0]);
        for extension in extensions(language) {
            hasher.update(extension.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(&[u8::MAX]);
    }
    *hasher.finalize().as_bytes()
}

fn detect_source_language(path: &str, source: Option<&str>) -> Option<SourceLanguage> {
    if let Some(language) = fixed_path_language(path) {
        return Some(language);
    }
    if let Some(language) = game_path_language(path, source) {
        return Some(language);
    }

    let extension = path_extension(path)?;
    if is_liquid_with_front_matter(extension, source) {
        return Some(SourceLanguage::Liquid);
    }
    if let Some(language) = detect_colliding_game_extension(path, extension, source) {
        return Some(language);
    }
    if content_gated_extension(extension) {
        return detect_content_gated_language(extension, source);
    }
    validated_extension_language(path, extension, source)
}

fn fixed_path_language(path: &str) -> Option<SourceLanguage> {
    is_play_routes_file(path)
        .then_some(SourceLanguage::Yaml)
        .or_else(|| v1_path_specific_language(path))
        .or_else(|| go_manifest::language(path))
}

fn is_liquid_with_front_matter(extension: &str, source: Option<&str>) -> bool {
    source.is_some_and(has_yaml_front_matter)
        && (extension.eq_ignore_ascii_case(".html") || extension.eq_ignore_ascii_case(".md"))
}

fn content_gated_extension(extension: &str) -> bool {
    [".d", ".inf", ".t"]
        .into_iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn detect_content_gated_language(extension: &str, source: Option<&str>) -> Option<SourceLanguage> {
    let source = source?;
    if extension.eq_ignore_ascii_case(".d") && looks_like_daedalus(source) {
        return Some(SourceLanguage::Daedalus);
    }
    if extension.eq_ignore_ascii_case(".inf") && looks_like_inform6(source) {
        return Some(SourceLanguage::Inform6);
    }
    if extension.eq_ignore_ascii_case(".t") && looks_like_tads(source) {
        return Some(SourceLanguage::Tads);
    }
    None
}

fn validated_extension_language(
    path: &str,
    extension: &str,
    source: Option<&str>,
) -> Option<SourceLanguage> {
    let language = extension_owner(path, true)?;
    if language == SourceLanguage::Aura && !is_salesforce_aura_file(path, source) {
        return None;
    }
    if language == SourceLanguage::Visualforce && !is_salesforce_visualforce_file(path, source) {
        return None;
    }
    if language == SourceLanguage::Apex
        && extension.eq_ignore_ascii_case(".cls")
        && source.is_some_and(looks_like_vb6)
    {
        return Some(SourceLanguage::Vb6);
    }
    if language == SourceLanguage::C
        && extension.eq_ignore_ascii_case(".h")
        && let Some(source) = source
    {
        return Some(detect_header_language(source));
    }
    Some(language)
}

fn detect_v1_source_language(path: &str, source: Option<&str>) -> Option<SourceLanguage> {
    if is_play_routes_file(path) {
        return Some(SourceLanguage::Yaml);
    }
    if let Some(language) = v1_path_specific_language(path) {
        return Some(language);
    }
    let extension = path_extension(path);
    if source.is_some_and(has_yaml_front_matter)
        && matches!(extension, Some(value) if value.eq_ignore_ascii_case(".html") || value.eq_ignore_ascii_case(".md"))
    {
        return Some(SourceLanguage::Liquid);
    }
    let mut language = extension_owner(path, false)?;
    if language == SourceLanguage::Aura && !is_salesforce_aura_file(path, source) {
        return None;
    }
    if language == SourceLanguage::Visualforce && !is_salesforce_visualforce_file(path, source) {
        return None;
    }
    if language == SourceLanguage::Apex
        && matches!(extension, Some(value) if value.eq_ignore_ascii_case(".cls"))
        && source.is_some_and(looks_like_vb6)
    {
        language = SourceLanguage::Vb6;
    }
    if language == SourceLanguage::C
        && matches!(extension, Some(value) if value.eq_ignore_ascii_case(".h"))
        && let Some(source) = source
    {
        language = detect_header_language(source);
    }
    Some(language)
}

fn extension_owner(path: &str, include_additions: bool) -> Option<SourceLanguage> {
    let extension = path_extension(path)?;
    SourceLanguage::ALL
        .into_iter()
        .find(|language| language_owns_extension(*language, extension, include_additions))
}

fn language_owns_extension(
    language: SourceLanguage,
    extension: &str,
    include_additions: bool,
) -> bool {
    language
        .v1_extensions()
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        || include_additions
            && language
                .additional_extensions()
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn path_extension(path: &str) -> Option<&str> {
    let filename = path.rsplit('/').next()?;
    let offset = filename.rfind('.')?;
    Some(&filename[offset..])
}

fn game_path_candidates(path: &str) -> impl Iterator<Item = SourceLanguage> {
    [
        is_decorate_path(path).then_some(SourceLanguage::DoomDecorate),
        is_zscript_path(path).then_some(SourceLanguage::Zscript),
        is_choice_script_path(path).then_some(SourceLanguage::ChoiceScript),
        is_paradox_script_path(path).then_some(SourceLanguage::ParadoxScript),
    ]
    .into_iter()
    .flatten()
}

fn game_path_language(path: &str, source: Option<&str>) -> Option<SourceLanguage> {
    if is_decorate_path(path) {
        return Some(SourceLanguage::DoomDecorate);
    }
    if is_zscript_path(path) {
        return Some(SourceLanguage::Zscript);
    }
    if is_choice_script_path(path) && source.is_some_and(looks_like_choice_script) {
        return Some(SourceLanguage::ChoiceScript);
    }
    if is_paradox_script_path(path) && source.is_some_and(looks_like_paradox_script) {
        return Some(SourceLanguage::ParadoxScript);
    }
    None
}

fn detect_colliding_game_extension(
    path: &str,
    extension: &str,
    source: Option<&str>,
) -> Option<SourceLanguage> {
    let source = source?;
    if extension.eq_ignore_ascii_case(".as") && looks_like_angelscript(source) {
        return Some(SourceLanguage::AngelScript);
    }
    if extension.eq_ignore_ascii_case(".c") {
        if looks_like_enforce_script(path, source) {
            return Some(SourceLanguage::EnforceScript);
        }
        if looks_like_lpc(path, source) {
            return Some(SourceLanguage::Lpc);
        }
    }
    if extension.eq_ignore_ascii_case(".cs") && looks_like_torque_script(source) {
        return Some(SourceLanguage::TorqueScript);
    }
    if extension.eq_ignore_ascii_case(".qc") && looks_like_valve_qc(source) {
        return Some(SourceLanguage::ValveQc);
    }
    None
}

fn is_decorate_path(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|filename| {
        filename.eq_ignore_ascii_case("decorate") || filename.eq_ignore_ascii_case("decorate.txt")
    })
}

fn is_zscript_path(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|filename| filename.eq_ignore_ascii_case("zscript.txt"))
}

fn is_choice_script_path(path: &str) -> bool {
    has_extension(path, "txt")
        && path
            .split('/')
            .any(|component| component.eq_ignore_ascii_case("scenes"))
}

fn is_paradox_script_path(path: &str) -> bool {
    if !has_extension(path, "txt") {
        return false;
    }
    path.split('/').any(|component| {
        matches_ignore_ascii_case(
            component,
            &[
                "common",
                "decisions",
                "events",
                "history",
                "missions",
                "on_actions",
                "scripted_effects",
                "scripted_triggers",
            ],
        )
    })
}

fn looks_like_choice_script(source: &str) -> bool {
    bounded_prefix(source, CONTENT_CLASSIFIER_BYTES)
        .lines()
        .map(str::trim_start)
        .any(|line| {
            [
                "*choice",
                "*create ",
                "*goto ",
                "*gosub ",
                "*label ",
                "*scene_list",
            ]
            .into_iter()
            .any(|marker| line.to_ascii_lowercase().starts_with(marker))
        })
}

fn looks_like_paradox_script(source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    let structured = prefix.contains("= {") || prefix.contains("={");
    structured
        && [
            "namespace =",
            "country_event =",
            "character_event =",
            "planet_event =",
            "scripted_trigger =",
            "on_action =",
            "potential =",
            "immediate =",
        ]
        .into_iter()
        .any(|marker| prefix.contains(marker))
}

fn looks_like_angelscript(source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    ["funcdef ", "mixin class ", "shared class ", "interface "]
        .into_iter()
        .any(|marker| prefix.contains(marker))
        || prefix.contains("@+")
        || prefix.contains("@ ")
}

fn looks_like_enforce_script(path: &str, source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    let language_marker = [
        "modded class ",
        "proto native ",
        "autoptr ",
        "ref array<",
        "override void eon",
    ]
    .into_iter()
    .any(|marker| prefix.contains(marker));
    let normalized = path.to_ascii_lowercase();
    language_marker
        || normalized.contains("/scripts/3_game/") && prefix.contains("class ")
        || normalized.contains("/scripts/4_world/") && prefix.contains("class ")
        || normalized.contains("/scripts/5_mission/") && prefix.contains("class ")
}

fn looks_like_lpc(path: &str, source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    let language_marker = [
        "inherit \"",
        "inherit(\"",
        "mapping ",
        "mixed ",
        "nomask ",
        "object *",
    ]
    .into_iter()
    .any(|marker| prefix.contains(marker));
    language_marker
        || path
            .split('/')
            .any(|component| component.eq_ignore_ascii_case("mudlib"))
            && (prefix.contains("void create(") || prefix.contains("reset("))
}

fn looks_like_torque_script(source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    ["datablock ", "function ", "new simobject(", "exec(\""]
        .into_iter()
        .any(|marker| prefix.contains(marker))
        && (prefix.contains('%') || prefix.contains('$') || prefix.contains("::"))
}

fn looks_like_valve_qc(source: &str) -> bool {
    bounded_prefix(source, CONTENT_CLASSIFIER_BYTES)
        .lines()
        .map(str::trim_start)
        .any(|line| {
            [
                "$body",
                "$bodygroup",
                "$cdmaterials",
                "$modelname",
                "$sequence",
            ]
            .into_iter()
            .any(|marker| line.to_ascii_lowercase().starts_with(marker))
        })
}

fn looks_like_daedalus(source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    [
        "instance ",
        "prototype ",
        "func ",
        "var c_npc",
        "var c_info",
    ]
    .into_iter()
    .filter(|marker| prefix.contains(marker))
    .take(2)
    .count()
        >= 2
}

fn looks_like_inform6(source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    [
        "constant story",
        "include \"parser\"",
        "include \"verblib\"",
        "[ initialise",
        "object ",
    ]
    .into_iter()
    .filter(|marker| prefix.contains(marker))
    .take(2)
    .count()
        >= 2
}

fn looks_like_tads(source: &str) -> bool {
    let prefix = bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase();
    [
        "#include <adv3.h>",
        "#include <tads.h>",
        "gamemaindef",
        "room 'external name'",
        "modify ",
    ]
    .into_iter()
    .any(|marker| prefix.contains(marker))
}

fn v1_path_specific_language(path: &str) -> Option<SourceLanguage> {
    let normalized = path.to_ascii_lowercase();
    let localization_marker = normalized
        .contains("/localization/")
        .then_some("/localization/")
        .or_else(|| {
            normalized
                .starts_with("localization/")
                .then_some("localization/")
        });
    let localization_xml = localization_marker.is_some_and(|marker| {
        has_extension(&normalized, "xml") && component_after(&normalized, marker).is_some()
    });
    if localization_xml || is_bg3_resource_xml_path(&normalized) {
        return Some(SourceLanguage::Bg3Resource);
    }
    if (normalized.contains("/stats/generated/") || normalized.starts_with("stats/generated/"))
        && has_extension(&normalized, "txt")
    {
        return Some(SourceLanguage::Bg3Stats);
    }
    if (normalized.contains("/story/rawfiles/goals/")
        || normalized.starts_with("story/rawfiles/goals/"))
        && has_extension(&normalized, "txt")
    {
        return Some(SourceLanguage::Osiris);
    }
    None
}

fn component_after<'a>(path: &'a str, marker: &str) -> Option<&'a str> {
    let remainder = path.split_once(marker)?.1;
    let (component, tail) = remainder.split_once('/')?;
    (!component.is_empty() && !tail.is_empty()).then_some(component)
}

fn is_bg3_resource_xml_path(path: &str) -> bool {
    if !has_extension(path, "xml") {
        return false;
    }
    ["/public/", "public/", "/mods/", "mods/"]
        .into_iter()
        .any(|marker| {
            let matches_position = if marker.starts_with('/') {
                path.contains(marker)
            } else {
                path.starts_with(marker)
            };
            matches_position && component_after(path, marker).is_some()
        })
}

fn has_extension(path: &str, expected: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case(expected))
}

fn is_play_routes_file(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let Some((directory, filename)) = normalized.rsplit_once('/') else {
        return false;
    };
    let Some(conf) = directory.rsplit('/').next() else {
        return false;
    };
    conf == "conf" && (filename == "routes" || filename.ends_with(".routes"))
}

fn is_salesforce_aura_file(path: &str, source: Option<&str>) -> bool {
    let normalized = path.to_ascii_lowercase();
    normalized.contains("/aura/")
        || normalized.starts_with("aura/")
        || source.is_some_and(|value| contains_markup_prefix(value, "aura:"))
}

fn is_salesforce_visualforce_file(path: &str, source: Option<&str>) -> bool {
    let normalized = path.to_ascii_lowercase();
    normalized.contains("/pages/")
        || normalized.starts_with("pages/")
        || normalized.contains("/components/")
        || normalized.starts_with("components/")
        || normalized.contains("/visualforce/")
        || normalized.starts_with("visualforce/")
        || source.is_some_and(|value| contains_markup_prefix(value, "apex:"))
}

fn looks_like_vb6(source: &str) -> bool {
    let mut has_option_explicit = false;
    let mut has_routine = false;
    for raw_line in bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).lines() {
        let line = raw_line
            .trim_start()
            .trim_end_matches('\r')
            .to_ascii_lowercase();
        if line.starts_with("version ") && line.contains(" class")
            || line.starts_with("attribute vb_name") && line.contains('=')
            || line.starts_with("begin vb.")
        {
            return true;
        }
        has_option_explicit |= line.starts_with("option explicit");
        let body = ["public ", "private ", "friend "]
            .into_iter()
            .find_map(|visibility| line.strip_prefix(visibility))
            .unwrap_or(&line);
        has_routine |= body.starts_with("sub ")
            || body.starts_with("function ")
            || body.starts_with("property ");
    }
    has_option_explicit && has_routine
}

fn detect_header_language(source: &str) -> SourceLanguage {
    let sample = ascii_lower_prefix(source);
    if ["@interface", "@implementation", "@protocol", "@synthesize"]
        .into_iter()
        .any(|marker| contains_marker_with_word_end(&sample, marker))
    {
        return SourceLanguage::ObjectiveC;
    }
    let normalized = collapse_ascii_whitespace(&sample);
    let looks_cpp = contains_ascii_word(&normalized, "namespace")
        || contains_cpp_class(&normalized)
        || word_followed_by(&normalized, "template", '<')
        || ["public", "private", "protected"]
            .into_iter()
            .any(|visibility| word_followed_by(&normalized, visibility, ':'))
        || contains_ascii_word(&normalized, "virtual")
        || contains_word_sequence(&normalized, "using", "namespace")
        || contains_using_alias(&normalized);
    if looks_cpp {
        SourceLanguage::Cpp
    } else {
        SourceLanguage::C
    }
}

fn has_yaml_front_matter(source: &str) -> bool {
    let sample = bounded_prefix(source, FRONT_MATTER_CLASSIFIER_BYTES);
    let Some((opening, remainder)) = sample.split_once('\n') else {
        return false;
    };
    opening.trim_end_matches('\r') == "---"
        && remainder
            .lines()
            .any(|line| line.trim_end_matches(['\r', ' ', '\t']) == "---")
}

fn contains_markup_prefix(source: &str, namespace: &str) -> bool {
    let sample = ascii_lower_prefix(source);
    let bytes = sample.as_bytes();
    let namespace = namespace.as_bytes();
    let mut offset = 0;
    while let Some(relative) = bytes[offset..].iter().position(|byte| *byte == b'<') {
        let mut cursor = offset + relative + 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor..cursor.saturating_add(namespace.len())) == Some(namespace) {
            return true;
        }
        offset = cursor;
        if offset >= bytes.len() {
            break;
        }
    }
    false
}

fn collapse_ascii_whitespace(source: &str) -> String {
    let mut normalized = String::with_capacity(source.len());
    let mut in_whitespace = false;
    for character in source.chars() {
        if character.is_ascii_whitespace() {
            if !in_whitespace {
                normalized.push(' ');
                in_whitespace = true;
            }
        } else {
            normalized.push(character);
            in_whitespace = false;
        }
    }
    normalized
}

fn contains_ascii_word(source: &str, word: &str) -> bool {
    source.match_indices(word).any(|(offset, _)| {
        let before = source[..offset].bytes().next_back();
        let after = source[offset + word.len()..].bytes().next();
        before.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
            && after.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
    })
}

fn contains_marker_with_word_end(source: &str, marker: &str) -> bool {
    source.match_indices(marker).any(|(offset, _)| {
        source[offset + marker.len()..]
            .bytes()
            .next()
            .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
    })
}

fn contains_cpp_class(source: &str) -> bool {
    source.match_indices("class").any(|(offset, _)| {
        let before = source[..offset].bytes().next_back();
        if before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
            return false;
        }
        let Some(mut tail) = tail_after_required_whitespace(source, offset, "class") else {
            return false;
        };
        let identifier_bytes = tail
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        if identifier_bytes == 0 {
            return false;
        }
        tail = tail[identifier_bytes..].trim_start();
        tail.starts_with([':', '{'])
    })
}

fn word_followed_by(source: &str, word: &str, punctuation: char) -> bool {
    source.match_indices(word).any(|(offset, _)| {
        let before = source[..offset].bytes().next_back();
        let tail = source[offset + word.len()..].trim_start();
        before.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
            && tail.starts_with(punctuation)
    })
}

fn contains_word_sequence(source: &str, first: &str, second: &str) -> bool {
    source.match_indices(first).any(|(offset, _)| {
        let before = source[..offset].bytes().next_back();
        if before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
            return false;
        }
        let Some(tail) = tail_after_required_whitespace(source, offset, first) else {
            return false;
        };
        tail.strip_prefix(second).is_some_and(|remainder| {
            remainder
                .bytes()
                .next()
                .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
        })
    })
}

fn contains_using_alias(source: &str) -> bool {
    source.match_indices("using").any(|(offset, _)| {
        let before = source[..offset].bytes().next_back();
        if before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
            return false;
        }
        let Some(tail) = tail_after_required_whitespace(source, offset, "using") else {
            return false;
        };
        let identifier_bytes = tail
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        identifier_bytes > 0 && tail[identifier_bytes..].trim_start().starts_with('=')
    })
}

fn tail_after_required_whitespace<'a>(
    source: &'a str,
    offset: usize,
    word: &str,
) -> Option<&'a str> {
    let tail = &source[offset + word.len()..];
    tail.starts_with(' ').then(|| tail.trim_start())
}

fn ascii_lower_prefix(source: &str) -> String {
    bounded_prefix(source, CONTENT_CLASSIFIER_BYTES).to_ascii_lowercase()
}

fn bounded_prefix(source: &str, maximum_bytes: usize) -> &str {
    &source[..source.floor_char_boundary(maximum_bytes)]
}

/// A canonical project-relative source path with forward-slash separators.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct NormalizedPath(String);

impl NormalizedPath {
    /// Normalize a relative path while rejecting absolute and parent-escaping forms.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidNormalizedPath`] when `raw` is empty, absolute,
    /// parent-escaping, contains a NUL byte, or exceeds the path bound.
    pub fn parse(raw: &str) -> Result<Self, InvalidNormalizedPath> {
        if raw.is_empty()
            || raw.len() > MAX_NORMALIZED_PATH_BYTES
            || raw.contains('\0')
            || raw.starts_with(['/', '\\'])
            || has_windows_drive_prefix(raw)
        {
            return Err(InvalidNormalizedPath);
        }

        let mut canonical = String::new();
        canonical
            .try_reserve_exact(raw.len())
            .map_err(|_| InvalidNormalizedPath)?;
        for component in raw.split(['/', '\\']) {
            match component {
                "" | "." => {}
                ".." => return Err(InvalidNormalizedPath),
                value => {
                    if !canonical.is_empty() {
                        canonical.push('/');
                    }
                    canonical.push_str(value);
                }
            }
        }
        if canonical.is_empty() || canonical.len() > MAX_NORMALIZED_PATH_BYTES {
            return Err(InvalidNormalizedPath);
        }
        Ok(Self(canonical))
    }

    /// Canonical project-relative path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the validated path without cloning its canonical allocation.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for NormalizedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NormalizedPath {
    type Err = InvalidNormalizedPath;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl TryFrom<String> for NormalizedPath {
    type Error = InvalidNormalizedPath;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

/// A path was not a bounded canonical project-relative source path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidNormalizedPath;

impl fmt::Display for InvalidNormalizedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("source path must be a bounded project-relative path")
    }
}

impl std::error::Error for InvalidNormalizedPath {}

/// Exact byte and human-facing line/column location for one source fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "SourcePositionValue")]
pub struct SourcePosition {
    byte: u64,
    line: u32,
    column: u32,
}

#[derive(Deserialize)]
struct SourcePositionValue {
    byte: u64,
    line: u32,
    column: u32,
}

impl SourcePosition {
    /// Validate a byte location with a one-based line and zero-based byte column.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSourceSpan`] when `line` is zero.
    pub const fn new(byte: u64, line: u32, column: u32) -> Result<Self, InvalidSourceSpan> {
        if line == 0 {
            return Err(InvalidSourceSpan);
        }
        Ok(Self { byte, line, column })
    }
}

impl TryFrom<SourcePositionValue> for SourcePosition {
    type Error = InvalidSourceSpan;

    fn try_from(value: SourcePositionValue) -> Result<Self, Self::Error> {
        Self::new(value.byte, value.line, value.column)
    }
}

/// Exact half-open source range, or an explicit zero-width synthetic point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "SourceSpanValue")]
pub struct SourceSpan {
    start: SourcePosition,
    end: SourcePosition,
}

#[derive(Deserialize)]
struct SourceSpanValue {
    start: SourcePosition,
    end: SourcePosition,
}

impl SourceSpan {
    /// Validate ordered start/end positions.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSourceSpan`] when the range is empty, reversed, or its
    /// line and column ordering contradicts its byte ordering.
    pub const fn new(
        start: SourcePosition,
        end: SourcePosition,
    ) -> Result<Self, InvalidSourceSpan> {
        let invalid = start.byte >= end.byte
            || end.line < start.line
            || (start.line == end.line && end.column < start.column);
        if invalid {
            return Err(InvalidSourceSpan);
        }
        Ok(Self { start, end })
    }

    /// Create a zero-width point for a fact derived from a file convention
    /// rather than source text (for example an empty framework route file).
    /// Ordinary parsed source facts must continue to use [`Self::new`].
    #[must_use]
    pub const fn synthetic(point: SourcePosition) -> Self {
        Self {
            start: point,
            end: point,
        }
    }

    /// Inclusive first source byte.
    #[must_use]
    pub const fn start_byte(self) -> u64 {
        self.start.byte
    }

    /// Exclusive last source byte.
    #[must_use]
    pub const fn end_byte(self) -> u64 {
        self.end.byte
    }

    /// One-based inclusive first line.
    #[must_use]
    pub const fn start_line(self) -> u32 {
        self.start.line
    }

    /// One-based line containing the exclusive end position.
    #[must_use]
    pub const fn end_line(self) -> u32 {
        self.end.line
    }

    /// Zero-based first byte column.
    #[must_use]
    pub const fn start_column(self) -> u32 {
        self.start.column
    }

    /// Zero-based exclusive last byte column.
    #[must_use]
    pub const fn end_column(self) -> u32 {
        self.end.column
    }
}

impl TryFrom<SourceSpanValue> for SourceSpan {
    type Error = InvalidSourceSpan;

    fn try_from(value: SourceSpanValue) -> Result<Self, Self::Error> {
        if value.start == value.end {
            Ok(Self::synthetic(value.start))
        } else {
            Self::new(value.start, value.end)
        }
    }
}

/// A parsed source range was empty, reversed, or used invalid line coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidSourceSpan;

impl fmt::Display for InvalidSourceSpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("source span is invalid")
    }
}

impl std::error::Error for InvalidSourceSpan {}

/// Storage-independent code-symbol category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Synthetic source-file graph node.
    File,
    /// Source module or namespace container.
    Module,
    /// Class declaration.
    Class,
    /// Struct or record declaration.
    Struct,
    /// C-family union declaration.
    Union,
    /// Interface declaration.
    Interface,
    /// Trait declaration.
    Trait,
    /// Protocol declaration.
    Protocol,
    /// Free or nested function.
    Function,
    /// Class/interface method.
    Method,
    /// Property declaration.
    Property,
    /// Field declaration.
    Field,
    /// Mutable or non-constant binding.
    Variable,
    /// Constant binding.
    Constant,
    /// Enum declaration.
    Enum,
    /// Enum member.
    EnumMember,
    /// Type alias.
    TypeAlias,
    /// Namespace declaration.
    Namespace,
    /// Function or method parameter.
    Parameter,
    /// Import declaration.
    Import,
    /// Explicit export declaration.
    Export,
    /// Framework route or command.
    Route,
    /// UI component.
    Component,
    /// Database table declaration.
    Table,
    /// Framework or deployment resource.
    Resource,
}

impl_stable_as_str!(
    SymbolKind,
    Self::File => "file",
    Self::Module => "module",
    Self::Class => "class",
    Self::Struct => "struct",
    Self::Union => "union",
    Self::Interface => "interface",
    Self::Trait => "trait",
    Self::Protocol => "protocol",
    Self::Function => "function",
    Self::Method => "method",
    Self::Property => "property",
    Self::Field => "field",
    Self::Variable => "variable",
    Self::Constant => "constant",
    Self::Enum => "enum",
    Self::EnumMember => "enum_member",
    Self::TypeAlias => "type_alias",
    Self::Namespace => "namespace",
    Self::Parameter => "parameter",
    Self::Import => "import",
    Self::Export => "export",
    Self::Route => "route",
    Self::Component => "component",
    Self::Table => "table",
    Self::Resource => "resource",
);

const STABLE_SYMBOL_KINDS: [SymbolKind; 25] = [
    SymbolKind::File,
    SymbolKind::Module,
    SymbolKind::Class,
    SymbolKind::Struct,
    SymbolKind::Union,
    SymbolKind::Interface,
    SymbolKind::Trait,
    SymbolKind::Protocol,
    SymbolKind::Function,
    SymbolKind::Method,
    SymbolKind::Property,
    SymbolKind::Field,
    SymbolKind::Variable,
    SymbolKind::Constant,
    SymbolKind::Enum,
    SymbolKind::EnumMember,
    SymbolKind::TypeAlias,
    SymbolKind::Namespace,
    SymbolKind::Parameter,
    SymbolKind::Import,
    SymbolKind::Export,
    SymbolKind::Route,
    SymbolKind::Component,
    SymbolKind::Table,
    SymbolKind::Resource,
];

impl SymbolKind {
    /// Every symbol kind, in stable storage order.
    pub const ALL: [Self; STABLE_SYMBOL_KINDS.len()] = STABLE_SYMBOL_KINDS;

    /// Parse the stable storage/protocol spelling of a symbol kind.
    #[must_use]
    pub fn from_stable_str(value: &str) -> Option<Self> {
        STABLE_SYMBOL_KINDS
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

/// Implementation-presence and test-ownership flags shared by extraction and storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolImplementationFlags {
    /// Whether this declaration has no implementation body, such as an overload signature.
    pub declaration_only: bool,
    /// Whether this declaration is owned by an inline test scope or test attribute.
    pub test_symbol: bool,
}

/// Module export flags shared by extraction and storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolExportFlags {
    /// Whether the containing module explicitly exports the declaration.
    pub exported: bool,
    /// Whether the declaration is the containing module's default export.
    pub default_export: bool,
}

impl SymbolExportFlags {
    /// Construct explicit named/default export state.
    #[must_use]
    pub const fn new(exported: bool, default_export: bool) -> Self {
        Self {
            exported,
            default_export,
        }
    }

    /// Construct named-export state with no default export.
    #[must_use]
    pub const fn named(exported: bool) -> Self {
        Self::new(exported, false)
    }
}

/// Execution modifiers shared by extraction and storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolExecutionFlags {
    /// Whether the declaration carries its language's async modifier.
    pub async_symbol: bool,
    /// Whether the declaration is a static class or type member.
    pub static_member: bool,
}

/// Language-level declaration visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Publicly accessible declaration.
    Public,
    /// Class-private declaration.
    Private,
    /// Subclass-visible declaration.
    Protected,
    /// Package or assembly internal declaration.
    Internal,
}

impl Visibility {
    /// Stable storage and protocol representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Protected => "protected",
            Self::Internal => "internal",
        }
    }

    /// Parse the stable storage/protocol representation.
    #[must_use]
    pub fn from_stable_str(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "private" => Some(Self::Private),
            "protected" => Some(Self::Protected),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

/// Unresolved structural relationship emitted before project-wide resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ReferenceKind {
    /// The owner invokes the target.
    Calls,
    /// The file imports the target module.
    Imports,
    /// The owner contains a general symbol reference.
    References,
    /// The owner implements the target contract.
    Implements,
    /// The owner extends or inherits from the target.
    Extends,
    /// The owner test exercises the target.
    Tests,
    /// The owner exports the target.
    Exports,
    /// The owner has or consumes the target type.
    TypeOf,
    /// The owner returns the target type.
    Returns,
    /// The owner constructs the target.
    Instantiates,
    /// The owner overrides the target.
    Overrides,
    /// The owner is decorated by the target.
    Decorates,
    /// The owner accesses a target field.
    FieldAccess,
    /// The owner defines and subsequently uses the target binding.
    DefUse,
    /// The syntax names a base type whose class/interface role must be resolved project-wide.
    Inherits,
}

impl_stable_as_str!(
    ReferenceKind,
    Self::Calls => "calls",
    Self::Imports => "imports",
    Self::References => "references",
    Self::Implements => "implements",
    Self::Extends => "extends",
    Self::Tests => "tests",
    Self::Exports => "exports",
    Self::TypeOf => "type_of",
    Self::Returns => "returns",
    Self::Instantiates => "instantiates",
    Self::Overrides => "overrides",
    Self::Decorates => "decorates",
    Self::FieldAccess => "field_access",
    Self::DefUse => "def_use",
    Self::Inherits => "inherits",
);

fn has_windows_drive_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::collections::BTreeSet;

    use super::{
        NormalizedPath, ReferenceKind, STABLE_SYMBOL_KINDS, SourceLanguage, SourcePosition,
        SourceSpan, SymbolKind, callable_signature_is_literal_free,
        declaration_value_is_search_safe, is_candidate_path_with, symbol_signature_is_search_safe,
        token_has_provider_key_shape, v1_language_registry_digest, v2_language_additions_digest,
        value_token_is_sensitive,
    };

    const SPAN_START_BYTE: u64 = 7;
    const SPAN_END_BYTE: u64 = 19;
    const SPAN_START_LINE: u32 = 2;
    const SPAN_END_LINE: u32 = 3;
    const SPAN_START_COLUMN: u32 = 4;
    const SPAN_END_COLUMN: u32 = 1;

    #[test]
    fn paths_canonicalize_and_spans_reject_escaping_coordinates() {
        let canonical = NormalizedPath::parse(r"src\feature\.\service.ts");
        assert_matches!(canonical, Ok(path) if path.as_str() == "src/feature/service.ts");
        assert!(NormalizedPath::parse("../secret.ts").is_err());
        assert!(NormalizedPath::parse("/absolute.ts").is_err());
        assert!(NormalizedPath::parse("C:\\absolute.ts").is_err());

        let start = position(SPAN_START_BYTE, SPAN_START_LINE, SPAN_START_COLUMN);
        let end = position(SPAN_END_BYTE, SPAN_END_LINE, SPAN_END_COLUMN);
        assert!(SourceSpan::new(start, end).is_ok());
        assert_eq!(SourceSpan::synthetic(start).start_byte(), SPAN_START_BYTE);
        assert_eq!(SourceSpan::synthetic(start).end_byte(), SPAN_START_BYTE);
        assert!(SourceSpan::new(start, start).is_err());
        assert!(SourceSpan::new(end, start).is_err());
        assert!(SourcePosition::new(SPAN_START_BYTE, 0, SPAN_START_COLUMN).is_err());
        let reversed_line_end = position(SPAN_END_BYTE, SPAN_START_LINE, SPAN_END_COLUMN);
        let reversed_line_start = position(SPAN_START_BYTE, SPAN_END_LINE, SPAN_START_COLUMN);
        assert!(SourceSpan::new(reversed_line_start, reversed_line_end).is_err());
        let reversed_column_end = position(SPAN_END_BYTE, SPAN_START_LINE, SPAN_END_COLUMN);
        assert!(SourceSpan::new(start, reversed_column_end).is_err());
    }

    #[test]
    fn extraction_enums_have_stable_values() {
        assert_eq!(SourceLanguage::TypeScript.as_str(), "typescript");
        assert_eq!(SourceLanguage::Tsx.as_str(), "tsx");
        assert_eq!(SourceLanguage::JavaScript.as_str(), "javascript");
        assert_eq!(
            SourceLanguage::from_stable_str("typescript"),
            Some(SourceLanguage::TypeScript)
        );
        assert_eq!(
            SourceLanguage::from_stable_str("java"),
            Some(SourceLanguage::Java)
        );
        assert_eq!(
            SourceLanguage::for_normalized_path("src/lib.RS"),
            Some(SourceLanguage::Rust)
        );
        assert_eq!(
            SourceLanguage::for_normalized_path("src/lib.java"),
            Some(SourceLanguage::Java)
        );
        assert_eq!(SourceLanguage::Jsx.as_str(), "jsx");
        assert_eq!(SourceLanguage::Rust.as_str(), "rust");
        assert_eq!(SourceLanguage::Python.as_str(), "python");
        assert_eq!(SourceLanguage::Go.as_str(), "go");
        assert_eq!(SymbolKind::TypeAlias.as_str(), "type_alias");
        assert_eq!(SymbolKind::File.as_str(), "file");
        assert_eq!(SymbolKind::Union.as_str(), "union");
        assert_eq!(
            SymbolKind::from_stable_str("union"),
            Some(SymbolKind::Union)
        );
        assert_eq!(SymbolKind::EnumMember.as_str(), "enum_member");
        for kind in STABLE_SYMBOL_KINDS {
            assert_eq!(SymbolKind::from_stable_str(kind.as_str()), Some(kind));
        }
        assert_eq!(SymbolKind::from_stable_str("unknown"), None);
        assert_eq!(ReferenceKind::TypeOf.as_str(), "type_of");
        assert_eq!(ReferenceKind::FieldAccess.as_str(), "field_access");
    }

    #[test]
    fn language_registry_preserves_v1_contract_and_tracks_v2_additions_separately() {
        let stable_ids = SourceLanguage::ALL.map(SourceLanguage::as_str);
        assert!(stable_ids.is_sorted_by(|previous, next| previous < next));
        assert_eq!(stable_ids.into_iter().collect::<BTreeSet<_>>().len(), 132);
        assert_eq!(
            SourceLanguage::ALL
                .into_iter()
                .filter(|language| language.is_v1_language())
                .count(),
            73
        );
        assert_eq!(
            SourceLanguage::ALL
                .into_iter()
                .filter(|language| language.is_game_scripting())
                .count(),
            52
        );
        assert!(
            SourceLanguage::ALL
                .into_iter()
                .filter(|language| language.is_game_scripting())
                .all(|language| !language.is_v1_language())
        );

        let v1_extensions = SourceLanguage::ALL
            .into_iter()
            .filter(|language| language.is_v1_language())
            .flat_map(SourceLanguage::v1_extensions)
            .collect::<Vec<_>>();
        assert_eq!(v1_extensions.len(), 163);
        assert_eq!(
            v1_extensions.iter().copied().collect::<BTreeSet<_>>().len(),
            163
        );
        assert_eq!(SourceLanguage::Python.v1_extensions(), &[".py", ".pyw"]);
        assert_eq!(SourceLanguage::Python.additional_extensions(), &[".pyi"]);
        assert_eq!(SourceLanguage::Rhai.v1_extensions(), [] as [&str; 0]);
        assert_eq!(SourceLanguage::Rhai.additional_extensions(), &[".rhai"]);
        assert_eq!(SourceLanguage::Toml.v1_extensions(), [] as [&str; 0]);
        assert_eq!(SourceLanguage::Toml.additional_extensions(), &[".toml"]);
        assert!(SourceLanguage::is_v1_candidate_path("src/service.py"));
        assert!(!SourceLanguage::is_v1_candidate_path("src/service.pyi"));
        assert!(!SourceLanguage::is_v1_candidate_path("scripts/policy.rhai"));
        assert!(!SourceLanguage::is_v1_candidate_path("Cargo.toml"));
        assert_eq!(
            SourceLanguage::for_normalized_path("scripts/policy.rhai"),
            Some(SourceLanguage::Rhai)
        );
        assert_eq!(
            SourceLanguage::for_v1_normalized_path_with_source(
                "src/service.py",
                "def ready():\n    return True\n",
            ),
            Some(SourceLanguage::Python)
        );
        assert_eq!(
            SourceLanguage::for_v1_normalized_path_with_source(
                "src/service.pyi",
                "def ready() -> bool: ...\n",
            ),
            None
        );

        for language in SourceLanguage::ALL {
            assert_eq!(
                SourceLanguage::from_stable_str(language.as_str()),
                Some(language)
            );
            let path = representative_path(language);
            assert_eq!(
                SourceLanguage::detect(&path, Some(representative_source(language))),
                Some(language),
                "{} was not routed from {path}",
                language.as_str()
            );
        }
    }

    #[test]
    fn language_registry_matches_independently_frozen_manifests() {
        assert_eq!(
            blake3::Hash::from_bytes(v1_language_registry_digest())
                .to_hex()
                .as_str(),
            "350f46c98c68929181d8c3b930eccb5d1fae39eaaff90e5a4c900b32b96a864f"
        );
        assert_eq!(
            blake3::Hash::from_bytes(v2_language_additions_digest())
                .to_hex()
                .as_str(),
            "65bfecbd7dabef18d1e0d567e26d286cd949c2c40089bd7bef31269fc699e3c1"
        );
    }

    #[test]
    fn content_and_path_overrides_are_bounded_and_unambiguous() {
        assert_eq!(
            SourceLanguage::detect("include/widget.h", None),
            Some(SourceLanguage::C)
        );
        assert_eq!(
            SourceLanguage::detect(
                "include/widget.h",
                Some("@interface Widget : NSObject\n@end")
            ),
            Some(SourceLanguage::ObjectiveC)
        );
        assert_eq!(
            SourceLanguage::detect(
                "include/widget.h",
                Some("namespace example { class Widget {}; }")
            ),
            Some(SourceLanguage::Cpp)
        );
        for cpp in [
            "namespace\tExample {}",
            "class\nWidget\t{ };",
            "template\n< typename T > struct Box {};",
            "template<typename T> struct Box {};",
            "struct Widget { public\t: void run(); };",
            "virtual\tvoid run();",
            "using\nnamespace Example;",
            "using\tWidget = Example::Widget;",
        ] {
            assert_eq!(
                SourceLanguage::detect("include/widget.h", Some(cpp)),
                Some(SourceLanguage::Cpp),
                "C++ clue was missed: {cpp}"
            );
        }
        assert_eq!(
            SourceLanguage::detect("include/widget.h", Some("int class_count;")),
            Some(SourceLanguage::C)
        );
        for c in [
            "@interfaceName value;",
            "int misusing_namespaceThing;",
            "int classWidget = 0;",
            "int usingWidget = 0;",
        ] {
            assert_eq!(
                SourceLanguage::detect("include/widget.h", Some(c)),
                Some(SourceLanguage::C),
                "C text was over-classified: {c}"
            );
        }
    }

    #[test]
    fn templating_and_salesforce_overrides_are_unambiguous() {
        assert_eq!(
            SourceLanguage::detect("classes/Widget.cls", None),
            Some(SourceLanguage::Apex)
        );
        assert_eq!(
            SourceLanguage::detect(
                "legacy/Widget.cls",
                Some("VERSION 1.0 CLASS\r\nAttribute VB_Name = \"Widget\"")
            ),
            Some(SourceLanguage::Vb6)
        );
        assert_eq!(
            SourceLanguage::detect(
                "layouts/default.html",
                Some("---\ntitle: Home\n---\n<body/>")
            ),
            Some(SourceLanguage::Liquid)
        );
        assert_eq!(
            SourceLanguage::detect("layouts/default.html", Some("<body/>")),
            Some(SourceLanguage::Html)
        );
        assert_eq!(
            SourceLanguage::detect("posts/hello.md", Some("---\ntitle: Hello\n---\ntext")),
            Some(SourceLanguage::Liquid)
        );
        assert_eq!(SourceLanguage::detect("posts/hello.md", Some("text")), None);

        assert_eq!(SourceLanguage::detect("ui/Widget.cmp", None), None);
        assert_eq!(
            SourceLanguage::detect("ui/Widget.cmp", Some("<aura:component/>")),
            Some(SourceLanguage::Aura)
        );
        assert_eq!(
            SourceLanguage::detect("ui/Widget.cmp", Some("<  \n aura:component/>")),
            Some(SourceLanguage::Aura)
        );
        assert_eq!(
            SourceLanguage::detect("force-app/main/default/aura/Widget/Widget.cmp", None),
            Some(SourceLanguage::Aura)
        );
        assert_eq!(
            SourceLanguage::detect("aura/Widget/Widget.cmp", None),
            Some(SourceLanguage::Aura)
        );
        assert_eq!(SourceLanguage::detect("ui/Widget.page", None), None);
        assert_eq!(
            SourceLanguage::detect("ui/Widget.page", Some("<apex:page/>")),
            Some(SourceLanguage::Visualforce)
        );
        assert_eq!(
            SourceLanguage::detect("ui/Widget.page", Some("<\t apex:page/>")),
            Some(SourceLanguage::Visualforce)
        );
        assert_eq!(
            SourceLanguage::detect("force-app/main/default/pages/Widget.page", None),
            Some(SourceLanguage::Visualforce)
        );
        assert_eq!(
            SourceLanguage::detect("pages/Widget.page", None),
            Some(SourceLanguage::Visualforce)
        );
        assert_eq!(
            SourceLanguage::detect(
                "layouts/not-front-matter.html",
                Some("---not-frontmatter\ntitle: Nope\n---\n<body/>")
            ),
            Some(SourceLanguage::Html)
        );
        assert_eq!(
            SourceLanguage::detect(
                "layouts/indented-close.html",
                Some("---\ntitle: Nope\n  ---\n<body/>")
            ),
            Some(SourceLanguage::Html)
        );
        assert_eq!(
            SourceLanguage::detect(
                "layouts/trailing-front-matter.html",
                Some("---\r\ntitle: Yep\r\n---   \r\n<body/>")
            ),
            Some(SourceLanguage::Liquid)
        );
    }

    #[test]
    fn framework_paths_and_content_detection_are_bounded() {
        assert_eq!(
            SourceLanguage::detect("game/Stats/Generated/Data/items.txt", None),
            Some(SourceLanguage::Bg3Stats)
        );
        assert_eq!(
            SourceLanguage::detect("game/Story/RawFiles/Goals/start.txt", None),
            Some(SourceLanguage::Osiris)
        );
        assert_eq!(
            SourceLanguage::detect("game/Localization/English/english.loca.xml", None),
            Some(SourceLanguage::Bg3Resource)
        );
        assert_eq!(
            SourceLanguage::detect("Public/orphan.xml", None),
            Some(SourceLanguage::Xml)
        );
        assert_eq!(
            SourceLanguage::detect("Public/MyMod/data/orphan.xml", None),
            Some(SourceLanguage::Bg3Resource)
        );
        assert_eq!(
            SourceLanguage::detect("play/conf/routes", None),
            Some(SourceLanguage::Yaml)
        );
        assert_eq!(
            SourceLanguage::detect("play/conf/admin.routes", None),
            Some(SourceLanguage::Yaml)
        );

        let late_marker = format!(
            "{}@interface TooLate",
            " ".repeat(super::CONTENT_CLASSIFIER_BYTES)
        );
        assert_eq!(
            SourceLanguage::detect("include/bounded.h", Some(&late_marker)),
            Some(SourceLanguage::C)
        );
    }

    #[test]
    fn future_content_gated_modes_only_admit_production_discovery_paths() {
        fn enabled(language: SourceLanguage) -> bool {
            matches!(
                language,
                SourceLanguage::Liquid | SourceLanguage::Aura | SourceLanguage::Visualforce
            )
        }

        assert!(!is_candidate_path_with("docs/readme.md", enabled));
        assert!(is_candidate_path_with("_posts/hello.md", enabled));
        assert!(!is_candidate_path_with("templates/default.html", enabled));
        assert!(is_candidate_path_with("_layouts/default.html", enabled));

        assert!(!is_candidate_path_with("ui/Widget.cmp", enabled));
        assert!(is_candidate_path_with("aura/Widget/Widget.cmp", enabled));
        assert!(!is_candidate_path_with("ui/Widget.page", enabled));
        assert!(is_candidate_path_with("pages/Widget.page", enabled));
        assert!(!is_candidate_path_with("ui/Widget.component", enabled));
        assert!(is_candidate_path_with(
            "force-app/main/default/pages/Widget.component",
            enabled
        ));
    }

    #[test]
    fn callable_search_signatures_reject_literal_defaults_without_rejecting_type_digits() {
        for safe in [
            "(value: u32) -> UTF8",
            "(ctx Context, items []Widget) error",
            "(value: Option<Result2>)",
        ] {
            assert!(callable_signature_is_literal_free(safe), "{safe}");
        }
        for unsafe_signature in [
            "(token = SECRET)",
            "(token 'secret')",
            "(token: `secret`)",
            "(retries: 3)",
            "(enabled: TRUE)",
            "(value DEFAULT PRIVATE_VALUE)",
            "(value: None)",
        ] {
            assert!(
                !callable_signature_is_literal_free(unsafe_signature),
                "{unsafe_signature}"
            );
        }
    }

    #[test]
    fn declaration_values_retain_references_without_retaining_credentials_or_literals() {
        for safe in [
            "OTHER_LIMIT",
            "feature::DEFAULT_LIMIT",
            "$DEFAULT_LIMIT",
            "${DEFAULT_LIMIT}",
            "FLAGS | ENABLED",
        ] {
            assert!(declaration_value_is_search_safe(safe), "{safe}");
        }
        for unsafe_value in [
            "sk_live_secret",
            "github_pat_credential",
            "database_password",
            "API_KEY",
            "ActualSecret7h3VeryLongCredential",
            "plain literal",
            "$(read_secret)",
            "42",
            "true",
            "'quoted'",
        ] {
            assert!(
                !declaration_value_is_search_safe(unsafe_value),
                "{unsafe_value}"
            );
        }
    }

    #[test]
    fn credential_token_screens_split_value_shapes_from_credential_words() {
        // Issued provider keys are recognised with either separator style.
        for key in [
            "AKIAIOSFODNN7EXAMPLE",
            "ASIAY34FZKBOKMUTVV7A",
            "sk_live_FAKE1234",
            "gho_abcdefgh",
            "ghs_abcdefgh12",
            "glpat-abcdefgh",
            "xoxb-1234-5678",
            "Glpat-aaaaaaaaaaaaaaaaaaaa",
            "SK_LIVE_FAKE1234567890abcdef",
            "akiaiosfodnn7example",
        ] {
            assert!(token_has_provider_key_shape(key), "{key}");
        }
        // Directory and module names, even ones sharing a prefix, are not keys.
        for word in [
            "token",
            "ERC20",
            "glpat",
            "xoxb",
            "asia-east1",
            "AsiaToken",
            "akiapolaau",
            "AKIA",
            "ghp_x",
            "sk_live_",
        ] {
            assert!(!token_has_provider_key_shape(word), "{word}");
        }
        // The declaration screen stays case-insensitive and adds credential
        // words and high-entropy values.
        for sensitive in [
            "password",
            "api_key",
            "access_token",
            "akiaiosfodnn7example",
            "Zx9mQ2vL8kP4rT6yW1nB3cD5",
        ] {
            assert!(value_token_is_sensitive(sensitive), "{sensitive}");
        }
        for plain in ["modules", "vpc", "ref", "v1", "IUniswapV3Pool"] {
            assert!(!value_token_is_sensitive(plain), "{plain}");
        }
    }

    #[test]
    fn persisted_symbol_signatures_share_one_literal_safe_policy() {
        for (kind, safe) in [
            (SymbolKind::Function, "(value: u32) -> Result"),
            (SymbolKind::Method, "Widget (Context *context)"),
            (SymbolKind::Constant, "= OTHER_LIMIT"),
            (SymbolKind::Variable, "= $DEFAULT_LIMIT"),
            (SymbolKind::Field, "[string]"),
            (
                SymbolKind::TypeAlias,
                "export type Identifier<T> = string | Readonly<T>;",
            ),
        ] {
            assert!(symbol_signature_is_search_safe(kind, safe), "{safe}");
        }
        for (kind, unsafe_signature) in [
            (SymbolKind::Function, "(token = 'secret')"),
            (SymbolKind::Constant, "= sk_live_secret"),
            (SymbolKind::Variable, "= 42"),
            (SymbolKind::Import, "#include \"secret.h\""),
            (SymbolKind::Class, "class Widget"),
            (SymbolKind::TypeAlias, "type Token = 'sk_live_secret';"),
        ] {
            assert!(
                !symbol_signature_is_search_safe(kind, unsafe_signature),
                "{unsafe_signature}"
            );
        }
    }

    #[test]
    fn serde_preserves_stable_languages_and_rejects_invalid_source_coordinates() {
        assert_eq!(
            serde_json::to_string(&SourceLanguage::TypeScript)
                .ok()
                .as_deref(),
            Some("\"typescript\"")
        );
        assert_eq!(
            serde_json::to_string(&SourceLanguage::JavaScript)
                .ok()
                .as_deref(),
            Some("\"javascript\"")
        );
        for (language, encoded) in [
            (SourceLanguage::Rust, "\"rust\""),
            (SourceLanguage::Python, "\"python\""),
            (SourceLanguage::Go, "\"go\""),
        ] {
            assert_eq!(
                serde_json::to_string(&language).ok().as_deref(),
                Some(encoded)
            );
        }
        assert!(
            serde_json::from_str::<SourcePosition>(r#"{"byte":0,"line":0,"column":0}"#).is_err()
        );
        assert!(
            serde_json::from_str::<SourceSpan>(
                r#"{"start":{"byte":1,"line":2,"column":0},"end":{"byte":2,"line":1,"column":0}}"#,
            )
            .is_err()
        );
    }

    fn representative_path(language: SourceLanguage) -> String {
        match language {
            SourceLanguage::Bg3Stats => "game/Stats/Generated/sample.txt".to_owned(),
            SourceLanguage::Aura => "force-app/main/default/aura/Sample/Sample.cmp".to_owned(),
            SourceLanguage::ChoiceScript => "game/scenes/startup.txt".to_owned(),
            SourceLanguage::DoomDecorate => "game/DECORATE".to_owned(),
            SourceLanguage::ParadoxScript => "game/events/start.txt".to_owned(),
            SourceLanguage::Visualforce => "force-app/main/default/pages/Sample.page".to_owned(),
            _ => {
                let extension = language
                    .v1_extensions()
                    .first()
                    .or_else(|| language.additional_extensions().first())
                    .unwrap_or_else(|| {
                        panic!("{} has no representative extension", language.as_str())
                    });
                format!("src/sample{extension}")
            }
        }
    }

    const fn representative_source(language: SourceLanguage) -> &'static str {
        match language {
            SourceLanguage::ChoiceScript => "*label start\n*choice\n  #Continue\n    *finish\n",
            SourceLanguage::Daedalus => "INSTANCE Start (C_NPC) { }\nFUNC VOID Run() { };\n",
            SourceLanguage::DoomDecorate => "actor Start 1000 { }\n",
            SourceLanguage::EnforceScript => "modded class Start { }\n",
            SourceLanguage::Inform6 => {
                "Constant Story \"Start\";\nInclude \"Parser\";\n[ Start; ];\n"
            }
            SourceLanguage::Lpc => "inherit \"/std/object\";\nvoid create() { }\n",
            SourceLanguage::ParadoxScript => "country_event = { immediate = { } }\n",
            SourceLanguage::Tads => "#include <adv3.h>\nstartRoom: Room { }\n",
            SourceLanguage::TorqueScript => "function Start(%value) { }\n",
            _ => "",
        }
    }

    fn position(byte: u64, line: u32, column: u32) -> SourcePosition {
        match SourcePosition::new(byte, line, column) {
            Ok(position) => position,
            Err(error) => panic!("test source position was invalid: {error}"),
        }
    }
}
