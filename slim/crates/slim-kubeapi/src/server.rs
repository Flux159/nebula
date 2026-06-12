//! Minimal HTTP/1.1 server for the Kubernetes API surface (the apiserver-lite
//! transport). Plain TCP for now; TLS + ServiceAccount projection layer on
//! top later so in-pod operators can connect. Watch responses stream
//! newline-delimited WatchEvents (chunked) — the wire format client-go
//! informers expect.

use crate::proxy::{LogOpts, PodProxy};
use crate::store::{Gone, ResourceInfo, SharedStore, WatchEvent};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

/// Spawn a per-connection handler thread that won't take the listener down with
/// it. `std::thread::spawn` panics on resource exhaustion (EAGAIN/ENOMEM) — in
/// an accept loop that panic kills the listener and drops the apiserver socket
/// (EOF) for every client at once. Builder::spawn returns Err instead; on
/// failure we refuse this one connection and keep serving. Pairs with the
/// process-wide RLIMIT_NOFILE raise at startup (see slim_http::raise_open_file_limit).
pub(crate) fn spawn_conn<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        eprintln!("slim-kubeapi: refusing connection — thread spawn failed ({name}): {e} (raw {:?})", e.raw_os_error());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A served connection. WebSocket exec needs a second, independent reader to
/// carry stdin/resize frames while the main thread writes output frames;
/// `dup_reader` provides one where the transport supports it (unix/tcp). TLS
/// streams can't be cloned, so they return None and exec runs output-only.
pub trait Conn: Read + Write + Send {
    fn dup_reader(&self) -> Option<Box<dyn Read + Send>> {
        None
    }
}
impl Conn for std::net::TcpStream {
    fn dup_reader(&self) -> Option<Box<dyn Read + Send>> {
        self.try_clone().ok().map(|s| Box::new(s) as Box<dyn Read + Send>)
    }
}
impl Conn for std::os::unix::net::UnixStream {
    fn dup_reader(&self) -> Option<Box<dyn Read + Send>> {
        self.try_clone().ok().map(|s| Box::new(s) as Box<dyn Read + Send>)
    }
}
impl Conn for rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream> {}

pub struct ApiServer {
    pub store: SharedStore,
    pub proxy: Option<Arc<dyn PodProxy>>,
}

struct Req {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn is_ws_exec(&self) -> bool {
        self.path.ends_with("/exec")
            && self.header("upgrade").map(|u| u.eq_ignore_ascii_case("websocket")).unwrap_or(false)
    }
}

impl ApiServer {
    pub fn new(store: SharedStore) -> Arc<Self> {
        Arc::new(ApiServer { store, proxy: None })
    }

    /// With a PodProxy, the apiserver serves pods/{}/log and pods/{}/exec from
    /// the proxy (slimd's in-process engine).
    pub fn with_proxy(store: SharedStore, proxy: Arc<dyn PodProxy>) -> Arc<Self> {
        Arc::new(ApiServer { store, proxy: Some(proxy) })
    }

