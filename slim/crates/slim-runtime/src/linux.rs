//! Linux implementation: namespaces, cgroup2, pivot_root, pty, exec.
//!
//! Fork-safety rule: slimd is multithreaded, so between fork() and exec()
//! the child may ONLY make raw syscalls — no Rust allocation (malloc could
//! be locked by another thread). Everything the child needs (CStrings,
//! resolved uids, mount lists, pre-created directories) is prepared by the
//! parent BEFORE fork. Error reporting uses a CLOEXEC pipe; the error path
//! allocates, which is acceptable because the child is already doomed.

use crate::spec::*;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};

pub fn become_subreaper() {
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        // Containers must not die with slimd's terminal; ignore SIGPIPE
        // (we write to closed attach sockets all the time).
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn errno_err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

fn cstr(s: &str) -> CString {
    CString::new(s.as_bytes()).unwrap_or_else(|_| CString::new("?").unwrap())
}

fn cstr_path(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap_or_else(|_| CString::new("?").unwrap())
}

// ---------- prepared (pre-fork) plans ----------

struct MountStep {
    src: CString,
    target: CString,
    fstype: CString,
    flags: libc::c_ulong,
    data: CString,
}

struct DevNode {
    path: CString,
    mode: libc::mode_t,
    dev: libc::dev_t,
}

struct SymlinkStep {
    target: CString,
    link: CString,
}

/// Everything the child needs, allocated before fork.
struct ChildPlan {
    pre_pivot: Vec<MountStep>,
    rootfs: CString,
    pivot_old: CString,
    post_pivot: Vec<MountStep>,
    dev_nodes: Vec<DevNode>,
    symlinks: Vec<SymlinkStep>,
    readonly_root: bool,
    hostname: CString,
    cwd: CString,
    argv: Vec<CString>,
    envp: Vec<CString>,
    uid: libc::uid_t,
    gid: libc::gid_t,
    sgids: Vec<libc::gid_t>,
}

const TMPFS_FLAGS: libc::c_ulong = libc::MS_NOSUID;
const BIND: libc::c_ulong = libc::MS_BIND | libc::MS_REC;

fn build_plan(spec: &ContainerSpec) -> io::Result<ChildPlan> {
    let root = &spec.rootfs;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("rootfs not found: {}", root.display()),
        ));
    }

    let mut pre_pivot = Vec::new();
    // Re-mount everything private so container mounts never leak out, then
    // turn the rootfs into a mount point (pivot_root requires one).
    pre_pivot.push(MountStep {
        src: cstr("none"),
        target: cstr("/"),
        fstype: cstr(""),
        flags: libc::MS_REC | libc::MS_PRIVATE,
        data: cstr(""),
    });
    pre_pivot.push(MountStep {
        src: cstr_path(root),
        target: cstr_path(root),
        fstype: cstr(""),
        flags: BIND,
        data: cstr(""),
    });

    // Bind mounts: parent pre-creates targets (dir or empty file) inside the
    // merged rootfs so the child only needs mount(2).
    for b in &spec.binds {
        let rel = b.target.trim_start_matches('/');
        let tgt = root.join(rel);
        if b.source.is_dir() {
            std::fs::create_dir_all(&tgt)?;
        } else {
            if let Some(p) = tgt.parent() {
                std::fs::create_dir_all(p)?;
            }
            if !tgt.exists() {
                std::fs::File::create(&tgt)?;
            }
        }
        pre_pivot.push(MountStep {
            src: cstr_path(&b.source),
            target: cstr_path(&tgt),
            fstype: cstr(""),
            flags: BIND,
            data: cstr(""),
        });
        if b.read_only {
            pre_pivot.push(MountStep {
                src: cstr(""),
                target: cstr_path(&tgt),
                fstype: cstr(""),
                flags: libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                data: cstr(""),
            });
        }
    }

    let pivot_dir = root.join(".pivot");
    std::fs::create_dir_all(&pivot_dir)?;

    // Post-pivot mounts (paths are now container-absolute).
    let mut post_pivot = vec![
        MountStep {
            src: cstr("proc"),
            target: cstr("/proc"),
            fstype: cstr("proc"),
            flags: libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            data: cstr(""),
        },
        MountStep {
            src: cstr("tmpfs"),
            target: cstr("/dev"),
            fstype: cstr("tmpfs"),
            flags: TMPFS_FLAGS,
            data: cstr("mode=755,size=65536k"),
        },
        MountStep {
            src: cstr("devpts"),
            target: cstr("/dev/pts"),
            fstype: cstr("devpts"),
            flags: libc::MS_NOSUID | libc::MS_NOEXEC,
            data: cstr("newinstance,ptmxmode=0666,mode=0620,gid=5"),
        },
        MountStep {
            src: cstr("shm"),
            target: cstr("/dev/shm"),
            fstype: cstr("tmpfs"),
            flags: TMPFS_FLAGS | libc::MS_NODEV,
            data: cstr(&format!(
                "mode=1777,size={}",
                if spec.shm_size > 0 { spec.shm_size } else { 64 * 1024 * 1024 }
            )),
        },
        MountStep {
            src: cstr("mqueue"),
            target: cstr("/dev/mqueue"),
            fstype: cstr("mqueue"),
            flags: libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            data: cstr(""),
        },
        MountStep {
            src: cstr("sysfs"),
            target: cstr("/sys"),
            fstype: cstr("sysfs"),
            flags: libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RDONLY,
            data: cstr(""),
        },
    ];
    for (target, opts) in &spec.tmpfs {
        post_pivot.push(MountStep {
            src: cstr("tmpfs"),
            target: cstr(target),
            fstype: cstr("tmpfs"),
            flags: TMPFS_FLAGS,
            data: cstr(opts),
        });
    }

    // Ensure standard mount points exist in the image (some minimal images
    // lack /proc or /sys).
    for d in ["proc", "sys", "dev", "tmp"] {
        let _ = std::fs::create_dir_all(root.join(d));
    }

    let makedev = |maj: u32, min: u32| libc::makedev(maj, min);
    let dev_nodes = vec![
        DevNode { path: cstr("/dev/null"), mode: 0o666, dev: makedev(1, 3) },
        DevNode { path: cstr("/dev/zero"), mode: 0o666, dev: makedev(1, 5) },
        DevNode { path: cstr("/dev/full"), mode: 0o666, dev: makedev(1, 7) },
        DevNode { path: cstr("/dev/random"), mode: 0o666, dev: makedev(1, 8) },
        DevNode { path: cstr("/dev/urandom"), mode: 0o666, dev: makedev(1, 9) },
        DevNode { path: cstr("/dev/tty"), mode: 0o666, dev: makedev(5, 0) },
    ];
    let symlinks = vec![
        SymlinkStep { target: cstr("/proc/self/fd"), link: cstr("/dev/fd") },
        SymlinkStep { target: cstr("/proc/self/fd/0"), link: cstr("/dev/stdin") },
        SymlinkStep { target: cstr("/proc/self/fd/1"), link: cstr("/dev/stdout") },
        SymlinkStep { target: cstr("/proc/self/fd/2"), link: cstr("/dev/stderr") },
        SymlinkStep { target: cstr("pts/ptmx"), link: cstr("/dev/ptmx") },
    ];

    // User resolution against the image's passwd/group — done HERE so the
    // child never parses files.
    let (uid, gid, sgids, home) = resolve_user(root, &spec.user)?;

    // Working directory: docker creates it if missing.
    let cwd = if spec.cwd.is_empty() { "/".to_string() } else { spec.cwd.clone() };
    let _ = std::fs::create_dir_all(root.join(cwd.trim_start_matches('/')));

    // Environment: image/config env + required defaults.
    let mut env = spec.env.clone();
    let has = |k: &str, env: &[String]| env.iter().any(|e| e.starts_with(&format!("{k}=")));
    if !has("PATH", &env) {
        env.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    }
    if !has("HOME", &env) {
        env.push(format!("HOME={}", if home.is_empty() { "/root".into() } else { home }));
    }
    if !has("HOSTNAME", &env) && !spec.hostname.is_empty() {
        env.push(format!("HOSTNAME={}", spec.hostname));
    }
    if spec.tty && !has("TERM", &env) {
        env.push("TERM=xterm".into());
    }

    if spec.argv.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no command specified (empty Cmd and Entrypoint)",
        ));
    }

    Ok(ChildPlan {
        pre_pivot,
        rootfs: cstr_path(root),
        pivot_old: cstr_path(&pivot_dir),
        post_pivot,
        dev_nodes,
        symlinks,
        readonly_root: spec.readonly_rootfs,
        hostname: cstr(&spec.hostname),
        cwd: cstr(&cwd),
        argv: spec.argv.iter().map(|a| cstr(a)).collect(),
        envp: env.iter().map(|e| cstr(e)).collect(),
        uid,
        gid,
        sgids,
    })
}

