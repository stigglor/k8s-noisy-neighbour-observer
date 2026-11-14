//! Turns reads of the `RUNQ` and `PREEMPT_WAIT` maps into monotonic totals for each entity.
//! Each read adds the increase of each cgroup or pair since the previous read to its entity, so
//! the totals never go down when the agent deletes a key from the map.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use observer_common::{Pair, RunqHist, IDLE_CG, THROTTLED_CG};

use crate::attribution::{Blame, Cause, Wait};
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

/// Samples of an unresolved cgroup or pair, and the reads they have waited.
#[derive(Debug, Default)]
struct Pending<T> {
    value: T,
    reads: u32,
}

/// Counts one more read for a pending value, and returns the value when the wait ends.
fn hold<K: Hash + Eq, T>(
    map: &mut HashMap<K, Pending<T>>,
    key: K,
    mut pending: Pending<T>,
) -> Option<T> {
    pending.reads += 1;
    if pending.reads < PENDING_READS {
        map.insert(key, pending);
        return None;
    }
    Some(pending.value)
}

/// Keys with no new samples in a read. The caller deletes them from the maps and reports the
/// keys it deleted to `deleted`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Idle {
    pub cgroups: Vec<u64>,
    pub pairs: Vec<Pair>,
}

#[derive(Debug, Default)]
struct EntityState {
    hist: RunqHist,
    blame: Blame,
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
    pending: HashMap<u64, Pending<RunqHist>>,
    entities: HashMap<Entity, EntityState>,
    cardinality: Cardinality<Entity>,
    system: (Entity, EntityState),
    overflow: (Entity, EntityState),
    culprits_per_victim: usize,
    /// Only the pairs of the latest read.
    last_pairs: HashMap<Pair, u64>,
    /// Increases of pairs with an unresolved victim or culprit.
    pending_pairs: HashMap<Pair, Pending<u64>>,
    /// The culprit workload of each culprit cgroup in `last_pairs`. It holds after the pod goes.
    culprit_owners: HashMap<u64, Entity>,
    /// Reads with no live pod and no pair for each culprit workload with a series.
    culprit_idle: HashMap<Entity, u32>,
}

impl Accumulator {
    pub fn new(
        granularity: Granularity,
        cardinality: Cardinality<Entity>,
        culprits_per_victim: usize,
    ) -> Self {
        Self {
            last: HashMap::new(),
            owners: HashMap::new(),
            pending: HashMap::new(),
            entities: HashMap::new(),
            cardinality,
            system: (Entity::system(granularity), EntityState::default()),
            overflow: (Entity::overflow(granularity), EntityState::default()),
            culprits_per_victim,
            last_pairs: HashMap::new(),
            pending_pairs: HashMap::new(),
            culprit_owners: HashMap::new(),
            culprit_idle: HashMap::new(),
        }
    }

    /// Adds the increase of each cgroup and pair since the previous read to its entity, and
    /// returns the keys with no new samples. `pairs` is `None` when its read failed. `live`
    /// holds the entities of the pods on the node. The pairs go before GC, so that GC sees the
    /// victims of this read.
    pub fn update(
        &mut self,
        runq: HashMap<u64, RunqHist>,
        pairs: Option<HashMap<Pair, u64>>,
        resolve: impl Fn(u64) -> Owner,
        live: &HashSet<Entity>,
    ) -> Idle {
        let (cgroups, seen) = self.update_runq(runq, &resolve);
        let pairs = match pairs {
            Some(pairs) => self.update_pairs(pairs, &resolve, live),
            None => Vec::new(),
        };
        self.collect_garbage(&seen, live);
        Idle { cgroups, pairs }
    }

    /// Forgets the last value of the keys that the caller deleted from the maps. A deleted key
    /// that comes back counts again from zero. A key that failed to delete keeps its last
    /// value, so the next read adds only its increase.
    pub fn deleted(&mut self, cgroups: &[u64], pairs: &[Pair]) {
        for cgroup_id in cgroups {
            self.last.remove(cgroup_id);
        }
        for pair in pairs {
            self.last_pairs.remove(pair);
        }
    }

