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

#[cfg(all(feature = "user", target_os = "linux"))]
mod pod {
    unsafe impl aya::Pod for super::Offsets {}
    unsafe impl aya::Pod for super::Bounds {}
    unsafe impl aya::Pod for super::RunqHist {}
}
