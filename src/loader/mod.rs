//! Exact loader installation commands and launch-output verification.

mod execute;
mod model;
mod verify;

pub use execute::{LoaderExecutor, SystemProcessRunner};
pub use model::{
    InstallerArtifact, InstallerSha1, LoaderError, LoaderFamily, LoaderInstallation,
    LoaderOutputExpectation, LoaderPlan, ProcessObserver, ProcessObserverError, ProcessRequest,
    ProcessRunner, ProcessStream, VerifiedLaunch,
};
pub use verify::verify_loader_output;