    /// The totals of `_system`, `_overflow` and each entity with its own series, in a stable order.
    pub fn snapshot(&self) -> Vec<(Entity, RunqHist)> {
        self.exported(|state| state.hist)
    }

    /// The wait by cause and by culprit of the entities in `snapshot`, in the same order.
    pub fn wait_snapshot(&self) -> Vec<(Entity, Wait)> {
        self.exported(|state| state.blame.wait())
    }

    pub fn cardinality(&self) -> &Cardinality<Entity> {
        &self.cardinality
    }

    /// Cgroups with samples that wait for their pod.
    pub fn unresolved(&self) -> usize {
        self.pending.len()
    }

    /// Returns the idle cgroups and the entities of this read.
    fn update_runq(
        &mut self,
        read: HashMap<u64, RunqHist>,
        resolve: &impl Fn(u64) -> Owner,
    ) -> (Vec<u64>, HashSet<Entity>) {
        self.owners
            .retain(|cgroup_id, _| read.contains_key(cgroup_id));
        let absent: Vec<u64> = self
            .pending
            .keys()
            .filter(|cgroup_id| !read.contains_key(cgroup_id))
            .copied()
            .collect();
        let mut idle = Vec::new();
        let mut seen = HashSet::new();
        for (cgroup_id, current) in &read {
            let last = self.last.get(cgroup_id);
            if last == Some(current) {
                idle.push(*cgroup_id);
            }
            let mut pending = self.pending.remove(cgroup_id).unwrap_or_default();
            add(&mut pending.value, &increase(last, current));
            if let Some(Owner::Entity(entity)) = self.settle_cgroup(*cgroup_id, pending, resolve) {
                seen.insert(entity);
            }
        }
        for cgroup_id in absent {
            if let Some(pending) = self.pending.remove(&cgroup_id) {
                self.settle_cgroup(cgroup_id, pending, resolve);
            }
        }
        self.last = read;
        (idle, seen)
    }

    /// Returns the idle pairs.
    fn update_pairs(
        &mut self,
        read: HashMap<Pair, u64>,
        resolve: &impl Fn(u64) -> Owner,
        live: &HashSet<Entity>,
    ) -> Vec<Pair> {
        let culprit_cgroups: HashSet<u64> = read.keys().map(|pair| pair.culprit_cg).collect();
        self.culprit_owners
            .retain(|cgroup_id, _| culprit_cgroups.contains(cgroup_id));
        let absent: Vec<Pair> = self
            .pending_pairs
            .keys()
            .filter(|pair| !read.contains_key(pair))
            .copied()
            .collect();
        let mut idle = Vec::new();
        let mut seen = HashSet::new();
        for (pair, current) in &read {
            let last = self.last_pairs.get(pair).copied();
            if last == Some(*current) {
                idle.push(*pair);
            }
            let increase = match last {
                Some(last) if *current >= last => current - last,
                _ => *current,
            };
            let mut pending = self.pending_pairs.remove(pair).unwrap_or_default();
            pending.value += increase;
            seen.extend(self.settle_pair(*pair, pending, resolve));
        }
        for pair in absent {
            if let Some(pending) = self.pending_pairs.remove(&pair) {
                seen.extend(self.settle_pair(pair, pending, resolve));
            }
        }
        self.last_pairs = read;
        let limit = self.culprits_per_victim;
        let cardinality = &mut self.cardinality;
        let own = self
            .entities
            .iter_mut()
            .filter(|(entity, _)| cardinality.slot(entity) == Some(Slot::Own))
            .map(|(_, state)| state)
            .collect::<Vec<_>>();
        for state in own
            .into_iter()
            .chain([&mut self.system.1, &mut self.overflow.1])
        {
            state.blame.end_read(limit, || cardinality.take_series());
        }
        self.forget_culprits(&seen, live);
        idle
    }

    fn exported<T>(&self, view: impl Fn(&EntityState) -> T) -> Vec<(Entity, T)> {
        let mut own: Vec<_> = self
            .entities
            .iter()
            .filter(|(entity, _)| self.cardinality.slot(entity) == Some(Slot::Own))
            .map(|(entity, state)| (entity.clone(), view(state)))
            .collect();
        own.sort_by(|a, b| a.0.cmp(&b.0));
        [&self.system, &self.overflow]
            .into_iter()
            .map(|(entity, state)| (entity.clone(), view(state)))
            .chain(own)
            .collect()
    }

