//! The engine of icloud-linux: configuration, the sync state database, the
//! local mirror and the synchronisation logic that ties them to iCloud Drive.
//!
//! Nothing in here touches FUSE or a GUI toolkit, so all of it is testable
//! with the in-memory Drive from `icloud-api`.

pub mod config;
pub mod connect;
pub mod dirs;
pub mod engine;
pub mod error;
pub mod fs;
pub mod hydrate;
pub mod installer;
pub mod mirror;
pub mod path;
pub mod policy;
pub mod setup;
pub mod state;
pub mod status;
pub mod sync_request;

pub use config::Config;
pub use dirs::Layout;
pub use engine::{Engine, EngineConfig, EngineStats};
pub use error::{Error, Result};
pub use fs::{Attr, DirItem, Errno, FileKind, FsCore, FsOptions, FsResult};
pub use mirror::Mirror;
pub use path::IcPath;
pub use policy::SyncPolicy;
pub use state::{Entry, SyncState};
