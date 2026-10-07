//! Stable, storage-independent contracts for Cartograph v2.
//!
//! Identifier constructors canonicalize UUID text at the boundary. Their
//! fields remain private so raw strings cannot accidentally cross between
//! project, generation, file, symbol, numerical site, document, model, and task identities.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

mod source;

pub use source::{
    FileParseStatus, InvalidNormalizedPath, InvalidSourceSpan, NormalizedPath, ReferenceKind,
    SourceLanguage, SourcePosition, SourceSpan, SymbolExecutionFlags, SymbolExportFlags,
    SymbolImplementationFlags, SymbolKind, Visibility, callable_signature_is_literal_free,
    declaration_value_is_search_safe, is_sensitive_value_token, symbol_signature_is_search_safe,
    token_has_provider_key_shape, v1_language_registry_digest, v2_language_additions_digest,
    value_token_is_sensitive,
};

const UUID_TEXT_LENGTH: usize = 36;
const UUID_BYTE_LENGTH: usize = 16;
const UUID_HYPHEN_OFFSETS: [usize; 4] = [8, 13, 18, 23];
const UUID_BYTE_HYPHEN_OFFSETS: [usize; 4] = [4, 6, 8, 10];
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";
const BLAKE3_HEX_LENGTH: usize = 64;
const BLAKE3_BYTE_LENGTH: usize = BLAKE3_HEX_LENGTH / 2;
const UPPER_NIBBLE_SHIFT: u8 = 4;
const NIBBLE_MASK: u8 = 0x0f;
const UUID_VERSION_INDEX: usize = 6;
const UUID_VARIANT_INDEX: usize = 8;
const UUID_V8_PREFIX: u8 = 0x80;
const UUID_RFC_VARIANT_PREFIX: u8 = 0x80;
const UUID_VERSION_MASK: u8 = 0x0f;
const UUID_VARIANT_MASK: u8 = 0x3f;
const HEX_DIGITS: &[u8] = b"0123456789abcdef";
const SOURCE_MANIFEST_DIGEST_DOMAIN: &[u8] = b"cartograph-v2-source-revision-v1";

/// A supplied branded identifier was not a canonicalizable, non-nil UUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidId;

impl fmt::Display for InvalidId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("identifier must be a non-nil UUID")
    }
}

impl std::error::Error for InvalidId {}

macro_rules! define_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            /// Parse and normalize a UUID without losing the identifier brand.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidId`] when `raw` is not a canonicalizable,
            /// non-nil UUID.
            pub fn parse(raw: &str) -> Result<Self, InvalidId> {
                normalize_uuid(raw).map(Self)
            }

            /// Return the canonical lowercase UUID text used at storage boundaries.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Build a deterministic RFC 9562 UUIDv8 from caller-owned hash bytes.
            #[must_use]
            pub fn from_uuid_v8(bytes: [u8; UUID_BYTE_LENGTH]) -> Self {
                Self(format_uuid_v8(bytes))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::parse(raw)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::parse(&raw)
            }
        }
    };
}

define_id!(ProjectId, "A stable Cartograph project identity.");
define_id!(FileId, "A stable source-file identity.");
define_id!(SymbolId, "A stable code-symbol identity.");
define_id!(NumericalSiteId, "A stable numerical source-site identity.");
define_id!(GenerationId, "An immutable index-generation identity.");
define_id!(DocumentId, "A stable logical search-document identity.");
define_id!(ModelId, "A registered embedding-model identity.");
define_id!(TaskId, "A persisted coding-task identity.");
define_id!(LeaseId, "An observable project-operation lease identity.");

/// A deterministic BLAKE3 digest used to compare complete logical generations.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ContentDigest(String);

impl ContentDigest {
    /// Parse a 32-byte digest rendered as hexadecimal text.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDigest`] when `raw` is not exactly 64 hexadecimal
    /// characters.
    pub fn parse(raw: &str) -> Result<Self, InvalidDigest> {
        let canonical = raw.to_ascii_lowercase();
        let valid = canonical.len() == BLAKE3_HEX_LENGTH
            && canonical.bytes().all(|byte| byte.is_ascii_hexdigit());
        valid.then_some(Self(canonical)).ok_or(InvalidDigest)
    }

