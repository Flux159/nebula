//! HTTP → Engine translation. Path matching is segment-based after stripping
//! an optional `/vX.Y` version prefix. Every unimplemented route returns a
//! clean 501 with a message (never a hang or panic).

use crate::engine::EngineRef;
use crate::inspect;
use crate::{archive, build, streams};
use slim_http::Ctx;
use std::io::Write;

pub fn handle(engine: &EngineRef, ctx: &mut Ctx) {
    let path = ctx.head.path.clone();
    let method = ctx.head.method.clone();
    let stripped = strip_version(&path);
    let segs: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();

    let res = route(engine, ctx, &method, &segs);
    if let Err(e) = res {
        if !ctx.responded {
            let (code, msg) = error_status(&e);
            let _ = ctx.respond_error(code, msg);
        }
    }
}

type R = std::io::Result<()>;

fn route(engine: &EngineRef, ctx: &mut Ctx, method: &str, segs: &[&str]) -> R {
    match (method, segs) {
        ("GET", ["_ping"]) | ("HEAD", ["_ping"]) => ping(ctx),
        ("GET", ["version"]) => ctx.respond_json(200, &inspect::version()),
        ("GET", ["info"]) => ctx.respond_json(200, &inspect::info(engine)),
        ("GET", ["events"]) => events(engine, ctx),
        ("POST", ["auth"]) => auth(ctx),

        // ----- containers -----
        ("GET", ["containers", "json"]) => list_containers(engine, ctx),
        ("POST", ["containers", "create"]) => create_container(engine, ctx),
        ("GET", ["containers", id, "json"]) => {
            let c = engine.get_entry(id)?.snapshot();
            ctx.respond_json(200, &inspect::container(engine, &c))
        }
        ("POST", ["containers", id, "start"]) => {
            engine.start(id)?;
            ctx.respond_empty(204)
        }
        ("POST", ["containers", id, "stop"]) => {
            let t = ctx.head.query_str("t").and_then(|s| s.parse().ok()).unwrap_or(10);
            engine.stop(id, t)?;
            ctx.respond_empty(204)
        }
        ("POST", ["containers", id, "restart"]) => {
            let t = ctx.head.query_str("t").and_then(|s| s.parse().ok()).unwrap_or(10);
            engine.restart(id, t)?;
            ctx.respond_empty(204)
        }
        ("POST", ["containers", id, "kill"]) => {
            let sig = ctx.head.query_str("signal").unwrap_or("SIGKILL").to_string();
            engine.kill(id, &sig)?;
            ctx.respond_empty(204)
        }
        ("POST", ["containers", id, "wait"]) => wait_container(engine, ctx, id),
        ("POST", ["containers", id, "rename"]) => {
            let name = ctx.head.query_str("name").unwrap_or("").to_string();
            engine.rename(id, &name)?;
            ctx.respond_empty(204)
        }
        ("DELETE", ["containers", id]) => {
            let force = ctx.head.query_bool("force");
            let v = ctx.head.query_bool("v");
            engine.remove(id, force, v)?;
            ctx.respond_empty(204)
        }
        ("GET", ["containers", id, "logs"]) => logs(engine, ctx, id),
        ("POST", ["containers", id, "attach"]) => attach(engine, ctx, id),
        ("GET", ["containers", id, "stats"]) => stats(engine, ctx, id),
        ("GET", ["containers", id, "top"]) => top(engine, ctx, id),
        ("GET", ["containers", id, "archive"]) | ("HEAD", ["containers", id, "archive"]) => {
            archive::get(engine, ctx, id, method == "HEAD")
        }
        ("PUT", ["containers", id, "archive"]) => archive::put(engine, ctx, id),
        ("POST", ["containers", id, "exec"]) => exec_create(engine, ctx, id),
        ("POST", ["containers", _id, "pause"]) | ("POST", ["containers", _id, "unpause"]) => {
            ctx.respond_error(501, "pause/unpause is not supported in slim")
        }
        ("POST", ["containers", "prune"]) => prune_containers(engine, ctx),

        // ----- exec -----
        ("POST", ["exec", id, "start"]) => exec_start(engine, ctx, id),
        ("GET", ["exec", id, "json"]) => exec_inspect(engine, ctx, id),
        ("POST", ["exec", id, "resize"]) => exec_resize(engine, ctx, id),

        // ----- images -----
        ("GET", ["images", "json"]) => list_images(engine, ctx),
        ("POST", ["images", "create"]) => pull_image(engine, ctx),
        ("GET", ["images", rest @ ..]) if rest.last() == Some(&"json") => {
            let name = rest[..rest.len() - 1].join("/");
            image_inspect(engine, ctx, &name)
        }
        ("GET", ["images", rest @ ..]) if rest.last() == Some(&"history") => {
            ctx.respond_json(200, &Vec::<serde_json::Value>::new())
        }
        ("POST", ["images", rest @ ..]) if rest.last() == Some(&"tag") => {
            let name = rest[..rest.len() - 1].join("/");
            tag_image(engine, ctx, &name)
        }
        ("POST", ["images", rest @ ..]) if rest.last() == Some(&"push") => {
            let _ = rest;
            ctx.respond_error(501, "image push is not yet supported in slim")
        }
        ("DELETE", ["images", rest @ ..]) => {
            let name = rest.join("/");
            delete_image(engine, ctx, &name)
        }
        ("POST", ["images", "prune"]) => {
            ctx.respond_json(200, &serde_json::json!({"ImagesDeleted": [], "SpaceReclaimed": 0}))
        }
        ("POST", ["build"]) => build::handle(engine, ctx),
        ("POST", ["commit"]) => ctx.respond_error(501, "commit is not yet supported in slim"),

        // ----- networks -----
        ("GET", ["networks"]) => list_networks(engine, ctx),
        ("POST", ["networks", "create"]) => create_network(engine, ctx),
        ("GET", ["networks", id]) => {
            let n = engine.net.get(id).ok_or_else(|| not_found(format!("network {id} not found")))?;
            ctx.respond_json(200, &inspect::network(engine, &n))
        }
        ("DELETE", ["networks", id]) => {
            engine.net.remove(id)?;
            ctx.respond_empty(204)
        }
        ("POST", ["networks", id, "connect"]) => network_connect(engine, ctx, id, true),
        ("POST", ["networks", id, "disconnect"]) => network_connect(engine, ctx, id, false),
        ("POST", ["networks", "prune"]) => {
            ctx.respond_json(200, &serde_json::json!({"NetworksDeleted": []}))
        }

        // ----- volumes -----
        ("GET", ["volumes"]) => {
            let vols = engine.volumes.list();
            ctx.respond_json(200, &slim_api::volume::VolumeListResponse { volumes: vols, warnings: vec![] })
        }
        ("POST", ["volumes", "create"]) => {
            let req: slim_api::volume::VolumeCreateRequest = ctx.body_json()?;
            let v = engine.volumes.create(&req.name, req.labels)?;
            ctx.respond_json(201, &v)
        }
        ("GET", ["volumes", name]) => {
            let v = engine.volumes.get(name).ok_or_else(|| not_found(format!("no such volume: {name}")))?;
            ctx.respond_json(200, &v)
        }
        ("DELETE", ["volumes", name]) => {
            engine.volumes.remove(name, ctx.head.query_bool("force"))?;
            ctx.respond_empty(204)
        }
        ("POST", ["volumes", "prune"]) => {
            ctx.respond_json(200, &serde_json::json!({"VolumesDeleted": [], "SpaceReclaimed": 0}))
        }

        // ----- system -----
        ("GET", ["system", "df"]) => ctx.respond_json(200, &inspect::system_df(engine)),
        ("POST", ["system", "prune"]) => {
            ctx.respond_json(200, &serde_json::json!({"ContainersDeleted":[],"ImagesDeleted":[],"SpaceReclaimed":0}))
        }

        _ => ctx.respond_error(
            501,
            format!("slim does not implement {} {}", ctx.head.method, ctx.head.path),
        ),
    }
}

