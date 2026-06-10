//! docker-slim — a standalone, docker-CLI-compatible client for the nebula
//! slim engine. Nebula's `nebula docker` wrapper execs this binary when the
//! active engine is slim; users with the real docker CLI can keep using it.
//!
//! HOST binary (macOS/Linux host triples), not the guest musl target.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(slim_client::run(&argv));
}
