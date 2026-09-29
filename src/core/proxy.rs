use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, ensure};
use tracing::warn;
use url::{Host, Url};

use crate::{MacosProxyConfig, ProxyApplyOutcome};

const MAX_HOST_LEN: usize = 64;
const MAX_BYPASS_LEN: usize = 8192;
const MAX_PAC_URL_LEN: usize = 256;
const PAC_PATH: &str = "/commands/pac";

/// Bounded wait for macOS to publish a resolvable primary network service while clearing a proxy.
/// The total stays far below the IPC handler timeout, so a takeover can still answer its caller.
const CLEAR_PROXY_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Diagnostic sysproxy reports when the primary service named by `State:/Network/Global/IPv4`
/// cannot be resolved in the preferences store.
#[cfg(target_os = "macos")]
const RESOLVE_ACTIVE_NETWORK_SERVICE: &str = "resolve active network service";

pub fn validate_proxy_config(config: &MacosProxyConfig) -> anyhow::Result<()> {
    match config {
        MacosProxyConfig::Disabled => Ok(()),
        MacosProxyConfig::Global { host, port, bypass } => {
            ensure!(host.len() <= MAX_HOST_LEN, "proxy host exceeds {MAX_HOST_LEN} bytes");
            ensure!(
                bypass.len() <= MAX_BYPASS_LEN,
                "proxy bypass exceeds {MAX_BYPASS_LEN} bytes"
            );
            ensure!(!host.contains('\0'), "proxy host contains NUL");
            ensure!(!bypass.contains('\0'), "proxy bypass contains NUL");
            ensure!(*port != 0, "proxy port must be nonzero");

            let address: IpAddr = host.parse().context("proxy host must be an IP address")?;
            ensure!(address.is_loopback(), "proxy host must be loopback");
            Ok(())
        }
        MacosProxyConfig::Pac { url } => validate_pac_url(url),
    }
}

fn validate_pac_url(raw: &str) -> anyhow::Result<()> {
    ensure!(raw.len() <= MAX_PAC_URL_LEN, "PAC URL exceeds {MAX_PAC_URL_LEN} bytes");
    ensure!(!raw.contains('\0'), "PAC URL contains NUL");

    let parsed = Url::parse(raw).context("invalid PAC URL")?;
    ensure!(parsed.scheme() == "http", "PAC URL scheme must be http");
    ensure!(parsed.username().is_empty(), "PAC URL must not contain a username");
    ensure!(parsed.password().is_none(), "PAC URL must not contain a password");
    ensure!(parsed.query().is_none(), "PAC URL must not contain a query");
    ensure!(parsed.fragment().is_none(), "PAC URL must not contain a fragment");
    ensure!(parsed.path() == PAC_PATH, "PAC URL path must be {PAC_PATH}");

    let authority = raw
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .and_then(|remainder| remainder.split(['/', '?', '#']).next())
        .context("PAC URL must contain an authority")?;
    ensure!(!authority.contains('@'), "PAC URL must not contain userinfo");
    ensure!(explicit_port(authority)? != 0, "PAC URL port must be nonzero");

    let host = parsed.host().context("PAC URL must contain a host")?;
    ensure!(
        parsed.host_str().is_some_and(|host| host.len() <= MAX_HOST_LEN),
        "PAC URL host exceeds {MAX_HOST_LEN} bytes"
    );
    let is_loopback = match host {
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
        Host::Domain(_) => false,
    };
    ensure!(is_loopback, "PAC URL host must be a loopback IP address");
    Ok(())
}

fn explicit_port(authority: &str) -> anyhow::Result<u16> {
    let port = if authority.starts_with('[') {
        let closing_bracket = authority.find(']').context("invalid PAC URL IPv6 host")?;
        authority
            .get(closing_bracket + 1..)
            .and_then(|remainder| remainder.strip_prefix(':'))
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    }
    .context("PAC URL must contain an explicit port")?;

    ensure!(!port.is_empty(), "PAC URL must contain an explicit port");
    port.parse::<u16>().context("PAC URL port must be a u16")
}

