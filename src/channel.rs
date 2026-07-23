#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelIdentity {
    pub id: &'static str,
    pub service_slug: &'static str,
    pub windows_service_name: &'static str,
    pub service_display_name: &'static str,
    pub macos_app_bundle_id: &'static str,
    pub macos_service_id: &'static str,
}

#[cfg(not(feature = "development-channel"))]
pub const CHANNEL_IDENTITY: ChannelIdentity = ChannelIdentity {
    id: "production",
    service_slug: "clash-verge-service",
    windows_service_name: "clash_verge_service",
    service_display_name: "Clash Verge Service",
    macos_app_bundle_id: "io.github.clash-verge-rev.clash-verge-rev",
    macos_service_id: "io.github.clash-verge-rev.clash-verge-rev.service",
};

#[cfg(feature = "development-channel")]
pub const CHANNEL_IDENTITY: ChannelIdentity = ChannelIdentity {
    id: "development",
    service_slug: "clash-verge-service-dev",
    windows_service_name: "clash_verge_service_dev",
    service_display_name: "Clash Verge Development Service",
    macos_app_bundle_id: "io.github.clash-verge-rev.clash-verge-rev.dev",
    macos_service_id: "io.github.clash-verge-rev.clash-verge-rev.dev.service",
};

pub const SERVICE_SLUG: &str = CHANNEL_IDENTITY.service_slug;
pub const WINDOWS_SERVICE_NAME: &str = CHANNEL_IDENTITY.windows_service_name;
pub const SERVICE_DISPLAY_NAME: &str = CHANNEL_IDENTITY.service_display_name;
pub const MACOS_APP_BUNDLE_ID: &str = CHANNEL_IDENTITY.macos_app_bundle_id;
pub const MACOS_SERVICE_ID: &str = CHANNEL_IDENTITY.macos_service_id;

#[cfg(test)]
mod tests {
    use super::CHANNEL_IDENTITY;

    #[test]
    fn compiled_channel_has_a_self_consistent_identity() {
        assert!(!CHANNEL_IDENTITY.id.is_empty());
        assert!(
            CHANNEL_IDENTITY
                .service_slug
                .starts_with("clash-verge-service")
        );
        assert!(
            CHANNEL_IDENTITY
                .windows_service_name
                .starts_with("clash_verge_service")
        );
        assert!(
            CHANNEL_IDENTITY
                .macos_service_id
                .starts_with(CHANNEL_IDENTITY.macos_app_bundle_id)
        );
    }
}
