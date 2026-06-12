//! slimd — the slim container engine daemon.
//!
//! Serves the Docker Engine API subset on a unix socket (default
//! /var/run/docker.sock, matching what vessel-init expects so the host-side
//! vsock proxy and `docker` CLI work unchanged). Inside the nebula vessel it
//! is supervised by vessel-init exactly like dockerd was.

mod archive;
mod build;
mod container;
mod dns;
mod engine;
mod exec;
mod inspect;
mod kube_bridge;
mod names;
mod router;
mod streams;
mod volumes;

use engine::Engine;
use std::path::PathBuf;

fn main() {
    // Make panics visible even when the file log is tmpfs (lost on crash): print
    // location + message to stderr and flush. The default hook can be swallowed
    // when stderr is redirected/buffered; this guarantees the message lands in a
    // live session, which is how we'll catch the per-container density wall.
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        eprintln!("slimd PANIC [{thread}] at {loc}: {msg}");
        use std::io::Write;
        let _ = std::io::stderr().flush();
    }));

    // Raise RLIMIT_NOFILE to the hard cap before doing anything — running many
    // containers holds a few fds each, and the default 1024 soft limit otherwise
    // walls density at a few hundred and turns into hard accept/spawn failures.
    match slim_http::raise_open_file_limit() {
        Ok(n) => println!("slimd: open-file limit raised to {n}"),
        Err(e) => eprintln!("slimd: could not raise open-file limit: {e}"),
    }

    let socket =
        std::env::var("SLIM_SOCKET").unwrap_or_else(|_| "/var/run/docker.sock".to_string());
    let data = std::env::var("SLIM_DATA").unwrap_or_else(|_| "/var/lib/nebula/slim".to_string());

    let engine = match Engine::open(&PathBuf::from(&data)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("slimd: failed to open engine at {data}: {e}");
            std::process::exit(1);
        }
    };
    engine.boot();

    // Kubernetes apiserver-lite + controller bridge (Deployments → containers).
    // Disable with SLIM_KUBE_API=off; address via SLIM_KUBE_API_ADDR.
    if std::env::var("SLIM_KUBE_API").as_deref() != Ok("off") {
        let addr =
            std::env::var("SLIM_KUBE_API_ADDR").unwrap_or_else(|_| "0.0.0.0:6443".to_string());
        kube_bridge::start(&engine, &addr);
    }

    println!("slimd: listening on {socket} (data {data})");
    let engine_for_handler = engine.clone();
    let result = slim_http::serve(&PathBuf::from(&socket), move |ctx| {
        router::handle(&engine_for_handler, ctx);
    });
    if let Err(e) = result {
        eprintln!("slimd: serve error: {e}");
        std::process::exit(1);
    }
}
