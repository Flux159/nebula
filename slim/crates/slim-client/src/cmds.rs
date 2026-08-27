//! docker subcommand implementations against the slim Engine API.

use crate::args::{self, Parsed};
use crate::format as fmt;
use crate::http::{demux_stdcopy, Client};
use crate::tty;
use serde_json::{json, Value};
use std::io::{Read, Write};

/// URL-encode a query value (kept local to avoid a slim-http dep).
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug)]
pub enum CmdError {
    /// Already printed something; exit with this code.
    Handled(i32),
    /// Print "Error: {msg}" and exit 1.
    Msg(String),
}
pub type CmdResult = Result<(), CmdError>;

fn msg(s: impl Into<String>) -> CmdError {
    CmdError::Msg(s.into())
}
impl From<crate::http::ApiError> for CmdError {
    fn from(e: crate::http::ApiError) -> Self {
        CmdError::Msg(e.message)
    }
}
impl From<std::io::Error> for CmdError {
    fn from(e: std::io::Error) -> Self {
        CmdError::Msg(e.to_string())
    }
}

const V: &str = "/v1.43";

// ---------- system ----------

pub fn version(client: &Client) -> CmdResult {
    let v: slim_api::system::VersionResponse = client.json("GET", &format!("{V}/version"), None)?;
    println!("Client: docker-slim");
    println!(" Version:    slim-0.1.0");
    println!(" API version: {}", v.api_version);
    println!();
    println!("Server: nebula-slim");
    println!(" Engine:");
    println!("  Version:    {}", v.version);
    println!(
        "  API version: {} (minimum {})",
        v.api_version, v.min_api_version
    );
    println!("  OS/Arch:    {}/{}", v.os, v.arch);
    Ok(())
}

pub fn info(client: &Client) -> CmdResult {
    let i: slim_api::system::InfoResponse = client.json("GET", &format!("{V}/info"), None)?;
    println!("Client:");
    println!(" Version: slim-0.1.0");
    println!();
    println!("Server:");
    println!(" Containers: {}", i.containers);
    println!("  Running: {}", i.containers_running);
    println!("  Stopped: {}", i.containers_stopped);
    println!(" Images: {}", i.images);
    println!(" Server Version: {}", i.server_version);
    println!(" Storage Driver: {}", i.driver);
    println!(" Operating System: {}", i.operating_system);
    println!(" OSType: {}", i.os_type);
    println!(" Architecture: {}", i.architecture);
    println!(" CPUs: {}", i.ncpu);
    println!(" Total Memory: {}", fmt::human_size(i.mem_total));
    println!(" Name: {}", i.name);
    Ok(())
}

// ---------- images ----------

pub fn pull(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(cargs, &[], &[], &[], false)?;
    let image = p
        .positional
        .first()
        .ok_or_else(|| msg("\"pull\" requires exactly 1 argument"))?;
    pull_image(client, image)?;
    println!("{image}");
    Ok(())
}

/// Shared image pull with progress, used by pull/run.
pub fn pull_image(client: &Client, image: &str) -> CmdResult {
    let (from, tag) = split_image_tag(image);
    let path = format!(
        "{V}/images/create?fromImage={}&tag={}",
        url_encode(&from),
        url_encode(&tag)
    );
    let mut resp = client.request(
        "POST",
        &path,
        &[("X-Registry-Auth", &auth_header(&from))],
        Some(b""),
    )?;
    if !(200..300).contains(&resp.status) {
        let body = resp.read_body().unwrap_or_default();
        return Err(msg(String::from_utf8_lossy(&body).into_owned()));
    }
    let mut buf = Vec::new();
    let mut failed = None;
    resp.stream_body(|chunk| buf.extend_from_slice(chunk))?;
    for line in buf.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        if let Ok(m) = serde_json::from_slice::<slim_api::ProgressMessage>(line) {
            if let Some(e) = m.error {
                failed = Some(e);
            } else if let Some(s) = m.status {
                match m.id {
                    Some(id) => println!("{id}: {s}"),
                    None => println!("{s}"),
                }
            }
        }
    }
    if let Some(e) = failed {
        return Err(msg(e));
    }
    Ok(())
}

/// `docker load [-i FILE] [-q]` — import a docker-save / OCI-layout archive
/// (plain or gzipped). Reads stdin when no `-i` is given, exactly like docker.
pub fn load(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-q", "--quiet"],
        &["-i", "--input"],
        &[("-i", "--input"), ("-q", "--quiet")],
        false,
    )?;
    let quiet = p.flag("-q");
    let path = format!("{V}/images/load?quiet={}", if quiet { "1" } else { "0" });
    let headers = [("Content-Type", "application/x-tar")];

    // A file gets a real Content-Length and is streamed from disk; stdin has
    // no length, so it is spooled to a temp file first (the engine has to seek
    // the archive anyway — the manifest is written last).
    let mut resp = match p.first("-i").filter(|f| *f != "-") {
        Some(file) => {
            let mut f = std::fs::File::open(file).map_err(|e| msg(format!("open {file}: {e}")))?;
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            client.request_reader("POST", &path, &headers, len, &mut f)?
        }
        None => {
            let tmp = std::env::temp_dir().join(format!("docker-slim-load-{}", std::process::id()));
            let mut f = std::fs::File::create(&tmp)?;
            std::io::copy(&mut std::io::stdin().lock(), &mut f)?;
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            drop(f);
            let mut f = std::fs::File::open(&tmp)?;
            let r = client.request_reader("POST", &path, &headers, len, &mut f);
            let _ = std::fs::remove_file(&tmp);
            r?
        }
    };
    if !(200..300).contains(&resp.status) {
        let body = resp.read_body().unwrap_or_default();
        let text = String::from_utf8_lossy(&body).trim().to_string();
        // An empty body is what a proxy in front of a not-yet-listening engine
        // returns; "Error: " on its own tells the user nothing.
        return Err(msg(if text.is_empty() {
            format!("engine returned HTTP {} with no message", resp.status)
        } else {
            text
        }));
    }
    let mut buf = Vec::new();
    resp.stream_body(|chunk| buf.extend_from_slice(chunk))?;
    let mut failed = None;
    for line in buf.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        if let Ok(m) = serde_json::from_slice::<slim_api::ProgressMessage>(line) {
            if let Some(e) = m.error {
                failed = Some(e);
            } else if let Some(s) = m.stream {
                print!("{s}");
            }
        }
    }
    if let Some(e) = failed {
        return Err(msg(e));
    }
    Ok(())
}

pub fn save(_client: &Client, _cargs: &[String]) -> CmdResult {
    Err(msg(
        "save is not supported by the slim engine: layers are stored unpacked, so \
         the original layer tars (and their digests) no longer exist. Produce the \
         archive with `docker save` where the image was built, then `docker-slim \
         load -i <archive>` here.",
    ))
}

pub fn push(_client: &Client, _cargs: &[String]) -> CmdResult {
    Err(msg("push is not yet supported by the slim engine"))
}

