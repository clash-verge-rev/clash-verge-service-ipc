use std::sync::Arc;

use anyhow::Result;
use flexi_logger::{Cleanup, FileSpec, Naming, writers::FileLogWriter};

use once_cell::sync::OnceCell;
use tokio::sync::Mutex;

use crate::core::structure::WriterConfig;

type SharedWriter = Arc<Mutex<FileLogWriter>>;
static GLOBAL_WRITER: OnceCell<SharedWriter> = OnceCell::new();

pub fn service_writer(config: &WriterConfig) -> Result<FileLogWriter> {
    Ok(FileLogWriter::builder(
        FileSpec::default()
            .directory(config.directory.clone())
            .basename("service")
            .suppress_timestamp(),
    )
    .format(clash_verge_logger::file_format_without_level)
    .rotate(
        flexi_logger::Criterion::Size(config.max_log_size),
        Naming::TimestampsCustomFormat {
            current_infix: Some("latest"),
            format: "%Y-%m-%d_%H-%M-%S",
        },
        Cleanup::KeepLogFiles(config.max_log_files),
    )
    .try_build()?)
}

pub async fn set_or_update_writer(config: &WriterConfig) -> Result<()> {
    let new_writer = service_writer(config)?;

    if let Some(shared) = GLOBAL_WRITER.get() {
        *shared.lock().await = new_writer;
        Ok(())
    } else {
        GLOBAL_WRITER
            .set(Arc::new(Mutex::new(new_writer)))
            .map_err(|_| anyhow::anyhow!("failed to init writer"))
    }
}

pub fn get_writer() -> Option<SharedWriter> {
    GLOBAL_WRITER.get().cloned()
}
