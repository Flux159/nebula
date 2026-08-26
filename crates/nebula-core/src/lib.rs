//! nebula-core: the embeddable engine behind Nebula.
//!
//! Provides the `VmmBackend` abstraction with two implementations on macOS:
//! - `vz`: Virtualization.framework — primary Vessel VM (Rosetta amd64, balloon)
//! - `krun`: libkrun — GPU/sandbox sidecar microVMs
//!
//! KVM (Linux) and WHP (Windows) backends slot in behind the same trait later.

pub mod backend;
pub mod display;
pub mod dns;
pub mod error;
pub mod home;
pub mod initramfs;
pub mod ipc;
pub mod proto;
pub mod spec;
pub mod vessels;

pub use backend::{VmHandle, VmState, VmmBackend};
pub use error::{Error, Result};
pub use spec::{BootSpec, ConsoleSpec, DiskSpec, NetSpec, ShareSpec, VmSpec, VsockPortMap};
