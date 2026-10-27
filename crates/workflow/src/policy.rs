//! What keeps a step's service up: readiness over HTTP, a `restart`
//! policy, a `healthcheck` probed while it is ready; and the `env_file`s
//! its environment is read from. Shapes as in Docker Compose and
//! process-compose; each parser returns problems with their offset.

use std::time::Duration;

use crate::{
    file::duration,
    yaml::{Data, Node},
};

/// A problem and where it is.
pub type Located = (String, usize);

/// An HTTP(S) URL that must answer with an accepted status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpProbe {
    pub url: String,
    /// Accepted statuses; empty: 200–399.
    pub status: Vec<u16>,
}

impl HttpProbe {
    pub fn accepts(&self, status: u16) -> bool {
        if self.status.is_empty() {
            (200..400).contains(&status)
        } else {
            self.status.contains(&status)
        }
    }
}

/// How a health check looks at a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// A command where the step runs (its shell, `cwd` and `env`); exit
    /// code 0 is healthy.
    Command(String),
    Http(HttpProbe),
    /// Something listens on this TCP port where the step runs.
    Port(u16),
}

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_RETRIES: u32 = 3;
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(2);

/// Probed every `interval` while the step is ready; `retries` failures in
/// a row (not counting those within `start_period` of getting ready) make
/// it unhealthy: it fails (and restarts with a `restart` policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Healthcheck {
    pub probe: Probe,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
    pub start_period: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Never (the default).
    No,
    /// When its command fails: a non-zero exit (before or after ready), a
    /// timeout, unhealthy.
    OnFailure,
    /// Also when its command exits with 0.
    Always,
}

impl RestartPolicy {
    pub fn name(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::OnFailure => "on_failure",
            Self::Always => "always",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    pub policy: RestartPolicy,
    /// Restarts before giving up (`None`: no limit).
    pub max: Option<u32>,
    /// Wait before each restart.
    pub backoff: Duration,
}

impl Default for Restart {
    fn default() -> Self {
        Self {
            policy: RestartPolicy::No,
            max: None,
            backoff: DEFAULT_BACKOFF,
        }
    }
}

/// A dotenv file whose variables a step gets (its `env` wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFile {
    /// Relative to the step's `cwd` (else its project, else the home
    /// folder) where it runs; may hold `${…}` variables.
    pub path: String,
    /// A missing required file fails the step.
    pub required: bool,
}

fn text(node: &Node, what: &str) -> Result<String, Located> {
    match node.text() {
        Some(text) if !text.trim().is_empty() => Ok(text.to_owned()),
        _ => Err((format!("`{what}` must be a non-empty string"), node.start)),
    }
}

fn http_url(node: &Node) -> Result<String, Located> {
    let url = text(node, "http")?;
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url)
    } else {
        Err((format!("`{url}` is not an http(s) URL"), node.start))
    }
}

/// `'http://…'` or `{url: 'http://…', status: 200 | [200, 204]}`.
pub fn http_probe(node: &Node) -> Result<HttpProbe, Located> {
    let Some(entries) = node.as_map() else {
        return Ok(HttpProbe {
            url: http_url(node)?,
            status: Vec::new(),
        });
    };
    if let Some(entry) = entries
        .iter()
        .find(|entry| !["url", "status"].contains(&entry.key.as_str()))
    {
        return Err((
            format!(
                "unknown key `{}` (an HTTP check has `url` and `status`)",
                entry.key
            ),
            entry.key_start,
        ));
    }
    let url = http_url(
        node.get("url")
            .ok_or(("an HTTP check needs `url`".to_owned(), node.start))?,
    )?;
    let status = match node.get("status") {
        None => Vec::new(),
        Some(value) => {
            let items: Vec<&Node> = match &value.data {
                Data::List(items) => items.iter().collect(),
                _ => vec![value],
            };
            items
                .into_iter()
                .map(|item| match &item.data {
                    Data::Number(number) => number
                        .as_u64()
                        .filter(|code| (100..600).contains(code))
                        .map(|code| code as u16)
                        .ok_or(("a status is 100–599".to_owned(), item.start)),
                    _ => Err((
                        "`status` is a status code or a list of them".to_owned(),
                        item.start,
                    )),
                })
                .collect::<Result<_, _>>()?
        }
    };
    Ok(HttpProbe { url, status })
}

fn port(node: &Node) -> Result<u16, Located> {
    match &node.data {
        Data::Number(number) => number
            .as_u64()
            .filter(|port| (1..=65535).contains(port))
            .map(|port| port as u16)
            .ok_or(("`port` must be 1-65535".to_owned(), node.start)),
        _ => Err(("`port` must be a port number".to_owned(), node.start)),
    }
}

/// The keys a `healthcheck` takes.
pub const HEALTHCHECK_KEYS: &[&str] = &[
    "test",
    "http",
    "port",
    "interval",
    "timeout",
    "retries",
    "start_period",
];

