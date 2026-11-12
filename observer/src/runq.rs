//! Turns reads of the `RUNQ` map into monotonic totals for each entity. Each read adds the
//! increase of each cgroup since the previous read to its entity, so the totals never go down
//! when the agent deletes a cgroup from the map.

use std::collections::{HashMap, HashSet};

use observer_common::RunqHist;

use crate::cardinality::{Cardinality, Slot};
use crate::config::Granularity;
use crate::workload::Entity;

/// Reads that an unresolved cgroup waits for its pod before the agent drops its samples, or
/// gives them to its fallback.
const PENDING_READS: u32 = 10;

/// Reads with no live pod and no cgroup in the map before an entity loses its series.
const IDLE_READS: u32 = 2;

/// Where the samples of a cgroup go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    System,
    Entity(Entity),
    /// In a pod that the watch does not know yet, or not in the cgroup tree. The fallback takes
    /// the samples once the wait ends, for a cgroup of a known pod that no container status names.
    Unresolved(Option<Entity>),
}

#[derive(Debug, Default)]
struct Pending {
    hist: RunqHist,
    reads: u32,
}

#[derive(Debug, Default)]
struct EntityState {
    hist: RunqHist,
    idle_reads: u32,
}

#[derive(Debug)]
pub struct Accumulator {
    /// Only the cgroups of the latest read, so memory stays within the map size.
    last: HashMap<u64, RunqHist>,
    /// The resolved owner of each cgroup in `last`. It holds after the pod or cgroup goes.
    owners: HashMap<u64, Owner>,
    /// Increases of unresolved cgroups. They never show under a temporary label, because a
    /// move to the real entity later would make that counter fall.
    pending: HashMap<u64, Pending>,
    entities: HashMap<Entity, EntityState>,
    cardinality: Cardinality<Entity>,
    system: (Entity, RunqHist),
    overflow: (Entity, RunqHist),
}

impl Accumulator {
    pub fn new(granularity: Granularity, cardinality: Cardinality<Entity>) -> Self {
        Self {
            last: HashMap::new(),
            owners: HashMap::new(),
            pending: HashMap::new(),
            entities: HashMap::new(),
            cardinality,
            system: (Entity::system(granularity), RunqHist::default()),
            overflow: (Entity::overflow(granularity), RunqHist::default()),
        }
    }

    /// Adds the increase since the previous read to the entity of each cgroup, and returns the
    /// cgroups with no new samples in that time. The caller deletes them from the map, so a
    /// cgroup that has gone stops using a map entry. A deleted cgroup that comes back counts
    /// again from zero. `live` holds the entities of the pods on the node.
    pub fn update(
        &mut self,
        mut read: HashMap<u64, RunqHist>,
        resolve: impl Fn(u64) -> Owner,
        live: &HashSet<Entity>,
    ) -> Vec<u64> {
        self.owners
            .retain(|cgroup_id, _| read.contains_key(cgroup_id));
        let mut idle = Vec::new();
        let mut seen = HashSet::new();
        for (cgroup_id, current) in &read {
            let last = self.last.get(cgroup_id);
            if last == Some(current) {
                idle.push(*cgroup_id);
            }
            let increase = increase(last, current);
            let owner = match self.owners.get(cgroup_id) {
                Some(owner) => owner.clone(),
                None => resolve(*cgroup_id),
            };
            let pending = self.pending.remove(cgroup_id);
            let owner = match owner {
                Owner::Unresolved(fallback) => {
                    let mut pending = pending.unwrap_or_default();
                    add(&mut pending.hist, &increase);
                    match self.wait(*cgroup_id, pending, fallback) {
                        Some(owner) => owner,
                        None => continue,
                    }
                }
                owner => {
                    let mut hist = increase;
                    if let Some(pending) = pending {
                        add(&mut hist, &pending.hist);
                    }
                    self.flush(&owner, &hist);
                    owner
                }
            };
            if let Owner::Entity(entity) = &owner {
                seen.insert(entity.clone());
            }
            self.owners.insert(*cgroup_id, owner);
        }
        for cgroup_id in &idle {
            read.remove(cgroup_id);
        }
        self.age_pending(&read, &resolve);
        self.last = read;
        self.collect_garbage(&seen, live);
        idle
    }

