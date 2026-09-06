//! Shared support for the Fabro black box battery: provider twins on
//! loopback, the shipped binary launched in an isolated environment, and
//! interview scripts.
//!
//! Task 3's parallel-regression harness adds sibling modules here; keep each
//! module's names distinct so the two merge cleanly.

#![allow(dead_code, reason = "each test file uses the subset it needs")]

pub(crate) mod interview;
pub(crate) mod launch;
pub(crate) mod twins;
