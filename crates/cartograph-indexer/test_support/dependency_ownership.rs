//! Explicit package-dependency ownership for independently compiled tests and benchmarks.
//!
//! Cargo exposes the package-wide dependency set to every auxiliary target. Underscore imports
//! make that target-granularity ownership visible to the strict compiler dependency audit.

use blake3 as _;
use cartograph_config as _;
use cartograph_db as _;
use cartograph_domain as _;
use cartograph_extract as _;
use cartograph_scip as _;
use globset as _;
use serde as _;
use serde_json as _;
use tempfile as _;
use thiserror as _;
use tokio as _;
