//! Optional syntax evidence fenced to the exact extracted file bytes.

use super::{
    NativeFileFacts, NormalizedPath, ResolutionIndexContext, SourceReadError, SourceReadOptions,
    SourceSnapshot, StageItemFailure, exact_limit_ceiling, native_read_reservation,
};

pub(super) fn read<Cancel>(
    file: &NativeFileFacts,
    context: &mut ResolutionIndexContext<'_, Cancel>,
) -> Result<Option<SourceSnapshot>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let path = NormalizedPath::parse(&file.file.normalized_path).map_err(|_| StageItemFailure)?;
    let limits = exact_limit_ceiling(file.file.byte_size)?;
    context
        .budget
        .charge(native_read_reservation(file.file.byte_size).ok_or(StageItemFailure)?)?;
    let snapshot = match context.source_root.read_with_cancellation(
        &path,
        SourceReadOptions::new(limits, &mut *context.cancelled),
    ) {
        Ok(snapshot) => snapshot,
        // In-memory callers may have no corresponding source at this root.
        // Missing or oversized evidence admits no extra resolution; revalidation
        // independently fails if an indexed source disappears.
        Err(SourceReadError::FileUnavailable | SourceReadError::SourceTooLarge)
            if !(context.cancelled)() =>
        {
            return Ok(None);
        }
        Err(_) => return Err(StageItemFailure),
    };
    if snapshot.content_hash() != &file.file.content_hash
        || snapshot.byte_size() != file.file.byte_size
        || snapshot.language().as_str() != file.file.language
    {
        // This snapshot cannot substantiate syntax from the extracted facts.
        return Ok(None);
    }
    Ok(Some(snapshot))
}
