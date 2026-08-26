//! OCI layer tar application with whiteout conversion.
//!
//! OCI tars encode deletions as `.wh.<name>` entries and opaque dirs as
//! `.wh..wh..opq`; the on-disk overlayfs convention is a 0:0 char device /
//! a `trusted.overlay.opaque` xattr. We convert at unpack time so layer
//! dirs are directly usable as overlay lowerdirs. The reverse (dir → tar)
//! is used by build/commit.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

pub struct HashingReader<R> {
    inner: R,
    hasher: crate::Sha256Stream,
    pub count: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: crate::Sha256Stream::new(),
            count: 0,
        }
    }

    /// Drain the rest of the stream (tar stops at the archive end marker but
    /// the diff_id covers ALL bytes) and return "sha256:<hex>".
    pub fn finish(mut self) -> io::Result<String> {
        io::copy(&mut self, &mut io::sink())?;
        Ok(format!("sha256:{}", self.hasher.finish_hex()))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

/// Apply one uncompressed layer tar into `dest`. Returns bytes written
/// (approximate layer size on disk).
pub fn apply_layer(reader: impl Read, dest: &Path) -> io::Result<u64> {
    std::fs::create_dir_all(dest)?;
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_unpack_xattrs(cfg!(target_os = "linux"));
    ar.set_overwrite(true);
    // NB: ownership is applied by hand below rather than via the tar crate's
    // set_preserve_ownerships, which fails the ENTIRE layer when one header's
    // numeric field is blank — an image that used to import (wrongly owned)
    // would stop importing at all. Blank means "unspecified", i.e. root.
    let mut size = 0u64;
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(fname) = path.file_name().and_then(|f| f.to_str()).map(String::from) else {
            continue;
        };
        let parent = path.parent().unwrap_or(Path::new("")).to_owned();

        if fname == ".wh..wh..opq" {
            let dir = dest.join(&parent);
            std::fs::create_dir_all(&dir)?;
            set_opaque(&dir);
            continue;
        }
        if let Some(victim) = fname.strip_prefix(".wh.") {
            let target = dest.join(&parent).join(victim);
            std::fs::create_dir_all(dest.join(&parent))?;
            // Remove anything this layer shadows, then mark the whiteout.
            if target.is_dir() && !target.is_symlink() {
                let _ = std::fs::remove_dir_all(&target);
            } else {
                let _ = std::fs::remove_file(&target);
            }
            make_whiteout(&target);
            continue;
        }
        size += entry.size();
        // Read the header before unpack_in consumes the entry. Layer tars
        // carry both numeric ids and uname/gname; only the numeric ids are
        // meaningful, so the names are ignored (resolving them against the
        // HOST's passwd would be wrong).
        let (uid, gid) = (
            entry.header().uid().unwrap_or(0),
            entry.header().gid().unwrap_or(0),
        );
        let mode = entry.header().mode().ok();
        let etype = entry.header().entry_type();
        let unpacked_to = unpack_dest(dest, &path);
        // unpack_in refuses path traversal; hardlinks/symlinks land as-is.
        if !entry.unpack_in(dest)? {
            continue;
        }
        // A hard link shares the target's inode, which this layer already
        // owns correctly — chowning through it would be redundant at best.
        if let Some(t) = unpacked_to.filter(|_| !etype.is_hard_link()) {
            set_ownership(&t, uid, gid, mode, etype.is_symlink());
        }
    }
    Ok(size)
}

/// The path `unpack_in` writes an entry to, mirroring its rules: leading '/',
/// root and '.' components are dropped, and a '..' anywhere means the entry is
/// skipped. `None` when nothing under `dest` is written.
fn unpack_dest(dest: &Path, path: &Path) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    for part in path.components() {
        match part {
            Component::Prefix(..) | Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => return None,
            Component::Normal(p) => out.push(p),
        }
    }
    (out != dest).then_some(out)
}

/// Apply a tar entry's uid/gid on disk. slimd unpacks as root in the guest, so
/// the chown lands; on a host-side unpack there is nothing to do (and an
/// unprivileged chown would only fail).
#[cfg(target_os = "linux")]
fn set_ownership(path: &Path, uid: u64, gid: u64, mode: Option<u32>, symlink: bool) {
    use std::os::unix::ffi::OsStrExt;
    // Already correct: we unpacked as root, and most image files are root's.
    if uid == 0 && gid == 0 {
        return;
    }
    let (Ok(uid), Ok(gid)) = (u32::try_from(uid), u32::try_from(gid)) else {
        return;
    };
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // lchown: a symlink's own ownership is what matters, not its target's.
    unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    // chown clears setuid/setgid, so restore the mode afterwards (a symlink's
    // mode is meaningless, and chmod would follow the link).
    if let Some(m) = mode.filter(|_| !symlink) {
        unsafe { libc::chmod(c.as_ptr(), (m & 0o7777) as libc::mode_t) };
    }
}

#[cfg(not(target_os = "linux"))]
fn set_ownership(_path: &Path, _uid: u64, _gid: u64, _mode: Option<u32>, _symlink: bool) {}

