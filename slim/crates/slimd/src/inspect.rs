//! Builders that translate slimd's internal state into Engine API response
//! shapes (inspect/list/version/info/stats). Kept lenient: fields we don't
//! track are zero/empty rather than omitted, so clients reading them get a
//! value instead of a missing-key error.

use crate::container::{self, Container, Entry};
use crate::engine::Engine;
use slim_api::container::*;
use slim_api::system::*;
use slim_image::ImageRecord as StoreImage;
use std::collections::BTreeMap;
use std::sync::Arc;

pub fn version() -> VersionResponse {
    VersionResponse {
        version: "slim-0.1.0".into(),
        api_version: slim_api::API_VERSION.into(),
        min_api_version: slim_api::MIN_API_VERSION.into(),
        git_commit: "slim".into(),
        os: "linux".into(),
        arch: arch(),
        kernel_version: kernel_version(),
        build_time: "2026-06-10T00:00:00.000000000Z".into(),
        platform: PlatformName { name: "nebula-slim".into() },
        components: vec![ComponentVersion {
            name: "Engine".into(),
            version: "slim-0.1.0".into(),
        }],
    }
}

pub fn info(engine: &Engine) -> InfoResponse {
    let containers = engine.list(true);
    let running = containers.iter().filter(|c| c.running()).count() as i64;
    let total = containers.len() as i64;
    InfoResponse {
        id: "SLIM:NEBULA".into(),
        containers: total,
        containers_running: running,
        containers_paused: 0,
        containers_stopped: total - running,
        images: engine.store.list().len() as i64,
        driver: "overlay2".into(),
        driver_status: vec![],
        memory_limit: true,
        swap_limit: true,
        cpu_cfs_period: true,
        cpu_cfs_quota: true,
        ipv4_forwarding: true,
        oom_kill_disable: false,
        ncpu: num_cpus(),
        mem_total: mem_total(),
        docker_root_dir: engine.paths.data.to_string_lossy().into_owned(),
        name: hostname(),
        kernel_version: kernel_version(),
        operating_system: "Nebula Slim (microVM)".into(),
        os_type: "linux".into(),
        os_version: String::new(),
        architecture: arch_uname(),
        server_version: "slim-0.1.0".into(),
        default_runtime: "slim".into(),
        live_restore_enabled: true,
        warnings: vec![],
    }
}

pub fn summary(engine: &Engine, c: &Container) -> ContainerSummary {
    let mut networks = BTreeMap::new();
    networks.insert(
        c.network.clone(),
        EndpointSettings {
            network_id: engine.net.get(&c.network).map(|n| n.id).unwrap_or_default(),
            ip_address: c.ip.clone(),
            gateway: engine.net.get(&c.network).map(|n| n.gateway()).unwrap_or_default(),
            mac_address: c.mac.clone(),
            ..Default::default()
        },
    );
    ContainerSummary {
        id: c.id.clone(),
        names: vec![c.slash_name()],
        image: c.image_ref.clone(),
        image_id: c.image_id.clone(),
        command: c.argv.join(" "),
        created: parse_created(&c.created),
        ports: port_summaries(engine, c),
        labels: c.config.labels.clone(),
        state: c.state.status.clone(),
        status: container::status_string(&c.state),
        network_settings: SummaryNetworkSettings { networks },
        mounts: mount_points(c),
    }
}

pub fn container(engine: &Engine, c: &Container) -> ContainerInspect {
    let net = engine.net.get(&c.network);
    let mut networks = BTreeMap::new();
    networks.insert(
        c.network.clone(),
        EndpointSettings {
            network_id: net.as_ref().map(|n| n.id.clone()).unwrap_or_default(),
            ip_address: c.ip.clone(),
            gateway: net.as_ref().map(|n| n.gateway()).unwrap_or_default(),
            mac_address: c.mac.clone(),
            ..Default::default()
        },
    );
    let ports = port_map(engine, c);
    ContainerInspect {
        id: c.id.clone(),
        created: c.created.clone(),
        path: c.argv.first().cloned().unwrap_or_default(),
        args: c.argv.iter().skip(1).cloned().collect(),
        state: ContainerState {
            status: c.state.status.clone(),
            running: c.running(),
            paused: false,
            restarting: false,
            oom_killed: c.state.oom_killed,
            dead: c.state.status == "dead",
            pid: if c.running() { c.state.pid as i64 } else { 0 },
            exit_code: c.state.exit_code as i64,
            error: c.state.error.clone(),
            started_at: nonempty_time(&c.state.started_at),
            finished_at: nonempty_time(&c.state.finished_at),
        },
        image: c.image_id.clone(),
        name: c.slash_name(),
        restart_count: c.state.restart_count,
        driver: "overlay2".into(),
        platform: "linux".into(),
        host_config: c.host_config.clone(),
        config: c.config.clone(),
        network_settings: NetworkSettings {
            bridge: String::new(),
            sandbox_key: format!("/var/run/netns/{}", c.short_id()),
            ports,
            gateway: net.as_ref().map(|n| n.gateway()).unwrap_or_default(),
            ip_address: c.ip.clone(),
            ip_prefix_len: if c.ip.is_empty() { 0 } else { 24 },
            mac_address: c.mac.clone(),
            networks,
        },
        mounts: mount_points(c),
        log_path: c.log_path.clone(),
    }
}

