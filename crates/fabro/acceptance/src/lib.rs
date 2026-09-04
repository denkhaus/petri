//! The Fabro compatibility corpus harness.
//!
//! Lowers every `.fabro` workflow and every Attractor `.dot` fixture in the
//! fetched Fabro repository and classifies the result. The bar is the GitHub
//! frontend's: every file either lowers, or is rejected with a specific
//! `unsupported.*` code — zero panics, zero generic errors. The Attractor
//! fixtures are expected rejections: they use the older dialect, and their
//! presence keeps the compatibility boundary tested.
//!
//! The corpus is fetched, not committed (`scripts/corpus-fetch-fabro.sh`), so
//! the corpus tests skip themselves when it is absent — see [`has_corpus`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use frontend::{CompileInputs, Diagnostic, DirFiles, Frontend as _, Severity};
use frontend_fabro::Fabro;

pub mod runs;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Lowered with no diagnostics at all.
    Clean,
    /// Lowered; warnings only.
    Warnings,
    /// Rejected, and every error is an `unsupported.*` code.
    Unsupported,
    /// Rejected with at least one error that is not `unsupported.*`.
    OtherError,
    /// The frontend panicked. Never acceptable.
    Panicked,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    /// The path under the Fabro repository.
    pub file:        String,
    pub class:       Class,
    pub diagnostics: Vec<Diagnostic>,
    pub nodes:       usize,
    pub panic:       Option<String>,
}

/// A lowered root graph and every nested workflow it may invoke.
pub struct Artifact {
    pub graph:    ir::Graph,
    pub children: Vec<ir::Graph>,
}

impl Outcome {
    pub fn unsupported_features(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .diagnostics
            .iter()
            .filter_map(|d| d.unsupported_feature().map(str::to_string))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn other_errors(&self) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error && d.unsupported_feature().is_none())
            .collect()
    }

    /// An Attractor-dialect fixture under `test/attractor/`.
    pub fn is_attractor(&self) -> bool {
        self.file.starts_with("test/attractor/")
    }
}

/// The corpus root: `crates/fabro/corpus/fabro`, the Fabro checkout.
pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus/fabro")
}

/// Is the corpus fetched? `PETRI_REQUIRE_FABRO_CORPUS` turns a skip into a
/// failure, so CI cannot pass on a silently absent corpus.
pub fn has_corpus(root: &Path) -> bool {
    root.join(".fabro/workflows").is_dir()
}

/// Every workflow the corpus holds: Fabro's own `.fabro/workflows/**/*.fabro`,
/// the demo workflows under `docs/`, and the Attractor `test/attractor/*.dot`
/// fixtures. Repository-relative, sorted.
pub fn workflows(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &root.join(".fabro"), "fabro", &mut out);
    walk(root, &root.join("docs"), "fabro", &mut out);
    walk(root, &root.join("test/attractor"), "dot", &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(root: &Path, dir: &Path, extension: &str, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "node_modules") {
                continue;
            }
            walk(root, &path, extension, out);
        } else if path.extension().is_some_and(|e| e == extension)
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Lower one workflow, catching panics so a crash is a classified result.
pub fn check_one(root: &Path, file: &str) -> Outcome {
    lower_one(root, file).0
}

/// [`check_one`], keeping the graph when there is one.
pub fn lower_one(root: &Path, file: &str) -> (Outcome, Option<Artifact>) {
    let path = root.join(file);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            return (
                Outcome {
                    file:        file.to_string(),
                    class:       Class::OtherError,
                    diagnostics: vec![Diagnostic::error(
                        "io",
                        frontend::Span::file(file),
                        e.to_string(),
                    )],
                    nodes:       0,
                    panic:       None,
                },
                None,
            );
        }
    };
    let files = DirFiles {
        root: root.to_path_buf(),
    };
    let inputs = CompileInputs::new();
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        Fabro.load(file, &text, &files, &inputs)
    }));
    match result {
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(ToString::to_string))
                .unwrap_or_else(|| "unknown panic".to_string());
            (
                Outcome {
                    file:        file.to_string(),
                    class:       Class::Panicked,
                    diagnostics: Vec::new(),
                    nodes:       0,
                    panic:       Some(message),
                },
                None,
            )
        }
        Ok(lowered) => {
            let frontend::Lowered {
                graph,
                children,
                diagnostics,
            } = lowered;
            let diagnostics = diagnostics.into_vec();
            let class = if graph.is_some() {
                if diagnostics.is_empty() {
                    Class::Clean
                } else {
                    Class::Warnings
                }
            } else if diagnostics
                .iter()
                .filter(|d| d.is_error())
                .all(|d| d.unsupported_feature().is_some())
            {
                Class::Unsupported
            } else {
                Class::OtherError
            };
            (
                Outcome {
                    file: file.to_string(),
                    class,
                    nodes: graph.as_ref().map_or(0, |g| g.nodes.len()),
                    diagnostics,
                    panic: None,
                },
                graph.map(|graph| Artifact { graph, children }),
            )
        }
    }
}

