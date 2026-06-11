//! Minimal HTTP/1.1 server for the Kubernetes API surface (the apiserver-lite
//! transport). Plain TCP for now; TLS + ServiceAccount projection layer on
//! top later so in-pod operators can connect. Watch responses stream
//! newline-delimited WatchEvents (chunked) — the wire format client-go
//! informers expect.

use crate::store::{Gone, ResourceInfo, SharedStore, WatchEvent};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

pub struct ApiServer {
    pub store: SharedStore,
}

struct Req {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

impl ApiServer {
    pub fn new(store: SharedStore) -> Arc<Self> {
        Arc::new(ApiServer { store })
    }

    pub fn serve(self: &Arc<Self>, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let me = self.clone();
            std::thread::spawn(move || {
                let _ = me.handle_conn(conn);
            });
        }
        Ok(())
    }

    /// Generic over the stream so the same handlers serve plain TCP and TLS
    /// (rustls StreamOwned). The full request is read up front, then handlers
    /// write to the stream as `&mut dyn Write`.
    pub fn handle_conn<S: Read + Write>(&self, mut stream: S) -> std::io::Result<()> {
        let req = {
            let mut reader = BufReader::new(&mut stream);
            match read_request(&mut reader)? {
                Some(r) => r,
                None => return Ok(()),
            }
        };
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

        let watch = query_bool(&req.query, "watch");

        match verb {
            Verb::Get if name.is_none() && watch => self.do_watch(&info, ns.as_deref(), req, w),
            Verb::Get if name.is_none() => self.do_list(&info, ns.as_deref(), req, w),
            Verb::Get => self.do_get(&info, &ns, name.unwrap(), w),
            Verb::Create => self.do_create(&info, &ns, req, w),
            Verb::Update => self.do_update(&info, &ns, name.unwrap_or(""), subresource, req, w),
            Verb::Patch => self.do_patch(&info, &ns, name.unwrap_or(""), subresource, req, w),
            Verb::Delete => self.do_delete(&info, &ns, name.unwrap_or(""), w),
        }
    }

    fn do_list(&self, info: &ResourceInfo, ns: Option<&str>, req: &Req, w: &mut dyn Write) -> std::io::Result<()> {
        let labels = label_selector(&req.query);
        let (items, rv) = self.store.list(info, ns, &labels);
        let list = json!({
            "apiVersion": info.group_version(),
            "kind": info.list_kind(),
            "metadata": {"resourceVersion": rv.to_string()},
            "items": items,
        });
        write_json(w, 200, &list)
    }

    fn do_get(&self, info: &ResourceInfo, ns: &Option<String>, name: &str, w: &mut dyn Write) -> std::io::Result<()> {
        let nsv = ns.clone().unwrap_or_default();
        match self.store.get(info, &nsv, name) {
            Some(obj) => write_json(w, 200, &obj),
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
    let clen = r
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; clen];
    if clen > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Req { method, path, query, body }))
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
