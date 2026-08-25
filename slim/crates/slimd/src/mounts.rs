//! Mount resolution: `-v`/`--volume` binds, `--mount` specs, and the image's
//! own `VOLUME` declarations.
//!
//! Resolution happens at CREATE time (docker parity: `docker inspect` on a
//! created container already lists its Mounts) and is replayed at every start,
//! so a restart re-binds exactly what the first start did.
//!
//! Host sources live on the virtiofs share that maps `$HOME` into the vessel
//! at its identical macOS path, which is why nothing here rewrites paths —
//! `-v "$HOME/Library/Application Support/app/conf:/conf:ro"` names the same
//! directory on both sides, spaces and all.

use crate::container::Container;
use crate::volumes::VolumeManager;
use serde::{Deserialize, Serialize};
use slim_api::container::MountSpec;
use slim_image::ImageRecord;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// One mount, fully resolved against the volume store.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ResolvedMount {
    /// "bind" | "volume" | "tmpfs"
    pub typ: String,
    /// Volume name (empty for binds/tmpfs).
    pub name: String,
    /// Host path: the bind source, or the volume's `_data` dir.
    pub source: String,
    /// Absolute path inside the container.
    pub target: String,
    pub read_only: bool,
    /// tmpfs mount options ("size=64m"), unused otherwise.
    pub options: String,
    /// Anonymous volume created for an image `VOLUME` (removed with `rm -v`).
    pub anonymous: bool,
}

/// `-v` / `--volume` spec, before the volume store is consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSpec {
    /// None → anonymous volume (`-v /data`).
    pub source: Option<String>,
    pub target: String,
    pub read_only: bool,
}

/// Normalize a container-side mount path. Docker requires it absolute; a
/// trailing slash is meaningless and would produce a doubled path later.
pub fn clean_target(t: &str) -> Result<String, String> {
    if !t.starts_with('/') {
        return Err(format!(
            "invalid mount target {t:?}: must be an absolute path"
        ));
    }
    let trimmed = t.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

fn is_path_source(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with('~')
}

/// Parse a docker `-v`/`--volume` spec: `[source:]target[:options]`.
///
/// The source may be any host path — including one with spaces, which is the
/// normal case on macOS (`~/Library/Application Support/…`). Colons separate
/// fields, so only a source containing a colon is ambiguous, and docker treats
/// that as an error too.
pub fn parse_volume_spec(spec: &str) -> Result<VolumeSpec, String> {
    let invalid = || format!("invalid volume specification: {spec:?}");
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, target, opts) = match parts.as_slice() {
        [dst] => (None, *dst, ""),
        [src, dst] => (Some(*src), *dst, ""),
        [src, dst, opts] => (Some(*src), *dst, *opts),
        _ => return Err(invalid()),
    };
    if target.is_empty() {
        return Err(invalid());
    }
    let mut read_only = false;
    for o in opts.split(',').filter(|o| !o.is_empty()) {
        match o {
            "ro" | "readonly" => read_only = true,
            // rw is the default; the SELinux/consistency/propagation flags are
            // accepted and ignored — they have no meaning inside the vessel.
            "rw" | "z" | "Z" | "nocopy" | "cached" | "delegated" | "consistent" | "bind"
            | "volume" | "private" | "rprivate" | "shared" | "rshared" | "slave" | "rslave" => {}
            other => return Err(format!("invalid mount option: {other:?}")),
        }
    }
    let source = match source {
        None => None,
        Some("") => return Err(invalid()),
        Some(s) => {
            if !is_path_source(s) && s.contains('/') {
                return Err(format!(
                    "invalid mount source {s:?}: host paths must be absolute"
                ));
            }
            Some(s.to_string())
        }
    };
    Ok(VolumeSpec {
        source,
        target: clean_target(target)?,
        read_only,
    })
}

