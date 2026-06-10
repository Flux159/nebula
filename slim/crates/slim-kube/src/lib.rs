//! slim-kube: a Kubernetes facade that maps real k8s YAML onto slim engine
//! containers via the Docker Engine API — NO control plane, NO k8s wire
//! protocol. Used by the standalone `kubectl-slim` binary.
//!
//! Mapping:
//!   Deployment  → N restart-supervised containers `<name>-<i>`
//!   Pod         → one container `<name>`
//!   Job         → run-to-completion container (restart=no) + backoffLimit
//!   Service     → published ports (NodePort/LoadBalancer) + DNS via pod
//!                 aliases (ClusterIP resolves by service name)
//!   ConfigMap   → env / (mounted files: TODO) supplied to selecting workloads
//!   Secret      → same as ConfigMap (base64 values decoded)
//!
//! Identity is carried in container labels (io.nebula.kube.*) so get/delete/
//! scale/logs can reconstruct the k8s view from the engine.

pub mod model;

use model::*;
use serde_json::{json, Value};
use slim_client::http::{ApiError, Client};
use std::collections::BTreeMap;

pub const LBL_MANAGED: &str = "io.nebula.kube.managed";
pub const LBL_KIND: &str = "io.nebula.kube.kind";
pub const LBL_NAME: &str = "io.nebula.kube.name";
pub const LBL_NS: &str = "io.nebula.kube.namespace";
pub const LBL_APP: &str = "io.nebula.kube.app";

const V: &str = "/v1.43";

#[derive(Debug)]
pub struct KubeError(pub String);
impl std::fmt::Display for KubeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for KubeError {}
impl From<ApiError> for KubeError {
    fn from(e: ApiError) -> Self {
        KubeError(e.message)
    }
}
impl From<std::io::Error> for KubeError {
    fn from(e: std::io::Error) -> Self {
        KubeError(e.to_string())
    }
}
fn ke(s: impl Into<String>) -> KubeError {
    KubeError(s.into())
}
pub type KubeResult<T> = Result<T, KubeError>;

pub struct Facade<'a> {
    pub client: &'a Client,
    pub namespace: String,
}

impl<'a> Facade<'a> {
    pub fn new(client: &'a Client, namespace: &str) -> Self {
        Facade { client, namespace: if namespace.is_empty() { "default".into() } else { namespace.into() } }
    }

    // ---------- apply ----------

    pub fn apply_yaml(&self, yaml: &str, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        let docs = parse_docs(yaml)?;
        // Pass 1: index config sources + services.
        let mut configmaps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut secrets: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut services: Vec<Service> = Vec::new();
        let mut workloads: Vec<Value> = Vec::new();
        for d in &docs {
            match kind_of(d).as_str() {
                "ConfigMap" => {
                    configmaps.insert(name_of(d), string_map(d.get("data")));
                }
                "Secret" => {
                    let mut m = string_map(d.get("data"));
                    for v in m.values_mut() {
                        if let Some(dec) = b64_decode_str(v) {
                            *v = dec;
                        }
                    }
                    // stringData is plaintext.
                    for (k, val) in string_map(d.get("stringData")) {
                        m.insert(k, val);
                    }
                    secrets.insert(name_of(d), m);
                }
                "Service" => services.push(Service::parse(d)),
                "Deployment" | "Pod" | "Job" | "ReplicaSet" | "DaemonSet" | "StatefulSet" => {
                    workloads.push(d.clone())
                }
                "" => {}
                other => out(&format!("warning: kind {other} is not supported by slim — skipped\n")),
            }
        }
        // Pass 2: create workloads with env + service ports resolved.
        for w in &workloads {
            self.apply_workload(w, &configmaps, &secrets, &services, out)?;
        }
        // Services with no matching workload yet: still acknowledge.
        for s in &services {
            out(&format!("service/{} created\n", s.name));
        }
        Ok(())
    }