    pub fn serve(self: &Arc<Self>, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let me = self.clone();
            spawn_conn("kubeapi-tcp", move || {
                let _ = me.handle_conn(conn);
            });
        }
        Ok(())
    }

    /// Serve the API over a unix socket (plain HTTP) — for host clients reached
    /// through nebula's socket proxy, mirroring docker.sock. No TLS/kubeconfig.
    pub fn serve_unix(self: &Arc<Self>, path: &str) -> std::io::Result<()> {
        use std::os::unix::net::UnixListener;
        let _ = std::fs::remove_file(path);
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener = UnixListener::bind(path)?;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let me = self.clone();
            spawn_conn("kubeapi-unix", move || {
                let _ = me.handle_conn(conn);
            });
        }
        Ok(())
    }

    /// Generic over the stream so the same handlers serve plain TCP and TLS
    /// (rustls StreamOwned). The full request is read up front, then handlers
    /// write to the stream as `&mut dyn Write`.
    pub fn handle_conn<S: Conn>(&self, mut stream: S) -> std::io::Result<()> {
        let req = {
            let mut reader = BufReader::new(&mut stream);
            match read_request(&mut reader)? {
                Some(r) => r,
                None => return Ok(()),
            }
        };
        // exec is the one duplex handler — it needs the raw Read+Write stream
        // for the WebSocket upgrade, so it can't go through the dyn-Write route.
        if req.is_ws_exec() {
            return self.handle_exec_ws(&req, &mut stream);
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.route(&req, &mut stream)));
        if matches!(res, Ok(Err(_)) | Err(_)) {
            let _ = write_json(&mut stream, 500, &status_obj(500, "internal error", "InternalError"));
        }
        Ok(())
    }

    fn route(&self, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let segs: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
        match (req.method.as_str(), segs.as_slice()) {
            // ---- meta / discovery ----
            ("GET", ["version"]) => write_json(w, 200, &version_info()),
            ("GET", ["api"]) => write_json(w, 200, &json!({"kind":"APIVersions","versions":["v1"],"serverAddressByClientCIDRs":[]})),
            ("GET", ["api", "v1"]) => write_json(w, 200, &self.api_resource_list("", "v1")),
            ("GET", ["apis"]) => write_json(w, 200, &self.api_group_list()),
            ("GET", ["apis", group]) => write_json(w, 200, &self.api_group(group)),
            ("GET", ["apis", group, version]) => write_json(w, 200, &self.api_resource_list(group, version)),
            ("GET", ["healthz"]) | ("GET", ["livez"]) | ("GET", ["readyz"]) => write_text(w, 200, "ok"),
            // OpenAPI v3: kubectl ≥1.27 requires it for client-side `apply`
            // validation (404 → "failed to download openapi"). We serve a
            // discovery doc + per-group-version docs whose schemas are
            // permissive (x-kubernetes-preserve-unknown-fields), so any object
            // validates. Operators use client-go and don't need this.
            ("GET", ["openapi", "v3"]) => write_json(w, 200, &self.openapi_root()),
            ("GET", ["openapi", "v3", "api", "v1"]) => write_json(w, 200, &self.openapi_gv("", "v1")),
            ("GET", ["openapi", "v3", "apis", g, v]) => write_json(w, 200, &self.openapi_gv(g, v)),
            // kubectl's client-side `apply` validator downloads OpenAPI v2 in
            // gnostic protobuf and errors if it 404s. We serve a minimal valid
            // Document (swagger/info/empty paths, no definitions) so the
            // download succeeds and validation finds no schema → skips → stock
            // `kubectl apply` works without --validate=false.
            ("GET", ["openapi", "v2"]) => write_raw(
                w,
                200,
                // The canonical type uses `@v1.0`, but `@` isn't a valid MIME
                // token char so Go's mime.ParseMediaType (in the kubectl
                // client) rejects it. The deprecated dotted form parses
                // cleanly and kubectl still accepts it as proto OpenAPI.
                "application/com.github.proto-openapi.spec.v2.v1.0+protobuf",
                &openapi_v2_proto(),
            ),
            ("GET", ["openapi", ..]) => {
                write_json(w, 404, &status_obj(404, "only openapi v2/v3 are served by slim", "NotFound"))
            }

            // ---- core group: /api/v1/... ----
            ("GET", ["api", "v1", rest @ ..]) => self.handle_resource("", "v1", rest, req, w, Verb::Get),
            ("POST", ["api", "v1", rest @ ..]) => self.handle_resource("", "v1", rest, req, w, Verb::Create),
            ("PUT", ["api", "v1", rest @ ..]) => self.handle_resource("", "v1", rest, req, w, Verb::Update),
            ("PATCH", ["api", "v1", rest @ ..]) => self.handle_resource("", "v1", rest, req, w, Verb::Patch),
            ("DELETE", ["api", "v1", rest @ ..]) => self.handle_resource("", "v1", rest, req, w, Verb::Delete),

            // ---- named groups: /apis/<group>/<version>/... ----
            ("GET", ["apis", g, v, rest @ ..]) => self.handle_resource(g, v, rest, req, w, Verb::Get),
            ("POST", ["apis", g, v, rest @ ..]) => self.handle_resource(g, v, rest, req, w, Verb::Create),
            ("PUT", ["apis", g, v, rest @ ..]) => self.handle_resource(g, v, rest, req, w, Verb::Update),
            ("PATCH", ["apis", g, v, rest @ ..]) => self.handle_resource(g, v, rest, req, w, Verb::Patch),
            ("DELETE", ["apis", g, v, rest @ ..]) => self.handle_resource(g, v, rest, req, w, Verb::Delete),

            _ => write_json(w, 404, &status_obj(404, &format!("not found: {} {}", req.method, req.path), "NotFound")),
        }
    }

    /// rest is everything after the group/version: either
    ///   [resource]                              (cluster list / namespaced via ?)
    ///   [resource, name]
    ///   [namespaces, ns, resource]
    ///   [namespaces, ns, resource, name]
    ///   [namespaces, ns, resource, name, subresource]
    fn handle_resource(&self, group: &str, _version: &str, rest: &[&str], req: &Req, w: &mut dyn Write, verb: Verb) -> std::io::Result<()> {
        let (ns, resource, name, subresource): (Option<String>, &str, Option<&str>, Option<&str>) = match rest {
            ["namespaces", ns, resource] => (Some(ns.to_string()), resource, None, None),
            ["namespaces", ns, resource, name] => (Some(ns.to_string()), resource, Some(name), None),
            ["namespaces", ns, resource, name, sub] => (Some(ns.to_string()), resource, Some(name), Some(sub)),
            [resource] => (None, resource, None, None),
            [resource, name] => (None, resource, Some(name), None),
            [resource, name, sub] => (None, resource, Some(name), Some(sub)),
            _ => return write_json(w, 404, &status_obj(404, "unsupported path", "NotFound")),
        };

        let Some(info) = self.store.lookup(group, resource) else {
            return write_json(w, 404, &status_obj(404, &format!("the server could not find the requested resource ({group}/{resource})"), "NotFound"));
        };

        // The scale subresource needs a real Scale object response (kubectl
        // `scale` decodes it), so route it specially.
        if subresource == Some("scale") {
            return self.handle_scale(verb, &info, &ns, name.unwrap_or(""), req, w);
        }
        // pods/{}/log → stream from the engine via the PodProxy.
        if subresource == Some("log") && resource == "pods" {
            return self.handle_log(&ns, name.unwrap_or(""), req, w);
        }

        let watch = query_bool(&req.query, "watch");

        match verb {
            Verb::Get if name.is_none() && watch => self.do_watch(&info, ns.as_deref(), req, w),
            Verb::Get if name.is_none() => self.do_list(&info, ns.as_deref(), req, w),
            Verb::Get => self.do_get(&info, &ns, name.unwrap(), req, w),
            Verb::Create => self.do_create(&info, &ns, req, w),
            Verb::Update => self.do_update(&info, &ns, name.unwrap_or(""), subresource, req, w),
            Verb::Patch => self.do_patch(&info, &ns, name.unwrap_or(""), subresource, req, w),
            Verb::Delete => self.do_delete(&info, &ns, name.unwrap_or(""), w),
        }
    }

    fn do_list(&self, info: &ResourceInfo, ns: Option<&str>, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let labels = label_selector(&req.query);
        let (items, rv) = self.store.list(info, ns, &labels);
        // kubectl's human-readable `get` requests a server-side Table; without
        // it kubectl falls back to a generic NAME/AGE printer (blank columns).
        if wants_table(req) {
            return write_json(w, 200, &build_table(info, &items, rv));
        }
        let list = json!({
            "apiVersion": info.group_version(),
            "kind": info.list_kind(),
            "metadata": {"resourceVersion": rv.to_string()},
            "items": items,
        });
        write_json(w, 200, &list)
    }

    fn do_get(&self, info: &ResourceInfo, ns: &Option<String>, name: &str, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let nsv = ns.clone().unwrap_or_default();
        match self.store.get(info, &nsv, name) {
            Some(obj) => {
                if wants_table(req) {
                    let rv = obj.pointer("/metadata/resourceVersion").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
                    return write_json(w, 200, &build_table(info, std::slice::from_ref(&obj), rv));
                }
                write_json(w, 200, &obj)
            }
            None => write_json(w, 404, &status_obj(404, &format!("{} \"{name}\" not found", info.singular), "NotFound")),
        }
    }

    fn do_create(&self, info: &ResourceInfo, ns: &Option<String>, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let Ok(obj) = serde_json::from_slice::<Value>(&req.body) else {
            return write_json(w, 400, &status_obj(400, "invalid body", "BadRequest"));
        };
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from);
        let gen = obj.pointer("/metadata/generateName").and_then(|v| v.as_str()).map(String::from);
        let name = match (name, gen) {
            (Some(n), _) => n,
            (None, Some(g)) => format!("{g}{}", rand_suffix()),
            _ => return write_json(w, 422, &status_obj(422, "metadata.name required", "Invalid")),
        };
        let nsv = ns.clone().unwrap_or_else(|| obj.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or("default").to_string());
        // CRD application also registers the new resource type.
        if info.resource == "customresourcedefinitions" {
            self.register_crd(&obj);
        }
        let created = self.store.put(info, obj, &nsv, &name, true);
        write_json(w, 201, &created)
    }

    fn do_update(&self, info: &ResourceInfo, ns: &Option<String>, name: &str, sub: Option<&str>, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let Ok(mut obj) = serde_json::from_slice::<Value>(&req.body) else {
            return write_json(w, 400, &status_obj(400, "invalid body", "BadRequest"));
        };
        let nsv = ns.clone().unwrap_or_default();
        // status subresource update: merge only status into the stored object.
        if sub == Some("status") {
            if let Some(mut existing) = self.store.get(info, &nsv, name) {
                if let Some(st) = obj.get("status").cloned() {
                    existing.as_object_mut().map(|o| o.insert("status".into(), st));
                }
                let saved = self.store.put(info, existing, &nsv, name, false);
                return write_json(w, 200, &saved);
            }
            return write_json(w, 404, &status_obj(404, "not found", "NotFound"));
        }
        if obj.pointer("/metadata/name").is_none() {
            obj.pointer_mut("/metadata").and_then(|m| m.as_object_mut()).map(|m| m.insert("name".into(), json!(name)));
        }
        let saved = self.store.put(info, obj, &nsv, name, false);
        write_json(w, 200, &saved)
    }

    fn do_patch(&self, info: &ResourceInfo, ns: &Option<String>, name: &str, sub: Option<&str>, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let nsv = ns.clone().unwrap_or_default();
        let Some(mut existing) = self.store.get(info, &nsv, name) else {
            return write_json(w, 404, &status_obj(404, &format!("{} \"{name}\" not found", info.singular), "NotFound"));
        };
        let Ok(patch) = serde_json::from_slice::<Value>(&req.body) else {
            return write_json(w, 400, &status_obj(400, "invalid patch", "BadRequest"));
        };
        // strategic-merge / merge patch: recursive merge (good enough for the
        // status/metadata updates operators do). JSON-patch arrays unsupported.
        if patch.is_object() {
            merge(&mut existing, &patch);
        }
        let _ = sub;
        let saved = self.store.put(info, existing, &nsv, name, false);
        write_json(w, 200, &saved)
    }

    fn do_delete(&self, info: &ResourceInfo, ns: &Option<String>, name: &str, w: &mut dyn Write) -> std::io::Result<()> {
        let nsv = ns.clone().unwrap_or_default();
        match self.store.delete(info, &nsv, name) {
            Some(obj) => write_json(w, 200, &obj),
            None => write_json(w, 404, &status_obj(404, &format!("{} \"{name}\" not found", info.singular), "NotFound")),
        }
    }

    /// The `/scale` subresource: GET returns an autoscaling/v1 Scale built
    /// from `.spec.replicas`; PUT/PATCH sets `.spec.replicas` on the parent
    /// and returns the updated Scale (what kubectl `scale` expects).
    fn handle_scale(&self, verb: Verb, info: &ResourceInfo, ns: &Option<String>, name: &str, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let nsv = ns.clone().unwrap_or_default();
        let Some(mut obj) = self.store.get(info, &nsv, name) else {
            return write_json(w, 404, &status_obj(404, &format!("{} \"{name}\" not found", info.singular), "NotFound"));
        };
        if matches!(verb, Verb::Update | Verb::Patch) {
            if let Ok(body) = serde_json::from_slice::<Value>(&req.body) {
                if let Some(r) = body.pointer("/spec/replicas").and_then(|v| v.as_i64()) {
                    obj.pointer_mut("/spec")
                        .and_then(|s| s.as_object_mut())
                        .map(|s| s.insert("replicas".into(), json!(r)));
                    obj = self.store.put(info, obj, &nsv, name, false);
                }
            }
        }
        let replicas = obj.pointer("/spec/replicas").and_then(|v| v.as_i64()).unwrap_or(1);
        let rv = obj.pointer("/metadata/resourceVersion").cloned().unwrap_or(json!("0"));
        let scale = json!({
            "apiVersion": "autoscaling/v1",
            "kind": "Scale",
            "metadata": {"name": name, "namespace": nsv, "resourceVersion": rv},
            "spec": {"replicas": replicas},
            "status": {"replicas": replicas},
        });
        write_json(w, 200, &scale)
    }

    /// WebSocket exec (v4/v5.channel.k8s.io). Handles the non-interactive
    /// `kubectl exec POD -- cmd` case: run the command, stream stdout/stderr as
    /// channel frames, then a Status on the error channel. (Interactive -it
    /// needs concurrent stdin over the same TLS stream — see kubectl-slim's
    /// engine-socket path for that.)
    fn handle_exec_ws<S: Conn>(&self, req: &Req, stream: &mut S) -> std::io::Result<()> {
        let Some(proxy) = self.proxy.clone() else {
            return write_to(stream, 501, "exec not available (no engine bound)");
        };
        // /api/v1/namespaces/<ns>/pods/<pod>/exec
        let segs: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
        let ns = seg_after(&segs, "namespaces").unwrap_or("default");
        let pod = seg_after(&segs, "pods").unwrap_or("");
        let cmd: Vec<String> = req.query.iter().filter(|(k, _)| k == "command").map(|(_, v)| v.clone()).collect();
        let tty = req.query.iter().any(|(k, v)| k == "tty" && (v == "true" || v == "1"));
        let want_stdin = req.query.iter().any(|(k, v)| k == "stdin" && (v == "true" || v == "1"));
        let container = req.query.iter().find(|(k, _)| k == "container").map(|(_, v)| v.as_str()).unwrap_or("");
        if cmd.is_empty() || pod.is_empty() {
            return write_to(stream, 400, "exec requires a pod and command");
        }

        let key = req.header("sec-websocket-key").unwrap_or("");
        let proto = pick_subprotocol(req.header("sec-websocket-protocol").unwrap_or(""));
        let accept = ws_accept(key);
        let handshake = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: {proto}\r\n\r\n"
        );
        stream.write_all(handshake.as_bytes())?;
        stream.flush()?;

        let mut h = match proxy.exec_start(ns, pod, container, &cmd, tty, want_stdin) {
            Ok(h) => h,
            Err(e) => {
                let _ = ws_send(stream, 3, exec_status(1, &format!("exec failed: {e}")).as_bytes());
                return ws_close(stream);
            }
        };

        // Interactive stdin/resize: when the client asked for stdin and the
        // transport can be split (unix/tcp, not TLS), spawn a reader that maps
        // the client's channel-0 frames to the exec stdin and channel-4 frames
        // to a pty resize. Closing the channel drops stdin → EOF to the process.
        let resize_fd: Option<RawFd> = h.pty.as_ref().map(|f| f.as_raw_fd());
        let stdin_file: Option<std::fs::File> = if want_stdin {
            if let Some(pty) = &h.pty {
                pty.try_clone().ok()
            } else {
                h.stdin.take()
            }
        } else {
            None
        };
        if want_stdin {
            match stream.dup_reader() {
                Some(mut rd) => {
                    let proxy2 = proxy.clone();
                    std::thread::spawn(move || {
                        read_client_frames(&mut rd, stdin_file, resize_fd, proxy2.as_ref());
                    });
                }
                None => drop(stdin_file),
            }
        }

        // Output readers → mpsc → single frame writer (this thread).
        let (tx, rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
        let mut joins = Vec::new();
        let mut spawn_reader = |mut f: std::fs::File, ch: u8, tx: std::sync::mpsc::Sender<(u8, Vec<u8>)>| {
            joins.push(std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send((ch, buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            }));
        };
        if let Some(pty) = h.pty {
            spawn_reader(pty, 1, tx.clone());
        } else {
            if let Some(out) = h.stdout {
                spawn_reader(out, 1, tx.clone());
            }
            if let Some(err) = h.stderr {
                spawn_reader(err, 2, tx.clone());
            }
        }
        drop(tx); // so rx ends when all readers finish (process exit)

        while let Ok((ch, bytes)) = rx.recv() {
            if ws_send(stream, ch, &bytes).is_err() {
                break;
            }
        }
        for j in joins {
            let _ = j.join();
        }
        let code = proxy.exec_wait(h.pid);
        let status = if code == 0 {
            exec_status(0, "")
        } else {
            exec_status(code, &format!("command terminated with non-zero exit code {code}"))
        };
        let _ = ws_send(stream, 3, status.as_bytes());
        ws_close(stream)
    }

    fn handle_log(&self, ns: &Option<String>, pod: &str, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let Some(proxy) = &self.proxy else {
            return write_json(w, 501, &status_obj(501, "logs not available (no engine bound)", "NotImplemented"));
        };
        let nsv = ns.clone().unwrap_or_else(|| "default".into());
        let container = query_str(&req.query, "container").unwrap_or("");
        let opts = LogOpts {
            follow: query_bool(&req.query, "follow"),
            tail: query_str(&req.query, "tailLines").and_then(|s| s.parse().ok()),
            timestamps: query_bool(&req.query, "timestamps"),
        };
        // kubectl logs reads a plain (optionally chunked) text stream.
        w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
        w.flush()?;
        let mut cw = ChunkWriter { inner: w };
        match proxy.logs(&nsv, pod, container, &opts, &mut cw) {
            Ok(_) => {}
            Err(e) => {
                let _ = cw.write_all(format!("error: {e}\n").as_bytes());
            }
        }
        cw.finish()
    }

    fn do_watch(&self, info: &ResourceInfo, ns: Option<&str>, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let labels = label_selector(&req.query);
        let since = query_str(&req.query, "resourceVersion").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let (backlog, rx) = match self.store.watch(info, ns, labels, since) {
            Ok(x) => x,
            Err(Gone) => {
                return write_json(w, 410, &status_obj(410, "requested resourceVersion too old", "Expired"));
            }
        };
        // Stream: 200 + chunked, one JSON WatchEvent per chunk.
        w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
        w.flush()?;
        for ev in backlog {
            if write_watch_event(w, &ev).is_err() {
                return Ok(());
            }
        }
        // Live stream until the client disconnects.
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(ev) => {
                    if write_watch_event(w, &ev).is_err() {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // keep-alive probe: empty write detects a dead peer
                    if w.write_all(b"0\r\n\r\n").is_err() {
                        break;
                    }
                    // Can't continue chunked after terminator; instead just
                    // re-send nothing. (Most informers reconnect periodically.)
                    break;
                }
                Err(_) => break,
            }
        }
        Ok(())
    }

    // ---- CRD registration ----

    fn register_crd(&self, crd: &Value) {
        let group = crd.pointer("/spec/group").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let names = crd.pointer("/spec/names");
        let plural = names.and_then(|n| n.get("plural")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let singular = names.and_then(|n| n.get("singular")).and_then(|v| v.as_str()).unwrap_or(&plural).to_string();
        let kind = names.and_then(|n| n.get("kind")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let short_names: Vec<String> = names
            .and_then(|n| n.get("shortNames"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let scope = crd.pointer("/spec/scope").and_then(|v| v.as_str()).unwrap_or("Namespaced");
        let versions: Vec<String> = crd
            .pointer("/spec/versions")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        for version in versions {
            self.store.register(ResourceInfo {
                group: group.clone(),
                version,
                resource: plural.clone(),
                singular: singular.clone(),
                kind: kind.clone(),
                namespaced: scope == "Namespaced",
                short_names: short_names.clone(),
            });
        }
    }

    // ---- discovery builders ----

    fn api_resource_list(&self, group: &str, version: &str) -> Value {
        let gv = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
        let resources: Vec<Value> = self
            .store
            .registry()
            .into_iter()
            .filter(|r| r.group == group && r.version == version)
            .map(|r| json!({
                "name": r.resource,
                "singularName": r.singular,
                "namespaced": r.namespaced,
                "kind": r.kind,
                "verbs": ["get","list","watch","create","update","patch","delete","deletecollection"],
                "shortNames": r.short_names,
            }))
            .collect();
        json!({"kind":"APIResourceList","apiVersion":"v1","groupVersion":gv,"resources":resources})
    }

    fn api_group_list(&self) -> Value {
        let mut groups: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
        for r in self.store.registry() {
            if !r.group.is_empty() {
                groups.entry(r.group).or_default().insert(r.version);
            }
        }
        let group_objs: Vec<Value> = groups.iter().map(|(g, vers)| group_obj(g, vers)).collect();
        json!({"kind":"APIGroupList","apiVersion":"v1","groups":group_objs})
    }

    // ---- openapi v3 (permissive) ----

    fn gv_paths(&self) -> std::collections::BTreeSet<(String, String)> {
        self.store.registry().into_iter().map(|r| (r.group, r.version)).collect()
    }

    fn openapi_root(&self) -> Value {
        let mut paths = serde_json::Map::new();
        for (g, v) in self.gv_paths() {
            let p = if g.is_empty() { format!("api/{v}") } else { format!("apis/{g}/{v}") };
            paths.insert(p.clone(), json!({"serverRelativeURL": format!("/openapi/v3/{p}")}));
        }
        json!({ "paths": paths })
    }

    fn openapi_gv(&self, group: &str, version: &str) -> Value {
        let mut schemas = serde_json::Map::new();
        for r in self.store.registry().into_iter().filter(|r| r.group == group && r.version == version) {
            let key = if group.is_empty() {
                format!("io.k8s.api.core.{version}.{}", r.kind)
            } else {
                format!("{group}.{version}.{}", r.kind)
            };
            schemas.insert(
                key,
                json!({
                    "type": "object",
                    "x-kubernetes-preserve-unknown-fields": true,
                    "x-kubernetes-group-version-kind": [{"group": group, "version": version, "kind": r.kind}],
                }),
            );
        }
        json!({
            "openapi": "3.0.0",
            "info": {"title": "slim", "version": "v0"},
            "components": {"schemas": schemas},
            "paths": {},
        })
    }

    fn api_group(&self, group: &str) -> Value {
        let vers: std::collections::BTreeSet<String> = self
            .store
            .registry()
            .into_iter()
            .filter(|r| r.group == group)
            .map(|r| r.version)
            .collect();
        group_obj(group, &vers)
    }
}

enum Verb {
    Get,
    Create,
    Update,
    Patch,
    Delete,
}

fn group_obj(group: &str, versions: &std::collections::BTreeSet<String>) -> Value {
    let gvs: Vec<Value> = versions
        .iter()
        .map(|v| json!({"groupVersion": format!("{group}/{v}"), "version": v}))
        .collect();
    let preferred = versions.iter().next_back().cloned().unwrap_or_default();
    json!({
        "name": group,
        "versions": gvs,
        "preferredVersion": {"groupVersion": format!("{group}/{preferred}"), "version": preferred},
    })
}

fn version_info() -> Value {
    json!({
        "major":"1","minor":"29",
        "gitVersion":"v1.29.0-slim","gitCommit":"slim","gitTreeState":"clean",
        "buildDate":"2026-06-10T00:00:00Z","goVersion":"none","compiler":"rustc",
        "platform":"linux/amd64",
    })
}

// ---- server-side Table printing (kubectl human-readable `get`) ----

fn wants_table(req: &Req) -> bool {
    req.header("accept").map(|a| a.contains("as=Table")).unwrap_or(false)
}

/// Build a meta.k8s.io/v1 Table for `items` with per-kind columns, so real
/// kubectl renders NAME/READY/STATUS/RESTARTS/AGE instead of its generic
/// NAME/AGE fallback.
fn build_table(info: &ResourceInfo, items: &[Value], rv: u64) -> Value {
    let cols: &[(&str, &str)] = match info.resource.as_str() {
        "pods" => &[("Name", "string"), ("Ready", "string"), ("Status", "string"), ("Restarts", "integer"), ("Age", "string")],
        "deployments" | "replicasets" | "statefulsets" => {
            &[("Name", "string"), ("Ready", "string"), ("Up-to-date", "integer"), ("Available", "integer"), ("Age", "string")]
        }
        _ => &[("Name", "string"), ("Age", "string")],
    };
    let coldefs: Vec<Value> = cols
        .iter()
        .enumerate()
        .map(|(i, (n, t))| json!({"name": n, "type": t, "format": if i == 0 { "name" } else { "" }, "description": "", "priority": 0}))
        .collect();
    let rows: Vec<Value> = items
        .iter()
        .map(|o| {
            let name = o.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("");
            let age = age_of(o);
            let cells: Vec<Value> = match info.resource.as_str() {
                "pods" => {
                    let status = pod_status_cell(o);
                    let (ready, restarts) = pod_ready_restarts(o);
                    vec![json!(name), json!(ready), json!(status), json!(restarts), json!(age)]
                }
                "deployments" | "replicasets" | "statefulsets" => {
                    let desired = o.pointer("/spec/replicas").and_then(|v| v.as_i64()).unwrap_or(1);
                    let ready = o.pointer("/status/readyReplicas").and_then(|v| v.as_i64()).unwrap_or(0);
                    let updated = o.pointer("/status/updatedReplicas").and_then(|v| v.as_i64()).unwrap_or(ready);
                    let avail = o.pointer("/status/availableReplicas").and_then(|v| v.as_i64()).unwrap_or(ready);
                    vec![json!(name), json!(format!("{ready}/{desired}")), json!(updated), json!(avail), json!(age)]
                }
                _ => vec![json!(name), json!(age)],
            };
            json!({"cells": cells, "object": o})
        })
        .collect();
    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": rv.to_string()},
        "columnDefinitions": coldefs,
        "rows": rows,
    })
}

/// Pod STATUS cell: `Init:done/total` while init containers run, else the phase.
fn pod_status_cell(pod: &Value) -> String {
    let phase = pod.pointer("/status/phase").and_then(|v| v.as_str()).unwrap_or("Pending");
    if phase == "Pending" {
        if let Some(inits) = pod.pointer("/status/initContainerStatuses").and_then(|v| v.as_array()) {
            if !inits.is_empty() {
                let done = inits
                    .iter()
                    .filter(|c| c.pointer("/state/terminated/exitCode").and_then(|v| v.as_i64()) == Some(0))
                    .count();
                if done < inits.len() {
                    return format!("Init:{done}/{}", inits.len());
                }
            }
        }
    }
    phase.to_string()
}

/// (ready "n/m", total restarts) from a pod's containerStatuses.
fn pod_ready_restarts(pod: &Value) -> (String, i64) {
    match pod.pointer("/status/containerStatuses").and_then(|c| c.as_array()) {
        Some(arr) if !arr.is_empty() => {
            let total = arr.len();
            let ready = arr.iter().filter(|c| c.get("ready").and_then(|r| r.as_bool()).unwrap_or(false)).count();
            let restarts = arr.iter().filter_map(|c| c.get("restartCount").and_then(|r| r.as_i64())).sum();
            (format!("{ready}/{total}"), restarts)
        }
        _ => {
            let running = pod.pointer("/status/phase").and_then(|v| v.as_str()) == Some("Running");
            (if running { "1/1".into() } else { "0/1".into() }, 0)
        }
    }
}

/// kubectl-style age ("5s","3m","2h","4d") from metadata.creationTimestamp.
fn age_of(obj: &Value) -> String {
    let Some(ts) = obj.pointer("/metadata/creationTimestamp").and_then(|v| v.as_str()) else {
        return "<unknown>".into();
    };
    let Some(created) = parse_rfc3339(ts) else { return "<unknown>".into() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let secs = (now - created).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Parse "YYYY-MM-DDTHH:MM:SSZ" into epoch seconds (days-from-civil).
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

fn status_obj(code: u16, msg: &str, reason: &str) -> Value {
    json!({
        "kind":"Status","apiVersion":"v1","metadata":{},
        "status":"Failure","message":msg,"reason":reason,"code":code,
    })
}

// ---- merge patch ----

fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                } else {
                    merge(b.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

// ---- WebSocket (exec subresource) ----

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn seg_after<'a>(segs: &'a [&'a str], key: &str) -> Option<&'a str> {
    segs.iter().position(|s| *s == key).and_then(|i| segs.get(i + 1)).copied()
}

/// Choose a channel subprotocol from the client's offer (prefer v4, which we
/// implement plainly; v5 framing is compatible for our output-only use).
fn pick_subprotocol(offered: &str) -> String {
    let want = ["v4.channel.k8s.io", "v5.channel.k8s.io", "channel.k8s.io"];
    for w in want {
        if offered.split(',').any(|p| p.trim() == w) {
            return w.to_string();
        }
    }
    "v4.channel.k8s.io".to_string()
}

fn ws_accept(key: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(WS_GUID.as_bytes());
    b64_std(&h.finalize())
}

/// Send a k8s channel message as one unmasked binary WebSocket frame:
/// payload = [channel byte] ++ data.
fn ws_send<S: Write>(stream: &mut S, channel: u8, data: &[u8]) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(1 + data.len());
    payload.push(channel);
    payload.extend_from_slice(data);
    let mut frame = vec![0x82u8]; // FIN + binary
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len < 65536 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&payload);
    stream.write_all(&frame)?;
    stream.flush()
}

/// Read one (possibly masked) WebSocket frame: returns (opcode, payload).
/// Returns None on EOF. Control-frame fragmentation is not handled (clients
/// don't fragment the small stdin/control frames we expect).
fn read_ws_frame<R: Read + ?Sized>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 2];
    if !read_full(r, &mut hdr)? {
        return Ok(None);
    }
    let opcode = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b) as usize;
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    Ok(Some((opcode, payload)))
}

fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match r.read(&mut buf[read..]) {
            Ok(0) => return Ok(false),
            Ok(n) => read += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Pump client → exec: channel 0 → stdin, channel 4 → pty resize. Ends on a
/// close frame or EOF, dropping stdin so the process sees EOF.
fn read_client_frames(
    rd: &mut dyn Read,
    mut stdin: Option<std::fs::File>,
    resize_fd: Option<RawFd>,
    proxy: &dyn PodProxy,
) {
    loop {
        match read_ws_frame(rd) {
            Ok(Some((op, payload))) => {
                if op == 0x8 {
                    break; // close
                }
                if payload.is_empty() {
                    continue;
                }
                let (channel, data) = (payload[0], &payload[1..]);
                match channel {
                    0 => {
                        if let Some(f) = stdin.as_mut() {
                            if f.write_all(data).is_err() {
                                break;
                            }
                            let _ = f.flush();
                        }
                    }
                    4 => {
                        if let Some(fd) = resize_fd {
                            if let Ok(v) = serde_json::from_slice::<Value>(data) {
                                let w = v.get("Width").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                                let h = v.get("Height").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                                if w > 0 && h > 0 {
                                    proxy.exec_resize(fd, w, h);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => break,
        }
    }
    drop(stdin);
}

fn ws_close<S: Write>(stream: &mut S) -> std::io::Result<()> {
    // Close frame WITH a 1000 (normal closure) status — a payload-less close is
    // status 1005, which kubectl surfaces as an error and masks the exit code.
    stream.write_all(&[0x88, 0x02, 0x03, 0xE8])?;
    stream.flush()
}

fn exec_status(code: i32, msg: &str) -> String {
    if code == 0 {
        json!({"kind":"Status","apiVersion":"v1","status":"Success","metadata":{}}).to_string()
    } else {
        json!({
            "kind":"Status","apiVersion":"v1","status":"Failure","metadata":{},
            "message": msg,"reason":"NonZeroExitCode",
            "details":{"causes":[{"reason":"ExitCode","message": code.to_string()}]},
        })
        .to_string()
    }
}

fn write_to<S: Write>(stream: &mut S, code: u16, msg: &str) -> std::io::Result<()> {
    let body = json!({"kind":"Status","status":"Failure","message":msg,"code":code}).to_string();
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(code),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn b64_std(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---- HTTP plumbing ----

fn read_request<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Req>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        head.extend_from_slice(&line);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return Ok(None);
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut r = httparse::Request::new(&mut headers);
    if r.parse(&head).is_err() {
        return Ok(None);
    }
    let method = r.method.unwrap_or("GET").to_string();
    let full = r.path.unwrap_or("/").to_string();
    let (path, query) = parse_query(&full);
    let hdrs: Vec<(String, String)> = r
        .headers
        .iter()
        .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned()))
        .collect();
    let clen = hdrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; clen];
    if clen > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Req { method, path, query, headers: hdrs, body }))
}

fn parse_query(full: &str) -> (String, Vec<(String, String)>) {
    let (path, qs) = full.split_once('?').unwrap_or((full, ""));
    let q = qs
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect();
    (path.to_string(), q)
}

fn query_str<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}
fn query_bool(q: &[(String, String)], key: &str) -> bool {
    matches!(query_str(q, key), Some("true") | Some("1"))
}
fn label_selector(q: &[(String, String)]) -> Vec<(String, String)> {
    query_str(q, "labelSelector")
        .map(|s| {
            s.split(',')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn write_json(w: &mut dyn Write, code: u16, body: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(code),
        bytes.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

fn write_raw(w: &mut dyn Write, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(code),
        body.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// A minimal gnostic `openapi.v2.Document` protobuf: swagger="2.0",
/// info{title,version}, empty paths, no definitions. Just enough for kubectl's
/// validator to parse it and find no schema (→ validation skipped).
fn openapi_v2_proto() -> Vec<u8> {
    fn varint(mut n: usize) -> Vec<u8> {
        let mut o = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n > 0 {
                o.push(b | 0x80);
            } else {
                o.push(b);
                break;
            }
        }
        o
    }
    fn ld(field: u32, body: &[u8]) -> Vec<u8> {
        let mut o = vec![((field << 3) | 2) as u8]; // wire type 2 = length-delimited
        o.extend(varint(body.len()));
        o.extend_from_slice(body);
        o
    }
    let mut info = Vec::new();
    info.extend(ld(1, b"slim")); // Info.title
    info.extend(ld(2, b"v0")); //   Info.version
    let mut doc = Vec::new();
    doc.extend(ld(1, b"2.0")); // Document.swagger
    doc.extend(ld(2, &info)); //  Document.info
    doc.extend(ld(8, &[])); //    Document.paths (empty)
    doc
}

fn write_text(w: &mut dyn Write, code: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(code),
        body.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(body.as_bytes())?;
    w.flush()
}

fn write_watch_event(w: &mut dyn Write, ev: &WatchEvent) -> std::io::Result<()> {
    let frame = json!({"type": ev.typ, "object": ev.object});
    let mut line = serde_json::to_vec(&frame).unwrap_or_default();
    line.push(b'\n');
    // chunked: size in hex, CRLF, data, CRLF
    write!(w, "{:x}\r\n", line.len())?;
    w.write_all(&line)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

/// Chunked-transfer writer over a borrowed response stream (for log streaming
/// after the headers are already sent).
struct ChunkWriter<'a> {
    inner: &'a mut dyn Write,
}
impl Write for ChunkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.inner, "{:x}\r\n", buf.len())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        self.inner.flush()?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
impl ChunkWriter<'_> {
    fn finish(&mut self) -> std::io::Result<()> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()
    }
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        410 => "Gone",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn rand_suffix() -> String {
    let mut b = [0u8; 3];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    b.iter().map(|x| format!("{x:02x}")).collect()
}
