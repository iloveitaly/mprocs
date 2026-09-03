//! Killing a runner without trusting a bare pid.
//!
//! A pid alone is not an identity: the runner can exit and the OS can
//! recycle its pid at any time. The runtime record therefore carries
//! the process start time the OS assigned to the runner — a recycled
//! pid always gets a new start time, so `(pid, start time)` names one
//! process instance. The value is OS-specific and only ever compared
//! for equality.
//!
//! `kill_verified` kills only that instance. On Linux a pidfd makes it
//! race-free; on Windows the process handle does; on macOS the instant
//! between the verify and the `kill` remains (there is no handle to
//! signal through).
//!
//! `kill_session` reaps everything a killed runner leaves behind: the
//! spawned runner is its own session leader (`setsid` before exec, see
//! `spawn.rs`), every task inherits that session, and session
//! membership cannot be forged from outside the tree.

use anyhow::bail;

/// The OS-assigned start time of the process instance `pid` names now.
pub fn process_start_time(pid: u32) -> anyhow::Result<u64> {
  #[cfg(target_os = "linux")]
  {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    match parse_proc_stat(&stat) {
      Some((_state, start_time)) => Ok(start_time),
      None => bail!("unreadable /proc/{pid}/stat"),
    }
  }
  #[cfg(target_os = "macos")]
  {
    Ok(bsdinfo(pid)?.1)
  }
  #[cfg(windows)]
  {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
      OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle =
      unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|err| anyhow::anyhow!("failed to open pid {pid}: {err}"))?;
    let time = handle_start_time(handle);
    let _ = unsafe { CloseHandle(handle) };
    time
  }
}

/// Kill `pid`, but only while it is still the recorded process
/// instance. Returns false when that instance is already gone; never
/// signals a process whose identity does not match.
pub fn kill_verified(pid: u32, start_time: u64) -> anyhow::Result<bool> {
  #[cfg(target_os = "linux")]
  {
    use rustix::process::{Pid, PidfdFlags, pidfd_open, pidfd_send_signal};
    let Some(raw) = Pid::from_raw(pid as i32) else {
      bail!("bad pid {pid}");
    };
    let fd = match pidfd_open(raw, PidfdFlags::empty()) {
      Ok(fd) => fd,
      Err(rustix::io::Errno::SRCH) => return Ok(false),
      // Pre-5.3 kernel: fall back to the verify-then-kill instant.
      Err(rustix::io::Errno::NOSYS) => {
        return kill_verified_by_pid(pid, start_time);
      }
      Err(err) => bail!("failed to open pid {pid}: {err}"),
    };
    // The pid may have been recycled before the open; a matching start
    // time proves the current holder is the recorded instance, and it
    // held the pid at open time too (processes do not come back), so
    // the fd names it. The signal then follows the fd, not the pid.
    if process_start_time(pid).ok() != Some(start_time) {
      return Ok(false);
    }
    match pidfd_send_signal(&fd, rustix::process::Signal::KILL) {
      Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(true),
      Err(err) => bail!("failed to kill pid {pid}: {err}"),
    }
  }
  #[cfg(target_os = "macos")]
  {
    kill_verified_by_pid(pid, start_time)
  }
  #[cfg(windows)]
  {
    use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows::Win32::System::Threading::{
      OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
      TerminateProcess,
    };
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
    let handle = match unsafe { OpenProcess(access, false, pid) } {
      Ok(handle) => handle,
      // The pid no longer names a process.
      Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
        return Ok(false);
      }
      Err(err) => bail!("failed to open pid {pid}: {err}"),
    };
    // The handle pins one process object, so verifying and terminating
    // through the same handle cannot hit a recycled pid.
    let result = (|| {
      if handle_start_time(handle).ok() != Some(start_time) {
        return Ok(false);
      }
      unsafe { TerminateProcess(handle, 1) }
        .map_err(|err| anyhow::anyhow!("failed to kill pid {pid}: {err}"))?;
      Ok(true)
    })();
    let _ = unsafe { CloseHandle(handle) };
    result
  }
}

#[cfg(unix)]
fn kill_verified_by_pid(pid: u32, start_time: u64) -> anyhow::Result<bool> {
  if process_start_time(pid).ok() != Some(start_time) {
    return Ok(false);
  }
  if unsafe { libc::kill(pid as i32, libc::SIGKILL) } < 0 {
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
      return Ok(false);
    }
    bail!("failed to kill pid {pid}: {err}");
  }
  Ok(true)
}

/// SIGKILL every live process in session `sid` except the caller, in
/// passes, until a pass finds no member that has not already been
/// signaled (tasks may fork while being reaped). An accepted SIGKILL
/// is already fatal — the process may take a moment to die but cannot
/// survive — so signaled members need no further waiting. Returns how
/// many unsignaled members are left if the passes run out: a task
/// forking faster than it can be killed.
#[cfg(unix)]
pub fn kill_session(sid: u32) -> anyhow::Result<u32> {
  let me = std::process::id();
  let mut signaled = std::collections::HashSet::new();
  for _ in 0..8 {
    let mut new = 0;
    for pid in session_members(sid, me)? {
      if signaled.insert(pid) {
        new += 1;
        let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
      }
    }
    if new == 0 {
      return Ok(0);
    }
  }
  let left = session_members(sid, me)?
    .into_iter()
    .filter(|pid| !signaled.contains(pid))
    .count();
  Ok(left as u32)
}