pub fn check_all(root: &Path) -> Vec<Outcome> {
    workflows(root)
        .into_iter()
        .map(|file| check_one(root, &file))
        .collect()
}

/// The report, as Markdown.
pub fn report(outcomes: &[Outcome], pin: &str) -> String {
    let mut out = String::new();
    let fabro: Vec<&Outcome> = outcomes.iter().filter(|o| !o.is_attractor()).collect();
    let attractor: Vec<&Outcome> = outcomes.iter().filter(|o| o.is_attractor()).collect();
    let _ = writeln!(out, "# Fabro compatibility corpus report\n");
    let _ = writeln!(
        out,
        "{} workflows from the Fabro repository at `{pin}`: {} Fabro workflows and demos, and \
         {} Attractor-dialect fixtures under `test/attractor/`, kept so the dialect boundary stays \
         tested: a fixture with an Attractor spelling is an expected `unsupported.attractor` \
         rejection, and one within the Fabro subset lowers. Generated by \
         `cargo nextest run -p petri-fabro-acceptance --test harness` after \
         `scripts/corpus-fetch-fabro.sh`.\n",
        outcomes.len(),
        fabro.len(),
        attractor.len()
    );
    let count = |set: &[&Outcome], c: Class| set.iter().filter(|o| o.class == c).count();
    let _ = writeln!(
        out,
        "| Result (of the {} Fabro workflows) | Count | Share |",
        fabro.len()
    );
    let _ = writeln!(out, "|---|---|---|");
    for (label, class) in [
        ("lowered clean", Class::Clean),
        ("lowered with warnings", Class::Warnings),
        (
            "rejected with a specific `unsupported.*` code",
            Class::Unsupported,
        ),
        ("**failed for any other reason**", Class::OtherError),
        ("**panicked**", Class::Panicked),
    ] {
        let n = count(&fabro, class);
        let share = if fabro.is_empty() {
            0
        } else {
            n * 100 / fabro.len()
        };
        let _ = writeln!(out, "| {label} | {n} | {share}% |");
    }
    let _ = writeln!(out, "\n## Rejections by feature\n");
    let mut features: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for outcome in outcomes {
        for feature in outcome.unsupported_features() {
            features.entry(feature).or_default().push(&outcome.file);
        }
    }
    let _ = writeln!(out, "| `unsupported.*` feature | Files |");
    let _ = writeln!(out, "|---|---|");
    for (feature, files) in &features {
        let _ = writeln!(out, "| `{feature}` | {} |", files.len());
    }
    let _ = writeln!(out, "\n## Warnings by code\n");
    let mut warnings: BTreeMap<String, usize> = BTreeMap::new();
    for outcome in outcomes {
        for d in outcome.diagnostics.iter().filter(|d| !d.is_error()) {
            *warnings.entry(d.code.to_string()).or_default() += 1;
        }
    }
    let _ = writeln!(out, "| Code | Occurrences |");
    let _ = writeln!(out, "|---|---|");
    for (code, n) in &warnings {
        let _ = writeln!(out, "| `{code}` | {n} |");
    }
    let _ = writeln!(out, "\n## Every file\n");
    let _ = writeln!(out, "| File | Result | Nodes | Codes |");
    let _ = writeln!(out, "|---|---|---|---|");
    for outcome in outcomes {
        let mut codes: Vec<String> = outcome
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect();
        codes.sort();
        codes.dedup();
        let class = match outcome.class {
            Class::Clean => "clean",
            Class::Warnings => "warnings",
            Class::Unsupported => "unsupported",
            Class::OtherError => "**other error**",
            Class::Panicked => "**panicked**",
        };
        let _ = writeln!(
            out,
            "| `{}` | {class} | {} | {} |",
            outcome.file,
            outcome.nodes,
            codes
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    out
}

/// The pinned Fabro commit, from `crates/fabro/corpus-pin.txt`.
pub fn pin() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus-pin.txt");
    fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}