/// `{test: <command> | http: … | port: N, interval?, timeout?, retries?,
/// start_period?}`.
pub fn healthcheck(node: &Node) -> Result<Healthcheck, Located> {
    let Some(entries) = node.as_map() else {
        return Err((
            "a `healthcheck` is a mapping: `{test: <command>, interval: 30s}` (or `http:`, `port:` instead of `test:`)"
                .into(),
            node.start,
        ));
    };
    if let Some(entry) = entries
        .iter()
        .find(|entry| !HEALTHCHECK_KEYS.contains(&entry.key.as_str()))
    {
        return Err((
            format!(
                "unknown key `{}` in `healthcheck` (known: {})",
                entry.key,
                HEALTHCHECK_KEYS.join(", ")
            ),
            entry.key_start,
        ));
    }
    let probes: Vec<_> = entries
        .iter()
        .filter(|entry| ["test", "http", "port"].contains(&entry.key.as_str()))
        .collect();
    let probe = match probes.as_slice() {
        [entry] => match entry.key.as_str() {
            "test" => Probe::Command(text(&entry.value, "test")?),
            "http" => Probe::Http(http_probe(&entry.value)?),
            _ => Probe::Port(port(&entry.value)?),
        },
        [] => {
            return Err((
                "a `healthcheck` needs `test` (a command), `http` or `port`".into(),
                node.start,
            ));
        }
        [_, second, ..] => {
            return Err((
                "a `healthcheck` has one of `test`, `http`, `port`".into(),
                second.key_start,
            ));
        }
    };
    let positive = |key: &str, default: Duration| -> Result<Duration, Located> {
        match node.get(key) {
            None => Ok(default),
            Some(value) => match duration(value) {
                Ok(duration) if !duration.is_zero() => Ok(duration),
                Ok(_) => Err((format!("`{key}` must be longer than 0"), value.start)),
                Err(err) => Err((format!("`{key}`: {err}"), value.start)),
            },
        }
    };
    let retries = match node.get("retries") {
        None => DEFAULT_RETRIES,
        Some(value) => match &value.data {
            Data::Number(number) => number
                .as_u64()
                .filter(|retries| (1..=1000).contains(retries))
                .map(|retries| retries as u32)
                .ok_or((
                    "`retries` must be a whole number from 1".to_owned(),
                    value.start,
                ))?,
            _ => return Err(("`retries` must be a number".into(), value.start)),
        },
    };
    let start_period = match node.get("start_period") {
        None => Duration::ZERO,
        Some(value) => {
            duration(value).map_err(|err| (format!("`start_period`: {err}"), value.start))?
        }
    };
    Ok(Healthcheck {
        probe,
        interval: positive("interval", DEFAULT_INTERVAL)?,
        timeout: positive("timeout", DEFAULT_PROBE_TIMEOUT)?,
        retries,
        start_period,
    })
}

/// `no`, `on_failure`, `always`, or `{policy: …, max?: N, backoff?: 2s}`.
pub fn restart(node: &Node) -> Result<Restart, Located> {
    let policy = |node: &Node| match node.text() {
        Some("no") => Ok(RestartPolicy::No),
        Some("on_failure") => Ok(RestartPolicy::OnFailure),
        Some("always") => Ok(RestartPolicy::Always),
        Some(other) => Err((
            format!("unknown policy `{other}` (no, on_failure, always)"),
            node.start,
        )),
        None if matches!(node.data, Data::Bool(false)) => Ok(RestartPolicy::No),
        None => Err((
            "a restart policy is no, on_failure or always".to_owned(),
            node.start,
        )),
    };
    let Some(entries) = node.as_map() else {
        return Ok(Restart {
            policy: policy(node)?,
            ..Restart::default()
        });
    };
    if let Some(entry) = entries
        .iter()
        .find(|entry| !["policy", "max", "backoff"].contains(&entry.key.as_str()))
    {
        return Err((
            format!(
                "unknown key `{}` (`restart` has policy, max, backoff)",
                entry.key
            ),
            entry.key_start,
        ));
    }
    let policy = policy(
        node.get("policy")
            .ok_or(("`restart` needs `policy`".to_owned(), node.start))?,
    )?;
    let max = match node.get("max") {
        None => None,
        Some(value) => Some(match &value.data {
            Data::Number(number) => number
                .as_u64()
                .filter(|max| *max >= 1 && *max <= u64::from(u32::MAX))
                .map(|max| max as u32)
                .ok_or((
                    "`max` must be a whole number from 1".to_owned(),
                    value.start,
                ))?,
            _ => return Err(("`max` must be a number of restarts".into(), value.start)),
        }),
    };
    let backoff = match node.get("backoff") {
        None => DEFAULT_BACKOFF,
        Some(value) => duration(value).map_err(|err| (format!("`backoff`: {err}"), value.start))?,
    };
    Ok(Restart {
        policy,
        max,
        backoff,
    })
}

