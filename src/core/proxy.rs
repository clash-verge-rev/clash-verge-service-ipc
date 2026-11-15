use std::net::IpAddr;

use anyhow::{Context, ensure};
use url::{Host, Url};

use crate::{MacosProxyConfig, ProxyApplyOutcome};

const MAX_HOST_LEN: usize = 64;
const MAX_BYPASS_LEN: usize = 8192;
const MAX_PAC_URL_LEN: usize = 256;
const PAC_PATH: &str = "/commands/pac";

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

#[cfg(all(target_os = "macos", not(feature = "test")))]
fn native_proxy_config(config: &MacosProxyConfig) -> (sysproxy::Sysproxy, sysproxy::Autoproxy) {
    match config {
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
    }
}

#[cfg(test)]
mod tests {

    #[cfg(feature = "test")]
    #[tokio::test]
    async fn finished_execution_cannot_reapply_proxy_after_terminal_cleanup() -> anyhow::Result<()> {
        let old = super::ProxyScope::default();
        let config = crate::MacosProxyConfig::Global {
            host: "127.0.0.1".into(),
            port: 7897,
            bypass: String::new(),
        };
        old.apply(Some(&config)).await?;
        old.finish().await?;
        assert!(old.apply(Some(&config)).await.is_err());
        let current = super::ProxyScope::default();
        current.apply(Some(&config)).await?;
        old.finish().await?;
        assert!(current.apply(Some(&config)).await.is_ok());
        Ok(())
    }
    use super::validate_proxy_config;
    use crate::MacosProxyConfig;

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
}

/// One core execution and its watchdog share exactly one proxy responsibility.
#[derive(Default)]
pub(super) struct ProxyScope {
    state: tokio::sync::Mutex<ProxyScopeState>,
}

#[derive(Default)]
struct ProxyScopeState {
    closed: bool,
    #[cfg(all(target_os = "macos", not(feature = "test")))]
    owner: Option<crate::core::desired::ActiveOwnerState>,
}

impl ProxyScope {
    pub(super) fn restored(active: crate::core::desired::ActiveOwnerState) -> std::sync::Arc<Self> {
        #[cfg(all(target_os = "macos", not(feature = "test")))]
        return std::sync::Arc::new(Self {
            state: tokio::sync::Mutex::new(ProxyScopeState {
                closed: false,
                owner: active.proxy_receipt.as_ref().map(|_| active.clone()),
            }),
        });
        #[cfg(any(not(target_os = "macos"), feature = "test"))]
        {
            let _ = active;
            std::sync::Arc::new(Self::default())
        }
    }

    pub(super) async fn clear(&self) -> anyhow::Result<()> {
        #[cfg(all(target_os = "macos", not(feature = "test")))]
        {
            let mut state = self.state.lock().await;
            clear_owned_proxy(&mut state.owner).await
        }
        #[cfg(any(not(target_os = "macos"), feature = "test"))]
        Ok(())
    }

    pub(super) async fn finish(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        state.closed = true;
        #[cfg(all(target_os = "macos", not(feature = "test")))]
        clear_owned_proxy(&mut state.owner).await?;
        Ok(())
    }

    pub(super) async fn apply(&self, config: Option<&MacosProxyConfig>) -> anyhow::Result<ProxyApplyOutcome> {
        let Some(config) = config else {
            return Ok(ProxyApplyOutcome::NotRequested);
        };
        validate_proxy_config(config)?;
        #[allow(unused_mut)]
        let mut state = self.state.lock().await;
        ensure!(!state.closed, "core execution no longer accepts proxy changes");
        #[cfg(feature = "test")]
        return Ok(ProxyApplyOutcome::Applied);
        #[cfg(all(not(feature = "test"), not(target_os = "macos")))]
        anyhow::bail!("macOS proxy configuration is unsupported on this platform");
        #[cfg(all(target_os = "macos", not(feature = "test")))]
        {
            let slot = &mut state.owner;
            // Finish the previous setting's conditional cleanup before replacing its receipt.
            clear_owned_proxy(slot).await?;
            let mut active = crate::core::desired::load_active_owner()
                .await?
                .context("proxy apply requires an active owner")?;
            let (system, auto) = native_proxy_config(config);
            let runtime = tokio::runtime::Handle::current();
            let (updated, applied) = tokio::task::spawn_blocking(move || {
                let applied = sysproxy::apply_privileged_native_owned(&system, &auto, |receipt| {
                    runtime
                        .block_on(crate::core::desired::persist_proxy_receipt(
                            &active,
                            Some(receipt.clone()),
                        ))
                        .map_err(|error| sysproxy::Error::Io(std::io::Error::other(format!("{error:#}"))))?;
                    active.proxy_receipt = Some(receipt.clone());
                    Ok(())
                });
                (active, applied)
            })
            .await
            .context("owned proxy apply task failed")?;
            if updated.proxy_receipt.is_some() {
                *slot = Some(updated);
            }
            match applied {
                Ok(applied) => match applied.result {
                    Ok(()) => Ok(ProxyApplyOutcome::Applied),
                    Err(error) => {
                        clear_owned_proxy(slot)
                            .await
                            .with_context(|| format!("failed to compensate proxy apply ({error}) with direct mode"))?;
                        Ok(ProxyApplyOutcome::DirectFallback {
                            message: error.to_string(),
                        })
                    }
                },
                Err(sysproxy::Error::NoActiveNetworkService) => Ok(ProxyApplyOutcome::DirectFallback {
                    message: sysproxy::Error::NoActiveNetworkService.to_string(),
                }),
                Err(error) => Err(error.into()),
            }
        }
    }
}

#[cfg(all(target_os = "macos", not(feature = "test")))]
async fn clear_owned_proxy(slot: &mut Option<crate::core::desired::ActiveOwnerState>) -> anyhow::Result<()> {
    let Some(active) = slot.as_ref() else {
        return Ok(());
    };
    let mut active = active.clone();
    let runtime = tokio::runtime::Handle::current();
    let (updated, cleared) = tokio::task::spawn_blocking(move || {
        let mut receipt = active
            .proxy_receipt
            .take()
            .expect("owned proxy scope must retain its receipt");
        let cleared = sysproxy::clear_privileged_native_if_unchanged(&mut receipt, |updated| {
            runtime
                .block_on(crate::core::desired::persist_proxy_receipt(
                    &active,
                    Some(updated.clone()),
                ))
                .map_err(|error| sysproxy::Error::Io(std::io::Error::other(format!("{error:#}"))))
        });
        active.proxy_receipt = Some(receipt);
        (active, cleared)
    })
    .await
    .context("owned proxy clear task failed")?;
    *slot = Some(updated.clone());
    cleared?;
    crate::core::desired::persist_proxy_receipt(&updated, None).await?;
    *slot = None;
    Ok(())
}

#[cfg(all(
    test,
    target_os = "macos",
    not(feature = "test"),
    feature = "client",
    feature = "development-channel"
))]
pub(in crate::core) mod native_tests;
