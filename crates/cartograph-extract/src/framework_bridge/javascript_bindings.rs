//! Trust native helpers only through their original value import and unique binding.

use super::{BTreeMap, ExtractError, FrameworkBuilder};
use crate::{ExtractedImportBinding, ImportBindingKind};

const ENTRY_BYTES: u64 = 256;
const MAX_IMPORT_ANCESTORS: usize = 16;
const MAX_IMPORT_CHILDREN: u32 = 64;
const CODEGEN_PATHS: &[&str] = &[
    "react-native/Libraries/Utilities/codegenNativeComponent",
    "react-native/Libraries/Utilities/codegenNativeComponent.js",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeImport {
    Emitter,
    DeviceEmitter,
    Codegen,
}

pub(super) type Imports = BTreeMap<String, NativeImport>;

pub(super) fn collect(
    builder: &mut FrameworkBuilder<'_, '_>,
    unique: &BTreeMap<String, usize>,
) -> Result<Imports, ExtractError> {
    let mut imports = Imports::new();
    for index in 0..builder.import_bindings().len() {
        builder.bridge.charge_work(1)?;
        let binding = &builder.import_bindings()[index];
        let Some(kind) = native_import(binding) else {
            continue;
        };
        if unique.get(&binding.local_name) != Some(&1) {
            continue;
        }
        let span = binding.span;
        let length = binding.local_name.len();
        if !value_import(builder, span)? {
            continue;
        }
        builder.bridge.reserve_working_bytes(
            ENTRY_BYTES + u64::try_from(length).map_err(|_| ExtractError::OutputLimit)?,
        )?;
        imports.insert(builder.import_bindings()[index].local_name.clone(), kind);
    }
    Ok(imports)
}

fn native_import(binding: &ExtractedImportBinding) -> Option<NativeImport> {
    if binding.kind == ImportBindingKind::Named && binding.module_specifier == "react-native" {
        return match binding.imported_name.as_str() {
            "NativeEventEmitter" => Some(NativeImport::Emitter),
            "DeviceEventEmitter" => Some(NativeImport::DeviceEmitter),
            "codegenNativeComponent" => Some(NativeImport::Codegen),
            _ => None,
        };
    }
    (binding.kind == ImportBindingKind::Default
        && CODEGEN_PATHS.contains(&binding.module_specifier.as_str()))
    .then_some(NativeImport::Codegen)
}

fn value_import(
    builder: &mut FrameworkBuilder<'_, '_>,
    span: cartograph_domain::SourceSpan,
) -> Result<bool, ExtractError> {
    let mut node = builder.syntax_root().and_then(|root| {
        root.named_descendant_for_byte_range(
            usize::try_from(span.start_byte()).ok()?,
            usize::try_from(span.end_byte()).ok()?,
        )
    });
    for _ in 0..MAX_IMPORT_ANCESTORS {
        builder.bridge.charge_work(1)?;
        let Some(current) = node else {
            return Ok(false);
        };
        if matches!(current.kind(), "import_specifier" | "import_statement")
            && !value_node(builder, current)?
        {
            return Ok(false);
        }
        if current.kind() == "import_statement" {
            return Ok(current
                .parent()
                .is_some_and(|parent| parent.kind() == "program"));
        }
        node = current.parent();
    }
    Ok(false)
}

fn value_node(
    builder: &mut FrameworkBuilder<'_, '_>,
    node: tree_sitter::Node<'_>,
) -> Result<bool, ExtractError> {
    if node.child_count() > MAX_IMPORT_CHILDREN {
        return Ok(false);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        builder.bridge.charge_work(1)?;
        if child.kind() == "type" {
            return Ok(false);
        }
    }
    Ok(true)
}
