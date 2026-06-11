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
use slim_kubeapi::{ApiServer, SharedStore, Store};
use std::collections::BTreeMap;

const OWNER: &str = "io.nebula.kube.owner"; // "<kind>/<ns>/<name>"
const POD_OF: &str = "io.nebula.kube.pod"; // "<ns>/<podname>"
const MANAGED: &str = "io.nebula.kube.bridge"; // "true"

/// Start the apiserver-lite + the reconcile loop. Returns the shared store
/// (so callers could seed it). Spawns background threads; never blocks.
pub fn start(engine: &EngineRef, api_addr: &str) -> SharedStore {
    let store = Store::new();
    seed_cluster(&store);

    // Serve the API.
    let api = ApiServer::new(store.clone());
    let addr = api_addr.to_string();
    std::thread::spawn(move || {
        if let Err(e) = api.serve(&addr) {
            eprintln!("slimd: kube apiserver on {addr} stopped: {e}");
        }
    });
    println!("slimd: kube apiserver-lite listening on {api_addr}");

    // Reconcile loop.
    let engine = engine.clone();
    let store2 = store.clone();
    std::thread::spawn(move || reconcile_loop(&engine, &store2));
    store
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

fn reconcile_loop(engine: &EngineRef, store: &SharedStore) {
    loop {
        if let Err(e) = reconcile_once(engine, store) {
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

fn reconcile_once(engine: &EngineRef, store: &SharedStore) -> std::io::Result<()> {
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
        ensure_container(engine, store, d);
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

fn ensure_container(engine: &EngineRef, store: &SharedStore, d: &Desired) {
    let exists = engine.get_entry(&d.cname).is_ok();
    if !exists {
        let Some(req) = build_create_req(store, d) else { return };
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
fn build_create_req(store: &SharedStore, d: &Desired) -> Option<slim_api::container::ContainerCreateRequest> {
    let containers = d.template.get("containers").and_then(|c| c.as_array())?;
    let c0 = containers.first()?;
    let image = c0.get("image").and_then(|v| v.as_str())?.to_string();

    let mut env = Vec::new();
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
    let host_config = slim_api::container::HostConfig {
        restart_policy: slim_api::container::RestartPolicy {
            name: d.restart.to_string(),
            maximum_retry_count: 0,
        },
        network_mode: "bridge".to_string(),
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
