//! slim-kubeapi: a passive Kubernetes apiserver-lite.
//!
//! Serves the real Kubernetes REST surface — discovery, CRUD, watch (with
//! resourceVersion + 410-on-stale), and dynamic CRD registration — backed by
//! an in-memory typeless store. It does NOT reconcile: posting a Deployment
//! stores it, it does not run containers (that's the kubectl-slim facade /
//! a future controller bridge). Its purpose is to let operators and `kubectl`
//! connect, list, and WATCH (incl. their CRDs) without crashlooping — the
//! thing the docker-facade can't do.
//!
//! See docs/slim-k8s-roadmap.md (Tier A, "passive generic apiserver").

pub mod proxy;
pub mod server;
pub mod store;
pub mod tls;

pub use proxy::{ExecHandle, LogOpts, PodProxy};
pub use server::ApiServer;
pub use store::{ResourceInfo, SharedStore, Store};
pub use tls::{generate as generate_tls, TlsIdentity};

/// Convenience: build a store and serve the API on `addr` (blocking).
pub fn serve(addr: &str) -> std::io::Result<()> {
    let store = Store::new();
    let api = ApiServer::new(store);
    api.serve(addr)
}
