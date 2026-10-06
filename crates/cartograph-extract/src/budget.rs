use std::mem::size_of;

use crate::{
    Containment, ExtractError, ExtractedCallScopeSite, ExtractedFile, ExtractedImportBinding,
    ExtractedNumericalSite, ExtractedReference, ExtractedSymbol, ExtractionDiagnostic,
    JavascriptMemberCallContext, JavascriptMemberReceiver, SourceSnapshot,
};

// The transient construction budget charges two vector-growth slots per fact so an allocation
// cannot outrun supervisor accounting. It is deliberately separate from the smaller retained
// output ceiling checked against the completed file.
const PARSER_RESERVATION_MULTIPLIER: u64 = 64;
const MINIMUM_PARSER_RESERVATION_BYTES: u64 = 1024 * 1024;
const READ_RESERVATION_MULTIPLIER: u64 = 2;
const MINIMUM_READ_RESERVATION_BYTES: u64 = 128 * 1024;
// Dense schemas can emit several bounded graph facts per source token. The completed output stays
// below the independent 64x construction reservation while admitting generated API fixtures.
const OUTPUT_LIMIT_MULTIPLIER: u64 = 32;
const MINIMUM_OUTPUT_LIMIT_BYTES: u64 = 256 * 1024;
const MINIMUM_FACT_LIMIT: u64 = 1024;
const MINIMUM_STRING_LIMIT_BYTES: u64 = 4096;
const MAXIMUM_FACT_STRING_BYTES: u64 = 256 * 1024;
const MAXIMUM_QUALIFIER_SEPARATOR_BYTES: u64 = 512;
const VECTOR_GROWTH_ALLOWANCE: u64 = 2;

/// Conservative in-flight reservation for source, Tree-sitter, and bounded Rust facts.
#[must_use]
pub fn native_extraction_reservation(source_bytes: u64) -> Option<u64> {
    source_bytes
        .checked_mul(PARSER_RESERVATION_MULTIPLIER)
        .and_then(|bytes| bytes.checked_add(MINIMUM_PARSER_RESERVATION_BYTES))
}

/// Conservative in-flight reservation for streamed UTF-8 validation and hashing.
#[must_use]
pub fn native_read_reservation(source_bytes: u64) -> Option<u64> {
    source_bytes
        .checked_mul(READ_RESERVATION_MULTIPLIER)
        .and_then(|bytes| bytes.checked_add(MINIMUM_READ_RESERVATION_BYTES))
}

/// Hard per-file limit for modeled Rust-owned extraction output.
#[must_use]
pub fn native_output_limit(source_bytes: u64) -> Option<u64> {
    source_bytes
        .checked_mul(OUTPUT_LIMIT_MULTIPLIER)
        .and_then(|bytes| bytes.checked_add(MINIMUM_OUTPUT_LIMIT_BYTES))
}

pub(crate) struct ExtractionBudget {
    retained_output_limit: u64,
    working_limit: u64,
    string_limit: u64,
    fact_limit: u64,
    retained_bytes: u64,
    facts: u64,
}

impl ExtractionBudget {
    pub(crate) fn new(snapshot: &SourceSnapshot) -> Result<Self, ExtractError> {
        let source_bytes = snapshot.byte_size();
        let retained_output_limit =
            native_output_limit(source_bytes).ok_or(ExtractError::OutputLimit)?;
        let working_limit =
            native_extraction_reservation(source_bytes).ok_or(ExtractError::OutputLimit)?;
        let string_limit = source_bytes
            .checked_add(MAXIMUM_QUALIFIER_SEPARATOR_BYTES)
            .ok_or(ExtractError::OutputLimit)?
            .clamp(MINIMUM_STRING_LIMIT_BYTES, MAXIMUM_FACT_STRING_BYTES);
        let fact_limit = source_bytes
            .checked_add(MINIMUM_FACT_LIMIT)
            .ok_or(ExtractError::OutputLimit)?;
        let retained_bytes = snapshot_header_bytes(snapshot).ok_or(ExtractError::OutputLimit)?;
        if retained_bytes > retained_output_limit || retained_bytes > working_limit {
            return Err(ExtractError::OutputLimit);
        }
        Ok(Self {
            retained_output_limit,
            working_limit,
            string_limit,
            fact_limit,
            retained_bytes,
            facts: 0,
        })
    }

