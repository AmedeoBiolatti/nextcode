//! Listening TCP sockets, from `/proc/net/tcp` and `/proc/net/tcp6`, with
//! their owning processes (found through `/proc/<pid>/fd` socket inodes,
//! where the agent may read them).

// The parsers are only called on Linux (tested everywhere).
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use proto::ListeningPort;

/// `st` of a listening socket in `/proc/net/tcp`.
const LISTEN: &str = "0A";

/// One listening socket of a `/proc/net/tcp{,6}` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Socket {
    pub address: IpAddr,
    pub port: u16,
    pub inode: u64,
}

/// The listening sockets of a table (after its header line); other
/// states and malformed lines are skipped.
pub(crate) fn parse_table(text: &str) -> Vec<Socket> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
            let fields: Vec<&str> = line.split_whitespace().collect();
            if *fields.get(3)? != LISTEN {
                return None;
            }
            let (address, port) = fields.get(1)?.split_once(':')?;
            Some(Socket {
                address: parse_address(address)?,
                port: u16::from_str_radix(port, 16).ok()?,
                inode: fields.get(9)?.parse().ok()?,
            })
        })
        .collect()
}

/// The kernel prints an address as 32-bit words in host byte order (so
/// 127.0.0.1 is `0100007F` on little-endian machines).
fn parse_address(hex: &str) -> Option<IpAddr> {
    let word = |index: usize| -> Option<[u8; 4]> {
        let text = hex.get(index * 8..index * 8 + 8)?;
        Some(u32::from_str_radix(text, 16).ok()?.to_ne_bytes())
    };
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(word(0)?))),
        32 => {
            let mut bytes = [0u8; 16];
            for index in 0..4 {
                bytes[index * 4..index * 4 + 4].copy_from_slice(&word(index)?);
            }
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

/// What a process's `cmdline` says, arguments separated by spaces.
pub(crate) fn command_line(cmdline: &[u8]) -> Option<String> {
    let text = cmdline
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// Every listening TCP socket, sorted by port then address.
#[cfg(target_os = "linux")]
pub(crate) fn listening() -> Vec<ListeningPort> {
    use std::{
        collections::{HashMap, HashSet},
        fs,
    };

    let mut sockets: Vec<Socket> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|table| fs::read_to_string(table).ok())
        .flat_map(|text| parse_table(&text))
        .collect();
    sockets.sort_by_key(|socket| (socket.port, socket.address));
    sockets.dedup_by_key(|socket| (socket.port, socket.address));

    let wanted: HashSet<u64> = sockets
        .iter()
        .map(|socket| socket.inode)
        .filter(|&inode| inode != 0)
        .collect();
    let mut owners: HashMap<u64, u32> = HashMap::new();
    if !wanted.is_empty()
        && let Ok(processes) = fs::read_dir("/proc")
    {
        for process in processes.flatten() {
            let Some(pid) = process
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            // Other users' descriptors are unreadable: those stay unowned.
            let Ok(fds) = fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(target) = fs::read_link(fd.path()) else {
                    continue;
                };
                let inode = target
                    .to_str()
                    .and_then(|target| target.strip_prefix("socket:["))
                    .and_then(|rest| rest.strip_suffix(']'))
                    .and_then(|inode| inode.parse::<u64>().ok());
                if let Some(inode) = inode.filter(|inode| wanted.contains(inode)) {
                    owners.entry(inode).or_insert(pid);
                }
            }
            if owners.len() == wanted.len() {
                break;
            }
        }
    }
    let mut commands: HashMap<u32, Option<String>> = HashMap::new();
    sockets
        .into_iter()
        .map(|socket| {
            let pid = owners.get(&socket.inode).copied();
            let command = pid.and_then(|pid| {
                commands
                    .entry(pid)
                    .or_insert_with(|| {
                        fs::read(format!("/proc/{pid}/cmdline"))
                            .ok()
                            .and_then(|cmdline| command_line(&cmdline))
                            .or_else(|| {
                                fs::read_to_string(format!("/proc/{pid}/comm"))
                                    .ok()
                                    .map(|comm| comm.trim().to_owned())
                            })
                    })
                    .clone()
            });
            ListeningPort {
                address: socket.address,
                port: socket.port,
                pid,
                command,
            }
        })
        .collect()
}

/// Agents run on Linux; elsewhere (tests on Windows) nothing is listed.
#[cfg(not(target_os = "linux"))]
pub(crate) fn listening() -> Vec<ListeningPort> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_endian = "little")]
    fn tables_list_listening_sockets_with_their_addresses_and_inodes() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1777 1 0000000000000000 100 0 0 10 0
   2: 0100007F:1F40 0100007F:D2F0 01 00000000:00000000 00:00000000 00000000  1000        0 52000 1 0000000000000000 20 4 30 10 -1
   3: 0E02A8C0:C350 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 9 1 0000000000000000 100 0 0 10 0
   bad line
";
        assert_eq!(
            parse_table(tcp),
            [
                Socket {
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 8000,
                    inode: 41234,
                },
                Socket {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 22,
                    inode: 1777,
                },
                Socket {
                    address: IpAddr::V4(Ipv4Addr::new(192, 168, 2, 14)),
                    port: 50000,
                    inode: 9,
                },
            ]
        );

        let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 778 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:1389 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 779 1 0000000000000000 100 0 0 10 0
   3: 00000000000000000000000001000000:0BB8 00000000000000000000000001000000:9C40 06 00000000:00000000 00:00000000 00000000  1000        0 0 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(
            parse_table(tcp6),
            [
                Socket {
                    address: IpAddr::V6(Ipv6Addr::LOCALHOST),
                    port: 3000,
                    inode: 777,
                },
                Socket {
                    address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    port: 8080,
                    inode: 778,
                },
                Socket {
                    address: "::ffff:127.0.0.1".parse().unwrap(),
                    port: 5001,
                    inode: 779,
                },
            ]
        );
    }

    #[test]
    fn command_lines_join_arguments() {
        assert_eq!(
            command_line(b"python3\0-m\0http.server\08000\0").as_deref(),
            Some("python3 -m http.server 8000")
        );
        assert_eq!(command_line(b""), None);
    }

    /// The agent's own listener shows up, with this process as its owner.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_listener_of_this_process_is_found_with_its_owner() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = listening();
        let ours = found
            .iter()
            .find(|entry| entry.port == port)
            .unwrap_or_else(|| panic!("{port} not in {found:?}"));
        assert_eq!(ours.address, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(ours.pid, Some(std::process::id()));
        assert!(ours.command.is_some());
    }
}
