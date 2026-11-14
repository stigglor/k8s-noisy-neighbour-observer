//! Run queue latency per cgroup. A wakeup stores the enqueue time of a task, and `sched_switch`
//! adds `now - enqueue time` of the incoming task to the histogram of its cgroup. The same wait
//! goes to the pair of that cgroup and its culprit, see "Culprit attribution" in `CLAUDE.md`.
//!
//! `OFFSETS` holds kernel struct offsets from the agent. See "Kernel struct offsets" in
//! `CLAUDE.md`.

#![no_std]
#![no_main]

use aya_ebpf::helpers::{bpf_get_current_cgroup_id, bpf_get_current_pid_tgid, bpf_ktime_get_ns};
use aya_ebpf::macros::{btf_tracepoint, map};
use aya_ebpf::maps::{LruHashMap, PerCpuArray, PerCpuHashMap};
use aya_ebpf::programs::BtfTracePointContext;
use aya_ebpf::Global;
use observer_common::{
    Bounds, Enqueued, Offsets, Pair, RunqHist, DROPPED_PAIRS, DROPPED_RUNQ, MAX_BOUNDS,
};

const TASK_RUNNING: u32 = 0;

#[no_mangle]
static OFFSETS: Global<Offsets> = Global::new(Offsets {
    task_pid: 0,
    task_cgroups: 0,
    css_set_dfl_cgrp: 0,
    cgroup_kn: 0,
    kernfs_node_id: 0,
});

#[no_mangle]
static BOUNDS: Global<Bounds> = Global::new(Bounds {
    ns: [0; MAX_BOUNDS],
    len: 0,
});

/// Enqueue time and culprit by thread id.
#[map]
static ENQUEUED: LruHashMap<u32, Enqueued> = LruHashMap::with_max_entries(65536, 0);

/// Not an LRU, because the common LRU of a per CPU map evicts live entries well below
/// `max_entries` on nodes with many CPUs. The agent deletes idle cgroups instead, and sets
/// `max_entries` at load from `NNO_MAX_CGROUPS`.
#[map]
static RUNQ: PerCpuHashMap<u64, RunqHist> = PerCpuHashMap::with_max_entries(1024, 0);

/// Wait in nanoseconds by victim and culprit cgroup. A plain hash like `RUNQ`, sized at load
/// from `NNO_MAX_PAIRS`.
#[map]
static PREEMPT_WAIT: PerCpuHashMap<Pair, u64> = PerCpuHashMap::with_max_entries(16384, 0);

/// Samples lost because `RUNQ` or `PREEMPT_WAIT` was full, by the `DROPPED_*` slot.
#[map]
static DROPPED: PerCpuArray<u64> = PerCpuArray::with_max_entries(2, 0);

/// The verifier accepts a direct load through a BTF pointer, because aya freezes `.rodata` and
/// the offset is then a known constant. Faults read as zero.
#[inline(always)]
unsafe fn read_at<T>(base: *const u8, offset: u32) -> Option<T> {
    if base.is_null() {
        return None;
    }
    Some(core::ptr::read_volatile(
        base.add(offset as usize) as *const T
    ))
}

#[inline(always)]
fn task_pid(task: *const u8, offsets: &Offsets) -> u32 {
    unsafe { read_at::<i32>(task, offsets.task_pid) }.unwrap_or(0) as u32
}

/// `task->cgroups->dfl_cgrp->kn->id`, the inode of the cgroup v2 directory.
#[inline(always)]
fn task_cgroup_id(task: *const u8, offsets: &Offsets) -> Option<u64> {
    unsafe {
        let cset = read_at::<*const u8>(task, offsets.task_cgroups)?;
        let cgrp = read_at::<*const u8>(cset, offsets.css_set_dfl_cgrp)?;
        let kn = read_at::<*const u8>(cgrp, offsets.cgroup_kn)?;
        read_at::<u64>(kn, offsets.kernfs_node_id)
    }
}

#[inline(always)]
fn enqueue(task: *const u8, ts: u64, culprit_cg: u64) {
    let pid = task_pid(task, &OFFSETS.load());
    if pid != 0 {
        let _ = ENQUEUED.insert(&pid, &Enqueued { ts, culprit_cg }, 0);
    }
}

