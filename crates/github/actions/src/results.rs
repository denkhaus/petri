//! The per-run results service, as steps see it.
//!
//! GitHub's toolkit reaches one backend for artifacts (and, behind
//! `ACTIONS_CACHE_SERVICE_V2`, the cache): `ACTIONS_RESULTS_URL` +
//! `ACTIONS_RUNTIME_TOKEN`. The host stands a local service in per run and
//! registers this capability; action steps inject the pair into their process
//! env — actions only, never `run:` scripts, the same visibility GitHub gives
//! the token.
//!
//! Only the port and token live here. The URL's host half depends on where the
//! *step's process* runs — loopback for host processes, the executor's
//! guaranteed alias for containers — so it is asked of the execution
//! environment at injection time (`ExecEnv::host_address`,
//! `ContainerRunner::host_address`), never baked in.

use smol_str::SmolStr;

/// Where the run's results service listens, and the exact bearer string it
/// accepts. Registered per run by the host; absent means no service was stood
/// up, and toolkit calls fail exactly as they do with no backend configured.
pub struct ResultsServiceCap {
    pub port: u16,
    pub token: SmolStr,
}

impl ResultsServiceCap {
    /// The env pair for a process that reaches this machine at `host_address`.
    pub fn env(&self, host_address: &str) -> [(SmolStr, SmolStr); 2] {
        [
            (
                SmolStr::new("ACTIONS_RESULTS_URL"),
                // The trailing slash is load-bearing: the toolkit resolves
                // `twirp/…` against this as a relative URL.
                SmolStr::new(format!("http://{host_address}:{}/", self.port)),
            ),
            (
                SmolStr::new("ACTIONS_RUNTIME_TOKEN"),
                self.token.clone(),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_keeps_its_trailing_slash() {
        let cap = ResultsServiceCap {
            port: 4242,
            token: SmolStr::new("t"),
        };
        let env = cap.env("host.docker.internal");
        assert_eq!(env[0].1, "http://host.docker.internal:4242/");
        assert_eq!(env[1].1, "t");
    }
}