/// Parse "user[:group]" against rootfs/etc/passwd + /etc/group.
/// Returns (uid, gid, supplementary gids, home).
fn resolve_user(root: &Path, user: &str) -> io::Result<(u32, u32, Vec<u32>, String)> {
    if user.is_empty() {
        return Ok((0, 0, vec![], "/root".into()));
    }
    let (u, g) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let passwd = std::fs::read_to_string(root.join("etc/passwd")).unwrap_or_default();
    let group = std::fs::read_to_string(root.join("etc/group")).unwrap_or_default();

    let mut uid: Option<u32> = u.parse().ok();
    let mut gid: Option<u32> = None;
    let mut home = String::new();
    let mut uname = u.to_string();
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 6 {
            continue;
        }
        let matches = f[0] == u || (uid.is_some() && f[2].parse::<u32>().ok() == uid);
        if matches {
            uname = f[0].to_string();
            uid = f[2].parse().ok();
            gid = f[3].parse().ok();
            home = f[5].to_string();
            break;
        }
    }
    let uid = uid.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("unable to find user {u}: no matching entries in passwd file"),
        )
    })?;
    if let Some(g) = g {
        gid = g.parse().ok().or_else(|| {
            group.lines().find_map(|line| {
                let f: Vec<&str> = line.split(':').collect();
                (f.len() >= 3 && f[0] == g).then(|| f[2].parse().ok()).flatten()
            })
        });
        if gid.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unable to find group {g}"),
            ));
        }
    }
    let gid = gid.unwrap_or(uid);
    // Supplementary groups: every group listing the user name.
    let sgids = group
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 4 && f[3].split(',').any(|m| m == uname) {
                f[2].parse().ok()
            } else {
                None
            }
        })
        .collect();
    Ok((uid, gid, sgids, home))
}

