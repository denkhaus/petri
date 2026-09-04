//! Raw engine flags on a container scope or a service, as GitHub's
//! `container.options` and `services.<id>.options` carry them, lowered to
//! the typed fields the Docker provider accepts.
//!
//! The graph carries these as shell-split tokens. The set is the one the
//! corpus and the acceptance battery use — env, user, DNS, added
//! capabilities, privilege, platform, and for services an entrypoint and a
//! health check. An unknown flag fails the acquire naming the flag, so a
//! workflow never runs with an option silently dropped.

use std::time::Duration;

use executor::EnvError;
use smol_str::SmolStr;

/// Options a job's container accepts.
#[derive(Debug, Default)]
pub(crate) struct ContainerOptions {
    /// `-e KEY=VALUE`; applied after the scope env, so a flag wins.
    pub env:        Vec<(String, String)>,
    pub user:       Option<String>,
    pub dns:        Vec<String>,
    pub cap_add:    Vec<String>,
    pub privileged: bool,
    pub platform:   Option<String>,
}

/// A service's Docker health check.
#[derive(Debug, Default)]
pub(crate) struct Health {
    pub cmd:             Option<String>,
    pub interval_ms:     Option<u64>,
    pub timeout_ms:      Option<u64>,
    pub retries:         Option<u64>,
    pub start_period_ms: Option<u64>,
}

/// Options a service container accepts.
#[derive(Debug, Default)]
pub(crate) struct ServiceOptions {
    pub env:        Vec<(String, String)>,
    pub user:       Option<String>,
    pub entrypoint: Option<Vec<String>>,
    pub dns:        Vec<String>,
    pub cap_add:    Vec<String>,
    pub health:     Option<Health>,
}

/// Lowers a job container's raw flags.
pub(crate) fn parse_container(flags: &[SmolStr]) -> Result<ContainerOptions, EnvError> {
    let mut options = ContainerOptions::default();
    let mut tokens = Tokens::new(flags);
    while let Some((flag, inline)) = tokens.next_flag() {
        match flag.as_str() {
            "-e" | "--env" => {
                let pair = tokens.value(&flag, inline)?;
                options.env.push(env_pair(&flag, &pair)?);
            }
            "-u" | "--user" => options.user = Some(tokens.value(&flag, inline)?),
            "--dns" => options.dns.push(tokens.value(&flag, inline)?),
            "--cap-add" => options.cap_add.push(tokens.value(&flag, inline)?),
            "--platform" => options.platform = Some(tokens.value(&flag, inline)?),
            "--privileged" => options.privileged = true,
            "--entrypoint"
            | "--health-cmd"
            | "--health-interval"
            | "--health-timeout"
            | "--health-retries"
            | "--health-start-period" => {
                return Err(unsupported(
                    &flag,
                    "it applies to a service container, not the job container",
                ));
            }
            _ => return Err(unsupported(&flag, "no typed mapping exists for it")),
        }
    }
    Ok(options)
}

/// Lowers a service container's raw flags.
pub(crate) fn parse_service(flags: &[SmolStr]) -> Result<ServiceOptions, EnvError> {
    let mut options = ServiceOptions::default();
    let mut tokens = Tokens::new(flags);
    while let Some((flag, inline)) = tokens.next_flag() {
        match flag.as_str() {
            "-e" | "--env" => {
                let pair = tokens.value(&flag, inline)?;
                options.env.push(env_pair(&flag, &pair)?);
            }
            "-u" | "--user" => options.user = Some(tokens.value(&flag, inline)?),
            "--entrypoint" => options.entrypoint = Some(vec![tokens.value(&flag, inline)?]),
            "--dns" => options.dns.push(tokens.value(&flag, inline)?),
            "--cap-add" => options.cap_add.push(tokens.value(&flag, inline)?),
            "--health-cmd" => health(&mut options).cmd = Some(tokens.value(&flag, inline)?),
            "--health-interval" => {
                health(&mut options).interval_ms =
                    Some(duration_ms(&flag, &tokens.value(&flag, inline)?)?);
            }
            "--health-timeout" => {
                health(&mut options).timeout_ms =
                    Some(duration_ms(&flag, &tokens.value(&flag, inline)?)?);
            }
            "--health-start-period" => {
                health(&mut options).start_period_ms =
                    Some(duration_ms(&flag, &tokens.value(&flag, inline)?)?);
            }
            "--health-retries" => {
                let raw = tokens.value(&flag, inline)?;
                let retries = raw
                    .parse::<u64>()
                    .map_err(|_| unsupported(&flag, &format!("`{raw}` is not a whole number")))?;
                health(&mut options).retries = Some(retries);
            }
            "--platform" | "--privileged" => {
                return Err(unsupported(
                    &flag,
                    "it applies to the job container, not a service",
                ));
            }
            _ => return Err(unsupported(&flag, "no typed mapping exists for it")),
        }
    }
    Ok(options)
}

