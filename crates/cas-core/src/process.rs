use crate::{CasError, ProcessInfo, Result};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, Signal, System};

fn lower_basename(value: &OsStr) -> String {
    std::path::Path::new(value)
        .file_name()
        .unwrap_or(value)
        .to_string_lossy()
        .to_ascii_lowercase()
}

fn is_codex_name(name: &str) -> bool {
    matches!(name, "codex" | "codex-cli" | "openai-codex")
}

fn process_is_codex(process: &sysinfo::Process) -> bool {
    if is_codex_name(&lower_basename(process.name())) {
        return true;
    }
    if let Some(exe) = process.exe()
        && let Some(name) = exe.file_name()
        && is_codex_name(&lower_basename(name))
    {
        return true;
    }
    if let Some(first) = process.cmd().first() {
        return is_codex_name(&lower_basename(first));
    }
    false
}

fn is_process_leader(process: &sysinfo::Process) -> bool {
    process.thread_kind().is_none()
}

fn process_is_live(process: &sysinfo::Process) -> bool {
    !matches!(
        process.status(),
        ProcessStatus::Zombie | ProcessStatus::Dead
    )
}

fn process_is_chatgpt_desktop(process: &sysinfo::Process) -> bool {
    if lower_basename(process.name()) == "chatgpt" {
        return true;
    }
    process
        .exe()
        .and_then(|exe| exe.file_name())
        .is_some_and(|name| lower_basename(name) == "chatgpt")
}

fn refresh(system: &mut System) {
    system.refresh_processes(ProcessesToUpdate::All, true);
}

fn target_pids(system: &System) -> HashSet<Pid> {
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            (is_process_leader(process) && process_is_live(process) && process_is_codex(process))
                .then_some(*pid)
        })
        .collect()
}

fn desktop_owner(system: &System, pid: Pid) -> Option<Pid> {
    let mut current = system.process(pid)?.parent();
    let mut owner = None;
    let mut hops = 0usize;
    while let Some(parent_pid) = current {
        let Some(parent) = system.process(parent_pid) else {
            break;
        };
        if process_is_chatgpt_desktop(parent)
            && is_process_leader(parent)
            && process_is_live(parent)
        {
            owner = Some(parent_pid);
        }
        current = parent.parent();
        hops += 1;
        if hops >= 32 {
            break;
        }
    }
    owner
}

fn termination_targets(system: &System, codex_pids: &HashSet<Pid>) -> HashSet<Pid> {
    codex_pids
        .iter()
        .map(|pid| desktop_owner(system, *pid).unwrap_or(*pid))
        .collect()
}

fn info_for(system: &System, pids: &HashSet<Pid>) -> Vec<ProcessInfo> {
    let mut out: Vec<_> = pids
        .iter()
        .filter_map(|pid| {
            system.process(*pid).map(|p| ProcessInfo {
                pid: pid.as_u32(),
                name: p.name().to_string_lossy().into_owned(),
            })
        })
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

pub fn list_codex_processes() -> Vec<ProcessInfo> {
    let mut system = System::new_all();
    refresh(&mut system);
    let pids = target_pids(&system);
    info_for(&system, &pids)
}

fn remaining_processes(system: &mut System, tracked: &HashSet<Pid>) -> HashSet<Pid> {
    refresh(system);
    let mut remaining: HashSet<Pid> = tracked
        .iter()
        .copied()
        .filter(|pid| system.process(*pid).is_some_and(process_is_live))
        .collect();
    remaining.extend(target_pids(system));
    remaining
}

fn wait_empty(system: &mut System, tracked: &HashSet<Pid>, timeout: Duration) -> HashSet<Pid> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = remaining_processes(system, tracked);
        if remaining.is_empty() || Instant::now() >= deadline {
            return remaining;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

pub fn terminate_all_codex() -> Result<Vec<ProcessInfo>> {
    let mut system = System::new_all();
    refresh(&mut system);
    let initial = target_pids(&system);
    if initial.is_empty() {
        return Ok(Vec::new());
    }
    let initial_info = info_for(&system, &initial);
    let initial_targets = termination_targets(&system, &initial);

    for pid in &initial_targets {
        if let Some(process) = system.process(*pid) {
            let _ = process
                .kill_with(Signal::Term)
                .unwrap_or_else(|| process.kill());
        }
    }

    let mut remaining = wait_empty(&mut system, &initial_targets, Duration::from_secs(3));
    if !remaining.is_empty() {
        refresh(&mut system);
        let current_codex = target_pids(&system);
        let mut escalation = termination_targets(&system, &current_codex);
        escalation.extend(
            initial_targets
                .iter()
                .copied()
                .filter(|pid| system.process(*pid).is_some()),
        );
        for pid in &escalation {
            if let Some(process) = system.process(*pid) {
                let _ = process.kill();
            }
        }
        remaining = wait_empty(&mut system, &escalation, Duration::from_secs(5));
    }

    if !remaining.is_empty() {
        refresh(&mut system);
        return Err(CasError::CodexStillRunning(info_for(&system, &remaining)));
    }

    Ok(initial_info)
}

pub fn ensure_no_codex_processes() -> Result<()> {
    let processes = list_codex_processes();
    if processes.is_empty() {
        Ok(())
    } else {
        Err(CasError::CodexStillRunning(processes))
    }
}

#[cfg(test)]
mod tests {
    use super::is_codex_name;

    #[test]
    fn identifies_codex_names() {
        assert!(is_codex_name("codex"));
        assert!(is_codex_name("codex-cli"));
        assert!(!is_codex_name("codex-mcp"));
        assert!(!is_codex_name("codex-app-server"));
        assert!(!is_codex_name("cas"));
        assert!(!is_codex_name("code"));
    }
}
