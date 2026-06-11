//! The pod-sandbox "pause" process — the kubelet pause-container equivalent.
//!
//! It does nothing but hold the pod's namespaces open and block forever, so the
//! sandbox container that owns the pod's netns/IP stays alive for the pod's whole
//! life. A few KB, statically linked, with no shell/`sleep` dependency — so any
//! pod (including distroless/scratch apps) gets a working sandbox. Baked into the
//! slim rootfs and registered by slimd as the `nebula/pause` image.

fn main() {
    // Block on signals forever; SIGTERM/SIGKILL from the engine stop us. The
    // loop just shrugs off spurious EINTR wakeups.
    loop {
        unsafe {
            libc::pause();
        }
    }
}
