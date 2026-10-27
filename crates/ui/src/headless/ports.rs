//! Headless port forwarding: `ports_list`, `ports_forward`, `ports_stop`,
//! and the `port_forwarded` / `port_released` / `port_available` events.
//! Same [`Ports`] model as the Ports panel (auto-forwarding included).

use devcontainer::ports::{ForwardPort, OnAutoForward};

use super::*;
use crate::ports::{Origin, Ports, PortsEvent};

/// `port` (a number, or `"host:port"`) and `host`?.
fn target(request: &Value) -> Result<ForwardPort, String> {
    let host = request["host"].as_str();
    let text = match &request["port"] {
        Value::Number(port) => port.to_string(),
        Value::String(text) => text.clone(),
        _ => return Err("missing `port`".into()),
    };
    let text = match host {
        Some(host) => format!("{host}:{text}"),
        None => text,
    };
    ForwardPort::parse(&text).ok_or_else(|| format!("not a port: `{text}`"))
}

fn action_name(action: OnAutoForward) -> &'static str {
    match action {
        OnAutoForward::Notify => "notify",
        OnAutoForward::OpenBrowser => "openBrowser",
        OnAutoForward::OpenBrowserOnce => "openBrowserOnce",
        OnAutoForward::OpenPreview => "openPreview",
        OnAutoForward::Silent => "silent",
        OnAutoForward::Ignore => "ignore",
    }
}

fn list_json(ports: &Ports) -> Value {
    let listening: Vec<Value> = ports
        .listeners()
        .iter()
        .map(|listener| {
            json!({
                "port": listener.port,
                "addresses": listener.addresses,
                "pid": listener.pid,
                "command": listener.command,
            })
        })
        .collect();
    let forwarded: Vec<Value> = ports
        .forwarded()
        .iter()
        .map(|forwarded| {
            let process = forwarded
                .target
                .host
                .is_none()
                .then(|| ports.listener(forwarded.target.port))
                .flatten()
                .and_then(|listener| listener.command.clone());
            json!({
                "port": forwarded.target.port,
                "host": forwarded.target.host,
                "local_port": forwarded.local_port(),
                "address": forwarded.address(),
                "url": forwarded.url(),
                "origin": forwarded.origin,
                "label": forwarded.attributes.label,
                "connections": forwarded.connections(),
                "process": process,
            })
        })
        .collect();
    json!({
        "available": ports.available(),
        "listening": listening,
        "forwarded": forwarded,
        "error": ports.error(),
    })
}

impl Session {
    pub(super) fn on_ports_event(&self, event: &PortsEvent) {
        match event {
            PortsEvent::Forwarded {
                target,
                local_port,
                origin,
            } => self.out.event(
                "port_forwarded",
                json!({
                    "port": target.port,
                    "host": target.host,
                    "local_port": local_port,
                    "origin": origin,
                }),
            ),
            PortsEvent::Released { target } => self.out.event(
                "port_released",
                json!({ "port": target.port, "host": target.host }),
            ),
            PortsEvent::Available {
                port,
                url,
                label,
                action,
            } => self.out.event(
                "port_available",
                json!({
                    "port": port,
                    "url": url,
                    "label": label,
                    "action": action_name(*action),
                }),
            ),
            PortsEvent::Changed => {}
        }
    }

    /// `ports_*` commands.
    pub(super) fn ports(
        &mut self,
        id: Value,
        command: &str,
        request: &Value,
        cx: &mut Context<Self>,
    ) {
        match command {
            "ports_list" => {
                // A fresh scan, so a server started just before is listed.
                let scan = self.ports.update(cx, |ports, cx| ports.scan(cx));
                let (out, ports) = (self.out.clone(), self.ports.clone());
                cx.spawn(async move |_, cx| {
                    scan.await;
                    let result = ports
                        .read_with(cx, |ports, _| list_json(ports))
                        .map_err(|err| err.to_string());
                    out.reply(&id, result);
                })
                .detach();
            }
            "ports_forward" => {
                let local_port = request["local_port"]
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok());
                let result = target(request).and_then(|target| {
                    self.ports.update(cx, |ports, cx| {
                        let local = ports.forward(target.clone(), Origin::User, local_port, cx)?;
                        let url = ports.find(&target).map(|f| f.url());
                        Ok(json!({ "local_port": local, "url": url }))
                    })
                });
                self.out.reply(&id, result);
            }
            "ports_stop" => {
                let result = target(request).and_then(|target| {
                    if self.ports.update(cx, |ports, cx| ports.stop(&target, cx)) {
                        Ok(Value::Null)
                    } else {
                        Err(format!("{target} is not forwarded"))
                    }
                });
                self.out.reply(&id, result);
            }
            other => self
                .out
                .reply(&id, Err(format!("unknown command `{other}`"))),
        }
    }
}