    fn owner_of(&self, cgroup_id: u64, resolve: &impl Fn(u64) -> Owner) -> Owner {
        match self.owners.get(&cgroup_id) {
            Some(owner) => owner.clone(),
            None => resolve(cgroup_id),
        }
    }

    /// The state that takes the samples of an entity: its own, or the one of `_overflow`. Clones
    /// the entity only when it is new to the budget or to the totals.
    fn state_of(&mut self, entity: &Entity) -> &mut EntityState {
        let slot = match self.cardinality.slot(entity) {
            Some(slot) => slot,
            None => self.cardinality.assign(entity.clone()),
        };
        // An overflow entity keeps a state too, so its idle reads count toward GC.
        if !self.entities.contains_key(entity) {
            self.entities.insert(entity.clone(), EntityState::default());
        }
        match (slot, self.entities.get_mut(entity)) {
            (Slot::Own, Some(state)) => state,
            _ => &mut self.overflow.1,
        }
    }

    fn flush(&mut self, owner: &Owner, hist: &RunqHist) {
        if *hist == RunqHist::default() {
            return;
        }
        match owner {
            Owner::System => add(&mut self.system.1.hist, hist),
            Owner::Entity(entity) => add(&mut self.state_of(entity).hist, hist),
            Owner::Unresolved(_) => {}
        }
    }

    /// Adds the pending samples of a cgroup to its owner, or holds them while it is unresolved.
    /// At the end of the wait, the fallback takes them, or the agent drops them. Returns the
    /// owner once it resolves, which then owns the cgroup.
    fn settle_cgroup(
        &mut self,
        cgroup_id: u64,
        pending: Pending<RunqHist>,
        resolve: &impl Fn(u64) -> Owner,
    ) -> Option<Owner> {
        let (owner, hist) = match self.owner_of(cgroup_id, resolve) {
            Owner::Unresolved(fallback) => {
                let hist = hold(&mut self.pending, cgroup_id, pending)?;
                (Owner::Entity(fallback?), hist)
            }
            owner => (owner, pending.value),
        };
        self.flush(&owner, &hist);
        self.owners.insert(cgroup_id, owner.clone());
        Some(owner)
    }

    /// The same as `settle_cgroup` for a pair, which also waits for its culprit. The culprit
    /// needs no fallback, because it is at workload level. Returns the culprit once it resolves.
    fn settle_pair(
        &mut self,
        pair: Pair,
        pending: Pending<u64>,
        resolve: &impl Fn(u64) -> Owner,
    ) -> Option<Entity> {
        let culprit = self.culprit_of(pair.culprit_cg, resolve);
        let victim = self.owner_of(pair.victim_cg, resolve);
        let ready = !matches!(victim, Owner::Unresolved(_)) && culprit.is_some();
        let wait_ns = match ready {
            true => pending.value,
            false => match hold(&mut self.pending_pairs, pair, pending) {
                Some(wait_ns) => wait_ns,
                None => return culprit,
            },
        };
        let victim = match victim {
            Owner::Unresolved(Some(fallback)) => Owner::Entity(fallback),
            victim => victim,
        };
        if let Some(culprit) = &culprit {
            self.blame(&victim, culprit.clone(), wait_ns);
        }
        culprit
    }

    /// Adds wait only to a victim that already has a state. A pair never assigns budget, so a
    /// victim that GC removed cannot come back through a late pair.
    fn blame(&mut self, victim: &Owner, culprit: Entity, wait_ns: u64) {
        if wait_ns == 0 {
            return;
        }
        let (cause, state) = match victim {
            Owner::System => (Cause::of(&self.system.0, &culprit), &mut self.system.1),
            Owner::Entity(entity) => {
                let cause = Cause::of(entity, &culprit);
                let state = match self.cardinality.slot(entity) {
                    Some(Slot::Own) => self.entities.get_mut(entity),
                    Some(Slot::Overflow) => Some(&mut self.overflow.1),
                    None => None,
                };
                match state {
                    Some(state) => (cause, state),
                    None => return,
                }
            }
            Owner::Unresolved(_) => return,
        };
        state.blame.add(cause, culprit, wait_ns);
    }

