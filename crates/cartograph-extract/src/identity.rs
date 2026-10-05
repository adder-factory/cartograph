use std::collections::HashMap;

use cartograph_domain::{
    FileId, NormalizedPath, NumericalSiteId, SourceSpan, SymbolId, SymbolKind,
};

use crate::ExtractError;

const FILE_ID_CONTEXT: &str = "cartograph.v2.file-id.2026-07-22";
const SYMBOL_ID_CONTEXT: &str = "cartograph.v2.symbol-id.2026-07-22";
const NUMERICAL_SITE_ID_CONTEXT: &str = "cartograph.v2.numerical-site-id.2026-08-01";
const UUID_BYTES: usize = 16;

pub(crate) fn file_id(path: &NormalizedPath) -> FileId {
    FileId::from_uuid_v8(stable_uuid_bytes(
        FILE_ID_CONTEXT,
        &[path.as_str().as_bytes()],
    ))
}

pub(crate) fn numerical_site_id(
    file_id: &FileId,
    span: &SourceSpan,
    categories: [&str; 2],
) -> NumericalSiteId {
    let [operation, hazard] = categories;
    NumericalSiteId::from_uuid_v8(stable_uuid_bytes(
        NUMERICAL_SITE_ID_CONTEXT,
        &[
            file_id.as_str().as_bytes(),
            &span.start_byte().to_le_bytes(),
            &span.end_byte().to_le_bytes(),
            operation.as_bytes(),
            hazard.as_bytes(),
        ],
    ))
}

pub(crate) struct SymbolIdentity<'a> {
    path: &'a NormalizedPath,
    ordinals: HashMap<(SymbolKind, String), u64>,
}

impl<'a> SymbolIdentity<'a> {
    pub(crate) fn new(path: &'a NormalizedPath) -> Self {
        Self {
            path,
            ordinals: HashMap::new(),
        }
    }

    pub(crate) fn next(
        &mut self,
        kind: SymbolKind,
        qualified_name: &str,
    ) -> Result<SymbolId, ExtractError> {
        self.ordinals
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        let mut owned_name = String::new();
        owned_name
            .try_reserve(qualified_name.len())
            .map_err(|_| ExtractError::OutputLimit)?;
        owned_name.push_str(qualified_name);
        let key = (kind, owned_name);
        let ordinal = self.ordinals.entry(key).or_default();
        let ordinal_bytes = ordinal.to_le_bytes();
        let id = SymbolId::from_uuid_v8(stable_uuid_bytes(
            SYMBOL_ID_CONTEXT,
            &[
                self.path.as_str().as_bytes(),
                kind.as_str().as_bytes(),
                qualified_name.as_bytes(),
                &ordinal_bytes,
            ],
        ));
        *ordinal = ordinal.saturating_add(1);
        Ok(id)
    }

    /// Exchange ordinal state with another identity over the same path, so an
    /// embedded extraction pass continues this file's ordinal sequence.
    pub(crate) fn exchange_ordinals(&mut self, other: &mut SymbolIdentity<'_>) {
        debug_assert_eq!(self.path, other.path);
        std::mem::swap(&mut self.ordinals, &mut other.ordinals);
    }
}

fn stable_uuid_bytes(context: &str, fields: &[&[u8]]) -> [u8; UUID_BYTES] {
    let mut hasher = blake3::Hasher::new_derive_key(context);
    for field in fields {
        hasher.update(&encoded_length(field.len()));
        hasher.update(field);
    }
    let mut bytes = [0_u8; UUID_BYTES];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..UUID_BYTES]);
    bytes
}

fn encoded_length(length: usize) -> [u8; std::mem::size_of::<u64>()] {
    u64::try_from(length).unwrap_or(u64::MAX).to_le_bytes()
}
