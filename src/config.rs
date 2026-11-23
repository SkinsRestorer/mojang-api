#![warn(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::{
    env,
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
};

use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use reqwest::Url;
use thiserror::Error;

const DEFAULT_SERVER_PORT: u16 = 3000;

/// Application settings.
///
/// This type does not implement `Debug`, because the Discord webhook URL contains a secret token.
pub struct Config {
    pub server_port: u16,
    pub proxy_list_file: Option<PathBuf>,
    pub discord_webhook: Option<Url>,
    /// Peers whose `CF-Connecting-IP` header is trusted as the client address.
    pub trusted_proxies: Vec<IpNet>,
}

impl Config {
    /// Loads application settings from environment variables.
    ///
    /// Variables that are set to an empty or blank value are treated as unset.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured variable is invalid.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var_os(name))
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        let text = |name: &'static str| -> Result<Option<String>, ConfigError> {
            lookup(name)
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_value| ConfigError::NotUnicode { name })
                })
                .transpose()
                .map(|value| {
                    value
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                })
        };

        let server_port = match (text("SERVER_PORT")?, text("PORT")?) {
            (Some(value), _) => parse_port("SERVER_PORT", &value)?,
            (None, Some(value)) => parse_port("PORT", &value)?,
            (None, None) => DEFAULT_SERVER_PORT,
        };
        // Paths do not have to be Unicode, so only Unicode values are trimmed.
        let proxy_list_file = lookup("PROXY_LIST_FILE")
            .map(|value| match value.into_string() {
                Ok(value) => OsString::from(value.trim()),
                Err(value) => value,
            })
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let discord_webhook = text("DISCORD_WEBHOOK")?
            .map(|value| parse_discord_webhook(&value))
            .transpose()?;
        let trusted_proxies = text("TRUSTED_PROXIES")?.map_or_else(
            || Ok(default_trusted_proxies()),
            |value| parse_trusted_proxies(&value),
        )?;

        Ok(Self {
            server_port,
            proxy_list_file,
            discord_webhook,
            trusted_proxies,
        })
    }
}

fn parse_port(name: &'static str, value: &str) -> Result<u16, ConfigError> {
    value
        .parse()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| ConfigError::InvalidPort {
            name,
            value: value.to_owned(),
        })
}

fn parse_discord_webhook(value: &str) -> Result<Url, ConfigError> {
    // The webhook URL contains a secret token, so errors never include the value.
    // `https` URLs always have a host, because parsing rejects an empty one.
    Url::parse(value)
        .ok()
        .filter(|url| url.scheme() == "https")
        .ok_or(ConfigError::InvalidDiscordWebhook)
}