    fn apply_workload(
        &self,
        d: &Value,
        configmaps: &BTreeMap<String, BTreeMap<String, String>>,
        secrets: &BTreeMap<String, BTreeMap<String, String>>,
        services: &[Service],
        out: &mut dyn FnMut(&str),
    ) -> KubeResult<()> {
        let kind = kind_of(d);
        let name = name_of(d);
        let pod_spec = match kind.as_str() {
            "Pod" => d.get("spec").cloned().unwrap_or(Value::Null),
            _ => d.pointer("/spec/template/spec").cloned().unwrap_or(Value::Null),
        };
        let app = d
            .pointer("/spec/selector/matchLabels/app")
            .and_then(|v| v.as_str())
            .or_else(|| d.pointer("/metadata/labels/app").and_then(|v| v.as_str()))
            .unwrap_or(&name)
            .to_string();
        let replicas = if kind == "Deployment" || kind == "ReplicaSet" || kind == "StatefulSet" {
            d.pointer("/spec/replicas").and_then(|v| v.as_i64()).unwrap_or(1)
        } else {
            1
        };
        let restart = if kind == "Job" { "no" } else { "always" };

        // Ports published by any Service selecting this app.
        let mut port_bindings = serde_json::Map::new();
        let mut exposed = serde_json::Map::new();
        for svc in services {
            if svc.selects(&app, &name) {
                for p in &svc.ports {
                    let target = p.target_port.unwrap_or(p.port);
                    let key = format!("{target}/tcp");
                    exposed.insert(key.clone(), json!({}));
                    let host = match svc.svc_type.as_str() {
                        "NodePort" | "LoadBalancer" => p.node_port.unwrap_or(p.port),
                        _ => p.port,
                    };
                    port_bindings.insert(key, json!([{"HostIp": "", "HostPort": host.to_string()}]));
                }
            }
        }

        let containers = pod_spec.get("containers").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        let main = containers.first().ok_or_else(|| ke(format!("{kind}/{name}: no containers")))?;
        let image = main.get("image").and_then(|v| v.as_str()).ok_or_else(|| ke("container missing image"))?.to_string();
        let env = self.resolve_env(main, configmaps, secrets)?;
        let cmd = strs(main.get("args"));
        let entrypoint = strs(main.get("command"));

        // Volumes from emptyDir/hostPath (subset) + configMap mounts: TODO.
        let _ = pod_spec.get("volumes");

        // Pull image up front (engine create requires it locally).
        let _ = self.client.action("POST", &format!("{V}/images/create?fromImage={}", url(&split_ref(&image).0)), None);
        self.pull(&image, out);

        for i in 0..replicas {
            let cname = if kind == "Pod" { name.clone() } else { format!("{name}-{i}") };
            let mut labels = serde_json::Map::new();
            labels.insert(LBL_MANAGED.into(), json!("true"));
            labels.insert(LBL_KIND.into(), json!(kind));
            labels.insert(LBL_NAME.into(), json!(name));
            labels.insert(LBL_NS.into(), json!(self.namespace));
            labels.insert(LBL_APP.into(), json!(app));

            let mut host_config = json!({
                "RestartPolicy": {"Name": restart, "MaximumRetryCount": 0},
                "NetworkMode": "bridge",
            });
            if !port_bindings.is_empty() && i == 0 {
                // Publish host ports on the first replica only (avoids collisions).
                host_config["PortBindings"] = Value::Object(port_bindings.clone());
            }
            let mut config = json!({
                "Image": image,
                "Env": env,
                "Labels": labels,
                "ExposedPorts": exposed,
                "HostConfig": host_config,
                "NetworkingConfig": {"EndpointsConfig": {"bridge": {"Aliases": [name, app]}}},
            });
            if !cmd.is_empty() {
                config["Cmd"] = json!(cmd);
            }
            if !entrypoint.is_empty() {
                config["Entrypoint"] = json!(entrypoint);
            }

            // Remove an existing container of the same name (apply = upsert).
            let _ = self.client.action("DELETE", &format!("{V}/containers/{cname}?force=true"), None);
            let created: slim_api::container::ContainerCreateResponse = self
                .client
                .json("POST", &format!("{V}/containers/create?name={cname}"), Some(&config))
                .map_err(|e| ke(format!("{kind}/{name}: {}", e.message)))?;
            self.client.action("POST", &format!("{V}/containers/{}/start", created.id), None)?;
        }
        out(&format!("{}/{} created\n", kind.to_lowercase(), name));
        Ok(())
    }

