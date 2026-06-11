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

use crate::container::State;
use crate::engine::EngineRef;
use serde_json::{json, Value};
use slim_kubeapi::{ApiServer, ExecHandle, LogOpts, PodProxy, SharedStore, Store};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const OWNER: &str = "io.nebula.kube.owner"; // "<kind>/<ns>/<name>"
const POD_OF: &str = "io.nebula.kube.pod"; // "<ns>/<podname>"
const MANAGED: &str = "io.nebula.kube.bridge"; // "true"
const POD_HOLDER: &str = "io.nebula.kube.holder"; // holder cname (pod netns owner)
const CNAME: &str = "io.nebula.kube.container"; // k8s container name

/// In-cluster context the bridge projects into pods so client-go operators
/// reach the apiserver (KUBERNETES_SERVICE_HOST/PORT + the ServiceAccount dir).
struct BridgeCtx {
    ca_pem: String,
    sa_root: PathBuf,
    kube_host: String, // the bridge gateway IP slimd's TLS listener is on
    kube_port: u16,
    /// Per-container readiness from the prober (cname → ready). Absent = no
    /// readiness probe (ready == running); present = gated on the probe.
    readiness: Arc<Mutex<HashMap<String, bool>>>,
    /// Host dir root for pod emptyDir volumes (<data>/kube-vol).
    vol_root: PathBuf,
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
        readiness: Arc::new(Mutex::new(HashMap::new())),
        vol_root: engine.paths.data.join("kube-vol"),
    };
    register_kubernetes_service(&store, engine, &ctx);

    // Probe loop: evaluates readiness/liveness probes and feeds readiness back
    // into pod status (reconcile reads ctx.readiness). Liveness failures kill
    // the container so the engine's restart supervision recreates it.
    {
        let engine_p = engine.clone();
        let store_p = store.clone();
        let readiness = ctx.readiness.clone();
        std::thread::spawn(move || probe_loop(&engine_p, &store_p, &readiness));
    }

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

/// Serves pod log/exec subresources from the engine. Pod `<pod>`/ns `<ns>` maps
/// to engine container `<ns>_<pod>` (holder); a named sidecar `<c>` maps to
/// `<ns>_<pod>.<c>`.
struct EngineProxy {
    engine: EngineRef,
}

impl EngineProxy {
    /// Resolve (ns, pod, container) → engine container name. Empty container =
    /// the holder; otherwise prefer the sidecar `<ns>_<pod>.<c>`, falling back to
    /// the holder when `<c>` is the first container (same name as the pod).
    fn cname_for(&self, ns: &str, pod: &str, container: &str) -> String {
        let holder = format!("{ns}_{pod}");
        if container.is_empty() {
            return holder;
        }
        let side = format!("{holder}.{container}");
        if self.engine.get_entry(&side).is_ok() {
            return side;
        }
        let init = format!("{holder}.init.{container}");
        if self.engine.get_entry(&init).is_ok() {
            return init;
        }
        holder
    }
}