    /// Render an exact 32-byte digest without a fallible text round trip.
    #[must_use]
    pub fn from_bytes(bytes: [u8; BLAKE3_BYTE_LENGTH]) -> Self {
        let mut encoded = String::with_capacity(BLAKE3_HEX_LENGTH);
        for byte in bytes {
            encoded.push(char::from(
                HEX_DIGITS[usize::from(byte >> UPPER_NIBBLE_SHIFT)],
            ));
            encoded.push(char::from(HEX_DIGITS[usize::from(byte & NIBBLE_MASK)]));
        }
        Self(encoded)
    }

    /// Return the canonical lowercase hexadecimal digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ContentDigest {
    type Err = InvalidDigest;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl TryFrom<String> for ContentDigest {
    type Error = InvalidDigest;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

/// A supplied deterministic digest was not 32-byte hexadecimal BLAKE3 text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidDigest;

impl fmt::Display for InvalidDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("content digest must contain 64 hexadecimal characters")
    }
}

impl std::error::Error for InvalidDigest {}

/// Mutating project operation serialized by an observable database lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOperation {
    /// Full or incremental code indexing.
    Index,
    /// Explicit synchronization of changed project state.
    Sync,
    /// Hook-triggered indexing or synchronization.
    Hook,
    /// Schema or storage migration.
    Migration,
    /// Derived BM25/vector index rebuild.
    Rebuild,
}

impl ProjectOperation {
    /// Parse the stable PostgreSQL representation.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProjectOperation`] when `raw` is not a known durable
    /// operation name.
    pub fn parse(raw: &str) -> Result<Self, InvalidProjectOperation> {
        match raw {
            "index" => Ok(Self::Index),
            "sync" => Ok(Self::Sync),
            "hook" => Ok(Self::Hook),
            "migration" => Ok(Self::Migration),
            "rebuild" => Ok(Self::Rebuild),
            _ => Err(InvalidProjectOperation),
        }
    }

    /// Stable PostgreSQL representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Sync => "sync",
            Self::Hook => "hook",
            Self::Migration => "migration",
            Self::Rebuild => "rebuild",
        }
    }
}

/// An unknown durable project-operation value was encountered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidProjectOperation;

impl fmt::Display for InvalidProjectOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("project operation is not recognized")
    }
}

impl std::error::Error for InvalidProjectOperation {}

/// Durable lifecycle state for one immutable index generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    /// Structural and search facts are still being staged.
    Staging,
    /// Every required stage finished and the generation may be published.
    Ready,
    /// This is the one complete generation visible to project readers.
    Current,
    /// A newer complete generation replaced this one.
    Superseded,
    /// Indexing terminated without producing a publishable generation.
    Failed,
    /// Retention owns this generation; its facts may be partially deleted.
    Retiring,
}

impl GenerationState {
    /// Stable PostgreSQL representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Ready => "ready",
            Self::Current => "current",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
            Self::Retiring => "retiring",
        }
    }
}

/// Versioned contract used to interpret a persisted logical-generation digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i16)]
pub enum GenerationDigestVersion {
    /// Original structural/search digest before reference-evidence persistence.
    V1 = 1,
    /// Complete digest including owner, unresolved-name, and resolver provenance evidence.
    V2 = 2,
    /// Reference-complete digest including exact represented call-site multiplicity.
    V3 = 3,
    /// Symbol-semantics-complete digest including visibility and declaration flags.
    V4 = 4,
    /// Native-index semantics including framework, resolver, and test-ownership evidence.
    V5 = 5,
    /// Cargo-workspace Rust crate and named-reexport resolution semantics.
    V6 = 6,
    /// Generation-scoped, privacy-safe static numerical site evidence.
    V7 = 7,
    /// Context-classified structural diagnostics and semantic clone evidence.
    V8 = 8,
    /// Precision-fenced executable, resolver, import-consumer, and biomarker evidence.
    V9 = 9,
    /// Call-target-precise secret exposure and incomplete-implementation evidence.
    V10 = 10,
    /// Anonymous Rust call-target normalization.
    V11 = 11,
    /// Stable bounded Go and Python anonymous call-target normalization.
    V12 = 12,
    /// Stable named TypeScript and JavaScript construction-target normalization.
    V13 = 13,
    /// Slang/WESL language facts and bounded JavaScript dispatch evidence.
    V14 = 14,
    /// Ada/VHDL unit resolution and guarded numerical-analysis precision.
    V15 = 15,
    /// Nominal Rust self-receiver ownership and cross-file method resolution.
    V16 = 16,
    /// Updated `ArkTS` and OCaml grammar semantics.
    V17 = 17,
    /// Updated CUDA grammar and Unicode identifier semantics.
    V18 = 18,
    /// Rust macro-argument references.
    V19 = 19,
    /// Rust turbofish calls that name their function.
    V20 = 20,
    /// Dedicated language families and v1 extraction parity across languages.
    V21 = 21,
    /// V1 cross-file resolution parity across languages.
    V22 = 22,
    /// V1 framework/bridge detail and deferred resolution parity across languages.
    V23 = 23,
}

