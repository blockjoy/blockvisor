use babel_api::engine::PosixSignal;
use eyre::{anyhow, Result};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use sysinfo::{
    Disk, DiskExt, Pid, Process, ProcessExt, ProcessRefreshKind, Signal, System, SystemExt,
};

const PROCESS_CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub fn get_ip_address(ifa_name: &str) -> Result<String> {
    let ifas = local_ip_address::list_afinet_netifas()?;
    let (_, ip) = ifas
        .into_iter()
        .find(|(name, ipaddr)| name == ifa_name && ipaddr.is_ipv4())
        .ok_or_else(|| anyhow!("interface {ifa_name} not found"))?;
    Ok(ip.to_string())
}

pub async fn gracefully_terminate_process(pid: Pid, timeout: Duration) -> bool {
    let mut sys = System::new();
    if !sys.refresh_process_specifics(pid, ProcessRefreshKind::new()) {
        return true;
    }
    if let Some(proc) = sys.process(pid) {
        proc.kill_with(Signal::Term);
        let now = std::time::Instant::now();
        while is_process_running(pid) {
            if now.elapsed() < timeout {
                tokio::time::sleep(Duration::from_secs(1)).await
            } else {
                return false;
            }
        }
    }
    true
}

pub fn is_process_running(pid: Pid) -> bool {
    let mut sys = System::new();
    sys.refresh_process_specifics(pid, ProcessRefreshKind::new())
        .then(|| sys.process(pid).map(|proc| proc.status()))
        .flatten()
        .map_or(false, |status| status != sysinfo::ProcessStatus::Zombie)
}

/// Kill all processes that match `cmd` and passed `args`.
pub fn kill_all_processes(cmd: &str, args: &[&str], timeout: Duration, signal: PosixSignal) {
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();

    let procs = find_processes(cmd, args, ps);
    let now = Instant::now();
    for (_, proc) in procs {
        kill_process_tree(proc, ps, now, timeout, into_sysinfo_signal(signal));
    }
}

/// Kill process and all its descendents.
fn kill_process_tree(
    proc: &Process,
    ps: &HashMap<Pid, Process>,
    now: Instant,
    timeout: Duration,
    signal: Signal,
) {
    // Better to kill parent first, since it may implement some child restart mechanism.
    // Try to interrupt the process, and kill it after timeout in case it has not finished.
    proc.kill_with(signal);
    while is_process_running(proc.pid()) {
        if now.elapsed() > timeout {
            proc.kill();
            proc.wait();
            break;
        }
        std::thread::sleep(PROCESS_CHECK_INTERVAL)
    }
    let children = ps.iter().filter(|(_, p)| p.parent() == Some(proc.pid()));
    for (_, child) in children {
        kill_process_tree(child, ps, now, timeout, signal);
    }
}