fn health(options: &mut ServiceOptions) -> &mut Health {
    options.health.get_or_insert_with(Health::default)
}

/// Splits `KEY=VALUE`. A bare `KEY` would mean "inherit from the runner's
/// environment" on GitHub, which no sandbox has; it is rejected.
fn env_pair(flag: &str, pair: &str) -> Result<(String, String), EnvError> {
    match pair.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(unsupported(
            flag,
            &format!(
                "`{pair}` must be `KEY=VALUE`; a bare name has no runner environment to inherit"
            ),
        )),
    }
}

/// A Docker duration: a number with an `ms`, `s`, `m`, or `h` suffix (a bare
/// number is seconds, as `docker run` reads it).
fn duration_ms(flag: &str, raw: &str) -> Result<u64, EnvError> {
    let (digits, unit) = raw
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((raw, "s"), |at| raw.split_at(at));
    let value: f64 = digits
        .parse()
        .map_err(|_| unsupported(flag, &format!("`{raw}` is not a duration")))?;
    let scale = match unit {
        "ms" => 1.0,
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        _ => return Err(unsupported(flag, &format!("`{raw}` has an unknown unit"))),
    };
    let millis = Duration::from_secs_f64(value * scale / 1_000.0).as_millis();
    u64::try_from(millis).map_err(|_| unsupported(flag, &format!("`{raw}` is out of range")))
}

fn unsupported(flag: &str, why: &str) -> EnvError {
    EnvError::Backend {
        backend:   SmolStr::new("sandbox"),
        operation: SmolStr::new("acquire"),
        message:   format!("container option `{flag}` is not supported: {why}"),
    }
}

/// A cursor over shell-split flags that reads `--flag value` and
/// `--flag=value` alike.
struct Tokens<'a> {
    flags: &'a [SmolStr],
    at:    usize,
}

impl<'a> Tokens<'a> {
    fn new(flags: &'a [SmolStr]) -> Self {
        Self { flags, at: 0 }
    }

    /// The next flag and, for `--flag=value`, its inline value.
    fn next_flag(&mut self) -> Option<(String, Option<String>)> {
        let token = self.flags.get(self.at)?;
        self.at += 1;
        if token.starts_with("--")
            && let Some((flag, value)) = token.split_once('=')
        {
            return Some((flag.to_owned(), Some(value.to_owned())));
        }
        Some((token.to_string(), None))
    }

    /// The flag's value: inline, or the next token.
    fn value(&mut self, flag: &str, inline: Option<String>) -> Result<String, EnvError> {
        if let Some(value) = inline {
            return Ok(value);
        }
        let value = self
            .flags
            .get(self.at)
            .ok_or_else(|| unsupported(flag, "it needs a value"))?;
        self.at += 1;
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(text: &str) -> Vec<SmolStr> {
        text.split_whitespace().map(SmolStr::new).collect()
    }

    #[test]
    fn container_flags_lower_in_both_spellings() {
        let options =
            parse_container(&flags("-e A=1 --env=B=2 --user root --dns 127.0.0.1 --cap-add=NET_ADMIN --privileged --platform linux/amd64"))
                .expect("parses");
        assert_eq!(options.env, vec![
            ("A".into(), "1".into()),
            ("B".into(), "2".into())
        ]);
        assert_eq!(options.user.as_deref(), Some("root"));
        assert_eq!(options.dns, vec!["127.0.0.1".to_owned()]);
        assert_eq!(options.cap_add, vec!["NET_ADMIN".to_owned()]);
        assert!(options.privileged);
        assert_eq!(options.platform.as_deref(), Some("linux/amd64"));
    }

    #[test]
    fn a_service_health_check_lowers_to_milliseconds() {
        let options = parse_service(&flags(
            "--health-cmd redis-cli --health-interval 1s --health-timeout 500ms --health-retries 30 --entrypoint /bin/sh",
        ))
        .expect("parses");
        let health = options.health.expect("health");
        assert_eq!(health.cmd.as_deref(), Some("redis-cli"));
        assert_eq!(health.interval_ms, Some(1_000));
        assert_eq!(health.timeout_ms, Some(500));
        assert_eq!(health.retries, Some(30));
        assert_eq!(options.entrypoint, Some(vec!["/bin/sh".to_owned()]));
    }

    #[test]
    fn unknown_and_misplaced_flags_are_rejected_by_name() {
        let error = parse_container(&flags("--shm-size 1g")).expect_err("rejects");
        assert!(error.to_string().contains("--shm-size"), "{error}");
        let error = parse_container(&flags("--entrypoint sh")).expect_err("rejects");
        assert!(error.to_string().contains("service container"), "{error}");
        let error = parse_container(&flags("-e BARE")).expect_err("rejects");
        assert!(error.to_string().contains("KEY=VALUE"), "{error}");
    }
}