pub fn images(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-q", "--all", "--no-trunc"],
        &["--format", "--filter"],
        &[("-q", "--quiet"), ("-a", "--all"), ("-f", "--filter")],
        false,
    )?;
    let list: Vec<slim_api::image::ImageSummary> =
        client.json("GET", &format!("{V}/images/json"), None)?;
    if let Some(f) = p.first("format") {
        for img in &list {
            let v = serde_json::to_value(img).unwrap_or(Value::Null);
            println!("{}", fmt::apply_format(f, &v).map_err(msg)?);
        }
        return Ok(());
    }
    if p.flag("quiet") {
        for img in &list {
            println!("{}", fmt::short_id(&img.id));
        }
        return Ok(());
    }
    let mut rows = Vec::new();
    for img in &list {
        for tag in img.repo_tags.iter().filter(|t| !t.is_empty()) {
            let (repo, t) = tag.rsplit_once(':').unwrap_or((tag.as_str(), "latest"));
            rows.push(vec![
                repo.to_string(),
                t.to_string(),
                fmt::short_id(&img.id),
                fmt::relative_time(img.created),
                fmt::human_size(img.size),
            ]);
        }
    }
    print!(
        "{}",
        fmt::table(&["REPOSITORY", "TAG", "IMAGE ID", "CREATED", "SIZE"], &rows)
    );
    Ok(())
}

pub fn tag(client: &Client, cargs: &[String]) -> CmdResult {
    if cargs.len() < 2 {
        return Err(msg("\"tag\" requires exactly 2 arguments"));
    }
    let (repo, t) = split_image_tag(&cargs[1]);
    let path = format!(
        "{V}/images/{}/tag?repo={}&tag={}",
        cargs[0],
        url_encode(&repo),
        url_encode(&t)
    );
    client.action("POST", &path, None)?;
    Ok(())
}

pub fn rmi(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(cargs, &["--force"], &[], &[("-f", "--force")], false)?;
    let force = p.flag("force");
    for name in &p.positional {
        let path = format!("{V}/images/{name}?force={force}");
        let resp: Vec<slim_api::image::ImageDeleteResponse> = client.json("DELETE", &path, None)?;
        for r in resp {
            if let Some(u) = r.untagged {
                println!("Untagged: {u}");
            }
            if let Some(d) = r.deleted {
                println!("Deleted: {d}");
            }
        }
    }
    Ok(())
}

pub fn image_sub(client: &Client, cargs: &[String]) -> CmdResult {
    match cargs.first().map(|s| s.as_str()) {
        Some("ls") | Some("list") | None => images(client, cargs.get(1..).unwrap_or(&[])),
        Some("pull") => pull(client, &cargs[1..]),
        Some("rm") => rmi(client, &cargs[1..]),
        Some("inspect") => inspect(client, &cargs[1..]),
        Some("tag") => tag(client, &cargs[1..]),
        Some("load") => load(client, &cargs[1..]),
        Some("save") => save(client, &cargs[1..]),
        Some(o) => Err(msg(format!("unknown image command: {o}"))),
    }
}

// ---------- container lifecycle ----------

pub fn create(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse_run_flags(cargs)?;
    let (id, _) = do_create(client, &p, true)?;
    println!("{id}");
    Ok(())
}

pub fn run(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse_run_flags(cargs)?;
    let detach = p.flag("detach");
    let interactive = p.flag("interactive");
    let tty_mode = p.flag("tty");
    let auto_rm = p.flag("rm");

    let (id, _) = do_create(client, &p, true)?;

    if detach {
        client.action("POST", &format!("{V}/containers/{id}/start"), None)?;
        println!("{id}");
        return Ok(());
    }

    let code = if interactive || tty_mode {
        attach_and_run(client, &id, interactive, tty_mode)?
    } else {
        // Non-interactive: start, then stream logs to EOF, then read exit code.
        client.action("POST", &format!("{V}/containers/{id}/start"), None)?;
        stream_logs(client, &id, true, false)?;
        wait_code(client, &id)?
    };

    if auto_rm {
        let _ = client.action(
            "DELETE",
            &format!("{V}/containers/{id}?force=true&v=true"),
            None,
        );
    }
    if code != 0 {
        return Err(CmdError::Handled(code));
    }
    Ok(())
}

fn attach_and_run(
    client: &Client,
    id: &str,
    interactive: bool,
    tty_mode: bool,
) -> Result<i32, CmdError> {
    let path = format!(
        "{V}/containers/{id}/attach?stream=1&stdout=1&stderr=1&stdin={}&logs=0",
        if interactive { 1 } else { 0 }
    );
    let mut sock = client.hijack("POST", &path, None)?;
    let _raw = if tty_mode { tty::enter_raw() } else { None };

    // Start after the attach is connected so no early output is lost.
    client.action("POST", &format!("{V}/containers/{id}/start"), None)?;

    // socket -> stdout (demux for non-tty)
    let mut read_sock = sock.try_clone()?;
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        loop {
            match read_sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tty_mode {
                        let _ = out.write_all(&buf[..n]);
                        let _ = out.flush();
                    } else {
                        demux_stdcopy(
                            &buf[..n],
                            |o| {
                                let _ = out.write_all(o);
                                let _ = out.flush();
                            },
                            |e| {
                                let _ = err.write_all(e);
                                let _ = err.flush();
                            },
                        );
                    }
                }
            }
        }
    });

    // stdin -> socket
    if interactive {
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    let _ = reader.join();
    wait_code(client, id)
}

fn wait_code(client: &Client, id: &str) -> Result<i32, CmdError> {
    let w: slim_api::container::WaitResponse =
        client.json("POST", &format!("{V}/containers/{id}/wait"), None)?;
    Ok(w.status_code as i32)
}

pub fn start(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-a", "-i"],
        &[],
        &[("--attach", "-a"), ("--interactive", "-i")],
        false,
    )?;
    for name in &p.positional {
        client.action("POST", &format!("{V}/containers/{name}/start"), None)?;
        println!("{name}");
    }
    Ok(())
}

pub fn stop(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(cargs, &[], &["-t", "--time"], &[("-t", "--time")], false)?;
    let t = p.first("time").unwrap_or("10");
    for name in &p.positional {
        client.action("POST", &format!("{V}/containers/{name}/stop?t={t}"), None)?;
        println!("{name}");
    }
    Ok(())
}

pub fn restart(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(cargs, &[], &["-t", "--time"], &[("-t", "--time")], false)?;
    let t = p.first("time").unwrap_or("10");
    for name in &p.positional {
        client.action(
            "POST",
            &format!("{V}/containers/{name}/restart?t={t}"),
            None,
        )?;
        println!("{name}");
    }
    Ok(())
}

pub fn kill(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &[],
        &["-s", "--signal"],
        &[("-s", "--signal")],
        false,
    )?;
    let sig = p.first("signal").unwrap_or("KILL");
    for name in &p.positional {
        client.action(
            "POST",
            &format!("{V}/containers/{name}/kill?signal={sig}"),
            None,
        )?;
        println!("{name}");
    }
    Ok(())
}

pub fn rm(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-f", "-v"],
        &[],
        &[("--force", "-f"), ("--volumes", "-v")],
        false,
    )?;
    let q = format!("force={}&v={}", p.flag("-f"), p.flag("-v"));
    for name in &p.positional {
        client.action("DELETE", &format!("{V}/containers/{name}?{q}"), None)?;
        println!("{name}");
    }
    Ok(())
}

pub fn wait(client: &Client, cargs: &[String]) -> CmdResult {
    for name in cargs {
        let code = wait_code(client, name)?;
        println!("{code}");
    }
    Ok(())
}

pub fn container_sub(client: &Client, cargs: &[String]) -> CmdResult {
    match cargs.first().map(|s| s.as_str()) {
        Some("ls") | Some("list") | None => ps(client, cargs.get(1..).unwrap_or(&[])),
        Some("run") => run(client, &cargs[1..]),
        Some("rm") => rm(client, &cargs[1..]),
        Some("start") => start(client, &cargs[1..]),
        Some("stop") => stop(client, &cargs[1..]),
        Some("inspect") => inspect(client, &cargs[1..]),
        Some("logs") => logs(client, &cargs[1..]),
        Some("exec") => exec(client, &cargs[1..]),
        Some(o) => Err(msg(format!("unknown container command: {o}"))),
    }
}

