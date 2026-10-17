//! Port forwarding for WSL and dev container projects (VS Code's Ports
//! view): container/distro ports reachable on `localhost` here.
//!
//! [`Ports`] (one per project, shared by the Ports panel and headless mode)
//! forwards ports through the agent: each forwarded port is a local
//! listener ([`forwarder`]) whose connections become agent tunnels. Ports
//! come from the user (persisted per project), from `forwardPorts` in
//! `devcontainer.json` (forwarded on connect, even before anything
//! listens), or automatically: the agent's listening sockets are polled
//! every 2 s and new ones are forwarded (`"ports": {"auto_forward"}` in
//! settings; in WSL only ports that start listening after the project
//! opened, since the distro's own services already listen then). An
//! automatically forwarded port is released when nothing listens on it any
//! more. `portsAttributes`/`otherPortsAttributes` give labels, the URL
//! scheme, and what happens on auto-forwarding (`onAutoForward`).
//!
//! Limits: TCP only (no UDP); local projects have nothing to forward.

pub mod forwarder;
pub mod panel;

use std::{collections::HashSet, io, net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

use devcontainer::ports::{ForwardPort, OnAutoForward, PortAttributes, PortProtocol, PortsConfig};
use futures::{StreamExt, channel::mpsc};
use gpui::{AppContext, Context, EventEmitter, Task};
use remote::{AgentClient, ListeningPort};
use serde::{Deserialize, Serialize};

use crate::{layout, project::Project, settings::PortSettings};

use forwarder::Forwarder;

/// How often the agent's listening sockets are read.
const POLL: Duration = Duration::from_secs(2);

/// Why a port is forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Found listening; released when it stops.
    Auto,
    /// Added by the user; kept (and restored) until stopped.
    User,
    /// `forwardPorts` of `devcontainer.json`.
    DevContainer,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto Forwarded",
            Self::User => "User Forwarded",
            Self::DevContainer => "devcontainer.json",
        }
    }
}

/// A port something listens on where the project lives, over all its
/// sockets (IPv4 and IPv6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    /// What the agent connects to: `localhost` when a socket listens on
    /// loopback or all addresses, else the one address it is bound to.
    pub connect_host: String,
    pub pid: Option<u32>,
    pub command: Option<String>,
}

/// Groups listening sockets by port.
pub fn group_listeners(ports: &[ListeningPort]) -> Vec<Listener> {
    let mut grouped: Vec<Listener> = Vec::new();
    for socket in ports {
        let listener = match grouped.iter_mut().find(|l| l.port == socket.port) {
            Some(listener) => listener,
            None => {
                grouped.push(Listener {
                    port: socket.port,
                    addresses: Vec::new(),
                    connect_host: String::new(),
                    pid: None,
                    command: None,
                });
                grouped.last_mut().expect("just pushed")
            }
        };
        listener.addresses.push(socket.address);
        listener.pid = listener.pid.or(socket.pid);
        if listener.command.is_none() {
            listener.command.clone_from(&socket.command);
        }
    }
    for listener in &mut grouped {
        let local = listener
            .addresses
            .iter()
            .any(|address| address.is_loopback() || address.is_unspecified());
        listener.connect_host = match listener.addresses.first() {
            Some(address) if !local => address.to_string(),
            _ => "localhost".into(),
        };
    }
    grouped.sort_by_key(|listener| listener.port);
    grouped
}

/// Debug adapters started by the editor listen for their debuggees, and
/// notebook kernels (their connection file `next_code-kernel-*.json` is on
/// their command line) for the editor's kernel bridge: those ports are not
/// the user's applications.
fn editor_owned(command: Option<&str>) -> bool {
    command.is_some_and(|command| {
        [
            "debugpy.adapter",
            "debugpy/adapter",
            "codelldb",
            "next_code_agent",
            "next_code-kernel-",
        ]
        .iter()
        .any(|marker| command.contains(marker))
    })
}

/// What a scan changes in automatic forwarding.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AutoChanges {
    /// Ports to forward now.
    pub forward: Vec<u16>,
    /// Automatically forwarded ports nothing listens on any more.
    pub release: Vec<u16>,
}