// ---------- system ----------

fn ping(ctx: &mut Ctx) -> R {
    // docker CLI reads Api-Version/Docker-Experimental headers off _ping.
    let body = b"OK";
    let head = format!(
        "HTTP/1.1 200 OK\r\nApi-Version: {}\r\nDocker-Experimental: false\r\nOSType: linux\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        slim_api::API_VERSION,
        body.len()
    );
    use std::io::Write as _;
    ctx.responded = true;
    let mut raw = ctx.raw_writer()?;
    raw.write_all(head.as_bytes())?;
    raw.write_all(body)?;
    raw.flush()
}

fn auth(ctx: &mut Ctx) -> R {
    let _cfg: slim_api::system::AuthConfig = ctx.body_json()?;
    // We don't validate here; the registry call later will. Report success so
    // `docker login` stores the credentials client-side.
    ctx.respond_json(200, &serde_json::json!({"Status": "Login Succeeded"}))
}

fn events(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let rx = engine.subscribe_events();
    let mut w = ctx.stream(200, "application/json")?;
    // Stream until the client disconnects.
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(ev) => {
                let mut line = serde_json::to_vec(&ev).unwrap_or_default();
                line.push(b'\n');
                if w.write_all(&line).is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !w.peer_alive() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

// ---------- containers ----------

fn list_containers(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let all = ctx.head.query_bool("all");
    let containers = engine.list(all);
    let summaries: Vec<_> = containers.iter().map(|c| inspect::summary(engine, c)).collect();
    // Apply name/status/label filters if present (lenient).
    let filtered = inspect::apply_container_filters(summaries, ctx.head.query_str("filters"));
    ctx.respond_json(200, &filtered)
}

fn create_container(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let name = ctx.head.query_str("name").map(|s| s.to_string());
    let req: slim_api::container::ContainerCreateRequest = ctx.body_json()?;
    let id = engine.create(&req, name.as_deref())?;
    ctx.respond_json(201, &slim_api::container::ContainerCreateResponse { id, warnings: vec![] })
}

/// `/wait` must send the 200 response HEADERS immediately, then the
/// `{"StatusCode":N}` body when the container exits. The docker (moby) client's
/// `ContainerWait` blocks until it receives those headers before it proceeds to
/// `POST /start` — sending headers only at exit deadlocks `docker run`.
fn wait_container(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    // Resolve up front so a missing container is a clean 404 (before headers).
    let _ = engine.get_entry(id)?;
    let mut w = ctx.stream(200, "application/json")?;
    let code = engine.wait(id).unwrap_or(-1);
    let body = serde_json::to_vec(&slim_api::container::WaitResponse {
        status_code: code as i64,
        error: None,
    })
    .unwrap_or_default();
    w.write_all(&body)?;
    w.finish()
}

fn prune_containers(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let mut deleted = Vec::new();
    for c in engine.list(true) {
        if c.state.status == "exited" {
            if engine.remove(&c.id, false, false).is_ok() {
                deleted.push(c.id);
            }
        }
    }
    ctx.respond_json(200, &serde_json::json!({"ContainersDeleted": deleted, "SpaceReclaimed": 0}))
}

fn logs(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let entry = engine.get_entry(id)?;
    let (log_path, tty) = {
        let c = entry.c.lock().unwrap();
        (c.log_path.clone(), c.config.tty)
    };
    let follow = ctx.head.query_bool("follow");
    let stdout = ctx.head.query_str("stdout").map(|v| v == "1" || v == "true").unwrap_or(true);
    let stderr = ctx.head.query_str("stderr").map(|v| v == "1" || v == "true").unwrap_or(true);
    let timestamps = ctx.head.query_bool("timestamps");
    let tail = match ctx.head.query_str("tail") {
        Some("all") | None => None,
        Some(n) => n.parse::<usize>().ok(),
    };
    let opts = slim_runtime::jsonlog::LogReadOpts { stdout, stderr, tail, since: None, until: None, timestamps };

    let mut w = ctx.stream(200, "application/vnd.docker.raw-stream")?;
    let mut pos = 0u64;
    pos = slim_runtime::jsonlog::read_log(std::path::Path::new(&log_path), &opts, pos, |stream, bytes| {
        let s = if stream == "stderr" { 2 } else { 1 };
        let _ = write_log_frame(&mut w, tty, s, bytes);
    })
    .unwrap_or(pos);

    if !follow {
        return Ok(());
    }
    // Follow: poll the file + watch for exit.
    let follow_opts = slim_runtime::jsonlog::LogReadOpts { tail: None, ..opts };
    loop {
        let running = entry.c.lock().unwrap().running();
        let new_pos = slim_runtime::jsonlog::read_log(
            std::path::Path::new(&log_path),
            &follow_opts,
            pos,
            |stream, bytes| {
                let s = if stream == "stderr" { 2 } else { 1 };
                let _ = write_log_frame(&mut w, tty, s, bytes);
            },
        )
        .unwrap_or(pos);
        if new_pos == pos {
            if !running {
                break;
            }
            if !w.peer_alive() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        pos = new_pos;
    }
    Ok(())
}

fn write_log_frame(w: &mut impl Write, tty: bool, stream: u8, bytes: &[u8]) -> std::io::Result<()> {
    if tty {
        w.write_all(bytes)
    } else {
        w.write_all(&streams::frame(stream, bytes))
    }
}

fn attach(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let entry = engine.get_entry(id)?;
    let rt = entry.rt.lock().unwrap().clone();
    let tty = rt.tty;
    let want_stdin = ctx.head.query_bool("stdin");
    let want_stdout = ctx.head.query_str("stdout").map(|v| v == "1" || v == "true").unwrap_or(true);
    let want_stderr = ctx.head.query_str("stderr").map(|v| v == "1" || v == "true").unwrap_or(true);

    let (sock, _buffered) = ctx.hijack(!tty)?;
    let sock_in = sock.try_clone()?;
    let rt2 = rt.clone();
    let in_thread = if want_stdin {
        Some(std::thread::spawn(move || streams::pump_socket_to_stdin(&rt2, sock_in)))
    } else {
        None
    };
    let alive_entry = entry.clone();
    streams::pump_output_to_socket(&entry, sock, !tty, want_stdout, want_stderr, move || {
        alive_entry.c.lock().map(|c| c.running()).unwrap_or(false)
    });
    if let Some(t) = in_thread {
        let _ = t.join();
    }
    Ok(())
}

fn stats(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let entry = engine.get_entry(id)?;
    let stream = ctx.head.query_str("stream").map(|v| v == "1" || v == "true").unwrap_or(true);
    if stream {
        let mut w = ctx.stream(200, "application/json")?;
        loop {
            let s = inspect::stats(engine, &entry);
            let mut line = serde_json::to_vec(&s).unwrap_or_default();
            line.push(b'\n');
            if w.write_all(&line).is_err() || !entry.c.lock().unwrap().running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        Ok(())
    } else {
        let s = inspect::stats(engine, &entry);
        ctx.respond_json(200, &s)
    }
}

fn top(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let _ = engine.get_entry(id)?;
    ctx.respond_json(
        200,
        &serde_json::json!({"Titles": ["PID","CMD"], "Processes": []}),
    )
}

// ---------- exec ----------

fn exec_create(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let cfg: slim_api::exec::ExecConfig = ctx.body_json()?;
    let exec_id = engine.exec_create(id, cfg)?;
    ctx.respond_json(201, &slim_api::exec::ExecCreateResponse { id: exec_id })
}

fn exec_start(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let start: slim_api::exec::ExecStartConfig = ctx.body_json()?;
    engine.exec_start(id, &start, ctx)
}

fn exec_inspect(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let ins = engine.exec_inspect(id)?;
    ctx.respond_json(200, &ins)
}

fn exec_resize(engine: &EngineRef, ctx: &mut Ctx, id: &str) -> R {
    let w = ctx.head.query_str("w").and_then(|s| s.parse().ok()).unwrap_or(80);
    let h = ctx.head.query_str("h").and_then(|s| s.parse().ok()).unwrap_or(24);
    engine.exec_resize(id, w, h);
    ctx.respond_empty(200)
}

// ---------- images ----------

fn list_images(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let imgs = engine.store.list();
    let summaries: Vec<_> = imgs.iter().map(|i| inspect::image_summary(engine, i)).collect();
    ctx.respond_json(200, &summaries)
}

fn pull_image(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let from = ctx.head.query_str("fromImage").unwrap_or("").to_string();
    let tag = ctx.head.query_str("tag").unwrap_or("").to_string();
    if from.is_empty() {
        return ctx.respond_error(400, "fromImage required (slim does not support import)");
    }
    let reference = if tag.is_empty() || from.contains('@') {
        from
    } else {
        format!("{from}:{tag}")
    };
    let auth = registry_auth(ctx);
    let mut w = ctx.stream(200, "application/json")?;
    let mut emit = |ev: slim_image::PullEvent| {
        let msg = pull_event_to_json(ev);
        let mut line = serde_json::to_vec(&msg).unwrap_or_default();
        line.push(b'\n');
        let _ = w.write_all(&line);
    };
    match engine.pull(&reference, auth, &mut emit) {
        Ok(_) => Ok(()),
        Err(e) => {
            let err = slim_api::ProgressMessage { error: Some(e.to_string()), ..Default::default() };
            let mut line = serde_json::to_vec(&err).unwrap_or_default();
            line.push(b'\n');
            let _ = w.write_all(&line);
            Ok(())
        }
    }
}

fn pull_event_to_json(ev: slim_image::PullEvent) -> slim_api::ProgressMessage {
    match ev {
        slim_image::PullEvent::Status(s) => slim_api::ProgressMessage { status: Some(s), ..Default::default() },
        slim_image::PullEvent::LayerStatus { id, status, current, total } => slim_api::ProgressMessage {
            id: Some(id),
            status: Some(status),
            progress_detail: Some(slim_api::ProgressDetail {
                current: Some(current as i64),
                total: Some(total),
            }),
            ..Default::default()
        },
    }
}

fn image_inspect(engine: &EngineRef, ctx: &mut Ctx, name: &str) -> R {
    let rec = engine.store.resolve(name).ok_or_else(|| not_found(format!("No such image: {name}")))?;
    ctx.respond_json(200, &inspect::image_inspect(engine, &rec))
}

fn tag_image(engine: &EngineRef, ctx: &mut Ctx, name: &str) -> R {
    let repo = ctx.head.query_str("repo").unwrap_or("");
    let tag = ctx.head.query_str("tag").unwrap_or("latest");
    let target = if tag.is_empty() { repo.to_string() } else { format!("{repo}:{tag}") };
    engine.store.tag(name, &target)?;
    ctx.respond_empty(201)
}

fn delete_image(engine: &EngineRef, ctx: &mut Ctx, name: &str) -> R {
    let force = ctx.head.query_bool("force");
    let in_use = |image_id: &str| {
        engine
            .containers
            .lock()
            .unwrap()
            .values()
            .any(|e| e.c.lock().unwrap().image_id == image_id)
    };
    let resp = engine.store.remove(name, force, &in_use)?;
    ctx.respond_json(200, &resp)
}

// ---------- networks ----------

fn list_networks(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let nets: Vec<_> = engine.net.list().iter().map(|n| inspect::network(engine, n)).collect();
    ctx.respond_json(200, &nets)
}

fn create_network(engine: &EngineRef, ctx: &mut Ctx) -> R {
    let req: slim_api::network::NetworkCreateRequest = ctx.body_json()?;
    let rec = engine.net.create(&req.name, req.internal, req.labels)?;
    // New network → new DNS listener on its gateway.
    engine.dns.listen(&rec.gateway());
    ctx.respond_json(
        201,
        &slim_api::network::NetworkCreateResponse { id: rec.id, warning: String::new() },
    )
}

fn network_connect(engine: &EngineRef, ctx: &mut Ctx, id: &str, connect: bool) -> R {
    let req: slim_api::network::NetworkConnectRequest = ctx.body_json()?;
    let entry = engine.get_entry(&req.container)?;
    let c = entry.snapshot();
    let net = engine.net.get(id).ok_or_else(|| not_found(format!("network {id} not found")))?;
    if connect {
        if c.running() {
            let aliases = req
                .endpoint_config
                .as_ref()
                .map(|e| e.aliases.clone())
                .unwrap_or_default();
            engine.net.connect(&net.name, &c.id, &c.name, c.state.pid, &aliases)?;
        }
    } else {
        engine.net.disconnect(&net.name, &c.id);
    }
    ctx.respond_empty(200)
}

// ---------- helpers ----------

fn strip_version(path: &str) -> String {
    // /v1.43/containers/json -> /containers/json
    if let Some(rest) = path.strip_prefix("/v") {
        if let Some(slash) = rest.find('/') {
            let ver = &rest[..slash];
            if ver.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return rest[slash..].to_string();
            }
        }
    }
    path.to_string()
}

fn registry_auth(ctx: &Ctx) -> Option<slim_image::registry::BasicAuth> {
    let header = ctx.head.header("X-Registry-Auth")?;
    let json = slim_image::registry::b64_decode(header);
    let cfg: slim_api::system::AuthConfig = serde_json::from_slice(&json).ok()?;
    if cfg.username.is_empty() {
        return None;
    }
    Some(slim_image::registry::BasicAuth { username: cfg.username, password: cfg.password })
}

fn not_found(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, msg)
}

fn error_status(e: &std::io::Error) -> (u16, String) {
    use std::io::ErrorKind::*;
    let code = match e.kind() {
        NotFound => 404,
        AlreadyExists => 409,
        InvalidInput => 400,
        PermissionDenied => 409,
        Unsupported => 501,
        _ => 500,
    };
    (code, e.to_string())
}
