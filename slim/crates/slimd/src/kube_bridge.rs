//! Controller bridge: reconcile the apiserver-lite's stored workloads into
//! real slim containers, and write Pod status back. This is what turns a
//! `kubectl apply -f deployment.yaml` (stored by slim-kubeapi) into actually
//! running containers on the engine — Tier B of the slim k8s story.
//!
//! Level-based: every tick it lists Deployments/Pods/Jobs, ensures the desired
//! containers exist (named `<ns>_<podname>`, labeled so we own them), removes
//! orphans, and syncs a Pod object per container so `kubectl get pods` and
//! operators watching Pods see live state. Simpler and more self-healing than
//! edge-triggered reconciliation.

use crate::engine::EngineRef;
use serde_json::{json, Value};
use slim_kubeapi::{ApiServer, ExecHandle, LogOpts, PodProxy, SharedStore, Store};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

const OWNER: &str = "io.nebula.kube.owner"; // "<kind>/<ns>/<name>"
const POD_OF: &str = "io.nebula.kube.pod"; // "<ns>/<podname>"
const MANAGED: &str = "io.nebula.kube.bridge"; // "true"

/// In-cluster context the bridge projects into pods so client-go operators
/// reach the apiserver (KUBERNETES_SERVICE_HOST/PORT + the ServiceAccount dir).
struct BridgeCtx {
    ca_pem: String,
    sa_root: PathBuf,
    kube_host: String, // the bridge gateway IP slimd's TLS listener is on
    kube_port: u16,
}

/// Start the apiserver-lite (TLS) + the reconcile loop. Spawns background
/// threads; never blocks. Returns the shared store.
pub fn start(engine: &EngineRef, api_addr: &str) -> SharedStore {
    let store = Store::new();
    seed_cluster(&store);

    // The gateway IP of the default bridge is where containers reach slimd.
    let gw = engine
        .net
        .get(slim_net::DEFAULT_NETWORK)
        .map(|n| n.gateway())
        .unwrap_or_else(|| "10.88.0.1".to_string());
    let port: u16 = api_addr.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(6443);

    // Serve pods/{}/log and pods/{}/exec from the in-process engine.
    let api = ApiServer::with_proxy(store.clone(), Arc::new(EngineProxy { engine: engine.clone() }));
    let addr = api_addr.to_string();
    // TLS so in-cluster operators (always HTTPS) can connect; the CA is
    // projected into pods. Falls back to plain HTTP if cert generation fails.
    let gw_ip = gw.parse::<std::net::IpAddr>().ok();
    let ca_pem = match slim_kubeapi::generate_tls(&[], gw_ip.as_slice()) {
        Ok(id) => {
            let ca = id.ca_pem.clone();
            let api2 = api.clone();
            let addr2 = addr.clone();
            std::thread::spawn(move || {
                if let Err(e) = id.serve(api2, &addr2) {
                    eprintln!("slimd: kube apiserver (TLS) on {addr2} stopped: {e}");
                }
            });
            println!("slimd: kube apiserver-lite listening on https://{api_addr}");
            ca
        }
        Err(e) => {
            eprintln!("slimd: TLS cert gen failed ({e}); serving plain HTTP");
            let api2 = api.clone();
            let addr2 = addr.clone();
            std::thread::spawn(move || {
                let _ = api2.serve(&addr2);
            });
            String::new()
        }
    };

    // Also serve the apiserver on a unix socket (plain HTTP) so host-side
    // kubectl-slim/helm-slim reach it through nebula's socket proxy (mirroring
    // docker.sock); kubectl-slim inside the vessel connects directly. This is
    // the "one source of truth" path — slim-kube is a thin client over it.
    let kube_sock = std::env::var("SLIM_KUBE_SOCKET").unwrap_or_else(|_| {
        std::env::var("SLIM_SOCKET")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .parent()
                    .map(|p| p.join("slim-kube.sock").to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "/var/run/slim-kube.sock".to_string())
    });
    {
        let api2 = api.clone();
        let sock = kube_sock.clone();
        std::thread::spawn(move || {
            if let Err(e) = api2.serve_unix(&sock) {
                eprintln!("slimd: kube apiserver (unix {sock}) stopped: {e}");
            }
        });
        println!("slimd: kube apiserver-lite unix socket at {kube_sock}");
    }

    let ctx = BridgeCtx {
        ca_pem,
        sa_root: engine.paths.data.join("kube-sa"),
        kube_host: gw,
        kube_port: port,
    };
    register_kubernetes_service(&store, engine, &ctx);

    let engine = engine.clone();
    let store2 = store.clone();
    std::thread::spawn(move || reconcile_loop(&engine, &store2, &ctx));
    store
}

