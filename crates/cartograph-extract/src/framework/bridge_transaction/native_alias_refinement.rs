//! A native alias may refine a dynamic leaf while preserving its fallback mode.

use crate::{DYNAMIC_DISPATCH_RESOLUTION_PREFIX, NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX};

pub(super) fn prepare(current: Option<&str>, next: String) -> Option<String> {
    match current {
        None => Some(next),
        Some(current)
            if current.starts_with(DYNAMIC_DISPATCH_RESOLUTION_PREFIX)
                && next.starts_with(NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX) =>
        {
            Some(format!("{DYNAMIC_DISPATCH_RESOLUTION_PREFIX}{next}"))
        }
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use cartograph_domain::{ReferenceKind, SourceLanguage};

    use crate::{
        DYNAMIC_DISPATCH_RESOLUTION_PREFIX, NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX, NativeExtractor,
        SourceLimits, SourceSnapshot,
        framework::{FrameworkBuilder, FrameworkInput, FrameworkNearReferenceInput},
    };

    #[test]
    fn rollback_restores_the_original_dynamic_reference() -> Result<(), Box<dyn std::error::Error>>
    {
        let source = "function ordinary(m) { m.run(); }";
        let snapshot = SourceSnapshot::from_bytes(
            "src/undo.js",
            source.as_bytes(),
            SourceLimits::new(source.len())?,
        )?;
        let file = NativeExtractor::new(SourceLanguage::JavaScript)?.extract(&snapshot)?;
        let original = serde_json::to_value(&file.references)?;
        let dynamic = format!("{DYNAMIC_DISPATCH_RESOLUTION_PREFIX}run");
        assert!(
            file.references
                .iter()
                .any(|reference| reference.name == "run"
                    && reference.resolution_name.as_deref() == Some(&dynamic))
        );
        let mut cancelled = || false;
        let mut builder =
            FrameworkBuilder::new(FrameworkInput::new(&snapshot, file), &mut cancelled)?;
        let checkpoint = builder.bridge.checkpoint();
        let start = source.find("run").ok_or("missing native call")?;
        let hint = format!("{NATIVE_MODULE_ALIAS_RESOLUTION_PREFIX}Camera::run");
        builder.add_reference_near_with_resolution(FrameworkNearReferenceInput {
            name: "run",
            resolution_name: Some(&hint),
            kind: ReferenceKind::Calls,
            start,
            end: start + "run".len(),
        })?;
        let refined = format!("{DYNAMIC_DISPATCH_RESOLUTION_PREFIX}{hint}");
        assert!(
            builder
                .references()
                .iter()
                .any(|reference| reference.resolution_name.as_deref() == Some(&refined))
        );
        builder.bridge.omit_facts(checkpoint);
        assert_eq!(serde_json::to_value(builder.references())?, original);
        Ok(())
    }
}
