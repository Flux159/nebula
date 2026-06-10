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
mod names;
mod router;
mod streams;
mod volumes;

use engine::Engine;
use std::path::PathBuf;

fn main() {
    let socket = std::env::var("SLIM_SOCKET")
        .unwrap_or_else(|_| "/var/run/docker.sock".to_string());
    let data = std::env::var("SLIM_DATA")
        .unwrap_or_else(|_| "/var/lib/nebula/slim".to_string());

    let engine = match Engine::open(&PathBuf::from(&data)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("slimd: failed to open engine at {data}: {e}");
            std::process::exit(1);
        }
    };
    engine.boot();

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
