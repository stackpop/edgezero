//! Shared bind-address resolution for EdgeZero dev servers.
//!
//! Centralises the precedence logic (env vars > config > defaults) so that
//! both the Axum adapter and the CLI dev server produce consistent results.

use std::net::{IpAddr, SocketAddr};

const DEFAULT_HOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
const DEFAULT_PORT: u16 = 8787;

/// Resolve a bind address from optional environment and config values.
///
/// Precedence (highest wins):
/// 1. `env_host` / `env_port` (typically `EDGEZERO_HOST` / `EDGEZERO_PORT`)
/// 2. `config_host` / `config_port` (from manifest or adapter config)
/// 3. Defaults: `127.0.0.1:8787`
///
/// Invalid values produce a `log::warn!` and fall back to the default.
/// Port 0 is rejected (random OS port is almost never intended).
pub fn resolve_bind_addr(
    env_host: Option<&str>,
    env_port: Option<&str>,
    config_host: Option<&str>,
    config_port: Option<u16>,
) -> SocketAddr {
    let host = resolve_host(env_host, config_host);
    let port = resolve_port(env_port, config_port);
    SocketAddr::from((host, port))
}

fn resolve_host(env_host: Option<&str>, config_host: Option<&str>) -> IpAddr {
    if let Some(v) = env_host {
        return v.parse().unwrap_or_else(|_| {
            log::warn!("EDGEZERO_HOST={v:?} is not a valid IP address, using default");
            DEFAULT_HOST
        });
    }
    if let Some(h) = config_host {
        return h.parse().unwrap_or_else(|_| {
            log::warn!("configured host={h:?} is not a valid IP address, using default");
            DEFAULT_HOST
        });
    }
    DEFAULT_HOST
}

fn resolve_port(env_port: Option<&str>, config_port: Option<u16>) -> u16 {
    let port = if let Some(v) = env_port {
        v.parse().unwrap_or_else(|_| {
            log::warn!("EDGEZERO_PORT={v:?} is not a valid port number, using default");
            DEFAULT_PORT
        })
    } else {
        config_port.unwrap_or(DEFAULT_PORT)
    };

    if port == 0 {
        log::warn!("port 0 is not supported, using default {DEFAULT_PORT}");
        return DEFAULT_PORT;
    }

    port
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn defaults_when_nothing_provided() {
        let addr = resolve_bind_addr(None, None, None, None);
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 8787)));
    }

    #[test]
    fn config_overrides_defaults() {
        let addr = resolve_bind_addr(None, None, Some("0.0.0.0"), Some(3000));
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(addr.port(), 3000);
    }

    #[test]
    fn env_overrides_config() {
        let addr = resolve_bind_addr(Some("0.0.0.0"), Some("4000"), Some("127.0.0.1"), Some(3000));
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(addr.port(), 4000);
    }

    #[test]
    fn partial_env_override_host_only() {
        let addr = resolve_bind_addr(Some("0.0.0.0"), None, None, Some(5000));
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(addr.port(), 5000);
    }

    #[test]
    fn partial_env_override_port_only() {
        let addr = resolve_bind_addr(None, Some("9000"), Some("0.0.0.0"), None);
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(addr.port(), 9000);
    }

    #[test]
    fn invalid_env_host_falls_back_to_default() {
        let addr = resolve_bind_addr(Some("not-an-ip"), None, Some("0.0.0.0"), None);
        assert_eq!(addr.ip(), DEFAULT_HOST);
    }

    #[test]
    fn invalid_env_port_falls_back_to_default() {
        let addr = resolve_bind_addr(None, Some("abc"), None, Some(3000));
        assert_eq!(addr.port(), DEFAULT_PORT);
    }

    #[test]
    fn invalid_config_host_falls_back_to_default() {
        let addr = resolve_bind_addr(None, None, Some("not-an-ip"), None);
        assert_eq!(addr.ip(), DEFAULT_HOST);
    }

    #[test]
    fn port_zero_from_env_falls_back_to_default() {
        let addr = resolve_bind_addr(None, Some("0"), None, None);
        assert_eq!(addr.port(), DEFAULT_PORT);
    }

    #[test]
    fn port_zero_from_config_falls_back_to_default() {
        let addr = resolve_bind_addr(None, None, None, Some(0));
        assert_eq!(addr.port(), DEFAULT_PORT);
    }

    #[test]
    fn ipv6_host_from_env() {
        let addr = resolve_bind_addr(Some("::1"), None, None, None);
        assert_eq!(addr.ip(), "::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn ipv6_host_from_config() {
        let addr = resolve_bind_addr(None, None, Some("::"), Some(3000));
        assert_eq!(addr.ip(), "::".parse::<IpAddr>().unwrap());
        assert_eq!(addr.port(), 3000);
    }
}
