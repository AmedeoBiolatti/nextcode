//! Port forwarding in `devcontainer.json`: `forwardPorts`,
//! `portsAttributes` and `otherPortsAttributes`, as VS Code reads them.

use std::fmt;

use regex::Regex;
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::Value;

/// A `forwardPorts` entry: `8000`, `"8000"`, or `"db:5432"` (the agent
/// connects to host `db`, e.g. another compose service).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForwardPort {
    /// `None`: the container itself (localhost).
    pub host: Option<String>,
    pub port: u16,
}

impl ForwardPort {
    /// `8000`, `host:8000` (`localhost`/`127.0.0.1` mean the container).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) => (Some(host.trim_matches(['[', ']'])), port),
            None => (None, text),
        };
        let port: u16 = port.parse().ok().filter(|&port| port != 0)?;
        let host = host.filter(|host| !is_local(host)).map(str::to_owned);
        if host.as_deref() == Some("") {
            return None;
        }
        Some(Self { host, port })
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => Some(Self {
                host: None,
                port: number
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|&port| port != 0)?,
            }),
            Value::String(text) => Self::parse(text),
            _ => None,
        }
    }
}

impl fmt::Display for ForwardPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{host}:{}", self.port),
            None => write!(f, "{}", self.port),
        }
    }
}

/// Whether `host` names the machine itself.
pub fn is_local(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// What happens when a port is forwarded automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnAutoForward {
    /// A notification with Open in Browser (the default).
    Notify,
    OpenBrowser,
    /// Like `openBrowser`, once per session.
    OpenBrowserOnce,
    /// VS Code's preview pane; this editor opens the browser.
    OpenPreview,
    /// Forwarded without a notification.
    Silent,
    /// Not forwarded automatically.
    Ignore,
}

/// The scheme of the port's URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Http,
    Https,
}

/// A `portsAttributes` value. `requireLocalPort` and `elevateIfNeeded` are
/// read but this editor never elevates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortAttributes {
    pub label: Option<String>,
    pub on_auto_forward: Option<OnAutoForward>,
    pub protocol: Option<PortProtocol>,
    /// Fail instead of picking another local port when the same is taken.
    pub require_local_port: Option<bool>,
    pub elevate_if_needed: Option<bool>,
}

impl PortAttributes {
    /// Fills the fields `self` leaves unset from `other`.
    fn fill_from(&mut self, other: &Self) {
        self.label = self.label.take().or_else(|| other.label.clone());
        self.on_auto_forward = self.on_auto_forward.or(other.on_auto_forward);
        self.protocol = self.protocol.or(other.protocol);
        self.require_local_port = self.require_local_port.or(other.require_local_port);
        self.elevate_if_needed = self.elevate_if_needed.or(other.elevate_if_needed);
    }
}

/// A `portsAttributes` key.
#[derive(Debug, Clone)]
pub enum PortPattern {
    Port(u16),
    /// Inclusive.
    Range(u16, u16),
    HostPort(String, u16),
    /// Matched against the listening process's command line.
    Command(Regex),
}

impl PartialEq for PortPattern {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Port(a), Self::Port(b)) => a == b,
            (Self::Range(a, b), Self::Range(c, d)) => (a, b) == (c, d),
            (Self::HostPort(a, b), Self::HostPort(c, d)) => (a, b) == (c, d),
            (Self::Command(a), Self::Command(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}

impl Eq for PortPattern {}

impl PortPattern {
    /// `8000`, `8000-8010`, `host:8000`, else a regular expression; `None`
    /// for an invalid expression.
    pub fn parse(key: &str) -> Option<Self> {
        let key = key.trim();
        if let Ok(port) = key.parse() {
            return Some(Self::Port(port));
        }
        if let Some((start, end)) = key.split_once('-')
            && let (Ok(start), Ok(end)) = (start.trim().parse(), end.trim().parse())
        {
            return Some(Self::Range(start, end));
        }
        if let Some(port) = ForwardPort::parse(key) {
            return Some(match port.host {
                Some(host) => Self::HostPort(host, port.port),
                None => Self::Port(port.port),
            });
        }
        Regex::new(key).ok().map(Self::Command)
    }

    /// Whether the port (`host` `None`: the container itself), listened to
    /// by `command` if known, matches.
    pub fn matches(&self, host: Option<&str>, port: u16, command: Option<&str>) -> bool {
        match self {
            Self::Port(p) => *p == port && host.is_none(),
            Self::Range(start, end) => (*start..=*end).contains(&port) && host.is_none(),
            Self::HostPort(h, p) => {
                *p == port && host.is_some_and(|host| host.eq_ignore_ascii_case(h))
            }
            Self::Command(regex) => command.is_some_and(|command| regex.is_match(command)),
        }
    }
}

/// The port forwarding part of a `devcontainer.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortsConfig {
    pub forward: Vec<ForwardPort>,
    /// `portsAttributes` in document order.
    pub attributes: Vec<(PortPattern, PortAttributes)>,
    pub other: Option<PortAttributes>,
}

