//! Engines: turn a query into ranked, deduplicated results by fanning out to
//! several independent engines in parallel.
//!
//! Every engine is an adapter resolved from settings (`engines.use` and
//! `engines.config`); adapters read credentials from the environment at query
//! time. Each engine's `EngineState` travels beside the results, so an empty
//! answer is never mistaken for a broken one.

mod adapter;
mod cache;
mod pool;
mod scratch;

pub(crate) use pool::fuse_answers;
pub use pool::Pool;

/// Apply the command-engine filesystem contract to optional edge processors.
pub(crate) fn prepare_command_scratch(path: &std::path::Path) -> Result<(), String> {
    scratch::prepare(path)
}

use crate::core::Link;

/// How one engine fared, so an agent can tell a genuinely empty result set
/// from an engine that failed or was skipped.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineState {
    pub name: String,
    pub status: EngineStatus,
    pub count: usize,
    /// Upstream rows dropped as invalid (empty title, unusable URL, bad snippet).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped: usize,
    /// Requested filters this engine cannot honor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported_filters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// Serialize `skipped` only when an engine actually dropped rows.
fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// The outcome of one engine's query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    Error,
    Timeout,
}

/// One engine's answer: valid links in upstream rank order, and how many
/// upstream rows were skipped as invalid before the limit was reached. Commands
/// can also report unsupported constraints; only requested field names reach callers.
#[derive(Debug, Default)]
pub struct Found {
    pub links: Vec<Link>,
    pub skipped: usize,
    /// Optional command v2 capability report; the pool retains only requested names.
    pub unsupported_filters: Vec<String>,
}

/// A link paired with its engine rank for reciprocal-rank fusion.
/// The rank is internal to the merge and does not reach the caller.
#[derive(Debug, Clone)]
pub(crate) struct Ranked {
    pub link: Link,
    pub rank: usize,
}

/// A boxed future, so the `Engine` trait stays object-safe without an
/// async-trait macro: the pool holds `Arc<dyn Engine>` and fans out. The
/// future is `'static` because it is spawned onto the runtime.
pub type EngineFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Found, EngineError>> + Send>>;

/// Discover pages in upstream rank order. The pool drops each future at its
/// deadline; implementations must not block the runtime and must release work on
/// cancellation. An owned, `'static` future lets the pool spawn each query.
pub trait Engine: Send + Sync + 'static {
    /// The engine's stable identifier, used in configuration and output.
    fn name(&self) -> &str;

    /// Run one query. Results are in the engine's own ranking order.
    fn search(&self, query: String, limit: usize) -> EngineFuture;

    /// Filter-aware entry point; legacy implementations report all constraints unsupported.
    fn search_query(&self, query: crate::core::Query, limit: usize) -> EngineFuture {
        self.search(query.text, limit)
    }

    /// Constraints not implemented by this transport, including on failure.
    fn unsupported_filters(&self, filters: &crate::core::service::QueryFilters) -> Vec<String> {
        filters.values().into_keys().collect()
    }
}

/// An engine failure, classified so the pool can report it without
/// leaking a response body into the engine state.
#[derive(Debug, Clone)]
pub struct EngineError {
    pub message: String,
    pub cause: FailureCause,
}

/// Why an engine query failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCause {
    /// The request never left, or the answer could not be read.
    Network,
    /// The engine answered with a status that is not success.
    Rejected,
    /// The engine's answer could not be parsed.
    Malformed,
    /// The deadline passed.
    Timeout,
}

impl EngineError {
    pub fn network(message: impl Into<String>) -> Self {
        EngineError {
            message: message.into(),
            cause: FailureCause::Network,
        }
    }
    pub fn rejected(message: impl Into<String>) -> Self {
        EngineError {
            message: message.into(),
            cause: FailureCause::Rejected,
        }
    }
    pub fn malformed(message: impl Into<String>) -> Self {
        EngineError {
            message: message.into(),
            cause: FailureCause::Malformed,
        }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EngineError {}
