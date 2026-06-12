//! Volume endpoints.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VolumeCreateRequest {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(
        rename = "Labels",
        deserialize_with = "crate::container::null_to_default"
    )]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Volume {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Driver")]
    pub driver: String, // "local"
    #[serde(rename = "Mountpoint")]
    pub mountpoint: String,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "Labels")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "Scope")]
    pub scope: String, // "local"
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VolumeListResponse {
    #[serde(rename = "Volumes")]
    pub volumes: Vec<Volume>,
    #[serde(rename = "Warnings")]
    pub warnings: Vec<String>,
}
