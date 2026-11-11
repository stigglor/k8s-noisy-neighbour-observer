//! Turns reads of the `RUNQ` map into monotonic totals.
//!
//! The map holds one histogram per cgroup. Each read adds the increase of each cgroup since the
//! previous read, so the totals never go down when the agent deletes a cgroup from the map.

use std::collections::HashMap;

use observer_common::RunqHist;

#[derive(Debug, Default)]
pub struct Accumulator {
    /// Only the cgroups of the latest read, so memory stays within the map size.
    last: HashMap<u64, RunqHist>,
    node: RunqHist,
}

impl Accumulator {
    /// Adds the increase since the previous read, and returns the cgroups with no new samples
    /// in that time. The caller deletes them from the map, so a cgroup that has gone stops
    /// using a map entry. A deleted cgroup that comes back counts again from zero.
    pub fn update(&mut self, mut read: HashMap<u64, RunqHist>) -> Vec<u64> {
        let mut idle = Vec::new();
        for (cgroup_id, current) in &read {
            let last = self.last.get(cgroup_id);
            if last == Some(current) {
                idle.push(*cgroup_id);
            }
            add(&mut self.node, &increase(last, current));
        }
        for cgroup_id in &idle {
            read.remove(cgroup_id);
        }
        self.last = read;
        idle
    }

    /// Totals over every cgroup on the node.
    pub fn node(&self) -> RunqHist {
        self.node
    }
}

/// A cgroup with no previous value, or with any value below it, starts again from zero.
fn increase(last: Option<&RunqHist>, current: &RunqHist) -> RunqHist {
    let Some(last) = last else {
        return *current;
    };
    let lower = current.sum_ns < last.sum_ns
        || current
            .bins
            .iter()
            .zip(last.bins)
            .any(|(now, before)| *now < before);
    if lower {
        return *current;
    }
    let mut increase = *current;
    for (count, before) in increase.bins.iter_mut().zip(last.bins) {
        *count -= before;
    }
    increase.sum_ns -= last.sum_ns;
    increase
}

fn add(total: &mut RunqHist, increase: &RunqHist) {
    for (count, more) in total.bins.iter_mut().zip(increase.bins) {
        *count = count.wrapping_add(more);
    }
    total.sum_ns = total.sum_ns.wrapping_add(increase.sum_ns);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(bins: &[u64], sum_ns: u64) -> RunqHist {
        let mut hist = RunqHist {
            sum_ns,
            ..Default::default()
        };
        hist.bins[..bins.len()].copy_from_slice(bins);
        hist
    }

    fn read(entries: &[(u64, RunqHist)]) -> HashMap<u64, RunqHist> {
        entries.iter().copied().collect()
    }

    #[test]
    fn first_read_counts_in_full() {
        let mut acc = Accumulator::default();
        acc.update(read(&[(1, hist(&[2, 1], 30)), (2, hist(&[0, 4], 80))]));
        assert_eq!(acc.node(), hist(&[2, 5], 110));
    }

    #[test]
    fn later_reads_add_the_increase() {
        let mut acc = Accumulator::default();
        acc.update(read(&[(1, hist(&[2, 1], 30))]));
        acc.update(read(&[(1, hist(&[3, 1], 35))]));
        assert_eq!(acc.node(), hist(&[3, 1], 35));
    }

    #[test]
    fn restarted_cgroup_counts_as_new() {
        let mut acc = Accumulator::default();
        acc.update(read(&[(1, hist(&[5, 5], 100))]));
        acc.update(read(&[(1, hist(&[1, 6], 40))]));
        assert_eq!(acc.node(), hist(&[6, 11], 140));
    }

    #[test]
    fn unchanged_cgroup_is_idle_and_forgotten() {
        let mut acc = Accumulator::default();
        assert!(acc.update(read(&[(1, hist(&[4], 10))])).is_empty());
        assert_eq!(acc.update(read(&[(1, hist(&[4], 10))])), vec![1]);
        assert!(acc.last.is_empty());
        acc.update(read(&[(1, hist(&[2], 3))]));
        assert_eq!(acc.node(), hist(&[6], 13));
    }

    #[test]
    fn vanished_cgroup_counts_as_new_on_return() {
        let mut acc = Accumulator::default();
        acc.update(read(&[(1, hist(&[4], 10))]));
        acc.update(read(&[(2, hist(&[1], 5))]));
        assert_eq!(acc.last.len(), 1);
        acc.update(read(&[(1, hist(&[4], 10)), (2, hist(&[1], 5))]));
        assert_eq!(acc.node(), hist(&[9], 25));
    }
}
