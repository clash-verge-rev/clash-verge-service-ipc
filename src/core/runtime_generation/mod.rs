//! Builds or refreshes the runtime directory used by a core.
//! Preparation writes only after the old core stops; staging updates a live generation and
//! therefore plans first and declines whenever it cannot preserve consistency.

mod assets;
mod staging;

pub(crate) use assets::{PreparedRuntime, prepare_runtime};
pub(crate) use staging::stage_runtime;
