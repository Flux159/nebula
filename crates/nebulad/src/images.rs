//! Build a vessel rootfs from a docker image — nebulad-side.
//!
//! Unlike the CLI flow (which shells the host `docker` binary), this drives
//! the engine's Docker REST API directly over the proxied socket: pull if
//! absent, create a stopped container, stream its filesystem export to the
//! staging dir (visible in the guest via the $HOME share), then run the
//! ext4-build script inside the engine where mkfs lives. No host docker CLI
//! needed — works wherever nebulad runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nebula_core::proto::{AgentRequest, AgentResponse};

use crate::vessel::Vessel;

/// One request against the engine docker socket. Bodies are tiny JSON both
/// ways except export (streamed by the caller).
async fn docker_req(
    sock: &Path,
    method: Method,
    path: &str,
    body: Option<Bytes>,
) -> anyhow::Result<Response<Incoming>> {
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(sock)
        .await
        .with_context(|| format!("docker socket {}", sock.display()))?;
    #[cfg(windows)]
    let stream = {
        let port: u16 = std::fs::read_to_string(sock)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .context("docker port file unreadable")?;
        tokio::net::TcpStream::connect(("127.0.0.1", port)).await?
    };
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "docker")
        .header(hyper::header::CONTENT_TYPE, "application/json");
    let req: Request<http_body_util::combinators::BoxBody<Bytes, hyper::Error>> = match body {
        Some(b) => builder.body(Full::new(b).map_err(|n| match n {}).boxed())?,
        None => builder.body(Empty::<Bytes>::new().map_err(|n| match n {}).boxed())?,
    };
    Ok(sender.send_request(req).await?)
}

async fn drain(mut body: Incoming) -> anyhow::Result<()> {
    while let Some(frame) = body.frame().await {
        let _ = frame?;
    }
    Ok(())
}

