//! Builds or refreshes the runtime directory used by a core.
//! Preparation writes only after the old core stops; staging updates a live generation and
//! therefore plans first and declines whenever it cannot preserve consistency.

mod assets;
mod readback;
mod staging;

pub(crate) use assets::{PreparedRuntime, approved_core_copy, prepare_runtime, validate_core_path};
pub(crate) use readback::read_runtime_file;
pub(crate) use staging::stage_runtime;

pub(crate) fn validate_tun_guard(
    yaml: &str,
    guard: Option<&crate::TunGuardConfig>,
) -> Result<(), crate::core::auth::ServiceError> {
    let Some(guard) = guard else {
        return Ok(());
    };
    let rejected = |detail| crate::core::auth::ServiceError::new(crate::ServiceErrorCode::TunGuardFailed, detail);
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).map_err(|_| rejected("TUN protection requires a valid YAML configuration"))?;
    let root = yaml
        .as_mapping()
        .ok_or_else(|| rejected("TUN protection requires a configuration mapping"))?;
    let interface = root.get("interface-name").and_then(serde_yaml_ng::Value::as_str);
    let tun = root.get("tun").and_then(serde_yaml_ng::Value::as_mapping);
    if interface != Some(guard.interface_name.as_str())
        || tun
            .and_then(|tun| tun.get("enable"))
            .and_then(serde_yaml_ng::Value::as_bool)
            != Some(true)
        || tun
            .and_then(|tun| tun.get("auto-detect-interface"))
            .and_then(serde_yaml_ng::Value::as_bool)
            != Some(false)
    {
        return Err(rejected(
            "TUN protection must match an enabled TUN with the selected interface-name and auto-detect-interface disabled",
        ));
    }
    Ok(())
}

pub(crate) async fn validate_tun_guard_file(config: &crate::ClashConfig) -> anyhow::Result<()> {
    if config.tun_guard.is_some() {
        let yaml = tokio::fs::read_to_string(&config.core_config.config_path).await?;
        validate_tun_guard(&yaml, config.tun_guard.as_ref())?;
    }
    Ok(())
}

#[cfg(test)]
mod tun_guard_tests {
    #[tokio::test]
    async fn persisted_guard_is_validated_without_changing_network_state() -> anyhow::Result<()> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tun-guard-policy-{}-{timestamp}", std::process::id()));
        tokio::fs::create_dir(&root).await?;
        let path = root.join("config.yaml");
        let mut config = crate::ClashConfig::default();
        config.core_config.config_path = path.to_string_lossy().into_owned();
        config.tun_guard = Some(crate::TunGuardConfig {
            interface_name: "WLAN".into(),
        });
        tokio::fs::write(
            &path,
            "interface-name: WLAN\ntun:\n  enable: true\n  auto-detect-interface: false\n",
        )
        .await?;
        super::validate_tun_guard_file(&config).await?;
        tokio::fs::write(
            &path,
            "interface-name: Ethernet\ntun:\n  enable: true\n  auto-detect-interface: false\n",
        )
        .await?;
        assert!(super::validate_tun_guard_file(&config).await.is_err());
        config.tun_guard = None;
        tokio::fs::remove_file(&path).await?;
        super::validate_tun_guard_file(&config).await?;
        tokio::fs::remove_dir(root).await?;
        Ok(())
    }
}
