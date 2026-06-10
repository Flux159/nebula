//! Image types: Engine API list/inspect + the OCI/Docker distribution types
//! the registry client and layer store share.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One element of `GET /images/json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImageSummary {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "ParentId")]
    pub parent_id: String,
    #[serde(rename = "RepoTags")]
    pub repo_tags: Vec<String>,
    #[serde(rename = "RepoDigests")]
    pub repo_digests: Vec<String>,
    #[serde(rename = "Created")]
    pub created: i64,
    #[serde(rename = "Size")]
    pub size: i64,
    #[serde(rename = "VirtualSize")]
    pub virtual_size: i64,
    #[serde(rename = "SharedSize")]
    pub shared_size: i64,
    #[serde(rename = "Labels", deserialize_with = "crate::container::null_to_default")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "Containers")]
    pub containers: i64,
}

/// `GET /images/{name}/json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImageInspect {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "RepoTags")]
    pub repo_tags: Vec<String>,
    #[serde(rename = "RepoDigests")]
    pub repo_digests: Vec<String>,
    #[serde(rename = "Created")]
    pub created: String,
    #[serde(rename = "Architecture")]
    pub architecture: String,
    #[serde(rename = "Os")]
    pub os: String,
    #[serde(rename = "Size")]
    pub size: i64,
    #[serde(rename = "Config")]
    pub config: ImageConfig,
    #[serde(rename = "RootFS")]
    pub root_fs: RootFs,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RootFs {
    #[serde(rename = "Type")]
    pub typ: String, // "layers"
    #[serde(rename = "Layers")]
    pub layers: Vec<String>, // diff ids
}

/// The runnable config inside an OCI image config blob (and what
/// `docker inspect` shows under .Config). Field names here are the OCI
/// JSON ones — identical casing to docker's.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImageConfig {
    #[serde(rename = "User")]
    pub user: String,
    #[serde(rename = "ExposedPorts", skip_serializing_if = "Option::is_none")]
    pub exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "Env", deserialize_with = "crate::container::null_to_default")]
    pub env: Vec<String>,
    #[serde(rename = "Entrypoint", skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Volumes", skip_serializing_if = "Option::is_none")]
    pub volumes: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: String,
    #[serde(rename = "Labels", deserialize_with = "crate::container::null_to_default")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "StopSignal", skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
}

/// Full OCI image config blob (application/vnd.oci.image.config.v1+json or
/// the docker v2 schema equivalent — same shape for our purposes).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OciImageConfig {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub config: ImageConfig,
    pub rootfs: OciRootFs,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub history: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OciRootFs {
    #[serde(rename = "type")]
    pub typ: String,
    pub diff_ids: Vec<String>,
}

// ---- distribution (manifest) types ----

pub const MT_MANIFEST_LIST_V2: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MT_MANIFEST_V2: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub variant: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ManifestIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub manifests: Vec<Descriptor>,
}

/// `POST /images/{name}/tag` & deletes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageDeleteResponse {
    #[serde(rename = "Untagged", skip_serializing_if = "Option::is_none")]
    pub untagged: Option<String>,
    #[serde(rename = "Deleted", skip_serializing_if = "Option::is_none")]
    pub deleted: Option<String>,
}
