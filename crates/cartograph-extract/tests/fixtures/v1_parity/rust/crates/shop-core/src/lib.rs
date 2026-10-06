//! Core domain types.
pub mod models;
pub mod store;

pub use models::{Order, Status};
pub use store::MemoryStore as DefaultStore;

pub const MAX_ITEMS: usize = 64;
pub static GLOBAL_FLAG: bool = false;

pub type OrderId = u64;

macro_rules! ensure {
    ($cond:expr) => {
        if !$cond {
            return None;
        }
    };
}

pub fn limit() -> usize {
    MAX_ITEMS
}

pub fn checked(n: usize) -> Option<usize> {
    ensure!(n < MAX_ITEMS);
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_positive() {
        assert!(limit() > 0);
    }
}
