//! Recorder liveness: is the process that started a recording still running?
//!
//! A session in `recording` status is only stale when the `hh` process that
//! recorded it is gone. A process id alone cannot answer that — PIDs are
//! reused, so a crashed recorder's PID may later belong to an unrelated
//! process — so the recorder's identity is the pair *(pid, start time)*
//! (migration 0008). A recording is considered alive while a process with that
//! pid exists **and** its start time matches what was recorded.
//!
//! The start time is an opaque, platform-specific token that is only ever
//! compared for equality on the same machine:
//!
//! - Linux: field 22 of `/proc/<pid>/stat` (clock ticks since boot).
//! - macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` start time, in microseconds since
//!   the Unix epoch.
//! - Windows: `GetProcessTimes` creation time (a `FILETIME`, 100 ns ticks).
//!
//! Every probe fails **safe**: when liveness cannot be determined (an
//! unsupported platform, `/proc` not mounted, permission denied, an unexpected
//! error) the answer is [`Liveness::Unknown`], and callers must treat that as
//! "possibly alive" — a session wrongly left `recording` is recoverable, a
//! wrongly `interrupted` one is not.

/// The identity of a running process: its pid plus its start-time token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    /// The OS process id.
    pub pid: u32,
    /// The platform start-time token, or `None` when it could not be read.
    pub start_time: Option<i64>,
}

impl ProcessIdentity {
    /// The identity of the calling process.
    pub(crate) fn current() -> Self {
        let pid = std::process::id();
        let start_time = match probe(pid) {
            Probe::Running(start) => start,
            Probe::Gone | Probe::Unknown => None,
        };
        Self { pid, start_time }
    }
}

/// Whether a recorded process is still the one running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// A process with the recorded pid and start time is running.
    Alive,
    /// No process has the recorded pid, or the pid now belongs to a process
    /// with a different start time (PID reuse), or it is a zombie.
    Gone,
    /// Liveness could not be determined. Treat as possibly alive.
    Unknown,
}

/// What the OS says about one pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// A live (non-zombie) process holds the pid; its start-time token, if
    /// readable.
    Running(Option<i64>),
    /// No live process holds the pid.
    Gone,
    /// The OS could not be asked, or did not answer definitively.
    Unknown,
}

/// Decide whether the recorder identified by `(pid, recorded_start)` is still
/// running. See the module docs for the fail-safe contract.
pub(crate) fn check(pid: u32, recorded_start: Option<i64>) -> Liveness {
    classify(probe(pid), recorded_start)
}

/// Pure decision table behind [`check`], split out so it is unit-testable on
/// every platform.
fn classify(probe: Probe, recorded_start: Option<i64>) -> Liveness {
    match (probe, recorded_start) {
        (Probe::Running(Some(now)), Some(then)) if now == then => Liveness::Alive,
        // No live process holds the pid; or it exists but belongs to a
        // different process — the recorder died and its pid was reused.
        (Probe::Gone, _) | (Probe::Running(Some(_)), Some(_)) => Liveness::Gone,
        // The pid exists but one side of the start-time comparison is missing,
        // so identity cannot be confirmed either way, or the OS could not be
        // asked at all: fail safe.
        (Probe::Running(_) | Probe::Unknown, _) => Liveness::Unknown,
    }
}

#[cfg(target_os = "linux")]
fn probe(pid: u32) -> Probe {
    use std::io::ErrorKind;
    // Without a readable procfs every pid would look absent; refuse to call
    // anything gone in that case.
    if std::fs::metadata("/proc/self/stat").is_err() {
        return Probe::Unknown;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_proc_stat(&stat),
        Err(e) if e.kind() == ErrorKind::NotFound => Probe::Gone,
        Err(_) => Probe::Unknown,
    }
}

/// Parse a Linux `/proc/<pid>/stat` line into a [`Probe`]. The command name
/// (field 2) is parenthesised and may itself contain spaces or `)`, so fields
/// are counted from the *last* `)`: the next field is the state (field 3) and
/// the start time is field 22. Malformed input yields [`Probe::Unknown`].
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(stat: &str) -> Probe {
    let Some(close) = stat.rfind(')') else {
        return Probe::Unknown;
    };
    let mut fields = stat[close + 1..].split_whitespace();
    let Some(state) = fields.next() else {
        return Probe::Unknown;
    };
    // Z = zombie, X = dead: the process has exited, it just has not been
    // reaped yet. Its recording loop is not running.
    if state == "Z" || state == "X" {
        return Probe::Gone;
    }
    // `fields` is now positioned at field 4; field 22 is 18 further on.
    match fields.nth(18).and_then(|s| s.parse::<i64>().ok()) {
        Some(ticks) => Probe::Running(Some(ticks)),
        None => Probe::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn probe(pid: u32) -> Probe {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return Probe::Unknown;
    };
    if pid <= 0 {
        // `kill(0 | negative, …)` addresses process groups; never probe them.
        return Probe::Unknown;
    }
    let Ok(size) = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return Probe::Unknown;
    };
    // SAFETY: `proc_bsdinfo` is a plain C struct of integers and byte arrays,
    // for which all-zero bytes is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer pointer and `size` describe `info` exactly; the call
    // writes at most `size` bytes into it and keeps no reference after return.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written == size {
        if info.pbi_status == libc::SZOMB {
            return Probe::Gone;
        }
        let micros = i64::try_from(info.pbi_start_tvsec)
            .ok()
            .and_then(|s| s.checked_mul(1_000_000))
            .zip(i64::try_from(info.pbi_start_tvusec).ok())
            .and_then(|(s, us)| s.checked_add(us));
        return Probe::Running(micros);
    }
    // `proc_pidinfo` failed; ask the kernel directly whether the pid exists.
    // Signal 0 performs only the existence/permission check.
    // SAFETY: `kill` with signal 0 sends nothing; `pid` is positive, so it
    // addresses exactly one process.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return Probe::Running(None);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Probe::Gone,
        // EPERM: it exists but belongs to someone else.
        Some(libc::EPERM) => Probe::Running(None),
        _ => Probe::Unknown,
    }
}