// ---------- ps ----------

pub fn ps(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-a", "-q", "--no-trunc"],
        &["--format", "--filter"],
        &[("--all", "-a"), ("--quiet", "-q"), ("-f", "--filter")],
        false,
    )?;
    let all = p.flag("-a");
    let mut path = format!("{V}/containers/json?all={all}");
    if let Some(f) = p.first("filter") {
        // Convert repeated --filter k=v into the server's filters json.
        let mut map = std::collections::BTreeMap::<String, Vec<String>>::new();
        for spec in p.all("filter") {
            if let Some((k, v)) = spec.split_once('=') {
                map.entry(k.to_string()).or_default().push(v.to_string());
            }
        }
        let _ = f;
        path.push_str(&format!(
            "&filters={}",
            url_encode(&serde_json::to_string(&map).unwrap())
        ));
    }
    let list: Vec<slim_api::container::ContainerSummary> = client.json("GET", &path, None)?;

    if let Some(f) = p.first("format") {
        for c in &list {
            let v = serde_json::to_value(c).unwrap_or(Value::Null);
            println!("{}", fmt::apply_format(f, &v).map_err(msg)?);
        }
        return Ok(());
    }
    if p.flag("-q") {
        for c in &list {
            println!("{}", fmt::short_id(&c.id));
        }
        return Ok(());
    }
    let mut rows = Vec::new();
    for c in &list {
        let name = c
            .names
            .first()
            .map(|n| n.trim_start_matches('/'))
            .unwrap_or("");
        rows.push(vec![
            fmt::short_id(&c.id),
            c.image.clone(),
            truncate_cmd(&c.command),
            fmt::relative_time(c.created),
            c.status.clone(),
            ports_summary(c),
            name.to_string(),
        ]);
    }
    print!(
        "{}",
        fmt::table(
            &[
                "CONTAINER ID",
                "IMAGE",
                "COMMAND",
                "CREATED",
                "STATUS",
                "PORTS",
                "NAMES"
            ],
            &rows
        )
    );
    Ok(())
}

fn ports_summary(c: &slim_api::container::ContainerSummary) -> String {
    let mut parts = Vec::new();
    for p in &c.ports {
        if p.public_port != 0 {
            // Show the address the port is actually bound to: "0.0.0.0" for a
            // wildcard publish, but "127.0.0.1" when that is all it is.
            let ip = if p.ip.is_empty() { "0.0.0.0" } else { &p.ip };
            parts.push(format!(
                "{ip}:{}->{}/{}",
                p.public_port, p.private_port, p.typ
            ));
        } else {
            parts.push(format!("{}/{}", p.private_port, p.typ));
        }
    }
    parts.join(", ")
}

// ---------- logs ----------

pub fn logs(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-f", "-t"],
        &["--tail", "--since", "--until"],
        &[("--follow", "-f"), ("--timestamps", "-t")],
        false,
    )?;
    let id = p
        .positional
        .first()
        .ok_or_else(|| msg("\"logs\" requires exactly 1 argument"))?;
    let follow = p.flag("-f");
    let tail = p.first("tail").unwrap_or("all");
    // determine tty
    let tty_mode = container_is_tty(client, id).unwrap_or(false);
    let extra = format!("&tail={tail}&timestamps={}", p.flag("-t"));
    stream_logs_full(client, id, follow, tty_mode, &extra)?;
    Ok(())
}

fn stream_logs(client: &Client, id: &str, follow: bool, _ts: bool) -> CmdResult {
    let tty_mode = container_is_tty(client, id).unwrap_or(false);
    stream_logs_full(client, id, follow, tty_mode, "&tail=all")
}

fn stream_logs_full(
    client: &Client,
    id: &str,
    follow: bool,
    tty_mode: bool,
    extra: &str,
) -> CmdResult {
    let path = format!(
        "{V}/containers/{id}/logs?stdout=1&stderr=1&follow={}{extra}",
        follow
    );
    let mut resp = client.request("GET", &path, &[], None)?;
    if resp.status == 404 {
        return Err(msg(format!("No such container: {id}")));
    }
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    resp.stream_body(|chunk| {
        if tty_mode {
            let _ = out.write_all(chunk);
            let _ = out.flush();
        } else {
            demux_stdcopy(
                chunk,
                |o| {
                    let _ = out.write_all(o);
                    let _ = out.flush();
                },
                |e| {
                    let _ = err.write_all(e);
                    let _ = err.flush();
                },
            );
        }
    })?;
    Ok(())
}

// ---------- exec ----------

pub fn exec(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-i", "-t", "-d"],
        &["-e", "--env", "-w", "--workdir", "-u", "--user"],
        &[
            ("--interactive", "-i"),
            ("--tty", "-t"),
            ("--detach", "-d"),
            ("--env", "-e"),
            ("--workdir", "-w"),
            ("--user", "-u"),
        ],
        true,
    )?;
    if p.positional.is_empty() {
        return Err(msg("\"exec\" requires at least 2 arguments"));
    }
    let id = &p.positional[0];
    let cmd: Vec<String> = p.positional[1..].to_vec();
    if cmd.is_empty() {
        return Err(msg("\"exec\" requires a command to run"));
    }
    let tty_mode = p.flag("-t");
    let interactive = p.flag("-i");
    let detach = p.flag("-d");

    let body = json!({
        "AttachStdin": interactive,
        "AttachStdout": true,
        "AttachStderr": true,
        "Tty": tty_mode,
        "Cmd": cmd,
        "Env": p.all("-e"),
        "WorkingDir": p.first("-w").unwrap_or(""),
        "User": p.first("-u").unwrap_or(""),
    });
    let created: slim_api::exec::ExecCreateResponse =
        client.json("POST", &format!("{V}/containers/{id}/exec"), Some(&body))?;
    let exec_id = created.id;

    if detach {
        client.action(
            "POST",
            &format!("{V}/exec/{exec_id}/start"),
            Some(&json!({"Detach": true})),
        )?;
        return Ok(());
    }

    let start_body = json!({"Detach": false, "Tty": tty_mode});
    let mut sock = client.hijack(
        "POST",
        &format!("{V}/exec/{exec_id}/start"),
        Some(&start_body),
    )?;
    let _raw = if tty_mode { tty::enter_raw() } else { None };

    let mut read_sock = sock.try_clone()?;
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        loop {
            match read_sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tty_mode {
                        let _ = out.write_all(&buf[..n]);
                        let _ = out.flush();
                    } else {
                        demux_stdcopy(
                            &buf[..n],
                            |o| {
                                let _ = out.write_all(o);
                                let _ = out.flush();
                            },
                            |e| {
                                let _ = err.write_all(e);
                                let _ = err.flush();
                            },
                        );
                    }
                }
            }
        }
    });
    if interactive {
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || sock.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        });
    }
    let _ = reader.join();

    let ins: slim_api::exec::ExecInspect =
        client.json("GET", &format!("{V}/exec/{exec_id}/json"), None)?;
    let code = ins.exit_code.unwrap_or(0) as i32;
    if code != 0 {
        return Err(CmdError::Handled(code));
    }
    Ok(())
}

// ---------- inspect ----------