/// New listening ports to forward (not forwarded yet, not `skipped`, not
/// the editor's own, not `onAutoForward: ignore`) and automatically
/// forwarded ports to release. `forwarded` holds `(host, port, origin)`.
pub fn auto_changes(
    listeners: &[Listener],
    forwarded: &[(Option<&str>, u16, Origin)],
    skipped: &HashSet<u16>,
    config: &PortsConfig,
) -> AutoChanges {
    let forward = listeners
        .iter()
        .filter(|listener| {
            let port = listener.port;
            let command = listener.command.as_deref();
            !forwarded
                .iter()
                .any(|&(host, p, _)| host.is_none() && p == port)
                && !skipped.contains(&port)
                && !editor_owned(command)
                && config.attributes(None, port, command).on_auto_forward
                    != Some(OnAutoForward::Ignore)
        })
        .map(|listener| listener.port)
        .collect();
    let release = forwarded
        .iter()
        .filter(|&&(host, port, origin)| {
            origin == Origin::Auto
                && host.is_none()
                && !listeners.iter().any(|listener| listener.port == port)
        })
        .map(|&(_, port, _)| port)
        .collect();
    AutoChanges { forward, release }
}

/// A forwarded port.
pub struct Forwarded {
    pub target: ForwardPort,
    pub origin: Origin,
    pub attributes: PortAttributes,
    forwarder: Forwarder,
}

impl Forwarded {
    pub fn local_port(&self) -> u16 {
        self.forwarder.local_port()
    }

    /// `localhost:NNNN`.
    pub fn address(&self) -> String {
        format!("localhost:{}", self.local_port())
    }

    /// What Open in Browser opens (`portsAttributes` protocol, else http).
    pub fn url(&self) -> String {
        let scheme = match self.attributes.protocol {
            Some(PortProtocol::Https) => "https",
            _ => "http",
        };
        format!("{scheme}://localhost:{}", self.local_port())
    }

    pub fn connections(&self) -> usize {
        self.forwarder.connections()
    }
}

pub enum PortsEvent {
    /// Ports, listeners or connection counts changed.
    Changed,
    Forwarded {
        target: ForwardPort,
        local_port: u16,
        origin: Origin,
    },
    Released {
        target: ForwardPort,
    },
    /// An automatically forwarded port (or a `forwardPorts` one) started
    /// listening: tell the user as `action` says.
    Available {
        port: u16,
        url: String,
        label: Option<String>,
        action: OnAutoForward,
    },
}

/// A user-forwarded port, as saved in the project's state directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SavedPort {
    host: Option<String>,
    port: u16,
    local_port: u16,
}

