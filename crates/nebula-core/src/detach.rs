//! Spawning long-lived children that hold nothing belonging to the caller.
//!
//! `nebula up` spawns `nebulad` and then exits; the daemon deliberately
//! outlives the command. That is only safe if the daemon inherits *nothing*
//! from the CLI, and on Windows `std::process::Command` cannot promise that.
//! Rust's spawn calls `CreateProcessW` with `bInheritHandles = TRUE` — it has
//! to, that is how a child receives its std handles — and the flag is not
//! selective: **every** inheritable handle in the parent is duplicated into
//! the child, including handles the parent itself inherited.
//!
//! The concrete bug (#29): an embedder runs `nebula up` with
//! `.stderr(Stdio::piped())` and `.output()`. Rust makes that pipe's write end
//! inheritable so `nebula` can receive it; `nebula` then leaks it on into
//! `nebulad`. `nebulad` never writes to it — its own stdio is the null device
//! — but it holds it open, and EOF requires *every* write handle to be closed.
//! So `.output()` blocks until the daemon exits, i.e. forever, on a command
//! that already succeeded.
//!
//! `Stdio::null()` does not fix that: the leak is the inherit flag, not the
//! std handles. The fix is `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, an explicit
//! list naming the only handles the child may inherit (the three we open for
//! it), plus `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` so the daemon gets
//! no console and no Ctrl-C aimed at the caller's process group.
//!
//! Unix has no equivalent leak — Rust opens every fd `CLOEXEC`, and the three
//! std fds are replaced with `/dev/null` — but it has the same ownership
//! problem in the other direction: a daemon left in the caller's session dies
//! with the caller's terminal. So the Unix path calls `setsid`.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// Where a detached child's stdout/stderr go.
///
/// Deliberately not "any `Stdio`": a pipe here would recreate the very bug
/// this module exists to prevent, because nothing can close a write handle
/// held by a process that never exits.
pub enum Stdio {
    /// The platform null device, opened fresh for the child.
    Null,
    /// A file this process owns. The child gets its own copy of the handle;
    /// the original is closed when the spawn call returns.
    File(std::fs::File),
}

/// Builder for a child process that outlives this one.
pub struct Detached {
    program: PathBuf,
    args: Vec<OsString>,
    stdout: Stdio,
    stderr: Stdio,
}

impl Detached {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdout: Stdio::Null,
            stderr: Stdio::Null,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn stdout(mut self, stdout: Stdio) -> Self {
        self.stdout = stdout;
        self
    }

    pub fn stderr(mut self, stderr: Stdio) -> Self {
        self.stderr = stderr;
        self
    }

    /// Start the child. Dropping the returned handle does not kill it.
    pub fn spawn(self) -> io::Result<Child> {
        imp::spawn(self.program, self.args, self.stdout, self.stderr)
    }
}

/// A running detached child. Dropping this releases our reference to the
/// process without killing or reaping it — the child is meant to outlive us.
pub struct Child(imp::Child);

impl Child {
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// Has it exited yet? Never blocks.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }
}

/// Build a Windows command line from a program path and its arguments,
/// following the `CommandLineToArgvW` quoting rules that `msvcrt` and Rust's
/// own `std` use to take it apart again.
///
/// Lives outside the `cfg(windows)` module so its rules can be tested on any
/// host — the Windows CI job only builds, it does not run tests.
#[cfg_attr(not(windows), allow(dead_code))]
fn command_line(program: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    #[cfg(windows)]
    use std::os::windows::ffi::OsStrExt;

    // Only Windows can encode an OsStr as UTF-16 losslessly; elsewhere (tests)
    // the paths and args are valid UTF-8 by construction.
    #[cfg(windows)]
    let wide = |s: &std::ffi::OsStr| s.encode_wide().collect::<Vec<u16>>();
    #[cfg(not(windows))]
    let wide = |s: &std::ffi::OsStr| s.to_string_lossy().encode_utf16().collect::<Vec<u16>>();

    let mut cmd = Vec::new();
    // argv[0] is always quoted: a program path is far likelier to contain a
    // space (`C:\Program Files\...`) than not.
    append_arg(&mut cmd, &wide(program.as_os_str()), true)?;
    for arg in args {
        cmd.push(u16::from(b' '));
        append_arg(&mut cmd, &wide(arg.as_ref()), false)?;
    }
    cmd.push(0);
    Ok(cmd)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn append_arg(cmd: &mut Vec<u16>, arg: &[u16], force_quotes: bool) -> io::Result<()> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    let quote = force_quotes
        || arg.is_empty()
        || arg
            .iter()
            .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t'));
    if quote {
        cmd.push(QUOTE);
    }
    let mut backslashes = 0usize;
    for &c in arg {
        if c == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nul byte in process argument",
            ));
        }
        if c == BACKSLASH {
            backslashes += 1;
        } else {
            if c == QUOTE {
                // A run of backslashes before a quote is halved by the parser,
                // so double it and add one to escape the quote itself.
                cmd.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
            }
            backslashes = 0;
        }
        cmd.push(c);
    }
    if quote {
        // Same halving applies to the closing quote we are about to add.
        cmd.extend(std::iter::repeat_n(BACKSLASH, backslashes));
        cmd.push(QUOTE);
    }
    Ok(())
}