pub fn inspect(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["-s"],
        &["-f", "--format", "--type"],
        &[("--format", "-f"), ("--size", "-s")],
        false,
    )?;
    let format = p.first("-f").map(|s| s.to_string());
    let mut results = Vec::new();
    let mut any_err = false;
    for name in &p.positional {
        // Try container, then image, then network, then volume.
        let v = inspect_one(client, name);
        match v {
            Some(v) => results.push(v),
            None => {
                eprintln!("Error: No such object: {name}");
                any_err = true;
            }
        }
    }
    if let Some(f) = format {
        for v in &results {
            println!("{}", fmt::apply_format(&f, v).map_err(msg)?);
        }
    } else {
        let arr = Value::Array(results);
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
    }
    if any_err {
        return Err(CmdError::Handled(1));
    }
    Ok(())
}

fn inspect_one(client: &Client, name: &str) -> Option<Value> {
    for path in [
        format!("{V}/containers/{name}/json"),
        format!("{V}/images/{name}/json"),
        format!("{V}/networks/{name}"),
        format!("{V}/volumes/{name}"),
    ] {
        if let Ok((200, body)) = client.call("GET", &path, &[], None) {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                return Some(v);
            }
        }
    }
    None
}

// ---------- cp ----------

/// Is this argument a `container:path` rather than a local path?
///
/// The colon is the only marker, which makes every absolute Windows path
/// ambiguous: `C:\\Users\\me` would otherwise parse as container `C`, so a
/// perfectly ordinary `cp C:\\dir container:/dest` was rejected with "one of
/// the paths must be a container path" while looking straight at one. Docker
/// resolves this the same way -- a single letter followed by a colon is a
/// drive, not a container.
///
/// It matters more on Windows than it looks: there is no virtiofs there, so
/// `cp` is the only way to get files into a container, and it did not accept
/// the only kind of absolute path the platform produces.
fn is_remote_path(s: &str) -> bool {
    if !s.contains(':') || s.starts_with('.') || s.starts_with('/') {
        return false;
    }
    // Only on Windows. `c:/etc` is a genuine single-letter container name on
    // Linux and macOS, and docker treats it as one there; the drive-letter
    // reading is correct only where drive letters exist. Applying it
    // everywhere would fix Windows by breaking the other two.
    !(cfg!(windows) && looks_like_drive(s))
}

/// `C:`, `C:\` or `C:/` -- a drive, not a container.
///
/// Deliberately not behind a `cfg`, so it can be tested on any platform; the
/// decision about whether to *apply* it lives in `is_remote_path`.
fn looks_like_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/')
}

pub fn cp(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(cargs, &["-a", "-L"], &[], &[], false)?;
    if p.positional.len() != 2 {
        return Err(msg("\"cp\" requires exactly 2 arguments"));
    }
    let (src, dst) = (&p.positional[0], &p.positional[1]);
    let src_remote = is_remote_path(src);
    let dst_remote = is_remote_path(dst);

    if src_remote && !dst_remote {
        // container:path -> host path
        let (id, cpath) = src.split_once(':').unwrap();
        let path = format!("{V}/containers/{id}/archive?path={}", url_encode(cpath));
        let mut resp = client.request("GET", &path, &[], None)?;
        if resp.status != 200 {
            let b = resp.read_body().unwrap_or_default();
            return Err(msg(String::from_utf8_lossy(&b).into_owned()));
        }
        let mut buf = Vec::new();
        resp.stream_body(|c| buf.extend_from_slice(c))?;
        extract_cp_tar(&buf, dst, cpath)?;
    } else if !src_remote && dst_remote {
        // host path -> container:path
        let (id, cpath) = dst.split_once(':').unwrap();
        let tar = make_cp_tar(src)?;
        // Contents go *into* the destination; a named directory is unpacked
        // beside it, in the parent, as docker does.
        let parent = if is_contents_of(src) {
            cpath.to_string()
        } else {
            parent_dir(cpath)
        };
        let path = format!("{V}/containers/{id}/archive?path={}", url_encode(&parent));
        let (status, body) = client.call(
            "PUT",
            &path,
            &[("Content-Type", "application/x-tar")],
            Some(&tar),
        )?;
        if !(200..300).contains(&status) {
            return Err(msg(String::from_utf8_lossy(&body).into_owned()));
        }
    } else {
        return Err(msg(
            "one of the paths must be a container path (container:path)",
        ));
    }
    Ok(())
}

fn extract_cp_tar(tar: &[u8], dst: &str, src_path: &str) -> CmdResult {
    let dst_p = std::path::Path::new(dst);
    let mut ar = tar::Archive::new(tar);
    // If dst is an existing dir, extract into it; else the single entry maps
    // to dst (rename). docker semantics simplified.
    if dst_p.is_dir() {
        ar.unpack(dst_p).map_err(|e| msg(e.to_string()))?;
    } else {
        let base = std::path::Path::new(src_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        for entry in ar.entries().map_err(|e| msg(e.to_string()))? {
            let mut entry = entry.map_err(|e| msg(e.to_string()))?;
            let ep = entry.path().map_err(|e| msg(e.to_string()))?.into_owned();
            let name = ep.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if ep.to_string_lossy() == base || name == base || ep.components().count() == 1 {
                entry.unpack(dst_p).map_err(|e| msg(e.to_string()))?;
                return Ok(());
            }
        }
        // Fallback: extract into parent.
        let mut ar2 = tar::Archive::new(tar);
        if let Some(parent) = dst_p.parent() {
            ar2.unpack(parent).map_err(|e| msg(e.to_string()))?;
        }
    }
    Ok(())
}

/// Does this source mean "the contents of the directory" rather than the
/// directory itself?
///
/// docker's convention is a trailing `/.`, and it is the only unambiguous way
/// to say it. The backslash form is accepted too, because a Windows caller
/// building the path with its own separator produces `dir\.` and means the
/// same thing.
pub(crate) fn is_contents_of(src: &str) -> bool {
    src.ends_with("/.") || src.ends_with("\\.")
}

fn make_cp_tar(src: &str) -> Result<Vec<u8>, CmdError> {
    let contents_only = is_contents_of(src);
    let src_p = std::path::Path::new(src);
    let mut buf = Vec::new();
    {
        let mut b = tar::Builder::new(&mut buf);
        if contents_only {
            // Each child at the top level, so unpacking at the destination
            // fills it rather than nesting a copy of the source inside it.
            //
            // Without this a trailing `/.` was simply part of the path: the
            // archive was named after the source directory and unpacked into
            // the destination's parent, so `cp conf container:/etc/import`
            // wrote /etc/conf and left /etc/import alone. Nothing failed --
            // the caller got a success and a container that had never seen
            // the files.
            // Strip the two-character suffix rather than calling parent():
            // Path normalises a trailing `.` component away, so parent()
            // climbs a level too far and reads the wrong directory.
            let base = &src[..src.len() - 2];
            let dir = std::path::Path::new(if base.is_empty() { "/" } else { base });
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry.file_name();
                if entry.file_type()?.is_dir() {
                    b.append_dir_all(&name, entry.path())
                        .map_err(|e| msg(e.to_string()))?;
                } else {
                    let mut f = std::fs::File::open(entry.path())?;
                    b.append_file(&name, &mut f)
                        .map_err(|e| msg(e.to_string()))?;
                }
            }
        } else {
            let name = src_p
                .file_name()
                .ok_or_else(|| msg("invalid source path"))?;
            if src_p.is_dir() {
                b.append_dir_all(name, src_p)
                    .map_err(|e| msg(e.to_string()))?;
            } else {
                let mut f = std::fs::File::open(src_p)?;
                b.append_file(name, &mut f)
                    .map_err(|e| msg(e.to_string()))?;
            }
        }
        b.finish().map_err(|e| msg(e.to_string()))?;
    }
    Ok(buf)
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((p, _)) => p.to_string(),
        None => ".".to_string(),
    }
}

