use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshDestination {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}
impl SshDestination {
    pub fn parse(input: &str) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "enter one SSH destination without options or commands",
            )
        };
        if input.chars().any(char::is_control) {
            return Err(invalid());
        }
        let input = input.trim();
        let input = input.strip_prefix("ssh ").map(str::trim).unwrap_or(input);
        let (authority, uri) = input
            .strip_prefix("ssh://")
            .map(|s| (s, true))
            .unwrap_or((input, false));
        if authority.is_empty()
            || authority.chars().any(char::is_whitespace)
            || authority
                .bytes()
                .any(|b| matches!(b, b'/' | b'?' | b'#' | b'%' | b'\\'))
        {
            return Err(invalid());
        }
        let (user, hostport) = if let Some((user, host)) = authority.split_once('@') {
            if !valid_name(user) || host.contains('@') {
                return Err(invalid());
            }
            (Some(user.to_owned()), host)
        } else {
            (None, authority)
        };
        let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
            let (host, tail) = bracketed.split_once(']').ok_or_else(invalid)?;
            host.parse::<std::net::Ipv6Addr>().map_err(|_| invalid())?;
            let port = if tail.is_empty() {
                None
            } else if uri {
                Some(parse_port(tail.strip_prefix(':').ok_or_else(invalid)?)?)
            } else {
                return Err(invalid());
            };
            (host, port)
        } else if uri && hostport.matches(':').count() == 1 {
            let (host, port) = hostport.split_once(':').ok_or_else(invalid)?;
            (host, Some(parse_port(port)?))
        } else {
            (hostport, None)
        };
        if !(valid_name(host) || host.parse::<std::net::Ipv6Addr>().is_ok()) {
            return Err(invalid());
        }
        Ok(Self {
            host: host.to_owned(),
            user,
            port,
        })
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        if !(valid_name(&self.host) || self.host.parse::<std::net::Ipv6Addr>().is_ok())
            || self.user.as_ref().is_some_and(|u| !valid_name(u))
            || self.port == Some(0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid SSH destination",
            ));
        }
        Ok(())
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}
fn parse_port(value: &str) -> io::Result<u16> {
    value
        .parse::<u16>()
        .ok()
        .filter(|n| *n > 0)
        .filter(|_| value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid SSH port"))
}
