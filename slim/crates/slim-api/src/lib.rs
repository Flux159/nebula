//! Docker Engine API types — the single source of truth shared by slimd (the
//! daemon) and slim-client (the CLIs), so the two can never drift on
//! serialization.
//!
//! Deliberately lenient: structs default missing fields and ignore unknown
//! ones, because real docker clients send far more than we implement. Maps
//! are BTreeMaps so serialized output is deterministic (golden-diff friendly).

pub mod container;
pub mod exec;
pub mod image;
pub mod network;
pub mod system;
pub mod volume;

use serde::{Deserialize, Serialize};

/// The API version we advertise. v1.43 ≈ docker 24.x.
pub const API_VERSION: &str = "1.43";
pub const MIN_API_VERSION: &str = "1.24";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub message: String,
}

impl ErrorResponse {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

/// `docker events` / `/events` message (lite).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventMessage {
    #[serde(rename = "Type")]
    pub typ: String, // container | image | network | volume
    #[serde(rename = "Action")]
    pub action: String,
    #[serde(rename = "Actor")]
    pub actor: EventActor,
    pub time: i64,
    #[serde(rename = "timeNano")]
    pub time_nano: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventActor {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Attributes", default)]
    pub attributes: std::collections::BTreeMap<String, String>,
}

/// Wire format for progress lines streamed during pull/build
/// (`application/json` lines).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProgressMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(rename = "progressDetail", skip_serializing_if = "Option::is_none")]
    pub progress_detail: Option<ProgressDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aux: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProgressDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
}