// ---------- build ----------

pub fn build(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["--no-cache", "-q", "--pull"],
        &[
            "-t",
            "--tag",
            "-f",
            "--file",
            "--target",
            "--build-arg",
            "--label",
        ],
        &[("--tag", "-t"), ("--file", "-f"), ("--quiet", "-q")],
        false,
    )?;
    let ctx = p.positional.first().map(|s| s.as_str()).unwrap_or(".");
    let ctx_path = std::path::Path::new(ctx);
    if !ctx_path.is_dir() {
        return Err(msg(format!(
            "unable to prepare context: path {ctx} not found"
        )));
    }
    let dockerfile = p.first("-f").unwrap_or("Dockerfile");
    // Dockerfile path relative to context (docker copies it into the context).
    let df_rel = std::path::Path::new(dockerfile)
        .strip_prefix(ctx_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| dockerfile.to_string());

    let tar = build_context_tar(ctx_path, dockerfile)?;

    let mut q = format!("dockerfile={}", url_encode(&df_rel));
    if let Some(t) = p.first("-t") {
        q.push_str(&format!("&t={}", url_encode(t)));
    }
    if let Some(target) = p.first("target") {
        q.push_str(&format!("&target={}", url_encode(target)));
    }
    if p.flag("--no-cache") {
        q.push_str("&nocache=1");
    }
    let bargs: std::collections::BTreeMap<String, String> = p
        .all("--build-arg")
        .iter()
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    if !bargs.is_empty() {
        q.push_str(&format!(
            "&buildargs={}",
            url_encode(&serde_json::to_string(&bargs).unwrap())
        ));
    }

    let path = format!("{V}/build?{q}");
    let mut resp = client.request(
        "POST",
        &path,
        &[("Content-Type", "application/x-tar")],
        Some(&tar),
    )?;
    if !(200..300).contains(&resp.status) {
        let b = resp.read_body().unwrap_or_default();
        return Err(msg(String::from_utf8_lossy(&b).into_owned()));
    }
    let mut pending = Vec::new();
    let mut build_error = None;
    resp.stream_body(|chunk| {
        pending.extend_from_slice(chunk);
        while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            if let Ok(m) =
                serde_json::from_slice::<slim_api::ProgressMessage>(&line[..line.len() - 1])
            {
                if let Some(s) = m.stream {
                    print!("{s}");
                    let _ = std::io::stdout().flush();
                }
                if let Some(e) = m.error {
                    build_error = Some(e);
                }
            }
        }
    })?;
    if let Some(e) = build_error {
        return Err(msg(e));
    }
    Ok(())
}

fn build_context_tar(ctx: &std::path::Path, _dockerfile: &str) -> Result<Vec<u8>, CmdError> {
    // Honor .dockerignore lightly client-side? The server also applies it, so
    // we send everything except .git for size.
    let mut buf = Vec::new();
    {
        let mut b = tar::Builder::new(&mut buf);
        append_dir(&mut b, ctx, ctx).map_err(|e| msg(e.to_string()))?;
        b.finish().map_err(|e| msg(e.to_string()))?;
    }
    Ok(buf)
}

fn append_dir<W: Write>(
    b: &mut tar::Builder<W>,
    base: &std::path::Path,
    dir: &std::path::Path,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let rel = path.strip_prefix(base).unwrap_or(&path);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            append_dir(b, base, &path)?;
        } else {
            let mut f = std::fs::File::open(&path)?;
            b.append_file(rel, &mut f)?;
        }
    }
    Ok(())
}

// ---------- port / stats / events ----------

pub fn port(client: &Client, cargs: &[String]) -> CmdResult {
    let id = cargs
        .first()
        .ok_or_else(|| msg("\"port\" requires at least 1 argument"))?;
    let c: slim_api::container::ContainerInspect =
        client.json("GET", &format!("{V}/containers/{id}/json"), None)?;
    for (port, binds) in &c.network_settings.ports {
        if let Some(binds) = binds {
            for b in binds {
                println!(
                    "{port} -> {}:{}",
                    if b.host_ip.is_empty() {
                        "0.0.0.0"
                    } else {
                        &b.host_ip
                    },
                    b.host_port
                );
            }
        }
    }
    Ok(())
}

pub fn stats(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["--no-stream", "-a", "--all"],
        &["--format"],
        &[("--all", "-a")],
        false,
    )?;
    let names = if p.positional.is_empty() {
        client
            .json::<Vec<slim_api::container::ContainerSummary>>(
                "GET",
                &format!("{V}/containers/json"),
                None,
            )?
            .into_iter()
            .map(|c| c.id)
            .collect()
    } else {
        p.positional.clone()
    };
    println!(
        "{}",
        fmt::table(&["NAME", "CPU %", "MEM USAGE / LIMIT", "PIDS"], &[]).trim_end()
    );
    for name in &names {
        let path = format!("{V}/containers/{name}/stats?stream=false");
        if let Ok(s) = client.json::<slim_api::container::StatsResponse>("GET", &path, None) {
            let mem = format!(
                "{} / {}",
                fmt::human_size(s.memory_stats.usage as i64),
                fmt::human_size(s.memory_stats.limit as i64)
            );
            println!(
                "{:<20} {:<8} {:<20} {}",
                s.name.trim_start_matches('/'),
                "0.00%",
                mem,
                s.pids_stats.current
            );
        }
    }
    Ok(())
}

pub fn events(client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &[],
        &["--since", "--until", "--filter", "--format"],
        &[],
        false,
    )?;
    let _ = &p;
    let mut resp = client.request("GET", &format!("{V}/events"), &[], None)?;
    let mut out = std::io::stdout();
    resp.stream_body(|chunk| {
        let _ = out.write_all(chunk);
        let _ = out.flush();
    })?;
    Ok(())
}

// ---------- login/logout ----------

pub fn login(_client: &Client, cargs: &[String]) -> CmdResult {
    let p = parse(
        cargs,
        &["--password-stdin"],
        &["-u", "--username", "-p", "--password"],
        &[("--username", "-u"), ("--password", "-p")],
        false,
    )?;
    let server = p
        .positional
        .first()
        .cloned()
        .unwrap_or_else(|| "https://index.docker.io/v1/".into());
    let user = p.first("-u").unwrap_or("").to_string();
    let mut pass = p.first("-p").unwrap_or("").to_string();
    if p.flag("--password-stdin") {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).ok();
        pass = s.trim().to_string();
    }
    save_cred(&server, &user, &pass)?;
    println!("Login Succeeded");
    Ok(())
}

pub fn logout(cargs: &[String]) -> CmdResult {
    let server = cargs
        .first()
        .cloned()
        .unwrap_or_else(|| "https://index.docker.io/v1/".into());
    let path = cred_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(auths) = v.get_mut("auths").and_then(|a| a.as_object_mut()) {
                auths.remove(&server);
            }
            let _ = std::fs::write(&path, serde_json::to_vec_pretty(&v).unwrap_or_default());
        }
    }
    println!("Removing login credentials for {server}");
    Ok(())
}

// ---------- volume ----------