#[cfg(not(windows))]
mod imp {
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::ExitStatus;

    pub struct Child(std::process::Child);

    impl Child {
        pub fn id(&self) -> u32 {
            self.0.id()
        }
        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.0.try_wait()
        }
    }

    impl From<super::Stdio> for std::process::Stdio {
        fn from(s: super::Stdio) -> Self {
            match s {
                super::Stdio::Null => std::process::Stdio::null(),
                super::Stdio::File(f) => f.into(),
            }
        }
    }

    pub fn spawn(
        program: PathBuf,
        args: Vec<OsString>,
        stdout: super::Stdio,
        stderr: super::Stdio,
    ) -> io::Result<super::Child> {
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        // SAFETY: setsid is async-signal-safe, allocates nothing, and touches
        // no state shared with the forked-from parent. It cannot fail here —
        // it only fails for a process that already leads its process group,
        // and a just-forked child never does.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        Ok(super::Child(Child(cmd.spawn()?)))
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::ExitStatus;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, BOOL, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
        CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    };

    const TRUE: BOOL = 1;

    /// A handle we own and must close. Not `HANDLE` itself, so that every exit
    /// path out of `spawn` (including the `?`s) releases it.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The process handle, kept only so we can ask whether the child is still
    /// alive. Closing it does not affect the child.
    pub struct Child {
        process: OwnedHandle,
        pid: u32,
    }

    impl Child {
        pub fn id(&self) -> u32 {
            self.pid
        }

        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            match unsafe { WaitForSingleObject(self.process.0, 0) } {
                WAIT_TIMEOUT => Ok(None),
                WAIT_OBJECT_0 => {
                    let mut code: u32 = 0;
                    if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(Some(ExitStatus::from_raw(code)))
                }
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    fn wide_z(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Open the null device with an inheritable handle. Each std slot gets its
    /// own: the child may close one of them, and closing a shared handle would
    /// take the other two with it.
    fn open_nul(write: bool) -> io::Result<OwnedHandle> {
        let name = wide_z(std::ffi::OsStr::new("NUL"));
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: TRUE,
        };
        let access = if write {
            FILE_GENERIC_WRITE
        } else {
            FILE_GENERIC_READ
        };
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(h))
    }

    /// An inheritable copy of a file we own, so that marking it inheritable
    /// never widens the original — another thread spawning concurrently must
    /// not pick this file up.
    fn inheritable_dup(f: &std::fs::File) -> io::Result<OwnedHandle> {
        let mut dup: HANDLE = null_mut();
        let me = unsafe { GetCurrentProcess() };
        let ok = unsafe {
            DuplicateHandle(
                me,
                f.as_raw_handle() as HANDLE,
                me,
                &mut dup,
                0,
                TRUE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(dup))
    }

    fn child_handle(s: super::Stdio) -> io::Result<OwnedHandle> {
        match s {
            super::Stdio::Null => open_nul(true),
            super::Stdio::File(f) => inheritable_dup(&f),
        }
    }

    /// A `PROC_THREAD_ATTRIBUTE_LIST`, whose size is only known at runtime.
    /// Backed by `usize` cells because the list is pointer-aligned and a
    /// `Vec<u8>` is not.
    struct AttrList {
        buf: Vec<usize>,
        initialized: bool,
    }

    impl AttrList {
        fn with_handles(handles: &[HANDLE]) -> io::Result<Self> {
            let mut size: usize = 0;
            // First call always "fails" with ERROR_INSUFFICIENT_BUFFER; it is
            // how the required size is reported.
            unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
            if size == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut list = AttrList {
                buf: vec![0usize; size.div_ceil(size_of::<usize>())],
                initialized: false,
            };
            if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), 1, 0, &mut size) } == 0 {
                return Err(io::Error::last_os_error());
            }
            list.initialized = true;
            // This is the whole fix: the child inherits these handles and
            // nothing else, whatever else happens to be inheritable in us.
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    list.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                    null_mut(),
                    null(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(list)
        }

        fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buf.as_mut_ptr().cast()
        }
    }

    impl Drop for AttrList {
        fn drop(&mut self) {
            if self.initialized {
                unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
            }
        }
    }

    pub fn spawn(
        program: PathBuf,
        args: Vec<OsString>,
        stdout: super::Stdio,
        stderr: super::Stdio,
    ) -> io::Result<super::Child> {
        let app = wide_z(program.as_os_str());
        let mut cmdline = super::command_line(&program, &args)?;

        let stdin = open_nul(false)?;
        let stdout = child_handle(stdout)?;
        let stderr = child_handle(stderr)?;
        let handles = [stdin.0, stdout.0, stderr.0];
        let mut attrs = AttrList::with_handles(&handles)?;

        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin.0;
        si.StartupInfo.hStdOutput = stdout.0;
        si.StartupInfo.hStdError = stderr.0;
        si.lpAttributeList = attrs.as_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmdline.as_mut_ptr(),
                null(),
                null(),
                // Still TRUE — the child cannot receive std handles otherwise.
                // The attribute list above is what makes it selective.
                TRUE,
                EXTENDED_STARTUPINFO_PRESENT | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
                null(),
                null(),
                &si as *const STARTUPINFOEXW as *const STARTUPINFOW,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // The child has its own copies now; ours go away with the OwnedHandles.
        drop(OwnedHandle(pi.hThread));
        Ok(super::Child(Child {
            process: OwnedHandle(pi.hProcess),
            pid: pi.dwProcessId,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(program: &str, args: &[&str]) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let wide = command_line(Path::new(program), &args).unwrap();
        assert_eq!(wide.last(), Some(&0), "command line must be NUL-terminated");
        String::from_utf16(&wide[..wide.len() - 1]).unwrap()
    }

    #[test]
    fn program_is_always_quoted() {
        assert_eq!(
            rendered(r"C:\bin\nebulad.exe", &[]),
            r#""C:\bin\nebulad.exe""#
        );
        assert_eq!(
            rendered(r"C:\Program Files\Nebula\nebulad.exe", &[]),
            r#""C:\Program Files\Nebula\nebulad.exe""#
        );
    }

    #[test]
    fn plain_args_are_left_alone() {
        assert_eq!(
            rendered("nebulad", &["vz-worker", "--spec"]),
            r#""nebulad" vz-worker --spec"#
        );
    }

    #[test]
    fn args_with_whitespace_or_nothing_get_quoted() {
        assert_eq!(rendered("d", &["a b"]), r#""d" "a b""#);
        assert_eq!(rendered("d", &["a\tb"]), "\"d\" \"a\tb\"");
        assert_eq!(rendered("d", &[""]), r#""d" """#);
    }

    #[test]
    fn backslashes_only_double_where_a_quote_follows() {
        // A trailing run before the closing quote we add must be doubled, or
        // it would escape that quote instead of standing for itself.
        assert_eq!(rendered("d", &[r"C:\a dir\"]), r#""d" "C:\a dir\\""#);
        // No closing quote to protect: the run stays literal.
        assert_eq!(rendered("d", &[r"C:\dir\"]), r#""d" C:\dir\"#);
        // 2n+1 backslashes before an embedded quote.
        assert_eq!(rendered("d", &[r#"a\"b"#]), r#""d" a\\\"b"#);
    }

    #[test]
    fn nul_in_an_argument_is_rejected() {
        let args = vec![OsString::from("a\0b")];
        assert!(command_line(Path::new("d"), &args).is_err());
    }
}
