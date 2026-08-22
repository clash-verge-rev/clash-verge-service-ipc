use std::net::IpAddr;

use anyhow::{Context, ensure};
use tracing::warn;
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
            ensure!(
                host.len() <= MAX_HOST_LEN,
                "proxy host exceeds {MAX_HOST_LEN} bytes"
            );
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
    ensure!(
        raw.len() <= MAX_PAC_URL_LEN,
        "PAC URL exceeds {MAX_PAC_URL_LEN} bytes"
    );
    ensure!(!raw.contains('\0'), "PAC URL contains NUL");

    let parsed = Url::parse(raw).context("invalid PAC URL")?;
    ensure!(parsed.scheme() == "http", "PAC URL scheme must be http");
    ensure!(
        parsed.username().is_empty(),
        "PAC URL must not contain a username"
    );
    ensure!(
        parsed.password().is_none(),
        "PAC URL must not contain a password"
    );
    ensure!(parsed.query().is_none(), "PAC URL must not contain a query");
    ensure!(
        parsed.fragment().is_none(),
        "PAC URL must not contain a fragment"
    );
    ensure!(parsed.path() == PAC_PATH, "PAC URL path must be {PAC_PATH}");

    let authority = raw
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .and_then(|remainder| remainder.split(['/', '?', '#']).next())
        .context("PAC URL must contain an authority")?;
    ensure!(
        !authority.contains('@'),
        "PAC URL must not contain userinfo"
    );
    ensure!(
        explicit_port(authority)? != 0,
        "PAC URL port must be nonzero"
    );

    let host = parsed.host().context("PAC URL must contain a host")?;
    ensure!(
        parsed
            .host_str()
            .is_some_and(|host| host.len() <= MAX_HOST_LEN),
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

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SystemProxy {
    host: String,
    port: u16,
    bypass: String,
    enable: bool,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AutoProxy {
    url: String,
    enable: bool,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProxyCall {
    System(SystemProxy),
    Auto(AutoProxy),
}

#[cfg(any(target_os = "macos", test))]
trait ProxyBackend {
    fn set_system_proxy(&mut self, proxy: SystemProxy) -> anyhow::Result<()>;
    fn set_auto_proxy(&mut self, proxy: AutoProxy) -> anyhow::Result<()>;
}

#[cfg(any(target_os = "macos", test))]
fn apply_with_backend(
    backend: &mut impl ProxyBackend,
    config: &MacosProxyConfig,
) -> anyhow::Result<()> {
    match config {
        MacosProxyConfig::Disabled => {
            backend.set_system_proxy(SystemProxy::default())?;
            backend.set_auto_proxy(AutoProxy::default())?;
        }
        MacosProxyConfig::Global { host, port, bypass } => {
            backend.set_auto_proxy(AutoProxy::default())?;
            backend.set_system_proxy(SystemProxy {
                host: host.clone(),
                port: *port,
                bypass: bypass.clone(),
                enable: true,
            })?;
        }
        MacosProxyConfig::Pac { url } => {
            backend.set_system_proxy(SystemProxy::default())?;
            backend.set_auto_proxy(AutoProxy {
                url: url.clone(),
                enable: true,
            })?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
use system_configuration::{
    core_foundation::{
        array::CFArray,
        base::{CFRelease, CFType, TCFType},
        dictionary::{CFDictionary, CFMutableDictionary},
        number::CFNumber,
        string::CFString,
    },
    dynamic_store::SCDynamicStoreBuilder,
    preferences::SCPreferences,
    sys::{
        network_configuration::{
            SCNetworkProtocolGetConfiguration, SCNetworkProtocolRef,
            SCNetworkProtocolSetConfiguration, SCNetworkServiceCopy, SCNetworkServiceCopyProtocol,
        },
        preferences::{
            SCPreferencesApplyChanges, SCPreferencesCommitChanges, SCPreferencesLock,
            SCPreferencesUnlock,
        },
    },
};

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct NoActiveNetworkService;

#[cfg(target_os = "macos")]
impl std::fmt::Display for NoActiveNetworkService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("no active network service")
    }
}

#[cfg(target_os = "macos")]
impl std::error::Error for NoActiveNetworkService {}

#[cfg(target_os = "macos")]
struct RealBackend {
    preferences: SCPreferences,
    protocol: SCNetworkProtocolRef,
    config: CFMutableDictionary<CFString, CFType>,
    locked: bool,
}

#[cfg(target_os = "macos")]
impl RealBackend {
    fn open() -> anyhow::Result<Self> {
        let service_id = active_network_service_id()?;
        let preferences = SCPreferences::default(&CFString::new("clash-verge-service"));

        // A privileged service must not wait indefinitely behind another preferences writer.
        let locked = unsafe { SCPreferencesLock(preferences.as_concrete_TypeRef(), 0) } != 0;
        ensure!(locked, "failed to lock SystemConfiguration preferences");

        unsafe {
            let service = SCNetworkServiceCopy(
                preferences.as_concrete_TypeRef(),
                service_id.as_concrete_TypeRef(),
            );
            if service.is_null() {
                SCPreferencesUnlock(preferences.as_concrete_TypeRef());
                anyhow::bail!("active network service is unavailable in preferences");
            }

            let protocol = SCNetworkServiceCopyProtocol(
                service,
                CFString::from_static_string("Proxies").as_concrete_TypeRef(),
            );
            CFRelease(service.cast());
            if protocol.is_null() {
                SCPreferencesUnlock(preferences.as_concrete_TypeRef());
                anyhow::bail!("active network service has no proxy protocol");
            }

            let current = SCNetworkProtocolGetConfiguration(protocol);
            let config = if current.is_null() {
                CFMutableDictionary::new()
            } else {
                let current = CFDictionary::<CFString, CFType>::wrap_under_get_rule(current);
                CFMutableDictionary::from(&current)
            };

            Ok(Self {
                preferences,
                protocol,
                config,
                locked: true,
            })
        }
    }

    fn set_number(&mut self, key: &'static str, value: i32) {
        self.config.set(
            CFString::from_static_string(key),
            CFNumber::from(value).as_CFType(),
        );
    }

    fn set_string(&mut self, key: &'static str, value: &str) {
        self.config.set(
            CFString::from_static_string(key),
            CFString::new(value).as_CFType(),
        );
    }

    fn commit(mut self) -> anyhow::Result<()> {
        let config = self.config.to_immutable();
        ensure!(
            unsafe {
                SCNetworkProtocolSetConfiguration(self.protocol, config.as_concrete_TypeRef())
            } != 0,
            "failed to stage native proxy configuration"
        );
        ensure!(
            unsafe { SCPreferencesCommitChanges(self.preferences.as_concrete_TypeRef()) } != 0,
            "failed to commit native proxy configuration"
        );
        ensure!(
            unsafe { SCPreferencesApplyChanges(self.preferences.as_concrete_TypeRef()) } != 0,
            "failed to apply native proxy configuration"
        );
        ensure!(
            self.unlock(),
            "failed to unlock SystemConfiguration preferences"
        );
        Ok(())
    }

    fn unlock(&mut self) -> bool {
        if !self.locked {
            return true;
        }
        let unlocked = unsafe { SCPreferencesUnlock(self.preferences.as_concrete_TypeRef()) != 0 };
        if unlocked {
            self.locked = false;
        }
        unlocked
    }
}

#[cfg(target_os = "macos")]
impl ProxyBackend for RealBackend {
    fn set_system_proxy(&mut self, proxy: SystemProxy) -> anyhow::Result<()> {
        const PROXY_KEYS: [(&str, &str, &str); 3] = [
            ("HTTPProxy", "HTTPPort", "HTTPEnable"),
            ("HTTPSProxy", "HTTPSPort", "HTTPSEnable"),
            ("SOCKSProxy", "SOCKSPort", "SOCKSEnable"),
        ];

        for (host_key, port_key, enable_key) in PROXY_KEYS {
            self.set_string(host_key, &proxy.host);
            self.set_number(port_key, i32::from(proxy.port));
            self.set_number(enable_key, i32::from(proxy.enable));
        }

        let bypass = if proxy.bypass.is_empty() {
            Vec::new()
        } else {
            proxy.bypass.split(',').map(CFString::new).collect()
        };
        self.config.set(
            CFString::from_static_string("ExceptionsList"),
            CFArray::from_CFTypes(&bypass).as_CFType(),
        );
        Ok(())
    }

    fn set_auto_proxy(&mut self, proxy: AutoProxy) -> anyhow::Result<()> {
        self.set_string("ProxyAutoConfigURLString", &proxy.url);
        self.set_number("ProxyAutoConfigEnable", i32::from(proxy.enable));
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn apply_real(config: &MacosProxyConfig) -> anyhow::Result<()> {
    let mut backend = RealBackend::open()?;
    apply_with_backend(&mut backend, config)?;
    backend.commit()
}

#[cfg(target_os = "macos")]
impl Drop for RealBackend {
    fn drop(&mut self) {
        if self.locked {
            self.unlock();
        }
        unsafe {
            CFRelease(self.protocol.cast());
        }
    }
}

#[cfg(target_os = "macos")]
fn active_network_service_id() -> anyhow::Result<CFString> {
    let store = SCDynamicStoreBuilder::new("clash-verge-service")
        .build()
        .context("failed to open SystemConfiguration dynamic store")?;
    let state = store
        .get(CFString::from_static_string("State:/Network/Global/IPv4"))
        .ok_or(NoActiveNetworkService)?;
    let dictionary = state
        .downcast_into::<CFDictionary>()
        .ok_or_else(|| anyhow::anyhow!("global IPv4 state is not a dictionary"))?;
    let service_id = dictionary
        .find(
            CFString::from_static_string("PrimaryService")
                .as_CFTypeRef()
                .cast(),
        )
        .ok_or(NoActiveNetworkService)?;
    let service_id = unsafe { CFString::wrap_under_get_rule(*service_id as _) };
    ensure!(!service_id.to_string().is_empty(), NoActiveNetworkService);
    Ok(service_id)
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
            let nothing_to_undo = is_no_active_network_service(&apply_error);
            match apply(&MacosProxyConfig::Disabled) {
                Ok(()) => {}
                Err(compensation_error)
                    if nothing_to_undo && is_no_active_network_service(&compensation_error) => {}
                Err(compensation_error) => {
                    return Err(compensation_error).with_context(|| {
                        format!(
                            "failed to compensate proxy apply failure ({apply_error}) with direct mode"
                        )
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
        Err(error) if is_no_active_network_service(&error) => {
            warn!("no active network service, nothing to clear: {error:#}");
            Ok(())
        }
        result => result,
    }
}

#[cfg(target_os = "macos")]
fn is_no_active_network_service(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<NoActiveNetworkService>().is_some())
}

#[cfg(not(target_os = "macos"))]
const fn is_no_active_network_service(_error: &anyhow::Error) -> bool {
    false
}

#[cfg(target_os = "macos")]
pub async fn apply_proxy_or_direct(
    config: Option<&MacosProxyConfig>,
) -> anyhow::Result<ProxyApplyOutcome> {
    let config = config.cloned();
    tokio::task::spawn_blocking(move || apply_proxy_or_direct_with(config.as_ref(), apply_real))
        .await
        .context("proxy apply task failed")?
}

#[cfg(not(target_os = "macos"))]
pub async fn apply_proxy_or_direct(
    config: Option<&MacosProxyConfig>,
) -> anyhow::Result<ProxyApplyOutcome> {
    match config {
        None => Ok(ProxyApplyOutcome::NotRequested),
        Some(_) => anyhow::bail!("macOS proxy configuration is unsupported on this platform"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoProxy, ProxyBackend, ProxyCall, SystemProxy, apply_proxy_or_direct_with,
        apply_with_backend, validate_proxy_config,
    };
    use crate::{MacosProxyConfig, ProxyApplyOutcome};

    #[derive(Default)]
    struct RecordingBackend {
        calls: Vec<ProxyCall>,
    }

    impl ProxyBackend for RecordingBackend {
        fn set_system_proxy(&mut self, proxy: SystemProxy) -> anyhow::Result<()> {
            self.calls.push(ProxyCall::System(proxy));
            Ok(())
        }

        fn set_auto_proxy(&mut self, proxy: AutoProxy) -> anyhow::Result<()> {
            self.calls.push(ProxyCall::Auto(proxy));
            Ok(())
        }
    }

    #[test]
    fn production_proxy_backend_has_no_external_command_dependency() {
        let manifest = include_str!("../../Cargo.toml");
        let source = include_str!("proxy.rs");
        let legacy_binary = concat!("network", "setup");
        let legacy_dependency = concat!("sys", "proxy");
        let synchronous_command = concat!("process::", "Command");
        let asynchronous_command = concat!("tokio::", "process");

        assert!(!manifest.contains(legacy_dependency));
        assert!(manifest.contains("system-configuration = \"0.7\""));
        assert!(!source.contains(legacy_binary));
        assert!(!source.contains(synchronous_command));
        assert!(!source.contains(asynchronous_command));
        assert!(source.contains("SCNetworkProtocolSetConfiguration"));
        assert!(source.contains("SCPreferencesCommitChanges"));
        assert!(source.contains("SCPreferencesApplyChanges"));
    }

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
            assert!(
                validate_proxy_config(&config).is_err(),
                "accepted {config:?}"
            );
        }
    }

    #[test]
    fn proxy_apply_order_disables_the_conflicting_mode_first() {
        let mut backend = RecordingBackend::default();
        apply_with_backend(
            &mut backend,
            &MacosProxyConfig::Global {
                host: "127.0.0.1".to_owned(),
                port: 7897,
                bypass: "localhost".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            backend.calls,
            [
                ProxyCall::Auto(AutoProxy::default()),
                ProxyCall::System(SystemProxy {
                    host: "127.0.0.1".to_owned(),
                    port: 7897,
                    bypass: "localhost".to_owned(),
                    enable: true,
                }),
            ]
        );

        backend.calls.clear();
        apply_with_backend(
            &mut backend,
            &MacosProxyConfig::Pac {
                url: "http://[::1]:33221/commands/pac".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            backend.calls,
            [
                ProxyCall::System(SystemProxy::default()),
                ProxyCall::Auto(AutoProxy {
                    url: "http://[::1]:33221/commands/pac".to_owned(),
                    enable: true,
                }),
            ]
        );

        backend.calls.clear();
        apply_with_backend(&mut backend, &MacosProxyConfig::Disabled).unwrap();
        assert_eq!(
            backend.calls,
            [
                ProxyCall::System(SystemProxy::default()),
                ProxyCall::Auto(AutoProxy::default()),
            ]
        );
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
            Err(anyhow::Error::new(super::NoActiveNetworkService))
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
                Err(anyhow::Error::new(super::NoActiveNetworkService))
            }
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("failed to compensate proxy apply failure"));
    }
}
