//! Explicit package-dependency ownership for independently compiled CLI integration targets.
//!
//! Cargo exposes the binary package's complete dependency set to each integration-test crate.
//! Underscore imports record intentional target ownership without weakening the compiler lint.

use blake3 as _;
use cartograph_agent as _;
use cartograph_config as _;
use cartograph_db as _;
use cartograph_domain as _;
use cartograph_llm as _;
use cartograph_mcp as _;
use cartograph_search as _;
use cartograph_test_support as _;
use clap as _;
use futures_util as _;
use notify as _;
use num_traits as _;
use regex as _;
use reqwest as _;
use serde as _;
use serde_json as _;
use sha2 as _;
#[cfg(unix)]
use signal_hook as _;
use sqlx_core as _;
use tempfile as _;
use thiserror as _;
use tokio as _;
use toml_edit as _;
use url as _;