#[cfg(windows)]
fn probe(pid: u32) -> Probe {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE,
    };
    // SAFETY: plain FFI call with no pointer arguments; a null return is
    // handled below.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        // SAFETY: reads the calling thread's last-error value; no arguments.
        let err = unsafe { GetLastError() };
        // ERROR_INVALID_PARAMETER is what OpenProcess reports for a pid no
        // process holds. Anything else (e.g. ERROR_ACCESS_DENIED) means the
        // process may well exist.
        return if err == ERROR_INVALID_PARAMETER {
            Probe::Gone
        } else {
            Probe::Unknown
        };
    }
    // An exited process stays openable while anyone holds a handle to it, so
    // check whether it has terminated (its handle is signalled) first.
    // SAFETY: `handle` is a valid process handle opened with SYNCHRONIZE.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    let result = if wait == WAIT_OBJECT_0 {
        Probe::Gone
    } else if wait == WAIT_TIMEOUT {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: `handle` was opened with PROCESS_QUERY_LIMITED_INFORMATION;
        // the four out-pointers reference live, writable FILETIMEs.
        let ok = unsafe {
            GetProcessTimes(
                handle,
                &raw mut created,
                &raw mut exited,
                &raw mut kernel,
                &raw mut user,
            )
        };
        if ok == 0 {
            Probe::Running(None)
        } else {
            let ticks =
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            Probe::Running(i64::try_from(ticks).ok())
        }
    } else {
        Probe::Unknown
    };
    // SAFETY: `handle` is valid and closed exactly once.
    unsafe { CloseHandle(handle) };
    result
}

/// Unsupported platform: liveness cannot be determined, so every recording
/// is treated as possibly alive (fail safe).
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn probe(_pid: u32) -> Probe {
    Probe::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_decision_table() {
        assert_eq!(classify(Probe::Gone, Some(5)), Liveness::Gone);
        assert_eq!(classify(Probe::Gone, None), Liveness::Gone);
        assert_eq!(classify(Probe::Running(Some(5)), Some(5)), Liveness::Alive);
        assert_eq!(classify(Probe::Running(Some(6)), Some(5)), Liveness::Gone);
        assert_eq!(classify(Probe::Running(None), Some(5)), Liveness::Unknown);
        assert_eq!(classify(Probe::Running(Some(5)), None), Liveness::Unknown);
        assert_eq!(classify(Probe::Unknown, Some(5)), Liveness::Unknown);
        assert_eq!(classify(Probe::Unknown, None), Liveness::Unknown);
    }

    #[test]
    fn parse_proc_stat_reads_start_time_and_state() {
        // Field 22 (start time) is 4242; the command name contains ") (".
        let line =
            "123 (we ird) (name) S 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 4242 1000 50";
        assert_eq!(parse_proc_stat(line), Probe::Running(Some(4242)));
        let zombie = "123 (x) Z 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 4242 0 0";
        assert_eq!(parse_proc_stat(zombie), Probe::Gone);
    }

    #[test]
    fn parse_proc_stat_never_panics_on_garbage() {
        for line in [
            "",
            ")",
            "1 (x)",
            "1 (x) S",
            "1 (x) S 1 2",
            "1 (x) S a b c d e f g h i j k l m n o p q r s",
        ] {
            assert_eq!(parse_proc_stat(line), Probe::Unknown, "{line:?}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn current_process_is_alive() {
        let me = ProcessIdentity::current();
        assert!(me.start_time.is_some(), "own start time must be readable");
        assert_eq!(check(me.pid, me.start_time), Liveness::Alive);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn same_pid_with_different_start_time_is_gone() {
        let me = ProcessIdentity::current();
        let start = me.start_time.expect("own start time");
        assert_eq!(check(me.pid, Some(start + 1)), Liveness::Gone);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn exited_child_is_gone() {
        // A child that blocks on stdin until we close it, so it is
        // definitely running (not already a zombie) when first probed.
        #[cfg(unix)]
        let mut cmd = std::process::Command::new("cat");
        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("findstr");
            c.arg("x");
            c
        };
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        let start = match probe(pid) {
            Probe::Running(s) => s,
            other => panic!("live child probed as {other:?}"),
        };
        drop(child.stdin.take());
        child.wait().expect("reap child");
        // Reaped: the pid is free (or, if the OS already reused it, held by a
        // process with a different start time). Either way, gone.
        assert_eq!(check(pid, start), Liveness::Gone);
    }
}
