//! In-memory object store with Kubernetes resourceVersion + watch semantics.
//!
//! Objects are stored as raw JSON (typeless) keyed by
//! (group, resource, namespace, name). A single global monotonic counter is
//! the resourceVersion (RV); every write bumps it and appends to a bounded
//! event log. Watchers replay the log from their requested RV (or get 410
//! Gone if it's older than the log floor) then stream live events. This is
//! the contract client-go informers depend on.

use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const EVENT_LOG_CAP: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gvr {
    pub group: String,    // "" for core
    pub resource: String, // plural, e.g. "pods", "deployments"
}

/// Registry entry describing one served resource type.
#[derive(Debug, Clone)]
pub struct ResourceInfo {
    pub group: String,
    pub version: String,
    pub resource: String, // plural
    pub singular: String,
    pub kind: String,
    pub namespaced: bool,
    pub short_names: Vec<String>,
}

impl ResourceInfo {
    pub fn group_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
    pub fn list_kind(&self) -> String {
        format!("{}List", self.kind)
    }
}

#[derive(Clone)]
pub struct WatchEvent {
    pub typ: &'static str, // ADDED | MODIFIED | DELETED
    pub rv: u64,
    pub gvr: Gvr,
    pub namespace: String,
    pub object: Value,
}

struct Watcher {
    gvr: Gvr,
    namespace: Option<String>, // None = all namespaces
    label_selector: Vec<(String, String)>,
    tx: std::sync::mpsc::Sender<WatchEvent>,
}

#[derive(Default)]
struct Inner {
    /// (group,resource) -> (namespace/name -> object)
    objects: BTreeMap<Gvr, BTreeMap<String, Value>>,
    registry: Vec<ResourceInfo>,
    log: VecDeque<WatchEvent>,
    watchers: Vec<Watcher>,
}

pub struct Store {
    rv: AtomicU64,
    inner: Mutex<Inner>,
}

pub type SharedStore = Arc<Store>;

fn key(ns: &str, name: &str) -> String {
    format!("{ns}/{name}")
}

impl Store {
    pub fn new() -> SharedStore {
        let s = Arc::new(Store {
            rv: AtomicU64::new(1),
            inner: Mutex::new(Inner::default()),
        });
        for r in builtin_resources() {
            s.register(r);
        }
        s
    }