#[inline(always)]
fn drop_sample(slot: u32) {
    if let Some(dropped) = DROPPED.get_ptr_mut(slot) {
        unsafe { *dropped += 1 };
    }
}

#[inline(always)]
fn bin_of(latency_ns: u64, bounds: &Bounds) -> usize {
    let mut bin = MAX_BOUNDS;
    for i in 0..MAX_BOUNDS {
        if i as u64 >= bounds.len {
            bin = i;
            break;
        }
        if latency_ns <= bounds.ns[i] {
            bin = i;
            break;
        }
    }
    bin
}

#[inline(always)]
fn record(cgroup_id: u64, latency_ns: u64) {
    let bin = bin_of(latency_ns, &BOUNDS.load());
    match RUNQ.get_ptr_mut(&cgroup_id) {
        Some(hist) => unsafe {
            if let Some(count) = (*hist).bins.get_mut(bin) {
                *count += 1;
            }
            (*hist).sum_ns += latency_ns;
        },
        None => {
            let mut hist = RunqHist {
                bins: [0; MAX_BOUNDS + 1],
                sum_ns: latency_ns,
            };
            if let Some(count) = hist.bins.get_mut(bin) {
                *count = 1;
            }
            if RUNQ.insert(&cgroup_id, &hist, 0).is_err() {
                drop_sample(DROPPED_RUNQ);
            }
        }
    }
}

#[inline(always)]
fn blame(pair: &Pair, wait_ns: u64) {
    match PREEMPT_WAIT.get_ptr_mut(pair) {
        Some(total) => unsafe { *total += wait_ns },
        None => {
            if PREEMPT_WAIT.insert(pair, &wait_ns, 0).is_err() {
                drop_sample(DROPPED_PAIRS);
            }
        }
    }
}

#[btf_tracepoint(function = "sched_wakeup")]
pub fn sched_wakeup(ctx: BtfTracePointContext) -> i32 {
    enqueue(ctx.arg(0), unsafe { bpf_ktime_get_ns() }, 0);
    0
}

#[btf_tracepoint(function = "sched_wakeup_new")]
pub fn sched_wakeup_new(ctx: BtfTracePointContext) -> i32 {
    enqueue(ctx.arg(0), unsafe { bpf_ktime_get_ns() }, 0);
    0
}

/// `TP_PROTO(bool preempt, struct task_struct *prev, struct task_struct *next,
/// unsigned int prev_state)`. A preempted `prev` stays queued whatever its state, and a `prev`
/// that is still runnable waits in the queue again, with `next` as its culprit.
#[btf_tracepoint(function = "sched_switch")]
pub fn sched_switch(ctx: BtfTracePointContext) -> i32 {
    let offsets = OFFSETS.load();
    let preempt = ctx.arg::<u64>(0) != 0;
    let prev: *const u8 = ctx.arg(1);
    let next: *const u8 = ctx.arg(2);
    let prev_state = ctx.arg::<u64>(3) as u32;
    let now = unsafe { bpf_ktime_get_ns() };

    let next_pid = task_pid(next, &offsets);
    let next_cg = match next_pid {
        0 => None,
        _ => task_cgroup_id(next, &offsets),
    };
    if next_pid != 0 {
        if let Some(&enqueued) = unsafe { ENQUEUED.get(&next_pid) } {
            let _ = ENQUEUED.remove(&next_pid);
            if let Some(victim_cg) = next_cg {
                let wait_ns = now.saturating_sub(enqueued.ts);
                record(victim_cg, wait_ns);
                // Current is still `prev` here. An idle `prev` (pid 0) keeps culprit 0.
                let culprit_cg = match enqueued.culprit_cg {
                    0 if bpf_get_current_pid_tgid() as u32 != 0 => unsafe {
                        bpf_get_current_cgroup_id()
                    },
                    culprit_cg => culprit_cg,
                };
                let pair = Pair {
                    victim_cg,
                    culprit_cg,
                };
                blame(&pair, wait_ns);
            }
        }
    }

    if preempt || prev_state == TASK_RUNNING {
        enqueue(prev, now, next_cg.unwrap_or(0));
    }
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[link_section = "license"]
#[no_mangle]
static LICENSE: [u8; 4] = *b"GPL\0";
