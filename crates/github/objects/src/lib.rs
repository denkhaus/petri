//! The per-run ObjectService: one process standing in for GitHub's results
//! backends — the twirp `ArtifactService` over a run-scoped store now, the
//! `CacheService` over a host-scoped store next — behind one listener, one
//! minted token, one blob implementation.
//!
//! The 2026-era toolkit talks to one backend, the *results service*:
//! `ACTIONS_RESULTS_URL` + `ACTIONS_RUNTIME_TOKEN`, JSON-twirp under
//! `{url}twirp/github.actions.results.api.v1.<Service>/<Method>`, with uploads
//! and downloads going through signed URLs the service mints. This crate
//! serves exactly that surface locally:
//!
//! - **The token is also the auth.** One unsigned JWT per run carries the
//!   `scp` claim the toolkit decodes without verifying, plus a random nonce —
//!   the listener accepts only that exact bearer string, and signed URLs are
//!   HMAC query tokens under a per-run secret. The listener binds beyond
//!   loopback so containers can reach it (`host.docker.internal`), and the
//!   token is what keeps that from being an open write endpoint on the LAN.
//! - **One service per run.** The host starts it beside the run dir and hands
//!   its capability to the run's steps; it dies with the run. A resumed run
//!   starts a fresh listener over the same store — the store is the durable
//!   half, the listener is not.
//!
//! The server runs on its own thread with its own single-threaded runtime, so
//! starting it needs no async context and dropping the [`ObjectService`] tears
//! it down from any context.

mod cache;
mod http;
mod index;
mod store;
mod token;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

/// The persistent store root — the host-scoped half of the object world: the
/// cache entries and the tool cache, owned by one root so one component prunes
/// them. `$PETRI_STORE` overrides; the default rides the same platform
/// convention as the action cache (`$XDG_CACHE_HOME`, else `~/.cache`).
pub fn default_store_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PETRI_STORE") {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("petri").join("store");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("petri")
            .join("store");
    }
    std::env::temp_dir().join("petri-store")
}

/// The cache half of a store root — where [`ObjectService::start`] expects its
/// cache store. The layout under the root is this crate's to own, so the
/// pruner that spans both halves has one authority to consult.
pub fn cache_dir(store: &Path) -> PathBuf {
    store.join("cache")
}

/// The tool-cache half of a store root, for this machine's OS. The toolkit's
/// own layout has no OS segment, so the store splits per OS above it.
pub fn tool_cache_dir(store: &Path) -> PathBuf {
    store.join("toolcache").join(std::env::consts::OS)
}

/// A running object service: listener, token, stores. Drop tears it down.
pub struct ObjectService {
    port: u16,
    token: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ObjectService {
    /// Bind an ephemeral port on every interface and serve: the artifact store
    /// under `artifacts` (run-scoped; created if absent, reopened on resume),
    /// the cache store under `cache` (host-scoped, shared across runs).
    pub fn start(artifacts: PathBuf, cache: PathBuf) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let backend = Arc::new(http::Backend::new(artifacts, cache, port)?);
        let token = backend.token().to_string();
        let (shutdown, rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("petri-objects".into())
            .spawn(move || http::serve(listener, backend, rx))?;
        Ok(Self {
            port,
            token,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    /// The bound port. The URL's host half is the caller's business: it depends
    /// on where the *client* runs (loopback for host processes, the gateway
    /// alias for containers), which only the execution environment knows.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The exact bearer string the listener accepts — and the JWT whose `scp`
    /// claim the toolkit reads.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for ObjectService {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