impl GenerationDigestVersion {
    /// Current digest contract emitted by this Cartograph v2 binary.
    pub const CURRENT: Self = Self::V23;

    /// Every admitted contract version in ascending order.
    ///
    /// A new version is added here and nowhere else; validation walks this list
    /// rather than repeating one guarded arm per version.
    pub const ALL: [Self; 23] = [
        Self::V1,
        Self::V2,
        Self::V3,
        Self::V4,
        Self::V5,
        Self::V6,
        Self::V7,
        Self::V8,
        Self::V9,
        Self::V10,
        Self::V11,
        Self::V12,
        Self::V13,
        Self::V14,
        Self::V15,
        Self::V16,
        Self::V17,
        Self::V18,
        Self::V19,
        Self::V20,
        Self::V21,
        Self::V22,
        Self::V23,
    ];

    /// Stable PostgreSQL `smallint` representation.
    #[must_use]
    pub const fn database_value(self) -> i16 {
        self as i16
    }

    /// Validate a PostgreSQL value before it enters generation type-state.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidGenerationDigestVersion`] when `value` does not name a
    /// supported digest contract.
    pub const fn from_database_value(value: i16) -> Result<Self, InvalidGenerationDigestVersion> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let candidate = Self::ALL[index];
            if candidate.database_value() == value {
                return Ok(candidate);
            }
            index += 1;
        }
        Err(InvalidGenerationDigestVersion)
    }
}

/// A stored logical-generation digest used an unknown contract version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidGenerationDigestVersion;

impl fmt::Display for InvalidGenerationDigestVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("generation digest version is not recognized")
    }
}

impl std::error::Error for InvalidGenerationDigestVersion {}

/// Privacy-preserving database identity shared by import and normal project runtime lookup.
#[must_use]
pub fn project_root_identity(repository_fingerprint: &ContentDigest) -> String {
    format!("project:{}", repository_fingerprint.as_str())
}

/// Ordered, bounded builder for the source-manifest digest used by indexing and freshness.
pub struct SourceManifestDigestBuilder {
    hasher: blake3::Hasher,
    remaining: usize,
    previous_path: Option<NormalizedPath>,
}

impl SourceManifestDigestBuilder {
    /// Begin an exact-size manifest. Entries must be pushed in ascending normalized-path order.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSourceManifest`] when `files` cannot be represented by
    /// the stable manifest encoding.
    pub fn new(files: usize) -> Result<Self, InvalidSourceManifest> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(SOURCE_MANIFEST_DIGEST_DOMAIN);
        hash_manifest_length(&mut hasher, files)?;
        Ok(Self {
            hasher,
            remaining: files,
            previous_path: None,
        })
    }

    /// Add one normalized path and its BLAKE3 content digest.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSourceManifest`] when too many entries are pushed, the
    /// paths are not strictly ascending, or an encoded length is unsupported.
    pub fn push(
        &mut self,
        path: &NormalizedPath,
        content_hash: &ContentDigest,
    ) -> Result<(), InvalidSourceManifest> {
        if self.remaining == 0
            || self
                .previous_path
                .as_ref()
                .is_some_and(|previous| previous >= path)
        {
            return Err(InvalidSourceManifest);
        }
        hash_manifest_text(&mut self.hasher, path.as_str())?;
        hash_manifest_text(&mut self.hasher, content_hash.as_str())?;
        self.previous_path = Some(path.clone());
        self.remaining -= 1;
        Ok(())
    }

    /// Finalize only after the declared number of entries was supplied.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSourceManifest`] when fewer entries were pushed than
    /// declared at construction.
    pub fn finish(self) -> Result<ContentDigest, InvalidSourceManifest> {
        if self.remaining != 0 {
            return Err(InvalidSourceManifest);
        }
        Ok(ContentDigest::from_bytes(
            *self.hasher.finalize().as_bytes(),
        ))
    }
}