    /// The totals of `_system`, `_overflow` and each entity with its own series, in a stable order.
    pub fn snapshot(&self) -> Vec<(Entity, RunqHist)> {
        let mut own: Vec<_> = self
            .entities
            .iter()
            .filter(|(entity, _)| self.cardinality.slot(entity) == Some(Slot::Own))
            .map(|(entity, state)| (entity.clone(), state.hist))
            .collect();
        own.sort_by(|a, b| a.0.cmp(&b.0));
        [self.system.clone(), self.overflow.clone()]
            .into_iter()
            .chain(own)
            .collect()
    }

    pub fn cardinality(&self) -> &Cardinality<Entity> {
        &self.cardinality
    }

    /// Cgroups with samples that wait for their pod.
    pub fn unresolved(&self) -> usize {
        self.pending.len()
    }

    /// Clones the entity only when it is new to the budget or to the totals.
    fn add_to(&mut self, entity: &Entity, increase: &RunqHist) {
        if *increase == RunqHist::default() {
            return;
        }
        let slot = match self.cardinality.slot(entity) {
            Some(slot) => slot,
            None => self.cardinality.assign(entity.clone()),
        };
        // An overflow entity keeps a state too, so its idle reads count toward GC.
        if !self.entities.contains_key(entity) {
            self.entities.insert(entity.clone(), EntityState::default());
        }
        match (slot, self.entities.get_mut(entity)) {
            (Slot::Own, Some(state)) => add(&mut state.hist, increase),
            _ => add(&mut self.overflow.1, increase),
        }
    }

    fn flush(&mut self, owner: &Owner, hist: &RunqHist) {
        match owner {
            Owner::System => add(&mut self.system.1, hist),
            Owner::Entity(entity) => self.add_to(entity, hist),
            Owner::Unresolved(_) => {}
        }
    }

    /// Counts one more read for an unresolved cgroup. At the end of the wait, the samples go to
    /// the fallback, which then owns the cgroup, or the agent drops them.
    fn wait(
        &mut self,
        cgroup_id: u64,
        mut pending: Pending,
        fallback: Option<Entity>,
    ) -> Option<Owner> {
        pending.reads += 1;
        if pending.reads < PENDING_READS {
            self.pending.insert(cgroup_id, pending);
            return None;
        }
        let owner = Owner::Entity(fallback?);
        self.flush(&owner, &pending.hist);
        Some(owner)
    }

    /// Ages the unresolved cgroups that this read did not hold, because the main loop already
    /// counted the others.
    fn age_pending(&mut self, read: &HashMap<u64, RunqHist>, resolve: &impl Fn(u64) -> Owner) {
        let absent: Vec<u64> = self
            .pending
            .keys()
            .filter(|cgroup_id| !read.contains_key(cgroup_id))
            .copied()
            .collect();
        for cgroup_id in absent {
            let Some(pending) = self.pending.remove(&cgroup_id) else {
                continue;
            };
            match resolve(cgroup_id) {
                Owner::Unresolved(fallback) => {
                    let _ = self.wait(cgroup_id, pending, fallback);
                }
                owner => self.flush(&owner, &pending.hist),
            }
        }
    }

