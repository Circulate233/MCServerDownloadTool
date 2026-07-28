//! Pure start-script rendering with ownership-aware atomic publication.

mod render;

pub use render::{
    ScriptError, ScriptOutcome, ScriptPlatform, ScriptRequest, WindowsFailureBehavior,
    write_start_script,
};