/// `.env`, or a list of paths and `{path: …, required: false}`.
pub fn env_files(node: &Node) -> Result<Vec<EnvFile>, Located> {
    let entry = |node: &Node| -> Result<EnvFile, Located> {
        match node.as_map() {
            None => Ok(EnvFile {
                path: text(node, "env_file")?,
                required: true,
            }),
            Some(entries) => {
                if let Some(entry) = entries
                    .iter()
                    .find(|entry| !["path", "required"].contains(&entry.key.as_str()))
                {
                    return Err((
                        format!(
                            "unknown key `{}` (an env file has path, required)",
                            entry.key
                        ),
                        entry.key_start,
                    ));
                }
                let path = text(
                    node.get("path")
                        .ok_or(("an env file needs `path`".to_owned(), node.start))?,
                    "path",
                )?;
                let required = match node.get("required") {
                    None => true,
                    Some(value) => match value.data {
                        Data::Bool(required) => required,
                        _ => return Err(("`required` must be true or false".into(), value.start)),
                    },
                };
                Ok(EnvFile { path, required })
            }
        }
    };
    match &node.data {
        Data::Null => Ok(Vec::new()),
        Data::List(items) => items.iter().map(entry).collect(),
        _ => Ok(vec![entry(node)?]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    fn node(text: &str) -> Node {
        yaml::load(text).root.unwrap()
    }

    #[test]
    fn http_probes() {
        let probe = http_probe(&node("'http://localhost:8000/health'")).unwrap();
        assert!(probe.accepts(204) && probe.accepts(302) && !probe.accepts(404));
        let probe = http_probe(&node("{url: 'https://x/h', status: [200, 401]}")).unwrap();
        assert!(probe.accepts(401) && !probe.accepts(204));
        assert!(
            http_probe(&node("ftp://x"))
                .unwrap_err()
                .0
                .contains("not an http(s) URL")
        );
        assert!(
            http_probe(&node("{url: 'http://x', status: 99}"))
                .unwrap_err()
                .0
                .contains("100–599")
        );
        assert!(
            http_probe(&node("{status: 200}"))
                .unwrap_err()
                .0
                .contains("needs `url`")
        );
    }

    #[test]
    fn healthchecks() {
        let check =
            healthcheck(&node("{test: curl -f localhost, interval: 5s, retries: 2}")).unwrap();
        assert_eq!(check.probe, Probe::Command("curl -f localhost".into()));
        assert_eq!(check.interval, Duration::from_secs(5));
        assert_eq!(
            (check.timeout, check.retries, check.start_period),
            (DEFAULT_PROBE_TIMEOUT, 2, Duration::ZERO)
        );
        assert_eq!(
            healthcheck(&node("{port: 5432}")).unwrap().probe,
            Probe::Port(5432)
        );
        assert!(matches!(
            healthcheck(&node("{http: 'http://x/h'}")).unwrap().probe,
            Probe::Http(_)
        ));
        let error = |text: &str| healthcheck(&node(text)).unwrap_err().0;
        assert!(error("{interval: 5s}").contains("needs `test`"));
        assert!(error("{test: x, port: 1}").contains("one of"));
        assert!(error("{test: x, interval: 0s}").contains("longer than 0"));
        assert!(error("{test: x, retries: 0}").contains("from 1"));
        assert!(error("{test: x, every: 5s}").contains("unknown key `every`"));
        assert!(error("curl x").contains("is a mapping"));
    }

    #[test]
    fn restart_policies() {
        assert_eq!(
            restart(&node("on_failure")).unwrap().policy,
            RestartPolicy::OnFailure
        );
        assert_eq!(restart(&node("no")).unwrap(), Restart::default());
        let full = restart(&node("{policy: always, max: 5, backoff: 10s}")).unwrap();
        assert_eq!(
            (full.policy, full.max, full.backoff),
            (RestartPolicy::Always, Some(5), Duration::from_secs(10))
        );
        assert!(
            restart(&node("sometimes"))
                .unwrap_err()
                .0
                .contains("unknown policy")
        );
        assert!(
            restart(&node("{max: 3}"))
                .unwrap_err()
                .0
                .contains("needs `policy`")
        );
        assert!(
            restart(&node("{policy: always, max: 0}"))
                .unwrap_err()
                .0
                .contains("from 1")
        );
    }

    #[test]
    fn env_file_lists() {
        assert_eq!(
            env_files(&node("[.env, {path: .env.local, required: false}]")).unwrap(),
            [
                EnvFile {
                    path: ".env".into(),
                    required: true
                },
                EnvFile {
                    path: ".env.local".into(),
                    required: false
                }
            ]
        );
        assert_eq!(env_files(&node(".env")).unwrap().len(), 1);
        assert!(
            env_files(&node("[{required: false}]"))
                .unwrap_err()
                .0
                .contains("needs `path`")
        );
    }
}
