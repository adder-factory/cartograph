//! The file's import bindings summarised by local name.
//!
//! Constant reads, cgo calls, and typing-form annotations ask how a name is
//! imported once per occurrence. Scanning every binding per occurrence would
//! make the work grow with imports times occurrences, so the bindings are
//! summarised by local name, incrementally as the walk records them.
//!
//! A summary says how *any* import of the file binds a name, not which import
//! binds it at one use: Python imports can be function-local or conditional,
//! and the walk does not model those scopes. Callers read a summary
//! conservatively. A name that any `typing` import binds as `Literal` is read
//! as `Literal`, so its value arguments never become type uses, even when
//! another import elsewhere in the file rebinds the name.

use std::collections::HashMap;

use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, walk::ExtractionBuilder};

/// The cgo pseudo-package, imported as `import "C"`.
pub(super) const CGO_PACKAGE: &str = "C";
/// Modules that define the special typing forms.
const PYTHON_TYPING_MODULES: [&str; 2] = ["typing", "typing_extensions"];

/// A Python `typing` special form whose arguments are not all types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TypingForm {
    /// `Literal[..]`: every argument is a value.
    Literal,
    /// `Annotated[T, ..]`: the arguments after `T` are metadata values.
    Annotated,
}

impl TypingForm {
    /// The special form a `typing` member name denotes, if any.
    pub(super) fn named(member: &str) -> Option<Self> {
        match member {
            "Literal" => Some(Self::Literal),
            "Annotated" => Some(Self::Annotated),
            _ => None,
        }
    }

    /// How many leading arguments of the form are types.
    pub(super) const fn type_arguments(self) -> usize {
        match self {
            Self::Literal => 0,
            Self::Annotated => 1,
        }
    }

    /// The form that reads fewer arguments as types, for a name two imports
    /// bind as different forms.
    const fn narrower(self, other: Self) -> Self {
        match (self, other) {
            (Self::Annotated, Self::Annotated) => Self::Annotated,
            _ => Self::Literal,
        }
    }
}

/// How the file's imports bind one local name.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LocalImports {
    /// An import of the cgo pseudo-package binds the name.
    pub(super) cgo: bool,
    /// A namespace import of a `typing` module binds the name (`import typing as t`).
    pub(super) typing_namespace: bool,
    /// The narrowest special form a named `typing` import binds the name to.
    pub(super) typing_form: Option<TypingForm>,
}

impl LocalImports {
    fn record(&mut self, binding: &ExtractedImportBinding) {
        let module = binding.module_specifier.as_str();
        self.cgo |= module == CGO_PACKAGE;
        if !PYTHON_TYPING_MODULES.contains(&module) {
            return;
        }
        match binding.kind {
            ImportBindingKind::Namespace => self.typing_namespace = true,
            ImportBindingKind::Named => {
                if let Some(form) = TypingForm::named(&binding.imported_name) {
                    self.typing_form = Some(
                        self.typing_form
                            .map_or(form, |recorded| recorded.narrower(form)),
                    );
                }
            }
            _ => {}
        }
    }
}

/// The summary of each local name among the first `indexed` bindings.
#[derive(Default)]
pub(super) struct ImportIndex {
    indexed: usize,
    by_local_name: HashMap<String, LocalImports>,
}

impl ImportIndex {
    /// Summarise the bindings recorded since the last refresh.
    fn refresh(&mut self, bindings: &[ExtractedImportBinding]) -> Result<(), ExtractError> {
        for binding in bindings.iter().skip(self.indexed) {
            if let Some(summary) = self.by_local_name.get_mut(binding.local_name.as_str()) {
                summary.record(binding);
                continue;
            }
            let mut name = String::new();
            name.try_reserve_exact(binding.local_name.len())
                .map_err(|_| ExtractError::OutputLimit)?;
            name.push_str(&binding.local_name);
            let mut summary = LocalImports::default();
            summary.record(binding);
            self.by_local_name
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            self.by_local_name.insert(name, summary);
        }
        self.indexed = bindings.len();
        Ok(())
    }
}

/// How the imports the walk has recorded so far bind `local_name`, or `None`
/// when no import binds it.
pub(super) fn local_imports(
    builder: &mut ExtractionBuilder<'_, '_>,
    local_name: &str,
) -> Result<Option<LocalImports>, ExtractError> {
    let index = &mut builder.polyglot.imports;
    index.refresh(&builder.facts.import_bindings)?;
    Ok(index.by_local_name.get(local_name).copied())
}