    pub fn register(&self, info: ResourceInfo) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.registry.iter().any(|r| {
            r.group == info.group && r.resource == info.resource && r.version == info.version
        }) {
            inner
                .objects
                .entry(Gvr {
                    group: info.group.clone(),
                    resource: info.resource.clone(),
                })
                .or_default();
            inner.registry.push(info);
        }
    }

    pub fn registry(&self) -> Vec<ResourceInfo> {
        self.inner.lock().unwrap().registry.clone()
    }

    pub fn lookup(&self, group: &str, resource: &str) -> Option<ResourceInfo> {
        self.inner
            .lock()
            .unwrap()
            .registry
            .iter()
            .find(|r| r.group == group && r.resource == resource)
            .cloned()
    }

    fn next_rv(&self) -> u64 {
        self.rv.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current_rv(&self) -> u64 {
        self.rv.load(Ordering::SeqCst)
    }

    /// Create or update (apply-style): assigns metadata, bumps RV, emits event.
    pub fn put(
        &self,
        info: &ResourceInfo,
        mut obj: Value,
        ns: &str,
        name: &str,
        create: bool,
    ) -> Value {
        let rv = self.next_rv();
        let gvr = Gvr {
            group: info.group.clone(),
            resource: info.resource.clone(),
        };
        let k = key(ns, name);
        let mut inner = self.inner.lock().unwrap();

        let existed = inner.objects.get(&gvr).and_then(|m| m.get(&k)).cloned();
        // Managed metadata fields.
        let meta = obj.get_mut("metadata").and_then(|m| m.as_object_mut());
        if let Some(meta) = meta {
            meta.insert("name".into(), json!(name));
            if info.namespaced {
                meta.insert("namespace".into(), json!(ns));
            }
            meta.insert("resourceVersion".into(), json!(rv.to_string()));
            if let Some(prev) = &existed {
                // preserve uid + creationTimestamp on update
                if let Some(uid) = prev.pointer("/metadata/uid") {
                    meta.insert("uid".into(), uid.clone());
                }
                if let Some(ts) = prev.pointer("/metadata/creationTimestamp") {
                    meta.insert("creationTimestamp".into(), ts.clone());
                }
            } else {
                meta.insert("uid".into(), json!(gen_uid()));
                meta.insert("creationTimestamp".into(), json!(now_rfc3339()));
            }
        }
        if let Some(o) = obj.as_object_mut() {
            o.insert("apiVersion".into(), json!(info.group_version()));
            o.insert("kind".into(), json!(info.kind));
        }

        inner
            .objects
            .entry(gvr.clone())
            .or_default()
            .insert(k, obj.clone());
        let typ = if existed.is_some() && !create {
            "MODIFIED"
        } else {
            "ADDED"
        };
        let ev = WatchEvent {
            typ,
            rv,
            gvr,
            namespace: ns.to_string(),
            object: obj.clone(),
        };
        self.emit(&mut inner, ev);
        obj
    }

    pub fn get(&self, info: &ResourceInfo, ns: &str, name: &str) -> Option<Value> {
        let gvr = Gvr {
            group: info.group.clone(),
            resource: info.resource.clone(),
        };
        self.inner
            .lock()
            .unwrap()
            .objects
            .get(&gvr)
            .and_then(|m| m.get(&key(ns, name)))
            .cloned()
    }

    pub fn delete(&self, info: &ResourceInfo, ns: &str, name: &str) -> Option<Value> {
        let rv = self.next_rv();
        let gvr = Gvr {
            group: info.group.clone(),
            resource: info.resource.clone(),
        };
        let mut inner = self.inner.lock().unwrap();
        let removed = inner
            .objects
            .get_mut(&gvr)
            .and_then(|m| m.remove(&key(ns, name)));
        if let Some(mut obj) = removed.clone() {
            if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                meta.insert("resourceVersion".into(), json!(rv.to_string()));
            }
            let ev = WatchEvent {
                typ: "DELETED",
                rv,
                gvr,
                namespace: ns.to_string(),
                object: obj,
            };
            self.emit(&mut inner, ev);
        }
        removed
    }

    /// List objects of a type, optionally filtered by namespace + labels.
    /// Returns (items, collection resourceVersion).
    pub fn list(
        &self,
        info: &ResourceInfo,
        ns: Option<&str>,
        labels: &[(String, String)],
    ) -> (Vec<Value>, u64) {
        let gvr = Gvr {
            group: info.group.clone(),
            resource: info.resource.clone(),
        };
        let inner = self.inner.lock().unwrap();
        let rv = self.current_rv();
        let items = inner
            .objects
            .get(&gvr)
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| match ns {
                        Some(ns) => k.starts_with(&format!("{ns}/")),
                        None => true,
                    })
                    .map(|(_, v)| v.clone())
                    .filter(|v| label_match(v, labels))
                    .collect()
            })
            .unwrap_or_default();
        (items, rv)
    }

    /// Begin a watch. Returns historical events with rv > since (or Err(410)
    /// if since is older than the log floor) plus a live receiver.
    pub fn watch(
        &self,
        info: &ResourceInfo,
        ns: Option<&str>,
        labels: Vec<(String, String)>,
        since: u64,
    ) -> Result<(Vec<WatchEvent>, std::sync::mpsc::Receiver<WatchEvent>), Gone> {
        let gvr = Gvr {
            group: info.group.clone(),
            resource: info.resource.clone(),
        };
        let mut inner = self.inner.lock().unwrap();

        let floor = inner.log.front().map(|e| e.rv).unwrap_or(0);
        // since==0 means "from now" (no replay). Non-zero older than floor → 410.
        if since != 0 && floor != 0 && since < floor.saturating_sub(1) {
            return Err(Gone);
        }
        let backlog: Vec<WatchEvent> = inner
            .log
            .iter()
            .filter(|e| {
                e.rv > since
                    && e.gvr == gvr
                    && ns_match(ns, &e.namespace)
                    && label_match(&e.object, &labels)
            })
            .cloned()
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        inner.watchers.push(Watcher {
            gvr,
            namespace: ns.map(String::from),
            label_selector: labels,
            tx,
        });
        Ok((backlog, rx))
    }

    fn emit(&self, inner: &mut Inner, ev: WatchEvent) {
        inner.log.push_back(ev.clone());
        while inner.log.len() > EVENT_LOG_CAP {
            inner.log.pop_front();
        }
        inner.watchers.retain(|w| {
            if w.gvr == ev.gvr
                && ns_match(w.namespace.as_deref(), &ev.namespace)
                && label_match(&ev.object, &w.label_selector)
            {
                w.tx.send(ev.clone()).is_ok()
            } else {
                true
            }
        });
    }
}

