//! Loads and attaches the eBPF programs, and reads their maps.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context};
use aya::maps::{Map, MapData, PerCpuArray, PerCpuHashMap};
use aya::programs::BtfTracePoint;
use aya::{include_bytes_aligned, Btf, Ebpf, EbpfLoader};
use observer_common::{Bounds, Pair, RunqHist, DROPPED_PAIRS, DROPPED_RUNQ, MAX_BOUNDS};

use crate::config::Config;
use crate::offsets;

const PROGRAMS: [&str; 3] = ["sched_wakeup", "sched_wakeup_new", "sched_switch"];

/// Map names as the eBPF object declares them, and the `map` label of each.
pub const MAPS: [(&str, &str); 3] = [
    ("ENQUEUED", "enqueued"),
    ("RUNQ", "runq"),
    ("PREEMPT_WAIT", "pairs"),
];

/// Holds the attached programs, which detach when it drops.
pub struct Probes {
    ebpf: Ebpf,
}

impl Probes {
    pub fn load(config: &Config) -> anyhow::Result<Self> {
        let offsets = offsets::resolve(Path::new(offsets::VMLINUX_BTF))?;
        let bounds = bounds(config)?;
        let mut ebpf = EbpfLoader::new()
            .override_global("OFFSETS", &offsets, true)
            .override_global("BOUNDS", &bounds, true)
            .map_max_entries("RUNQ", config.max_cgroups)
            .map_max_entries("PREEMPT_WAIT", config.max_pairs)
            .load(include_bytes_aligned!(concat!(
                env!("OUT_DIR"),
                "/observer"
            )))
            .context("loading the eBPF object")?;
        let btf = Btf::from_sys_fs().context("reading kernel BTF")?;
        for name in PROGRAMS {
            let program: &mut BtfTracePoint = ebpf
                .program_mut(name)
                .ok_or_else(|| anyhow!("the eBPF object has no program {name}"))?
                .try_into()?;
            program
                .load(name, &btf)
                .with_context(|| format!("loading {name}"))?;
            program
                .attach()
                .with_context(|| format!("attaching {name}"))?;
        }
        Ok(Self { ebpf })
    }

    /// The run queue histogram of each cgroup, summed over CPUs.
    pub fn read_runq(&self) -> anyhow::Result<HashMap<u64, RunqHist>> {
        let map: PerCpuHashMap<_, u64, RunqHist> = PerCpuHashMap::try_from(self.map("RUNQ")?)?;
        let mut read = HashMap::new();
        for item in map.iter() {
            let (cgroup_id, per_cpu) = item.context("reading RUNQ")?;
            let mut sum = RunqHist::default();
            for hist in per_cpu.iter() {
                for (total, count) in sum.bins.iter_mut().zip(hist.bins) {
                    *total += count;
                }
                sum.sum_ns += hist.sum_ns;
            }
            read.insert(cgroup_id, sum);
        }
        Ok(read)
    }

    /// Deletes cgroups from `RUNQ`. A sample that lands between the read and the delete is lost.
    pub fn delete_runq(&mut self, cgroup_ids: &[u64]) -> anyhow::Result<()> {
        let map = self
            .ebpf
            .map_mut("RUNQ")
            .ok_or_else(|| anyhow!("the eBPF object has no map RUNQ"))?;
        let mut map: PerCpuHashMap<_, u64, RunqHist> = PerCpuHashMap::try_from(map)?;
        for cgroup_id in cgroup_ids {
            // Only the agent deletes keys, so a failure leaves an idle entry for the next read.
            let _ = map.remove(cgroup_id);
        }
        Ok(())
    }

    /// The wait of each victim and culprit pair in nanoseconds, summed over CPUs.
    pub fn read_pairs(&self) -> anyhow::Result<HashMap<Pair, u64>> {
        let map: PerCpuHashMap<_, Pair, u64> = PerCpuHashMap::try_from(self.map("PREEMPT_WAIT")?)?;
        let mut read = HashMap::new();
        for item in map.iter() {
            let (pair, per_cpu) = item.context("reading PREEMPT_WAIT")?;
            read.insert(pair, per_cpu.iter().sum());
        }
        Ok(read)
    }

    /// Deletes pairs from `PREEMPT_WAIT`, with the same loss as `delete_runq`.
    pub fn delete_pairs(&mut self, pairs: &[Pair]) -> anyhow::Result<()> {
        let map = self
            .ebpf
            .map_mut("PREEMPT_WAIT")
            .ok_or_else(|| anyhow!("the eBPF object has no map PREEMPT_WAIT"))?;
        let mut map: PerCpuHashMap<_, Pair, u64> = PerCpuHashMap::try_from(map)?;
        for pair in pairs {
            let _ = map.remove(pair);
        }
        Ok(())
    }

    /// Samples the programs lost because a map was full, summed over CPUs, by the `map` label.
    pub fn read_dropped(&self) -> anyhow::Result<[(&'static str, u64); 2]> {
        let map: PerCpuArray<_, u64> = PerCpuArray::try_from(self.map("DROPPED")?)?;
        let read = |slot| -> anyhow::Result<u64> {
            Ok(map.get(&slot, 0).context("reading DROPPED")?.iter().sum())
        };
        Ok([
            ("runq", read(DROPPED_RUNQ)?),
            ("pairs", read(DROPPED_PAIRS)?),
        ])
    }

    /// The key and value memory the kernel preallocates for a map. A per CPU map keeps the key
    /// once and the value once per CPU for each entry.
    pub fn map_bytes(&self, name: &str) -> anyhow::Result<u64> {
        let map = self.map(name)?;
        let info = map_data(map)?.info()?;
        let cpus = match map {
            Map::PerCpuHashMap(_) => aya::util::nr_cpus()
                .map_err(|(path, error)| anyhow!("reading {path}: {error}"))?
                .get(),
            _ => 1,
        };
        let entry = u64::from(info.key_size()) + u64::from(info.value_size()) * u64::from(cpus);
        Ok(u64::from(info.max_entries()) * entry)
    }

    fn map(&self, name: &str) -> anyhow::Result<&Map> {
        self.ebpf
            .map(name)
            .ok_or_else(|| anyhow!("the eBPF object has no map {name}"))
    }
}

fn map_data(map: &Map) -> anyhow::Result<&MapData> {
    match map {
        Map::LruHashMap(data) | Map::PerCpuHashMap(data) => Ok(data),
        _ => Err(anyhow!("unexpected map type")),
    }
}

fn bounds(config: &Config) -> anyhow::Result<Bounds> {
    let mut bounds = Bounds {
        ns: [0; MAX_BOUNDS],
        len: config.histogram_bounds.len() as u64,
    };
    for (ns, bound) in bounds.ns.iter_mut().zip(&config.histogram_bounds) {
        *ns = u64::try_from(bound.as_nanos()).context("a histogram bound is too large")?;
    }
    Ok(bounds)
}
