//! `petri` — the binary.
//!
//! The whole of it: the distribution's runtime, handed to the format-agnostic
//! command line. What the commands do lives in `cli`; which frontends and step
//! kinds they see is decided here, by [`petri::runtime`].

#[tokio::main]
async fn main() -> std::process::ExitCode {
    cli::main(petri::runtime).await
}