fn parse_trusted_proxies(value: &str) -> Result<Vec<IpNet>, ConfigError> {
    if value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let networks = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry
                .parse::<IpNet>()
                .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                .map(|network| unmap_ipv4(network).trunc())
                .map_err(|_error| ConfigError::InvalidTrustedProxy {
                    value: entry.to_owned(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // A list of separators would otherwise trust no peer, which only `none` should do.
    if networks.is_empty() {
        return Err(ConfigError::EmptyTrustedProxies);
    }
    Ok(networks)
}

/// Converts an IPv4-mapped IPv6 range, such as `::ffff:10.0.0.0/104`, to its IPv4 range.
///
/// The rate limiter compares canonical peer addresses, so a mapped peer is always an
/// IPv4 address and would never match the IPv6 form.
fn unmap_ipv4(network: IpNet) -> IpNet {
    let IpNet::V6(v6) = network else {
        return network;
    };
    match (v6.addr().to_ipv4_mapped(), v6.prefix_len().checked_sub(96)) {
        (Some(address), Some(prefix)) => Ipv4Net::new(address, prefix).map_or(network, IpNet::V4),
        _ => network,
    }
}

/// Loopback, private, and shared address ranges, which covers a reverse proxy on the same
/// host or network, including platforms that use the CGNAT range `100.64.0.0/10`.
fn default_trusted_proxies() -> Vec<IpNet> {
    let v4 = |address: Ipv4Addr, prefix| Ipv4Net::new_assert(address, prefix).into();
    let v6 = |address: Ipv6Addr, prefix| Ipv6Net::new_assert(address, prefix).into();
    vec![
        v4(Ipv4Addr::new(127, 0, 0, 0), 8),
        v4(Ipv4Addr::new(10, 0, 0, 0), 8),
        v4(Ipv4Addr::new(172, 16, 0, 0), 12),
        v4(Ipv4Addr::new(192, 168, 0, 0), 16),
        v4(Ipv4Addr::new(100, 64, 0, 0), 10),
        v6(Ipv6Addr::LOCALHOST, 128),
        v6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    ]
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{name} must contain valid Unicode")]
    NotUnicode { name: &'static str },
    #[error("{name} must be a TCP port from 1 to 65535, but it is {value:?}")]
    InvalidPort { name: &'static str, value: String },
    #[error("DISCORD_WEBHOOK must be an absolute https:// URL")]
    InvalidDiscordWebhook,
    #[error("TRUSTED_PROXIES contains an invalid IP address or CIDR range: {value:?}")]
    InvalidTrustedProxy { value: String },
    #[error("TRUSTED_PROXIES contains no IP address or CIDR range; use \"none\" to trust no peer")]
    EmptyTrustedProxies,
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, net::IpAddr, path::PathBuf};

    use super::{Config, ConfigError};

    fn load(variables: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, OsString> = variables
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        Config::from_lookup(|name| variables.get(name).cloned())
    }

    fn trusts(config: &Config, address: &str) -> bool {
        let address: IpAddr = address.parse().expect("test address should parse");
        config
            .trusted_proxies
            .iter()
            .any(|network| network.contains(&address))
    }

    #[test]
    fn uses_defaults_when_variables_are_missing_or_blank() {
        for variables in [
            &[][..],
            &[
                ("SERVER_PORT", ""),
                ("PORT", " "),
                ("DISCORD_WEBHOOK", ""),
                ("PROXY_LIST_FILE", ""),
                ("TRUSTED_PROXIES", ""),
            ][..],
        ] {
            let config = load(variables).expect("defaults should be valid");
            assert_eq!(config.server_port, 3000);
            assert!(config.discord_webhook.is_none(), "webhook should be unset");
            assert!(
                config.proxy_list_file.is_none(),
                "proxy file should be unset"
            );
            assert!(trusts(&config, "10.1.2.3"), "private peers are trusted");
            assert!(trusts(&config, "::1"), "loopback is trusted");
            assert!(
                trusts(&config, "100.64.0.1"),
                "shared address space is trusted"
            );
            assert!(
                trusts(&config, "100.127.255.254"),
                "shared address space is trusted"
            );
            assert!(!trusts(&config, "100.128.0.1"), "100.128.0.0 is public");
            assert!(!trusts(&config, "169.254.1.1"), "link-local is not trusted");
            assert!(!trusts(&config, "203.0.113.7"), "public peers are not");
        }
    }

    #[test]
    fn prefers_server_port_over_port() {
        let config = load(&[("SERVER_PORT", "8080"), ("PORT", "9090")])
            .expect("configuration should be valid");
        assert_eq!(config.server_port, 8080);

        let config = load(&[("PORT", "9090")]).expect("configuration should be valid");
        assert_eq!(config.server_port, 9090);
    }

    #[test]
    fn names_the_invalid_port_variable() {
        for (name, value) in [("SERVER_PORT", "0"), ("PORT", "http"), ("PORT", "70000")] {
            let Err(error) = load(&[(name, value)]) else {
                panic!("{name}={value:?} should be rejected");
            };
            assert_eq!(
                error.to_string(),
                format!("{name} must be a TCP port from 1 to 65535, but it is {value:?}")
            );
        }
    }

    #[test]
    fn requires_an_https_discord_webhook() {
        for value in [
            "http://discord.example/api/webhooks/1/token",
            "https://",
            "not a url",
        ] {
            assert!(matches!(
                load(&[("DISCORD_WEBHOOK", value)]),
                Err(ConfigError::InvalidDiscordWebhook)
            ));
        }
        let config = load(&[(
            "DISCORD_WEBHOOK",
            "https://discord.example/api/webhooks/1/token",
        )])
        .expect("an https webhook should be valid");
        assert!(config.discord_webhook.is_some(), "webhook should be set");
    }

    #[test]
    fn parses_trusted_proxy_lists() {
        let config = load(&[("TRUSTED_PROXIES", "203.0.113.0/24, 2001:db8::1")])
            .expect("trusted proxies should parse");
        assert!(trusts(&config, "203.0.113.9"), "range member is trusted");
        assert!(trusts(&config, "2001:db8::1"), "single address is trusted");
        assert!(!trusts(&config, "10.0.0.1"), "defaults are replaced");

        let config = load(&[("TRUSTED_PROXIES", "none")]).expect("none should parse");
        assert!(config.trusted_proxies.is_empty(), "no peer is trusted");

        assert!(matches!(
            load(&[("TRUSTED_PROXIES", "10.0.0.0/8,proxy")]),
            Err(ConfigError::InvalidTrustedProxy { value }) if value == "proxy"
        ));
    }

    #[test]
    fn rejects_trusted_proxy_lists_without_entries() {
        for value in [",", " , ", ",,\t,"] {
            let Err(error) = load(&[("TRUSTED_PROXIES", value)]) else {
                panic!("TRUSTED_PROXIES={value:?} should be rejected");
            };
            assert_eq!(
                error.to_string(),
                "TRUSTED_PROXIES contains no IP address or CIDR range; use \"none\" to trust no peer"
            );
        }
    }

    #[test]
    fn converts_ipv4_mapped_trusted_proxies_to_ipv4() {
        let config = load(&[(
            "TRUSTED_PROXIES",
            "::ffff:198.51.100.0/120, ::ffff:192.0.2.1",
        )])
        .expect("mapped addresses should parse");
        assert!(trusts(&config, "198.51.100.200"), "mapped range is trusted");
        assert!(trusts(&config, "192.0.2.1"), "mapped address is trusted");
        assert!(!trusts(&config, "192.0.2.2"), "other addresses are not");
        assert_eq!(
            config.trusted_proxies,
            [
                "198.51.100.0/24".parse().expect("range should parse"),
                "192.0.2.1/32".parse().expect("range should parse"),
            ]
        );
    }

    #[test]
    fn reads_the_proxy_list_path() {
        for value in ["proxies.txt", " proxies.txt\t"] {
            let config =
                load(&[("PROXY_LIST_FILE", value)]).expect("configuration should be valid");
            assert_eq!(config.proxy_list_file, Some(PathBuf::from("proxies.txt")));
        }
    }
}
