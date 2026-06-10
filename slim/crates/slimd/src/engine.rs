//! The Engine: owns the image store, networks, volumes, container table, and
//! all lifecycle logic. The router (router.rs) is a thin translation layer
//! from HTTP to these methods.

use crate::container::*;
use crate::dns::DnsServer;
use crate::volumes::VolumeManager;
use slim_api::container::*;
use slim_image::registry::BasicAuth;
use slim_image::{ImageRecord, Store};
use slim_net::NetManager;
use slim_runtime::jsonlog::{rfc3339_now, LogWriter};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Paths {
    pub data: PathBuf,    // /var/lib/nebula/slim
    pub state: PathBuf,   // data/containers
    pub images: PathBuf,  // data/images
    pub volumes: PathBuf, // data/volumes
    pub run: PathBuf,     // /run/slim (transient overlay mounts)
}

pub struct Engine {
    pub paths: Paths,
    pub store: Store,
    pub net: NetManager,
    pub volumes: VolumeManager,
    pub dns: DnsServer,
    pub containers: Mutex<BTreeMap<String, Arc<Entry>>>,
    pub execs: Mutex<BTreeMap<String, Arc<ExecSession>>>,
    pub events: Mutex<Vec<std::sync::mpsc::Sender<slim_api::EventMessage>>>,
    #[allow(dead_code)]
    pub start_time: String,
}

pub type EngineRef = Arc<Engine>;

fn nf(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, msg.into())
}
fn conflict(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, msg.into())
}

impl Engine {
    pub fn open(data: &Path) -> io::Result<EngineRef> {
        let paths = Paths {
            data: data.to_path_buf(),
            state: data.join("containers"),
            images: data.join("images"),
            volumes: data.join("volumes"),
            run: PathBuf::from(std::env::var("SLIM_RUN_DIR").unwrap_or_else(|_| "/run/slim".into())),
        };
        for d in [&paths.state, &paths.images, &paths.volumes, &paths.run] {
            std::fs::create_dir_all(d)?;
        }
        let store = Store::open(&paths.images)?;
        let net = NetManager::new(&paths.data.join("net"))?;
        let volumes = VolumeManager::open(&paths.volumes)?;
        let engine = Arc::new(Engine {
            store,
            net,
            volumes,
            dns: DnsServer::new(),
            containers: Mutex::new(BTreeMap::new()),
            execs: Mutex::new(BTreeMap::new()),
            events: Mutex::new(Vec::new()),
            start_time: rfc3339_now(),
            paths,
        });
        Ok(engine)
    }

    pub fn boot(self: &EngineRef) {
        slim_runtime::become_subreaper();
        if let Err(e) = self.net.boot() {
            eprintln!("slimd: network boot warning: {e}");
        }
        // DNS on every known network gateway.
        for n in self.net.list() {
            self.dns.listen(&n.gateway());
        }
        self.load_containers();
        // Apply restart policies for containers that were running.
        self.restore_running();
    }

    // ---------- persistence ----------

    fn container_dir(&self, id: &str) -> PathBuf {
        self.paths.state.join(id)
    }

    fn persist(&self, c: &Container) {
        let dir = self.container_dir(&c.id);
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(b) = serde_json::to_vec_pretty(c) {
            let tmp = dir.join("config.json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join("config.json"));
            }
        }
    }

    fn load_containers(self: &EngineRef) {
        let Ok(rd) = std::fs::read_dir(&self.paths.state) else { return };
        for e in rd.flatten() {
            let cfg = e.path().join("config.json");
            let Ok(bytes) = std::fs::read(&cfg) else { continue };
            let Ok(mut c) = serde_json::from_slice::<Container>(&bytes) else { continue };
            // Anything not cleanly running is marked exited on load.
            if c.state.status == "running" {
                c.state.status = "exited".into();
                c.state.pid = 0;
            }
            let entry = Arc::new(Entry {
                base_dir: e.path(),
                c: Mutex::new(c.clone()),
                rt: Mutex::new(Arc::new(Runtime::default())),
                subscribers: Mutex::new(Vec::new()),
            });
            self.containers.lock().unwrap().insert(c.id.clone(), entry);
        }
    }