impl PortsConfig {
    /// The attributes of a port: those of matching `portsAttributes` keys
    /// (exact port keys first, then the others in document order, each
    /// filling what earlier ones left unset), else `otherPortsAttributes`.
    pub fn attributes(
        &self,
        host: Option<&str>,
        port: u16,
        command: Option<&str>,
    ) -> PortAttributes {
        let matching: Vec<&(PortPattern, PortAttributes)> = self
            .attributes
            .iter()
            .filter(|(pattern, _)| pattern.matches(host, port, command))
            .collect();
        if matching.is_empty() {
            return self.other.clone().unwrap_or_default();
        }
        let exact = |pattern: &PortPattern| {
            matches!(pattern, PortPattern::Port(_) | PortPattern::HostPort(..))
        };
        let mut merged = PortAttributes::default();
        for (_, attributes) in matching.iter().filter(|(pattern, _)| exact(pattern)) {
            merged.fill_from(attributes);
        }
        for (_, attributes) in matching.iter().filter(|(pattern, _)| !exact(pattern)) {
            merged.fill_from(attributes);
        }
        merged
    }
}

/// `forwardPorts`, skipping entries that are not ports.
pub(crate) fn forward_ports<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ForwardPort>, D::Error> {
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.iter().filter_map(ForwardPort::from_value).collect())
}

/// `portsAttributes` in document order, skipping invalid keys and values.
pub(crate) fn ports_attributes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(PortPattern, PortAttributes)>, D::Error> {
    struct Entries;

    impl<'de> Visitor<'de> for Entries {
        type Value = Vec<(PortPattern, PortAttributes)>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an object of port attributes")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while let Some((key, value)) = map.next_entry::<String, Value>()? {
                if let (Some(pattern), Ok(attributes)) = (
                    PortPattern::parse(&key),
                    serde_json::from_value::<PortAttributes>(value),
                ) {
                    entries.push((pattern, attributes));
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn forward_ports_take_numbers_strings_and_hosts() {
        let config = Config::parse(
            r#"{ "image": "x",
                 "forwardPorts": [8000, "3000", "db:5432", "localhost:9000", "nope", 0, 70000, true],
                 "portsAttributes": {
                    "8000": { "label": "Web", "onAutoForward": "openBrowser", "protocol": "https" },
                    "9000-9010": { "label": "Range", "onAutoForward": "silent", "requireLocalPort": true },
                    "db:5432": { "label": "Postgres", "elevateIfNeeded": false },
                    "python.*manage": { "onAutoForward": "ignore" },
                    "([": { "label": "invalid regex" },
                    "7000": { "onAutoForward": "bogus" }
                 },
                 "otherPortsAttributes": { "onAutoForward": "silent" } }"#,
        )
        .unwrap();
        let ports = config.ports();
        let port = |host: Option<&str>, port| ForwardPort {
            host: host.map(str::to_owned),
            port,
        };
        assert_eq!(
            ports.forward,
            [
                port(None, 8000),
                port(None, 3000),
                port(Some("db"), 5432),
                port(None, 9000)
            ]
        );
        assert_eq!(ports.forward[2].to_string(), "db:5432");
        assert_eq!(ports.attributes.len(), 4, "{:?}", ports.attributes);

        let web = ports.attributes(None, 8000, None);
        assert_eq!(web.label.as_deref(), Some("Web"));
        assert_eq!(web.on_auto_forward, Some(OnAutoForward::OpenBrowser));
        assert_eq!(web.protocol, Some(PortProtocol::Https));

        let ranged = ports.attributes(None, 9005, None);
        assert_eq!(ranged.label.as_deref(), Some("Range"));
        assert_eq!(ranged.require_local_port, Some(true));
        // Ranges and plain ports are the container's, not another host's.
        assert_eq!(
            ports.attributes(Some("db"), 9005, None).on_auto_forward,
            Some(OnAutoForward::Silent),
            "falls back to otherPortsAttributes"
        );

        let db = ports.attributes(Some("db"), 5432, None);
        assert_eq!(db.label.as_deref(), Some("Postgres"));
        assert_eq!(ports.attributes(None, 5432, None).label, None);

        let django = ports.attributes(None, 8001, Some("python manage.py runserver 8001"));
        assert_eq!(django.on_auto_forward, Some(OnAutoForward::Ignore));
        // An exact key wins over a command match; the command fills the rest.
        let both = ports.attributes(None, 8000, Some("python manage.py runserver"));
        assert_eq!(both.on_auto_forward, Some(OnAutoForward::OpenBrowser));

        let other = ports.attributes(None, 1234, None);
        assert_eq!(other.on_auto_forward, Some(OnAutoForward::Silent));
    }

    #[test]
    fn configs_without_ports_have_none() {
        let ports = Config::parse(r#"{ "image": "x" }"#).unwrap().ports();
        assert_eq!(ports, PortsConfig::default());
        assert_eq!(ports.attributes(None, 80, None), PortAttributes::default());
    }

    #[test]
    fn patterns_parse_ports_ranges_hosts_and_expressions() {
        assert_eq!(PortPattern::parse("80"), Some(PortPattern::Port(80)));
        assert_eq!(
            PortPattern::parse("8000 - 8010"),
            Some(PortPattern::Range(8000, 8010))
        );
        assert_eq!(
            PortPattern::parse("db:5432"),
            Some(PortPattern::HostPort("db".into(), 5432))
        );
        assert_eq!(
            PortPattern::parse("127.0.0.1:80"),
            Some(PortPattern::Port(80))
        );
        assert!(matches!(
            PortPattern::parse("^node"),
            Some(PortPattern::Command(_))
        ));
        assert_eq!(PortPattern::parse("(("), None);
    }
}