pub fn volume(client: &Client, cargs: &[String]) -> CmdResult {
    match cargs.first().map(|s| s.as_str()) {
        Some("create") => {
            let name = cargs.get(1).cloned().unwrap_or_default();
            let v: slim_api::volume::Volume = client.json(
                "POST",
                &format!("{V}/volumes/create"),
                Some(&json!({"Name": name})),
            )?;
            println!("{}", v.name);
            Ok(())
        }
        Some("ls") | Some("list") | None => {
            let resp: slim_api::volume::VolumeListResponse =
                client.json("GET", &format!("{V}/volumes"), None)?;
            let rows: Vec<Vec<String>> = resp
                .volumes
                .iter()
                .map(|v| vec![v.driver.clone(), v.name.clone()])
                .collect();
            print!("{}", fmt::table(&["DRIVER", "VOLUME NAME"], &rows));
            Ok(())
        }
        Some("rm") => {
            for name in &cargs[1..] {
                client.action("DELETE", &format!("{V}/volumes/{name}"), None)?;
                println!("{name}");
            }
            Ok(())
        }
        Some("inspect") => {
            let mut arr = Vec::new();
            for name in &cargs[1..] {
                let v: Value = client.json("GET", &format!("{V}/volumes/{name}"), None)?;
                arr.push(v);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_default()
            );
            Ok(())
        }
        Some(o) => Err(msg(format!("unknown volume command: {o}"))),
    }
}

// ---------- network ----------

pub fn network(client: &Client, cargs: &[String]) -> CmdResult {
    match cargs.first().map(|s| s.as_str()) {
        Some("create") => {
            let p = parse(
                &cargs[1..],
                &["--internal"],
                &["-d", "--driver"],
                &[("-d", "--driver")],
                false,
            )?;
            let name = p
                .positional
                .first()
                .ok_or_else(|| msg("network create requires a name"))?;
            let r: slim_api::network::NetworkCreateResponse = client.json(
                "POST",
                &format!("{V}/networks/create"),
                Some(&json!({"Name": name, "Internal": p.flag("--internal")})),
            )?;
            println!("{}", r.id);
            Ok(())
        }
        Some("ls") | Some("list") | None => {
            let nets: Vec<slim_api::network::NetworkInspect> =
                client.json("GET", &format!("{V}/networks"), None)?;
            let rows: Vec<Vec<String>> = nets
                .iter()
                .map(|n| {
                    vec![
                        fmt::short_id(&n.id),
                        n.name.clone(),
                        n.driver.clone(),
                        n.scope.clone(),
                    ]
                })
                .collect();
            print!(
                "{}",
                fmt::table(&["NETWORK ID", "NAME", "DRIVER", "SCOPE"], &rows)
            );
            Ok(())
        }
        Some("rm") => {
            for name in &cargs[1..] {
                client.action("DELETE", &format!("{V}/networks/{name}"), None)?;
                println!("{name}");
            }
            Ok(())
        }
        Some("inspect") => {
            let mut arr = Vec::new();
            for name in &cargs[1..] {
                arr.push(client.json::<Value>("GET", &format!("{V}/networks/{name}"), None)?);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_default()
            );
            Ok(())
        }
        Some("connect") | Some("disconnect") => {
            let verb = cargs[0].clone();
            if cargs.len() < 3 {
                return Err(msg(format!(
                    "network {verb} requires NETWORK and CONTAINER"
                )));
            }
            let (net, container) = (&cargs[1], &cargs[2]);
            client.action(
                "POST",
                &format!("{V}/networks/{net}/{verb}"),
                Some(&json!({"Container": container})),
            )?;
            Ok(())
        }
        Some(o) => Err(msg(format!("unknown network command: {o}"))),
    }
}

pub fn system(client: &Client, cargs: &[String]) -> CmdResult {
    match cargs.first().map(|s| s.as_str()) {
        Some("info") => info(client),
        Some("df") => {
            let v: Value = client.json("GET", &format!("{V}/system/df"), None)?;
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(())
        }
        Some("prune") => {
            client.action("POST", &format!("{V}/system/prune"), None)?;
            println!("Total reclaimed space: 0B");
            Ok(())
        }
        _ => info(client),
    }
}

// ---------- create body builder ----------

fn parse_run_flags(cargs: &[String]) -> Result<Parsed, CmdError> {
    parse(
        cargs,
        &[
            "-d",
            "-i",
            "-t",
            "--rm",
            "-P",
            "--privileged",
            "--init",
            "--read-only",
            "--net-optional",
        ],
        &[
            "--name",
            "-p",
            "-v",
            "-e",
            "--env-file",
            "-w",
            "-u",
            "--network",
            "--restart",
            "--entrypoint",
            "-h",
            "-l",
            "-m",
            "--cpus",
            "--add-host",
            "--pid",
            "--ipc",
            "--shm-size",
            "--pull",
            "--stop-signal",
            "--memory-swap",
            "--cpu-shares",
            "--mount",
            "--network-alias",
            "--dns",
            "--tmpfs",
        ],
        &[
            ("-d", "--detach"),
            ("-i", "--interactive"),
            ("-t", "--tty"),
            ("-p", "--publish"),
            ("-v", "--volume"),
            ("-e", "--env"),
            ("-w", "--workdir"),
            ("-u", "--user"),
            ("--net", "--network"),
            ("-h", "--hostname"),
            ("-l", "--label"),
            ("-m", "--memory"),
            ("-P", "--publish-all"),
        ],
        true,
    )
}

/// Create a container from parsed run flags. `auto_pull` pulls the image if
/// it isn't present locally.
fn do_create(client: &Client, p: &Parsed, auto_pull: bool) -> Result<(String, bool), CmdError> {
    if p.positional.is_empty() {
        return Err(msg("requires at least 1 argument (the image)"));
    }
    let image = &p.positional[0];
    let cmd: Vec<String> = p.positional[1..].to_vec();

    let body = build_create_body(p, image, &cmd)?;
    let name_q = p
        .first("name")
        .map(|n| format!("?name={}", url_encode(n)))
        .unwrap_or_default();
    let path = format!("{V}/containers/create{name_q}");

    let created: Result<slim_api::container::ContainerCreateResponse, _> =
        client.json("POST", &path, Some(&body));
    match created {
        Ok(c) => Ok((c.id, false)),
        Err(e) if e.message.contains("No such image") && auto_pull => {
            eprintln!("Unable to find image '{image}' locally");
            pull_image(client, image)?;
            let c: slim_api::container::ContainerCreateResponse =
                client.json("POST", &path, Some(&body))?;
            Ok((c.id, true))
        }
        Err(e) => Err(e.into()),
    }
}

