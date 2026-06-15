//! slim-build: a Dockerfile executor over slim-image's layer store and
//! slim-runtime — no buildkit.
//!
//! Each instruction either mutates the in-progress image config or produces
//! one new layer (RUN via a throwaway container snapshot; COPY/ADD via a
//! staged upper dir). Layer caching keys each step on the chain of prior
//! steps + the instruction + (for COPY/ADD) a content hash of the inputs,
//! matching docker's cache mental model.

pub mod dockerfile;

use dockerfile::{Instruction, ShellOrExec};
use sha2::Digest;
use slim_api::image::{ImageConfig, OciImageConfig, OciRootFs};
use slim_image::{ImageRecord, Store};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct BuildError(pub String);
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for BuildError {}
fn be(s: impl Into<String>) -> BuildError {
    BuildError(s.into())
}
impl From<io::Error> for BuildError {
    fn from(e: io::Error) -> Self {
        BuildError(e.to_string())
    }
}

pub struct BuildOptions {
    pub dockerfile: String,     // relative path in context, default "Dockerfile"
    pub tag: Option<String>,    // -t
    pub target: Option<String>, // --target stage
    pub build_args: BTreeMap<String, String>,
    pub no_cache: bool,
    pub labels: BTreeMap<String, String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            dockerfile: "Dockerfile".into(),
            tag: None,
            target: None,
            build_args: BTreeMap::new(),
            no_cache: false,
            labels: BTreeMap::new(),
        }
    }
}

/// Progress callback: classic (non-buildkit) builder stream lines.
pub type Progress<'a> = dyn FnMut(&str) + 'a;

/// Pull callback — the host (slimd) owns registry auth, so build delegates
/// "ensure this base image is local" back to it.
pub type EnsureImage<'a> = dyn FnMut(&str, &mut Progress) -> Result<ImageRecord, BuildError> + 'a;

struct Stage {
    name: Option<String>,
    layers: Vec<String>, // diff_ids
    config: ImageConfig,
    arch: String,
    os: String,
}

struct Builder<'a> {
    store: &'a Store,
    ctx: PathBuf,
    ignore: Vec<String>,
    cache: BuildCache,
    no_cache: bool,
    global_args: BTreeMap<String, String>,
    finished_stages: Vec<Stage>, // by index, for COPY --from=N / name
}

pub fn build(
    store: &Store,
    context_dir: &Path,
    opts: &BuildOptions,
    ensure_image: &mut EnsureImage,
    progress: &mut Progress,
) -> Result<ImageRecord, BuildError> {
    let df_path = context_dir.join(&opts.dockerfile);
    let df_src = std::fs::read_to_string(&df_path)
        .map_err(|e| be(format!("cannot read {}: {e}", df_path.display())))?;
    let df = dockerfile::parse(&df_src).map_err(|e| be(e.to_string()))?;

    let ignore = read_dockerignore(context_dir);
    let cache = BuildCache::load(store);
    let mut b = Builder {
        store,
        ctx: context_dir.to_path_buf(),
        ignore,
        cache,
        no_cache: opts.no_cache,
        global_args: opts.build_args.clone(),
        finished_stages: Vec::new(),
    };

    // Split into stages on FROM.
    let mut stages: Vec<Vec<&Instruction>> = Vec::new();
    for inst in &df.instructions {
        if matches!(inst, Instruction::From { .. }) {
            stages.push(vec![inst]);
        } else if let Some(last) = stages.last_mut() {
            last.push(inst);
        }
    }

    let target_idx = match &opts.target {
        Some(t) => stages
            .iter()
            .position(|s| matches!(s[0], Instruction::From { stage: Some(n), .. } if n == t))
            .ok_or_else(|| be(format!("target stage {t} not found")))?,
        None => stages.len() - 1,
    };

    let total = stages[..=target_idx].iter().map(|s| s.len()).sum::<usize>();
    let mut step = 0;
    let mut last_stage: Option<Stage> = None;

    for (si, stage_insts) in stages.iter().enumerate() {
        if si > target_idx {
            break;
        }
        let mut stage = b.begin_stage(stage_insts[0], ensure_image, progress, &mut step, total)?;
        for inst in &stage_insts[1..] {
            step += 1;
            b.exec_instruction(&mut stage, inst, progress, step, total)?;
        }
        last_stage = Some(clone_stage(&stage));
        b.finished_stages.push(stage);
    }

    let stage = last_stage.ok_or_else(|| be("no stages built"))?;
    let mut config = stage.config.clone();
    for (k, v) in &opts.labels {
        config.labels.insert(k.clone(), v.clone());
    }
    let record = b.finalize(&stage, config, opts.tag.as_deref(), progress)?;
    b.cache.save(store);
    Ok(record)
}

