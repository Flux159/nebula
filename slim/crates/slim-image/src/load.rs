//! `docker load`: import a `docker save` / OCI-layout tar into the store.
//!
//! Both layouts are accepted:
//!   * classic docker save — `manifest.json` naming a config json and a list
//!     of uncompressed layer tars (`<hex>/layer.tar` or `blobs/sha256/<hex>`)
//!   * OCI layout — `index.json` → manifest blob → config + layer blobs
//!     (gzipped or not; the magic bytes decide, not the media type)
//!
//! Tars are read twice: once to index every entry's offset (the manifest is
//! written LAST by docker save, so a single streaming pass cannot work), then
//! once per layer by seeking to its offset. That keeps peak memory flat no
//! matter how big the archive is — an app shipping a 135 MB images.tar.gz is
//! the case this exists for.

use crate::{other, unpack, ImageRecord, Store};
use serde::Deserialize;
use slim_api::image::{Manifest, ManifestIndex, OciImageConfig};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One image registered by a load, in the order the archive listed them.
#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub id: String,
    pub repo_tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct SaveManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Where each tar member lives, so layers can be re-read by seeking.
struct TarIndex {
    /// normalized path → (byte offset of the file data, size)
    at: BTreeMap<String, (u64, u64)>,
    /// Small members (json manifests/configs) kept in memory.
    small: BTreeMap<String, Vec<u8>>,
}

const SMALL_MAX: u64 = 1 << 20;

fn norm(p: &str) -> String {
    p.trim_start_matches("./").trim_end_matches('/').to_string()
}

fn index_tar(path: &Path) -> io::Result<TarIndex> {
    let mut idx = TarIndex {
        at: BTreeMap::new(),
        small: BTreeMap::new(),
    };
    let mut ar = tar::Archive::new(File::open(path)?);
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let name = norm(&entry.path()?.to_string_lossy());
        let size = entry.size();
        idx.at
            .insert(name.clone(), (entry.raw_file_position(), size));
        if size <= SMALL_MAX {
            let mut buf = Vec::with_capacity(size as usize);
            entry.read_to_end(&mut buf)?;
            idx.small.insert(name, buf);
        }
    }
    Ok(idx)
}

impl TarIndex {
    fn read(&self, tar_path: &Path, member: &str) -> io::Result<Vec<u8>> {
        let member = norm(member);
        if let Some(b) = self.small.get(&member) {
            return Ok(b.clone());
        }
        let (off, size) = *self
            .at
            .get(&member)
            .ok_or_else(|| other(format!("archive is missing {member}")))?;
        let mut f = File::open(tar_path)?;
        f.seek(SeekFrom::Start(off))?;
        let mut buf = Vec::with_capacity(size as usize);
        f.take(size).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn size_of(&self, member: &str) -> io::Result<u64> {
        let member = norm(member);
        self.at
            .get(&member)
            .map(|(_, size)| *size)
            .ok_or_else(|| other(format!("archive is missing {member}")))
    }

    fn open(&self, tar_path: &Path, member: &str) -> io::Result<(io::Take<File>, u64)> {
        let member = norm(member);
        let (off, size) = *self
            .at
            .get(&member)
            .ok_or_else(|| other(format!("archive is missing {member}")))?;
        let mut f = File::open(tar_path)?;
        f.seek(SeekFrom::Start(off))?;
        Ok((f.take(size), size))
    }
}

/// `1f 8b` — gzip. docker save writes plain tars but its OCI blobs (and the
/// tarballs apps ship) are routinely gzipped, so sniff rather than trust names.
fn is_gzip(head: &[u8]) -> bool {
    head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b
}

/// Decompress `src` into `dst` when it is gzipped; returns the path to use.
fn maybe_gunzip(src: &Path, dst: &Path) -> io::Result<PathBuf> {
    let mut head = [0u8; 6];
    let n = File::open(src)?.read(&mut head)?;
    if !is_gzip(&head[..n]) {
        // xz/bzip2 archives are a docker CLI convenience we don't implement;
        // say so instead of failing later with "not a tar".
        if n >= 6 && (&head[..6] == b"\xfd7zXZ\x00" || &head[..3] == b"BZh") {
            return Err(other(
                "compressed archive: only gzip is supported — pipe it through your own decompressor",
            ));
        }
        return Ok(src.to_path_buf());
    }
    let mut gz = flate2::read::GzDecoder::new(File::open(src)?);
    let mut out = File::create(dst)?;
    io::copy(&mut gz, &mut out)?;
    Ok(dst.to_path_buf())
}

impl Store {
    /// Import every image in a docker-save/OCI tar at `tar_path`.
    /// `emit` receives human-readable progress lines.
    pub fn load_tar(
        &self,
        tar_path: &Path,
        emit: &mut dyn FnMut(String),
    ) -> io::Result<Vec<LoadedImage>> {
        let gunzipped = self
            .root
            .join(format!("blobs/.load-{}.tar", std::process::id()));
        let path = maybe_gunzip(tar_path, &gunzipped)?;
        let result = self.load_plain_tar(&path, emit);
        let _ = std::fs::remove_file(&gunzipped);
        result
    }

    fn load_plain_tar(
        &self,
        path: &Path,
        emit: &mut dyn FnMut(String),
    ) -> io::Result<Vec<LoadedImage>> {
        let idx = index_tar(path)?;
        let entries = if idx.small.contains_key("manifest.json") {
            let raw = idx.read(path, "manifest.json")?;
            serde_json::from_slice::<Vec<SaveManifestEntry>>(&raw)
                .map_err(|e| other(format!("bad manifest.json: {e}")))?
        } else if idx.at.contains_key("index.json") {
            self.entries_from_oci_index(path, &idx)?
        } else {
            return Err(other(
                "not a docker-save archive (no manifest.json or index.json)",
            ));
        };
        if entries.is_empty() {
            return Err(other("archive contains no images"));
        }

        let mut loaded = Vec::new();
        for e in &entries {
            loaded.push(self.load_one(path, &idx, e, emit)?);
        }
        Ok(loaded)
    }

