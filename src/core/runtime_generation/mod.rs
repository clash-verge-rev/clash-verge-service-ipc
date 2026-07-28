//! A runtime generation: the directory a core is started against, and the rules for building a
//! new one or refreshing the one a core is already running in.
//!
//! The two verbs differ in when they run, not in what they know. `prepare_runtime` builds a
//! generation nothing is reading yet, so it may write freely and clean up by deleting the whole
//! directory. `stage_runtime` rewrites a generation a live core holds open, so every decision it
//! makes has to be defensible against that core reading the file mid-write — which is why it
//! plans first, and why it declines rather than guesses.
//!
//! They share their validation, their path derivation and their manifest format, and that sharing
//! is the reason this is one module: the two halves are mutually recursive and neither can be
//! understood alone. What leaves the module is only what a caller needs to start or restage a
//! core; the twenty-seven items the halves lend each other stay inside.

mod assets;
mod staging;

pub(crate) use assets::{PreparedRuntime, prepare_runtime};
pub(crate) use staging::stage_runtime;
