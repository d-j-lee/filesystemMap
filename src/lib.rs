//! fsmap — map a filesystem or codebase: sizes, kinds, code, git history, imports, and
//! the findings worth acting on.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod brief;
pub mod classify;
pub mod content;
pub mod deps;
pub mod diff;
pub mod dupes;
pub mod git;
pub mod insights;
pub mod model;
pub mod prune;
pub mod report;
pub mod scan;
pub mod snapshot;
pub mod strip;
pub mod util;
