//! slim-image: registry pull, content-addressed layer store, overlayfs
//! rootfs preparation.
//!
//! Layout under <root> (normally /var/lib/nebula/slim/images):
//!   blobs/sha256/<hex>      raw blobs (manifests, configs, compressed layers
//!                           are NOT kept after unpack — only config+manifest)
//!   layers/<hex>/           unpacked layer dirs, keyed by diff_id (shared
//!                           across images)
//!   db.json                 images + repo:tag table

pub mod refs;
pub mod registry;
pub mod unpack;

use refs::Reference;
use registry::{BasicAuth, RegistryClient};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use slim_api::image::*;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Sha256Stream(sha2::Sha256);

impl Default for Sha256Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Stream {
    pub fn new() -> Self {
        Self(sha2::Sha256::new())
    }
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finish_hex(self) -> String {
        hex::encode(self.0.finalize())
    }
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(data).to_vec()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageRecord {
    /// Image ID = config digest ("sha256:<hex>").
    pub id: String,
    pub manifest_digest: String,
    pub diff_ids: Vec<String>,
    pub size: i64,
    pub created: String,
    pub architecture: String,
    pub os: String,
    pub config: ImageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Db {
    images: BTreeMap<String, ImageRecord>,
    /// repo ("alpine", "ghcr.io/x/y") → tag → image id.
    repos: BTreeMap<String, BTreeMap<String, String>>,
    /// repo → digest string for RepoDigests.
    #[serde(default)]
    digests: BTreeMap<String, BTreeMap<String, String>>,
}

pub struct Store {
    pub root: PathBuf,
    pub arch: String,
    db: Mutex<Db>,
}

#[derive(Debug, Clone)]
pub enum PullEvent {
    Status(String),
    LayerStatus { id: String, status: String, current: u64, total: i64 },
}

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl Store {
    pub fn open(root: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(root.join("blobs/sha256"))?;
        std::fs::create_dir_all(root.join("layers"))?;
        let db = match std::fs::read(root.join("db.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Db::default(),
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            a => a,
        }
        .to_string();
        Ok(Store { root: root.to_path_buf(), arch, db: Mutex::new(db) })
    }

    fn save_db(&self, db: &Db) {
        let tmp = self.root.join("db.json.tmp");
        if serde_json::to_vec_pretty(db).ok().and_then(|b| std::fs::write(&tmp, b).ok()).is_some()
        {
            let _ = std::fs::rename(&tmp, self.root.join("db.json"));
        }
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        self.root.join("blobs/sha256").join(digest.trim_start_matches("sha256:"))
    }

    pub fn layer_dir(&self, diff_id: &str) -> PathBuf {
        self.root.join("layers").join(diff_id.trim_start_matches("sha256:"))
    }

    // ---------- pull ----------

    pub fn pull(
        &self,
        reference: &str,
        auth: Option<BasicAuth>,
        emit: &mut dyn FnMut(PullEvent),
    ) -> io::Result<ImageRecord> {
        let r = Reference::parse(reference);
        let client = RegistryClient::for_reference(&r, auth);
        emit(PullEvent::Status(format!(
            "{}: Pulling from {}",
            if r.tag.is_empty() { &r.digest } else { &r.tag },
            r.repo
        )));
        let (manifest, manifest_digest, raw_manifest) =
            client.manifest(&r, &self.arch).map_err(other)?;

        // Config blob.
        let mut config_raw = Vec::new();
        client
            .fetch_blob(&r, &manifest.config.digest, &mut config_raw, |_| {})
            .map_err(other)?;
        let oci: OciImageConfig = serde_json::from_slice(&config_raw)
            .map_err(|e| other(format!("bad image config: {e}")))?;
        let image_id = manifest.config.digest.clone();

        // Layers.
        let mut total_size = 0i64;
        for (i, layer) in manifest.layers.iter().enumerate() {
            let short: String = layer.digest.trim_start_matches("sha256:").chars().take(12).collect();
            let want_diff = oci.rootfs.diff_ids.get(i).cloned().unwrap_or_default();
            if !want_diff.is_empty() && self.layer_dir(&want_diff).join(".complete").exists() {
                emit(PullEvent::LayerStatus {
                    id: short,
                    status: "Already exists".into(),
                    current: 0,
                    total: layer.size,
                });
                total_size += layer.size;
                continue;
            }
            emit(PullEvent::LayerStatus {
                id: short.clone(),
                status: "Pulling fs layer".into(),
                current: 0,
                total: layer.size,
            });

            // Stream: registry → digest check → gunzip → tar apply, no temp
            // file for the common path.
            let mt = &layer.media_type;
            if mt.contains("zstd") {
                return Err(other(format!(
                    "layer {short} uses zstd compression — not supported by slim yet (tasks/issues.md)"
                )));
            }
            let tmp = self.root.join(format!("layers/.tmp-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);

            // Two-phase: download blob to file (verifies digest), then unpack.
            // (Streaming both at once doubles failure complexity; pulls are
            // disk-bound anyway. Revisit if profiling says otherwise.)
            let blob_tmp = self.root.join("blobs/.partial");
            {
                let mut f = std::fs::File::create(&blob_tmp)?;
                let mut last = 0u64;
                client
                    .fetch_blob(&r, &layer.digest, &mut f, |cur| {
                        if cur - last > 1_000_000 {
                            last = cur;
                            emit(PullEvent::LayerStatus {
                                id: short.clone(),
                                status: "Downloading".into(),
                                current: cur,
                                total: layer.size,
                            });
                        }
                    })
                    .map_err(other)?;
            }
            emit(PullEvent::LayerStatus {
                id: short.clone(),
                status: "Extracting".into(),
                current: 0,
                total: layer.size,
            });
            let blob = std::fs::File::open(&blob_tmp)?;
            let reader: Box<dyn Read> = if mt.contains("gzip") {
                Box::new(flate2::read::GzDecoder::new(blob))
            } else {
                Box::new(blob)
            };
            let mut hashing = unpack::HashingReader::new(reader);
            unpack::apply_layer(&mut hashing, &tmp)?;
            let diff_id = hashing.finish()?;
            if !want_diff.is_empty() && diff_id != want_diff {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(other(format!(
                    "layer diff_id mismatch (want {want_diff}, got {diff_id})"
                )));
            }
            let final_dir = self.layer_dir(&diff_id);
            let _ = std::fs::remove_dir_all(&final_dir);
            std::fs::rename(&tmp, &final_dir)?;
            std::fs::write(final_dir.join(".complete"), layer.size.to_string())?;
            let _ = std::fs::remove_file(&blob_tmp);
            total_size += layer.size;
            emit(PullEvent::LayerStatus {
                id: short,
                status: "Pull complete".into(),
                current: layer.size as u64,
                total: layer.size,
            });
        }

        // Persist config + manifest blobs.
        std::fs::write(self.blob_path(&image_id), &config_raw)?;
        std::fs::write(self.blob_path(&manifest_digest), &raw_manifest)?;

        let record = ImageRecord {
            id: image_id.clone(),
            manifest_digest: manifest_digest.clone(),
            diff_ids: oci.rootfs.diff_ids.clone(),
            size: total_size,
            created: oci.created.clone(),
            architecture: oci.architecture.clone(),
            os: oci.os.clone(),
            config: oci.config.clone(),
        };
        {
            let mut db = self.db.lock().unwrap();
            db.images.insert(image_id.clone(), record.clone());
            let repo_key = r.familiar_repo();
            if !r.tag.is_empty() {
                db.repos.entry(repo_key.clone()).or_default().insert(r.tag.clone(), image_id.clone());
            }
            db.digests
                .entry(repo_key)
                .or_default()
                .insert(manifest_digest.clone(), image_id.clone());
            self.save_db(&db);
        }
        emit(PullEvent::Status(format!("Digest: {manifest_digest}")));
        emit(PullEvent::Status(format!(
            "Status: Downloaded newer image for {}",
            r.familiar()
        )));
        Ok(record)
    }

    // ---------- lookup / list / tag / remove ----------

    pub fn resolve(&self, name_or_id: &str) -> Option<ImageRecord> {
        let db = self.db.lock().unwrap();
        // repo:tag / repo@digest forms.
        let r = Reference::parse(name_or_id);
        let repo_key = r.familiar_repo();
        if !r.digest.is_empty() {
            if let Some(id) = db.digests.get(&repo_key).and_then(|m| m.get(&r.digest)) {
                return db.images.get(id).cloned();
            }
        }
        if let Some(id) = db.repos.get(&repo_key).and_then(|m| m.get(&r.tag)) {
            return db.images.get(id).cloned();
        }
        // ID forms: sha256:..., full hex, prefix.
        let want = name_or_id.trim_start_matches("sha256:");
        if !want.is_empty() {
            let mut hit = None;
            for (id, rec) in db.images.iter() {
                if id.trim_start_matches("sha256:").starts_with(want) {
                    if hit.is_some() {
                        return None; // ambiguous
                    }
                    hit = Some(rec.clone());
                }
            }
            return hit;
        }
        None
    }

    pub fn repo_tags(&self, image_id: &str) -> Vec<String> {
        let db = self.db.lock().unwrap();
        let mut tags = Vec::new();
        for (repo, m) in db.repos.iter() {
            for (tag, id) in m {
                if id == image_id {
                    tags.push(format!("{repo}:{tag}"));
                }
            }
        }
        tags
    }

    pub fn repo_digests(&self, image_id: &str) -> Vec<String> {
        let db = self.db.lock().unwrap();
        let mut out = Vec::new();
        for (repo, m) in db.digests.iter() {
            for (digest, id) in m {
                if id == image_id {
                    out.push(format!("{repo}@{digest}"));
                }
            }
        }
        out
    }

    pub fn list(&self) -> Vec<ImageRecord> {
        self.db.lock().unwrap().images.values().cloned().collect()
    }

    pub fn tag(&self, src: &str, target: &str) -> io::Result<()> {
        let rec = self
            .resolve(src)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("No such image: {src}")))?;
        let t = Reference::parse(target);
        let mut db = self.db.lock().unwrap();
        db.repos
            .entry(t.familiar_repo())
            .or_default()
            .insert(if t.tag.is_empty() { "latest".into() } else { t.tag }, rec.id);
        self.save_db(&db);
        Ok(())
    }

    /// Remove a tag or an image. Returns docker-style delete responses.
    /// `in_use` lets the caller veto deleting layer data still referenced by
    /// containers.
    pub fn remove(
        &self,
        name_or_id: &str,
        force: bool,
        in_use: &dyn Fn(&str) -> bool,
    ) -> io::Result<Vec<ImageDeleteResponse>> {
        let rec = self.resolve(name_or_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("No such image: {name_or_id}"))
        })?;
        let mut out = Vec::new();
        let mut db = self.db.lock().unwrap();

        let r = Reference::parse(name_or_id);
        let by_tag = db.repos.get(&r.familiar_repo()).and_then(|m| m.get(&r.tag)).is_some();
        let all_tags: usize = db
            .repos
            .values()
            .flat_map(|m| m.values())
            .filter(|id| **id == rec.id)
            .count();

        if by_tag && all_tags > 1 && !force {
            // Untag only.
            db.repos.get_mut(&r.familiar_repo()).unwrap().remove(&r.tag);
            self.save_db(&db);
            return Ok(vec![ImageDeleteResponse {
                untagged: Some(format!("{}:{}", r.familiar_repo(), r.tag)),
                deleted: None,
            }]);
        }
        if in_use(&rec.id) && !force {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "conflict: unable to remove repository reference \"{name_or_id}\" (must force) - container is using its referenced image"
                ),
            ));
        }
        // Drop all tags + digests, then the image and any unshared layers.
        for (repo, m) in db.repos.iter_mut() {
            m.retain(|tag, id| {
                if id == &rec.id {
                    out.push(ImageDeleteResponse {
                        untagged: Some(format!("{repo}:{tag}")),
                        deleted: None,
                    });
                    false
                } else {
                    true
                }
            });
        }
        for m in db.digests.values_mut() {
            m.retain(|_, id| id != &rec.id);
        }
        db.images.remove(&rec.id);
        let still_used: std::collections::BTreeSet<&String> =
            db.images.values().flat_map(|i| i.diff_ids.iter()).collect();
        for diff in &rec.diff_ids {
            if !still_used.contains(diff) {
                let _ = std::fs::remove_dir_all(self.layer_dir(diff));
            }
        }
        let _ = std::fs::remove_file(self.blob_path(&rec.id));
        out.push(ImageDeleteResponse { untagged: None, deleted: Some(rec.id.clone()) });
        self.save_db(&db);
        Ok(out)
    }

    /// Register an image built locally (docker build / commit).
    pub fn insert_local(
        &self,
        config_raw: &[u8],
        record: ImageRecord,
        tag_as: Option<&str>,
    ) -> io::Result<()> {
        std::fs::write(self.blob_path(&record.id), config_raw)?;
        let mut db = self.db.lock().unwrap();
        db.images.insert(record.id.clone(), record.clone());
        if let Some(t) = tag_as {
            let t = Reference::parse(t);
            db.repos
                .entry(t.familiar_repo())
                .or_default()
                .insert(if t.tag.is_empty() { "latest".into() } else { t.tag }, record.id.clone());
        }
        self.save_db(&db);
        Ok(())
    }

    // ---------- rootfs prepare (overlay) ----------

    /// Mount an overlay rootfs for a container at <dir>/merged.
    pub fn prepare_rootfs(&self, image: &ImageRecord, dir: &Path) -> io::Result<PathBuf> {
        let upper = dir.join("upper");
        let work = dir.join("work");
        let merged = dir.join("merged");
        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&merged)?;
        // overlay lowerdir: FIRST entry is the TOP layer; diff_ids are
        // bottom-first, so reverse.
        let lowers: Vec<String> = image
            .diff_ids
            .iter()
            .rev()
            .map(|d| self.layer_dir(d).to_string_lossy().into_owned())
            .collect();
        if lowers.is_empty() {
            return Err(other("image has no layers"));
        }
        mount_overlay(&lowers, &upper, &work, &merged)?;
        Ok(merged)
    }

    pub fn unmount_rootfs(&self, dir: &Path) {
        unmount(&dir.join("merged"));
    }
}

impl Reference {
    /// The repo key used in the db ("alpine", "ghcr.io/x/y").
    pub fn familiar_repo(&self) -> String {
        if self.registry == "docker.io" {
            self.repo.strip_prefix("library/").unwrap_or(&self.repo).to_string()
        } else {
            format!("{}/{}", self.registry, self.repo)
        }
    }
}

#[cfg(target_os = "linux")]
fn mount_overlay(lowers: &[String], upper: &Path, work: &Path, merged: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let data = format!(
        "lowerdir={},upperdir={},workdir={}",
        lowers.join(":"),
        upper.display(),
        work.display()
    );
    let src = c"overlay";
    let fstype = c"overlay";
    let target = std::ffi::CString::new(merged.as_os_str().as_bytes()).unwrap();
    let data_c = std::ffi::CString::new(data).unwrap();
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            data_c.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("overlay mount failed: {}", io::Error::last_os_error()),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn mount_overlay(_l: &[String], _u: &Path, _w: &Path, _m: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "overlay requires linux"))
}

#[cfg(target_os = "linux")]
pub fn unmount(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
}

#[cfg(not(target_os = "linux"))]
pub fn unmount(_path: &Path) {}
