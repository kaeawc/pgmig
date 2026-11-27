// One integration-test binary for the crate: one module per area.
mod apply;
mod apply_report;
mod clean;
mod concurrent_indexes;
mod config;
mod drift;
#[cfg(unix)]
mod edit;
mod filters;
mod json;
mod migrate;
mod policy;
mod support;
