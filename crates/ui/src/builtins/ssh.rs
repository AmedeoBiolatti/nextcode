//! The Remote - SSH built-in extension: projects on machines reached with
//! OpenSSH's `ssh` (`ssh://[user@]host[:port]/path`), the agent installed
//! and started there as in WSL (`remote::ssh`). Connect to SSH Host… opens
//! Open Recent at `ssh://`, which suggests the hosts of `~/.ssh/config`
//! and the tailnet's Linux machines (`remote::tailscale`), which Tailscale
//! SSH signs into without keys. On by default. Off, `ssh://` locations do
//! not open; projects already open stay connected.

use std::io;

use gpui::{Context, Div, prelude::*};

use super::{Builtin, Connector, Entry, Machine, MenuItem};
use crate::shell::Shell;

gpui::actions!(ssh, [ConnectToHost]);

pub const BUILTIN: Builtin = Builtin {
    id: "ssh",
    name: "Remote - SSH",
    description: "Open folders on machines you reach with ssh, Tailscale machines included \
                  (ssh://host/path).",
    default_enabled: true,
    labels: &[("ssh::ConnectToHost", "Connect to SSH Host…")],
    menu_items: &[MenuItem {
        menu: "File",
        after: "app::OpenRecent",
        items: &[Entry::Command("ssh::ConnectToHost")],
    }],
    window_actions,
    connectors: &[Connector {
        scheme: "ssh",
        describe: remote::ssh::describe,
        connect,
        machines,
    }],
    ..Builtin::NONE
};

fn window_actions(element: Div, cx: &mut Context<Shell>) -> Div {
    element.on_action(cx.listener(|shell, _: &ConnectToHost, window, cx| {
        shell.show_project_picker("ssh://", window, cx)
    }))
}

fn connect(
    host: &str,
    path: &str,
    progress: &mut dyn FnMut(&str),
) -> io::Result<remote::Connection> {
    remote::ssh::connect(&remote::ssh::Host::parse(host)?, path, progress)
}

/// The hosts of `~/.ssh/config`, then the tailnet's Linux machines (the
/// agent runs on Linux) that are not among them.
fn machines() -> Vec<Machine> {
    let mut machines: Vec<Machine> = remote::ssh::configured_hosts()
        .into_iter()
        .map(|host| Machine {
            host,
            detail: "~/.ssh/config".into(),
        })
        .collect();
    // Without Tailscale (not installed, not connected) there are none.
    let peers = remote::tailscale::peers().unwrap_or_default();
    for peer in peers.into_iter().filter(|peer| peer.os == "linux") {
        if machines.iter().any(|machine| machine.host == peer.address) {
            continue;
        }
        machines.push(Machine {
            detail: tailscale_detail(&peer),
            host: peer.address,
        });
    }
    machines
}

fn tailscale_detail(peer: &remote::tailscale::Peer) -> String {
    let mut detail = format!(
        "{} · Tailscale · {}",
        peer.name,
        if peer.online { "online" } else { "offline" }
    );
    if peer.tailscale_ssh {
        detail.push_str(" · Tailscale SSH");
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Location;

    #[test]
    fn ssh_locations_are_typed_as_urls_and_shown_the_same_way() {
        let remote = |host: &str, path: &str| Location::Remote {
            scheme: "ssh".into(),
            host: host.into(),
            path: path.into(),
        };
        for (typed, host, path, shown) in [
            ("ssh://box", "box", "~", "ssh://box/~"),
            ("ssh://box/~", "box", "~", "ssh://box/~"),
            ("ssh://box/~/", "box", "~", "ssh://box/~"),
            (
                "ssh://me@box:2222/~/src/p/",
                "me@box:2222",
                "~/src/p",
                "ssh://me@box:2222/~/src/p",
            ),
            ("ssh://box/", "box", "/", "ssh://box/"),
            ("ssh://box/srv/p", "box", "/srv/p", "ssh://box/srv/p"),
            (
                "ssh://[fd7a::1]:22/srv",
                "[fd7a::1]:22",
                "/srv",
                "ssh://[fd7a::1]:22/srv",
            ),
        ] {
            let location = Location::parse(typed).unwrap();
            assert_eq!(location, remote(host, path), "{typed}");
            assert_eq!(location.to_string(), shown);
            assert_eq!(Location::parse(shown), Some(location));
        }
        // Not a connector's scheme, or no host: what it was before.
        assert!(matches!(
            Location::parse("ftp://box/p"),
            Some(Location::Wsl { .. })
        ));
        assert_eq!(Location::parse("ssh:///p"), None);
        assert_eq!(Location::parse("ssh://"), None);
        assert!(remote("box", "~/p").posix());
        assert_eq!(super::super::connector("ssh").unwrap().0.id, "ssh");
    }

    #[test]
    fn tailscale_machines_say_where_they_come_from() {
        let peer = remote::tailscale::Peer {
            name: "box".into(),
            address: "box.tail1.ts.net".into(),
            os: "linux".into(),
            online: true,
            tailscale_ssh: true,
        };
        assert_eq!(
            tailscale_detail(&peer),
            "box · Tailscale · online · Tailscale SSH"
        );
    }
}