fn hash_manifest_text(
    hasher: &mut blake3::Hasher,
    value: &str,
) -> Result<(), InvalidSourceManifest> {
    hash_manifest_length(hasher, value.len())?;
    hasher.update(value.as_bytes());
    Ok(())
}

fn hash_manifest_length(
    hasher: &mut blake3::Hasher,
    value: usize,
) -> Result<(), InvalidSourceManifest> {
    let length = u64::try_from(value).map_err(|_| InvalidSourceManifest)?;
    hasher.update(&length.to_le_bytes());
    Ok(())
}

/// A source manifest had the wrong count, ordering, or representable length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidSourceManifest;

impl fmt::Display for InvalidSourceManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("source manifest must have an exact ordered path and digest set")
    }
}

impl std::error::Error for InvalidSourceManifest {}

/// Search-document category used for intent routing and field boosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// One extracted code symbol.
    Symbol,
    /// File-level structural and summary evidence.
    File,
    /// Human-authored documentation.
    Documentation,
    /// Test code or test-case evidence.
    Test,
    /// Build, deployment, or tool configuration.
    Configuration,
}

impl DocumentKind {
    /// Stable PostgreSQL representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::Documentation => "documentation",
            Self::Test => "test",
            Self::Configuration => "configuration",
        }
    }
}

/// Structural graph relationship between two symbols.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum EdgeKind {
    /// The source invokes the target.
    Calls,
    /// The source imports the target's module or package.
    Imports,
    /// The source contains a resolved reference to the target.
    References,
    /// The source implements the target contract.
    Implements,
    /// The source extends or inherits from the target.
    Extends,
    /// The source test exercises the target.
    Tests,
    /// The source has or consumes the target type.
    TypeOf,
    /// The source returns the target type.
    Returns,
    /// The source constructs the target.
    Instantiates,
    /// The source overrides the target declaration.
    Overrides,
    /// The source is decorated by the target.
    Decorates,
    /// The source accesses a target field.
    FieldAccess,
    /// The source defines and subsequently uses the target binding.
    DefUse,
    /// The source exports the target.
    Exports,
    /// The source lexical scope contains the target.
    Contains,
}

const EDGE_KIND_VALUES: [&str; EdgeKind::Contains as usize + 1] = [
    "calls",
    "imports",
    "references",
    "implements",
    "extends",
    "tests",
    "type_of",
    "returns",
    "instantiates",
    "overrides",
    "decorates",
    "field_access",
    "def_use",
    "exports",
    "contains",
];

const EDGE_KINDS: [EdgeKind; EdgeKind::Contains as usize + 1] = [
    EdgeKind::Calls,
    EdgeKind::Imports,
    EdgeKind::References,
    EdgeKind::Implements,
    EdgeKind::Extends,
    EdgeKind::Tests,
    EdgeKind::TypeOf,
    EdgeKind::Returns,
    EdgeKind::Instantiates,
    EdgeKind::Overrides,
    EdgeKind::Decorates,
    EdgeKind::FieldAccess,
    EdgeKind::DefUse,
    EdgeKind::Exports,
    EdgeKind::Contains,
];

impl EdgeKind {
    /// Every edge kind, in stable storage order.
    pub const ALL: [Self; EDGE_KINDS.len()] = EDGE_KINDS;

    /// Stable PostgreSQL representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        EDGE_KIND_VALUES[self as usize]
    }

    /// Parse the stable PostgreSQL and wire representation.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        EDGE_KINDS.into_iter().find(|kind| kind.as_str() == value)
    }
}

