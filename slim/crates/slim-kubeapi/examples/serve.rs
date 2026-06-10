// Standalone apiserver-lite for testing: slim_kubeapi::serve on a TCP port.
fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8443".into());
    eprintln!("slim-kubeapi serving on {addr}");
    slim_kubeapi::serve(&addr).unwrap();
}
