use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Serialize)]
pub struct ProcNode {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub children: Vec<Self>,
}

impl ProcNode {
    pub fn descendants(&self) -> usize {
        self.children.iter().map(|c| 1 + c.descendants()).sum()
    }

    pub fn total_cpu(&self) -> f32 {
        self.cpu_percent + self.children.iter().map(Self::total_cpu).sum::<f32>()
    }

    pub fn total_rss(&self) -> u64 {
        self.rss_bytes + self.children.iter().map(Self::total_rss).sum::<u64>()
    }
}

pub fn sample_tree(root: u32) -> Option<ProcNode> {
    let mut sys = System::new();
    let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250)));
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let procs = sys.processes();
    procs.get(&Pid::from_u32(root))?;
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in procs {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    fn build(pid: Pid, sys: &System, children: &HashMap<Pid, Vec<Pid>>, depth: usize) -> Option<ProcNode> {
        let p = sys.process(pid)?;
        let mut kids: Vec<ProcNode> = if depth < 32 {
            children.get(&pid).into_iter().flatten().filter_map(|c| build(*c, sys, children, depth + 1)).collect()
        } else {
            Vec::new()
        };
        kids.sort_by_key(|k| k.pid);
        Some(ProcNode {
            pid: pid.as_u32(),
            name: p.name().to_string_lossy().into_owned(),
            cpu_percent: p.cpu_usage(),
            rss_bytes: p.memory(),
            children: kids,
        })
    }
    build(Pid::from_u32(root), &sys, &children, 0)
}

pub fn pid_alive(pid: i64) -> bool {
    pid > 0 && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}