fn into_sysinfo_signal(posix: PosixSignal) -> Signal {
    match posix {
        PosixSignal::SIGABRT => Signal::Abort,
        PosixSignal::SIGALRM => Signal::Alarm,
        PosixSignal::SIGBUS => Signal::Bus,
        PosixSignal::SIGCHLD => Signal::Child,
        PosixSignal::SIGCLD => Signal::Child,
        PosixSignal::SIGCONT => Signal::Continue,
        PosixSignal::SIGEMT => Signal::Trap,
        PosixSignal::SIGFPE => Signal::FloatingPointException,
        PosixSignal::SIGHUP => Signal::Hangup,
        PosixSignal::SIGILL => Signal::Illegal,
        PosixSignal::SIGINFO => Signal::Power,
        PosixSignal::SIGINT => Signal::Interrupt,
        PosixSignal::SIGIO => Signal::IO,
        PosixSignal::SIGIOT => Signal::IOT,
        PosixSignal::SIGKILL => Signal::Kill,
        PosixSignal::SIGPIPE => Signal::Pipe,
        PosixSignal::SIGPOLL => Signal::Poll,
        PosixSignal::SIGPROF => Signal::Profiling,
        PosixSignal::SIGPWR => Signal::Power,
        PosixSignal::SIGQUIT => Signal::Quit,
        PosixSignal::SIGSEGV => Signal::Segv,
        PosixSignal::SIGSTOP => Signal::Stop,
        PosixSignal::SIGTSTP => Signal::TSTP,
        PosixSignal::SIGSYS => Signal::Sys,
        PosixSignal::SIGTERM => Signal::Term,
        PosixSignal::SIGTRAP => Signal::Trap,
        PosixSignal::SIGTTIN => Signal::TTIN,
        PosixSignal::SIGTTOU => Signal::TTOU,
        PosixSignal::SIGUNUSED => Signal::Sys,
        PosixSignal::SIGURG => Signal::Urgent,
        PosixSignal::SIGUSR1 => Signal::User1,
        PosixSignal::SIGUSR2 => Signal::User2,
        PosixSignal::SIGVTALRM => Signal::VirtualAlarm,
        PosixSignal::SIGXCPU => Signal::XCPU,
        PosixSignal::SIGXFSZ => Signal::XFSZ,
        PosixSignal::SIGWINCH => Signal::Winch,
    }
}

/// Find all processes that match `cmd` and passed `args`.
pub fn find_processes<'a>(
    cmd: &'a str,
    args: &'a [&'a str],
    ps: &'a HashMap<Pid, Process>,
) -> impl Iterator<Item = (&'a Pid, &'a Process)> {
    ps.iter().filter(move |(_, process)| {
        let proc_call = process
            .cmd()
            .iter()
            .map(|item| item.as_str())
            .collect::<Vec<_>>();
        if let Some(proc_cmd) = proc_call.first() {
            // first element is cmd, rest are arguments
            (cmd == *proc_cmd && *args == proc_call[1..])
                // if not a binary, but a script (with shebang) is executed,
                // then the process looks like: /bin/sh ./lalala.sh,
                // so first element is shebang, second is cmd, rest are arguments
                || (proc_call.len() > 1 && cmd == proc_call[1] && *args == proc_call[2..])
        } else {
            false
        }
    })
}

/// Find drive that depth of canonical mount point path is the biggest and at the same time
/// given `path` starts with it.
/// May return `None` if can't find such, but in worst case it should return `/` disk.
pub fn find_disk_by_path<'a>(sys: &'a System, path: &Path) -> Option<&'a Disk> {
    sys.disks()
        .iter()
        .max_by(|a, b| {
            match (
                a.mount_point().canonicalize(),
                b.mount_point().canonicalize(),
            ) {
                (Ok(a_mount_point), Ok(b_mount_point)) => {
                    match (
                        path.starts_with(&a_mount_point),
                        path.starts_with(&b_mount_point),
                    ) {
                        (true, true) => a_mount_point
                            .ancestors()
                            .count()
                            .cmp(&b_mount_point.ancestors().count()),
                        (false, true) => Ordering::Less,
                        (true, false) => Ordering::Greater,
                        (false, false) => Ordering::Equal,
                    }
                }
                (Err(_), Ok(_)) => Ordering::Less,
                (Ok(_), Err(_)) => Ordering::Greater,
                (Err(_), Err(_)) => Ordering::Equal,
            }
        })
        .and_then(|disk| {
            let mount_point = disk.mount_point().canonicalize().ok()?;
            if path.starts_with(mount_point) {
                Some(disk)
            } else {
                None
            }
        })
}

/// Get available disk space for drive on which given path reside.
pub fn available_disk_space_by_path(path: &Path) -> Result<u64> {
    let mut sys = System::new_all();
    sys.refresh_all();
    find_disk_by_path(&sys, path)
        .map(|disk| disk.available_space())
        .ok_or_else(|| anyhow!("Cannot get available disk space"))
}