/// Set by the kube bridge on every container it manages (keep in sync with
/// kube_bridge::MANAGED).
const KUBE_MANAGED_LABEL: &str = "io.nebula.kube.bridge";

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Resolve every mount a container asks for, in docker's precedence order:
/// `--mount` and `-v` first (explicit wins), then the image's `VOLUME`
/// declarations for targets nothing else covers.
pub fn resolve(
    volumes: &VolumeManager,
    c: &Container,
    image: &ImageRecord,
) -> io::Result<Vec<ResolvedMount>> {
    // Keyed by target so a later spec replaces an earlier one for the same
    // path (and so image VOLUMEs never shadow an explicit mount).
    let mut out: BTreeMap<String, ResolvedMount> = BTreeMap::new();

    for spec in &c.host_config.binds {
        let v = parse_volume_spec(spec).map_err(bad)?;
        let m = match v.source {
            Some(src) if is_path_source(&src) => ResolvedMount {
                typ: "bind".into(),
                source: src,
                target: v.target.clone(),
                read_only: v.read_only,
                ..Default::default()
            },
            Some(name) => volume_mount(volumes, &name, &v.target, v.read_only, false)?,
            None => anonymous_volume(volumes, &v.target, v.read_only)?,
        };
        out.insert(v.target, m);
    }

    for m in &c.host_config.mounts {
        let target = clean_target(&m.target).map_err(bad)?;
        let resolved = resolve_mount_spec(volumes, m, &target)?;
        out.insert(target, resolved);
    }

    // Image VOLUMEs: docker gives each one an anonymous volume so writes
    // survive `docker cp`-style inspection and layer teardown. Skipped for
    // any target the caller already mounted — and for pods, because
    // Kubernetes ignores image VOLUMEs outright (a pod's writes go to the
    // container filesystem unless its spec asked for a volume).
    if !c.config.labels.contains_key(KUBE_MANAGED_LABEL) {
        if let Some(vols) = &image.config.volumes {
            for path in vols.keys() {
                let Ok(target) = clean_target(path) else {
                    continue;
                };
                if out.contains_key(&target) {
                    continue;
                }
                out.insert(target.clone(), anonymous_volume(volumes, &target, false)?);
            }
        }
    }

    Ok(out.into_values().collect())
}

fn resolve_mount_spec(
    volumes: &VolumeManager,
    m: &MountSpec,
    target: &str,
) -> io::Result<ResolvedMount> {
    match m.typ.as_str() {
        "bind" => {
            if m.source.is_empty() {
                return Err(bad("--mount type=bind requires a source".into()));
            }
            // docker refuses to invent a bind source for --mount (unlike -v).
            if !Path::new(&m.source).exists() {
                return Err(bad(format!(
                    "invalid mount config: bind source path does not exist: {}",
                    m.source
                )));
            }
            Ok(ResolvedMount {
                typ: "bind".into(),
                source: m.source.clone(),
                target: target.to_string(),
                read_only: m.read_only,
                ..Default::default()
            })
        }
        "volume" => {
            if m.source.is_empty() {
                anonymous_volume(volumes, target, m.read_only)
            } else {
                volume_mount(volumes, &m.source, target, m.read_only, false)
            }
        }
        "tmpfs" => Ok(ResolvedMount {
            typ: "tmpfs".into(),
            target: target.to_string(),
            read_only: m.read_only,
            options: String::new(),
            ..Default::default()
        }),
        other => Err(bad(format!("unsupported mount type: {other:?}"))),
    }
}

fn volume_mount(
    volumes: &VolumeManager,
    name: &str,
    target: &str,
    read_only: bool,
    anonymous: bool,
) -> io::Result<ResolvedMount> {
    let data = volumes.ensure(name)?;
    Ok(ResolvedMount {
        typ: "volume".into(),
        name: name.to_string(),
        source: data.to_string_lossy().into_owned(),
        target: target.to_string(),
        read_only,
        options: String::new(),
        anonymous,
    })
}

fn anonymous_volume(
    volumes: &VolumeManager,
    target: &str,
    read_only: bool,
) -> io::Result<ResolvedMount> {
    let name = slim_net::rand_id();
    volume_mount(volumes, &name, target, read_only, true)
}

/// What the runtime needs: bind mounts, plus (target, options) tmpfs entries.
pub type PreparedMounts = (Vec<slim_runtime::BindMount>, Vec<(String, String)>);