    pub(crate) fn reserve_fact<'value>(
        &mut self,
        retained_bytes: u64,
        strings: impl IntoIterator<Item = &'value str>,
    ) -> Result<(), ExtractError> {
        if strings
            .into_iter()
            .any(|value| usize_to_u64(value.len()) > self.string_limit)
        {
            return Err(ExtractError::OutputLimit);
        }
        let facts = self.facts.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        let next = self
            .retained_bytes
            .checked_add(retained_bytes)
            .ok_or(ExtractError::OutputLimit)?;
        if facts > self.fact_limit || next > self.working_limit {
            return Err(ExtractError::OutputLimit);
        }
        self.facts = facts;
        self.retained_bytes = next;
        Ok(())
    }

    pub(crate) const fn output_limit(&self) -> u64 {
        self.retained_output_limit
    }

    pub(crate) fn ensure_string_length(&self, length: usize) -> Result<(), ExtractError> {
        let length = usize_to_u64(length);
        if length > self.string_limit || length > MAXIMUM_FACT_STRING_BYTES {
            Err(ExtractError::OutputLimit)
        } else {
            Ok(())
        }
    }

    /// Charge working memory a walker retains beyond the emitted facts.
    pub(crate) fn reserve_working_bytes(&mut self, bytes: u64) -> Result<(), ExtractError> {
        self.retained_bytes = self
            .retained_bytes
            .checked_add(bytes)
            .filter(|next| *next <= self.working_limit)
            .ok_or(ExtractError::OutputLimit)?;
        Ok(())
    }

    pub(crate) fn reserve_additional_string(&mut self, value: &str) -> Result<(), ExtractError> {
        self.ensure_string_length(value.len())?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(usize_to_u64(value.len()))
            .filter(|next| *next <= self.working_limit)
            .ok_or(ExtractError::OutputLimit)?;
        Ok(())
    }
}

pub(crate) fn ensure_fact_string_length(length: usize) -> Result<(), ExtractError> {
    if usize_to_u64(length) > MAXIMUM_FACT_STRING_BYTES {
        Err(ExtractError::OutputLimit)
    } else {
        Ok(())
    }
}

impl ExtractedFile {
    /// Modeled Rust-owned bytes retained by this output, including vector/string capacity.
    #[must_use]
    pub fn modeled_retained_bytes(&self) -> u64 {
        file_header_bytes(self)
            .and_then(|base| {
                base.checked_add(vector_bytes::<ExtractedSymbol>(self.symbols.capacity()))
            })
            .and_then(|bytes| {
                self.symbols.iter().try_fold(bytes, |total, symbol| {
                    total.checked_add(symbol_string_bytes(symbol))
                })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<Containment>(self.containments.capacity()))
            })
            .and_then(|bytes| {
                self.containments.iter().try_fold(bytes, |total, edge| {
                    total.checked_add(containment_string_bytes(edge))
                })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<ExtractedReference>(
                    self.references.capacity(),
                ))
            })
            .and_then(|bytes| {
                self.references.iter().try_fold(bytes, |total, reference| {
                    total.checked_add(reference_string_bytes(reference))
                })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<ExtractedCallScopeSite>(
                    self.call_scope_sites.capacity(),
                ))
            })
            .and_then(|bytes| {
                self.call_scope_sites.iter().try_fold(bytes, |total, site| {
                    total.checked_add(usize_to_u64(site.owner.as_str().len()))
                })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<JavascriptMemberCallContext>(
                    self.javascript_member_calls.capacity(),
                ))
            })
            .and_then(|bytes| {
                self.javascript_member_calls
                    .iter()
                    .try_fold(bytes, |total, call| {
                        total.checked_add(javascript_call_context_string_bytes(call))
                    })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<ExtractedNumericalSite>(
                    self.numerical_sites.capacity(),
                ))
            })
            .and_then(|bytes| {
                self.numerical_sites.iter().try_fold(bytes, |total, site| {
                    total.checked_add(numerical_site_string_bytes(site))
                })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<ExtractedImportBinding>(
                    self.import_bindings.capacity(),
                ))
            })
            .and_then(|bytes| {
                self.import_bindings
                    .iter()
                    .try_fold(bytes, |total, binding| {
                        total.checked_add(import_binding_string_bytes(binding))
                    })
            })
            .and_then(|bytes| {
                bytes.checked_add(vector_bytes::<ExtractionDiagnostic>(
                    self.diagnostics.capacity(),
                ))
            })
            .and_then(|bytes| bytes.checked_add(usize_to_u64(self.test_search_text.capacity())))
            .unwrap_or(u64::MAX)
    }
}

pub(crate) fn symbol_budget_bytes(symbol: &ExtractedSymbol) -> u64 {
    vector_growth_bytes::<ExtractedSymbol>().saturating_add(symbol_string_bytes(symbol))
}

pub(crate) fn containment_budget_bytes(edge: &Containment) -> u64 {
    vector_growth_bytes::<Containment>().saturating_add(containment_string_bytes(edge))
}

pub(crate) fn reference_budget_bytes(reference: &ExtractedReference) -> u64 {
    vector_growth_bytes::<ExtractedReference>().saturating_add(reference_string_bytes(reference))
}

pub(crate) fn call_scope_site_budget_bytes(site: &ExtractedCallScopeSite) -> u64 {
    vector_growth_bytes::<ExtractedCallScopeSite>()
        .saturating_add(usize_to_u64(site.owner.as_str().len()))
}