impl PodProxy for EngineProxy {
    fn logs(&self, ns: &str, pod: &str, container: &str, opts: &LogOpts, w: &mut dyn Write) -> std::io::Result<()> {
        let cname = self.cname_for(ns, pod, container);
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

    fn exec_start(&self, ns: &str, pod: &str, container: &str, cmd: &[String], tty: bool, stdin: bool) -> std::io::Result<ExecHandle> {
        let cname = self.cname_for(ns, pod, container);
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

/// A desired pod: its full container list (container 0 is the pod sandbox /
/// netns holder; the rest join its netns). Keyed in the reconcile map by the
/// holder cname (`<ns>_<pod>`).
struct Desired {
    pod_ns: String,
    pod_name: String,
    holder_cname: String, // <ns>_<pod>
    owner: String,        // "<kind>/<ns>/<name>" or "Pod/<ns>/<name>"
    template: Value,      // full pod spec
    labels: Value,        // pod labels
    restart: &'static str,
    init_containers: Vec<Value>, // spec.initContainers, ordered (run before main)
    containers: Vec<Value>,      // spec.containers, ordered
}

impl Desired {
    /// Engine container name for main container index `i`: the holder keeps the
    /// bare `<ns>_<pod>` name; sidecars get `<ns>_<pod>.<container-name>`.
    fn cname(&self, i: usize) -> String {
        if i == 0 {
            self.holder_cname.clone()
        } else {
            format!("{}.{}", self.holder_cname, cspec_name(&self.containers[i]))
        }
    }
    /// Engine container name for init container index `i`.
    fn init_cname(&self, i: usize) -> String {
        format!("{}.init.{}", self.holder_cname, cspec_name(&self.init_containers[i]))
    }
}

fn cspec_name(c: &Value) -> String {
    c.get("name").and_then(|v| v.as_str()).unwrap_or("main").to_string()
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

    // Ensure each pod's containers exist (holder first) + sync one Pod object.
    for d in desired.values() {
        ensure_pod(engine, store, ctx, d);
    }

    // Remove orphan containers we own that are no longer desired; when a pod's
    // holder goes away, drop its Pod object too.
    let mut desired_cnames: std::collections::BTreeSet<String> = Default::default();
    for d in desired.values() {
        for i in 0..d.init_containers.len() {
            desired_cnames.insert(d.init_cname(i));
        }
        for i in 0..d.containers.len() {
            desired_cnames.insert(d.cname(i));
        }
    }
    for c in engine.list(true) {
        if c.config.labels.get(MANAGED).map(|v| v == "true").unwrap_or(false)
            && !desired_cnames.contains(&c.name)
        {
            let is_holder = c.config.labels.get(POD_HOLDER).map(|h| *h == c.name).unwrap_or(false);
            let _ = engine.remove(&c.name, true, false);
            if is_holder {
                if let Some(pod) = c.config.labels.get(POD_OF) {
                    if let Some((ns, name)) = pod.split_once('/') {
                        if let Some(info) = store.lookup("", "pods") {
                            store.delete(&info, ns, name);
                        }
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
    let containers = template.get("containers").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    if containers.is_empty() {
        return;
    }
    let init_containers = template.get("initContainers").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    for i in 0..replicas {
        let pod_name = format!("{name}-{i}");
        let holder_cname = format!("{ns}_{pod_name}");
        out.insert(
            holder_cname.clone(),
            Desired {
                pod_ns: ns.clone(),
                pod_name,
                holder_cname,
                owner: format!("{kind}/{ns}/{name}"),
                template: template.clone(),
                labels: labels.clone(),
                restart,
                init_containers: init_containers.clone(),
                containers: containers.clone(),
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
    let template = obj.pointer("/spec").cloned().unwrap_or(Value::Null);
    let containers = template.get("containers").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    if containers.is_empty() {
        return;
    }
    let init_containers = template.get("initContainers").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    let holder_cname = format!("{ns}_{name}");
    let restart = obj.pointer("/spec/restartPolicy").and_then(|v| v.as_str())
        .map(|p| if p == "Never" || p == "OnFailure" { "no" } else { "always" })
        .unwrap_or("always");
    out.insert(
        holder_cname.clone(),
        Desired {
            pod_ns: ns.clone(),
            pod_name: name.clone(),
            holder_cname,
            owner: format!("Pod/{ns}/{name}"),
            template,
            labels: obj.pointer("/metadata/labels").cloned().unwrap_or(json!({})),
            restart,
            init_containers,
            containers,
        },
    );
}

/// Ensure a pod's containers. Init containers run first, in order, each to
/// exit-0 before the next; main containers (holder first, then sidecars) start
/// only once all init containers have succeeded. Then sync one Pod object.
fn ensure_pod(engine: &EngineRef, store: &SharedStore, ctx: &BridgeCtx, d: &Desired) {
    // Init containers run with their own bridge netns (the pod sandbox holder
    // isn't up yet) + the pod's emptyDir volumes — the canonical "populate a
    // shared volume before the app starts" use. Restart-on-failure so a flaky
    // init retries (Never pods don't retry).
    let init_restart = if d.restart == "no" { "no" } else { "on-failure" };
    let mut init_done = 0usize;
    let mut initializing = false;
    for (i, ispec) in d.init_containers.iter().enumerate() {
        let cname = d.init_cname(i);
        match engine.get_entry(&cname) {
            Ok(entry) => {
                let st = entry.c.lock().unwrap().state.clone();
                if (st.status == "exited" || st.status == "dead") && st.exit_code == 0 {
                    init_done += 1;
                    continue; // this init finished; move to the next
                }
                initializing = true; // running, created, or retrying-on-failure
                break;
            }
            Err(_) => {
                if let Some(req) = build_create_req(store, ctx, d, ispec, "bridge", init_restart, false) {
                    let image = req.config.image.clone();
                    if engine.store.resolve(&image).is_none() {
                        let _ = engine.ensure_image(&image);
                    }
                    match engine.create(&req, Some(&cname)) {
                        Ok(_) => {
                            if let Err(e) = engine.start(&cname) {
                                eprintln!("slimd: bridge init start {cname} failed: {e}");
                            }
                        }
                        Err(e) => eprintln!("slimd: bridge init create {cname} failed: {e}"),
                    }
                }
                initializing = true;
                break;
            }
        }
    }

    if !initializing {
        // All init containers done → bring up the main containers.
        for (i, cspec) in d.containers.iter().enumerate() {
            let cname = d.cname(i);
            if engine.get_entry(&cname).is_ok() {
                continue;
            }
            let (net, dns) = if i == 0 {
                ("bridge".to_string(), true)
            } else {
                (format!("container:{}", d.holder_cname), false)
            };
            let Some(req) = build_create_req(store, ctx, d, cspec, &net, d.restart, dns) else { continue };
            let image = req.config.image.clone();
            if engine.store.resolve(&image).is_none() {
                let _ = engine.ensure_image(&image);
            }
            match engine.create(&req, Some(&cname)) {
                Ok(_) => {
                    if let Err(e) = engine.start(&cname) {
                        eprintln!("slimd: bridge start {cname} failed: {e}");
                    }
                }
                Err(e) => eprintln!("slimd: bridge create {cname} failed: {e}"),
            }
        }
    }
    sync_pod_status(engine, store, ctx, d, init_done);
}

/// Build aggregated Pod status (init + main containerStatuses) and store it.
/// While init containers are still running the main containers report
/// `PodInitializing` and the pod stays `Pending`.
fn sync_pod_status(engine: &EngineRef, store: &SharedStore, ctx: &BridgeCtx, d: &Desired, init_done: usize) {
    let total_init = d.init_containers.len();
    let initializing = init_done < total_init;

    let mut init_statuses = Vec::with_capacity(total_init);
    for (i, ispec) in d.init_containers.iter().enumerate() {
        let name = cspec_name(ispec);
        let image = ispec.get("image").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match engine.get_entry(&d.init_cname(i)) {
            Ok(e) => {
                let c = e.c.lock().unwrap();
                init_statuses.push(container_status(&name, &image, &c.id, &c.state, None));
            }
            Err(_) => init_statuses.push(waiting_status(&name, &image, "PodInitializing")),
        }
    }

    let mut cstatuses = Vec::with_capacity(d.containers.len());
    let (mut running, mut term_ok, mut term_bad, mut total) = (0usize, 0usize, 0usize, 0usize);
    for (i, cspec) in d.containers.iter().enumerate() {
        total += 1;
        let name = cspec_name(cspec);
        let image = cspec.get("image").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if initializing {
            cstatuses.push(waiting_status(&name, &image, "PodInitializing"));
            continue;
        }
        match engine.get_entry(&d.cname(i)) {
            Ok(entry) => {
                let c = entry.c.lock().unwrap();
                match c.state.status.as_str() {
                    "running" => running += 1,
                    "exited" | "dead" if c.state.exit_code == 0 => term_ok += 1,
                    "exited" | "dead" => term_bad += 1,
                    _ => {}
                }
                let ready_override = ctx.readiness.lock().unwrap().get(&d.cname(i)).copied();
                cstatuses.push(container_status(&name, &image, &c.id, &c.state, ready_override));
            }
            Err(_) => cstatuses.push(waiting_status(&name, &image, "ContainerCreating")),
        }
    }

    let phase = if initializing {
        "Pending"
    } else if total > 0 && running == total {
        "Running"
    } else if term_bad > 0 && d.restart == "no" {
        "Failed"
    } else if total > 0 && term_ok == total {
        "Succeeded"
    } else {
        "Pending"
    };
    let ip = engine
        .get_entry(&d.holder_cname)
        .map(|e| e.c.lock().unwrap().ip.clone())
        .unwrap_or_default();
    sync_pod(store, d, phase, &ip, "", &cstatuses, &init_statuses);
}

/// Build a k8s containerStatus from a live engine container State. `ready_override`
/// is the prober verdict when a readiness probe exists (None = ready when running).
fn container_status(name: &str, image: &str, id: &str, st: &State, ready_override: Option<bool>) -> Value {
    let state_obj = match st.status.as_str() {
        "running" => json!({"running": {"startedAt": st.started_at}}),
        "exited" | "dead" => json!({"terminated": {
            "exitCode": st.exit_code,
            "startedAt": st.started_at,
            "finishedAt": st.finished_at,
            "reason": if st.exit_code == 0 { "Completed" } else { "Error" },
        }}),
        _ => json!({"waiting": {"reason": "ContainerCreating"}}),
    };
    let ready = st.status == "running" && ready_override.unwrap_or(true);
    json!({
        "name": name,
        "image": image,
        "imageID": image,
        "containerID": format!("slim://{id}"),
        "ready": ready,
        "started": st.status == "running",
        "restartCount": st.restart_count,
        "state": state_obj,
    })
}

/// A containerStatus for a container that doesn't exist yet (waiting).
fn waiting_status(name: &str, image: &str, reason: &str) -> Value {
    json!({
        "name": name, "image": image, "imageID": "",
        "ready": false, "started": false, "restartCount": 0,
        "state": {"waiting": {"reason": reason}},
    })
}

/// Translate one pod container (`cspec`) into a docker-style create request,
/// resolving env from ConfigMaps/Secrets and emptyDir mounts. `network_mode` is
/// "bridge" for the sandbox holder / init containers, or `container:<holder>` for
/// sidecars; `holder_dns` adds the pod's DNS aliases (holder only).
fn build_create_req(
    store: &SharedStore,
    ctx: &BridgeCtx,
    d: &Desired,
    cspec: &Value,
    network_mode: &str,
    restart: &str,
    holder_dns: bool,
) -> Option<slim_api::container::ContainerCreateRequest> {
    let image = cspec.get("image").and_then(|v| v.as_str())?.to_string();

    let mut env = Vec::new();
    // In-cluster config for operators (client-go reads these env vars).
    env.push(format!("KUBERNETES_SERVICE_HOST={}", ctx.kube_host));
    env.push(format!("KUBERNETES_SERVICE_PORT={}", ctx.kube_port));
    env.push(format!("KUBERNETES_SERVICE_PORT_HTTPS={}", ctx.kube_port));
    env.push(format!("KUBERNETES_PORT=tcp://{}:{}", ctx.kube_host, ctx.kube_port));
    for ef in cspec.get("envFrom").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
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
    for e in cspec.get("env").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
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

    let cmd = strs(cspec.get("args"));
    let entrypoint = strs(cspec.get("command"));

    let mut labels = std::collections::BTreeMap::new();
    labels.insert(MANAGED.to_string(), "true".to_string());
    labels.insert(OWNER.to_string(), d.owner.clone());
    labels.insert(POD_OF.to_string(), format!("{}/{}", d.pod_ns, d.pod_name));
    labels.insert(POD_HOLDER.to_string(), d.holder_cname.clone());
    labels.insert(CNAME.to_string(), cspec_name(cspec));
    if let Some(obj) = d.labels.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                labels.insert(k.clone(), s.to_string());
            }
        }
    }

    let mut config = slim_api::container::ContainerConfig { image, cmd, env, labels, ..Default::default() };
    if !entrypoint.is_empty() {
        config.entrypoint = Some(entrypoint);
    }

    // Binds: ServiceAccount projection (every container) + emptyDir mounts.
    let mut binds = Vec::new();
    if let Some(sa_dir) = ensure_sa(ctx, &d.pod_ns) {
        binds.push(format!("{}:/var/run/secrets/kubernetes.io/serviceaccount:ro", sa_dir.display()));
    }
    binds.extend(volume_binds(ctx, store, d, cspec));

    let host_config = slim_api::container::HostConfig {
        restart_policy: slim_api::container::RestartPolicy { name: restart.to_string(), maximum_retry_count: 0 },
        network_mode: network_mode.to_string(),
        binds,
        ..Default::default()
    };
    // DNS aliases (pod name + app label) belong to the holder, which owns the IP.
    let mut endpoints = std::collections::BTreeMap::new();
    if holder_dns {
        let mut aliases = vec![d.pod_name.clone()];
        if let Some(app) = d.labels.get("app").and_then(|v| v.as_str()) {
            aliases.push(app.to_string());
        }
        endpoints.insert("bridge".to_string(), slim_api::container::EndpointSettings { aliases, ..Default::default() });
    }

    Some(slim_api::container::ContainerCreateRequest {
        config,
        host_config,
        networking_config: slim_api::container::NetworkingConfig { endpoints_config: endpoints },
    })
}

/// Resolve a container's volumeMounts against the pod's volumes into host binds.
/// Supports emptyDir (shared per-pod host dir), configMap/secret (materialized to
/// files, mounted read-only), and hostPath (direct bind). subPath + readOnly are
/// honored. Other volume types (downwardAPI/projected/csi/...) are skipped.
fn volume_binds(ctx: &BridgeCtx, store: &SharedStore, d: &Desired, cspec: &Value) -> Vec<String> {
    let mut by_name: BTreeMap<String, Value> = BTreeMap::new();
    for v in d.template.get("volumes").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
            by_name.insert(n.to_string(), v);
        }
    }
    let mut out = Vec::new();
    for m in cspec.get("volumeMounts").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let (Some(name), Some(mount_path)) = (
            m.get("name").and_then(|v| v.as_str()),
            m.get("mountPath").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let Some(vol) = by_name.get(name) else { continue };
        let sub = m.get("subPath").and_then(|v| v.as_str());
        let ro = m.get("readOnly").and_then(|v| v.as_bool()).unwrap_or(false);

        if vol.get("emptyDir").is_some() {
            let mut host = ctx.vol_root.join(&d.holder_cname).join(name);
            if let Some(s) = sub {
                host = host.join(s);
            }
            if std::fs::create_dir_all(&host).is_ok() {
                out.push(bind_str(&host, mount_path, ro));
            }
        } else if let Some(cm) = vol.get("configMap") {
            let cmname = cm.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let data = config_data(store, "configmaps", &d.pod_ns, cmname);
            if let Some(b) = materialize_bind(ctx, d, &format!("cm-{name}"), &data, cm.get("items"), mount_path, sub) {
                out.push(b);
            }
        } else if let Some(sec) = vol.get("secret") {
            let sname = sec.get("secretName").and_then(|v| v.as_str()).unwrap_or("");
            let data = secret_files(store, &d.pod_ns, sname);
            if let Some(b) = materialize_bind(ctx, d, &format!("sec-{name}"), &data, sec.get("items"), mount_path, sub) {
                out.push(b);
            }
        } else if let Some(hp) = vol.get("hostPath") {
            if let Some(path) = hp.get("path").and_then(|v| v.as_str()) {
                let mut src = PathBuf::from(path);
                if let Some(s) = sub {
                    src = src.join(s);
                }
                out.push(bind_str(&src, mount_path, ro));
            }
        }
    }
    out
}

fn bind_str(host: &std::path::Path, mount_path: &str, ro: bool) -> String {
    if ro {
        format!("{}:{}:ro", host.display(), mount_path)
    } else {
        format!("{}:{}", host.display(), mount_path)
    }
}

/// Materialize configMap/secret data to a per-pod host dir (one file per key,
/// honoring `items` key→path selection) and return a read-only bind. With a
/// subPath, bind just that one file. Snapshot at create time (a slim liberty:
/// updates to the ConfigMap/Secret don't propagate to a running container).
fn materialize_bind(
    ctx: &BridgeCtx,
    d: &Desired,
    tag: &str,
    data: &BTreeMap<String, String>,
    items: Option<&Value>,
    mount_path: &str,
    sub: Option<&str>,
) -> Option<String> {
    let dir = ctx.vol_root.join(&d.holder_cname).join(tag);
    std::fs::create_dir_all(&dir).ok()?;
    let mut files: Vec<(String, String)> = Vec::new();
    if let Some(arr) = items.and_then(|v| v.as_array()) {
        for it in arr {
            let k = it.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let p = it.get("path").and_then(|v| v.as_str()).unwrap_or(k);
            if let Some(val) = data.get(k) {
                files.push((p.to_string(), val.clone()));
            }
        }
    } else {
        for (k, v) in data {
            files.push((k.clone(), v.clone()));
        }
    }
    for (rel, val) in &files {
        let fpath = dir.join(rel);
        if let Some(parent) = fpath.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&fpath, val);
    }
    match sub {
        Some(s) => Some(bind_str(&dir.join(s), mount_path, true)),
        None => Some(bind_str(&dir, mount_path, true)),
    }
}

/// Secret data decoded to plaintext: base64 `.data` values + plaintext `.stringData`.
fn secret_files(store: &SharedStore, ns: &str, name: &str) -> BTreeMap<String, String> {
    let Some(info) = store.lookup("", "secrets") else { return BTreeMap::new() };
    let Some(obj) = store.get(&info, ns, name) else { return BTreeMap::new() };
    let mut out = BTreeMap::new();
    if let Some(m) = obj.get("data").and_then(|d| d.as_object()) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), decode_secret(s));
            }
        }
    }
    if let Some(m) = obj.get("stringData").and_then(|d| d.as_object()) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
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

fn sync_pod(store: &SharedStore, d: &Desired, phase: &str, ip: &str, msg: &str, cstatuses: &[Value], init_statuses: &[Value]) {
    let Some(info) = store.lookup("", "pods") else { return };
    let mut labels = d.labels.clone();
    if let Some(o) = labels.as_object_mut() {
        o.insert(MANAGED.to_string(), json!("true"));
    }
    let (owner_kind, owner_name) = {
        let mut it = d.owner.splitn(3, '/');
        (it.next().unwrap_or("").to_string(), { it.next(); it.next().unwrap_or("").to_string() })
    };
    // Pod is Ready iff every main container reports ready.
    let all_ready = !cstatuses.is_empty()
        && cstatuses.iter().all(|c| c.get("ready").and_then(|r| r.as_bool()).unwrap_or(false));
    // Initialized iff every init container has terminated successfully.
    let initialized = init_statuses
        .iter()
        .all(|c| c.pointer("/state/terminated/exitCode").and_then(|v| v.as_i64()) == Some(0));
    let yn = |b: bool| if b { "True" } else { "False" };
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
            "initContainerStatuses": init_statuses,
            "containerStatuses": cstatuses,
            "conditions": [
                {"type":"PodScheduled","status":"True"},
                {"type":"Initialized","status": yn(initialized)},
                {"type":"ContainersReady","status": yn(all_ready)},
                {"type":"Ready","status": yn(all_ready)},
            ],
        },
    });
    store.put(&info, pod, &d.pod_ns, &d.pod_name, false);
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