/// Make every resolved mount real against a prepared rootfs, and hand the
/// runtime what it needs. Bind sources are created when missing (docker
/// parity for `-v`), and a fresh volume inherits whatever the image ships at
/// that path — the behaviour `-v db:/var/lib/mysql` depends on.
pub fn prepare(mounts: &[ResolvedMount], merged: &Path) -> io::Result<PreparedMounts> {
    let mut binds = Vec::new();
    let mut tmpfs = Vec::new();
    for m in mounts {
        let in_image = image_path(merged, &m.target);
        match m.typ.as_str() {
            "tmpfs" => tmpfs.push((m.target.clone(), m.options.clone())),
            "volume" => {
                let src = PathBuf::from(&m.source);
                std::fs::create_dir_all(&src)?;
                seed_volume(&src, &in_image);
                binds.push(slim_runtime::BindMount {
                    source: src,
                    target: m.target.clone(),
                    read_only: m.read_only,
                });
            }
            _ => {
                let src = PathBuf::from(&m.source);
                create_missing_source(&src, &in_image)?;
                binds.push(slim_runtime::BindMount {
                    source: src,
                    target: m.target.clone(),
                    read_only: m.read_only,
                });
            }
        }
    }
    Ok((binds, tmpfs))
}

/// Where a container-absolute path lands in the prepared rootfs.
fn image_path(merged: &Path, target: &str) -> PathBuf {
    merged.join(target.trim_start_matches('/'))
}

/// `-v /missing/path:/x` creates the source, like docker. Docker always makes
/// a DIRECTORY, which then shadows the file the app meant to write there;
/// slim looks at what the image has at the target and matches it, so a
/// single-file bind whose source doesn't exist yet becomes a file, not a
/// directory that breaks the mount.
fn create_missing_source(src: &Path, in_image: &Path) -> io::Result<()> {
    if src.exists() {
        return Ok(());
    }
    let want_file = in_image
        .symlink_metadata()
        .map(|m| m.file_type().is_file())
        .unwrap_or(false);
    if want_file {
        if let Some(parent) = src.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(src)?;
    } else {
        std::fs::create_dir_all(src)?;
    }
    Ok(())
}

/// Copy the image's content at the mount point into a still-empty volume.
fn seed_volume(data: &Path, in_image: &Path) {
    let empty = std::fs::read_dir(data)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if !empty || !in_image.is_dir() {
        return;
    }
    let _ = copy_tree(in_image, data);
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(to)?;
    if let Ok(md) = from.metadata() {
        let _ =
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(md.permissions().mode()));
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            if let Ok(link) = std::fs::read_link(&src) {
                let _ = std::os::unix::fs::symlink(link, &dst);
            }
        } else if ft.is_dir() {
            copy_tree(&src, &dst)?;
        } else if ft.is_file() {
            std::fs::copy(&src, &dst)?;
        }
        // Sockets/devices in an image VOLUME are not worth reproducing.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_spec_forms() {
        assert_eq!(
            parse_volume_spec("/host/dir:/ctr/dir").unwrap(),
            VolumeSpec {
                source: Some("/host/dir".into()),
                target: "/ctr/dir".into(),
                read_only: false
            }
        );
        assert_eq!(
            parse_volume_spec("name:/data:ro").unwrap(),
            VolumeSpec {
                source: Some("name".into()),
                target: "/data".into(),
                read_only: true
            }
        );
        assert_eq!(
            parse_volume_spec("/data").unwrap(),
            VolumeSpec {
                source: None,
                target: "/data".into(),
                read_only: false
            }
        );
    }

    #[test]
    fn spaces_in_the_host_path_survive() {
        let v = parse_volume_spec(
            "/Users/me/Library/Application Support/app/conf:/rathena/conf/import:ro",
        )
        .unwrap();
        assert_eq!(
            v.source.as_deref(),
            Some("/Users/me/Library/Application Support/app/conf")
        );
        assert_eq!(v.target, "/rathena/conf/import");
        assert!(v.read_only);
    }

    #[test]
    fn trailing_slash_and_rw_options() {
        let v = parse_volume_spec("/host/dir/:/ctr/dir/:rw,z").unwrap();
        assert_eq!(v.target, "/ctr/dir");
        assert!(!v.read_only);
    }

    #[test]
    fn rejects_relative_targets_and_bad_options() {
        assert!(parse_volume_spec("/host:ctr").is_err());
        assert!(parse_volume_spec("/host:/ctr:nope").is_err());
        assert!(parse_volume_spec("relative/path:/ctr").is_err());
    }
}
