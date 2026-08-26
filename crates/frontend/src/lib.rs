//! Machinery every frontend shares.
//!
//! A frontend is pure: file text in, a `Graph` and diagnostics out. This crate holds
//! what that takes — [`diag`] for saying what went wrong and where, [`yaml`] for
//! reading files with positions, [`expr`] for the `${{ }}` grammar and its two
//! lowerings, and [`print`] for a stable text form of a lowered graph.

pub mod diag;
pub mod expr;
pub mod print;
pub mod yaml;

pub use diag::{Diagnostic, Diagnostics, Lowered, Severity, Span};
pub use print::print_graph;