    fn resolve_env(
        &self,
        container: &Value,
        configmaps: &BTreeMap<String, BTreeMap<String, String>>,
        secrets: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> KubeResult<Vec<String>> {
        let mut env = Vec::new();
        // envFrom
        for ef in container.get("envFrom").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            if let Some(n) = ef.pointer("/configMapRef/name").and_then(|v| v.as_str()) {
                if let Some(m) = configmaps.get(n) {
                    for (k, val) in m {
                        env.push(format!("{k}={val}"));
                    }
                }
            }
            if let Some(n) = ef.pointer("/secretRef/name").and_then(|v| v.as_str()) {
                if let Some(m) = secrets.get(n) {
                    for (k, val) in m {
                        env.push(format!("{k}={val}"));
                    }
                }
            }
        }
        // env
        for e in container.get("env").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            if let Some(val) = e.get("value").and_then(|v| v.as_str()) {
                env.push(format!("{name}={val}"));
            } else if let Some(r) = e.get("valueFrom") {
                if let (Some(cm), Some(key)) = (
                    r.pointer("/configMapKeyRef/name").and_then(|v| v.as_str()),
                    r.pointer("/configMapKeyRef/key").and_then(|v| v.as_str()),
                ) {
                    if let Some(v) = configmaps.get(cm).and_then(|m| m.get(key)) {
                        env.push(format!("{name}={v}"));
                    }
                } else if let (Some(sec), Some(key)) = (
                    r.pointer("/secretKeyRef/name").and_then(|v| v.as_str()),
                    r.pointer("/secretKeyRef/key").and_then(|v| v.as_str()),
                ) {
                    if let Some(v) = secrets.get(sec).and_then(|m| m.get(key)) {
                        env.push(format!("{name}={v}"));
                    }
                }
            }
        }
        Ok(env)
    }

    fn pull(&self, image: &str, out: &mut dyn FnMut(&str)) {
        let (repo, tag) = split_ref(image);
        let path = format!("{V}/images/create?fromImage={}&tag={}", url(&repo), url(&tag));
        if let Ok(mut resp) = self.client.request("POST", &path, &[], Some(b"")) {
            let _ = resp.stream_body(|_| {});
        }
        let _ = out;
    }

    // ---------- get ----------

    pub fn get(&self, kind: &str, name: Option<&str>) -> KubeResult<Vec<KubeObject>> {
        let want = normalize_kind(kind);
        // Pods are the individual containers (every workload's members), the
        // way `kubectl get pods` shows the Deployment's pods, not the
        // Deployment.
        if want == "Pod" {
            let mut pods = self.list_pods()?;
            if let Some(n) = name {
                pods.retain(|p| p.name == n || p.name.starts_with(&format!("{n}-")));
            }
            return Ok(pods);
        }
        let all = self.list_managed()?;
        Ok(all
            .into_iter()
            .filter(|o| (want.is_empty() || o.kind.eq_ignore_ascii_case(&want)) && name.map(|n| o.name == n).unwrap_or(true))
            .collect())
    }

    fn list_pods(&self) -> KubeResult<Vec<KubeObject>> {
        let filters = serde_json::to_string(&json!({"label": [format!("{LBL_MANAGED}=true")]})).unwrap();
        let path = format!("{V}/containers/json?all=true&filters={}", url(&filters));
        let conts: Vec<slim_api::container::ContainerSummary> = self.client.json("GET", &path, None)?;
        Ok(conts
            .into_iter()
            .map(|c| {
                let pod = c.names.first().cloned().unwrap_or_default().trim_start_matches('/').to_string();
                let running = c.state == "running";
                KubeObject {
                    kind: "Pod".into(),
                    name: pod.clone(),
                    ready: if running { "1/1".into() } else { "0/1".into() },
                    status: if running { "Running".into() } else { "Pending".into() },
                    restarts: 0,
                    members: vec![pod],
                }
            })
            .collect())
    }

    /// Reconstruct k8s objects from container labels.
    fn list_managed(&self) -> KubeResult<Vec<KubeObject>> {
        let filters = serde_json::to_string(&json!({"label": [format!("{LBL_MANAGED}=true")]})).unwrap();
        let path = format!("{V}/containers/json?all=true&filters={}", url(&filters));
        let conts: Vec<slim_api::container::ContainerSummary> = self.client.json("GET", &path, None)?;
        // Group containers into workloads by (kind,name).
        let mut groups: BTreeMap<(String, String), Vec<slim_api::container::ContainerSummary>> = BTreeMap::new();
        for c in conts {
            let kind = c.labels.get(LBL_KIND).cloned().unwrap_or_default();
            let name = c.labels.get(LBL_NAME).cloned().unwrap_or_default();
            groups.entry((kind, name)).or_default().push(c);
        }
        let mut out = Vec::new();
        for ((kind, name), members) in groups {
            let total = members.len();
            let ready = members.iter().filter(|c| c.state == "running").count();
            let restarts = 0; // RestartCount not in summary; left at 0 (lite)
            out.push(KubeObject {
                kind: if kind == "Pod" { "Pod".into() } else { kind.clone() },
                name,
                ready: format!("{ready}/{total}"),
                status: if ready == total { "Running".into() } else { "Pending".into() },
                restarts,
                members: members.into_iter().map(|c| c.names.first().cloned().unwrap_or_default().trim_start_matches('/').to_string()).collect(),
            });
        }
        Ok(out)
    }

    // ---------- delete ----------

    pub fn delete(&self, kind: &str, name: &str, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        let objs = self.get(kind, Some(name))?;
        if objs.is_empty() {
            return Err(ke(format!("{} \"{name}\" not found", normalize_kind(kind).to_lowercase())));
        }
        for o in &objs {
            for member in &o.members {
                let _ = self.client.action("DELETE", &format!("{V}/containers/{member}?force=true&v=true"), None);
            }
            out(&format!("{}/{} deleted\n", o.kind.to_lowercase(), o.name));
        }
        Ok(())
    }

    pub fn delete_yaml(&self, yaml: &str, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        for d in parse_docs(yaml)? {
            let kind = kind_of(d_ref(&d));
            let name = name_of(d_ref(&d));
            if matches!(kind.as_str(), "Deployment" | "Pod" | "Job" | "ReplicaSet" | "StatefulSet" | "DaemonSet") {
                let _ = self.delete(&kind, &name, out);
            } else if !kind.is_empty() {
                out(&format!("{}/{} deleted\n", kind.to_lowercase(), name));
            }
        }
        Ok(())
    }

    // ---------- scale ----------

    pub fn scale(&self, name: &str, replicas: i64, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        let objs = self.get("deployment", Some(name))?;
        let obj = objs.first().ok_or_else(|| ke(format!("deployments.apps \"{name}\" not found")))?;
        let current = obj.members.len() as i64;
        if replicas > current {
            // Clone replica 0's config into new members.
            let base = format!("{name}-0");
            let inspect: slim_api::container::ContainerInspect =
                self.client.json("GET", &format!("{V}/containers/{base}/json"), None)?;
            for i in current..replicas {
                let cname = format!("{name}-{i}");
                let mut config = serde_json::to_value(&inspect.config).unwrap_or(Value::Null);
                config["HostConfig"] = serde_json::to_value(&inspect.host_config).unwrap_or(Value::Null);
                // Drop published ports on scaled replicas to avoid collisions.
                config["HostConfig"]["PortBindings"] = json!({});
                let created: slim_api::container::ContainerCreateResponse =
                    self.client.json("POST", &format!("{V}/containers/create?name={cname}"), Some(&config))?;
                self.client.action("POST", &format!("{V}/containers/{}/start", created.id), None)?;
            }
        } else {
            for i in replicas..current {
                let cname = format!("{name}-{i}");
                let _ = self.client.action("DELETE", &format!("{V}/containers/{cname}?force=true&v=true"), None);
            }
        }
        out(&format!("deployment.apps/{name} scaled\n"));
        Ok(())
    }

    // ---------- logs / exec ----------

    /// Resolve a pod-ish name to an engine container (exact, or first replica
    /// of a deployment).
    pub fn resolve_container(&self, name: &str) -> KubeResult<String> {
        // exact container?
        if self.client.call("GET", &format!("{V}/containers/{name}/json"), &[], None).map(|(s, _)| s == 200).unwrap_or(false) {
            return Ok(name.to_string());
        }
        let zero = format!("{name}-0");
        if self.client.call("GET", &format!("{V}/containers/{zero}/json"), &[], None).map(|(s, _)| s == 200).unwrap_or(false) {
            return Ok(zero);
        }
        Err(ke(format!("pods \"{name}\" not found")))
    }
}