fn port_summaries(_engine: &Engine, c: &Container) -> Vec<PortSummary> {
    let mut out = Vec::new();
    for (key, binds) in &c.host_config.port_bindings {
        let (port, proto) = split_port(key);
        for b in binds {
            out.push(PortSummary {
                ip: if b.host_ip.is_empty() { "0.0.0.0".into() } else { b.host_ip.clone() },
                private_port: port,
                public_port: b.host_port.parse().unwrap_or(port),
                typ: proto.clone(),
            });
        }
        if binds.is_empty() {
            out.push(PortSummary { ip: String::new(), private_port: port, public_port: 0, typ: proto });
        }
    }
    out
}

fn port_map(_engine: &Engine, c: &Container) -> BTreeMap<String, Option<Vec<PortBinding>>> {
    let mut m = BTreeMap::new();
    for (key, binds) in &c.host_config.port_bindings {
        let norm = if key.contains('/') { key.clone() } else { format!("{key}/tcp") };
        if binds.is_empty() {
            m.insert(norm, None);
        } else {
            let mapped = binds
                .iter()
                .map(|b| PortBinding {
                    host_ip: if b.host_ip.is_empty() { "0.0.0.0".into() } else { b.host_ip.clone() },
                    host_port: b.host_port.clone(),
                })
                .collect();
            m.insert(norm, Some(mapped));
        }
    }
    m
}

fn mount_points(c: &Container) -> Vec<MountPoint> {
    let mut out = Vec::new();
    for b in &c.host_config.binds {
        let parts: Vec<&str> = b.splitn(3, ':').collect();
        if parts.len() < 2 {
            continue;
        }
        let src = parts[0];
        let is_volume = !src.starts_with('/');
        out.push(MountPoint {
            typ: if is_volume { "volume".into() } else { "bind".into() },
            name: if is_volume { src.to_string() } else { String::new() },
            source: src.to_string(),
            destination: parts[1].to_string(),
            mode: parts.get(2).map(|s| s.to_string()).unwrap_or_default(),
            rw: !parts.get(2).map(|o| o.contains("ro")).unwrap_or(false),
        });
    }
    out
}

pub fn image_summary(engine: &Engine, i: &StoreImage) -> slim_api::image::ImageSummary {
    slim_api::image::ImageSummary {
        id: i.id.clone(),
        parent_id: String::new(),
        repo_tags: {
            let t = engine.store.repo_tags(&i.id);
            if t.is_empty() { vec!["<none>:<none>".into()] } else { t }
        },
        repo_digests: engine.store.repo_digests(&i.id),
        created: parse_created(&i.created),
        size: i.size,
        virtual_size: i.size,
        shared_size: -1,
        labels: i.config.labels.clone(),
        containers: -1,
    }
}

pub fn image_inspect(engine: &Engine, i: &StoreImage) -> slim_api::image::ImageInspect {
    slim_api::image::ImageInspect {
        id: i.id.clone(),
        repo_tags: engine.store.repo_tags(&i.id),
        repo_digests: engine.store.repo_digests(&i.id),
        created: i.created.clone(),
        architecture: i.architecture.clone(),
        os: i.os.clone(),
        size: i.size,
        config: i.config.clone(),
        root_fs: slim_api::image::RootFs {
            typ: "layers".into(),
            layers: i.diff_ids.clone(),
        },
    }
}

pub fn network(engine: &Engine, n: &slim_net::NetworkRecord) -> slim_api::network::NetworkInspect {
    let mut containers = BTreeMap::new();
    for (cid, ep) in &n.endpoints {
        containers.insert(
            cid.clone(),
            slim_api::network::NetworkContainer {
                name: ep.container_name.clone(),
                ipv4_address: format!("{}/24", ep.ip),
                mac_address: ep.mac.clone(),
            },
        );
    }
    let _ = engine;
    slim_api::network::NetworkInspect {
        name: n.name.clone(),
        id: n.id.clone(),
        created: n.created.clone(),
        scope: "local".into(),
        driver: "bridge".into(),
        internal: n.internal,
        ipam: slim_api::network::Ipam {
            driver: "default".into(),
            config: vec![slim_api::network::IpamConfig {
                subnet: n.subnet(),
                gateway: n.gateway(),
            }],
        },
        containers,
        labels: n.labels.clone(),
    }
}

