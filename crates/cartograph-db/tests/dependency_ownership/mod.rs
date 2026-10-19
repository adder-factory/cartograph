//! Explicit package-dependency ownership for independently compiled integration targets.
//!
//! Each live test uses a narrow public API slice, while Cargo exposes the package-wide dependency
//! set to every test crate. These underscore imports make that target-granularity ownership explicit.

use blake3 as _;
use cartograph_domain as _;
use futures_util as _;
use getrandom as _;
use itoa as _;
use jiff as _;
#[cfg(unix)]
use libc as _;
use num_traits as _;
use regex as _;
use rocksdb as _;
#[cfg(unix)]
use rustix as _;
use serde as _;
use serde_json as _;
use tempfile as _;
use thiserror as _;
use tokio as _;