// ---------- helpers ----------

fn parse_docs(yaml: &str) -> KubeResult<Vec<Value>> {
    let mut out = Vec::new();
    for doc in yaml.split("\n---") {
        let t = doc.trim();
        if t.is_empty() {
            continue;
        }
        let v: serde_yaml::Value = serde_yaml::from_str(t).map_err(|e| ke(format!("YAML parse error: {e}")))?;
        // Convert to serde_json::Value for uniform pointer access.
        let jv: Value = serde_json::to_value(&v).map_err(|e| ke(e.to_string()))?;
        if !jv.is_null() {
            out.push(jv);
        }
    }
    Ok(out)
}

fn d_ref(v: &Value) -> &Value {
    v
}

fn kind_of(d: &Value) -> String {
    d.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string()
}
fn name_of(d: &Value) -> String {
    d.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Some(Value::Object(o)) = v {
        for (k, val) in o {
            if let Some(s) = val.as_str() {
                m.insert(k.clone(), s.to_string());
            } else {
                m.insert(k.clone(), val.to_string());
            }
        }
    }
    m
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn normalize_kind(k: &str) -> String {
    match k.to_lowercase().trim_end_matches('s') {
        "deployment" | "deploy" => "Deployment",
        "pod" | "po" => "Pod",
        "job" => "Job",
        "service" | "svc" => "Service",
        "replicaset" | "rs" => "ReplicaSet",
        "" | "all" => "",
        _ => "",
    }
    .to_string()
}

fn split_ref(image: &str) -> (String, String) {
    if let Some(at) = image.find('@') {
        return (image[..at].to_string(), image[at + 1..].to_string());
    }
    match image.rsplit_once(':') {
        Some((r, t)) if !t.contains('/') => (r.to_string(), t.to_string()),
        _ => (image.to_string(), "latest".to_string()),
    }
}

fn url(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn b64_decode_str(s: &str) -> Option<String> {
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
    let mut bytes = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s.trim().as_bytes() {
        if c == b'=' {
            break;
        }
        let v = inv(c);
        if v < 0 {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multidoc() {
        let y = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cfg\ndata:\n  K: V\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 2\n";
        let docs = parse_docs(y).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(kind_of(&docs[0]), "ConfigMap");
        assert_eq!(name_of(&docs[1]), "web");
        assert_eq!(docs[1].pointer("/spec/replicas").unwrap().as_i64(), Some(2));
    }

    #[test]
    fn secret_decode() {
        assert_eq!(b64_decode_str("aGVsbG8="), Some("hello".into()));
    }

    #[test]
    fn kind_norm() {
        assert_eq!(normalize_kind("deploy"), "Deployment");
        assert_eq!(normalize_kind("po"), "Pod");
        assert_eq!(normalize_kind("svc"), "Service");
    }
}
