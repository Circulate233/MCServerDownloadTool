//! Transactional installation orchestration and durable install state.

pub(crate) mod atomic;
mod core;
mod filesystem;
mod lock;
mod model;
mod session;
mod state;

pub use core::{InstallCore, Installer};
pub use filesystem::InstallRoot;
pub use model::{
    InstallError, InstallEvent, InstallObserver, InstallObserverError, InstallPlan, InstallResult,
    InstallStage,
};
pub use session::InstallSession;
pub use state::InstallState;
