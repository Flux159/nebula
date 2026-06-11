//! slim-kube: a thin Kubernetes client for the slim apiserver-lite.
//!
//! Talks the *real* Kubernetes REST API over the kube unix socket that slimd
//! serves next to docker.sock (see slimd::kube_bridge). This is the one source
//! of truth: CRUD, discovery, CRDs, watch, scale, logs and exec all go through
//! the apiserver, which reconciles workloads into engine containers. Used by
//! the standalone `kubectl-slim` and `helm-slim` binaries.
//!
//! Earlier this crate mapped YAML straight onto docker containers; routing
//! through the apiserver means kubectl-slim now also sees CRDs/operators and a
//! consistent object view rather than a reconstructed-from-labels one.

pub mod model;
pub mod ws;

use model::KubeObject;
use serde_json::{json, Value};
use slim_client::http::{ApiError, Client};
use std::cell::RefCell;
use std::io::Write;

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

/// A discovered API resource (group/version/plural + scope), used to build
/// REST paths and resolve user-supplied kind/short names.
#[derive(Clone)]
struct Res {
    group: String,
    version: String,
    resource: String, // plural
    singular: String,
    kind: String,
    namespaced: bool,
    short_names: Vec<String>,
}

impl Res {
    fn gv_path(&self) -> String {
        if self.group.is_empty() {
            format!("/api/{}", self.version)
        } else {
            format!("/apis/{}/{}", self.group, self.version)
        }
    }
    fn collection_path(&self, ns: &str) -> String {
        if self.namespaced && !ns.is_empty() {
            format!("{}/namespaces/{}/{}", self.gv_path(), ns, self.resource)
        } else {
            format!("{}/{}", self.gv_path(), self.resource)
        }
    }
    fn object_path(&self, ns: &str, name: &str) -> String {
        format!("{}/{}", self.collection_path(ns), name)
    }
    fn display(&self) -> &str {
        if !self.singular.is_empty() {
            &self.singular
        } else {
            &self.kind
        }
    }
}

pub struct Facade<'a> {
    pub client: &'a Client,
    pub namespace: String,
    disco: RefCell<Option<Vec<Res>>>,
}

impl<'a> Facade<'a> {
    pub fn new(client: &'a Client, namespace: &str) -> Self {
        Facade {
            client,
            namespace: if namespace.is_empty() { "default".into() } else { namespace.into() },
            disco: RefCell::new(None),
        }
    }

    // ---------- discovery ----------