pub struct Ports {
    project: Project,
    client: Option<Arc<AgentClient>>,
    config: PortsConfig,
    auto_forward: bool,
    forwarded: Vec<Forwarded>,
    listeners: Vec<Listener>,
    /// Listening ports not forwarded automatically: stopped by the user, or
    /// (WSL) already listening when the project opened. Forgotten once
    /// nothing listens on them.
    skipped: HashSet<u16>,
    /// Ports announced while they keep listening.
    announced: HashSet<u16>,
    /// Ports `openBrowserOnce` opened this session.
    opened_once: HashSet<u16>,
    scans: usize,
    /// The last scan's or forwarding's failure.
    error: Option<String>,
    changes: mpsc::UnboundedSender<()>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<PortsEvent> for Ports {}

impl Ports {
    /// Starts polling and forwards `forwardPorts` and the saved user ports
    /// (remote projects; local ones get an inert model).
    pub fn new(project: Project, settings: &PortSettings, cx: &mut Context<Self>) -> Self {
        let client = project.agent();
        let (changes, mut changed) = mpsc::unbounded::<()>();
        let mut tasks = Vec::new();
        if client.is_some() {
            // Connection counts change on tunnel threads.
            tasks.push(cx.spawn(async move |this, cx| {
                while changed.next().await.is_some() {
                    while changed.try_recv().is_ok() {}
                    if this
                        .update(cx, |_, cx| {
                            cx.emit(PortsEvent::Changed);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
            tasks.push(cx.spawn(async move |this, cx| {
                // Once subscribers can hear about them.
                if this
                    .update(cx, |this, cx| this.forward_initial(cx))
                    .is_err()
                {
                    return;
                }
                loop {
                    let Ok(scan) = this.update(cx, |this, cx| this.scan(cx)) else {
                        break;
                    };
                    scan.await;
                    cx.background_executor().timer(POLL).await;
                }
            }));
        }
        Self {
            config: project.ports_config(),
            project,
            client,
            auto_forward: settings.auto_forward,
            forwarded: Vec::new(),
            listeners: Vec::new(),
            skipped: HashSet::new(),
            announced: HashSet::new(),
            opened_once: HashSet::new(),
            scans: 0,
            error: None,
            changes,
            _tasks: tasks,
        }
    }

    /// `forwardPorts`, then the user ports saved for the project.
    fn forward_initial(&mut self, cx: &mut Context<Self>) {
        for target in self.config.forward.clone() {
            if let Err(err) = self.forward(target, Origin::DevContainer, None, cx) {
                self.error = Some(err);
            }
        }
        for saved in self.load_saved() {
            let target = ForwardPort {
                host: saved.host,
                port: saved.port,
            };
            // The same local port if it is free, else the next one.
            if let Err(err) = self.restore(target, saved.local_port, cx) {
                self.error = Some(err);
            }
        }
    }

    /// Whether the project is remote (only those forward ports).
    pub fn available(&self) -> bool {
        self.client.is_some()
    }

    pub fn forwarded(&self) -> &[Forwarded] {
        &self.forwarded
    }

    /// The last scan, by port.
    pub fn listeners(&self) -> &[Listener] {
        &self.listeners
    }

    pub fn listener(&self, port: u16) -> Option<&Listener> {
        self.listeners.iter().find(|listener| listener.port == port)
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn find(&self, target: &ForwardPort) -> Option<&Forwarded> {
        self.forwarded.iter().find(|f| &f.target == target)
    }

    /// Reads the listening sockets now and applies them (auto-forwarding);
    /// the task ends once applied.
    pub fn scan(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let Some(client) = self.client.clone() else {
            return Task::ready(());
        };
        cx.spawn(async move |this, cx| {
            let ports = cx
                .background_spawn(async move { client.listening_ports() })
                .await;
            this.update(cx, |this, cx| this.apply_scan(ports, cx)).ok();
        })
    }

    fn apply_scan(&mut self, ports: io::Result<Vec<ListeningPort>>, cx: &mut Context<Self>) {
        match ports {
            Ok(ports) => {
                self.listeners = group_listeners(&ports);
                if self.error.take().is_some() {
                    cx.notify();
                }
            }
            Err(err) => {
                self.error = Some(format!("Cannot list listening ports: {err}"));
                cx.notify();
                return;
            }
        }
        let listening: HashSet<u16> = self.listeners.iter().map(|l| l.port).collect();
        let first = self.scans == 0;
        self.scans += 1;
        self.skipped.retain(|port| listening.contains(port));
        self.announced.retain(|port| listening.contains(port));
        if first && !self.project.in_container() {
            // A distro's own services (and WSL's localhost forwarding of
            // them) are not new applications.
            self.skipped.extend(listening.iter().copied());
        }
        if self.auto_forward {
            let forwarded: Vec<(Option<&str>, u16, Origin)> = self
                .forwarded
                .iter()
                .map(|f| (f.target.host.as_deref(), f.target.port, f.origin))
                .collect();
            let changes = auto_changes(&self.listeners, &forwarded, &self.skipped, &self.config);
            for port in changes.release {
                self.remove(&ForwardPort { host: None, port }, cx);
            }
            for port in changes.forward {
                let target = ForwardPort { host: None, port };
                if let Err(err) = self.forward(target, Origin::Auto, None, cx) {
                    // Not retried every scan.
                    self.skipped.insert(port);
                    self.error = Some(err);
                }
            }
        }
        let mut available = Vec::new();
        for forwarded in &self.forwarded {
            let port = forwarded.target.port;
            if forwarded.origin == Origin::User
                || forwarded.target.host.is_some()
                || !listening.contains(&port)
                || !self.announced.insert(port)
            {
                continue;
            }
            let action = match forwarded.attributes.on_auto_forward {
                None | Some(OnAutoForward::Notify) => OnAutoForward::Notify,
                Some(OnAutoForward::OpenBrowser | OnAutoForward::OpenPreview) => {
                    OnAutoForward::OpenBrowser
                }
                Some(OnAutoForward::OpenBrowserOnce) if self.opened_once.insert(port) => {
                    OnAutoForward::OpenBrowser
                }
                Some(OnAutoForward::OpenBrowserOnce | OnAutoForward::Silent) => {
                    OnAutoForward::Silent
                }
                Some(OnAutoForward::Ignore) => continue,
            };
            available.push(PortsEvent::Available {
                port,
                url: forwarded.url(),
                label: forwarded.attributes.label.clone(),
                action,
            });
        }
        for event in available {
            cx.emit(event);
        }
        cx.emit(PortsEvent::Changed);
        cx.notify();
    }

    /// Forwards `target` (already forwarded: its local port, a user request
    /// making it a user port). `local_port` is required when given;
    /// otherwise the same port is preferred, else the next free one.
    pub fn forward(
        &mut self,
        target: ForwardPort,
        origin: Origin,
        local_port: Option<u16>,
        cx: &mut Context<Self>,
    ) -> Result<u16, String> {
        if let Some(existing) = self.forwarded.iter_mut().find(|f| f.target == target) {
            let local = existing.local_port();
            if origin == Origin::User && existing.origin == Origin::Auto {
                existing.origin = Origin::User;
                self.save();
                cx.notify();
            }
            return Ok(local);
        }
        let require = local_port.is_some();
        self.start(target, origin, local_port.unwrap_or(0), require, cx)
    }

    /// A saved port: its old local port when free.
    fn restore(
        &mut self,
        target: ForwardPort,
        local_port: u16,
        cx: &mut Context<Self>,
    ) -> Result<u16, String> {
        if let Some(existing) = self.forwarded.iter_mut().find(|f| f.target == target) {
            existing.origin = Origin::User;
            return Ok(existing.local_port());
        }
        self.start(target, Origin::User, local_port, false, cx)
    }

    /// `preferred` 0 means the target's port number.
    fn start(
        &mut self,
        target: ForwardPort,
        origin: Origin,
        preferred: u16,
        require: bool,
        cx: &mut Context<Self>,
    ) -> Result<u16, String> {
        let client = self
            .client
            .clone()
            .ok_or("Port forwarding is for WSL and container projects")?;
        let listener = self.listener(target.port).filter(|_| target.host.is_none());
        let command = listener.and_then(|l| l.command.clone());
        let attributes =
            self.config
                .attributes(target.host.as_deref(), target.port, command.as_deref());
        let connect_host = match &target.host {
            Some(host) => host.clone(),
            None => listener.map_or_else(|| "localhost".into(), |l| l.connect_host.clone()),
        };
        let preferred = if preferred == 0 {
            target.port
        } else {
            preferred
        };
        let require = require || attributes.require_local_port == Some(true);
        let local = forwarder::bind_local(preferred, require)
            .map_err(|err| format!("Cannot forward {target}: {err}"))?;
        let changes = self.changes.clone();
        let forwarder = Forwarder::start(client, connect_host, target.port, local, move || {
            changes.unbounded_send(()).ok();
        })
        .map_err(|err| format!("Cannot forward {target}: {err}"))?;
        let local_port = forwarder.local_port();
        self.forwarded.push(Forwarded {
            target: target.clone(),
            origin,
            attributes,
            forwarder,
        });
        self.forwarded
            .sort_by(|a, b| (a.target.port, &a.target.host).cmp(&(b.target.port, &b.target.host)));
        if origin == Origin::User {
            self.save();
        }
        cx.emit(PortsEvent::Forwarded {
            target,
            local_port,
            origin,
        });
        cx.emit(PortsEvent::Changed);
        cx.notify();
        Ok(local_port)
    }

    /// Stops forwarding at the user's request: an automatic port stays
    /// unforwarded while it keeps listening. Whether it was forwarded.
    pub fn stop(&mut self, target: &ForwardPort, cx: &mut Context<Self>) -> bool {
        let Some(forwarded) = self.remove(target, cx) else {
            return false;
        };
        if target.host.is_none() && self.listener(target.port).is_some() {
            self.skipped.insert(target.port);
        }
        if forwarded.origin == Origin::User {
            self.save();
        }
        true
    }

    /// Moves a forwarded port to local port `local_port` (which must be
    /// free); its open connections close.
    pub fn change_local_port(
        &mut self,
        target: &ForwardPort,
        local_port: u16,
        cx: &mut Context<Self>,
    ) -> Result<u16, String> {
        let Some(index) = self.forwarded.iter().position(|f| &f.target == target) else {
            return Err(format!("{target} is not forwarded"));
        };
        if self.forwarded[index].local_port() == local_port {
            return Ok(local_port);
        }
        let client = self.client.clone().ok_or("not a remote project")?;
        let listener = forwarder::bind_local(local_port, true).map_err(|err| err.to_string())?;
        let connect_host = match &target.host {
            Some(host) => host.clone(),
            None => self
                .listener(target.port)
                .map_or_else(|| "localhost".into(), |l| l.connect_host.clone()),
        };
        let changes = self.changes.clone();
        let forwarder = Forwarder::start(client, connect_host, target.port, listener, move || {
            changes.unbounded_send(()).ok();
        })
        .map_err(|err| err.to_string())?;
        let forwarded = &mut self.forwarded[index];
        // The old listener and its connections close here.
        forwarded.forwarder = forwarder;
        let origin = forwarded.origin;
        if origin == Origin::User {
            self.save();
        }
        cx.emit(PortsEvent::Forwarded {
            target: target.clone(),
            local_port,
            origin,
        });
        cx.emit(PortsEvent::Changed);
        cx.notify();
        Ok(local_port)
    }

    fn remove(&mut self, target: &ForwardPort, cx: &mut Context<Self>) -> Option<Forwarded> {
        let index = self.forwarded.iter().position(|f| &f.target == target)?;
        // Dropping it closes the listener and its connections.
        let forwarded = self.forwarded.remove(index);
        if target.host.is_none() {
            self.announced.remove(&target.port);
        }
        cx.emit(PortsEvent::Released {
            target: target.clone(),
        });
        cx.emit(PortsEvent::Changed);
        cx.notify();
        Some(forwarded)
    }

    fn saved_path(&self) -> Option<PathBuf> {
        Some(layout::project_state_dir(&self.project.state_key())?.join("ports.json"))
    }

    fn load_saved(&self) -> Vec<SavedPort> {
        self.saved_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persists the user ports; failures only cost the restore.
    fn save(&self) {
        let saved: Vec<SavedPort> = self
            .forwarded
            .iter()
            .filter(|f| f.origin == Origin::User)
            .map(|f| SavedPort {
                host: f.target.host.clone(),
                port: f.target.port,
                local_port: f.local_port(),
            })
            .collect();
        let Some(path) = self.saved_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(json) = serde_json::to_string_pretty(&saved) {
            std::fs::write(path, json).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcontainer::Config;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn socket(address: IpAddr, port: u16, command: Option<&str>) -> ListeningPort {
        ListeningPort {
            address,
            port,
            pid: command.map(|_| 42),
            command: command.map(str::to_owned),
        }
    }

    const ANY4: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    const ANY6: IpAddr = IpAddr::V6(Ipv6Addr::UNSPECIFIED);

    #[test]
    fn listeners_group_families_and_pick_where_to_connect() {
        let listeners = group_listeners(&[
            socket(ANY6, 8000, None),
            socket(ANY4, 8000, Some("python3 -m http.server")),
            socket("172.17.0.2".parse().unwrap(), 5000, Some("flask")),
            socket(IpAddr::V6(Ipv6Addr::LOCALHOST), 3000, Some("node")),
        ]);
        let summary: Vec<(u16, &str, Option<&str>)> = listeners
            .iter()
            .map(|l| (l.port, l.connect_host.as_str(), l.command.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                (3000, "localhost", Some("node")),
                (5000, "172.17.0.2", Some("flask")),
                (8000, "localhost", Some("python3 -m http.server")),
            ]
        );
        assert_eq!(listeners[2].addresses.len(), 2);
    }

    #[test]
    fn new_ports_are_forwarded_and_gone_automatic_ones_released() {
        let config = Config::parse(
            r#"{ "image": "x", "portsAttributes": {
                "9229": { "onAutoForward": "ignore" },
                "^redis": { "onAutoForward": "ignore" } } }"#,
        )
        .unwrap()
        .ports();
        let listeners = group_listeners(&[
            socket(ANY4, 8000, Some("python3 -m http.server")),
            socket(ANY4, 9229, Some("node --inspect")),
            socket(ANY4, 6379, Some("redis-server *:6379")),
            socket(ANY4, 5678, Some("python -m debugpy.adapter")),
            socket(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                41234,
                Some(
                    "/p/.venv/bin/python -m ipykernel_launcher -f /tmp/next_code-kernel-ab12.json",
                ),
            ),
            socket(ANY4, 3000, Some("node")),
            socket(ANY4, 4000, None),
            socket(ANY4, 5432, None),
        ]);
        let skipped: HashSet<u16> = [4000].into();
        let forwarded = [
            (None, 3000, Origin::Auto),
            (None, 7000, Origin::Auto),
            (None, 7001, Origin::User),
            (None, 7002, Origin::DevContainer),
            (Some("db"), 5432, Origin::DevContainer),
        ];
        let changes = auto_changes(&listeners, &forwarded, &skipped, &config);
        assert_eq!(
            changes,
            AutoChanges {
                // 5432 listens in the container itself: `db:5432` is
                // another host's port.
                forward: vec![5432, 8000],
                release: vec![7000],
            }
        );
        // Nothing listening: only automatic ports go.
        assert_eq!(
            auto_changes(&[], &forwarded, &HashSet::new(), &config),
            AutoChanges {
                forward: vec![],
                release: vec![3000, 7000],
            }
        );
    }
}