// ---------- stdio plumbing ----------

struct Stdio {
    /// fds the CHILD dups onto 0/1/2 (slave or pipe ends).
    child: [RawFd; 3],
    handle: Handle,
}

fn pipe2() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(errno_err("pipe2"));
    }
    Ok((fds[0], fds[1]))
}

fn setup_stdio(tty: bool, open_stdin: bool) -> io::Result<Stdio> {
    if tty {
        let mut master: RawFd = 0;
        let mut slave: RawFd = 0;
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        } != 0
        {
            return Err(errno_err("openpty"));
        }
        unsafe {
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok(Stdio {
            child: [slave, slave, slave],
            handle: Handle {
                pid: 0,
                pty_master: Some(unsafe { File::from_raw_fd(master) }),
                stdin: None,
                stdout: None,
                stderr: None,
            },
        })
    } else {
        let (out_r, out_w) = pipe2()?;
        let (err_r, err_w) = pipe2()?;
        let (in_r, in_w) = if open_stdin {
            let (r, w) = pipe2()?;
            (r, Some(w))
        } else {
            (-1, None)
        };
        Ok(Stdio {
            child: [in_r, out_w, err_w],
            handle: Handle {
                pid: 0,
                pty_master: None,
                stdin: in_w.map(|fd| unsafe { File::from_raw_fd(fd) }),
                stdout: Some(unsafe { File::from_raw_fd(out_r) }),
                stderr: Some(unsafe { File::from_raw_fd(err_r) }),
            },
        })
    }
}