fn clone_stage(s: &Stage) -> Stage {
    Stage {
        name: s.name.clone(),
        layers: s.layers.clone(),
        config: s.config.clone(),
        arch: s.arch.clone(),
        os: s.os.clone(),
    }
}

impl Builder<'_> {
    fn begin_stage(
        &mut self,
        from: &Instruction,
        ensure_image: &mut EnsureImage,
        progress: &mut Progress,
        step: &mut usize,
        total: usize,
    ) -> Result<Stage, BuildError> {
        *step += 1;
        let Instruction::From { image, stage, .. } = from else {
            return Err(be("stage did not start with FROM"));
        };
        let image = self.expand(image, &BTreeMap::new());
        progress(&format!("Step {step}/{total} : FROM {image}\n"));

        // FROM <previous stage name>?
        if let Some(prev) = self
            .finished_stages
            .iter()
            .find(|s| s.name.as_deref() == Some(image.as_str()))
        {
            let base = clone_stage(prev);
            progress(&format!(" ---> {}\n", short(&stage_id(&base))));
            return Ok(Stage {
                name: stage.clone(),
                ..base
            });
        }

        let record = if image == "scratch" {
            ImageRecord {
                architecture: self.store.arch.clone(),
                os: "linux".into(),
                ..Default::default()
            }
        } else if let Some(r) = self.store.resolve(&image) {
            r
        } else {
            ensure_image(&image, progress)?
        };
        progress(&format!(" ---> {}\n", short(&record.id)));
        Ok(Stage {
            name: stage.clone(),
            layers: record.diff_ids.clone(),
            config: record.config.clone(),
            arch: if record.architecture.is_empty() {
                self.store.arch.clone()
            } else {
                record.architecture.clone()
            },
            os: if record.os.is_empty() {
                "linux".into()
            } else {
                record.os.clone()
            },
        })
    }

    fn exec_instruction(
        &mut self,
        stage: &mut Stage,
        inst: &Instruction,
        progress: &mut Progress,
        step: usize,
        total: usize,
    ) -> Result<(), BuildError> {
        let desc = describe(inst);
        progress(&format!("Step {step}/{total} : {desc}\n"));

        // Cache lookup for layer-producing instructions.
        let parent = stage_id(stage);
        let content_hash = self.content_hash(inst, stage)?;
        let cache_key = sha_hex(format!("{parent}\n{desc}\n{content_hash}").as_bytes());

        if !self.no_cache {
            if let Some(c) = self.cache.get(&cache_key) {
                stage.layers = c.layers.clone();
                stage.config =
                    serde_json::from_str(&c.config).unwrap_or_else(|_| stage.config.clone());
                progress(" ---> Using cache\n");
                progress(&format!(" ---> {}\n", short(&stage_id(stage))));
                return Ok(());
            }
        }

        match inst {
            Instruction::Run(soe) => self.do_run(stage, soe)?,
            Instruction::Copy {
                from,
                chown,
                sources,
                dest,
            } => self.do_copy(stage, from.as_deref(), chown.as_deref(), sources, dest)?,
            Instruction::Add {
                chown,
                sources,
                dest,
            } => self.do_copy(stage, None, chown.as_deref(), sources, dest)?,
            Instruction::Env(kv) => {
                for (k, v) in kv {
                    let v = self.expand(v, &BTreeMap::new());
                    set_env(&mut stage.config.env, k, &v);
                }
            }
            Instruction::Arg { name, default } => {
                // Build args only affect expansion; make them visible to
                // subsequent expand() via global_args (predeclared form).
                if !self.global_args.contains_key(name) {
                    if let Some(d) = default {
                        self.global_args.insert(name.clone(), d.clone());
                    }
                }
            }
            Instruction::Label(kv) => {
                for (k, v) in kv {
                    stage
                        .config
                        .labels
                        .insert(k.clone(), self.expand(v, &BTreeMap::new()));
                }
            }
            Instruction::Workdir(w) => {
                let w = self.expand(w, &BTreeMap::new());
                stage.config.working_dir = if w.starts_with('/') {
                    w
                } else {
                    format!("{}/{}", stage.config.working_dir.trim_end_matches('/'), w)
                };
            }
            Instruction::User(u) => stage.config.user = self.expand(u, &BTreeMap::new()),
            Instruction::Cmd(soe) => stage.config.cmd = Some(to_argv(soe, &stage.config)),
            Instruction::Entrypoint(soe) => {
                stage.config.entrypoint = Some(to_argv(soe, &stage.config))
            }
            Instruction::Expose(ports) => {
                let m = stage.config.exposed_ports.get_or_insert_with(BTreeMap::new);
                for p in ports {
                    let key = if p.contains('/') {
                        p.clone()
                    } else {
                        format!("{p}/tcp")
                    };
                    m.insert(key, serde_json::json!({}));
                }
            }
            Instruction::Volume(vs) => {
                let m = stage.config.volumes.get_or_insert_with(BTreeMap::new);
                for v in vs {
                    m.insert(v.clone(), serde_json::json!({}));
                }
            }
            Instruction::StopSignal(s) => stage.config.stop_signal = Some(s.clone()),
            Instruction::Shell(_) => { /* affects RUN shell form; handled in to_argv via config? kept simple */
            }
            Instruction::Healthcheck(_) => { /* parsed, not enforced */ }
            Instruction::Unsupported { verb, .. } => {
                progress(&format!(
                    " ---> [warning] {verb} is not supported by slim and was skipped\n"
                ));
            }
            Instruction::From { .. } => unreachable!("FROM only begins a stage"),
        }

        // Record cache entry.
        let snapshot = CachedStep {
            layers: stage.layers.clone(),
            config: serde_json::to_string(&stage.config).unwrap_or_default(),
        };
        self.cache.put(cache_key, snapshot);
        progress(&format!(" ---> {}\n", short(&stage_id(stage))));
        Ok(())
    }

    fn do_run(&mut self, stage: &mut Stage, soe: &ShellOrExec) -> Result<(), BuildError> {
        let argv = to_argv(soe, &stage.config);
        let work = self.store.root.join(format!("build/run-{}", slim_net_id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        let rec = self.stage_record(stage);
        let merged = self
            .store
            .prepare_rootfs(&rec, &work)
            .map_err(|e| be(format!("overlay for RUN failed: {e}")))?;

        let spec = slim_runtime::ContainerSpec {
            id: format!("build-{}", slim_net_id()),
            rootfs: merged.clone(),
            argv,
            env: stage.config.env.clone(),
            cwd: if stage.config.working_dir.is_empty() {
                "/".into()
            } else {
                stage.config.working_dir.clone()
            },
            user: stage.config.user.clone(),
            hostname: "buildkit".into(),
            tty: false,
            open_stdin: false,
            ..Default::default()
        };
        let result = run_to_completion(&spec);
        self.store.unmount_rootfs(&work);

        let code = result?;
        if code != 0 {
            let _ = std::fs::remove_dir_all(&work);
            return Err(be(format!(
                "The command '{}' returned a non-zero code: {code}",
                match soe {
                    ShellOrExec::Shell(s) => s.clone(),
                    ShellOrExec::Exec(v) => v.join(" "),
                }
            )));
        }
        // Snapshot the upper dir as a new layer.
        let upper = work.join("upper");
        self.commit_layer(stage, &upper)?;
        let _ = std::fs::remove_dir_all(&work);
        Ok(())
    }

    fn do_copy(
        &mut self,
        stage: &mut Stage,
        from: Option<&str>,
        chown: Option<&str>,
        sources: &[String],
        dest: &str,
    ) -> Result<(), BuildError> {
        let work = self
            .store
            .root
            .join(format!("build/copy-{}", slim_net_id()));
        let upper = work.join("upper");
        std::fs::create_dir_all(&upper)?;

        // Source root: the build context, or a previous stage's merged fs.
        let (src_root, src_mounted_dir) = match from {
            None => (self.ctx.clone(), None),
            Some(f) => {
                let prev = self.stage_by_ref(f)?;
                let mdir = self
                    .store
                    .root
                    .join(format!("build/from-{}", slim_net_id()));
                std::fs::create_dir_all(&mdir)?;
                let rec = self.stage_record(&prev);
                let merged = self.store.prepare_rootfs(&rec, &mdir)?;
                (merged, Some(mdir))
            }
        };

        let dest_is_dir = dest.ends_with('/') || sources.len() > 1 || dest == ".";
        let dest_rel = dest.trim_start_matches('/');
        let dest_base = if stage.config.working_dir.is_empty() || dest.starts_with('/') {
            upper.join(dest_rel)
        } else {
            upper
                .join(stage.config.working_dir.trim_start_matches('/'))
                .join(dest_rel)
        };

        for src in sources {
            let matches = glob_in(&src_root, src, from.is_none().then_some(&self.ignore));
            if matches.is_empty() {
                cleanup_dir(&src_mounted_dir, self.store);
                return Err(be(format!("COPY failed: no source files match {src}")));
            }
            for m in matches {
                let target = if dest_is_dir {
                    dest_base.join(m.file_name().unwrap_or_default())
                } else {
                    dest_base.clone()
                };
                copy_tree(&m, &target)?;
            }
        }
        if let Some(c) = chown {
            apply_chown(&dest_base, c);
        }
        cleanup_dir(&src_mounted_dir, self.store);
        self.commit_layer(stage, &upper)?;
        let _ = std::fs::remove_dir_all(&work);
        Ok(())
    }

    /// Pack `upper` as a layer, store it by diff_id, append to the stage.
    fn commit_layer(&mut self, stage: &mut Stage, upper: &Path) -> Result<(), BuildError> {
        if !upper.exists() || dir_is_empty(upper) {
            return Ok(()); // no-op layer (e.g. RUN true)
        }
        let mut hasher = HashWriter::new();
        slim_image::unpack::pack_layer(upper, &mut hasher)?;
        let diff_id = format!("sha256:{}", hasher.finish_hex());
        let dest = self.store.layer_dir(&diff_id);
        if !dest.join(".complete").exists() {
            let _ = std::fs::remove_dir_all(&dest);
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            // The upper dir IS the unpacked layer (overlay on-disk convention
            // matches our layer store). Move it into place.
            match std::fs::rename(upper, &dest) {
                Ok(_) => {}
                Err(_) => {
                    copy_tree(upper, &dest)?;
                }
            }
            std::fs::write(dest.join(".complete"), "0")?;
        }
        stage.layers.push(diff_id);
        Ok(())
    }

    fn finalize(
        &self,
        stage: &Stage,
        config: ImageConfig,
        tag: Option<&str>,
        progress: &mut Progress,
    ) -> Result<ImageRecord, BuildError> {
        let oci = OciImageConfig {
            architecture: stage.arch.clone(),
            os: stage.os.clone(),
            config,
            rootfs: OciRootFs {
                typ: "layers".into(),
                diff_ids: stage.layers.clone(),
            },
            created: slim_runtime::jsonlog::rfc3339_now(),
            history: vec![],
        };
        let config_raw = serde_json::to_vec(&oci).map_err(|e| be(e.to_string()))?;
        let id = format!("sha256:{}", sha_hex(&config_raw));
        let record = ImageRecord {
            id: id.clone(),
            manifest_digest: String::new(),
            diff_ids: stage.layers.clone(),
            size: 0,
            created: oci.created.clone(),
            architecture: oci.architecture.clone(),
            os: oci.os.clone(),
            config: oci.config.clone(),
        };
        self.store.insert_local(&config_raw, record.clone(), tag)?;
        if let Some(t) = tag {
            progress(&format!("Successfully built {}\n", short(&id)));
            progress(&format!("Successfully tagged {t}\n"));
        } else {
            progress(&format!("Successfully built {}\n", short(&id)));
        }
        Ok(record)
    }

    // ---------- helpers ----------

    fn stage_record(&self, stage: &Stage) -> ImageRecord {
        ImageRecord {
            id: stage_id(stage),
            diff_ids: stage.layers.clone(),
            config: stage.config.clone(),
            architecture: stage.arch.clone(),
            os: stage.os.clone(),
            ..Default::default()
        }
    }

    fn stage_by_ref(&self, r: &str) -> Result<Stage, BuildError> {
        if let Ok(idx) = r.parse::<usize>() {
            return self
                .finished_stages
                .get(idx)
                .map(clone_stage)
                .ok_or_else(|| be(format!("COPY --from={r}: no such stage")));
        }
        if let Some(s) = self
            .finished_stages
            .iter()
            .find(|s| s.name.as_deref() == Some(r))
        {
            return Ok(clone_stage(s));
        }
        // --from=<external image>
        if let Some(rec) = self.store.resolve(r) {
            return Ok(Stage {
                name: None,
                layers: rec.diff_ids.clone(),
                config: rec.config.clone(),
                arch: rec.architecture.clone(),
                os: rec.os.clone(),
            });
        }
        Err(be(format!(
            "COPY --from={r}: stage or image not found locally"
        )))
    }

    /// Content hash for cache keying of COPY/ADD (RUN keys on its command
    /// only, matching docker — RUN cache is not invalidated by file changes).
    fn content_hash(&self, inst: &Instruction, _stage: &Stage) -> Result<String, BuildError> {
        let sources = match inst {
            Instruction::Copy {
                from: None,
                sources,
                ..
            }
            | Instruction::Add { sources, .. } => sources,
            _ => return Ok(String::new()),
        };
        let mut h = sha2::Sha256::new();
        for src in sources {
            for m in glob_in(&self.ctx, src, Some(&self.ignore)) {
                hash_tree(&m, &mut h);
            }
        }
        Ok(hex::encode(h.finalize()))
    }

    fn expand(&self, s: &str, locals: &BTreeMap<String, String>) -> String {
        expand_vars(s, &self.global_args, locals)
    }
}

// ---------- run a build container to completion ----------

fn run_to_completion(spec: &slim_runtime::ContainerSpec) -> Result<i32, BuildError> {
    let handle = slim_runtime::start_container(spec).map_err(|e| be(e.to_string()))?;
    // Drain stdout/stderr so the pipe never blocks the build (output is not
    // shown in classic builder unless RUN fails; we discard for now).
    let mut joins = Vec::new();
    for f in [handle.stdout, handle.stderr].into_iter().flatten() {
        joins.push(std::thread::spawn(move || {
            let mut f = f;
            let _ = io::copy(&mut f, &mut io::sink());
        }));
    }
    let status = slim_runtime::wait_pid(handle.pid).map_err(|e| be(e.to_string()))?;
    for j in joins {
        let _ = j.join();
    }
    slim_runtime::remove_cgroup(&spec.id);
    Ok(status.code)
}

// ---------- build cache ----------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CachedStep {
    layers: Vec<String>,
    config: String,
}