pub(crate) fn javascript_call_context_budget_bytes(call: &JavascriptMemberCallContext) -> u64 {
    vector_growth_bytes::<JavascriptMemberCallContext>()
        .saturating_add(javascript_call_context_string_bytes(call))
}

fn javascript_call_context_string_bytes(call: &JavascriptMemberCallContext) -> u64 {
    match &call.receiver {
        JavascriptMemberReceiver::Constructor(name) => usize_to_u64(name.capacity()),
        JavascriptMemberReceiver::Shadowed | JavascriptMemberReceiver::LocalImport => 0,
    }
}

pub(crate) fn numerical_site_budget_bytes(site: &ExtractedNumericalSite) -> u64 {
    vector_growth_bytes::<ExtractedNumericalSite>()
        .saturating_add(numerical_site_string_bytes(site))
}

pub(crate) fn import_binding_budget_bytes(binding: &ExtractedImportBinding) -> u64 {
    vector_growth_bytes::<ExtractedImportBinding>()
        .saturating_add(import_binding_string_bytes(binding))
}

pub(crate) fn diagnostic_budget_bytes() -> u64 {
    vector_growth_bytes::<ExtractionDiagnostic>()
}

fn file_header_bytes(file: &ExtractedFile) -> Option<u64> {
    usize_to_u64(size_of::<ExtractedFile>())
        .checked_add(usize_to_u64(file.file_id.as_str().len()))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(file.path.as_str().len())))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(file.content_hash.as_str().len())))
}

fn snapshot_header_bytes(snapshot: &SourceSnapshot) -> Option<u64> {
    usize_to_u64(size_of::<ExtractedFile>())
        .checked_add(usize_to_u64(snapshot.file_id().as_str().len()))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(snapshot.path().as_str().len())))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(snapshot.content_hash().as_str().len())))
}

fn symbol_string_bytes(symbol: &ExtractedSymbol) -> u64 {
    usize_to_u64(symbol.id.as_str().len())
        .checked_add(usize_to_u64(symbol.name.capacity()))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(symbol.qualified_name.capacity())))
        .and_then(|bytes| {
            bytes.checked_add(usize_to_u64(
                symbol.signature.as_ref().map_or(0, String::capacity),
            ))
        })
        .and_then(|bytes| {
            bytes.checked_add(usize_to_u64(
                symbol.docstring.as_ref().map_or(0, String::capacity),
            ))
        })
        .and_then(|bytes| bytes.checked_add(usize_to_u64(symbol.body_search_text.capacity())))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(symbol.structural_digest.as_str().len())))
        .and_then(|bytes| bytes.checked_add(usize_to_u64(symbol.clone_shape_digest.as_str().len())))
        .and_then(|bytes| {
            bytes.checked_add(usize_to_u64(
                symbol
                    .clone_token_profile
                    .as_ref()
                    .map_or(0, crate::CloneTokenProfile::retained_bytes),
            ))
        })
        .unwrap_or(u64::MAX)
}

fn containment_string_bytes(edge: &Containment) -> u64 {
    usize_to_u64(edge.parent.as_str().len()).saturating_add(usize_to_u64(edge.child.as_str().len()))
}

fn reference_string_bytes(reference: &ExtractedReference) -> u64 {
    let owner = reference
        .owner
        .as_ref()
        .map_or(0, |owner| owner.as_str().len());
    usize_to_u64(owner)
        .saturating_add(usize_to_u64(reference.name.capacity()))
        .saturating_add(usize_to_u64(
            reference
                .resolution_name
                .as_ref()
                .map_or(0, String::capacity),
        ))
}

fn numerical_site_string_bytes(site: &ExtractedNumericalSite) -> u64 {
    usize_to_u64(site.id.as_str().len())
        .saturating_add(
            site.owner
                .as_ref()
                .map_or(0, |owner| usize_to_u64(owner.as_str().len())),
        )
        .saturating_add(usize_to_u64(site.operation.capacity()))
        .saturating_add(usize_to_u64(site.hazard.capacity()))
        .saturating_add(usize_to_u64(site.precision.capacity()))
        .saturating_add(usize_to_u64(site.expression_digest.as_str().len()))
        .saturating_add(usize_to_u64(site.provenance.capacity()))
        .saturating_add(usize_to_u64(site.unknowns.capacity()))
}

fn import_binding_string_bytes(binding: &ExtractedImportBinding) -> u64 {
    usize_to_u64(binding.module_specifier.capacity())
        .saturating_add(usize_to_u64(binding.imported_name.capacity()))
        .saturating_add(usize_to_u64(binding.local_name.capacity()))
}

fn vector_growth_bytes<T>() -> u64 {
    usize_to_u64(size_of::<T>()).saturating_mul(VECTOR_GROWTH_ALLOWANCE)
}

fn vector_bytes<T>(capacity: usize) -> u64 {
    usize_to_u64(size_of::<T>()).saturating_mul(usize_to_u64(capacity))
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