    fn restore_running(self: &EngineRef) {
        let ids: Vec<String> = self.containers.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let policy = {
                let entry = self.get_entry(&id).unwrap();
                let c = entry.c.lock().unwrap();
                c.host_config.restart_policy.name.clone()
            };
            if policy == "always" || policy == "unless-stopped" {
                let _ = self.start(&id);
            }
        }
    }

    // ---------- lookup ----------

    pub fn get_entry(&self, name_or_id: &str) -> io::Result<Arc<Entry>> {
        let map = self.containers.lock().unwrap();
        // exact id
        if let Some(e) = map.get(name_or_id) {
            return Ok(e.clone());
        }
        let want = name_or_id.trim_start_matches('/');
        let mut hit = None;
        for e in map.values() {
            let c = e.c.lock().unwrap();
            if c.name == want || c.id.starts_with(want) {
                if hit.is_some() {
                    return Err(conflict(format!("multiple containers match {name_or_id}")));
                }
                hit = Some(e.clone());
            }
        }
        hit.ok_or_else(|| nf(format!("No such container: {name_or_id}")))
    }

    fn name_taken(&self, name: &str) -> bool {
        self.containers
            .lock()
            .unwrap()
            .values()
            .any(|e| e.c.lock().unwrap().name == name)
    }

    // ---------- images (delegate to store, add auth/events) ----------

    pub fn pull(
        &self,
        reference: &str,
        auth: Option<BasicAuth>,
        emit: &mut dyn FnMut(slim_image::PullEvent),
    ) -> io::Result<ImageRecord> {
        let rec = self.store.pull(reference, auth, emit)?;
        self.emit_event("image", "pull", &rec.id, BTreeMap::new());
        Ok(rec)
    }

    /// Used by build's ensure-image hook.
    pub fn ensure_image(&self, reference: &str) -> io::Result<ImageRecord> {
        if let Some(r) = self.store.resolve(reference) {
            return Ok(r);
        }
        let mut sink = |_e: slim_image::PullEvent| {};
        self.store.pull(reference, None, &mut sink)
    }

    // ---------- create ----------

    pub fn create(&self, req: &ContainerCreateRequest, name: Option<&str>) -> io::Result<String> {
        let image = self
            .store
            .resolve(&req.config.image)
            .ok_or_else(|| nf(format!("No such image: {}", req.config.image)))?;

        let id = slim_net::rand_id();
        let name = match name {
            Some(n) if !n.is_empty() => n.trim_start_matches('/').to_string(),
            _ => crate::names::random_name(),
        };
        if self.name_taken(&name) {
            return Err(conflict(format!(
                "Conflict. The container name \"/{name}\" is already in use"
            )));
        }

        let argv = resolve_argv(&image.config, &req.config);
        if argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "No command specified",
            ));
        }
        let env = resolve_env(&image.config, &req.config);
        let working_dir = if !req.config.working_dir.is_empty() {
            req.config.working_dir.clone()
        } else {
            image.config.working_dir.clone()
        };
        let user = if !req.config.user.is_empty() {
            req.config.user.clone()
        } else {
            image.config.user.clone()
        };
        let hostname = if !req.config.hostname.is_empty() {
            req.config.hostname.clone()
        } else {
            id.chars().take(12).collect()
        };

        // Network choice.
        let network = match req.host_config.network_mode.as_str() {
            "" | "default" | "bridge" => slim_net::DEFAULT_NETWORK.to_string(),
            "none" => "none".to_string(),
            "host" => {
                eprintln!("slimd: host networking unsupported in slim; using bridge");
                slim_net::DEFAULT_NETWORK.to_string()
            }
            other if other.starts_with("container:") => {
                eprintln!("slimd: container: networking unsupported; using bridge");
                slim_net::DEFAULT_NETWORK.to_string()
            }
            other => other.to_string(),
        };

        let dir = self.container_dir(&id);
        std::fs::create_dir_all(&dir)?;
        let c = Container {
            id: id.clone(),
            name,
            image_ref: req.config.image.clone(),
            image_id: image.id.clone(),
            created: rfc3339_now(),
            config: req.config.clone(),
            host_config: req.host_config.clone(),
            argv,
            env,
            working_dir,
            user,
            hostname,
            network,
            ip: String::new(),
            mac: String::new(),
            aliases: req
                .networking_config
                .endpoints_config
                .values()
                .flat_map(|e| e.aliases.iter().cloned())
                .collect(),
            state: State { status: "created".into(), ..Default::default() },
            log_path: dir.join("container-json.log").to_string_lossy().into_owned(),
            rootfs_base: dir.join("rootfs").to_string_lossy().into_owned(),
        };
        self.persist(&c);
        let entry = Arc::new(Entry {
            base_dir: dir,
            c: Mutex::new(c.clone()),
            rt: Mutex::new(Arc::new(Runtime::default())),
            subscribers: Mutex::new(Vec::new()),
        });
        self.containers.lock().unwrap().insert(id.clone(), entry);
        self.emit_event("container", "create", &id, BTreeMap::new());
        Ok(id)
    }

    // ---------- start ----------

    pub fn start(self: &Arc<Self>, name_or_id: &str) -> io::Result<()> {
        let entry = self.get_entry(name_or_id)?;
        {
            let c = entry.c.lock().unwrap();
            if c.running() {
                return Ok(()); // idempotent (docker returns 304)
            }
        }
        self.start_entry(&entry)
    }

    fn start_entry(self: &Arc<Self>, entry: &Arc<Entry>) -> io::Result<()> {
        let mut c = entry.c.lock().unwrap();
        let image = self
            .store
            .resolve(&c.image_id)
            .ok_or_else(|| nf(format!("image {} missing", c.image_id)))?;

        // Prepare overlay rootfs.
        let base = PathBuf::from(&c.rootfs_base);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base)?;
        let merged = self.store.prepare_rootfs(&image, &base)?;

        // Resolve binds + mounts + volumes into runtime BindMounts.
        let binds = self.resolve_mounts(&c, &merged)?;

        // hosts + resolv: write into the merged rootfs so they survive as
        // bind targets (we bind our generated files over the image's).
        let etc = merged.join("etc");
        std::fs::create_dir_all(&etc)?;

        let spec = slim_runtime::ContainerSpec {
            id: c.id.clone(),
            rootfs: merged.clone(),
            argv: c.argv.clone(),
            env: c.env.clone(),
            cwd: c.working_dir.clone(),
            user: c.user.clone(),
            hostname: c.hostname.clone(),
            tty: c.config.tty,
            open_stdin: c.config.open_stdin,
            binds,
            tmpfs: c
                .host_config
                .tmpfs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            netns: None,
            readonly_rootfs: c.host_config.readonly_rootfs,
            shm_size: c.host_config.shm_size,
            memory: c.host_config.memory,
            memory_swap: c.host_config.memory_swap,
            nano_cpus: c.host_config.nano_cpus,
            cpu_shares: c.host_config.cpu_shares,
            pids_limit: c.host_config.pids_limit.unwrap_or(0),
        };

        let handle = slim_runtime::start_container(&spec).map_err(|e| {
            self.store.unmount_rootfs(&base);
            io::Error::other(format!("failed to start container: {e}"))
        })?;
        let pid = handle.pid;

        // Networking.
        if c.network != "none" {
            let aliases = self.aliases_for(&c);
            match self.net.connect(&c.network, &c.id, &c.name, pid, &aliases) {
                Ok(ep) => {
                    c.ip = ep.ip.clone();
                    c.mac = ep.mac.clone();
                    self.dns.set(&c.name, &ep.ip);
                    for a in &aliases {
                        self.dns.set(a, &ep.ip);
                    }
                    // Publish ports.
                    let ports = collect_ports(&c.host_config.port_bindings);
                    if let Err(e) = self.net.publish(&c.id, &ep.ip, &ports) {
                        eprintln!("slimd: port publish failed: {e}");
                    }
                    // Refresh /etc/hosts for everyone on this network.
                    self.refresh_network_hosts(&c.network);
                }
                Err(e) => eprintln!("slimd: network connect failed: {e}"),
            }
        }

        // Write hosts/resolv now that we know our IP (best-effort; the files
        // live in the overlay upper, visible to the running container).
        let gw = self
            .net
            .get(&c.network)
            .map(|n| n.gateway())
            .unwrap_or_default();
        let extra = self.net.hosts_entries(&c.network);
        let _ = std::fs::write(
            etc.join("hosts"),
            hosts_file(&c.hostname, &c.ip, &extra, &c.host_config.extra_hosts),
        );
        if c.network != "none" {
            let _ = std::fs::write(etc.join("resolv.conf"), resolv_conf(&gw, &c.host_config.dns));
        }

        // Output plumbing: pump pty/pipes → log + subscribers.
        let rt = Arc::new(make_runtime(handle));
        *entry.rt.lock().unwrap() = rt.clone();
        self.spawn_pump(entry, &rt, &c.log_path, c.config.tty);

        c.state = State {
            status: "running".into(),
            pid,
            started_at: rfc3339_now(),
            restart_count: c.state.restart_count,
            ..Default::default()
        };
        self.persist(&c);
        let cid = c.id.clone();
        drop(c);

        self.emit_event("container", "start", &cid, BTreeMap::new());
        self.spawn_waiter(entry.clone(), rt, base);
        Ok(())
    }

    fn aliases_for(&self, c: &Container) -> Vec<String> {
        // Short id + any endpoint aliases (--network-alias, compose/kube
        // service names) are all resolvable on the network.
        let mut aliases = vec![c.short_id()];
        aliases.extend(c.aliases.iter().cloned());
        aliases.retain(|a| !a.is_empty() && *a != c.name);
        aliases.dedup();
        aliases
    }

    fn resolve_mounts(
        &self,
        c: &Container,
        merged: &Path,
    ) -> io::Result<Vec<slim_runtime::BindMount>> {
        let mut out = Vec::new();
        // /etc/hosts and /etc/resolv.conf are written into the overlay upper
        // directly (above), so no bind needed.

        // -v / --volume binds: "src:dst[:ro]" or "name:dst[:ro]".
        for b in &c.host_config.binds {
            let parts: Vec<&str> = b.splitn(3, ':').collect();
            let (src, dst, ro) = match parts.as_slice() {
                [src, dst] => (src.to_string(), dst.to_string(), false),
                [src, dst, opts] => (src.to_string(), dst.to_string(), opts.contains("ro")),
                _ => continue,
            };
            let source = if src.starts_with('/') {
                PathBuf::from(&src)
            } else {
                // named volume
                self.volumes.ensure(&src)?
            };
            out.push(slim_runtime::BindMount { source, target: dst, read_only: ro });
        }
        // --mount specs.
        for m in &c.host_config.mounts {
            let source = match m.typ.as_str() {
                "bind" => PathBuf::from(&m.source),
                "volume" => self.volumes.ensure(&m.source)?,
                _ => continue,
            };
            out.push(slim_runtime::BindMount {
                source,
                target: m.target.clone(),
                read_only: m.read_only,
            });
        }
        // Anonymous volumes from image config (VOLUME) + request volumes.
        let _ = merged;
        Ok(out)
    }

    fn refresh_network_hosts(&self, network: &str) {
        // Rewrite /etc/hosts in every running member's overlay upper.
        // try_lock + skip: the container currently being started already holds
        // its own lock (and writes its own hosts file), so re-locking it here
        // would self-deadlock.
        let entries = self.net.hosts_entries(network);
        let map = self.containers.lock().unwrap();
        for e in map.values() {
            let Ok(c) = e.c.try_lock() else { continue };
            if c.network == network && c.running() {
                let etc = PathBuf::from(&c.rootfs_base).join("merged/etc");
                let _ = std::fs::write(
                    etc.join("hosts"),
                    hosts_file(&c.hostname, &c.ip, &entries, &c.host_config.extra_hosts),
                );
            }
        }
    }

    // ---------- output pump + waiter ----------

    fn spawn_pump(&self, entry: &Arc<Entry>, rt: &Arc<Runtime>, log_path: &str, tty: bool) {
        let mut writer = LogWriter::open(Path::new(log_path)).ok();
        if tty {
            // Single stream: read pty master, label as stdout.
            let Some(pty) = rt.pty.as_ref().map(|m| m.lock().unwrap().try_clone()) else { return };
            let Ok(mut pty) = pty else { return };
            let entry = entry.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match pty.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            if let Some(w) = writer.as_mut() {
                                let _ = w.write("stdout", &String::from_utf8_lossy(chunk));
                            }
                            fan_out(&entry, STREAM_STDOUT, chunk);
                        }
                    }
                }
            });
        } else {
            // Two streams.
            let writer = Arc::new(Mutex::new(writer));
            for (stream, file) in [
                (STREAM_STDOUT, rt.stdout_clone()),
                (STREAM_STDERR, rt.stderr_clone()),
            ] {
                let Some(mut file) = file else { continue };
                let entry = entry.clone();
                let writer = writer.clone();
                let sname = if stream == STREAM_STDOUT { "stdout" } else { "stderr" };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match file.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let chunk = &buf[..n];
                                if let Some(w) = writer.lock().unwrap().as_mut() {
                                    let _ = w.write(sname, &String::from_utf8_lossy(chunk));
                                }
                                fan_out(&entry, stream, chunk);
                            }
                        }
                    }
                });
            }
        }
    }

    fn spawn_waiter(self: &Arc<Self>, entry: Arc<Entry>, rt: Arc<Runtime>, base: PathBuf) {
        let engine = self.clone();
        std::thread::spawn(move || {
            let pid = { entry.c.lock().unwrap().state.pid };
            let status = slim_runtime::wait_pid(pid).unwrap_or(slim_runtime::ExitStatus {
                code: 255,
                oom_killed: false,
            });
            let oom = slim_runtime::read_oom(&entry.c.lock().unwrap().id);

            // Notify exit waiters.
            {
                let (lock, cv) = &*rt.exited;
                *lock.lock().unwrap() = Some(status.code);
                cv.notify_all();
            }
            // Give the output pump a moment to drain the final bytes to live
            // attachers before we close their streams.
            std::thread::sleep(std::time::Duration::from_millis(50));
            signal_eof(&entry);

            engine.store.unmount_rootfs(&base);
            let (id, policy, max_retry, auto_remove) = {
                let mut c = entry.c.lock().unwrap();
                c.state.status = "exited".into();
                c.state.pid = 0;
                c.state.exit_code = status.code;
                c.state.oom_killed = oom;
                c.state.finished_at = rfc3339_now();
                engine.persist(&c);
                (
                    c.id.clone(),
                    c.host_config.restart_policy.name.clone(),
                    c.host_config.restart_policy.maximum_retry_count,
                    c.host_config.auto_remove,
                )
            };
            engine.net.unpublish(&id);
            engine.net.disconnect_all(&id);
            engine.dns.remove_ip(&entry.c.lock().unwrap().ip.clone());

            let mut attrs = BTreeMap::new();
            attrs.insert("exitCode".to_string(), status.code.to_string());
            engine.emit_event("container", "die", &id, attrs);

            // Restart policy.
            let should_restart = match policy.as_str() {
                "always" | "unless-stopped" => true,
                "on-failure" => {
                    status.code != 0 && {
                        let c = entry.c.lock().unwrap();
                        max_retry == 0 || c.state.restart_count < max_retry
                    }
                }
                _ => false,
            };
            if should_restart {
                std::thread::sleep(std::time::Duration::from_millis(500));
                // Skip restart if it was stopped meanwhile.
                let still_exited = entry.c.lock().unwrap().state.status == "exited";
                if still_exited {
                    {
                        let mut c = entry.c.lock().unwrap();
                        c.state.restart_count += 1;
                    }
                    let _ = engine.start_entry(&entry);
                }
            } else if auto_remove {
                let _ = engine.remove(&id, true, false);
            }
        });
    }

    // ---------- stop / kill / restart / rm / wait ----------

    pub fn stop(self: &Arc<Self>, name_or_id: &str, timeout_secs: i64) -> io::Result<()> {
        let entry = self.get_entry(name_or_id)?;
        let (pid, signal, id) = {
            let c = entry.c.lock().unwrap();
            if !c.running() {
                return Ok(());
            }
            let sig = c
                .config
                .stop_signal
                .as_deref()
                .map(slim_runtime::parse_signal)
                .unwrap_or(libc::SIGTERM);
            (c.state.pid, sig, c.id.clone())
        };
        // Disable restart for an explicit stop.
        self.mark_stopping(&entry);
        let _ = slim_runtime::signal_pid(pid, signal);
        // Wait up to timeout, then SIGKILL.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(timeout_secs.max(0) as u64);
        while std::time::Instant::now() < deadline {
            if !entry.c.lock().unwrap().running() {
                self.emit_event("container", "stop", &id, BTreeMap::new());
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = slim_runtime::kill_cgroup(&id);
        let _ = slim_runtime::signal_pid(pid, libc::SIGKILL);
        // Wait for the waiter thread to mark it exited so a follow-up `rm`
        // (docker stop && docker rm) doesn't race a still-"running" state.
        let kill_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < kill_deadline {
            if !entry.c.lock().unwrap().running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.emit_event("container", "stop", &id, BTreeMap::new());
        Ok(())
    }

    pub fn kill(self: &Arc<Self>, name_or_id: &str, signal: &str) -> io::Result<()> {
        let entry = self.get_entry(name_or_id)?;
        let (pid, id) = {
            let c = entry.c.lock().unwrap();
            if !c.running() {
                return Err(conflict(format!(
                    "Container {} is not running",
                    c.short_id()
                )));
            }
            (c.state.pid, c.id.clone())
        };
        let sig = slim_runtime::parse_signal(signal);
        if sig == libc::SIGKILL {
            self.mark_stopping(&entry);
            let _ = slim_runtime::kill_cgroup(&id);
        }
        slim_runtime::signal_pid(pid, sig)?;
        let mut attrs = BTreeMap::new();
        attrs.insert("signal".to_string(), sig.to_string());
        self.emit_event("container", "kill", &id, attrs);
        Ok(())
    }

    /// Temporarily neutralize the restart policy (explicit stop/kill).
    fn mark_stopping(&self, entry: &Arc<Entry>) {
        let mut c = entry.c.lock().unwrap();
        if c.host_config.restart_policy.name == "always"
            || c.host_config.restart_policy.name == "on-failure"
            || c.host_config.restart_policy.name == "unless-stopped"
        {
            c.host_config.restart_policy.name = "no".into();
            // Not persisted: a fresh start re-reads the original from create
            // request only if we persist; keep the in-memory veto simple.
        }
    }

    pub fn restart(self: &Arc<Self>, name_or_id: &str, timeout: i64) -> io::Result<()> {
        let _ = self.stop(name_or_id, timeout);
        self.start(name_or_id)
    }

    pub fn wait(&self, name_or_id: &str) -> io::Result<i32> {
        let entry = self.get_entry(name_or_id)?;
        // Already exited?
        {
            let c = entry.c.lock().unwrap();
            if c.state.status == "exited" {
                return Ok(c.state.exit_code);
            }
        }
        let rt = entry.rt.lock().unwrap().clone();
        let (lock, cv) = &*rt.exited;
        let mut guard = lock.lock().unwrap();
        while guard.is_none() {
            // Re-check the persisted state too (covers races).
            if entry.c.lock().unwrap().state.status == "exited" {
                return Ok(entry.c.lock().unwrap().state.exit_code);
            }
            let (g, _timeout) = cv
                .wait_timeout(guard, std::time::Duration::from_millis(500))
                .unwrap();
            guard = g;
        }
        Ok(guard.unwrap_or(0))
    }

    pub fn remove(self: &Arc<Self>, name_or_id: &str, force: bool, remove_volumes: bool) -> io::Result<()> {
        let entry = self.get_entry(name_or_id)?;
        let id = {
            let c = entry.c.lock().unwrap();
            if c.running() {
                if !force {
                    return Err(conflict(format!(
                        "You cannot remove a running container {}. Stop the container before attempting removal or force remove",
                        c.short_id()
                    )));
                }
            }
            c.id.clone()
        };
        if entry.c.lock().unwrap().running() {
            let _ = self.kill(&id, "SIGKILL");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        self.net.disconnect_all(&id);
        self.net.unpublish(&id);
        let _ = remove_volumes; // anonymous-volume tracking: TODO (S4 follow-up)
        self.containers.lock().unwrap().remove(&id);
        let _ = std::fs::remove_dir_all(self.container_dir(&id));
        self.emit_event("container", "destroy", &id, BTreeMap::new());
        Ok(())
    }

    pub fn rename(&self, name_or_id: &str, new: &str) -> io::Result<()> {
        if self.name_taken(new) {
            return Err(conflict(format!("name /{new} already in use")));
        }
        let entry = self.get_entry(name_or_id)?;
        let mut c = entry.c.lock().unwrap();
        c.name = new.trim_start_matches('/').to_string();
        self.persist(&c);
        Ok(())
    }

    // ---------- list / inspect ----------

    pub fn list(&self, all: bool) -> Vec<Container> {
        let map = self.containers.lock().unwrap();
        let mut out: Vec<Container> = map
            .values()
            .map(|e| e.c.lock().unwrap().clone())
            .filter(|c| all || c.running())
            .collect();
        out.sort_by(|a, b| b.created.cmp(&a.created));
        out
    }

    // ---------- events ----------

    pub fn subscribe_events(&self) -> std::sync::mpsc::Receiver<slim_api::EventMessage> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.events.lock().unwrap().push(tx);
        rx
    }

    pub fn emit_event(&self, typ: &str, action: &str, id: &str, attrs: BTreeMap<String, String>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mut attributes = attrs;
        // docker always includes name/image attributes when available.
        if typ == "container" {
            if let Ok(e) = self.get_entry(id) {
                let c = e.c.lock().unwrap();
                attributes.entry("name".into()).or_insert_with(|| c.name.clone());
                attributes.entry("image".into()).or_insert_with(|| c.image_ref.clone());
            }
        }
        let msg = slim_api::EventMessage {
            typ: typ.to_string(),
            action: action.to_string(),
            actor: slim_api::EventActor { id: id.to_string(), attributes },
            time: now.as_secs() as i64,
            time_nano: now.as_nanos() as i64,
        };
        self.events.lock().unwrap().retain(|tx| tx.send(msg.clone()).is_ok());
    }
}

// ---------- Runtime helpers ----------

fn make_runtime(handle: slim_runtime::Handle) -> Runtime {
    Runtime {
        tty: handle.pty_master.is_some(),
        pty: handle.pty_master.map(Mutex::new),
        stdin: handle.stdin.map(Mutex::new),
        stdout: handle.stdout.map(Mutex::new),
        stderr: handle.stderr.map(Mutex::new),
        exited: Arc::new((Mutex::new(None), std::sync::Condvar::new())),
    }
}

impl Runtime {
    fn stdout_clone(&self) -> Option<std::fs::File> {
        self.stdout.as_ref().and_then(|m| m.lock().unwrap().try_clone().ok())
    }
    fn stderr_clone(&self) -> Option<std::fs::File> {
        self.stderr.as_ref().and_then(|m| m.lock().unwrap().try_clone().ok())
    }
}

fn fan_out(entry: &Arc<Entry>, stream: u8, chunk: &[u8]) {
    entry
        .subscribers
        .lock()
        .unwrap()
        .retain(|tx| tx.send((stream, chunk.to_vec())).is_ok());
}

fn signal_eof(entry: &Arc<Entry>) {
    entry.subscribers.lock().unwrap().clear();
}
