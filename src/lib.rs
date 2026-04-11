//! `bundle` library — exposes the full internal module tree so that
//! integration tests in `tests/` can call public APIs directly.
pub mod apply;
pub mod bundle;
pub mod bundlefile;
pub mod cmd;
pub mod project;
pub mod registry;
pub mod util;
