use std::sync::Once;

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
