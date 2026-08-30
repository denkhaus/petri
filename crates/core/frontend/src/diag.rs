//! Diagnostics: the one way every frontend reports a problem.
//!
//! Rejection is loud and specific. A construct the engine cannot express is an
//! `unsupported.*` Error with a hint naming the alternative or the package that
//! will add it — never parsed and ignored, never silently approximated. A file
//! with any Error produces no graph.

use std::fmt;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Warning,
    Error,
}

/// A position in a source file. Lines and columns are 1-based; `0` means
/// unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub file:   SmolStr,
    pub line:   u32,
    pub column: u32,
}

impl Span {
    pub fn new(file: &str, line: u32, column: u32) -> Self {
        Self {
            file: SmolStr::new(file),
            line,
            column,
        }
    }

    /// A span that names the file but no position, for whole-file problems.
    pub fn file(file: &str) -> Self {
        Self::new(file, 0, 0)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.file)
        } else {
            write!(f, "{}:{}:{}", self.file, self.line, self.column)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Stable and greppable: `unsupported.concurrency`, `expr.parse`,
    /// `yaml.syntax`.
    pub code:     SmolStr,
    pub message:  String,
    /// What to do instead.
    pub hint:     Option<String>,
    pub span:     Span,
}

impl Diagnostic {
    pub fn error(code: &str, span: Span, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code: SmolStr::new(code),
            message: message.into(),
            hint: None,
            span,
        }
    }

    pub fn warning(code: &str, span: Span, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code: SmolStr::new(code),
            message: message.into(),
            hint: None,
            span,
        }
    }

    /// An `unsupported.<feature>` error: the construct is real, and the engine
    /// cannot run it yet. `hint` says where it is headed.
    pub fn unsupported(feature: &str, span: Span, message: impl Into<String>, hint: &str) -> Self {
        Self::error(&format!("unsupported.{feature}"), span, message).with_hint(hint)
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Whether this is an `unsupported.*` rejection, and of what.
    pub fn unsupported_feature(&self) -> Option<&str> {
        self.code.strip_prefix("unsupported.")
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(
            f,
            "{}: {severity}[{}]: {}",
            self.span, self.code, self.message
        )?;
        if let Some(hint) = &self.hint {
            write!(f, "\n    hint: {hint}")?;
        }
        Ok(())
    }
}

/// Collects diagnostics while a frontend works. Frontends never bail on the
/// first problem: one pass reports everything it can.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn error(&mut self, code: &str, span: Span, message: impl Into<String>) {
        self.push(Diagnostic::error(code, span, message));
    }

    pub fn warning(&mut self, code: &str, span: Span, message: impl Into<String>) {
        self.push(Diagnostic::warning(code, span, message));
    }

    pub fn unsupported(
        &mut self,
        feature: &str,
        span: Span,
        message: impl Into<String>,
        hint: &str,
    ) {
        self.push(Diagnostic::unsupported(feature, span, message, hint));
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| d.is_error())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| !d.is_error())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }

    pub fn extend(&mut self, other: Diagnostics) {
        self.items.extend(other.items);
    }
}

/// What a frontend hands back: a graph when there were no errors, and
/// everything it had to say either way.
#[derive(Debug)]
pub struct Lowered {
    pub graph:       Option<ir::Graph>,
    pub diagnostics: Diagnostics,
}

impl Lowered {
    pub fn rejected(diagnostics: Diagnostics) -> Self {
        Self {
            graph: None,
            diagnostics,
        }
    }

    /// A graph is only handed out when nothing was an Error.
    pub fn from_parts(graph: ir::Graph, diagnostics: Diagnostics) -> Self {
        Self {
            graph: (!diagnostics.has_errors()).then_some(graph),
            diagnostics,
        }
    }
}