fn build_create_body(p: &Parsed, image: &str, cmd: &[String]) -> Result<Value, CmdError> {
    let tty = p.flag("-t");
    let interactive = p.flag("-i");

    // env: -e KEY=VAL or KEY (inherit), --env-file
    let mut env = Vec::new();
    for e in p.all("-e") {
        if e.contains('=') {
            env.push(e);
        } else if let Ok(v) = std::env::var(&e) {
            env.push(format!("{e}={v}"));
        }
    }
    for f in p.all("env-file") {
        if let Ok(content) = std::fs::read_to_string(&f) {
            for line in content.lines() {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with('#') {
                    env.push(line.to_string());
                }
            }
        }
    }

    // ports
    let (port_bindings, exposed) = parse_ports(&p.all("-p"));

    // labels
    let mut labels = serde_json::Map::new();
    for l in p.all("-l") {
        let (k, v) = l.split_once('=').unwrap_or((l.as_str(), ""));
        labels.insert(k.to_string(), Value::String(v.to_string()));
    }
    // --net-optional: opt out of strict networking — start without a network
    // (with a warning) if the address pool is exhausted, instead of failing.
    // Carried to slimd as a label (keep in sync with engine::NET_OPTIONAL_LABEL).
    if p.flag("--net-optional") {
        labels.insert(
            "io.nebula.slim.net-optional".to_string(),
            Value::String("true".to_string()),
        );
    }

    // restart
    let (restart_name, restart_max) = parse_restart(p.first("restart").unwrap_or(""));

    let mut mounts = Vec::new();
    for m in p.all("--mount") {
        mounts.push(parse_mount(&m)?);
    }
    let mut tmpfs = serde_json::Map::new();
    for t in p.all("--tmpfs") {
        let (path, opts) = t.split_once(':').unwrap_or((t.as_str(), ""));
        tmpfs.insert(path.to_string(), Value::String(opts.to_string()));
    }

    let mut host_config = json!({
        "Binds": p.all("-v"),
        "Mounts": mounts,
        "Tmpfs": tmpfs,
        "Dns": p.all("--dns"),
        "PortBindings": port_bindings,
        "PublishAllPorts": p.flag("-P"),
        "NetworkMode": p.first("network").unwrap_or("bridge"),
        "RestartPolicy": {"Name": restart_name, "MaximumRetryCount": restart_max},
        "Privileged": p.flag("--privileged"),
        "ReadonlyRootfs": p.flag("--read-only"),
        "ExtraHosts": p.all("add-host"),
        "ShmSize": parse_size(p.first("shm-size").unwrap_or("0")),
        "Memory": parse_size(p.first("-m").unwrap_or("0")),
    });
    if let Some(cpus) = p.first("cpus") {
        if let Ok(c) = cpus.parse::<f64>() {
            host_config["NanoCpus"] = json!((c * 1e9) as i64);
        }
    }
    if let Some(init) = p.flag("--init").then_some(true) {
        host_config["Init"] = json!(init);
    }
    if let Some(pid) = p.first("pid") {
        host_config["Pid"] = json!(pid);
    }

    let mut config = json!({
        "Image": image,
        "Cmd": cmd,
        "Env": env,
        "Tty": tty,
        "OpenStdin": interactive,
        "AttachStdin": interactive,
        "AttachStdout": true,
        "AttachStderr": true,
        "Labels": labels,
        "ExposedPorts": exposed,
        "HostConfig": host_config,
    });
    if let Some(w) = p.first("-w") {
        config["WorkingDir"] = json!(w);
    }
    if let Some(u) = p.first("-u") {
        config["User"] = json!(u);
    }
    if let Some(h) = p.first("-h") {
        config["Hostname"] = json!(h);
    }
    if let Some(ep) = p.first("entrypoint") {
        // string form → wrap; docker accepts "" to reset.
        config["Entrypoint"] = json!([ep]);
    }
    if let Some(sig) = p.first("stop-signal") {
        config["StopSignal"] = json!(sig);
    }
    let aliases = p.all("--network-alias");
    if !aliases.is_empty() {
        let net = p.first("network").unwrap_or("bridge").to_string();
        config["NetworkingConfig"] = json!({"EndpointsConfig": {net: {"Aliases": aliases}}});
    }
    Ok(config)
}

/// `[ip:][hostPort:]containerPort[/proto]` → (PortBindings, ExposedPorts).
///
/// The host IP is preserved: `-p 127.0.0.1:6900:6900` publishes on loopback
/// only, and reporting it as `0.0.0.0` would be a lie about who can reach the
/// container. IPv6 literals come bracketed (`[::1]:8080:80`).
fn parse_ports(specs: &[String]) -> (Value, Value) {
    let mut bindings: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut exposed = serde_json::Map::new();
    for spec in specs {
        let (spec, proto) = match spec.rsplit_once('/') {
            Some((s, p)) if p == "tcp" || p == "udp" => (s, p),
            _ => (spec.as_str(), "tcp"),
        };
        // Split off a bracketed IPv6 host first; the rest is colon-separated.
        let (host_ip, rest) = match spec.strip_prefix('[').and_then(|r| r.split_once("]:")) {
            Some((ip, rest)) => (ip.to_string(), rest.to_string()),
            None => {
                let parts: Vec<&str> = spec.split(':').collect();
                match parts.as_slice() {
                    [ip, h, c] => (ip.to_string(), format!("{h}:{c}")),
                    _ => (String::new(), spec.to_string()),
                }
            }
        };
        let (host_port, cport) = match rest.split_once(':') {
            Some((h, c)) => (h.to_string(), c.to_string()),
            None => (String::new(), rest.clone()),
        };
        if cport.is_empty() {
            continue;
        }
        let key = format!("{cport}/{proto}");
        exposed.insert(key.clone(), json!({}));
        // Docker keeps every binding for a port (`-p 127.0.0.1:80:80 -p
        // 192.168.1.5:80:80` is two), so append rather than replace.
        let entry = bindings.entry(key).or_insert_with(|| json!([]));
        if let Some(arr) = entry.as_array_mut() {
            arr.push(json!({"HostIp": host_ip, "HostPort": host_port}));
        }
    }
    (Value::Object(bindings), Value::Object(exposed))
}

/// `--mount type=bind,source=/a,target=/b,readonly` → an Engine API Mount.
fn parse_mount(spec: &str) -> Result<Value, CmdError> {
    let mut typ = "volume".to_string();
    let mut source = String::new();
    let mut target = String::new();
    let mut read_only = false;
    for field in spec.split(',') {
        let (k, v) = match field.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim().to_string()),
            None => (field.trim(), String::new()),
        };
        match k {
            "type" => typ = v,
            "source" | "src" => source = v,
            "target" | "dst" | "destination" => target = v,
            "readonly" | "read-only" | "ro" => read_only = v.is_empty() || v == "true" || v == "1",
            "volume-nocopy" | "bind-propagation" | "consistency" | "bind-nonrecursive" => {}
            other => return Err(msg(format!("invalid field '{other}' in --mount"))),
        }
    }
    if target.is_empty() {
        return Err(msg(format!("--mount requires a target: {spec:?}")));
    }
    Ok(json!({
        "Type": typ,
        "Source": source,
        "Target": target,
        "ReadOnly": read_only,
    }))
}

fn parse_restart(spec: &str) -> (String, i64) {
    match spec.split_once(':') {
        Some((name, max)) => (name.to_string(), max.parse().unwrap_or(0)),
        None => (spec.to_string(), 0),
    }
}

fn parse_size(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return 0;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix(['b', 'B']) {
        (n, 1i64)
    } else if let Some(n) = s.strip_suffix(['k', 'K']) {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix(['m', 'M']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['g', 'G']) {
        (n, 1024 * 1024 * 1024)
    } else {
        (s, 1)
    };
    num.trim()
        .parse::<f64>()
        .map(|v| (v * mult as f64) as i64)
        .unwrap_or(0)
}

// ---------- credentials ----------

fn cred_path() -> std::path::PathBuf {
    let dir = std::env::var("DOCKER_CONFIG")
        .unwrap_or_else(|_| format!("{}/.docker", std::env::var("HOME").unwrap_or_default()));
    std::path::Path::new(&dir).join("config.json")
}

fn save_cred(server: &str, user: &str, pass: &str) -> Result<(), CmdError> {
    let path = cred_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut v: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}));
    let auth = slim_b64(format!("{user}:{pass}").as_bytes());
    v.as_object_mut()
        .unwrap()
        .entry("auths")
        .or_insert_with(|| json!({}));
    v["auths"][server] = json!({"auth": auth});
    std::fs::write(&path, serde_json::to_vec_pretty(&v).unwrap_or_default())?;
    Ok(())
}

