//! Sparse-aware writes for guest disk images.
//!
//! The shipped rootfs is 1 GiB of which ~23 MB is non-zero, and installing it
//! used to write every byte three times — ~3.2 GB of I/O to deliver 23 MB
//! (issue #24). On Windows that is 3.2 GB through Defender's real-time
//! scanner, which turned an embedded app's engine upgrade into a five-minute
//! stall. Writing the zeros was never worth the bandwidth on any platform:
//! skipping them is both smaller *and* faster.
//!
//! The trick is a hole rather than a write: seek past a run of zeros and the
//! filesystem records "nothing here", and reads still return zeros. APFS,
//! ext4, btrfs and XFS do that on a plain seek; **NTFS does not** unless the
//! handle has been marked sparse first, so a seek there allocates the zeros
//! anyway — hence [`mark_sparse`].
//!
//! Output is byte-identical to a dense copy, which is the property everything
//! here is built to preserve: a hole written wrong is silent until something
//! tries to boot from it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Context;

/// Zero-run granularity. 64 KiB is NTFS's sparse allocation unit — below it
/// Windows cannot punch a hole at all — and a clean multiple of the 4 KiB
/// blocks ext4 and APFS use.
pub const BLOCK: usize = 64 * 1024;

/// Read/write unit. Holes are decided per [`BLOCK`], but adjacent non-empty
/// blocks are written as one call: the rootfs's non-zero content is not
/// scattered evenly, and issuing 64 KiB writes for a contiguous 20 MB region
/// is a lot of syscalls for no benefit. A multiple of BLOCK, so hole
/// alignment is unaffected.
const IO_CHUNK: usize = 16 * BLOCK;

/// Stream `src` into a newly created file at `dst`, seeking past runs of
/// zeros instead of writing them. Returns the logical size written.
///
/// The result is byte-for-byte what a dense copy would produce: the final
/// length is set explicitly, so a file ending in zeros keeps them.
pub fn write_sparse(src: &mut impl Read, dst: &Path) -> anyhow::Result<u64> {
    write_sparse_many(src, &[dst])
}

/// As [`write_sparse`], but filling several destinations from one pass over
/// the source.
///
/// Reading is the expensive half — decompressing a gzipped rootfs, or pulling
/// a GiB back off disk — so producing the pristine copy and the live disk
/// together beats writing one and then copying it. That only matters where
/// the filesystem cannot clone: on APFS or btrfs/XFS the second file should
/// be a reflink of the first, which is cheaper still *and* shares its
/// extents. See `install_image` for which path is taken where.
pub fn write_sparse_many(src: &mut impl Read, dsts: &[&Path]) -> anyhow::Result<u64> {
    anyhow::ensure!(!dsts.is_empty(), "no destination to write");
    let mut outs = Vec::with_capacity(dsts.len());
    for dst in dsts {
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let f = File::create(dst).with_context(|| format!("create {} failed", dst.display()))?;
        mark_sparse(&f);
        // The second element is that handle's cursor; it falls behind the
        // logical position by exactly the holes skipped so far.
        outs.push((f, 0u64));
    }

    let mut buf = vec![0u8; IO_CHUNK];
    let mut logical: u64 = 0;
    loop {
        let n = read_block(src, &mut buf).context("reading source failed")?;
        if n == 0 {
            break;
        }
        // Walk the chunk block by block, writing each maximal run of
        // non-empty blocks in one go and skipping the rest.
        let mut at = 0usize;
        while at < n {
            let block_end = (at + BLOCK).min(n);
            if !buf[at..block_end].iter().any(|&b| b != 0) {
                at = block_end;
                continue;
            }
            let run_start = at;
            let mut run_end = block_end;
            while run_end < n {
                let next_end = (run_end + BLOCK).min(n);
                if !buf[run_end..next_end].iter().any(|&b| b != 0) {
                    break;
                }
                run_end = next_end;
            }
            let at_logical = logical + run_start as u64;
            for (i, (out, cursor)) in outs.iter_mut().enumerate() {
                if *cursor != at_logical {
                    out.seek(SeekFrom::Start(at_logical))?;
                    *cursor = at_logical;
                }
                out.write_all(&buf[run_start..run_end])
                    .with_context(|| format!("write {} failed", dsts[i].display()))?;
                *cursor += (run_end - run_start) as u64;
            }
            at = run_end;
        }
        logical += n as u64;
    }

    // Trailing zeros were never written, and a short final block may have
    // left a handle behind the true end. set_len is what makes the size
    // exact — and it extends with a hole, not with written zeros.
    for (out, _) in outs.iter_mut() {
        out.set_len(logical)?;
        out.flush()?;
    }
    Ok(logical)
}