    fn discovery(&self) -> KubeResult<Vec<Res>> {
        if let Some(d) = self.disco.borrow().as_ref() {
            return Ok(d.clone());
        }
        let mut out = self.fetch_reslist("/api/v1").unwrap_or_default();
        if let Ok(groups) = self.client.json::<Value>("GET", "/apis", None) {
            for g in groups.get("groups").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                for gv in g.get("versions").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                    if let Some(gvs) = gv.get("groupVersion").and_then(|v| v.as_str()) {
                        if let Ok(rs) = self.fetch_reslist(&format!("/apis/{gvs}")) {
                            out.extend(rs);
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            return Err(ke("could not reach the slim apiserver (is the engine running?)"));
        }
        *self.disco.borrow_mut() = Some(out.clone());
        Ok(out)
    }

    fn fetch_reslist(&self, path: &str) -> KubeResult<Vec<Res>> {
        let v: Value = self.client.json("GET", path, None)?;
        let gv = v.get("groupVersion").and_then(|x| x.as_str()).unwrap_or("");
        let (group, version) = match gv.split_once('/') {
            Some((g, ver)) => (g.to_string(), ver.to_string()),
            None => (String::new(), gv.to_string()),
        };
        let mut out = Vec::new();
        for r in v.get("resources").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
            let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if name.is_empty() || name.contains('/') {
                continue; // skip subresources (pods/log, etc.)
            }
            out.push(Res {
                group: group.clone(),
                version: version.clone(),
                resource: name.to_string(),
                singular: r.get("singularName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                kind: r.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                namespaced: r.get("namespaced").and_then(|x| x.as_bool()).unwrap_or(true),
                short_names: r
                    .get("shortNames")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Resolve a user kind string (plural, singular, Kind, short name, or the
    /// `kind.group` form) to a discovered resource.
    fn resolve(&self, kind: &str) -> KubeResult<Res> {
        let k = kind.to_lowercase();
        let base = k.split('.').next().unwrap_or(&k);
        let d = self.discovery()?;
        for r in &d {
            if r.resource == base
                || r.singular == base
                || r.kind.to_lowercase() == base
                || r.short_names.iter().any(|s| s.to_lowercase() == base)
            {
                return Ok(r.clone());
            }
        }
        // tolerate a trailing plural 's' the discovery names don't carry
        let dep = base.strip_suffix('s').unwrap_or(base);
        for r in &d {
            if r.singular == dep || r.kind.to_lowercase() == dep {
                return Ok(r.clone());
            }
        }
        Err(ke(format!("the server doesn't have a resource type \"{kind}\"")))
    }

    fn obj_ns(&self, d: &Value, res: &Res) -> String {
        if !res.namespaced {
            return String::new();
        }
        d.pointer("/metadata/namespace")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| self.namespace.clone())
    }

    // ---------- apply ----------

    /// Apply a multi-doc manifest. CRDs are applied first so their types
    /// register before any custom resources reference them. Returns the kinds
    /// the apiserver doesn't know (so `--strict` can fail on them).
    pub fn apply_yaml(&self, yaml: &str, out: &mut dyn FnMut(&str)) -> KubeResult<Vec<String>> {
        let docs = parse_docs(yaml)?;
        let (mut crds, mut rest) = (Vec::new(), Vec::new());
        for d in docs {
            if kind_of(&d) == "CustomResourceDefinition" {
                crds.push(d);
            } else {
                rest.push(d);
            }
        }
        for d in &crds {
            self.apply_one(d, out)?;
        }
        if !crds.is_empty() {
            self.disco.borrow_mut().take(); // bust discovery cache so CRs resolve
        }
        let mut skipped = Vec::new();
        for d in &rest {
            match self.apply_one(d, out) {
                Ok(()) => {}
                Err(KubeError(m)) if m.starts_with("UNSUPPORTED:") => {
                    let kn = m.trim_start_matches("UNSUPPORTED:").to_string();
                    out(&format!("warning: {kn} — kind unknown to the slim apiserver, skipped\n"));
                    skipped.push(kn);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(skipped)
    }

    fn apply_one(&self, d: &Value, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        let kind = kind_of(d);
        if kind.is_empty() {
            return Ok(());
        }
        let name = name_of(d);
        if name.is_empty() {
            return Err(ke(format!("{kind}: metadata.name is required")));
        }
        let res = match self.resolve(&kind) {
            Ok(r) => r,
            Err(_) => return Err(KubeError(format!("UNSUPPORTED:{kind}/{name}"))),
        };
        let ns = self.obj_ns(d, &res);
        let obj_path = res.object_path(&ns, &name);
        let mut body = d.clone();
        let action = match self.client.json::<Value>("GET", &obj_path, None) {
            Ok(existing) => {
                // carry resourceVersion so the update isn't rejected as stale
                if let Some(rv) = existing.pointer("/metadata/resourceVersion").cloned() {
                    ensure_meta(&mut body).insert("resourceVersion".into(), rv);
                }
                self.client.json::<Value>("PUT", &obj_path, Some(&body))?;
                "configured"
            }
            Err(_) => {
                self.client.json::<Value>("POST", &res.collection_path(&ns), Some(&body))?;
                "created"
            }
        };
        out(&format!("{}/{} {}\n", res.display(), name, action));
        Ok(())
    }

    pub fn delete_yaml(&self, yaml: &str, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        for d in parse_docs(yaml)? {
            let kind = kind_of(&d);
            let name = name_of(&d);
            if kind.is_empty() || name.is_empty() {
                continue;
            }
            let Ok(res) = self.resolve(&kind) else { continue };
            let ns = self.obj_ns(&d, &res);
            match self.client.json::<Value>("DELETE", &res.object_path(&ns, &name), None) {
                Ok(_) => out(&format!("{}/{} deleted\n", res.display(), name)),
                Err(e) if e.status == 404 => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    // ---------- get / describe ----------

    pub fn get(&self, kind: &str, name: Option<&str>) -> KubeResult<Vec<KubeObject>> {
        let res = self.resolve(kind)?;
        let ns = if res.namespaced { self.namespace.clone() } else { String::new() };
        let mut rows = Vec::new();
        match name {
            Some(n) => {
                let obj: Value = self.client.json("GET", &res.object_path(&ns, n), None).map_err(|e| {
                    if e.status == 404 {
                        ke(format!("{} \"{n}\" not found", res.display()))
                    } else {
                        e.into()
                    }
                })?;
                rows.push(obj_to_kube(&res, &obj));
            }
            None => {
                let list: Value = self.client.json("GET", &res.collection_path(&ns), None)?;
                for item in list.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                    rows.push(obj_to_kube(&res, &item));
                }
            }
        }
        Ok(rows)
    }

    pub fn delete(&self, kind: &str, name: &str, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        let res = self.resolve(kind)?;
        let ns = if res.namespaced { self.namespace.clone() } else { String::new() };
        self.client.json::<Value>("DELETE", &res.object_path(&ns, name), None).map_err(|e| {
            if e.status == 404 {
                ke(format!("{} \"{name}\" not found", res.display()))
            } else {
                e.into()
            }
        })?;
        out(&format!("{}/{} deleted\n", res.display(), name));
        Ok(())
    }

    // ---------- scale ----------

    /// Scale a workload via the `/scale` subresource. `kind` defaults to
    /// deployments when empty (kubectl `scale deployment/x`).
    pub fn scale(&self, kind: &str, name: &str, replicas: i64, out: &mut dyn FnMut(&str)) -> KubeResult<()> {
        let res = self.resolve(if kind.is_empty() { "deployments" } else { kind })?;
        let ns = self.namespace.clone();
        let scale_path = format!("{}/scale", res.object_path(&ns, name));
        let body = json!({
            "apiVersion": "autoscaling/v1",
            "kind": "Scale",
            "metadata": {"name": name, "namespace": ns},
            "spec": {"replicas": replicas},
        });
        self.client.json::<Value>("PUT", &scale_path, Some(&body)).map_err(|e| {
            if e.status == 404 {
                ke(format!("{} \"{name}\" not found", res.display()))
            } else {
                e.into()
            }
        })?;
        out(&format!("{}/{} scaled\n", res.display(), name));
        Ok(())
    }

    // ---------- logs / exec ----------

    /// Resolve a pod-ish name to an existing pod (exact, or the first replica
    /// `<name>-0` of a workload).
    pub fn resolve_pod(&self, name: &str) -> KubeResult<String> {
        let ns = &self.namespace;
        let exists = |n: &str| {
            self.client
                .call("GET", &format!("/api/v1/namespaces/{ns}/pods/{n}"), &[], None)
                .map(|(s, _)| s == 200)
                .unwrap_or(false)
        };
        if exists(name) {
            return Ok(name.to_string());
        }
        let zero = format!("{name}-0");
        if exists(&zero) {
            return Ok(zero);
        }
        Err(ke(format!("pods \"{name}\" not found")))
    }

    pub fn logs(&self, pod: &str, container: &str, follow: bool, w: &mut dyn Write) -> KubeResult<()> {
        let ns = self.namespace.clone();
        let pod = self.resolve_pod(pod)?;
        let cq = if container.is_empty() { String::new() } else { format!("&container={container}") };
        let path = format!("/api/v1/namespaces/{ns}/pods/{pod}/log?follow={follow}&timestamps=false{cq}");
        let mut resp = self.client.request("GET", &path, &[], None)?;
        if resp.status == 404 {
            return Err(ke(format!("pods \"{pod}\" not found")));
        }
        resp.stream_body(|c| {
            let _ = w.write_all(c);
            let _ = w.flush();
        })?;
        Ok(())
    }

    /// Exec into a pod through the apiserver's WebSocket exec subresource.
    /// Returns the command's exit code.
    pub fn exec(&self, pod: &str, container: &str, cmd: &[String], interactive: bool, tty: bool) -> KubeResult<i32> {
        let ns = self.namespace.clone();
        let pod = self.resolve_pod(pod)?;
        ws::exec(&self.client.socket, &ns, &pod, container, cmd, tty, interactive).map_err(|e| ke(e.to_string()))
    }
}

// ---------- object → table row ----------

fn obj_to_kube(res: &Res, obj: &Value) -> KubeObject {
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or(&res.kind).to_string();
    let (ready, status, restarts) = summarize(res, obj);
    KubeObject { kind, name, ready, status, restarts, members: vec![] }
}

fn summarize(res: &Res, obj: &Value) -> (String, String, i64) {
    match res.resource.as_str() {
        "pods" => {
            let phase = obj.pointer("/status/phase").and_then(|v| v.as_str()).unwrap_or("Pending").to_string();
            // Prefer real containerStatuses (ready count + restarts); fall back
            // to phase for older/sparse status.
            match obj.pointer("/status/containerStatuses").and_then(|c| c.as_array()) {
                Some(arr) if !arr.is_empty() => {
                    let total = arr.len();
                    let ready = arr.iter().filter(|c| c.get("ready").and_then(|r| r.as_bool()).unwrap_or(false)).count();
                    let restarts = arr.iter().filter_map(|c| c.get("restartCount").and_then(|r| r.as_i64())).sum();
                    (format!("{ready}/{total}"), phase, restarts)
                }
                _ => {
                    let ready = if phase == "Running" { "1/1" } else { "0/1" }.to_string();
                    (ready, phase, 0)
                }
            }
        }
        "deployments" | "statefulsets" | "replicasets" => {
            let desired = obj.pointer("/spec/replicas").and_then(|v| v.as_i64()).unwrap_or(1);
            let avail = obj
                .pointer("/status/readyReplicas")
                .or_else(|| obj.pointer("/status/availableReplicas"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            (format!("{avail}/{desired}"), String::new(), 0)
        }
        _ => {
            let status = obj.pointer("/status/phase").and_then(|v| v.as_str()).unwrap_or("").to_string();
            (String::new(), status, 0)
        }
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
        let jv: Value = serde_json::to_value(&v).map_err(|e| ke(e.to_string()))?;
        if !jv.is_null() {
            out.push(jv);
        }
    }
    Ok(out)
}

fn kind_of(d: &Value) -> String {
    d.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string()
}
fn name_of(d: &Value) -> String {
    d.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Get (creating if absent) the object's `/metadata` map for in-place edits.
fn ensure_meta(obj: &mut Value) -> &mut serde_json::Map<String, Value> {
    let map = obj.as_object_mut().expect("manifest must be a JSON object");
    map.entry("metadata").or_insert_with(|| json!({}));
    map.get_mut("metadata").unwrap().as_object_mut().expect("metadata must be an object")
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
    fn res_paths() {
        let dep = Res {
            group: "apps".into(),
            version: "v1".into(),
            resource: "deployments".into(),
            singular: "deployment".into(),
            kind: "Deployment".into(),
            namespaced: true,
            short_names: vec!["deploy".into()],
        };
        assert_eq!(dep.collection_path("default"), "/apis/apps/v1/namespaces/default/deployments");
        assert_eq!(dep.object_path("default", "web"), "/apis/apps/v1/namespaces/default/deployments/web");
        let ns = Res {
            group: String::new(),
            version: "v1".into(),
            resource: "namespaces".into(),
            singular: "namespace".into(),
            kind: "Namespace".into(),
            namespaced: false,
            short_names: vec!["ns".into()],
        };
        assert_eq!(ns.collection_path(""), "/api/v1/namespaces");
        assert_eq!(ns.object_path("", "kube-system"), "/api/v1/namespaces/kube-system");
    }

    #[test]
    fn ensure_meta_creates() {
        let mut o = json!({"kind": "Pod"});
        ensure_meta(&mut o).insert("resourceVersion".into(), json!("7"));
        assert_eq!(o.pointer("/metadata/resourceVersion").unwrap().as_str(), Some("7"));
    }

    #[test]
    fn summarize_pod() {
        let res = Res {
            group: String::new(),
            version: "v1".into(),
            resource: "pods".into(),
            singular: "pod".into(),
            kind: "Pod".into(),
            namespaced: true,
            short_names: vec![],
        };
        let p = json!({"status": {"phase": "Running"}});
        let (ready, status, _) = summarize(&res, &p);
        assert_eq!(ready, "1/1");
        assert_eq!(status, "Running");
    }
}
