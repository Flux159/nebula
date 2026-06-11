//! The nebula home directory (`~/.nebula` by default) — the root every
//! component (CLI, nebulad, vessel logic) anchors to.

use std::path::PathBuf;

use anyhow::Context;

pub fn nebula_home() -> anyhow::Result<PathBuf> {
    // NEBULA_HOME lets embedders run a fully isolated instance (own engine,
    // disks, sockets) that can't collide with a user's standalone Nebula.
    if let Some(custom) = std::env::var_os("NEBULA_HOME") {
        return Ok(PathBuf::from(custom));
    }
    // USERPROFILE is the Windows spelling of HOME.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("HOME/USERPROFILE not set")?;
    Ok(PathBuf::from(home).join(".nebula"))
}