/// Close the child-side fds in the parent after fork.
fn close_child_fds(stdio: &Stdio) {
    let mut seen = Vec::new();
    for fd in stdio.child {
        if fd >= 0 && !seen.contains(&fd) {
            unsafe { libc::close(fd) };
            seen.push(fd);
        }
    }
}

// ---------- the child-side syscall sequences (NO allocation) ----------

unsafe fn child_die(err_w: RawFd, msg: &[u8]) -> ! {
    let _ = libc::write(err_w, msg.as_ptr() as *const libc::c_void, msg.len());
    libc::_exit(127)
}

macro_rules! child_try {
    ($err_w:expr, $call:expr, $msg:expr) => {
        if $call != 0 {
            child_die($err_w, $msg)
        }
    };
}

unsafe fn apply_mount(step: &MountStep) -> libc::c_int {
    libc::mount(
        step.src.as_ptr(),
        step.target.as_ptr(),
        if step.fstype.as_bytes().is_empty() {
            std::ptr::null()
        } else {
            step.fstype.as_ptr()
        },
        step.flags,
        if step.data.as_bytes().is_empty() {
            std::ptr::null()
        } else {
            step.data.as_ptr() as *const libc::c_void
        },
    )
}

unsafe fn child_stdio(child: &[RawFd; 3], tty: bool, err_w: RawFd) {
    if libc::setsid() < 0 {
        child_die(err_w, b"setsid failed");
    }
    if tty && libc::ioctl(child[1], libc::TIOCSCTTY as _, 0) != 0 {
        child_die(err_w, b"TIOCSCTTY failed");
    }
    for (i, fd) in child.iter().enumerate() {
        if *fd >= 0 {
            if libc::dup2(*fd, i as RawFd) < 0 {
                child_die(err_w, b"dup2 failed");
            }
        } else if i == 0 {
            // No stdin requested: 0 reads EOF via /dev/null opened pre-fork
            // is not possible (no alloc); closing fd 0 is acceptable.
            libc::close(0);
        }
    }
    for fd in child.iter() {
        if *fd > 2 {
            libc::close(*fd);
        }
    }
}

unsafe fn child_set_ids(plan: &ChildPlan, err_w: RawFd) {
    if !plan.sgids.is_empty() {
        let _ = libc::setgroups(plan.sgids.len(), plan.sgids.as_ptr());
    } else {
        let _ = libc::setgroups(0, std::ptr::null());
    }
    child_try!(err_w, libc::setgid(plan.gid), b"setgid failed");
    child_try!(err_w, libc::setuid(plan.uid), b"setuid failed");
}

unsafe fn child_enter_rootfs(plan: &ChildPlan, err_w: RawFd) {
    for m in &plan.pre_pivot {
        if apply_mount(m) != 0 {
            child_die(err_w, b"bind/private mount failed");
        }
    }
    child_try!(
        err_w,
        libc::syscall(libc::SYS_pivot_root, plan.rootfs.as_ptr(), plan.pivot_old.as_ptr())
            as libc::c_int,
        b"pivot_root failed"
    );
    child_try!(err_w, libc::chdir(c"/".as_ptr()), b"chdir / failed");
    child_try!(
        err_w,
        libc::umount2(c"/.pivot".as_ptr(), libc::MNT_DETACH),
        b"umount old root failed"
    );
    let _ = libc::rmdir(c"/.pivot".as_ptr());
    for m in &plan.post_pivot {
        // /sys and mqueue are best-effort (tier-2 nested containers may
        // refuse); proc and /dev are not.
        let rc = apply_mount(m);
        if rc != 0 {
            let target = m.target.as_bytes();
            if target == b"/proc" || target == b"/dev" {
                child_die(err_w, b"mount /proc or /dev failed");
            }
        }
    }
    for d in &plan.dev_nodes {
        if libc::mknod(d.path.as_ptr(), libc::S_IFCHR | d.mode, d.dev) != 0 {
            // Nested (tier-2) runs can't mknod: bind the host node instead.
            let fd = libc::open(d.path.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o666);
            if fd >= 0 {
                libc::close(fd);
            }
            let _ = libc::mount(d.path.as_ptr(), d.path.as_ptr(), std::ptr::null(), BIND, std::ptr::null());
        }
    }
    for s in &plan.symlinks {
        let _ = libc::symlink(s.target.as_ptr(), s.link.as_ptr());
    }
    if plan.readonly_root {
        let _ = libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            std::ptr::null(),
        );
    }
}