pub fn stats(_engine: &Engine, entry: &Arc<Entry>) -> StatsResponse {
    let c = entry.c.lock().unwrap();
    let cg = if c.running() {
        slim_runtime::read_stats(&c.id, c.state.pid)
    } else {
        slim_runtime::CgroupStats::default()
    };
    let limit = if cg.memory_limit > 0 { cg.memory_limit } else { mem_total() as u64 };
    StatsResponse {
        read: slim_runtime::jsonlog::rfc3339_now(),
        preread: "0001-01-01T00:00:00Z".into(),
        pids_stats: PidsStats { current: cg.pids_current },
        cpu_stats: CpuStats {
            cpu_usage: CpuUsage { total_usage: cg.cpu_usage_usec * 1000 },
            system_cpu_usage: 0,
            online_cpus: num_cpus() as u32,
        },
        precpu_stats: CpuStats::default(),
        memory_stats: MemoryStats { usage: cg.memory_current, limit },
        name: c.slash_name(),
        id: c.id.clone(),
        networks: BTreeMap::new(),
    }
}

pub fn system_df(engine: &Engine) -> serde_json::Value {
    serde_json::json!({
        "LayersSize": 0,
        "Images": engine.store.list().iter().map(|i| serde_json::json!({
            "Id": i.id, "Size": i.size, "RepoTags": engine.store.repo_tags(&i.id)
        })).collect::<Vec<_>>(),
        "Containers": engine.list(true).len(),
        "Volumes": engine.volumes.list().len(),
        "BuildCache": [],
    })
}

/// Lenient client filter handling for `docker ps --filter`.
pub fn apply_container_filters(
    summaries: Vec<ContainerSummary>,
    filters: Option<&str>,
) -> Vec<ContainerSummary> {
    let Some(f) = filters else { return summaries };
    let Ok(map) = serde_json::from_str::<BTreeMap<String, Vec<String>>>(f) else {
        return summaries;
    };
    summaries
        .into_iter()
        .filter(|s| {
            for (key, vals) in &map {
                match key.as_str() {
                    "name" => {
                        if !vals.iter().any(|v| s.names.iter().any(|n| n.contains(v.trim_start_matches('/')))) {
                            return false;
                        }
                    }
                    "status" => {
                        if !vals.contains(&s.state) {
                            return false;
                        }
                    }
                    "id" => {
                        if !vals.iter().any(|v| s.id.starts_with(v)) {
                            return false;
                        }
                    }
                    "label" => {
                        for v in vals {
                            let (k, val) = v.split_once('=').unwrap_or((v.as_str(), ""));
                            match s.labels.get(k) {
                                Some(lv) if val.is_empty() || lv == val => {}
                                _ => return false,
                            }
                        }
                    }
                    _ => {} // unknown filter: ignore (lenient)
                }
            }
            true
        })
        .collect()
}

// ---------- host facts ----------

fn split_port(key: &str) -> (u16, String) {
    match key.split_once('/') {
        Some((p, pr)) => (p.parse().unwrap_or(0), pr.to_string()),
        None => (key.parse().unwrap_or(0), "tcp".to_string()),
    }
}

fn nonempty_time(s: &str) -> String {
    if s.is_empty() { "0001-01-01T00:00:00Z".into() } else { s.to_string() }
}

fn parse_created(rfc3339: &str) -> i64 {
    slim_runtime::jsonlog::parse_rfc3339(rfc3339).unwrap_or(0)
}

fn num_cpus() -> i64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n } else { 1 }
}

fn mem_total() -> i64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(kb) = line.strip_prefix("MemTotal:") {
                    if let Some(v) = kb.trim().strip_suffix(" kB").and_then(|n| n.trim().parse::<i64>().ok()) {
                        return v * 1024;
                    }
                }
            }
        }
    }
    0
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "nebula-slim".into())
}

fn kernel_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn arch() -> String {
    match std::env::consts::ARCH {
        "aarch64" => "arm64".into(),
        "x86_64" => "amd64".into(),
        a => a.into(),
    }
}

fn arch_uname() -> String {
    std::env::consts::ARCH.to_string()
}
