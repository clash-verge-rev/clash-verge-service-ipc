use std::sync::Arc;

use anyhow::Result;
use flexi_logger::{Cleanup, FileSpec, Naming, writers::FileLogWriter};

use tokio::sync::Mutex;

use crate::core::structure::WriterConfig;

pub type SharedWriter = Arc<Mutex<FileLogWriter>>;

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