unsafe fn child_exec(plan: &ChildPlan, err_w: RawFd) -> ! {
    child_try!(err_w, libc::chdir(plan.cwd.as_ptr()), b"chdir to workdir failed");
    child_set_ids(plan, err_w);
    let mut argv: Vec<*const libc::c_char> = plan.argv.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null()); // pre-sized Vec: single alloc, tolerable
    let mut envp: Vec<*const libc::c_char> = plan.envp.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    // execvpe semantics: PATH lookup from the NEW env.
    let path_entry = plan
        .envp
        .iter()
        .find(|e| e.as_bytes().starts_with(b"PATH="))
        .map(|e| &e.as_bytes()[5..]);
    exec_with_path(&plan.argv[0], &argv, &envp, path_entry);
    child_die(err_w, b"exec failed: executable not found or not runnable")
}

/// execve with manual PATH search (musl's execvpe uses the CALLER's environ).
unsafe fn exec_with_path(
    arg0: &CString,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
    path: Option<&[u8]>,
) {
    let a0 = arg0.as_bytes();
    if a0.contains(&b'/') {
        libc::execve(arg0.as_ptr(), argv.as_ptr(), envp.as_ptr());
        return;
    }
    let path = path.unwrap_or(b"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    let mut buf = [0u8; 512];
    for dir in path.split(|b| *b == b':') {
        if dir.is_empty() || dir.len() + a0.len() + 2 > buf.len() {
            continue;
        }
        buf[..dir.len()].copy_from_slice(dir);
        buf[dir.len()] = b'/';
        buf[dir.len() + 1..dir.len() + 1 + a0.len()].copy_from_slice(a0);
        buf[dir.len() + 1 + a0.len()] = 0;
        libc::execve(buf.as_ptr() as *const libc::c_char, argv.as_ptr(), envp.as_ptr());
    }
}

// ---------- start / exec entry points ----------

pub fn start_container(spec: &ContainerSpec) -> io::Result<Handle> {
    let plan = build_plan(spec)?;
    let mut stdio = setup_stdio(spec.tty, spec.open_stdin)?;
    let (err_r, err_w) = pipe2()?;
    let (pid_r, pid_w) = pipe2()?;
    let (gate_r, gate_w) = pipe2()?;

    let netns_fd: RawFd = match &spec.netns {
        Some(p) => {
            let f = cstr_path(p);
            let fd = unsafe { libc::open(f.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(errno_err("open netns"));
            }
            fd
        }
        None => -1,
    };

    let inter = unsafe { libc::fork() };
    if inter < 0 {
        return Err(errno_err("fork"));
    }
    if inter == 0 {
        // ---------- intermediate: single-threaded, may unshare ----------
        unsafe {
            child_try!(
                err_w,
                libc::unshare(
                    libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC
                ),
                b"unshare failed (kernel without namespace support?)"
            );
            if netns_fd >= 0 {
                child_try!(err_w, libc::setns(netns_fd, libc::CLONE_NEWNET), b"setns netns failed");
            } else {
                child_try!(err_w, libc::unshare(libc::CLONE_NEWNET), b"unshare netns failed");
            }
            let child = libc::fork();
            if child < 0 {
                child_die(err_w, b"second fork failed");
            }
            if child == 0 {
                // ---------- grandchild: PID 1 of the container ----------
                child_stdio(&stdio.child, spec.tty, err_w);
                if !plan.hostname.as_bytes().is_empty() {
                    let _ = libc::sethostname(
                        plan.hostname.as_ptr(),
                        plan.hostname.as_bytes().len(),
                    );
                }
                child_enter_rootfs(&plan, err_w);
                // Wait for the parent to finish cgroup placement.
                let mut b = [0u8; 1];
                let _ = libc::read(gate_r, b.as_mut_ptr() as *mut libc::c_void, 1);
                child_exec(&plan, err_w)
            }
            let buf = (child as i32).to_ne_bytes();
            let _ = libc::write(pid_w, buf.as_ptr() as *const libc::c_void, 4);
            libc::_exit(0)
        }
    }
    // ---------- parent ----------
    unsafe {
        libc::close(err_w);
        libc::close(pid_w);
        libc::close(gate_r);
        if netns_fd >= 0 {
            libc::close(netns_fd);
        }
    }
    close_child_fds(&stdio);
    // Reap the intermediate immediately.
    let mut st = 0;
    unsafe { libc::waitpid(inter, &mut st, 0) };

    let mut pid_buf = [0u8; 4];
    let mut pid_file = unsafe { File::from_raw_fd(pid_r) };
    let n = pid_file.read(&mut pid_buf).unwrap_or(0);
    if n < 4 {
        // Intermediate died before the second fork: collect the error.
        return Err(read_child_error(err_r, "container setup failed before start"));
    }
    let pid = i32::from_ne_bytes(pid_buf);

    cgroup_setup(&spec.id, spec, pid);

    // Open the gate: child proceeds to exec.
    unsafe {
        let one = [1u8];
        libc::write(gate_w, one.as_ptr() as *const libc::c_void, 1);
        libc::close(gate_w);
    }

    // err pipe: EOF (CLOEXEC fired on exec) = success; data = failure.
    let err = read_child_error_opt(err_r);
    if let Some(e) = err {
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, 0) }; // reap the failed child
        remove_cgroup(&spec.id);
        return Err(e);
    }
    stdio.handle.pid = pid;
    Ok(stdio.handle)
}

