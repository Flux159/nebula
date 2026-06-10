//! Network endpoints (subset).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkCreateRequest {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Driver")]
    pub driver: String, // "" | bridge
    #[serde(rename = "Internal")]
    pub internal: bool,
    #[serde(rename = "Labels", deserialize_with = "crate::container::null_to_default")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "IPAM", skip_serializing_if = "Option::is_none")]
    pub ipam: Option<Ipam>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Ipam {
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(rename = "Config", deserialize_with = "crate::container::null_to_default")]
    pub config: Vec<IpamConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IpamConfig {
    #[serde(rename = "Subnet")]
    pub subnet: String,
    #[serde(rename = "Gateway", skip_serializing_if = "String::is_empty")]
    pub gateway: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkCreateResponse {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Warning", default)]
    pub warning: String,
}

/// `GET /networks` element + `GET /networks/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkInspect {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Created")]
    pub created: String,
    #[serde(rename = "Scope")]
    pub scope: String, // "local"
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(rename = "Internal")]
    pub internal: bool,
    #[serde(rename = "IPAM")]
    pub ipam: Ipam,
    #[serde(rename = "Containers")]
    pub containers: BTreeMap<String, NetworkContainer>,
    #[serde(rename = "Labels")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkContainer {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "IPv4Address")]
    pub ipv4_address: String,
    #[serde(rename = "MacAddress")]
    pub mac_address: String,
}

/// `POST /networks/{id}/connect|disconnect`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkConnectRequest {
    #[serde(rename = "Container")]
    pub container: String,
    #[serde(rename = "EndpointConfig", skip_serializing_if = "Option::is_none")]
    pub endpoint_config: Option<crate::container::EndpointSettings>,
    #[serde(rename = "Force")]
    pub force: bool,
}
