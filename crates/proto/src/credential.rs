//! The git credential helper protocol (`git help credential`): `key=value`
//! lines ended by a blank line or the end of input. The agent relays a
//! helper call's text to the editor, which runs `git credential` on its own
//! machine; both sides keep only [`KEYS`], so neither end can smuggle other
//! instructions (`quit`, capability negotiation) through the other.

use serde::{Deserialize, Serialize};

/// The longest credential text relayed.
pub const MAX_TEXT: usize = 64 * 1024;

/// Set for the editor's own `git credential` runs: a forwarding helper
/// configured on the editor's machine too (a WSL editor's own projects)
/// then answers nothing instead of forwarding back to an editor.
pub const NO_FORWARDING_ENV: &str = "NEXT_CODE_NO_CREDENTIAL_FORWARDING";

/// Keys relayed; others are dropped.
pub const KEYS: &[&str] = &[
    "protocol",
    "host",
    "path",
    "url",
    "username",
    "password",
    "password_expiry_utc",
    "oauth_refresh_token",
    "wwwauth[]",
];

/// What git asks of a credential helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Get,
    Store,
    Erase,
}

impl Operation {
    /// The helper argument git passes (`get`, `store`, `erase`).
    pub fn parse(argument: &str) -> Option<Self> {
        match argument {
            "get" => Some(Self::Get),
            "store" => Some(Self::Store),
            "erase" => Some(Self::Erase),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Store => "store",
            Self::Erase => "erase",
        }
    }

    /// The `git credential` subcommand that does the same through the
    /// user's own helpers.
    pub fn git_command(self) -> &'static str {
        match self {
            Self::Get => "fill",
            Self::Store => "approve",
            Self::Erase => "reject",
        }
    }
}

/// A credential's attributes in their order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credential {
    pub fields: Vec<(String, String)>,
}

impl Credential {
    /// Reads `key=value` lines up to a blank line or the end. Lines without
    /// `=`, keys outside [`KEYS`] and values holding NUL are dropped.
    pub fn parse(text: &str) -> Self {
        let fields = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .take_while(|line| !line.is_empty())
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                (KEYS.contains(&key) && !value.contains('\0'))
                    .then(|| (key.to_owned(), value.to_owned()))
            })
            .collect();
        Self { fields }
    }

    /// The last value of `key` (git lets later lines win).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The protocol text: one line per field (no blank line: the end of
    /// input ends it).
    pub fn to_text(&self) -> String {
        self.fields
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_keys_up_to_the_blank_line() {
        let credential = Credential::parse(
            "protocol=https\nhost=example.com:8443\r\ncapability[]=authtype\nnot a field\n\
             wwwauth[]=Basic realm=\"git\"\nusername=ada\npassword=p=ss\0\n\
             path=org/repo.git\n\nhost=after-the-end\n",
        );
        assert_eq!(
            credential.fields,
            [
                ("protocol".to_owned(), "https".to_owned()),
                ("host".to_owned(), "example.com:8443".to_owned()),
                ("wwwauth[]".to_owned(), "Basic realm=\"git\"".to_owned()),
                ("username".to_owned(), "ada".to_owned()),
                ("path".to_owned(), "org/repo.git".to_owned()),
            ]
        );
        assert_eq!(credential.get("host"), Some("example.com:8443"));
        assert_eq!(credential.get("password"), None);
    }

    #[test]
    fn serialises_line_per_field_and_round_trips() {
        let text = "protocol=https\nhost=h\nusername=u\npassword=a=b c\n";
        let credential = Credential::parse(text);
        assert_eq!(credential.to_text(), text);
        assert_eq!(credential.get("password"), Some("a=b c"));
        // Later lines win, as in git.
        assert_eq!(Credential::parse("host=a\nhost=b").get("host"), Some("b"));
        assert!(Credential::parse("\nhost=a\n").is_empty());
    }

    #[test]
    fn operations_map_to_git_credential_subcommands() {
        for (argument, command) in [("get", "fill"), ("store", "approve"), ("erase", "reject")] {
            let operation = Operation::parse(argument).unwrap();
            assert_eq!(operation.as_str(), argument);
            assert_eq!(operation.git_command(), command);
        }
        assert_eq!(Operation::parse("capabilities"), None);
    }
}