pub fn exec_in_container(target_pid: i32, spec: &ExecSpec) -> io::Result<Handle> {
    exec_in_container_cg(target_pid, spec, None)
}

/// `cgroup_id`: place the exec process into the container's cgroup.
pub fn exec_in_container_cg(
    target_pid: i32,
    spec: &ExecSpec,
    cgroup_id: Option<&str>,
) -> io::Result<Handle> {
    // The exec'ed process sees the container's mount ns, so user resolution
    // must read /proc/<pid>/root (slimd CAN see through it as root).
    let proc_root = PathBuf::from(format!("/proc/{target_pid}/root"));
    let (uid, gid, sgids, home) = resolve_user(&proc_root, &spec.user)?;

    let mut env = spec.env.clone();
    let has = |k: &str, env: &[String]| env.iter().any(|e| e.starts_with(&format!("{k}=")));
    if !has("PATH", &env) {
        env.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    }
    if !has("HOME", &env) {
        env.push(format!("HOME={}", if home.is_empty() { "/root".into() } else { home }));
    }
    if spec.tty && !has("TERM", &env) {
        env.push("TERM=xterm".into());
    }
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "exec: empty command"));
    }

    let plan = ChildPlan {
        pre_pivot: vec![],
        rootfs: cstr("/"),
        pivot_old: cstr("/"),
        post_pivot: vec![],
        dev_nodes: vec![],
        symlinks: vec![],
        readonly_root: false,
        hostname: cstr(""),
        cwd: cstr(if spec.cwd.is_empty() { "/" } else { &spec.cwd }),
        argv: spec.argv.iter().map(|a| cstr(a)).collect(),
        envp: env.iter().map(|e| cstr(e)).collect(),
        uid,
        gid,
        sgids,
    };

    // Open all ns fds before entering any of them.
    let ns_names = ["ipc", "uts", "net", "pid", "mnt"]; // mnt LAST
    let mut ns_fds: Vec<RawFd> = Vec::new();
    for name in ns_names {
        let p = cstr(&format!("/proc/{target_pid}/ns/{name}"));
        let fd = unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            for fd in &ns_fds {
                unsafe { libc::close(*fd) };
            }
            return Err(errno_err("open container namespace (container not running?)"));
        }
        ns_fds.push(fd);
    }

    let mut stdio = setup_stdio(spec.tty, spec.open_stdin)?;
    let (err_r, err_w) = pipe2()?;
    let (pid_r, pid_w) = pipe2()?;

    let inter = unsafe { libc::fork() };
    if inter < 0 {
        return Err(errno_err("fork"));
    }
    if inter == 0 {
        unsafe {
            for fd in &ns_fds {
                if libc::setns(*fd, 0) != 0 {
                    child_die(err_w, b"setns failed");
                }
            }
            let child = libc::fork();
            if child < 0 {
                child_die(err_w, b"second fork failed");
            }
            if child == 0 {
                child_stdio(&stdio.child, spec.tty, err_w);
                child_exec(&plan, err_w)
            }
            let buf = (child as i32).to_ne_bytes();
            let _ = libc::write(pid_w, buf.as_ptr() as *const libc::c_void, 4);
            libc::_exit(0)
        }
    }
    unsafe {
        libc::close(err_w);
        libc::close(pid_w);
        for fd in &ns_fds {
            libc::close(*fd);
        }
    }
    close_child_fds(&stdio);
    let mut st = 0;
    unsafe { libc::waitpid(inter, &mut st, 0) };

    let mut pid_buf = [0u8; 4];
    let mut pid_file = unsafe { File::from_raw_fd(pid_r) };
    let n = pid_file.read(&mut pid_buf).unwrap_or(0);
    if n < 4 {
        return Err(read_child_error(err_r, "exec setup failed"));
    }
    let pid = i32::from_ne_bytes(pid_buf);

    if let Some(id) = cgroup_id {
        let _ = std::fs::write(format!("{CGROUP_ROOT}/{id}/cgroup.procs"), pid.to_string());
    }

    if let Some(e) = read_child_error_opt(err_r) {
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, 0) };
        return Err(e);
    }
    stdio.handle.pid = pid;
    Ok(stdio.handle)
}

