//! Shared bind-address resolution for `EdgeZero` dev servers.
//!
//! Centralises the precedence logic (env vars > config > defaults) so that
//! both the Axum adapter and the CLI dev server produce consistent results.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Default bind host: localhost (`127.0.0.1`).
pub const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// Default bind port (`8787`).
pub const DEFAULT_PORT: u16 = 8787;

/// A resolved bind address plus any warnings emitted while falling back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindAddrResolution {
    pub addr: SocketAddr,
    pub warnings: Vec<String>,
}

/// Resolve a bind address from optional environment and config values.
///
/// Precedence (highest wins):
/// 1. `env_host` / `env_port` (typically `EDGEZERO_HOST` / `EDGEZERO_PORT`)
/// 2. `config_host` / `config_port` (from manifest or adapter config)
/// 3. Defaults: `127.0.0.1:8787`
///
/// Invalid values produce warnings and fall back to the next precedence level.
#[inline]
#[must_use]
pub fn resolve_bind_addr(
    env_host: Option<&str>,
    env_port: Option<&str>,
    config_host: Option<&str>,
    config_port: Option<u16>,
) -> BindAddrResolution {
    let mut warnings = Vec::new();
    let host = resolve_host(env_host, config_host, &mut warnings);
    let port = resolve_port(env_port, config_port, &mut warnings);

    BindAddrResolution {
        addr: SocketAddr::from((host, port)),
        warnings,
    }
}

fn resolve_host(
    env_host: Option<&str>,
    config_host: Option<&str>,
    warnings: &mut Vec<String>,
) -> IpAddr {
    if let Some(value) = env_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "EDGEZERO_HOST={value:?} is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    if let Some(value) = config_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "configured host={value:?} is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    DEFAULT_HOST
}

fn resolve_port(
    env_port: Option<&str>,
    config_port: Option<u16>,
    warnings: &mut Vec<String>,
) -> u16 {
    if let Some(value) = env_port {
        match value.parse::<u16>() {
            Ok(0) => warnings.push(
                "EDGEZERO_PORT=\"0\" is not supported (would bind to a random OS port); falling back"
                    .to_owned(),
            ),
            Ok(port) => return port,
            Err(_) => warnings.push(format!(
                "EDGEZERO_PORT={value:?} is not a valid port number; falling back"
            )),
        }
    }

    match config_port {
        Some(0) => warnings.push(
            "configured port=0 is not supported (would bind to a random OS port); falling back"
                .to_owned(),
        ),
        Some(port) => return port,
        None => {}
    }

    DEFAULT_PORT
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn defaults_when_nothing_provided() {
        let resolution = resolve_bind_addr(None, None, None, None);
        assert_eq!(resolution.addr, SocketAddr::from(([127, 0, 0, 1], 8787)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn config_overrides_defaults() {
        let resolution = resolve_bind_addr(None, None, Some("0.0.0.0"), Some(3000));
        assert_eq!(resolution.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(resolution.addr.port(), 3000);
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn env_overrides_config() {
        let resolution =
            resolve_bind_addr(Some("0.0.0.0"), Some("4000"), Some("127.0.0.1"), Some(3000));
        assert_eq!(resolution.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(resolution.addr.port(), 4000);
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn partial_env_override_host_only() {
        let resolution = resolve_bind_addr(Some("0.0.0.0"), None, None, Some(5000));
        assert_eq!(resolution.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(resolution.addr.port(), 5000);
    }

    #[test]
    fn partial_env_override_port_only() {
        let resolution = resolve_bind_addr(None, Some("9000"), Some("0.0.0.0"), None);
        assert_eq!(resolution.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(resolution.addr.port(), 9000);
    }

    #[test]
    fn invalid_env_host_falls_back_to_config() {
        let resolution = resolve_bind_addr(Some("not-an-ip"), None, Some("0.0.0.0"), None);
        assert_eq!(resolution.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("EDGEZERO_HOST"));
        assert!(resolution.warnings[0].contains("not a valid IP address"));
    }

    #[test]
    fn invalid_env_port_falls_back_to_config() {
        let resolution = resolve_bind_addr(None, Some("abc"), None, Some(3000));
        assert_eq!(resolution.addr.port(), 3000);
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("EDGEZERO_PORT"));
        assert!(resolution.warnings[0].contains("not a valid port number"));
    }

    #[test]
    fn invalid_config_host_falls_back_to_default() {
        let resolution = resolve_bind_addr(None, None, Some("not-an-ip"), None);
        assert_eq!(resolution.addr.ip(), DEFAULT_HOST);
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("configured host"));
    }

    #[test]
    fn port_zero_from_env_falls_back_to_config() {
        let resolution = resolve_bind_addr(None, Some("0"), None, Some(3000));
        assert_eq!(resolution.addr.port(), 3000);
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("port); falling back"));
    }

    #[test]
    fn port_zero_from_config_falls_back_to_default() {
        let resolution = resolve_bind_addr(None, None, None, Some(0));
        assert_eq!(resolution.addr.port(), DEFAULT_PORT);
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("configured port=0"));
    }

    #[test]
    fn invalid_env_and_config_port_fall_back_to_default() {
        let resolution = resolve_bind_addr(None, Some("abc"), None, Some(0));
        assert_eq!(resolution.addr.port(), DEFAULT_PORT);
        assert_eq!(resolution.warnings.len(), 2);
    }

    #[test]
    fn ipv6_host_from_env() {
        let resolution = resolve_bind_addr(Some("::1"), None, None, None);
        assert_eq!(resolution.addr.ip(), "::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn ipv6_host_from_config() {
        let resolution = resolve_bind_addr(None, None, Some("::"), Some(3000));
        assert_eq!(resolution.addr.ip(), "::".parse::<IpAddr>().unwrap());
        assert_eq!(resolution.addr.port(), 3000);
    }
}
