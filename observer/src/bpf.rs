//! Loads and attaches the eBPF programs, and reads their maps.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::Path;

use anyhow::{anyhow, Context};
use aya::maps::{Map, MapData, PerCpuArray, PerCpuHashMap, PerCpuValues};
use aya::programs::BtfTracePoint;
use aya::{include_bytes_aligned, Btf, Ebpf, EbpfLoader, Pod};
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
        self.read_per_cpu("RUNQ", |per_cpu: &PerCpuValues<RunqHist>| {
            let mut sum = RunqHist::default();
            for hist in per_cpu.iter() {
                for (total, count) in sum.bins.iter_mut().zip(hist.bins) {
                    *total += count;
                }
                sum.sum_ns += hist.sum_ns;
            }
            sum
        })
    }

    /// The wait of each victim and culprit pair in nanoseconds, summed over CPUs.
    pub fn read_pairs(&self) -> anyhow::Result<HashMap<Pair, u64>> {
        self.read_per_cpu("PREEMPT_WAIT", |per_cpu: &PerCpuValues<u64>| {
            per_cpu.iter().sum()
        })
    }

    /// Deletes cgroups from `RUNQ`, and returns the cgroups it deleted.
    pub fn delete_runq(&mut self, cgroup_ids: &[u64]) -> anyhow::Result<Vec<u64>> {
        self.delete::<u64, RunqHist>("RUNQ", cgroup_ids)
    }

    /// Deletes pairs from `PREEMPT_WAIT`, and returns the pairs it deleted.
    pub fn delete_pairs(&mut self, pairs: &[Pair]) -> anyhow::Result<Vec<Pair>> {
        self.delete::<Pair, u64>("PREEMPT_WAIT", pairs)
    }

    fn read_per_cpu<K: Pod + Eq + Hash, V: Pod, T>(
        &self,
        name: &str,
        sum: impl Fn(&PerCpuValues<V>) -> T,
    ) -> anyhow::Result<HashMap<K, T>> {
        let map: PerCpuHashMap<_, K, V> = PerCpuHashMap::try_from(self.map(name)?)?;
        let mut read = HashMap::new();
        for item in map.iter() {
            let (key, per_cpu) = item.with_context(|| format!("reading {name}"))?;
            read.insert(key, sum(&per_cpu));
        }
        Ok(read)
    }

    /// Only the agent deletes keys, so a key that fails to delete stays as an idle entry, and
    /// the caller keeps its last value. A sample between the read and the delete is lost.
    fn delete<K: Pod, V: Pod>(&mut self, name: &str, keys: &[K]) -> anyhow::Result<Vec<K>> {
        let map = self
            .ebpf
            .map_mut(name)
            .ok_or_else(|| anyhow!("the eBPF object has no map {name}"))?;
        let mut map: PerCpuHashMap<_, K, V> = PerCpuHashMap::try_from(map)?;
        Ok(keys
            .iter()
            .filter(|key| map.remove(key).is_ok())
            .copied()
            .collect())
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
