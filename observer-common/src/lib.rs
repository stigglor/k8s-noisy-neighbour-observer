//! Types shared by the eBPF programs and the agent. The layout is `#[repr(C)]`, because both
//! sides read the same bytes from `.rodata` and the BPF maps.

#![no_std]

/// The most histogram bounds the eBPF side accepts.
pub const MAX_BOUNDS: usize = 12;

/// Byte offsets of the kernel struct members the eBPF side reads. The agent resolves them from
/// the BTF of the running kernel.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Offsets {
    pub task_pid: u32,
    pub task_cgroups: u32,
    pub css_set_dfl_cgrp: u32,
    pub cgroup_kn: u32,
    pub kernfs_node_id: u32,
}

/// Upper bounds of the histogram bins in nanoseconds. Only the first `len` entries count.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub ns: [u64; MAX_BOUNDS],
    pub len: u64,
}

/// Run queue latency of one cgroup. `bins[i]` counts samples at or below bound `i` and above
/// bound `i - 1`, and `bins[len]` counts samples above the last bound.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RunqHist {
    pub bins: [u64; MAX_BOUNDS + 1],
    pub sum_ns: u64,
}

/// A task in the run queue. `culprit_cg` is the cgroup id of the task that preempted it, or 0
/// after a wakeup.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Enqueued {
    pub ts: u64,
    pub culprit_cg: u64,
}

/// A victim cgroup that waited and the culprit cgroup it waited behind. `culprit_cg` is
/// `IDLE_CG` when the CPU was idle before the victim ran.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pair {
    pub victim_cg: u64,
    pub culprit_cg: u64,
}

/// No cgroup has id 0.
pub const IDLE_CG: u64 = 0;

/// Slots of the `DROPPED` map, one for each map that can be full.
pub const DROPPED_RUNQ: u32 = 0;
pub const DROPPED_PAIRS: u32 = 1;

#[cfg(all(feature = "user", target_os = "linux"))]
mod pod {
    unsafe impl aya::Pod for super::Offsets {}
    unsafe impl aya::Pod for super::Bounds {}
    unsafe impl aya::Pod for super::RunqHist {}
    unsafe impl aya::Pod for super::Pair {}
}