/// Sparse-aware file copy. A drop-in for `std::fs::copy` on image files,
/// including its permission-preserving behaviour.
pub fn copy_sparse(from: &Path, to: &Path) -> anyhow::Result<u64> {
    // Copying a file onto itself truncates the source to nothing on the way
    // in. `install-image --kernel ~/.nebula/kernel/Image` has done exactly
    // that; refuse rather than destroy the input.
    if same_file(from, to) {
        anyhow::bail!("refusing to copy {} onto itself", from.display());
    }
    let mut src = File::open(from).with_context(|| format!("open {} failed", from.display()))?;
    let n = write_sparse(&mut src, to)?;
    // `fs::copy` carries the mode across; keep parity so swapping this in
    // cannot silently change a file's permissions.
    if let Ok(meta) = src.metadata() {
        let _ = std::fs::set_permissions(to, meta.permissions());
    }
    Ok(n)
}

/// Fill `buf` from `src`, short reads and all, so zero-runs are detected on
/// aligned blocks whatever the reader's chunking is (a gzip decoder hands
/// back whatever its window produced). Returns bytes read; < buf.len() only
/// at EOF — which is what keeps block alignment true across the whole file.
fn read_block(src: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        // `to` usually does not exist yet, which is not a match.
        _ => false,
    }
}

/// Mark a freshly created file sparse. Only NTFS needs telling; everywhere
/// else a seek already makes a hole.
///
/// Best effort: a filesystem that refuses (FAT32, a network share) just gets
/// the dense file it would have got before.
#[cfg(windows)]
fn mark_sparse(f: &File) {
    use std::os::windows::io::AsRawHandle;

    // winioctl.h: CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 49, METHOD_BUFFERED, FILE_SPECIAL_ACCESS)
    const FSCTL_SET_SPARSE: u32 = 0x000900C4;
    let mut returned: u32 = 0;
    unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            f.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(windows))]
fn mark_sparse(_f: &File) {}

/// Bytes actually allocated on disk — the number a hole changes, as opposed
/// to the file's length. `None` where the platform will not say.
#[cfg(unix)]
pub fn physical_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.blocks() * 512)
}

/// Windows: `GetCompressedFileSizeW` is the documented way to ask what a
/// sparse or compressed file actually occupies. Worth the extra call —
/// Windows is where this saving matters, so it should be the platform that
/// can show it.
#[cfg(windows)]
pub fn physical_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut high: u32 = 0;
    let low = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW(wide.as_ptr(), &mut high)
    };
    // INVALID_FILE_SIZE, and only then is the error real (a valid size can
    // legitimately have 0xFFFFFFFF as its low word).
    if low == u32::MAX && std::io::Error::last_os_error().raw_os_error() != Some(0) {
        return None;
    }
    Some(((high as u64) << 32) | low as u64)
}

