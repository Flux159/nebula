//! `docker cp` — GET/PUT/HEAD /containers/{id}/archive.
//!
//! Operates on the container's filesystem: /proc/<pid>/root while running, or
//! a temporary overlay mount while stopped.

use crate::engine::{Engine, EngineRef};
use slim_http::Ctx;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

type R = io::Result<()>;

/// Resolve a filesystem root for the container, returning the root plus an
/// optional temp-mount dir to unmount when done.
fn container_fs(engine: &Engine, id: &str) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let entry = engine.get_entry(id)?;
    let c = entry.snapshot();
    if c.running() && c.state.pid > 0 {
        return Ok((PathBuf::from(format!("/proc/{}/root", c.state.pid)), None));
    }
    // Stopped: mount the overlay read/write at a temp dir.
    let image = engine
        .store
        .resolve(&c.image_id)
        .ok_or_else(|| io::Error::other("image missing"))?;
    let dir = engine.paths.run.join(format!("cp-{}", slim_net::rand_id()));
    std::fs::create_dir_all(&dir)?;
    let merged = engine.store.prepare_rootfs(&image, &dir)?;
    Ok((merged, Some(dir)))
}

fn cleanup(engine: &Engine, mount: Option<PathBuf>) {
    if let Some(d) = mount {
        engine.store.unmount_rootfs(&d);
        let _ = std::fs::remove_dir_all(d);
    }
}

pub fn get(engine: &EngineRef, ctx: &mut Ctx, id: &str, head_only: bool) -> R {
    let path = ctx.head.query_str("path").unwrap_or("").to_string();
    if path.is_empty() {
        return ctx.respond_error(400, "path parameter required");
    }
    let (root, mount) = container_fs(engine, id)?;
    let target = join_secure(&root, &path);
    if !target.exists() {
        cleanup(engine, mount);
        return ctx.respond_error(
            404,
            format!("Could not find the file {path} in container {id}"),
        );
    }
    // Stat header (docker cp reads this).
    let stat = stat_header(&target, &path);
    if head_only {
        cleanup(engine, mount);
        ctx.responded = true;
        let mut raw = ctx.raw_writer()?;
        let head = format!(
            "HTTP/1.1 200 OK\r\nX-Docker-Container-Path-Stat: {stat}\r\nContent-Length: 0\r\n\r\n"
        );
        return raw.write_all(head.as_bytes());
    }
    // Stream a tar of the target.
    ctx.responded = true;
    let mut raw = ctx.raw_writer()?;
    let head = format!(
        "HTTP/1.1 200 OK\r\nX-Docker-Container-Path-Stat: {stat}\r\nContent-Type: application/x-tar\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    raw.write_all(head.as_bytes())?;
    let mut chunked = ChunkWrite { inner: raw };
    {
        let mut builder = tar::Builder::new(&mut chunked);
        let name = target.file_name().unwrap_or_default();
        if target.is_dir() {
            builder.append_dir_all(name, &target)?;
        } else {
            let mut f = std::fs::File::open(&target)?;
            builder.append_file(name, &mut f)?;
        }
        builder.finish()?;
    }
    chunked.finish()?;
    cleanup(engine, mount);
    Ok(())
}

pub fn put(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let path = ctx.head.query_str("path").unwrap_or("").to_string();
    if path.is_empty() {
        return ctx.respond_error(400, "path parameter required");
    }
    let (root, mount) = container_fs(engine, id)?;
    let dest = join_secure(&root, &path);
    if let Err(e) = std::fs::create_dir_all(&dest) {
        cleanup(engine, mount);
        return ctx.respond_error(400, format!("destination {path}: {e}"));
    }
    let body = ctx.body_vec(2 * 1024 * 1024 * 1024)?;
    let mut ar = tar::Archive::new(&body[..]);
    ar.set_overwrite(true);
    ar.set_preserve_permissions(true);
    let res = ar.unpack(&dest);
    cleanup(engine, mount);
    match res {
        Ok(_) => ctx.respond_empty(200),
        Err(e) => ctx.respond_error(500, format!("extraction failed: {e}")),
    }
}

/// Join a container-absolute path under a root, refusing to escape it.
fn join_secure(root: &Path, path: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for comp in Path::new(path).components() {
        use std::path::Component::*;
        match comp {
            RootDir | Prefix(_) | CurDir => {}
            ParentDir => {
                out.pop();
                if !out.starts_with(root) {
                    out = root.to_path_buf();
                }
            }
            Normal(c) => out.push(c),
        }
    }
    out
}

fn stat_header(target: &Path, path: &str) -> String {
    let meta = std::fs::symlink_metadata(target).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mode = meta
        .as_ref()
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.mode()
        })
        .unwrap_or(0);
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let json = serde_json::json!({
        "name": name,
        "size": size,
        "mode": mode,
        "mtime": slim_runtime::jsonlog::rfc3339_now(),
        "linkTarget": "",
    });
    slim_image::registry::b64(serde_json::to_string(&json).unwrap_or_default().as_bytes())
}

/// Minimal chunked-encoding writer over the raw socket (archive get streams a
/// tar via the borrowed raw writer rather than slim-http's ChunkedWriter,
/// because the tar builder needs an owning &mut Write).
struct ChunkWrite {
    inner: std::os::unix::net::UnixStream,
}
impl Write for ChunkWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.inner, "{:x}\r\n", buf.len())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl ChunkWrite {
    fn finish(&mut self) -> io::Result<()> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()
    }
}