/// Create the `kubernetes` Service in default + point its DNS at the apiserver.
fn register_kubernetes_service(store: &SharedStore, engine: &EngineRef, ctx: &BridgeCtx) {
    if let Some(info) = store.lookup("", "services") {
        store.put(
            &info,
            json!({"metadata":{"name":"kubernetes","namespace":"default","labels":{"component":"apiserver"}},
                   "spec":{"clusterIP": ctx.kube_host, "ports":[{"name":"https","port":ctx.kube_port,"targetPort":ctx.kube_port}]}}),
            "default",
            "kubernetes",
            true,
        );
    }
    for name in ["kubernetes.default.svc.cluster.local", "kubernetes.default.svc", "kubernetes.default", "kubernetes"] {
        engine.dns.set(name, &ctx.kube_host);
    }
}

/// Serves pod log/exec subresources from the engine. A pod named `<pod>` in
/// namespace `<ns>` maps to the engine container `<ns>_<pod>`.
struct EngineProxy {
    engine: EngineRef,
}

impl PodProxy for EngineProxy {
    fn logs(&self, ns: &str, pod: &str, opts: &LogOpts, w: &mut dyn Write) -> std::io::Result<()> {
        let cname = format!("{ns}_{pod}");
        let entry = self.engine.get_entry(&cname)?;
        let log_path = entry.c.lock().unwrap().log_path.clone();
        let ropts = slim_runtime::jsonlog::LogReadOpts {
            stdout: true,
            stderr: true,
            tail: opts.tail,
            since: None,
            until: None,
            timestamps: opts.timestamps,
        };
        let path = std::path::Path::new(&log_path);
        let mut pos = slim_runtime::jsonlog::read_log(path, &ropts, 0, |_s, bytes| {
            let _ = w.write_all(bytes);
        })?;
        if !opts.follow {
            return Ok(());
        }
        let fopts = slim_runtime::jsonlog::LogReadOpts { tail: None, ..ropts };
        loop {
            let running = entry.c.lock().unwrap().running();
            let mut wrote = false;
            pos = slim_runtime::jsonlog::read_log(path, &fopts, pos, |_s, bytes| {
                wrote = true;
                let _ = w.write_all(bytes);
            })?;
            if !wrote {
                if !running {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        Ok(())
    }

    fn exec_start(&self, ns: &str, pod: &str, cmd: &[String], tty: bool, stdin: bool) -> std::io::Result<ExecHandle> {
        let cname = format!("{ns}_{pod}");
        let entry = self.engine.get_entry(&cname)?;
        let pid = {
            let c = entry.c.lock().unwrap();
            if !c.running() {
                return Err(std::io::Error::other("container not running"));
            }
            c.state.pid
        };
        let spec = slim_runtime::ExecSpec {
            argv: cmd.to_vec(),
            env: vec![],
            cwd: String::new(),
            user: String::new(),
            tty,
            open_stdin: stdin,
        };
        let h = slim_runtime::exec_in_container_cg(pid, &spec, Some(&cname))?;
        Ok(ExecHandle {
            pid: h.pid,
            tty: h.pty_master.is_some(),
            pty: h.pty_master,
            stdin: h.stdin,
            stdout: h.stdout,
            stderr: h.stderr,
        })
    }

    fn exec_wait(&self, pid: i32) -> i32 {
        slim_runtime::wait_pid(pid).map(|s| s.code).unwrap_or(-1)
    }

    fn exec_resize(&self, pty_fd: i32, cols: u16, rows: u16) {
        slim_runtime::resize_pty(pty_fd, cols, rows);
    }
}

/// Minimal cluster scaffolding so kubectl/operators see a sane world.
fn seed_cluster(store: &SharedStore) {
    let ns = |name: &str| {
        let info = store.lookup("", "namespaces").unwrap();
        store.put(&info, json!({"metadata":{"name":name}, "spec":{}, "status":{"phase":"Active"}}), "", name, true);
    };
    for n in ["default", "kube-system", "kube-public"] {
        ns(n);
    }
    // a Node object so scheduling-aware tools don't choke
    if let Some(info) = store.lookup("", "nodes") {
        store.put(
            &info,
            json!({"metadata":{"name":"slim","labels":{"kubernetes.io/hostname":"slim"}},
                   "status":{"phase":"Running","conditions":[{"type":"Ready","status":"True"}],
                             "nodeInfo":{"kubeletVersion":"v1.29.0-slim"}}}),
            "",
            "slim",
            true,
        );
    }
}

fn reconcile_loop(engine: &EngineRef, store: &SharedStore, ctx: &BridgeCtx) {
    loop {
        if let Err(e) = reconcile_once(engine, store, ctx) {
            eprintln!("slimd: kube reconcile error: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
}

struct Desired {
    cname: String,     // engine container name
    pod_ns: String,
    pod_name: String,
    owner: String,     // "<kind>/<ns>/<name>" or "Pod/<ns>/<name>"
    template: Value,   // pod spec
    labels: Value,     // pod labels
    restart: &'static str,
}

fn reconcile_once(engine: &EngineRef, store: &SharedStore, ctx: &BridgeCtx) -> std::io::Result<()> {
    let mut desired: BTreeMap<String, Desired> = BTreeMap::new();

    // Deployments / ReplicaSets / StatefulSets → N replicas.
    for kind in ["deployments", "replicasets", "statefulsets"] {
        if let Some(info) = store.lookup("apps", kind) {
            for d in store.list(&info, None, &[]).0 {
                collect_workload(&d, &mut desired, "always");
            }
        }
    }
    // Jobs → run-to-completion (single pod).
    if let Some(info) = store.lookup("batch", "jobs") {
        for j in store.list(&info, None, &[]).0 {
            collect_workload(&j, &mut desired, "no");
        }
    }
    // Bare Pods (not owned by us — avoid double-managing the pods we synthesize).
    if let Some(info) = store.lookup("", "pods") {
        for p in store.list(&info, None, &[]).0 {
            let managed = p.pointer("/metadata/labels").and_then(|l| l.get(MANAGED)).is_some();
            if managed {
                continue;
            }
            collect_bare_pod(&p, &mut desired);
        }
    }

    // Ensure desired containers exist + sync Pod objects.
    for d in desired.values() {
        ensure_container(engine, store, ctx, d);
    }

    // Remove orphan containers we own that are no longer desired.
    let desired_cnames: std::collections::BTreeSet<&String> = desired.keys().collect();
    for c in engine.list(true) {
        if c.config.labels.get(MANAGED).map(|v| v == "true").unwrap_or(false)
            && !desired_cnames.contains(&c.name)
        {
            let _ = engine.remove(&c.name, true, false);
            // drop its Pod object
            if let Some(pod) = c.config.labels.get(POD_OF) {
                if let Some((ns, name)) = pod.split_once('/') {
                    if let Some(info) = store.lookup("", "pods") {
                        store.delete(&info, ns, name);
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect_workload(obj: &Value, out: &mut BTreeMap<String, Desired>, restart: &'static str) {
    let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("Deployment").to_string();
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let ns = obj.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or("default").to_string();
    if name.is_empty() {
        return;
    }
    let replicas = if restart == "no" {
        1
    } else {
        obj.pointer("/spec/replicas").and_then(|v| v.as_i64()).unwrap_or(1).max(0)
    };
    let template = obj.pointer("/spec/template/spec").cloned().unwrap_or(Value::Null);
    let labels = obj.pointer("/spec/template/metadata/labels").cloned()
        .or_else(|| obj.pointer("/spec/selector/matchLabels").cloned())
        .unwrap_or(json!({}));
    for i in 0..replicas {
        let pod_name = format!("{name}-{i}");
        let cname = format!("{ns}_{pod_name}");
        out.insert(
            cname.clone(),
            Desired {
                cname,
                pod_ns: ns.clone(),
                pod_name,
                owner: format!("{kind}/{ns}/{name}"),
                template: template.clone(),
                labels: labels.clone(),
                restart,
            },
        );
    }
}

fn collect_bare_pod(obj: &Value, out: &mut BTreeMap<String, Desired>) {
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let ns = obj.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or("default").to_string();
    if name.is_empty() {
        return;
    }
    let cname = format!("{ns}_{name}");
    out.insert(
        cname.clone(),
        Desired {
            cname,
            pod_ns: ns.clone(),
            pod_name: name.clone(),
            owner: format!("Pod/{ns}/{name}"),
            template: obj.pointer("/spec").cloned().unwrap_or(Value::Null),
            labels: obj.pointer("/metadata/labels").cloned().unwrap_or(json!({})),
            restart: obj.pointer("/spec/restartPolicy").and_then(|v| v.as_str())
                .map(|p| if p == "Never" || p == "OnFailure" { "no" } else { "always" })
                .unwrap_or("always"),
        },
    );
}

fn ensure_container(engine: &EngineRef, store: &SharedStore, ctx: &BridgeCtx, d: &Desired) {
    let exists = engine.get_entry(&d.cname).is_ok();
    if !exists {
        let Some(req) = build_create_req(store, ctx, d) else { return };
        // Pull image if needed, then create+start.
        let image = req.config.image.clone();
        if engine.store.resolve(&image).is_none() {
            let _ = engine.ensure_image(&image);
        }
        match engine.create(&req, Some(&d.cname)) {
            Ok(_) => {
                if let Err(e) = engine.start(&d.cname) {
                    eprintln!("slimd: bridge start {} failed: {e}", d.cname);
                }
            }
            Err(e) => {
                eprintln!("slimd: bridge create {} failed: {e}", d.cname);
                sync_pod(store, d, "Pending", "", &format!("create failed: {e}"));
                return;
            }
        }
    }
    // Sync Pod status from the live container.
    let (phase, ip, msg) = match engine.get_entry(&d.cname) {
        Ok(entry) => {
            let c = entry.c.lock().unwrap();
            let phase = match c.state.status.as_str() {
                "running" => "Running",
                "created" => "Pending",
                "exited" if c.state.exit_code == 0 => "Succeeded",
                "exited" => "Failed",
                _ => "Unknown",
            };
            (phase, c.ip.clone(), String::new())
        }
        Err(_) => ("Pending", String::new(), String::new()),
    };
    sync_pod(store, d, phase, &ip, &msg);
}

/// Translate a pod template into a docker-style create request, resolving env
/// from ConfigMaps/Secrets in the store.
fn build_create_req(store: &SharedStore, ctx: &BridgeCtx, d: &Desired) -> Option<slim_api::container::ContainerCreateRequest> {
    let containers = d.template.get("containers").and_then(|c| c.as_array())?;
    let c0 = containers.first()?;
    let image = c0.get("image").and_then(|v| v.as_str())?.to_string();

    let mut env = Vec::new();
    // In-cluster config for operators (client-go reads these env vars).
    env.push(format!("KUBERNETES_SERVICE_HOST={}", ctx.kube_host));
    env.push(format!("KUBERNETES_SERVICE_PORT={}", ctx.kube_port));
    env.push(format!("KUBERNETES_SERVICE_PORT_HTTPS={}", ctx.kube_port));
    env.push(format!("KUBERNETES_PORT=tcp://{}:{}", ctx.kube_host, ctx.kube_port));
    // envFrom
    for ef in c0.get("envFrom").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        if let Some(n) = ef.pointer("/configMapRef/name").and_then(|v| v.as_str()) {
            for (k, v) in config_data(store, "configmaps", &d.pod_ns, n) {
                env.push(format!("{k}={v}"));
            }
        }
        if let Some(n) = ef.pointer("/secretRef/name").and_then(|v| v.as_str()) {
            for (k, v) in config_data(store, "secrets", &d.pod_ns, n) {
                env.push(format!("{k}={}", decode_secret(&v)));
            }
        }
    }
    // env
    for e in c0.get("env").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        if let Some(v) = e.get("value").and_then(|v| v.as_str()) {
            env.push(format!("{name}={v}"));
        } else if let Some(r) = e.get("valueFrom") {
            if let (Some(cm), Some(key)) = (r.pointer("/configMapKeyRef/name").and_then(|v| v.as_str()), r.pointer("/configMapKeyRef/key").and_then(|v| v.as_str())) {
                if let Some(v) = config_data(store, "configmaps", &d.pod_ns, cm).get(key) {
                    env.push(format!("{name}={v}"));
                }
            } else if let (Some(s), Some(key)) = (r.pointer("/secretKeyRef/name").and_then(|v| v.as_str()), r.pointer("/secretKeyRef/key").and_then(|v| v.as_str())) {
                if let Some(v) = config_data(store, "secrets", &d.pod_ns, s).get(key) {
                    env.push(format!("{name}={}", decode_secret(v)));
                }
            }
        }
    }

    let cmd = strs(c0.get("args"));
    let entrypoint = strs(c0.get("command"));

    let mut labels = std::collections::BTreeMap::new();
    labels.insert(MANAGED.to_string(), "true".to_string());
    labels.insert(OWNER.to_string(), d.owner.clone());
    labels.insert(POD_OF.to_string(), format!("{}/{}", d.pod_ns, d.pod_name));
    if let Some(obj) = d.labels.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                labels.insert(k.clone(), s.to_string());
            }
        }
    }

    let mut config = slim_api::container::ContainerConfig {
        image,
        cmd,
        env,
        labels,
        ..Default::default()
    };
    if !entrypoint.is_empty() {
        config.entrypoint = Some(entrypoint);
    }
    // Project the ServiceAccount dir (ca.crt + token + namespace) so in-cluster
    // clients authenticate/verify TLS the standard way.
    let mut binds = Vec::new();
    if let Some(sa_dir) = ensure_sa(ctx, &d.pod_ns) {
        binds.push(format!("{}:/var/run/secrets/kubernetes.io/serviceaccount:ro", sa_dir.display()));
    }
    let host_config = slim_api::container::HostConfig {
        restart_policy: slim_api::container::RestartPolicy {
            name: d.restart.to_string(),
            maximum_retry_count: 0,
        },
        network_mode: "bridge".to_string(),
        binds,
        ..Default::default()
    };
    // Endpoint alias = the pod's app label + pod name, for in-cluster DNS.
    let mut endpoints = std::collections::BTreeMap::new();
    let mut aliases = vec![d.pod_name.clone()];
    if let Some(app) = d.labels.get("app").and_then(|v| v.as_str()) {
        aliases.push(app.to_string());
    }
    endpoints.insert(
        "bridge".to_string(),
        slim_api::container::EndpointSettings { aliases, ..Default::default() },
    );

    Some(slim_api::container::ContainerCreateRequest {
        config,
        host_config,
        networking_config: slim_api::container::NetworkingConfig { endpoints_config: endpoints },
    })
}

/// Write (idempotently) the ServiceAccount dir for a namespace: ca.crt, a
/// static bearer token (auth isn't enforced — the VM is the boundary), and the
/// namespace file. Returns the dir to bind-mount, or None if no CA (plain HTTP).
fn ensure_sa(ctx: &BridgeCtx, ns: &str) -> Option<PathBuf> {
    if ctx.ca_pem.is_empty() {
        return None;
    }
    let dir = ctx.sa_root.join(ns);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let _ = std::fs::write(dir.join("ca.crt"), &ctx.ca_pem);
    let _ = std::fs::write(dir.join("namespace"), ns);
    let token_path = dir.join("token");
    if !token_path.exists() {
        let _ = std::fs::write(&token_path, format!("slim-sa.{ns}.token"));
    }
    Some(dir)
}

fn config_data(store: &SharedStore, resource: &str, ns: &str, name: &str) -> BTreeMap<String, String> {
    let Some(info) = store.lookup("", resource) else { return BTreeMap::new() };
    let Some(obj) = store.get(&info, ns, name) else { return BTreeMap::new() };
    let mut out = BTreeMap::new();
    if let Some(m) = obj.get("data").and_then(|d| d.as_object()) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    // Secret stringData (plaintext) overrides
    if let Some(m) = obj.get("stringData").and_then(|d| d.as_object()) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

fn decode_secret(v: &str) -> String {
    // Secret .data values are base64; stringData isn't. Try decode, fall back.
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
    let (mut acc, mut bits) = (0u32, 0);
    for &c in v.as_bytes() {
        if c == b'=' {
            break;
        }
        let d = inv(c);
        if d < 0 {
            return v.to_string(); // not base64 → use as-is
        }
        acc = (acc << 6) | d as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| v.to_string())
}

fn sync_pod(store: &SharedStore, d: &Desired, phase: &str, ip: &str, msg: &str) {
    let Some(info) = store.lookup("", "pods") else { return };
    let mut labels = d.labels.clone();
    if let Some(o) = labels.as_object_mut() {
        o.insert(MANAGED.to_string(), json!("true"));
    }
    let (owner_kind, owner_name) = {
        let mut it = d.owner.splitn(3, '/');
        (it.next().unwrap_or("").to_string(), { it.next(); it.next().unwrap_or("").to_string() })
    };
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": d.pod_name,
            "namespace": d.pod_ns,
            "labels": labels,
            "ownerReferences": [{"kind": owner_kind, "name": owner_name, "controller": true}],
        },
        "spec": d.template,
        "status": {
            "phase": phase,
            "podIP": ip,
            "hostIP": "10.88.0.1",
            "message": msg,
            "conditions": [{"type":"Ready","status": if phase=="Running" {"True"} else {"False"}}],
        },
    });
    store.put(&info, pod, &d.pod_ns, &d.pod_name, false);
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}