#[cfg(target_os = "linux")]
fn set_opaque(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
    unsafe {
        libc::setxattr(
            c.as_ptr(),
            c"trusted.overlay.opaque".as_ptr(),
            c"y".as_ptr() as *const libc::c_void,
            1,
            0,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn set_opaque(_dir: &Path) {}

#[cfg(target_os = "linux")]
fn make_whiteout(target: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
    unsafe {
        // Whiteout = 0:0 char device with mode 000 (S_IFCHR alone).
        libc::mknod(c.as_ptr(), libc::S_IFCHR, libc::makedev(0, 0));
    }
}

#[cfg(not(target_os = "linux"))]
fn make_whiteout(target: &Path) {
    // Host-side tests: represent whiteouts as empty marker files.
    let _ = std::fs::File::create(target.with_file_name(format!(
        ".wh-marker.{}",
        target.file_name().unwrap_or_default().to_string_lossy()
    )));
}

/// Pack a container/build upper dir into an OCI layer tar (whiteouts
/// converted back). Returns the tar bytes via the writer.
pub fn pack_layer(upper: &Path, out: impl std::io::Write) -> io::Result<()> {
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    pack_dir(&mut builder, upper, Path::new(""))?;
    builder.finish()
}

fn pack_dir<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    base: &Path,
    rel: &Path,
) -> io::Result<()> {
    let dir = base.join(rel);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    if rel != Path::new("") && is_opaque(&dir) {
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        builder.append_data(&mut h, rel.join(".wh..wh..opq"), io::empty())?;
    }
    for e in entries {
        let name = e.file_name();
        let rel_path = rel.join(&name);
        let meta = e.metadata()?;
        let ft = meta.file_type();
        if is_whiteout_node(&meta) {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            let wh = rel.join(format!(".wh.{}", name.to_string_lossy()));
            builder.append_data(&mut h, wh, io::empty())?;
            continue;
        }
        if ft.is_dir() {
            builder.append_dir(&rel_path, e.path())?;
            pack_dir(builder, base, &rel_path)?;
        } else if ft.is_symlink() || ft.is_file() {
            builder.append_path_with_name(e.path(), &rel_path)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_whiteout_node(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    meta.file_type().is_char_device() && meta.rdev() == 0
}

#[cfg(target_os = "linux")]
fn is_opaque(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
    let mut buf = [0u8; 4];
    let n = unsafe {
        libc::getxattr(
            c.as_ptr(),
            c"trusted.overlay.opaque".as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            4,
        )
    };
    n == 1 && buf[0] == b'y'
}

#[cfg(not(target_os = "linux"))]
fn is_whiteout_node(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn is_opaque(_dir: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_basic_layer() {
        let dir = std::env::temp_dir().join(format!("slimg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build a tar in memory: a dir, a file, and a whiteout.
        let mut tarbuf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tarbuf);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "etc/", io::empty()).unwrap();
            let data = b"hello\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "etc/motd", &data[..]).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "etc/.wh.oldfile", io::empty())
                .unwrap();
            b.finish().unwrap();
        }
        let hashing = HashingReader::new(&tarbuf[..]);
        let mut hashing = hashing;
        // Every header above leaves uid/gid blank, which is what a hand-rolled
        // or older tar writes. That must still apply cleanly: "unspecified"
        // means root, not a failed layer.
        apply_layer(&mut hashing, &dir).unwrap();
        let diff = hashing.finish().unwrap();
        assert!(diff.starts_with("sha256:"));
        assert_eq!(
            std::fs::read_to_string(dir.join("etc/motd")).unwrap(),
            "hello\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn owned_entries_apply() {
        let dir = std::env::temp_dir().join(format!("slimg-own-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut tarbuf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tarbuf);
            let data = b"x\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_uid(4242);
            h.set_gid(4243);
            h.set_cksum();
            b.append_data(&mut h, "owned", &data[..]).unwrap();
            b.finish().unwrap();
        }
        // The chown itself needs root (appstack.sh covers that in the guest);
        // what is asserted here is that a header carrying ids still unpacks.
        apply_layer(&tarbuf[..], &dir).unwrap();
        assert!(dir.join("owned").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unpack_dest_mirrors_tar_rules() {
        let d = Path::new("/layer");
        assert_eq!(
            unpack_dest(d, Path::new("etc/motd")),
            Some(d.join("etc/motd"))
        );
        // leading '/' and '.' are dropped, exactly as unpack_in drops them
        assert_eq!(
            unpack_dest(d, Path::new("/etc/motd")),
            Some(d.join("etc/motd"))
        );
        assert_eq!(
            unpack_dest(d, Path::new("./etc/motd")),
            Some(d.join("etc/motd"))
        );
        // traversal and empty names write nothing, so there is nothing to chown
        assert_eq!(unpack_dest(d, Path::new("../escape")), None);
        assert_eq!(unpack_dest(d, Path::new("etc/../../escape")), None);
        assert_eq!(unpack_dest(d, Path::new("./")), None);
        assert_eq!(unpack_dest(d, Path::new("/")), None);
    }
}