    /// OCI layout: index.json → per-manifest blob → config + layers.
    fn entries_from_oci_index(
        &self,
        path: &Path,
        idx: &TarIndex,
    ) -> io::Result<Vec<SaveManifestEntry>> {
        let raw = idx.read(path, "index.json")?;
        let index: ManifestIndex =
            serde_json::from_slice(&raw).map_err(|e| other(format!("bad index.json: {e}")))?;
        let mut out = Vec::new();
        for desc in &index.manifests {
            if let Some(p) = &desc.platform {
                if !p.architecture.is_empty() && p.architecture != self.arch {
                    continue;
                }
            }
            let blob = blob_member(&desc.digest);
            let raw = match idx.read(path, &blob) {
                Ok(r) => r,
                Err(_) => continue,
            };
            // A nested index (multi-arch) inside the archive: recurse one level.
            if let Ok(nested) = serde_json::from_slice::<ManifestIndex>(&raw) {
                if !nested.manifests.is_empty() {
                    for d in &nested.manifests {
                        if let Some(p) = &d.platform {
                            if !p.architecture.is_empty() && p.architecture != self.arch {
                                continue;
                            }
                        }
                        if let Ok(m) = serde_json::from_slice::<Manifest>(
                            &idx.read(path, &blob_member(&d.digest))?,
                        ) {
                            out.push(entry_from_manifest(&m, ref_name(desc)));
                        }
                    }
                    continue;
                }
            }
            let m: Manifest =
                serde_json::from_slice(&raw).map_err(|e| other(format!("bad manifest: {e}")))?;
            out.push(entry_from_manifest(&m, ref_name(desc)));
        }
        Ok(out)
    }

    fn load_one(
        &self,
        path: &Path,
        idx: &TarIndex,
        e: &SaveManifestEntry,
        emit: &mut dyn FnMut(String),
    ) -> io::Result<LoadedImage> {
        let config_raw = idx.read(path, &e.config)?;
        let oci: OciImageConfig = serde_json::from_slice(&config_raw)
            .map_err(|err| other(format!("bad image config {}: {err}", e.config)))?;
        let image_id = format!("sha256:{}", hex::encode(crate::sha256(&config_raw)));

        let mut total = 0i64;
        for (i, member) in e.layers.iter().enumerate() {
            let want = oci.rootfs.diff_ids.get(i).cloned().unwrap_or_default();
            let size = idx.size_of(member)?;
            total += size as i64;
            if !want.is_empty() && self.layer_dir(&want).join(".complete").exists() {
                emit(format!("Loading layer: {} already exists", short(&want)));
                continue;
            }
            emit(format!("Loading layer: {}", short(&want)));
            let tmp = self
                .root
                .join(format!("layers/.tmp-load-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let mut head = [0u8; 2];
            let peek = idx.open(path, member)?.0.read(&mut head).unwrap_or(0);
            let (blob, _) = idx.open(path, member)?;
            let reader: Box<dyn Read> = if is_gzip(&head[..peek]) {
                Box::new(flate2::read::GzDecoder::new(blob))
            } else {
                Box::new(blob)
            };
            let mut hashing = unpack::HashingReader::new(reader);
            unpack::apply_layer(&mut hashing, &tmp)?;
            let diff_id = hashing.finish()?;
            if !want.is_empty() && diff_id != want {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(other(format!(
                    "layer digest mismatch in archive (want {want}, got {diff_id})"
                )));
            }
            let final_dir = self.layer_dir(&diff_id);
            let _ = std::fs::remove_dir_all(&final_dir);
            std::fs::rename(&tmp, &final_dir)?;
            std::fs::write(final_dir.join(".complete"), size.to_string())?;
        }

        let record = ImageRecord {
            id: image_id.clone(),
            manifest_digest: String::new(),
            diff_ids: oci.rootfs.diff_ids.clone(),
            size: total,
            created: oci.created.clone(),
            architecture: oci.architecture.clone(),
            os: oci.os.clone(),
            config: oci.config.clone(),
        };
        let tags: Vec<String> = e.repo_tags.clone().unwrap_or_default();
        self.insert_local(&config_raw, record, None)?;
        for t in &tags {
            self.tag(&image_id, t)?;
            emit(format!("Loaded image: {t}"));
        }
        if tags.is_empty() {
            emit(format!("Loaded image ID: {image_id}"));
        }
        Ok(LoadedImage {
            id: image_id,
            repo_tags: tags,
        })
    }
}

fn entry_from_manifest(m: &Manifest, tag: Option<String>) -> SaveManifestEntry {
    SaveManifestEntry {
        config: blob_member(&m.config.digest),
        repo_tags: tag.map(|t| vec![t]),
        layers: m.layers.iter().map(|l| blob_member(&l.digest)).collect(),
    }
}

fn blob_member(digest: &str) -> String {
    format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"))
}

/// OCI layout records the image's name in the descriptor annotations
/// (`org.opencontainers.image.ref.name`, or docker's `io.containerd...`).
fn ref_name(desc: &slim_api::image::Descriptor) -> Option<String> {
    for k in [
        "org.opencontainers.image.ref.name",
        "io.containerd.image.name",
    ] {
        if let Some(v) = desc.annotations.get(k) {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    None
}

fn short(diff_id: &str) -> String {
    diff_id
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect()
}