// ---- readiness / liveness probes (Phase 2) ----

enum ProbeKind {
    Http { path: String, port: u16 },
    Tcp { port: u16 },
    Exec { cmd: Vec<String> },
}

struct Probe {
    kind: ProbeKind,
    initial_delay: u64,
    period: u64,
    success_threshold: u32,
    failure_threshold: u32,
    timeout: u64,
}

#[derive(Default)]
struct ProbeTrack {
    first_seen: Option<Instant>,
    last_run: Option<Instant>,
    successes: u32,
    failures: u32,
    passing: bool,
}

/// Probe loop: every second, evaluate due readiness/liveness probes for managed
/// running pods. Readiness feeds `readiness` (→ pod status); liveness failure
/// kills the container so restart supervision recreates it. One thread, serial —
/// fine at embedding scale; a slow exec/HTTP probe only delays the next tick.
fn probe_loop(engine: &EngineRef, store: &SharedStore, readiness: &Arc<Mutex<HashMap<String, bool>>>) {
    let mut tracks: HashMap<(String, &'static str), ProbeTrack> = HashMap::new();
    loop {
        probe_tick(engine, store, readiness, &mut tracks);
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn probe_tick(
    engine: &EngineRef,
    store: &SharedStore,
    readiness: &Arc<Mutex<HashMap<String, bool>>>,
    tracks: &mut HashMap<(String, &'static str), ProbeTrack>,
) {
    let Some(info) = store.lookup("", "pods") else { return };
    let pods = store.list(&info, None, &[]).0;
    let mut live: HashSet<String> = HashSet::new();
    for pod in pods {
        if pod.pointer("/metadata/labels").and_then(|l| l.get(MANAGED)).is_none() {
            continue;
        }
        let ns = pod.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or("default");
        let name = pod.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let cname = format!("{ns}_{name}");
        let pod_ip = pod.pointer("/status/podIP").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let c0 = pod.pointer("/spec/containers/0");
        let readiness_probe = c0.and_then(|c| c.get("readinessProbe")).and_then(parse_probe);
        let liveness_probe = c0.and_then(|c| c.get("livenessProbe")).and_then(parse_probe);
        live.insert(cname.clone());

        let running = engine.get_entry(&cname).map(|e| e.c.lock().unwrap().running()).unwrap_or(false);
        if !running {
            // Not running (or restarting): drop tracks so probing restarts clean.
            if readiness_probe.is_some() {
                readiness.lock().unwrap().insert(cname.clone(), false);
            }
            tracks.remove(&(cname.clone(), "readiness"));
            tracks.remove(&(cname.clone(), "liveness"));
            continue;
        }

        if let Some(p) = &readiness_probe {
            let pass = eval_probe(engine, &cname, &pod_ip, p, tracks, "readiness");
            readiness.lock().unwrap().insert(cname.clone(), pass);
        }
        if let Some(p) = &liveness_probe {
            let healthy = eval_probe(engine, &cname, &pod_ip, p, tracks, "liveness");
            if !healthy {
                eprintln!("slimd: liveness probe failed for {cname}; restarting");
                // SIGKILL the process directly (NOT engine.kill, which marks the
                // container stopping and vetoes its restart policy). The exit
                // monitor then restarts it per `always` and bumps restartCount —
                // exactly kubelet liveness semantics.
                if let Ok(entry) = engine.get_entry(&cname) {
                    let (pid, id) = {
                        let c = entry.c.lock().unwrap();
                        (c.state.pid, c.id.clone())
                    };
                    // Never signal pid <= 1: signal_pid(0) would SIGKILL slimd's
                    // whole process group (it's pid 1 in the vessel).
                    if pid > 1 {
                        let sig = slim_runtime::parse_signal("KILL");
                        let _ = slim_runtime::kill_cgroup(&id);
                        let _ = slim_runtime::signal_pid(pid, sig);
                    }
                }
                tracks.remove(&(cname.clone(), "liveness"));
                tracks.remove(&(cname.clone(), "readiness"));
                readiness.lock().unwrap().insert(cname.clone(), false);
            }
        }
    }
    // GC state for pods that disappeared.
    readiness.lock().unwrap().retain(|k, _| live.contains(k));
    tracks.retain(|(c, _), _| live.contains(c));
}

/// Run a probe if due; track success/failure streaks. Returns readiness
/// "passing" for the readiness kind, or "healthy" (failures < threshold) for
/// liveness. Respects initialDelaySeconds and periodSeconds.
fn eval_probe(
    engine: &EngineRef,
    cname: &str,
    pod_ip: &str,
    p: &Probe,
    tracks: &mut HashMap<(String, &'static str), ProbeTrack>,
    kind: &'static str,
) -> bool {
    let now = Instant::now();
    let t = tracks.entry((cname.to_string(), kind)).or_default();
    if t.first_seen.is_none() {
        t.first_seen = Some(now);
    }
    if now.duration_since(t.first_seen.unwrap()).as_secs() < p.initial_delay {
        return kind != "readiness"; // not ready yet; liveness healthy during grace
    }
    let due = t.last_run.map(|l| now.duration_since(l).as_secs() >= p.period).unwrap_or(true);
    if due {
        t.last_run = Some(now);
        if run_probe(engine, cname, pod_ip, p) {
            t.successes += 1;
            t.failures = 0;
            if t.successes >= p.success_threshold {
                t.passing = true;
            }
        } else {
            t.failures += 1;
            t.successes = 0;
            if t.failures >= p.failure_threshold {
                t.passing = false;
            }
        }
    }
    if kind == "readiness" {
        t.passing
    } else {
        t.failures < p.failure_threshold
    }
}

fn run_probe(engine: &EngineRef, cname: &str, pod_ip: &str, p: &Probe) -> bool {
    match &p.kind {
        ProbeKind::Tcp { port } => tcp_ok(pod_ip, *port, p.timeout),
        ProbeKind::Http { path, port } => http_ok(pod_ip, *port, path, p.timeout),
        ProbeKind::Exec { cmd } => exec_ok(engine, cname, cmd),
    }
}

fn tcp_ok(ip: &str, port: u16, timeout: u64) -> bool {
    use std::net::ToSocketAddrs;
    if ip.is_empty() {
        return false;
    }
    match format!("{ip}:{port}").to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(sa) => std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(timeout)).is_ok(),
        None => false,
    }
}

fn http_ok(ip: &str, port: u16, path: &str, timeout: u64) -> bool {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    if ip.is_empty() {
        return false;
    }
    let Some(sa) = format!("{ip}:{port}").to_socket_addrs().ok().and_then(|mut a| a.next()) else { return false };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(timeout)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(timeout)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(timeout)));
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let req = format!("GET {path} HTTP/1.0\r\nHost: {ip}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    let n = s.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return false;
    }
    String::from_utf8_lossy(&buf[..n])
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .map(|c| (200..400).contains(&c))
        .unwrap_or(false)
}

fn exec_ok(engine: &EngineRef, cname: &str, cmd: &[String]) -> bool {
    let Ok(entry) = engine.get_entry(cname) else { return false };
    let pid = {
        let c = entry.c.lock().unwrap();
        if !c.running() {
            return false;
        }
        c.state.pid
    };
    let spec = slim_runtime::ExecSpec {
        argv: cmd.to_vec(),
        env: vec![],
        cwd: String::new(),
        user: String::new(),
        tty: false,
        open_stdin: false,
    };
    match slim_runtime::exec_in_container_cg(pid, &spec, Some(cname)) {
        Ok(h) => slim_runtime::wait_pid(h.pid).map(|s| s.code == 0).unwrap_or(false),
        Err(_) => false,
    }
}

fn parse_probe(v: &Value) -> Option<Probe> {
    let u = |k: &str, d: u64| v.get(k).and_then(|x| x.as_u64()).unwrap_or(d);
    let kind = if let Some(h) = v.get("httpGet") {
        ProbeKind::Http {
            path: h.get("path").and_then(|x| x.as_str()).unwrap_or("/").to_string(),
            port: probe_port(h.get("port"))?,
        }
    } else if let Some(t) = v.get("tcpSocket") {
        ProbeKind::Tcp { port: probe_port(t.get("port"))? }
    } else if let Some(e) = v.get("exec") {
        let cmd = strs(e.get("command"));
        if cmd.is_empty() {
            return None;
        }
        ProbeKind::Exec { cmd }
    } else {
        return None;
    };
    Some(Probe {
        kind,
        initial_delay: u("initialDelaySeconds", 0),
        period: u("periodSeconds", 10).max(1),
        success_threshold: u("successThreshold", 1).max(1) as u32,
        failure_threshold: u("failureThreshold", 3).max(1) as u32,
        timeout: u("timeoutSeconds", 1).max(1),
    })
}

/// Probe port as an integer (named ports aren't resolved — a slim liberty).
fn probe_port(v: Option<&Value>) -> Option<u16> {
    let v = v?;
    v.as_u64().map(|n| n as u16).or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
