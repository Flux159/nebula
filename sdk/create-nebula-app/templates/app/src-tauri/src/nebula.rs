//! The app's Rust base layer to the Nebula engine: a small hyper client for
//! the v1alpha1 HTTP API. Tauri commands and components (model-config, …)
//! build on this instead of each rolling their own HTTP plumbing.
//!
//! Full API reference: docs/httpapi.md in https://github.com/Flux159/nebula.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::de::DeserializeOwned;

#[derive(Clone)]
pub struct Nebula {
    base: String,
    token: Option<String>,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>,
}

impl Nebula {
    /// `api_port` comes from nebula.config.json; the engine binds loopback.
    pub fn new(api_port: u16) -> Self {
        Self {
            base: format!("http://127.0.0.1:{api_port}"),
            token: std::env::var("NEBULA_API_TOKEN").ok(),
            client: Client::builder(TokioExecutor::new()).build_http(),
        }
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, String> {
        let mut req = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{}", self.base, path));
        if let Some(token) = &self.token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let req = match body {
            Some(v) => req
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(v.to_string())))
                .map_err(|e| e.to_string())?,
            None => req.body(Full::new(Bytes::new())).map_err(|e| e.to_string())?,
        };
        let res = self.client.request(req).await.map_err(|e| e.to_string())?;
        let status = res.status();
        let bytes = res
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        if !status.is_success() {
            let msg = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(msg);
        }
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        self.request("GET", path, None).await
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, String> {
        self.request("POST", path, Some(body)).await
    }
}
