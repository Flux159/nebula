//! Deterministic newc-format cpio (initramfs) builder.
//!
//! Used to assemble guest boot images on the host without external tools.
//! The Linux kernel populates rootfs from this archive before exec'ing /init,
//! so a static init binary plus a /dev/console node is a complete bootable guest.

const MAGIC: &[u8; 6] = b"070701";
const TRAILER: &str = "TRAILER!!!";

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;

struct Entry {
    name: String,
    mode: u32,
    rdev: (u32, u32),
    data: Vec<u8>,
}

#[derive(Default)]
pub struct InitramfsBuilder {
    entries: Vec<Entry>,
}

impl InitramfsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dir(mut self, name: &str, perm: u32) -> Self {
        self.entries.push(Entry {
            name: norm(name),
            mode: S_IFDIR | (perm & 0o7777),
            rdev: (0, 0),
            data: Vec::new(),
        });
        self
    }

    pub fn file(mut self, name: &str, data: Vec<u8>, perm: u32) -> Self {
        self.entries.push(Entry {
            name: norm(name),
            mode: S_IFREG | (perm & 0o7777),
            rdev: (0, 0),
            data,
        });
        self
    }

    pub fn symlink(mut self, name: &str, target: &str) -> Self {
        self.entries.push(Entry {
            name: norm(name),
            mode: S_IFLNK | 0o777,
            rdev: (0, 0),
            data: target.as_bytes().to_vec(),
        });
        self
    }

    pub fn char_dev(mut self, name: &str, major: u32, minor: u32, perm: u32) -> Self {
        self.entries.push(Entry {
            name: norm(name),
            mode: S_IFCHR | (perm & 0o7777),
            rdev: (major, minor),
            data: Vec::new(),
        });
        self
    }

    pub fn build(self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            // ino must be unique per entry; everything else is fixed for determinism.
            write_entry(&mut out, i as u32 + 1, e);
        }
        write_entry(
            &mut out,
            0,
            &Entry {
                name: TRAILER.into(),
                mode: 0,
                rdev: (0, 0),
                data: Vec::new(),
            },
        );
        out
    }
}

fn norm(name: &str) -> String {
    name.trim_start_matches('/').to_string()
}

fn write_entry(out: &mut Vec<u8>, ino: u32, e: &Entry) {
    let name_bytes = e.name.as_bytes();
    let namesize = name_bytes.len() + 1; // include NUL
    let nlink: u32 = if e.mode & S_IFDIR != 0 { 2 } else { 1 };

    out.extend_from_slice(MAGIC);
    for v in [
        ino,                 // ino
        e.mode,              // mode
        0,                   // uid
        0,                   // gid
        nlink,               // nlink
        0,                   // mtime
        e.data.len() as u32, // filesize
        0,                   // devmajor
        0,                   // devminor
        e.rdev.0,            // rdevmajor
        e.rdev.1,            // rdevminor
        namesize as u32,     // namesize
        0,                   // check (always 0 for newc)
    ] {
        out.extend_from_slice(format!("{v:08X}").as_bytes());
    }
    out.extend_from_slice(name_bytes);
    out.push(0);
    pad4(out);
    out.extend_from_slice(&e.data);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal newc parser used to verify round-trip correctness.
    fn parse(archive: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut entries = Vec::new();
        let mut off = 0usize;
        loop {
            assert_eq!(&archive[off..off + 6], MAGIC, "bad magic at {off}");
            let field = |i: usize| -> u32 {
                let s =
                    std::str::from_utf8(&archive[off + 6 + i * 8..off + 6 + (i + 1) * 8]).unwrap();
                u32::from_str_radix(s, 16).unwrap()
            };
            let mode = field(1);
            let filesize = field(6) as usize;
            let namesize = field(11) as usize;
            let name_start = off + 110;
            let name =
                String::from_utf8(archive[name_start..name_start + namesize - 1].to_vec()).unwrap();
            let mut data_start = name_start + namesize;
            data_start += (4 - data_start % 4) % 4;
            let data = archive[data_start..data_start + filesize].to_vec();
            off = data_start + filesize;
            off += (4 - off % 4) % 4;
            if name == TRAILER {
                return entries;
            }
            entries.push((name, mode, data));
        }
    }

    #[test]
    fn builds_parseable_archive_with_all_entry_types() {
        let archive = InitramfsBuilder::new()
            .dir("/dev", 0o755)
            .char_dev("/dev/console", 5, 1, 0o600)
            .file("/init", b"#!/bin/sh\n".to_vec(), 0o755)
            .symlink("/linuxrc", "/init")
            .build();

        let entries = parse(&archive);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].0, "dev");
        assert_eq!(entries[0].1 & 0o170000, S_IFDIR);
        assert_eq!(entries[1].0, "dev/console");
        assert_eq!(entries[1].1 & 0o170000, S_IFCHR);
        assert_eq!(entries[2].0, "init");
        assert_eq!(entries[2].2, b"#!/bin/sh\n");
        assert_eq!(entries[3].0, "linuxrc");
        assert_eq!(entries[3].2, b"/init");
        // Whole archive is 4-byte aligned.
        assert_eq!(archive.len() % 4, 0);
    }

    #[test]
    fn deterministic_output() {
        let build = || {
            InitramfsBuilder::new()
                .dir("/dev", 0o755)
                .file("/init", vec![1, 2, 3], 0o755)
                .build()
        };
        assert_eq!(build(), build());
    }
}