/// Live (non-zombie) members of session `sid`, excluding `skip`.
#[cfg(unix)]
fn session_members(sid: u32, skip: u32) -> anyhow::Result<Vec<u32>> {
  let mut members = Vec::new();
  for pid in list_pids()? {
    if pid == skip {
      continue;
    }
    if unsafe { libc::getsid(pid as i32) } != sid as i32 {
      continue;
    }
    // A zombie holds its session but cannot run; killing it does
    // nothing and it only disappears when its parent reaps it.
    if is_zombie(pid) {
      continue;
    }
    members.push(pid);
  }
  Ok(members)
}

#[cfg(target_os = "linux")]
fn list_pids() -> anyhow::Result<Vec<u32>> {
  let mut pids = Vec::new();
  for entry in std::fs::read_dir("/proc")? {
    if let Some(pid) = entry?
      .file_name()
      .to_str()
      .and_then(|name| name.parse::<u32>().ok())
    {
      pids.push(pid);
    }
  }
  Ok(pids)
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
  match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
    Ok(stat) => {
      parse_proc_stat(&stat).is_some_and(|(state, _start_time)| state == "Z")
    }
    Err(_) => false,
  }
}

/// `/proc/<pid>/stat` fields after the comm (which may contain spaces
/// and parens): state is field 3, starttime field 22.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<(&str, u64)> {
  let rest = stat.rsplit_once(')')?.1;
  let mut fields = rest.split_whitespace();
  let state = fields.next()?;
  let start_time = fields.nth(18)?.parse().ok()?;
  Some((state, start_time))
}

#[cfg(target_os = "macos")]
fn list_pids() -> anyhow::Result<Vec<u32>> {
  let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
  if needed < 0 {
    bail!(
      "proc_listallpids failed: {}",
      std::io::Error::last_os_error()
    );
  }
  // Headroom for processes started since the size call.
  let mut pids = vec![0 as libc::pid_t; needed as usize + 64];
  let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
  let count =
    unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
  if count < 0 {
    bail!(
      "proc_listallpids failed: {}",
      std::io::Error::last_os_error()
    );
  }
  pids.truncate((count as usize).min(pids.len()));
  Ok(
    pids
      .into_iter()
      .filter(|&pid| pid > 0)
      .map(|p| p as u32)
      .collect(),
  )
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: u32) -> bool {
  match bsdinfo(pid) {
    Ok((status, _start_time)) => status == libc::SZOMB,
    // A same-session pid with no process info is a zombie pending its
    // parent's reap: proc_pidinfo refuses zombies.
    Err(_) => true,
  }
}

/// (`pbi_status`, start time in microseconds) for `pid`.
#[cfg(target_os = "macos")]
fn bsdinfo(pid: u32) -> anyhow::Result<(u32, u64)> {
  let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
  let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
  let got = unsafe {
    libc::proc_pidinfo(
      pid as i32,
      libc::PROC_PIDTBSDINFO,
      0,
      (&mut info as *mut libc::proc_bsdinfo).cast(),
      size,
    )
  };
  if got != size {
    bail!(
      "no process info for pid {pid}: {}",
      std::io::Error::last_os_error()
    );
  }
  Ok((
    info.pbi_status,
    info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
  ))
}

#[cfg(windows)]
fn handle_start_time(
  handle: windows::Win32::Foundation::HANDLE,
) -> anyhow::Result<u64> {
  use windows::Win32::Foundation::FILETIME;
  use windows::Win32::System::Threading::GetProcessTimes;
  let mut creation = FILETIME::default();
  let mut exit = FILETIME::default();
  let mut kernel = FILETIME::default();
  let mut user = FILETIME::default();
  unsafe {
    GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
  }
  .map_err(|err| anyhow::anyhow!("failed to read process times: {err}"))?;
  Ok(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn own_start_time_is_stable() {
    let pid = std::process::id();
    let a = process_start_time(pid).unwrap();
    let b = process_start_time(pid).unwrap();
    assert_eq!(a, b);
    assert!(a > 0);
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn proc_stat_parses_awkward_comm() {
    let stat =
      "123 (a b) c) R 1 123 123 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 4242 0 0";
    assert_eq!(parse_proc_stat(stat), Some(("R", 4242)));
  }

  #[cfg(unix)]
  #[test]
  fn kill_verified_refuses_a_mismatched_identity() {
    let mut child = std::process::Command::new("sleep")
      .arg("30")
      .spawn()
      .unwrap();
    let pid = child.id();
    let start_time = process_start_time(pid).unwrap();

    assert!(!kill_verified(pid, start_time + 1).unwrap());
    assert!(child.try_wait().unwrap().is_none(), "child was killed");

    assert!(kill_verified(pid, start_time).unwrap());
    let status = child.wait().unwrap();
    assert!(!status.success());
  }

  #[cfg(unix)]
  #[test]
  fn kill_session_reaps_the_whole_tree() {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("sh");
    // The echoed byte proves the pre_exec setsid ran and the
    // grandchild was forked before the sweep starts.
    cmd.args(["-c", "sleep 30 & echo r; exec sleep 30"]);
    cmd.stdout(std::process::Stdio::piped());
    unsafe {
      cmd.pre_exec(|| {
        if libc::setsid() < 0 {
          return Err(std::io::Error::last_os_error());
        }
        Ok(())
      });
    }
    let mut child = cmd.spawn().unwrap();
    let mut byte = [0u8; 1];
    child.stdout.take().unwrap().read_exact(&mut byte).unwrap();
    let sid = child.id();
    assert_eq!(unsafe { libc::getsid(sid as i32) }, sid as i32);

    assert_eq!(kill_session(sid).unwrap(), 0);
    let status = child.wait().unwrap();
    assert!(!status.success());
  }
}