/// Pull + export `image` and assemble a bootable vessel rootfs in `dir`
/// (rootfs.img + data.img). Mirrors the CLI's --from-image build.
pub async fn build_rootfs_from_image(
    vessel: Arc<Vessel>,
    docker_sock: PathBuf,
    image: String,
    name: String,
    dir: PathBuf,
    rootfs_mb: u64,
    data_gib: u64,
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME not set")?;
    let stage = PathBuf::from(&host_home)
        .join(".nebula-image-build")
        .join(&name);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;

    let cleanup = |stage: &Path| {
        let _ = std::fs::remove_dir_all(stage);
    };

    // Local images work without a registry: only pull when absent.
    let inspect = docker_req(
        &docker_sock,
        Method::GET,
        &format!("/v1.43/images/{image}/json"),
        None,
    )
    .await?;
    if inspect.status() != StatusCode::OK {
        drain(inspect.into_body()).await.ok();
        let pull = docker_req(
            &docker_sock,
            Method::POST,
            &format!("/v1.43/images/create?fromImage={}", urlish(&image)),
            None,
        )
        .await?;
        let status = pull.status();
        // The pull streams progress JSON; wait for it to finish. Keep the
        // tail — on failure it carries dockerd's actual error message.
        let body = pull.collect().await?.to_bytes();
        if status != StatusCode::OK {
            cleanup(&stage);
            let msg = String::from_utf8_lossy(&body);
            let msg = msg.trim();
            let tail = &msg[msg.len().saturating_sub(300)..];
            bail!("image `{image}` could not be pulled (HTTP {status}): {tail}");
        }
        let again = docker_req(
            &docker_sock,
            Method::GET,
            &format!("/v1.43/images/{image}/json"),
            None,
        )
        .await?;
        let ok = again.status() == StatusCode::OK;
        drain(again.into_body()).await.ok();
        if !ok {
            cleanup(&stage);
            bail!("image `{image}` is not available after pull");
        }
    } else {
        drain(inspect.into_body()).await.ok();
    }

    // Stopped container -> filesystem export (tar) streamed to the stage.
    let create = docker_req(
        &docker_sock,
        Method::POST,
        "/v1.43/containers/create",
        Some(Bytes::from(serde_json::to_vec(&serde_json::json!({
            "Image": image,
            "Cmd": ["/bin/true"],
        }))?)),
    )
    .await?;
    if create.status() != StatusCode::CREATED {
        let body = create.collect().await?.to_bytes();
        cleanup(&stage);
        bail!("docker create failed: {}", String::from_utf8_lossy(&body));
    }
    let created: serde_json::Value = serde_json::from_slice(&create.collect().await?.to_bytes())?;
    let cid = created["Id"]
        .as_str()
        .context("docker create returned no Id")?
        .to_string();

    let export_res = async {
        let export = docker_req(
            &docker_sock,
            Method::GET,
            &format!("/v1.43/containers/{cid}/export"),
            None,
        )
        .await?;
        anyhow::ensure!(
            export.status() == StatusCode::OK,
            "docker export failed (HTTP {})",
            export.status()
        );
        let mut out = tokio::fs::File::create(stage.join("export.tar")).await?;
        let mut body = export.into_body();
        use tokio::io::AsyncWriteExt;
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                out.write_all(&data).await?;
            }
        }
        out.flush().await?;
        Ok(())
    }
    .await;
    let _ = docker_req(
        &docker_sock,
        Method::DELETE,
        &format!("/v1.43/containers/{cid}"),
        None,
    )
    .await;
    if let Err(e) = export_res {
        cleanup(&stage);
        return Err(e);
    }

    // ext4 assembly inside the engine (it has mkfs + our static guest bins);
    // same script as the CLI flow.
    let script = format!(
        r#"set -e
STAGE='{stage}'; SIZE_MB={rootfs_mb}; DATA_MB={data_mb}
BUILD=/var/lib/nebula/img-build-{name}
rm -rf "$BUILD"; mkdir -p "$BUILD/root"
tar -xf "$STAGE/export.tar" -C "$BUILD/root"
cp /sbin/nebula-init "$BUILD/root/sbin/nebula-init"
cp /usr/bin/vessel-agent "$BUILD/root/usr/bin/vessel-agent" 2>/dev/null || {{ mkdir -p "$BUILD/root/usr/bin"; cp /usr/bin/vessel-agent "$BUILD/root/usr/bin/vessel-agent"; }}
mkdir -p "$BUILD/root/var/lib/nebula" "$BUILD/root/run" "$BUILD/root/tmp" "$BUILD/root/proc" "$BUILD/root/sys" "$BUILD/root/dev"
truncate -s ${{SIZE_MB}}M "$BUILD/rootfs.img"
mkfs.ext4 -q -L nebula-root -d "$BUILD/root" "$BUILD/rootfs.img"
truncate -s ${{DATA_MB}}M "$BUILD/data.img"
mkfs.ext4 -q -L nebula-data "$BUILD/data.img"
mv "$BUILD/rootfs.img" "$STAGE/rootfs.img"
mv "$BUILD/data.img" "$STAGE/data.img"
rm -rf "$BUILD"
"#,
        stage = stage.display(),
        data_mb = data_gib * 1024,
    );
    let exec = tokio::task::spawn_blocking(move || {
        vessel.agent_request_long(
            &AgentRequest::Exec {
                cmd: "/bin/sh".into(),
                args: vec!["-c".into(), script],
                env: vec![],
                timeout_ms: 900_000,
            },
            Duration::from_secs(910),
        )
    })
    .await??;
    match exec {
        AgentResponse::Exec(r) if r.exit_code == 0 => {}
        AgentResponse::Exec(r) => {
            cleanup(&stage);
            bail!("image build failed:\n{}{}", r.stdout, r.stderr);
        }
        other => {
            cleanup(&stage);
            bail!("unexpected agent response: {other:?}");
        }
    }

    for f in ["rootfs.img", "data.img"] {
        let src = stage.join(f);
        let dst = dir.join(f);
        if std::fs::rename(&src, &dst).is_err() {
            // Cross-volume fallback: sparse, these are disk images.
            nebula_core::sparse::copy_sparse(&src, &dst)?;
            let _ = std::fs::remove_file(&src);
        }
    }
    cleanup(&stage);
    Ok(())
}

/// Minimal query escaping for image refs (':' and '/' are accepted by
/// dockerd in fromImage; '+' and spaces are not expected in refs).
fn urlish(s: &str) -> String {
    s.replace('%', "%25")
        .replace('&', "%26")
        .replace(' ', "%20")
}