#[cfg(not(any(unix, windows)))]
pub fn physical_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn scratch(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nebula-sparse-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// The property everything else rests on: same bytes out as in.
    fn round_trips(name: &str, data: &[u8]) {
        let dst = scratch(name);
        let n = write_sparse(&mut Cursor::new(data), &dst).unwrap();
        assert_eq!(n, data.len() as u64, "{name}: logical size");
        let back = std::fs::read(&dst).unwrap();
        assert_eq!(back.len(), data.len(), "{name}: file length");
        assert!(back == data, "{name}: content differs from the source");
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn empty_input() {
        round_trips("empty", &[]);
    }

    #[test]
    fn all_zeros_keeps_its_length() {
        round_trips("zeros", &vec![0u8; BLOCK * 3]);
    }

    #[test]
    fn no_zeros_at_all() {
        let data: Vec<u8> = (0..BLOCK * 2 + 7).map(|i| (i % 255 + 1) as u8).collect();
        round_trips("dense", &data);
    }

    #[test]
    fn data_holes_data() {
        let mut data = vec![0u8; BLOCK * 5];
        data[..BLOCK].fill(0xAB);
        data[BLOCK * 4..].fill(0xCD);
        round_trips("holes", &data);
    }

    #[test]
    fn trailing_zeros_are_preserved() {
        // The case set_len exists for: nothing is written after the data, so
        // without it the file would end early.
        let mut data = vec![0u8; BLOCK * 4];
        data[..100].fill(0x7F);
        round_trips("trailing", &data);
    }

    #[test]
    fn partial_final_block() {
        let mut data = vec![0u8; BLOCK * 2 + 13];
        data[BLOCK * 2..].fill(0x11);
        round_trips("partial", &data);
    }

    #[test]
    fn a_single_nonzero_byte_in_a_block_keeps_the_block() {
        let mut data = vec![0u8; BLOCK * 3];
        data[BLOCK + 40_000] = 1;
        round_trips("needle", &data);
    }

    #[test]
    fn shorter_than_one_block() {
        round_trips("tiny", &[1, 2, 3, 0, 0, 0, 4]);
    }

    /// A reader that hands back one byte at a time — a gzip decoder can be
    /// nearly as awkward, and zero-run detection must not depend on chunking.
    struct Dribble<'a>(&'a [u8], usize);
    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1 >= self.0.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[self.1];
            self.1 += 1;
            Ok(1)
        }
    }

    #[test]
    fn short_reads_do_not_change_the_output() {
        let mut data = vec![0u8; BLOCK * 2 + 5];
        data[10] = 9;
        data[BLOCK + 3] = 8;
        let dst = scratch("dribble");
        write_sparse(&mut Dribble(&data, 0), &dst).unwrap();
        assert!(std::fs::read(&dst).unwrap() == data);
        let _ = std::fs::remove_file(&dst);
    }

    /// Does this filesystem hole a file of `len` at all? APFS declines below
    /// ~32 MiB (measured: 16 MiB with 64 KiB of data reports fully
    /// allocated, 32 MiB reports 64 KiB), so a sparseness assertion on a
    /// small sample measures the filesystem, not us.
    fn platform_holes_at(len: u64) -> bool {
        let probe = scratch("probe");
        let mut f = File::create(&probe).unwrap();
        f.write_all(&[1u8; BLOCK]).unwrap();
        f.set_len(len).unwrap();
        drop(f);
        let holed = physical_bytes(&probe).map(|p| p < len / 2).unwrap_or(false);
        let _ = std::fs::remove_file(&probe);
        holed
    }

    #[test]
    fn copy_matches_the_source_and_punches_holes() {
        const LEN: usize = BLOCK * 768; // 48 MiB — above every threshold we know of
        let src = scratch("copy-src");
        let dst = scratch("copy-dst");
        let mut data = vec![0u8; LEN];
        data[..BLOCK].fill(0x5A);
        std::fs::write(&src, &data).unwrap();

        copy_sparse(&src, &dst).unwrap();
        assert!(std::fs::read(&dst).unwrap() == data, "content differs");
        assert_eq!(std::fs::metadata(&dst).unwrap().len(), LEN as u64);

        // The point of the exercise: the copy is materially smaller on disk —
        // wherever the platform is willing to make holes at all.
        if platform_holes_at(LEN as u64) {
            let phys = physical_bytes(&dst).expect("unix reports allocation");
            assert!(
                phys < LEN as u64 / 2,
                "expected a sparse copy, got {phys} bytes for a {LEN}-byte file"
            );
        }
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn many_destinations_all_match() {
        let mut data = vec![0u8; BLOCK * 5];
        data[..BLOCK].fill(0x3C);
        data[BLOCK * 3..BLOCK * 3 + 10].fill(0x7E);
        let a = scratch("many-a");
        let b = scratch("many-b");
        let n = write_sparse_many(&mut Cursor::new(&data), &[&a, &b]).unwrap();
        assert_eq!(n, data.len() as u64);
        for p in [&a, &b] {
            assert!(std::fs::read(p).unwrap() == data, "{} differs", p.display());
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn runs_spanning_io_chunks_survive() {
        // Non-zero data straddling the read boundary is where coalescing gets
        // the offsets wrong if it gets them wrong at all.
        let mut data = vec![0u8; IO_CHUNK * 2 + BLOCK * 3];
        for (i, b) in data.iter_mut().enumerate() {
            let in_run = (i > IO_CHUNK - BLOCK && i < IO_CHUNK + BLOCK * 2)
                || (i > IO_CHUNK * 2 - 10 && i < IO_CHUNK * 2 + 10);
            if in_run {
                *b = (i % 251 + 1) as u8;
            }
        }
        round_trips("spanning", &data);
    }

    #[test]
    fn no_destination_is_an_error() {
        assert!(write_sparse_many(&mut Cursor::new(b"x"), &[]).is_err());
    }

    #[test]
    fn copying_a_file_onto_itself_is_refused() {
        // fs::copy truncates the source first, which has already eaten an
        // installed kernel once (`install-image --kernel ~/.nebula/...`).
        let src = scratch("self");
        std::fs::write(&src, b"important").unwrap();
        assert!(copy_sparse(&src, &src).is_err());
        assert_eq!(std::fs::read(&src).unwrap(), b"important");
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn overwrites_a_larger_existing_file() {
        // Upgrades write over the previous install; nothing of it may survive.
        let dst = scratch("overwrite");
        std::fs::write(&dst, vec![0xFFu8; BLOCK * 4]).unwrap();
        let data = vec![0x22u8; 100];
        write_sparse(&mut Cursor::new(&data), &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), data);
        let _ = std::fs::remove_file(&dst);
    }
}
