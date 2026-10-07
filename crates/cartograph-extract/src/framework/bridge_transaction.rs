//! Output admission and rollback for the optional bridge extraction pass.

use super::owner_index::{OWNER_INDEX_ENTRY_BYTES, SourceOwnerIndex};
use crate::{
    ExtractError, ExtractedFile, SourceSnapshot,
    budget::{
        ExtractionBudget, containment_budget_bytes, diagnostic_budget_bytes,
        import_binding_budget_bytes, reference_budget_bytes, symbol_budget_bytes,
    },
};

const MAX_BRIDGE_SOURCE_PASSES: usize = 32;
const REFERENCE_INDEX_ENTRY_BYTES: u64 = 16;
const REFERENCE_REFINEMENT_ENTRY_BYTES: u64 = 64;

pub(crate) type BridgeCheckpoint = (usize, usize, usize, usize);

/// Owns the admitted output and the undo state needed to omit optional bridge facts.
pub(crate) struct BridgeTransaction<'cancel> {
    pub(super) file: ExtractedFile,
    pub(super) budget: ExtractionBudget,
    pub(super) cancelled: &'cancel mut dyn FnMut() -> bool,
    pub(super) original_symbols: usize,
    pub(super) original_references: usize,
    pub(super) original_ownership: Option<SourceOwnerIndex>,
    work: usize,
    reference_refinements: Vec<(usize, Option<String>)>,
}

impl<'cancel> BridgeTransaction<'cancel> {
    pub(super) fn new(
        snapshot: &SourceSnapshot,
        file: ExtractedFile,
        cancelled: &'cancel mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        let mut budget = ExtractionBudget::new(snapshot)?;
        reserve_initial_facts(&mut budget, &file)?;
        let original_symbols = file.symbols.len();
        let original_references = file.references.len();
        let work = snapshot
            .source()
            .len()
            .saturating_mul(MAX_BRIDGE_SOURCE_PASSES)
            .saturating_add(original_symbols)
            .saturating_add(file.references.len());
        Ok(Self {
            file,
            budget,
            cancelled,
            original_symbols,
            original_references,
            original_ownership: None,
            work,
            reference_refinements: Vec::new(),
        })
    }

    pub(crate) fn check_cancelled(&mut self) -> Result<(), ExtractError> {
        if (self.cancelled)() {
            Err(ExtractError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn reserve_working_bytes(&mut self, bytes: u64) -> Result<(), ExtractError> {
        self.budget.reserve_working_bytes(bytes)
    }

    pub(crate) fn index_owners(&mut self) -> Result<(), ExtractError> {
        if self.original_ownership.is_some() {
            return Ok(());
        }
        let count = self.original_symbols;
        let work = count.saturating_mul(
            usize::try_from(count.max(1).ilog2())
                .unwrap_or(usize::MAX)
                .saturating_add(1),
        );
        self.charge_work(work.saturating_add(self.file.references.len()))?;
        let bytes = count.saturating_mul(OWNER_INDEX_ENTRY_BYTES);
        self.reserve_working_bytes(u64::try_from(bytes).map_err(|_| ExtractError::OutputLimit)?)?;
        self.reserve_working_bytes(REFERENCE_INDEX_ENTRY_BYTES.saturating_mul(
            u64::try_from(self.file.references.len()).map_err(|_| ExtractError::OutputLimit)?,
        ))?;
        self.original_ownership = Some(SourceOwnerIndex::build(
            &self.file.symbols[..count],
            self.cancelled,
        )?);
        Ok(())
    }

    pub(crate) fn charge_work(&mut self, units: usize) -> Result<(), ExtractError> {
        self.check_cancelled()?;
        self.work = self
            .work
            .checked_sub(units)
            .ok_or(ExtractError::OutputLimit)?;
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> BridgeCheckpoint {
        (
            self.file.symbols.len(),
            self.file.containments.len(),
            self.file.references.len(),
            self.reference_refinements.len(),
        )
    }

    pub(crate) fn retained_output_fits(&self) -> bool {
        self.file.modeled_retained_bytes() <= self.budget.output_limit()
    }

    pub(crate) fn omit_facts(&mut self, checkpoint: BridgeCheckpoint) {
        for (index, previous) in self.reference_refinements.drain(checkpoint.3..).rev() {
            self.file.references[index].resolution_name = previous;
        }
        self.file.symbols.truncate(checkpoint.0);
        self.file.containments.truncate(checkpoint.1);
        self.file.references.truncate(checkpoint.2);
        self.file.symbols.shrink_to_fit();
        self.file.containments.shrink_to_fit();
        self.file.references.shrink_to_fit();
        crate::walk::note_optional_omission(&mut self.file, self.budget.output_limit());
    }

    pub(super) fn refine_reference(
        &mut self,
        index: usize,
        resolution_name: String,
    ) -> Result<(), ExtractError> {
        if self.file.references[index].resolution_name.is_some() {
            return Ok(());
        }
        self.budget.reserve_additional_string(&resolution_name)?;
        self.reserve_working_bytes(REFERENCE_REFINEMENT_ENTRY_BYTES)?;
        self.reference_refinements
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        let previous = self.file.references[index]
            .resolution_name
            .replace(resolution_name);
        self.reference_refinements.push((index, previous));
        Ok(())
    }
}

#[cfg(test)]
mod work_tests;

fn reserve_initial_facts(
    budget: &mut ExtractionBudget,
    file: &ExtractedFile,
) -> Result<(), ExtractError> {
    for symbol in &file.symbols {
        budget.reserve_fact(
            symbol_budget_bytes(symbol),
            [
                symbol.name.as_str(),
                symbol.qualified_name.as_str(),
                symbol.signature.as_deref().unwrap_or(""),
                symbol.docstring.as_deref().unwrap_or(""),
                symbol.body_search_text.as_str(),
            ],
        )?;
    }
    for containment in &file.containments {
        budget.reserve_fact(containment_budget_bytes(containment), std::iter::empty())?;
    }
    for reference in &file.references {
        budget.reserve_fact(
            reference_budget_bytes(reference),
            [
                reference.name.as_str(),
                reference.resolution_name.as_deref().unwrap_or(""),
            ],
        )?;
    }
    for binding in &file.import_bindings {
        budget.reserve_fact(
            import_binding_budget_bytes(binding),
            [
                binding.module_specifier.as_str(),
                binding.imported_name.as_str(),
                binding.local_name.as_str(),
            ],
        )?;
    }
    for _ in &file.diagnostics {
        budget.reserve_fact(diagnostic_budget_bytes(), std::iter::empty())?;
    }
    Ok(())
}