#[cfg(target_os = "macos")]
fn apply_real(config: &MacosProxyConfig) -> anyhow::Result<()> {
    let (system, auto) = match config {
        MacosProxyConfig::Disabled => (sysproxy::Sysproxy::default(), sysproxy::Autoproxy::default()),
        MacosProxyConfig::Global { host, port, bypass } => (
            sysproxy::Sysproxy {
                host: host.clone(),
                port: *port,
                bypass: bypass.clone(),
                enable: true,
            },
            sysproxy::Autoproxy::default(),
        ),
        MacosProxyConfig::Pac { url } => (
            sysproxy::Sysproxy::default(),
            sysproxy::Autoproxy {
                url: url.clone(),
                enable: true,
            },
        ),
    };
    sysproxy::apply_privileged_native(&system, &auto).map_err(Into::into)
}

#[cfg(any(target_os = "macos", test))]
fn apply_proxy_or_direct_with(
    config: Option<&MacosProxyConfig>,
    mut apply: impl FnMut(&MacosProxyConfig) -> anyhow::Result<()>,
) -> anyhow::Result<ProxyApplyOutcome> {
    let Some(config) = config else {
        return Ok(ProxyApplyOutcome::NotRequested);
    };
    validate_proxy_config(config)?;

    match apply(config) {
        Ok(()) => Ok(ProxyApplyOutcome::Applied),
        Err(apply_error) => {
            // A lookup failure means no write of ours failed midway, so there is nothing to undo.
            let nothing_to_undo = is_network_service_unavailable(&apply_error);
            match apply(&MacosProxyConfig::Disabled) {
                Ok(()) => {}
                Err(compensation_error) if nothing_to_undo && is_network_service_unavailable(&compensation_error) => {}
                Err(compensation_error) => {
                    return Err(compensation_error).with_context(|| {
                        format!("failed to compensate proxy apply failure ({apply_error}) with direct mode")
                    });
                }
            }
            Ok(ProxyApplyOutcome::DirectFallback {
                message: apply_error.to_string(),
            })
        }
    }
}

#[cfg(target_os = "macos")]
pub async fn apply_proxy(config: &MacosProxyConfig) -> anyhow::Result<()> {
    validate_proxy_config(config)?;
    let config = config.clone();
    tokio::task::spawn_blocking(move || apply_real(&config))
        .await
        .context("proxy apply task failed")?
}

#[cfg(not(target_os = "macos"))]
pub async fn apply_proxy(_config: &MacosProxyConfig) -> anyhow::Result<()> {
    anyhow::bail!("macOS proxy configuration is unsupported on this platform")
}

/// Succeeds when macOS has no active network service: with nothing to write to, the requested
/// "no proxy" state already holds, and failing would block callers that clear before stopping.
pub async fn clear_proxy() -> anyhow::Result<()> {
    match apply_proxy(&MacosProxyConfig::Disabled).await {
        Err(error) if is_network_service_unavailable(&error) => {
            warn!("no active network service, nothing to clear: {error:#}");
            Ok(())
        }
        result => result,
    }
}

/// Clears the proxy, waiting briefly for macOS to publish a resolvable primary network service.
///
/// `State:/Network/Global/IPv4` starts naming a primary service before that service can be
/// resolved in the preferences store, so a takeover that lands in that window (login items run
/// before DHCP completes, or a network switch is in progress) cannot rewrite the previous owner's
/// proxy yet. Failing the whole takeover there leaves the user with no core at all, while the
/// proxy settings are replaced or cleared as soon as the network settles, so wait instead of
/// aborting. Real write failures still surface as errors.
pub async fn clear_proxy_waiting_for_network() -> anyhow::Result<()> {
    clear_proxy_waiting_with(|| apply_proxy(&MacosProxyConfig::Disabled), CLEAR_PROXY_RETRY_DELAYS).await
}

async fn clear_proxy_waiting_with<F, Fut>(mut apply: F, delays: &[Duration]) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut retries = 0;
    loop {
        match apply().await {
            Ok(()) => return Ok(()),
            Err(error) if is_network_service_unavailable(&error) => {
                let Some(delay) = delays.get(retries) else {
                    warn!("no resolvable network service after {retries} retries, nothing to clear: {error:#}");
                    return Ok(());
                };
                warn!("network service is not resolvable yet, retrying proxy clear in {delay:?}: {error:#}");
                tokio::time::sleep(*delay).await;
                retries += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether macOS could not resolve an active network service to read or write.
///
/// `State:/Network/Global/IPv4` can name a primary service that `SCNetworkServiceCopy` cannot
/// resolve from the preferences store: during the login window (a login item starts before DHCP
/// completes) and while a network switch or VPN handoff is in progress. sysproxy reports that as
/// `SystemConfiguration("resolve active network service")`, which means the same as
/// `NoActiveNetworkService`: there is no active service, so nothing was applied.
#[cfg(target_os = "macos")]
fn is_network_service_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<sysproxy::Error>(),
            Some(
                sysproxy::Error::NoActiveNetworkService
                    | sysproxy::Error::NetworkInterface
                    | sysproxy::Error::SystemConfiguration(RESOLVE_ACTIVE_NETWORK_SERVICE)
            )
        )
    })
}

