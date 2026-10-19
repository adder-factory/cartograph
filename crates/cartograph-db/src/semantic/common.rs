const EMBEDDING_SOURCE_DOMAIN: &[u8] = b"cartograph-v2-embedding-source-v1";

pub(crate) fn validate_normalization(
    vector: &[f32],
    normalization: EmbeddingNormalization,
) -> Result<(), SemanticStorageError> {
    if normalization == EmbeddingNormalization::None {
        return Ok(());
    }
    let magnitude = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    ((magnitude - 1.0).abs() <= 0.001).ok_or(super::types::invalid("embedding_normalization"))
}

pub(crate) struct DocumentSource {
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) kind: DocumentKind,
    pub(crate) qualified_name: String,
    pub(crate) code: String,
    pub(crate) natural_text: String,
    pub(crate) summary: String,
}

impl DocumentSource {
    pub(crate) fn encoded_length(&self) -> Result<u64, SemanticStorageError> {
        [
            &self.path,
            &self.language,
            self.kind.as_str(),
            &self.qualified_name,
            &self.code,
            &self.natural_text,
            &self.summary,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| {
            let length = u64::try_from(value.len()).map_err(|_| corrupt("document_source"))?;
            total
                .checked_add(length)
                .and_then(|sum| sum.checked_add(32))
                .ok_or_else(|| corrupt("document_source"))
        })
    }

    pub(crate) fn render(self) -> Result<(String, ContentDigest), SemanticStorageError> {
        let mut text = String::new();
        let capacity = usize::try_from(self.encoded_length()?)
            .map_err(|_| database_error("render-document-source"))?;
        text.try_reserve(capacity)
            .map_err(|_| database_error("render-document-source"))?;
        append_field(&mut text, "path", &self.path);
        append_field(&mut text, "language", &self.language);
        append_field(&mut text, "kind", self.kind.as_str());
        append_field(&mut text, "name", &self.qualified_name);
        append_field(&mut text, "code", &self.code);
        append_field(&mut text, "natural_text", &self.natural_text);
        append_field(&mut text, "summary", &self.summary);
        let mut hasher = blake3::Hasher::new();
        hasher.update(EMBEDDING_SOURCE_DOMAIN);
        hasher.update(&u64::try_from(text.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(text.as_bytes());
        Ok((
            text,
            ContentDigest::from_bytes(*hasher.finalize().as_bytes()),
        ))
    }
}

fn append_field(output: &mut String, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(label);
    output.push_str(":\n");
    output.push_str(value);
}

pub(crate) const fn corrupt(field: &'static str) -> SemanticStorageError {
    SemanticStorageError::CorruptStoredValue { field }
}

use super::types::{EmbeddingNormalization, SemanticStorageError, database_error};
use cartograph_domain::{ContentDigest, DocumentKind};