struct BuildCache {
    map: BTreeMap<String, CachedStep>,
}

impl BuildCache {
    fn load(store: &Store) -> Self {
        let p = store.root.join("build-cache.json");
        let map = std::fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { map }
    }
    fn get(&self, key: &str) -> Option<&CachedStep> {
        self.map.get(key)
    }
    fn put(&mut self, key: String, step: CachedStep) {
        self.map.insert(key, step);
    }
    fn save(&self, store: &Store) {
        let p = store.root.join("build-cache.json");
        if let Ok(b) = serde_json::to_vec(&self.map) {
            let _ = std::fs::write(p, b);
        }
    }
}

// ---------- misc helpers ----------

struct HashWriter(sha2::Sha256);
impl HashWriter {
    fn new() -> Self {
        Self(sha2::Sha256::new())
    }
    fn finish_hex(self) -> String {
        hex::encode(self.0.finalize())
    }
}
impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn stage_id(stage: &Stage) -> String {
    let mut h = sha2::Sha256::new();
    h.update(stage.layers.join(",").as_bytes());
    h.update(serde_json::to_vec(&stage.config).unwrap_or_default());
    format!("sha256:{}", hex::encode(h.finalize()))
}

fn sha_hex(b: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(b))
}

fn short(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

fn slim_net_id() -> String {
    // Random 16-hex id without a dep (build temp dirs only need uniqueness).
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_err()
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ ((std::process::id() as u64) << 40);
        buf = t.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn describe(inst: &Instruction) -> String {
    match inst {
        Instruction::Run(s) => format!("RUN {}", soe_str(s)),
        Instruction::Cmd(s) => format!("CMD {}", soe_str(s)),
        Instruction::Entrypoint(s) => format!("ENTRYPOINT {}", soe_str(s)),
        Instruction::Copy {
            sources,
            dest,
            from,
            ..
        } => {
            let f = from
                .as_ref()
                .map(|f| format!("--from={f} "))
                .unwrap_or_default();
            format!("COPY {f}{} {dest}", sources.join(" "))
        }
        Instruction::Add { sources, dest, .. } => format!("ADD {} {dest}", sources.join(" ")),
        Instruction::Env(kv) => format!(
            "ENV {}",
            kv.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Instruction::Arg { name, default } => match default {
            Some(d) => format!("ARG {name}={d}"),
            None => format!("ARG {name}"),
        },
        Instruction::Label(kv) => format!(
            "LABEL {}",
            kv.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Instruction::Expose(p) => format!("EXPOSE {}", p.join(" ")),
        Instruction::Workdir(w) => format!("WORKDIR {w}"),
        Instruction::User(u) => format!("USER {u}"),
        Instruction::Volume(v) => format!("VOLUME {}", v.join(" ")),
        Instruction::StopSignal(s) => format!("STOPSIGNAL {s}"),
        Instruction::Shell(s) => format!("SHELL {}", s.join(" ")),
        Instruction::Healthcheck(h) => format!("HEALTHCHECK {h}"),
        Instruction::From { image, .. } => format!("FROM {image}"),
        Instruction::Unsupported { verb, rest } => format!("{verb} {rest}"),
    }
}

fn soe_str(s: &ShellOrExec) -> String {
    match s {
        ShellOrExec::Shell(s) => s.clone(),
        ShellOrExec::Exec(v) => format!("{v:?}"),
    }
}

fn to_argv(soe: &ShellOrExec, _config: &ImageConfig) -> Vec<String> {
    match soe {
        ShellOrExec::Exec(v) => v.clone(),
        ShellOrExec::Shell(s) => vec!["/bin/sh".into(), "-c".into(), s.clone()],
    }
}

fn set_env(env: &mut Vec<String>, key: &str, val: &str) {
    let prefix = format!("{key}=");
    if let Some(e) = env.iter_mut().find(|e| e.starts_with(&prefix)) {
        *e = format!("{key}={val}");
    } else {
        env.push(format!("{key}={val}"));
    }
}

/// ${VAR}, $VAR, ${VAR:-default}, ${VAR:+alt} expansion over build args.
fn expand_vars(
    s: &str,
    args: &BTreeMap<String, String>,
    locals: &BTreeMap<String, String>,
) -> String {
    let lookup = |k: &str| {
        locals
            .get(k)
            .or_else(|| args.get(k))
            .cloned()
            .unwrap_or_default()
    };
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if b[i] == b'$' && i + 1 < b.len() {
            if b[i + 1] == b'{' {
                if let Some(end) = s[i + 2..].find('}') {
                    let expr = &s[i + 2..i + 2 + end];
                    out.push_str(&eval_brace(expr, &lookup));
                    i += 2 + end + 1;
                    continue;
                }
            } else {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && (b[j] == b'_' || b[j].is_ascii_alphanumeric()) {
                    j += 1;
                }
                if j > start {
                    out.push_str(&lookup(&s[start..j]));
                    i = j;
                    continue;
                }
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn eval_brace(expr: &str, lookup: &dyn Fn(&str) -> String) -> String {
    if let Some((name, default)) = expr.split_once(":-") {
        let v = lookup(name);
        if v.is_empty() {
            default.to_string()
        } else {
            v
        }
    } else if let Some((name, alt)) = expr.split_once(":+") {
        let v = lookup(name);
        if v.is_empty() {
            String::new()
        } else {
            alt.to_string()
        }
    } else {
        lookup(expr)
    }
}

fn read_dockerignore(ctx: &Path) -> Vec<String> {
    std::fs::read_to_string(ctx.join(".dockerignore"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn ignored(rel: &str, patterns: &[String]) -> bool {
    let mut ignored = false;
    for p in patterns {
        let (neg, pat) = match p.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, p.as_str()),
        };
        if glob_match(pat, rel) {
            ignored = !neg;
        }
    }
    ignored
}

/// Resolve a COPY source pattern against a root, honoring .dockerignore.
fn glob_in(root: &Path, pattern: &str, ignore: Option<&Vec<String>>) -> Vec<PathBuf> {
    let pat = pattern.trim_start_matches("./").trim_start_matches('/');
    let mut out = Vec::new();
    if !pat.contains('*') && !pat.contains('?') {
        let p = root.join(pat);
        if p.exists() && !ignore.map(|ig| ignored(pat, ig)).unwrap_or(false) {
            out.push(p);
        }
        return out;
    }
    // Shallow glob over the immediate directory of the pattern.
    let (dir, file_pat) = match pat.rsplit_once('/') {
        Some((d, f)) => (root.join(d), f.to_string()),
        None => (root.to_path_buf(), pat.to_string()),
    };
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if glob_match(&file_pat, &name) {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(&e.path())
                    .to_string_lossy()
                    .into_owned();
                if !ignore.map(|ig| ignored(&rel, ig)).unwrap_or(false) {
                    out.push(e.path());
                }
            }
        }
    }
    out.sort();
    out
}

/// Minimal glob: `*` (any run, no `/`), `?` (one char). Good enough for
/// COPY/.dockerignore common cases.
fn glob_match(pat: &str, name: &str) -> bool {
    fn m(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => m(&p[1..], n) || (!n.is_empty() && n[0] != b'/' && m(p, &n[1..])),
            b'?' => !n.is_empty() && n[0] != b'/' && m(&p[1..], &n[1..]),
            c => !n.is_empty() && n[0] == c && m(&p[1..], &n[1..]),
        }
    }
    m(pat.as_bytes(), name.as_bytes())
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else if meta.file_type().is_symlink() {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        let target = std::fs::read_link(src)?;
        let _ = std::fs::remove_file(dst);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dst)?;
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn hash_tree(p: &Path, h: &mut sha2::Sha256) {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return;
    };
    h.update(
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .as_bytes(),
    );
    if meta.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            hash_tree(&e.path(), h);
        }
    } else if let Ok(data) = std::fs::read(p) {
        h.update((data.len() as u64).to_le_bytes());
        h.update(&data);
    }
}

fn dir_is_empty(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut r| r.next().is_none())
        .unwrap_or(true)
}

fn cleanup_dir(dir: &Option<PathBuf>, store: &Store) {
    if let Some(d) = dir {
        store.unmount_rootfs(d);
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(target_os = "linux")]
fn apply_chown(path: &Path, spec: &str) {
    use std::os::unix::ffi::OsStrExt;
    let (uid, gid) = match spec.split_once(':') {
        Some((u, g)) => (u.parse().unwrap_or(0), g.parse().unwrap_or(0)),
        None => {
            let u = spec.parse().unwrap_or(0);
            (u, u)
        }
    };
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        unsafe { libc::chown(c.as_ptr(), uid, gid) };
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_chown(_path: &Path, _spec: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_and_expand() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(!glob_match("*.txt", "a.md"));
        assert!(glob_match("app?", "app1"));
        let mut args = BTreeMap::new();
        args.insert("VER".to_string(), "1.2".to_string());
        assert_eq!(expand_vars("v$VER", &args, &BTreeMap::new()), "v1.2");
        assert_eq!(
            expand_vars("${MISSING:-def}", &args, &BTreeMap::new()),
            "def"
        );
        assert_eq!(expand_vars("${VER:+yes}", &args, &BTreeMap::new()), "yes");
        assert_eq!(expand_vars("\\$VER", &args, &BTreeMap::new()), "$VER");
    }

    #[test]
    fn dockerignore_semantics() {
        let pats = vec!["*.log".to_string(), "!keep.log".to_string()];
        assert!(ignored("a.log", &pats));
        assert!(!ignored("keep.log", &pats));
        assert!(!ignored("a.txt", &pats));
    }
}