#[cfg(not(target_os = "macos"))]
const fn is_network_service_unavailable(_error: &anyhow::Error) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub async fn apply_proxy_or_direct(config: Option<&MacosProxyConfig>) -> anyhow::Result<ProxyApplyOutcome> {
    let config = config.cloned();
    tokio::task::spawn_blocking(move || apply_proxy_or_direct_with(config.as_ref(), apply_real))
        .await
        .context("proxy apply task failed")?
}

#[cfg(not(target_os = "macos"))]
pub async fn apply_proxy_or_direct(config: Option<&MacosProxyConfig>) -> anyhow::Result<ProxyApplyOutcome> {
    match config {
        None => Ok(ProxyApplyOutcome::NotRequested),
        Some(_) => anyhow::bail!("macOS proxy configuration is unsupported on this platform"),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::RESOLVE_ACTIVE_NETWORK_SERVICE;
    use super::{
        apply_proxy_or_direct_with, clear_proxy_waiting_with, is_network_service_unavailable, validate_proxy_config,
    };
    use crate::{MacosProxyConfig, ProxyApplyOutcome};
    use std::time::Duration;

    #[test]
    fn proxy_contract_accepts_only_loopback_targets() {
        assert!(
            validate_proxy_config(&MacosProxyConfig::Global {
                host: "127.0.0.1".to_owned(),
                port: 7897,
                bypass: "localhost".to_owned(),
            })
            .is_ok()
        );
        assert!(
            validate_proxy_config(&MacosProxyConfig::Pac {
                url: "http://127.0.0.1:33221/commands/pac".to_owned(),
            })
            .is_ok()
        );
        assert!(
            validate_proxy_config(&MacosProxyConfig::Pac {
                url: "http://[::1]:80/commands/pac".to_owned(),
            })
            .is_ok()
        );
        assert!(
            validate_proxy_config(&MacosProxyConfig::Global {
                host: "203.0.113.9".to_owned(),
                port: 7897,
                bypass: String::new(),
            })
            .is_err()
        );
        assert!(
            validate_proxy_config(&MacosProxyConfig::Pac {
                url: "https://example.invalid/proxy.pac".to_owned(),
            })
            .is_err()
        );
    }

    #[test]
    fn proxy_contract_rejects_unbounded_or_unsafe_fields() {
        let invalid = [
            MacosProxyConfig::Global {
                host: "127.0.0.1\0".to_owned(),
                port: 7897,
                bypass: String::new(),
            },
            MacosProxyConfig::Global {
                host: "127.0.0.1".to_owned(),
                port: 0,
                bypass: String::new(),
            },
            MacosProxyConfig::Global {
                host: "1".repeat(65),
                port: 7897,
                bypass: String::new(),
            },
            MacosProxyConfig::Global {
                host: "::1".to_owned(),
                port: 7897,
                bypass: "x".repeat(8193),
            },
            MacosProxyConfig::Global {
                host: "::1".to_owned(),
                port: 7897,
                bypass: "localhost\0example".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: format!("http://127.0.0.1:1/commands/pac?{}", "x".repeat(256)),
            },
            MacosProxyConfig::Pac {
                url: "http://127.0.0.1/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://127.0.0.1:0/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://user@127.0.0.1:33221/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://@127.0.0.1:33221/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://user:pass@127.0.0.1:33221/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://127.0.0.1:33221/commands/pac?x=1".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://127.0.0.1:33221/commands/pac#fragment".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://127.0.0.1:33221/other".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://203.0.113.9:33221/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://localhost:33221/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://[2001:db8::1]:33221/commands/pac".to_owned(),
            },
            MacosProxyConfig::Pac {
                url: "http://[::1]:33221/commands/pac\0".to_owned(),
            },
        ];

        for config in invalid {
            assert!(validate_proxy_config(&config).is_err(), "accepted {config:?}");
        }
    }

    #[test]
    fn proxy_apply_failure_compensates_once_with_disabled() {
        let config = MacosProxyConfig::Global {
            host: "127.0.0.1".to_owned(),
            port: 7897,
            bypass: String::new(),
        };
        let mut calls = Vec::new();

        let outcome = apply_proxy_or_direct_with(Some(&config), |config| {
            calls.push(config.clone());
            if calls.len() == 1 {
                anyhow::bail!("apply failed")
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(
            outcome,
            ProxyApplyOutcome::DirectFallback {
                message: "apply failed".to_owned(),
            }
        );
        assert_eq!(calls, [config, MacosProxyConfig::Disabled]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_network_service_still_reports_direct_fallback() {
        let config = MacosProxyConfig::Global {
            host: "127.0.0.1".to_owned(),
            port: 7897,
            bypass: String::new(),
        };
        let mut calls = Vec::new();

        let outcome = apply_proxy_or_direct_with(Some(&config), |config| {
            calls.push(config.clone());
            Err(anyhow::Error::new(sysproxy::Error::NoActiveNetworkService))
        })
        .unwrap();

        assert!(matches!(outcome, ProxyApplyOutcome::DirectFallback { .. }));
        assert_eq!(calls, [config, MacosProxyConfig::Disabled]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_partial_write_still_requires_compensation() {
        let config = MacosProxyConfig::Global {
            host: "127.0.0.1".to_owned(),
            port: 7897,
            bypass: String::new(),
        };
        let mut calls = 0;

        let error = apply_proxy_or_direct_with(Some(&config), |_| {
            calls += 1;
            if calls == 1 {
                Err(anyhow::anyhow!("native proxy transaction failed"))
            } else {
                Err(anyhow::Error::new(sysproxy::Error::NoActiveNetworkService))
            }
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("failed to compensate proxy apply failure"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_unresolvable_network_service_reads_as_unavailable() {
        let unavailable = [
            anyhow::Error::new(sysproxy::Error::NoActiveNetworkService),
            anyhow::Error::new(sysproxy::Error::NetworkInterface),
            anyhow::Error::new(sysproxy::Error::SystemConfiguration(RESOLVE_ACTIVE_NETWORK_SERVICE)),
        ];
        for error in unavailable {
            assert!(is_network_service_unavailable(&error), "not recognized: {error:#}");
        }

        let write_failure = anyhow::Error::new(sysproxy::Error::SystemConfiguration("commit proxy configuration"));
        assert!(!is_network_service_unavailable(&write_failure));
        assert!(!is_network_service_unavailable(&anyhow::anyhow!(
            "native proxy transaction failed"
        )));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn clearing_waits_for_a_resolvable_network_service() -> anyhow::Result<()> {
        let attempts = std::cell::Cell::new(0);
        let delays = [Duration::ZERO, Duration::ZERO];

        clear_proxy_waiting_with(
            || {
                let attempt = attempts.get();
                attempts.set(attempt + 1);
                async move {
                    if attempt < 2 {
                        Err(anyhow::Error::new(sysproxy::Error::SystemConfiguration(
                            RESOLVE_ACTIVE_NETWORK_SERVICE,
                        )))
                    } else {
                        Ok(())
                    }
                }
            },
            &delays,
        )
        .await?;

        assert_eq!(
            attempts.get(),
            3,
            "the clear must be retried until the service resolves"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn clearing_tolerates_a_service_that_never_resolves() -> anyhow::Result<()> {
        let attempts = std::cell::Cell::new(0);
        let delays = [Duration::ZERO, Duration::ZERO];

        clear_proxy_waiting_with(
            || {
                attempts.set(attempts.get() + 1);
                async { Err(anyhow::Error::new(sysproxy::Error::NoActiveNetworkService)) }
            },
            &delays,
        )
        .await?;

        assert_eq!(attempts.get(), 3, "the retry budget is bounded by the delay schedule");
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn clearing_reports_write_failures_without_retrying() {
        let attempts = std::cell::Cell::new(0);

        let error = clear_proxy_waiting_with(
            || {
                attempts.set(attempts.get() + 1);
                async { Err(anyhow::anyhow!("native proxy transaction failed")) }
            },
            &[Duration::ZERO],
        )
        .await
        .expect_err("a write failure must not be treated as nothing to clear");

        assert!(format!("{error:#}").contains("native proxy transaction failed"));
        assert_eq!(attempts.get(), 1);
    }
}