fn read_child_error_opt(err_r: RawFd) -> Option<io::Error> {
    let mut f = unsafe { File::from_raw_fd(err_r) };
    let mut msg = String::new();
    let _ = f.read_to_string(&mut msg);
    if msg.is_empty() {
        None
    } else {
        Some(io::Error::other(msg))
    }
}

fn read_child_error(err_r: RawFd, fallback: &str) -> io::Error {
    read_child_error_opt(err_r).unwrap_or_else(|| io::Error::other(fallback.to_string()))
}

// ---------- lifecycle ----------

pub fn signal_pid(pid: i32, signal: i32) -> io::Result<()> {
    if unsafe { libc::kill(pid, signal) } != 0 {
        return Err(errno_err("kill"));
    }
    Ok(())
}

/// SIGKILL the whole container via cgroup.kill (kernel ≥5.14 — we ship the
/// kernel, so this is guaranteed in the vessel; tier-2 falls back to kill).
pub fn kill_cgroup(id: &str) -> io::Result<()> {
    std::fs::write(format!("{CGROUP_ROOT}/{id}/cgroup.kill"), "1")
}

pub fn remove_cgroup(id: &str) {
    let _ = std::fs::remove_dir(format!("{CGROUP_ROOT}/{id}"));
}

/// Blocking wait; call from a dedicated waiter thread.
pub fn wait_pid(pid: i32) -> io::Result<ExitStatus> {
    let mut st = 0;
    let r = unsafe { libc::waitpid(pid, &mut st, 0) };
    if r < 0 {
        return Err(errno_err("waitpid"));
    }
    let code = if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else if libc::WIFSIGNALED(st) {
        128 + libc::WTERMSIG(st)
    } else {
        255
    };
    Ok(ExitStatus { code, oom_killed: false })
}

