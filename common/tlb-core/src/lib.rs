//! Shared building blocks for the table load balancing PoC.
//!
//! The SQL issued from here is deliberately plain, standard SQL (MERGE,
//! window functions, FETCH FIRST, PERCENTILE_DISC) so that the strategies
//! can be ported to other SQL dialects. Engine-specific statements are
//! limited to the instrumentation in `common/tools`.

pub mod apply;
pub mod batch_log;
pub mod config;
pub mod db;
pub mod lag;
pub mod shutdown;
