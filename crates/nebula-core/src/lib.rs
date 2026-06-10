//! nebula-core: the embeddable engine behind Nebula.
//!
//! Provides the `VmmBackend` abstraction with two implementations on macOS:
//! - `vz`: Virtualization.framework — primary Vessel VM (Rosetta amd64, balloon)
//! - `krun`: libkrun — GPU/sandbox sidecar microVMs
//!
//! KVM (Linux) and WHP (Windows) backends slot in behind the same trait later.

pub mod backend;
pub mod error;
pub mod initramfs;
pub mod spec;

pub use backend::{VmHandle, VmState, VmmBackend};
pub use error::{Error, Result};
pub use spec::{BootSpec, ConsoleSpec, DiskSpec, NetSpec, ShareSpec, VmSpec};