pub fn read_oom(id: &str) -> bool {
    std::fs::read_to_string(format!("{CGROUP_ROOT}/{id}/memory.events"))
        .unwrap_or_default()
        .lines()
        .any(|l| {
            l.strip_prefix("oom_kill ")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|n| n > 0)
                .unwrap_or(false)
        })
}

pub fn read_stats(id: &str, _pid: i32) -> CgroupStats {
    let base = format!("{CGROUP_ROOT}/{id}");
    let read_u64 = |f: &str| -> u64 {
        std::fs::read_to_string(format!("{base}/{f}"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    let cpu_usage = std::fs::read_to_string(format!("{base}/cpu.stat"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0);
    let mem_max = std::fs::read_to_string(format!("{base}/memory.max")).unwrap_or_default();
    CgroupStats {
        memory_current: read_u64("memory.current"),
        memory_limit: mem_max.trim().parse().unwrap_or(0), // "max" → 0 → caller uses host total
        cpu_usage_usec: cpu_usage,
        pids_current: read_u64("pids.current"),
    }
}

pub fn resize_pty(fd: RawFd, w: u16, h: u16) {
    let ws = libc::winsize {
        ws_row: h,
        ws_col: w,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws);
    }
}

pub fn parse_signal(s: &str) -> i32 {
    if let Ok(n) = s.parse::<i32>() {
        return n;
    }
    match s.trim_start_matches("SIG") {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "WINCH" => libc::SIGWINCH,
        _ => libc::SIGTERM,
    }
}

// ---------- cgroup setup ----------

fn cgroup_setup(id: &str, spec: &ContainerSpec, pid: i32) {
    // All best-effort: tier-2 (nested) runs may not own the cgroup tree.
    let dir = format!("{CGROUP_ROOT}/{id}");
    let _ = std::fs::create_dir_all(&dir);
    for ctl in ["/sys/fs/cgroup/cgroup.subtree_control", &format!("{CGROUP_ROOT}/cgroup.subtree_control")] {
        let _ = std::fs::write(ctl, "+memory +pids +cpu");
    }
    if spec.memory > 0 {
        let _ = std::fs::write(format!("{dir}/memory.max"), spec.memory.to_string());
        let swap = if spec.memory_swap < 0 {
            "max".to_string()
        } else if spec.memory_swap > spec.memory {
            (spec.memory_swap - spec.memory).to_string()
        } else {
            // docker default: total swap+mem = 2*mem → swap.max = mem
            spec.memory.to_string()
        };
        let _ = std::fs::write(format!("{dir}/memory.swap.max"), swap);
    }
    if spec.nano_cpus > 0 {
        let quota = spec.nano_cpus / 10_000; // per 100ms period
        let _ = std::fs::write(format!("{dir}/cpu.max"), format!("{quota} 100000"));
    }
    if spec.cpu_shares > 1 {
        let weight = 1 + ((spec.cpu_shares - 2) * 9999) / 262142;
        let _ = std::fs::write(format!("{dir}/cpu.weight"), weight.clamp(1, 10000).to_string());
    }
    if spec.pids_limit > 0 {
        let _ = std::fs::write(format!("{dir}/pids.max"), spec.pids_limit.to_string());
    }
    let _ = std::fs::write(format!("{dir}/cgroup.procs"), pid.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_resolution() {
        let dir = std::env::temp_dir().join(format!("slimrt-user-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\nguest:x:405:100:guest:/dev/null:/sbin/nologin\n",
        )
        .unwrap();
        std::fs::write(dir.join("etc/group"), "wheel:x:10:root,guest\nusers:x:100:\n").unwrap();
        let (uid, gid, sgids, _) = resolve_user(&dir, "guest").unwrap();
        assert_eq!((uid, gid), (405, 100));
        assert_eq!(sgids, vec![10]);
        let (uid, gid, _, _) = resolve_user(&dir, "405:wheel").unwrap();
        assert_eq!((uid, gid), (405, 10));
        assert!(resolve_user(&dir, "nosuch").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