fn auth_header(image: &str) -> String {
    // Look up creds for the image's registry; send as X-Registry-Auth.
    let registry = registry_of(image);
    let server_keys = [
        registry.clone(),
        "https://index.docker.io/v1/".to_string(),
        "registry-1.docker.io".to_string(),
    ];
    let creds = std::fs::read(cred_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    if let Some(v) = creds {
        for key in &server_keys {
            if let Some(auth) = v["auths"].get(key).and_then(|a| a["auth"].as_str()) {
                let decoded = slim_b64_decode(auth);
                let s = String::from_utf8_lossy(&decoded);
                if let Some((u, p)) = s.split_once(':') {
                    let cfg = json!({"username": u, "password": p, "serveraddress": registry});
                    return slim_b64(serde_json::to_string(&cfg).unwrap().as_bytes());
                }
            }
        }
    }
    String::new()
}

fn registry_of(image: &str) -> String {
    let first = image.split('/').next().unwrap_or("");
    if first.contains('.') || first.contains(':') || first == "localhost" {
        first.to_string()
    } else {
        "docker.io".to_string()
    }
}

// ---------- small helpers ----------

fn parse(
    cargs: &[String],
    bools: &[&str],
    valued: &[&str],
    aliases: &[(&str, &str)],
    stop: bool,
) -> Result<Parsed, CmdError> {
    args::parse(cargs, bools, valued, aliases, stop).map_err(msg)
}

fn split_image_tag(image: &str) -> (String, String) {
    if let Some(at) = image.find('@') {
        return (image[..at].to_string(), image[at + 1..].to_string());
    }
    match image.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
        _ => (image.to_string(), "latest".to_string()),
    }
}

fn truncate_cmd(cmd: &str) -> String {
    let c = format!("\"{cmd}\"");
    if c.len() > 22 {
        format!("{}…", &c[..21])
    } else {
        c
    }
}

fn container_is_tty(client: &Client, id: &str) -> Option<bool> {
    let c: slim_api::container::ContainerInspect = client
        .json("GET", &format!("{V}/containers/{id}/json"), None)
        .ok()?;
    Some(c.config.tty)
}

// base64 (shared shape with slim-image, kept local to avoid a dep edge).
fn slim_b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn slim_b64_decode(s: &str) -> Vec<u8> {
    let inv = |c: u8| -> i8 {
        match c {
            b'A'..=b'Z' => (c - b'A') as i8,
            b'a'..=b'z' => (c - b'a' + 26) as i8,
            b'0'..=b'9' => (c - b'0' + 52) as i8,
            b'+' => 62,
            b'/' => 63,
            _ => -1,
        }
    };
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        let v = inv(c);
        if v < 0 {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(specs: &[&str]) -> Value {
        let owned: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        parse_ports(&owned).0
    }

    // A colon is the only marker separating a container path from a local
    // one, which makes every absolute Windows path ambiguous. These pin the
    // distinction on all three platforms: the parsing is the same everywhere,
    // so a regression on Windows is caught by a test run on Linux or macOS.
    #[test]
    fn contents_of_is_recognised_in_both_separator_styles() {
        for yes in ["conf/.", "/a/b/.", "conf\\.", "C:\\x\\."] {
            assert!(is_contents_of(yes), "{yes} means contents-of");
        }
        for no in ["conf", "/a/b", "conf/..", "conf/.hidden", "."] {
            assert!(!is_contents_of(no), "{no} does not mean contents-of");
        }
    }

    // The bug this guards: a trailing `/.` was treated as an ordinary path
    // component, so the archive nested a copy of the source inside the
    // destination instead of filling it -- and reported success either way.
    #[test]
    fn contents_of_tars_children_at_the_top_level() {
        let dir = std::env::temp_dir().join(format!("slim-cp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), b"b").unwrap();

        let names = |src: String| -> Vec<String> {
            let tar = make_cp_tar(&src).unwrap();
            let mut ar = tar::Archive::new(std::io::Cursor::new(tar));
            ar.entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
                .collect()
        };

        let contents = names(format!("{}/.", dir.display()));
        assert!(
            contents.iter().any(|n| n == "a.txt"),
            "children should be at the top level, got {contents:?}"
        );
        assert!(
            !contents.iter().any(|n| n.starts_with("slim-cp-test")),
            "the source directory must not appear as a prefix, got {contents:?}"
        );

        let named = names(dir.display().to_string());
        assert!(
            named.iter().all(|n| n.starts_with("slim-cp-test")),
            "without the suffix the directory itself is archived, got {named:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drive_letters_are_recognised_as_drives() {
        for drive in ["C:\\Users\\me\\state\\sql", "c:/Users/me", "D:\\", "Z:"] {
            assert!(looks_like_drive(drive), "{drive} should read as a drive");
        }
        // A container name is not a drive unless it is exactly one letter.
        for not_drive in ["ragnarok-db:/x", "ab:/x", "C", "", ":", "CC:/x"] {
            assert!(
                !looks_like_drive(not_drive),
                "{not_drive} should not read as a drive"
            );
        }
    }

    #[test]
    fn local_paths_are_not_container_paths() {
        for local in ["./relative", "/absolute/unix/path", "relative/no/colon"] {
            assert!(!is_remote_path(local), "{local} should be a local path");
        }
    }

    // The drive rule must not cost Linux and macOS a legal container name.
    #[cfg(not(windows))]
    #[test]
    fn single_letter_container_still_works_off_windows() {
        assert!(
            is_remote_path("c:/etc"),
            "single-letter containers are valid off Windows"
        );
    }

    #[test]
    fn container_paths_are_still_recognised() {
        for remote in [
            "ragnarok-db:/docker-entrypoint-initdb.d",
            "mycontainer:/var/lib",
            "ab:/x",
            "0123456789abcdef:/etc",
        ] {
            assert!(
                is_remote_path(remote),
                "{remote} should be a container path"
            );
        }
    }

    #[test]
    fn host_ip_is_preserved() {
        let b = ports(&["127.0.0.1:6900:6900"]);
        assert_eq!(b["6900/tcp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(b["6900/tcp"][0]["HostPort"], "6900");
    }

    #[test]
    fn plain_and_container_only_forms() {
        let b = ports(&["8080:80", "5432"]);
        assert_eq!(b["80/tcp"][0]["HostIp"], "");
        assert_eq!(b["80/tcp"][0]["HostPort"], "8080");
        assert_eq!(b["5432/tcp"][0]["HostPort"], "");
    }

    #[test]
    fn udp_and_ipv6_and_repeats() {
        let b = ports(&[
            "127.0.0.1:53:53/udp",
            "[::1]:8080:80",
            "192.168.1.5:8080:80",
        ]);
        assert_eq!(b["53/udp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(b["80/tcp"][0]["HostIp"], "::1");
        assert_eq!(b["80/tcp"][1]["HostIp"], "192.168.1.5");
    }

    #[test]
    fn mount_spec_bind_readonly() {
        let m =
            parse_mount("type=bind,source=/Users/me/Application Support/x,target=/conf,readonly")
                .unwrap();
        assert_eq!(m["Type"], "bind");
        assert_eq!(m["Source"], "/Users/me/Application Support/x");
        assert_eq!(m["Target"], "/conf");
        assert_eq!(m["ReadOnly"], true);
    }

    #[test]
    fn mount_spec_rejects_junk() {
        assert!(parse_mount("type=bind,source=/a").is_err());
        assert!(parse_mount("type=bind,srcx=/a,target=/b").is_err());
    }
}
