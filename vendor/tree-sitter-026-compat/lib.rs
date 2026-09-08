//! Runtime-neutral bridge for the published ABAP grammar's 0.26 dependency.
//!
//! These are the exact 0.27 types and constants. This facade contains no parser,
//! native build, FFI, conversions, or second runtime. Its intentionally limited
//! surface serves only `tree-sitter-abap-sqry` and `sqry-tree-sitter-support`.

pub use tree_sitter_runtime::{LANGUAGE_VERSION, Language, MIN_COMPATIBLE_LANGUAGE_VERSION};
