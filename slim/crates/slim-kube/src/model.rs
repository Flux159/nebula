//! Parsed views of the k8s kinds the facade maps.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Service {
    pub name: String,
    pub svc_type: String, // ClusterIP | NodePort | LoadBalancer
    pub selector_app: String,
    pub ports: Vec<ServicePort>,
}

#[derive(Debug, Clone)]
pub struct ServicePort {
    pub port: u16,
    pub target_port: Option<u16>,
    pub node_port: Option<u16>,
}

impl Service {
    pub fn parse(d: &Value) -> Service {
        let name = d
            .pointer("/metadata/name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let svc_type = d
            .pointer("/spec/type")
            .and_then(|v| v.as_str())
            .unwrap_or("ClusterIP")
            .to_string();
        let selector_app = d
            .pointer("/spec/selector/app")
            .and_then(|v| v.as_str())
            .unwrap_or(&name)
            .to_string();
        let mut ports = Vec::new();
        if let Some(arr) = d.pointer("/spec/ports").and_then(|v| v.as_array()) {
            for p in arr {
                let port = p.get("port").and_then(num_u16);
                let Some(port) = port else { continue };
                ports.push(ServicePort {
                    port,
                    target_port: p.get("targetPort").and_then(num_u16),
                    node_port: p.get("nodePort").and_then(num_u16),
                });
            }
        }
        Service {
            name,
            svc_type,
            selector_app,
            ports,
        }
    }

    pub fn selects(&self, app: &str, workload_name: &str) -> bool {
        self.selector_app == app
            || self.selector_app == workload_name
            || self.name == workload_name
            || self.name == app
    }
}

fn num_u16(v: &Value) -> Option<u16> {
    v.as_i64()
        .map(|n| n as u16)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// The reconstructed-from-engine view of a k8s object for `get`.
#[derive(Debug, Clone)]
pub struct KubeObject {
    pub kind: String,
    pub name: String,
    pub ready: String,  // "1/1"
    pub status: String, // Running | Pending
    pub restarts: i64,
    pub members: Vec<String>, // engine container names
}
