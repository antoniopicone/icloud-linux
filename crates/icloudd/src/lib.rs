//! The iCloud Drive FUSE daemon.
//!
//! `adapter` is the thin layer over the kernel; the rules live in
//! `icloud-core`. `daemon::run` is what the `icloudd` binary calls.

mod adapter;
pub mod daemon;
mod inodes;
pub mod logging;

pub use adapter::IcloudFs;
