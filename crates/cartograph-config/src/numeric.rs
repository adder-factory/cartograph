use serde_json::{Map, Value};

/// Inclusive upper bound for a positive configuration integer, with the exact
/// rejection its section reports.
#[derive(Clone, Copy)]
pub struct BoundedU64Field<E> {
    /// Inclusive maximum admitted value.
    pub maximum: u64,
    /// Error returned for a non-integer, zero, or value above the bound.
    pub invalid: E,
}

/// Read an optional positive JSON integer without changing section-specific errors.
/// # Errors
/// Returns the supplied error when a present value violates the inclusive bound.
pub fn optional_bounded_u64<E>(
    object: &Map<String, Value>,
    key: &str,
    field: BoundedU64Field<E>,
) -> Result<Option<u64>, E> {
    let BoundedU64Field { maximum, invalid } = field;
    object
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .filter(|value| (1..=maximum).contains(value))
                .ok_or(invalid)
        })
        .transpose()
}
