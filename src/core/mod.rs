//! Protocol-free live search, guarded fetching, configuration and passage selection.
//! Frontends compose this service without introducing protocol dependencies here.

pub mod config;
pub mod crawl;
pub mod engines;
pub mod fetch;
pub mod research;
mod service;
pub mod text;

pub use config::Config;
pub use crawl::{CrawlFailure, CrawlOptions, CrawlReport, CrawlRequest};
pub use fetch::Page;
pub use research::{Coverage, Evidence, Packet, ResearchOptions};
pub use service::{Answer, Link, Query, QueryFilters, Search, SearchError};
