//! Client for the nebulad control socket.

use nebula_core::ipc::{self, IpcStream as UnixStream};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Context;
use nebula_core::proto::{DaemonRequest, DaemonResponse};

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

pub fn control_sock() -> anyhow::Result<PathBuf> {
    Ok(nebula_home()?.join("run/nebulad.sock"))
}

pub fn connect() -> anyhow::Result<UnixStream> {
    let path = control_sock()?;
    ipc::connect(&path)
        .with_context(|| format!("nebulad is not running (no socket at {})", path.display()))
}

pub fn request(req: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let stream = connect()?;
    request_on(stream, req).map(|(resp, _)| resp)
}

/// Send a request and return the response plus the still-open stream
/// (needed by `shell`, which upgrades the connection).
pub fn request_on(
    stream: UnixStream,
    req: &DaemonRequest,
) -> anyhow::Result<(DaemonResponse, (BufReader<UnixStream>, UnixStream))> {
    let mut writer = stream.try_clone()?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line)?;
    let resp: DaemonResponse =
        serde_json::from_str(resp_line.trim()).context("bad response from nebulad")?;
    Ok((resp, (reader, writer)))
}

pub fn daemon_running() -> bool {
    control_sock()
        .map(|p| ipc::connect(&p).is_ok())
        .unwrap_or(false)
}