fn format_uuid_v8(mut bytes: [u8; UUID_BYTE_LENGTH]) -> String {
    bytes[UUID_VERSION_INDEX] = (bytes[UUID_VERSION_INDEX] & UUID_VERSION_MASK) | UUID_V8_PREFIX;
    bytes[UUID_VARIANT_INDEX] =
        (bytes[UUID_VARIANT_INDEX] & UUID_VARIANT_MASK) | UUID_RFC_VARIANT_PREFIX;
    let mut encoded = String::with_capacity(UUID_TEXT_LENGTH);
    for (index, byte) in bytes.into_iter().enumerate() {
        if UUID_BYTE_HYPHEN_OFFSETS.contains(&index) {
            encoded.push('-');
        }
        encoded.push(char::from(
            HEX_DIGITS[usize::from(byte >> UPPER_NIBBLE_SHIFT)],
        ));
        encoded.push(char::from(HEX_DIGITS[usize::from(byte & NIBBLE_MASK)]));
    }
    encoded
}

fn normalize_uuid(raw: &str) -> Result<String, InvalidId> {
    if raw.len() != UUID_TEXT_LENGTH || !raw.is_ascii() {
        return Err(InvalidId);
    }
    for (offset, byte) in raw.bytes().enumerate() {
        let expects_hyphen = UUID_HYPHEN_OFFSETS.contains(&offset);
        if (expects_hyphen && byte != b'-') || (!expects_hyphen && !byte.is_ascii_hexdigit()) {
            return Err(InvalidId);
        }
    }
    let canonical = raw.to_ascii_lowercase();
    if canonical == NIL_UUID {
        return Err(InvalidId);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::{
        BLAKE3_BYTE_LENGTH, BLAKE3_HEX_LENGTH, ContentDigest, DocumentKind, EdgeKind, FileId,
        FileParseStatus, GenerationDigestVersion, GenerationId, GenerationState, LeaseId,
        NormalizedPath, ProjectId, ProjectOperation, SourceManifestDigestBuilder, SymbolId,
        UUID_BYTE_LENGTH, project_root_identity,
    };

    const UPPERCASE_UUID: &str = "4EACCC79-2ED5-4E22-8D77-A8E66D13C345";
    const CANONICAL_UUID: &str = "4eaccc79-2ed5-4e22-8d77-a8e66d13c345";
    const TEST_DIGEST_BYTE: u8 = 0xab;
    const TEST_UUID_V8_BYTE: u8 = 0x11;
    const EXPECTED_TEST_UUID_V8: &str = "11111111-1111-8111-9111-111111111111";
    const DIGEST_V1_DATABASE_VALUE: i16 = 1;
    const DIGEST_V2_DATABASE_VALUE: i16 = 2;
    const DIGEST_V3_DATABASE_VALUE: i16 = 3;
    const DIGEST_V4_DATABASE_VALUE: i16 = 4;
    const DIGEST_V5_DATABASE_VALUE: i16 = 5;
    const DIGEST_V6_DATABASE_VALUE: i16 = 6;
    const DIGEST_V7_DATABASE_VALUE: i16 = 7;
    const DIGEST_V8_DATABASE_VALUE: i16 = 8;
    const DIGEST_V9_DATABASE_VALUE: i16 = 9;
    const DIGEST_V10_DATABASE_VALUE: i16 = 10;
    const DIGEST_V11_DATABASE_VALUE: i16 = 11;
    const DIGEST_V12_DATABASE_VALUE: i16 = 12;
    const DIGEST_V13_DATABASE_VALUE: i16 = 13;
    const DIGEST_V14_DATABASE_VALUE: i16 = 14;
    const DIGEST_V15_DATABASE_VALUE: i16 = 15;
    const DIGEST_V16_DATABASE_VALUE: i16 = 16;
    const DIGEST_V17_DATABASE_VALUE: i16 = 17;
    const DIGEST_V18_DATABASE_VALUE: i16 = 18;
    const DIGEST_V19_DATABASE_VALUE: i16 = 19;
    const DIGEST_V20_DATABASE_VALUE: i16 = 20;
    const DIGEST_V21_DATABASE_VALUE: i16 = 21;
    const DIGEST_V22_DATABASE_VALUE: i16 = 22;
    const DIGEST_V23_DATABASE_VALUE: i16 = 23;
    const UNKNOWN_DIGEST_DATABASE_VALUE: i16 = 24;

    #[test]
    fn branded_ids_canonicalize_and_validate_deserialized_values() {
        let project = ProjectId::parse(UPPERCASE_UUID);
        let project = match project {
            Ok(project) => project,
            Err(error) => panic!("valid project UUID was rejected: {error}"),
        };

        assert_eq!(project.as_str(), CANONICAL_UUID);
        assert_eq!(
            serde_json::to_string(&project).ok().as_deref(),
            Some("\"4eaccc79-2ed5-4e22-8d77-a8e66d13c345\"")
        );
        assert!(serde_json::from_str::<GenerationId>("\"not-a-uuid\"").is_err());
        assert!(ProjectId::parse("00000000-0000-0000-0000-000000000000").is_err());
    }

    #[test]
    fn lifecycle_and_document_kinds_have_stable_database_values() {
        assert_eq!(GenerationState::Staging.as_str(), "staging");
        assert_eq!(GenerationState::Current.as_str(), "current");
        assert_eq!(DocumentKind::Symbol.as_str(), "symbol");
        assert_eq!(DocumentKind::Documentation.as_str(), "documentation");
        assert_eq!(FileParseStatus::Parsed.as_str(), "parsed");
        assert_eq!(FileParseStatus::Skipped.as_str(), "skipped");
        assert_eq!(EdgeKind::Instantiates.as_str(), "instantiates");
        assert_eq!(EdgeKind::DefUse.as_str(), "def_use");
        let fingerprint = ContentDigest::from_bytes([TEST_DIGEST_BYTE; BLAKE3_BYTE_LENGTH]);
        assert_eq!(
            project_root_identity(&fingerprint),
            format!("project:{}", fingerprint.as_str())
        );
    }

    #[test]
    fn generation_digest_versions_have_stable_database_values() {
        let versions = [
            (DIGEST_V1_DATABASE_VALUE, GenerationDigestVersion::V1),
            (DIGEST_V2_DATABASE_VALUE, GenerationDigestVersion::V2),
            (DIGEST_V3_DATABASE_VALUE, GenerationDigestVersion::V3),
            (DIGEST_V4_DATABASE_VALUE, GenerationDigestVersion::V4),
            (DIGEST_V5_DATABASE_VALUE, GenerationDigestVersion::V5),
            (DIGEST_V6_DATABASE_VALUE, GenerationDigestVersion::V6),
            (DIGEST_V7_DATABASE_VALUE, GenerationDigestVersion::V7),
            (DIGEST_V8_DATABASE_VALUE, GenerationDigestVersion::V8),
            (DIGEST_V9_DATABASE_VALUE, GenerationDigestVersion::V9),
            (DIGEST_V10_DATABASE_VALUE, GenerationDigestVersion::V10),
            (DIGEST_V11_DATABASE_VALUE, GenerationDigestVersion::V11),
            (DIGEST_V12_DATABASE_VALUE, GenerationDigestVersion::V12),
            (DIGEST_V13_DATABASE_VALUE, GenerationDigestVersion::V13),
            (DIGEST_V14_DATABASE_VALUE, GenerationDigestVersion::V14),
            (DIGEST_V15_DATABASE_VALUE, GenerationDigestVersion::V15),
            (DIGEST_V16_DATABASE_VALUE, GenerationDigestVersion::V16),
            (DIGEST_V17_DATABASE_VALUE, GenerationDigestVersion::V17),
            (DIGEST_V18_DATABASE_VALUE, GenerationDigestVersion::V18),
            (DIGEST_V19_DATABASE_VALUE, GenerationDigestVersion::V19),
            (DIGEST_V20_DATABASE_VALUE, GenerationDigestVersion::V20),
            (DIGEST_V21_DATABASE_VALUE, GenerationDigestVersion::V21),
            (DIGEST_V22_DATABASE_VALUE, GenerationDigestVersion::V22),
            (DIGEST_V23_DATABASE_VALUE, GenerationDigestVersion::V23),
        ];
        for (value, version) in versions {
            assert_eq!(version.database_value(), value);
            assert_eq!(
                GenerationDigestVersion::from_database_value(value),
                Ok(version)
            );
        }
        assert_eq!(
            GenerationDigestVersion::CURRENT.database_value(),
            DIGEST_V23_DATABASE_VALUE
        );
        assert!(
            GenerationDigestVersion::from_database_value(UNKNOWN_DIGEST_DATABASE_VALUE).is_err()
        );
    }

    #[test]
    fn generation_digests_canonicalize_but_reject_malformed_content() {
        let uppercase = "A".repeat(BLAKE3_HEX_LENGTH);
        let digest = ContentDigest::parse(&uppercase);
        assert_matches!(digest, Ok(value) if value.as_str() == "a".repeat(BLAKE3_HEX_LENGTH));
        assert!(ContentDigest::parse("abc").is_err());
        assert!(ContentDigest::parse(&"z".repeat(BLAKE3_HEX_LENGTH)).is_err());
        assert_eq!(
            ContentDigest::from_bytes([TEST_DIGEST_BYTE; BLAKE3_BYTE_LENGTH]).as_str(),
            "ab".repeat(BLAKE3_BYTE_LENGTH)
        );
    }

    #[test]
    fn source_manifest_digest_is_frozen_exact_sized_and_strictly_ordered() {
        let a = NormalizedPath::parse("a.rs")
            .unwrap_or_else(|error| panic!("manifest path failed: {error}"));
        let b = NormalizedPath::parse("src/b.ts")
            .unwrap_or_else(|error| panic!("manifest path failed: {error}"));
        let a_hash = ContentDigest::from_bytes([1; BLAKE3_BYTE_LENGTH]);
        let b_hash = ContentDigest::from_bytes([2; BLAKE3_BYTE_LENGTH]);
        let mut complete = SourceManifestDigestBuilder::new(2)
            .unwrap_or_else(|error| panic!("manifest builder failed: {error}"));
        assert!(complete.push(&a, &a_hash).is_ok());
        assert!(complete.push(&b, &b_hash).is_ok());
        let digest = complete
            .finish()
            .unwrap_or_else(|error| panic!("manifest finish failed: {error}"));
        assert_eq!(
            digest.as_str(),
            "dbf7b7a4b06f64d04cce7a209c45bec03b9ceea6909d0bd79f19228d9ad53be9"
        );

        let mut under = SourceManifestDigestBuilder::new(2)
            .unwrap_or_else(|error| panic!("manifest builder failed: {error}"));
        assert!(under.push(&a, &a_hash).is_ok());
        assert!(under.finish().is_err());

        let mut over = SourceManifestDigestBuilder::new(1)
            .unwrap_or_else(|error| panic!("manifest builder failed: {error}"));
        assert!(over.push(&a, &a_hash).is_ok());
        assert!(over.push(&b, &b_hash).is_err());

        let mut duplicate = SourceManifestDigestBuilder::new(2)
            .unwrap_or_else(|error| panic!("manifest builder failed: {error}"));
        assert!(duplicate.push(&a, &a_hash).is_ok());
        assert!(duplicate.push(&a, &a_hash).is_err());

        let mut out_of_order = SourceManifestDigestBuilder::new(2)
            .unwrap_or_else(|error| panic!("manifest builder failed: {error}"));
        assert!(out_of_order.push(&b, &b_hash).is_ok());
        assert!(out_of_order.push(&a, &a_hash).is_err());
    }

    #[test]
    fn operation_and_lease_contracts_have_stable_storage_values() {
        assert_eq!(ProjectOperation::Index.as_str(), "index");
        assert_eq!(ProjectOperation::Hook.as_str(), "hook");
        assert_eq!(ProjectOperation::Rebuild.as_str(), "rebuild");
        assert!(LeaseId::parse(CANONICAL_UUID).is_ok());
    }

    #[test]
    fn deterministic_uuid_bytes_preserve_identifier_brands() {
        let file = FileId::from_uuid_v8([TEST_UUID_V8_BYTE; UUID_BYTE_LENGTH]);
        let symbol = SymbolId::from_uuid_v8([TEST_UUID_V8_BYTE; UUID_BYTE_LENGTH]);
        assert_eq!(file.as_str(), EXPECTED_TEST_UUID_V8);
        assert_eq!(symbol.as_str(), file.as_str());
    }
}