    /// Drops the series of an entity with no live pod and no cgroup in this read, one read
    /// after it first goes idle. An entity that comes back starts again from zero.
    fn collect_garbage(&mut self, seen: &HashSet<Entity>, live: &HashSet<Entity>) {
        let mut gone = Vec::new();
        for (entity, state) in &mut self.entities {
            if seen.contains(entity) || live.contains(entity) {
                state.idle_reads = 0;
                continue;
            }
            state.idle_reads += 1;
            if state.idle_reads >= IDLE_READS {
                gone.push(entity.clone());
            }
        }
        for entity in gone {
            self.entities.remove(&entity);
            self.cardinality.release(&entity);
        }
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
    use crate::workload::Workload;

    const COST: usize = 11;

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

    fn entity(name: &str) -> Entity {
        let workload = Workload {
            kind: "Deployment".into(),
            name: name.into(),
        };
        Entity::new(Granularity::Workload, "shop", &workload, "", "")
    }

    fn accumulator(budget: usize) -> Accumulator {
        Accumulator::new(Granularity::Workload, Cardinality::new(budget, COST))
    }

    /// Cgroup 1 is in `web`, cgroup 2 in `api`, cgroups above 100 are unresolved, others system.
    fn resolve(cgroup_id: u64) -> Owner {
        match cgroup_id {
            1 => Owner::Entity(entity("web")),
            2 => Owner::Entity(entity("api")),
            101.. => Owner::Unresolved(None),
            _ => Owner::System,
        }
    }

    fn update(acc: &mut Accumulator, entries: &[(u64, RunqHist)]) -> Vec<u64> {
        acc.update(read(entries), resolve, &HashSet::new())
    }

    fn totals(acc: &Accumulator) -> HashMap<String, RunqHist> {
        acc.snapshot()
            .into_iter()
            .map(|(entity, hist)| (entity.workload, hist))
            .collect()
    }

    fn node(acc: &Accumulator) -> RunqHist {
        let mut node = RunqHist::default();
        for (_, hist) in acc.snapshot() {
            add(&mut node, &hist);
        }
        node
    }

    #[test]
    fn pending_cgroup_ages_once_per_read() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(101, hist(&[1], 5))]);
        for _ in 1..PENDING_READS - 1 {
            update(&mut acc, &[]);
        }
        assert_eq!(acc.unresolved(), 1);
        update(&mut acc, &[]);
        assert_eq!(acc.unresolved(), 0);
    }

    #[test]
    fn fallback_takes_the_samples_when_the_wait_ends() {
        let mut acc = accumulator(1500);
        let sandbox = |id| match id {
            200 => Owner::Unresolved(Some(entity("web"))),
            id => resolve(id),
        };
        for reads in 1..=PENDING_READS as u64 {
            acc.update(
                read(&[(200, hist(&[reads], reads))]),
                sandbox,
                &HashSet::new(),
            );
        }
        assert_eq!(acc.unresolved(), 0);
        let expected = hist(&[PENDING_READS as u64], PENDING_READS as u64);
        assert_eq!(totals(&acc)["web"], expected);
        acc.update(read(&[(200, hist(&[12], 12))]), sandbox, &HashSet::new());
        assert_eq!(totals(&acc)["web"], hist(&[12], 12));
    }

    #[test]
    fn first_read_counts_in_full() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(1, hist(&[2, 1], 30)), (2, hist(&[0, 4], 80))]);
        assert_eq!(node(&acc), hist(&[2, 5], 110));
        assert_eq!(totals(&acc)["web"], hist(&[2, 1], 30));
        assert_eq!(totals(&acc)["api"], hist(&[0, 4], 80));
    }

    #[test]
    fn later_reads_add_the_increase() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(1, hist(&[2, 1], 30))]);
        update(&mut acc, &[(1, hist(&[3, 1], 35))]);
        assert_eq!(totals(&acc)["web"], hist(&[3, 1], 35));
    }

    #[test]
    fn restarted_cgroup_counts_as_new() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(1, hist(&[5, 5], 100))]);
        update(&mut acc, &[(1, hist(&[1, 6], 40))]);
        assert_eq!(totals(&acc)["web"], hist(&[6, 11], 140));
    }

    #[test]
    fn unchanged_cgroup_is_idle_and_forgotten() {
        let mut acc = accumulator(1500);
        assert!(update(&mut acc, &[(1, hist(&[4], 10))]).is_empty());
        assert_eq!(update(&mut acc, &[(1, hist(&[4], 10))]), vec![1]);
        assert!(acc.last.is_empty());
        update(&mut acc, &[(1, hist(&[2], 3))]);
        assert_eq!(totals(&acc)["web"], hist(&[6], 13));
    }

    #[test]
    fn vanished_cgroup_counts_as_new_on_return() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(3, hist(&[4], 10))]);
        update(&mut acc, &[(4, hist(&[1], 5))]);
        assert_eq!(acc.last.len(), 1);
        update(&mut acc, &[(3, hist(&[4], 10)), (4, hist(&[1], 5))]);
        assert_eq!(totals(&acc)["_system"], hist(&[9], 25));
    }

    #[test]
    fn system_and_overflow_always_export() {
        let acc = accumulator(1500);
        let names: Vec<_> = acc
            .snapshot()
            .into_iter()
            .map(|(e, _)| e.workload)
            .collect();
        assert_eq!(names, ["_system", "_overflow"]);
    }

    #[test]
    fn pending_cgroup_flushes_into_its_entity() {
        let mut acc = accumulator(1500);
        let resolved = std::cell::Cell::new(false);
        let resolve = |id| match id {
            101 if resolved.get() => Owner::Entity(entity("late")),
            id => resolve(id),
        };
        acc.update(read(&[(101, hist(&[3], 30))]), resolve, &HashSet::new());
        assert_eq!(acc.unresolved(), 1);
        assert!(!totals(&acc).contains_key("late"));
        assert_eq!(node(&acc), RunqHist::default());
        acc.update(read(&[(101, hist(&[5], 50))]), resolve, &HashSet::new());
        resolved.set(true);
        acc.update(read(&[(101, hist(&[6], 60))]), resolve, &HashSet::new());
        assert_eq!(acc.unresolved(), 0);
        assert_eq!(totals(&acc)["late"], hist(&[6], 60));
    }

    #[test]
    fn pending_cgroup_resolves_after_it_goes_idle() {
        let mut acc = accumulator(1500);
        let resolved = std::cell::Cell::new(false);
        let resolve = |id| match id {
            101 if resolved.get() => Owner::Entity(entity("late")),
            id => resolve(id),
        };
        acc.update(read(&[(101, hist(&[3], 30))]), resolve, &HashSet::new());
        resolved.set(true);
        acc.update(read(&[]), resolve, &HashSet::new());
        assert_eq!(totals(&acc)["late"], hist(&[3], 30));
    }

    #[test]
    fn pending_cgroup_drops_after_ten_reads() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(101, hist(&[3], 30))]);
        for _ in 1..PENDING_READS {
            assert_eq!(acc.unresolved(), 1);
            update(&mut acc, &[]);
        }
        assert_eq!(acc.unresolved(), 0);
        assert_eq!(node(&acc), RunqHist::default());
    }

    #[test]
    fn entity_goes_one_read_after_it_leaves() {
        let mut acc = accumulator(1500);
        let live = HashSet::from([entity("web")]);
        acc.update(read(&[(1, hist(&[1], 1))]), resolve, &live);
        acc.update(read(&[]), resolve, &live);
        assert!(totals(&acc).contains_key("web"), "a live pod keeps it");
        let series = acc.cardinality().series();
        acc.update(read(&[]), resolve, &HashSet::new());
        assert!(totals(&acc).contains_key("web"), "kept for one more read");
        acc.update(read(&[]), resolve, &HashSet::new());
        assert!(!totals(&acc).contains_key("web"));
        assert_eq!(acc.cardinality().series(), series - COST);
        acc.update(read(&[(1, hist(&[2], 2))]), resolve, &HashSet::new());
        assert_eq!(totals(&acc)["web"], hist(&[2], 2));
    }

    #[test]
    fn overflow_entity_stays_in_overflow() {
        let mut acc = accumulator(3 * COST);
        update(&mut acc, &[(1, hist(&[1], 1)), (2, hist(&[2], 2))]);
        let first = totals(&acc);
        let (own, over) = if first.contains_key("web") {
            ("web", 2)
        } else {
            ("api", 1)
        };
        assert_eq!(acc.cardinality().overflowed(), 1);
        update(&mut acc, &[(1, hist(&[5], 5)), (2, hist(&[5], 5))]);
        let later = totals(&acc);
        assert!(later.contains_key(own));
        assert_eq!(later.len(), 3, "{later:?}");
        assert_eq!(
            later["_overflow"],
            hist(&[5], 5),
            "cgroup {over} stays in _overflow"
        );
    }
}
