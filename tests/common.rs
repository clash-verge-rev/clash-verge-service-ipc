use std::sync::Once;

use clash_verge_service_ipc::{OwnerCredentials, test_owner_credentials};

static INIT_TRACING: Once = Once::new();

pub fn init_tracing_for_tests() {
    INIT_TRACING.call_once(|| {
        let _ = tracing_log::LogTracer::init();

        let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "trace".to_string());

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(env_filter))
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_writer(std::io::stderr)
            .try_init();

        if let Err(e) = subscriber {
            eprintln!("tracing_subscriber try_init returned error: {:?}", e);
        }

        tracing::info!("tracing initialized for tests");
    });
}

// Each test binary compiles this module separately and uses a different part of it; a helper one
// binary does not need is not an unused helper.
#[allow(dead_code)]
pub fn owner_credentials() -> OwnerCredentials {
    let app_data_dir =
        std::env::temp_dir().join(format!("service-ipc-owner-{}", std::process::id()));
    test_owner_credentials(&app_data_dir)
        .expect("test owner credentials should be secured for the current user")
}

/// Where `cargo` put a helper binary this test needs to spawn.
///
/// The suite drives real child processes — a mock core, one that crashes, one that never opens
/// its socket — because the properties under test are about a process somebody else is holding
/// open. They are built alongside the tests and land beside them.
#[allow(dead_code)]
pub fn test_bin_path(name: &str) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path
}

/// Wait until the service is answering on its socket.
///
/// `run_ipc_server` returns once it has spawned the listener, not once the listener is bound, so
/// a test that connects immediately races it. Five seconds is far longer than binding takes and
/// short enough that a genuinely dead server fails the test rather than hanging it.
#[allow(dead_code)]
pub async fn wait_for_ipc() -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if clash_verge_service_ipc::connect().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    anyhow::bail!("IPC server did not become ready")
}