#[derive(Debug)]
pub struct Gone;

fn ns_match(want: Option<&str>, got: &str) -> bool {
    match want {
        None => true,
        Some(ns) => ns == got,
    }
}

fn label_match(obj: &Value, selector: &[(String, String)]) -> bool {
    if selector.is_empty() {
        return true;
    }
    let labels = obj.pointer("/metadata/labels");
    selector.iter().all(|(k, v)| {
        labels
            .and_then(|l| l.get(k))
            .and_then(|x| x.as_str())
            .map(|x| x == v)
            .unwrap_or(false)
    })
}

fn gen_uid() -> String {
    // RFC4122-ish from /dev/urandom; not cryptographically meaningful here.
    let mut b = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The core + apps resources we serve out of the box. Custom resources are
/// added at runtime when a CRD is applied.
pub fn builtin_resources() -> Vec<ResourceInfo> {
    let core = |resource: &str, kind: &str, namespaced: bool, short: &[&str]| ResourceInfo {
        group: String::new(),
        version: "v1".into(),
        resource: resource.into(),
        singular: kind.to_lowercase(),
        kind: kind.into(),
        namespaced,
        short_names: short.iter().map(|s| s.to_string()).collect(),
    };
    let apps = |resource: &str, kind: &str, short: &[&str]| ResourceInfo {
        group: "apps".into(),
        version: "v1".into(),
        resource: resource.into(),
        singular: kind.to_lowercase(),
        kind: kind.into(),
        namespaced: true,
        short_names: short.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        core("pods", "Pod", true, &["po"]),
        core("services", "Service", true, &["svc"]),
        core("configmaps", "ConfigMap", true, &["cm"]),
        core("secrets", "Secret", true, &[]),
        core("namespaces", "Namespace", false, &["ns"]),
        core("nodes", "Node", false, &["no"]),
        core("serviceaccounts", "ServiceAccount", true, &["sa"]),
        core("events", "Event", true, &["ev"]),
        core("endpoints", "Endpoints", true, &["ep"]),
        core(
            "persistentvolumeclaims",
            "PersistentVolumeClaim",
            true,
            &["pvc"],
        ),
        apps("deployments", "Deployment", &["deploy"]),
        apps("replicasets", "ReplicaSet", &["rs"]),
        apps("statefulsets", "StatefulSet", &["sts"]),
        apps("daemonsets", "DaemonSet", &["ds"]),
        ResourceInfo {
            group: "batch".into(),
            version: "v1".into(),
            resource: "jobs".into(),
            singular: "job".into(),
            kind: "Job".into(),
            namespaced: true,
            short_names: vec![],
        },
        ResourceInfo {
            group: "apiextensions.k8s.io".into(),
            version: "v1".into(),
            resource: "customresourcedefinitions".into(),
            singular: "customresourcedefinition".into(),
            kind: "CustomResourceDefinition".into(),
            namespaced: false,
            short_names: vec!["crd".into(), "crds".into()],
        },
        ResourceInfo {
            group: "rbac.authorization.k8s.io".into(),
            version: "v1".into(),
            resource: "roles".into(),
            singular: "role".into(),
            kind: "Role".into(),
            namespaced: true,
            short_names: vec![],
        },
        ResourceInfo {
            group: "rbac.authorization.k8s.io".into(),
            version: "v1".into(),
            resource: "rolebindings".into(),
            singular: "rolebinding".into(),
            kind: "RoleBinding".into(),
            namespaced: true,
            short_names: vec![],
        },
        ResourceInfo {
            group: "rbac.authorization.k8s.io".into(),
            version: "v1".into(),
            resource: "clusterroles".into(),
            singular: "clusterrole".into(),
            kind: "ClusterRole".into(),
            namespaced: false,
            short_names: vec![],
        },
        ResourceInfo {
            group: "rbac.authorization.k8s.io".into(),
            version: "v1".into(),
            resource: "clusterrolebindings".into(),
            singular: "clusterrolebinding".into(),
            kind: "ClusterRoleBinding".into(),
            namespaced: false,
            short_names: vec![],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod_info() -> ResourceInfo {
        builtin_resources()
            .into_iter()
            .find(|r| r.resource == "pods")
            .unwrap()
    }

    #[test]
    fn put_get_list_delete() {
        let s = Store::new();
        let info = pod_info();
        let obj = json!({"metadata":{"labels":{"app":"web"}}, "spec":{"x":1}});
        let created = s.put(&info, obj, "default", "p1", true);
        assert_eq!(created["metadata"]["name"], json!("p1"));
        assert_eq!(created["metadata"]["namespace"], json!("default"));
        assert_eq!(created["kind"], json!("Pod"));
        assert!(created["metadata"]["uid"].as_str().is_some());
        assert!(created["metadata"]["resourceVersion"].as_str().is_some());

        let got = s.get(&info, "default", "p1").unwrap();
        assert_eq!(got["spec"]["x"], json!(1));

        let (items, _rv) = s.list(&info, Some("default"), &[]);
        assert_eq!(items.len(), 1);
        let (items, _) = s.list(&info, Some("default"), &[("app".into(), "web".into())]);
        assert_eq!(items.len(), 1);
        let (items, _) = s.list(&info, Some("default"), &[("app".into(), "nope".into())]);
        assert_eq!(items.len(), 0);

        s.delete(&info, "default", "p1");
        assert!(s.get(&info, "default", "p1").is_none());
    }

    #[test]
    fn watch_replays_and_streams() {
        let s = Store::new();
        let info = pod_info();
        // create one before watching
        s.put(&info, json!({"metadata":{}}), "default", "p0", true);
        let rv0 = 1; // watch from the beginning
        let (backlog, rx) = s.watch(&info, Some("default"), vec![], rv0).unwrap();
        assert!(backlog
            .iter()
            .any(|e| e.typ == "ADDED" && e.object["metadata"]["name"] == json!("p0")));
        // live event
        s.put(&info, json!({"metadata":{}}), "default", "p1", true);
        let ev = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(ev.typ, "ADDED");
        assert_eq!(ev.object["metadata"]["name"], json!("p1"));
    }

    #[test]
    fn watch_too_old_is_gone() {
        let s = Store::new();
        let info = pod_info();
        // overflow the log floor
        for i in 0..(EVENT_LOG_CAP + 10) {
            s.put(
                &info,
                json!({"metadata":{}}),
                "default",
                &format!("p{i}"),
                true,
            );
        }
        // requesting rv=1 (older than floor) → Gone
        assert!(s.watch(&info, None, vec![], 1).is_err());
    }
}