    /// The culprit is at workload level, so a pod with no container status names it at once.
    fn culprit_of(&mut self, cgroup_id: u64, resolve: &impl Fn(u64) -> Owner) -> Option<Entity> {
        match cgroup_id {
            IDLE_CG => return Some(Entity::idle()),
            THROTTLED_CG => return Some(Entity::throttled()),
            _ => {}
        }
        if let Some(culprit) = self.culprit_owners.get(&cgroup_id) {
            return Some(culprit.clone());
        }
        let culprit = match self.owner_of(cgroup_id, resolve) {
            Owner::System => self.system.0.to_workload(),
            Owner::Entity(entity) | Owner::Unresolved(Some(entity)) => entity.to_workload(),
            Owner::Unresolved(None) => return None,
        };
        self.culprit_owners.insert(cgroup_id, culprit.clone());
        Some(culprit)
    }

    /// Drops the series of a culprit workload with no live pod and no pair in this read, one
    /// read after it first goes idle, and frees its budget.
    fn forget_culprits(&mut self, seen: &HashSet<Entity>, live: &HashSet<Entity>) {
        let live: HashSet<Entity> = live.iter().map(Entity::to_workload).collect();
        let named: HashSet<Entity> = self
            .entities
            .values()
            .chain([&self.system.1, &self.overflow.1])
            .flat_map(|state| state.blame.culprits())
            .cloned()
            .collect();
        self.culprit_idle
            .retain(|culprit, _| named.contains(culprit));
        let mut gone = Vec::new();
        for culprit in named {
            let idle = self.culprit_idle.entry(culprit.clone()).or_default();
            if culprit.is_system() || seen.contains(&culprit) || live.contains(&culprit) {
                *idle = 0;
                continue;
            }
            *idle += 1;
            if *idle >= IDLE_READS {
                gone.push(culprit);
            }
        }
        for culprit in gone {
            self.culprit_idle.remove(&culprit);
            let freed = self
                .entities
                .values_mut()
                .chain([&mut self.system.1, &mut self.overflow.1])
                .filter_map(|state| state.blame.forget(&culprit).then_some(()))
                .count();
            self.cardinality.free_series(freed);
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
            if let Some(state) = self.entities.remove(&entity) {
                self.cardinality.free_series(state.blame.culprits().count());
            }
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

    const COST: usize = 16;

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
        Accumulator::new(Granularity::Workload, Cardinality::new(budget, COST), 3)
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

    /// One read of `RUNQ` alone, where every delete succeeds.
    fn runq(
        acc: &mut Accumulator,
        read: HashMap<u64, RunqHist>,
        resolve: impl Fn(u64) -> Owner,
        live: &HashSet<Entity>,
    ) -> Vec<u64> {
        let idle = acc.update(read, None, resolve, live);
        acc.deleted(&idle.cgroups, &idle.pairs);
        idle.cgroups
    }

    fn update(acc: &mut Accumulator, entries: &[(u64, RunqHist)]) -> Vec<u64> {
        runq(acc, read(entries), resolve, &HashSet::new())
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
            runq(
                &mut acc,
                read(&[(200, hist(&[reads], reads))]),
                sandbox,
                &HashSet::new(),
            );
        }
        assert_eq!(acc.unresolved(), 0);
        let expected = hist(&[PENDING_READS as u64], PENDING_READS as u64);
        assert_eq!(totals(&acc)["web"], expected);
        runq(
            &mut acc,
            read(&[(200, hist(&[12], 12))]),
            sandbox,
            &HashSet::new(),
        );
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
        runq(
            &mut acc,
            read(&[(101, hist(&[3], 30))]),
            resolve,
            &HashSet::new(),
        );
        assert_eq!(acc.unresolved(), 1);
        assert!(!totals(&acc).contains_key("late"));
        assert_eq!(node(&acc), RunqHist::default());
        runq(
            &mut acc,
            read(&[(101, hist(&[5], 50))]),
            resolve,
            &HashSet::new(),
        );
        resolved.set(true);
        runq(
            &mut acc,
            read(&[(101, hist(&[6], 60))]),
            resolve,
            &HashSet::new(),
        );
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
        runq(
            &mut acc,
            read(&[(101, hist(&[3], 30))]),
            resolve,
            &HashSet::new(),
        );
        resolved.set(true);
        runq(&mut acc, read(&[]), resolve, &HashSet::new());
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
        runq(&mut acc, read(&[(1, hist(&[1], 1))]), resolve, &live);
        runq(&mut acc, read(&[]), resolve, &live);
        assert!(totals(&acc).contains_key("web"), "a live pod keeps it");
        let series = acc.cardinality().series();
        runq(&mut acc, read(&[]), resolve, &HashSet::new());
        assert!(totals(&acc).contains_key("web"), "kept for one more read");
        runq(&mut acc, read(&[]), resolve, &HashSet::new());
        assert!(!totals(&acc).contains_key("web"));
        assert_eq!(acc.cardinality().series(), series - COST);
        runq(
            &mut acc,
            read(&[(1, hist(&[2], 2))]),
            resolve,
            &HashSet::new(),
        );
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

    const MS: u64 = 1_000_000;

    fn pairs(entries: &[(u64, u64, u64)]) -> HashMap<Pair, u64> {
        entries
            .iter()
            .map(|&(victim_cg, culprit_cg, ns)| {
                let pair = Pair {
                    victim_cg,
                    culprit_cg,
                };
                (pair, ns)
            })
            .collect()
    }

    /// One read of both maps, where every delete succeeds. `web` and `api` stay live.
    fn both(acc: &mut Accumulator, runq: &[(u64, RunqHist)], entries: &[(u64, u64, u64)]) -> Idle {
        let live = HashSet::from([entity("web"), entity("api")]);
        let idle = acc.update(read(runq), Some(pairs(entries)), resolve, &live);
        acc.deleted(&idle.cgroups, &idle.pairs);
        idle
    }

    fn waits(acc: &Accumulator) -> HashMap<String, Wait> {
        acc.wait_snapshot()
            .into_iter()
            .map(|(entity, wait)| (entity.workload, wait))
            .collect()
    }

    fn by_culprit(wait: &Wait) -> Vec<(&str, u64)> {
        let culprits = wait.by_culprit.iter();
        culprits.map(|(c, ns)| (c.workload.as_str(), *ns)).collect()
    }

    #[test]
    fn pair_wait_goes_to_the_victim_by_cause_and_culprit() {
        let mut acc = accumulator(1500);
        let web = [(1, hist(&[3], 125 * MS))];
        let entries = [(1, 2, 100 * MS), (1, 1, 20 * MS), (1, 3, 5 * MS)];
        assert!(both(&mut acc, &web, &entries).pairs.is_empty());
        let wait = &waits(&acc)["web"];
        assert_eq!(wait.by_cause, [20 * MS, 100 * MS, 5 * MS, 0, 0]);
        assert_eq!(
            by_culprit(wait),
            [("api", 100 * MS), ("web", 20 * MS), ("_other", 5 * MS)]
        );
        assert_eq!(acc.cardinality().series(), 3 * COST + 2);
        let entries = [(1, 2, 150 * MS), (1, 1, 20 * MS), (1, 3, 5 * MS)];
        assert_eq!(both(&mut acc, &web, &entries).pairs.len(), 2);
        assert_eq!(by_culprit(&waits(&acc)["web"])[0], ("api", 150 * MS));
        assert_eq!(acc.last_pairs.len(), 1);
    }

    #[test]
    fn cause_series_add_up_to_the_histogram_sum() {
        let mut acc = accumulator(1500);
        for scale in [1, 3] {
            let web = [(1, hist(&[5], scale * 300 * MS))];
            let entries = [
                (1, 2, 100),
                (1, 1, 20),
                (1, 3, 30),
                (1, IDLE_CG, 50),
                (1, THROTTLED_CG, 100),
            ];
            both(
                &mut acc,
                &web,
                &entries.map(|(v, c, ms)| (v, c, scale * ms * MS)),
            );
            let wait = &waits(&acc)["web"];
            let hist = &totals(&acc)["web"];
            assert_eq!(wait.by_cause.iter().sum::<u64>(), hist.sum_ns);
            assert_eq!(wait.by_cause[3], scale * 50 * MS, "the idle cause");
            assert_eq!(wait.by_cause[4], scale * 100 * MS, "the throttled cause");
            let culprits = wait.by_culprit.iter();
            assert!(culprits
                .clone()
                .all(|(c, _)| !c.is_idle() && !c.is_throttled()));
            let named: u64 = culprits.map(|(_, ns)| ns).sum();
            assert_eq!(named, hist.sum_ns - wait.by_cause[3] - wait.by_cause[4]);
        }
    }

    #[test]
    fn system_and_overflow_victims_have_wait_series() {
        let mut acc = accumulator(1500);
        both(&mut acc, &[(3, hist(&[1], 7))], &[(3, 3, 7)]);
        let names: Vec<_> = acc.wait_snapshot().into_iter().collect();
        assert_eq!(names[0].0.workload, "_system");
        assert_eq!(names[0].1.by_cause, [0, 0, 7, 0, 0]);
        assert_eq!(by_culprit(&names[0].1), [("_other", 7)]);
        assert_eq!(names[1].0.workload, "_overflow");
        assert_eq!(by_culprit(&names[1].1), [("_other", 0)]);
    }

    #[test]
    fn unresolved_pair_waits_and_never_shows() {
        let mut acc = accumulator(1500);
        let resolved = std::cell::Cell::new(false);
        let resolve = |id| match id {
            101 if resolved.get() => Owner::Entity(entity("late")),
            id => resolve(id),
        };
        let live = HashSet::from([entity("web")]);
        let web = read(&[(1, hist(&[1], 50 * MS))]);
        acc.update(
            web.clone(),
            Some(pairs(&[(1, 101, 30 * MS)])),
            resolve,
            &live,
        );
        assert_eq!(waits(&acc)["web"].by_cause, [0; 5]);
        assert_eq!(acc.pending_pairs.len(), 1);
        resolved.set(true);
        acc.update(web, Some(pairs(&[(1, 101, 50 * MS)])), resolve, &live);
        let wait = &waits(&acc)["web"];
        assert_eq!(wait.by_cause, [0, 50 * MS, 0, 0, 0]);
        assert_eq!(by_culprit(wait), [("late", 50 * MS), ("_other", 0)]);
    }

    #[test]
    fn unresolved_pair_drops_after_ten_reads() {
        let mut acc = accumulator(1500);
        both(&mut acc, &[], &[(101, 1, 30 * MS)]);
        for _ in 1..PENDING_READS {
            assert_eq!(acc.pending_pairs.len(), 1);
            both(&mut acc, &[], &[]);
        }
        assert!(acc.pending_pairs.is_empty());
        assert!(acc
            .wait_snapshot()
            .iter()
            .all(|(_, w)| w.by_cause == [0; 5]));
    }

    #[test]
    fn unresolved_idle_pair_ages_once_per_read() {
        let mut acc = accumulator(1500);
        let live = HashSet::new();
        for _ in 1..PENDING_READS {
            acc.update(read(&[]), Some(pairs(&[(1, 101, 30 * MS)])), resolve, &live);
            assert_eq!(acc.pending_pairs.len(), 1);
        }
        acc.update(read(&[]), Some(pairs(&[(1, 101, 30 * MS)])), resolve, &live);
        assert!(acc.pending_pairs.is_empty());
    }

    #[test]
    fn culprit_series_stop_at_ninety_percent_of_the_budget() {
        // Web takes 3 x COST of 55, so one culprit series fits below 90% and a second does not.
        let mut acc = accumulator(55);
        both(
            &mut acc,
            &[(1, hist(&[1], 1))],
            &[(1, 2, 100 * MS), (1, 3, 100 * MS)],
        );
        assert_eq!(acc.cardinality().series(), 3 * COST + 1);
        let wait = &waits(&acc)["web"];
        assert_eq!(wait.by_culprit.len(), 2);
        assert_eq!(wait.by_culprit[1], (Entity::other(), 100 * MS));
    }

    #[test]
    fn culprit_series_go_one_read_after_the_culprit_leaves() {
        let mut acc = accumulator(1500);
        let web = read(&[(1, hist(&[1], 1))]);
        let live = HashSet::from([entity("web"), entity("api")]);
        acc.update(
            web.clone(),
            Some(pairs(&[(1, 2, 100 * MS)])),
            resolve,
            &live,
        );
        let series = acc.cardinality().series();
        let live = HashSet::from([entity("web")]);
        acc.update(
            web.clone(),
            Some(pairs(&[(1, 2, 100 * MS)])),
            resolve,
            &live,
        );
        acc.update(web.clone(), Some(pairs(&[])), resolve, &live);
        assert_eq!(by_culprit(&waits(&acc)["web"])[0].0, "api", "one more read");
        acc.update(web, Some(pairs(&[])), resolve, &live);
        assert_eq!(by_culprit(&waits(&acc)["web"]), [("_other", 0)]);
        assert_eq!(acc.cardinality().series(), series - 1);
    }

    #[test]
    fn victim_frees_its_culprit_series() {
        let mut acc = accumulator(1500);
        both(
            &mut acc,
            &[(1, hist(&[1], 1))],
            &[(1, 2, 100 * MS), (1, 3, 100 * MS)],
        );
        assert_eq!(acc.cardinality().series(), 3 * COST + 2);
        update(&mut acc, &[]);
        update(&mut acc, &[]);
        assert!(!waits(&acc).contains_key("web"));
        assert_eq!(acc.cardinality().series(), 2 * COST);
    }

    #[test]
    fn failed_delete_keeps_the_last_value() {
        let mut acc = accumulator(1500);
        let live = HashSet::from([entity("web")]);
        let web = |ns| read(&[(1, hist(&[1], ns))]);
        acc.update(web(10), Some(pairs(&[(1, 2, 10)])), resolve, &live);
        let idle = acc.update(web(10), Some(pairs(&[(1, 2, 10)])), resolve, &live);
        assert_eq!(idle.cgroups, [1]);
        // Both deletes fail, so neither key is reported, and the next read adds only the increase.
        acc.deleted(&[], &[]);
        acc.update(web(15), Some(pairs(&[(1, 2, 15)])), resolve, &live);
        assert_eq!(totals(&acc)["web"].sum_ns, 15);
        assert_eq!(waits(&acc)["web"].by_cause[1], 15);
        // A delete that succeeds forgets the key, so a key that comes back counts from zero.
        let idle = acc.update(web(15), Some(pairs(&[(1, 2, 15)])), resolve, &live);
        acc.deleted(&idle.cgroups, &idle.pairs);
        acc.update(web(4), Some(pairs(&[(1, 2, 4)])), resolve, &live);
        assert_eq!(totals(&acc)["web"].sum_ns, 19);
        assert_eq!(waits(&acc)["web"].by_cause[1], 19);
    }

    #[test]
    fn late_pair_cannot_bring_back_a_victim() {
        let mut acc = accumulator(1500);
        let resolved = std::cell::Cell::new(false);
        let resolve = |id| match id {
            101 if resolved.get() => Owner::Entity(entity("api")),
            id => resolve(id),
        };
        let none = HashSet::new();
        acc.update(
            read(&[(1, hist(&[1], 1))]),
            Some(pairs(&[(1, 101, 9)])),
            resolve,
            &none,
        );
        acc.update(read(&[]), Some(pairs(&[])), resolve, &none);
        resolved.set(true);
        // The pair resolves in the read where GC removes web, and again after it.
        acc.update(read(&[]), Some(pairs(&[])), resolve, &none);
        assert!(!waits(&acc).contains_key("web"));
        assert_eq!(acc.cardinality().series(), 2 * COST);
        acc.update(read(&[]), Some(pairs(&[(1, 101, 20)])), resolve, &none);
        assert!(!waits(&acc).contains_key("web"));
        assert_eq!(acc.cardinality().series(), 2 * COST);
    }

    #[test]
    fn failed_pair_read_still_counts_the_histogram() {
        let mut acc = accumulator(1500);
        let idle = acc.update(read(&[(1, hist(&[2], 8))]), None, resolve, &HashSet::new());
        assert!(idle.pairs.is_empty());
        assert_eq!(totals(&acc)["web"], hist(&[2], 8));
    }
}
