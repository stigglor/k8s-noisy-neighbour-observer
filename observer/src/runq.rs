//! Turns reads of the `RUNQ` and `PREEMPT_WAIT` maps into monotonic totals for each entity.
//! Each read adds the increase of each cgroup or pair since the previous read to its entity, so
//! the totals never go down when the agent deletes a key from the map.

use std::collections::{HashMap, HashSet};

use observer_common::{Pair, RunqHist, IDLE_CG};

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

#[derive(Debug, Default)]
struct Pending {
    hist: RunqHist,
    reads: u32,
}

#[derive(Debug, Default)]
struct PendingPair {
    wait_ns: u64,
    reads: u32,
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
    pending: HashMap<u64, Pending>,
    entities: HashMap<Entity, EntityState>,
    cardinality: Cardinality<Entity>,
    system: (Entity, EntityState),
    overflow: (Entity, EntityState),
    culprits_per_victim: usize,
    /// Only the pairs of the latest read.
    last_pairs: HashMap<Pair, u64>,
    /// Increases of pairs with an unresolved victim or culprit, kept as `pending` keeps cgroups.
    pending_pairs: HashMap<Pair, PendingPair>,
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

    /// Adds the wait of each victim and culprit pair since the previous read, and returns the
    /// pairs with no new wait, for the caller to delete. Call it after `update` with the same
    /// `resolve` and `live`, so that a victim cgroup has the owner of its histogram.
    pub fn update_pairs(
        &mut self,
        mut read: HashMap<Pair, u64>,
        resolve: impl Fn(u64) -> Owner,
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
            pending.wait_ns += increase;
            seen.extend(self.settle(*pair, pending, &resolve));
        }
        for pair in &idle {
            read.remove(pair);
        }
        for pair in absent {
            if let Some(pending) = self.pending_pairs.remove(&pair) {
                seen.extend(self.settle(pair, pending, &resolve));
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

    /// The totals of `_system`, `_overflow` and each entity with its own series, in a stable order.
    pub fn snapshot(&self) -> Vec<(Entity, RunqHist)> {
        self.exported(|state| state.hist)
    }

    /// The wait by cause and by culprit of the entities in `snapshot`, in the same order.
    pub fn wait_snapshot(&self) -> Vec<(Entity, Wait)> {
        self.exported(|state| state.blame.wait())
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

    pub fn cardinality(&self) -> &Cardinality<Entity> {
        &self.cardinality
    }

    /// Cgroups with samples that wait for their pod.
    pub fn unresolved(&self) -> usize {
        self.pending.len()
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

    /// Adds the pending wait of a pair to its victim, or keeps it while the victim or the culprit
    /// is unresolved, as `wait` does for a cgroup. Returns the culprit workload once it resolves.
    fn settle(
        &mut self,
        pair: Pair,
        mut pending: PendingPair,
        resolve: &impl Fn(u64) -> Owner,
    ) -> Option<Entity> {
        let culprit = self.culprit_of(pair.culprit_cg, resolve);
        let victim = match self.owners.get(&pair.victim_cg) {
            Some(owner) => owner.clone(),
            None => resolve(pair.victim_cg),
        };
        let ready = !matches!(victim, Owner::Unresolved(_)) && culprit.is_some();
        if !ready {
            pending.reads += 1;
            if pending.reads < PENDING_READS {
                self.pending_pairs.insert(pair, pending);
                return culprit;
            }
        }
        let victim = match victim {
            Owner::Unresolved(Some(fallback)) => Owner::Entity(fallback),
            victim => victim,
        };
        if let Some(culprit) = &culprit {
            self.blame(&victim, culprit.clone(), pending.wait_ns);
        }
        culprit
    }

    fn blame(&mut self, victim: &Owner, culprit: Entity, wait_ns: u64) {
        if wait_ns == 0 {
            return;
        }
        let (state, cause) = match victim {
            Owner::System => (&mut self.system.1, Cause::of(&self.system.0, &culprit)),
            Owner::Entity(entity) => {
                let cause = Cause::of(entity, &culprit);
                (self.state_of(entity), cause)
            }
            Owner::Unresolved(_) => return,
        };
        state.blame.add(cause, culprit, wait_ns);
    }

    /// The culprit is at workload level, so a pod with no container status names it at once.
    fn culprit_of(&mut self, cgroup_id: u64, resolve: &impl Fn(u64) -> Owner) -> Option<Entity> {
        if cgroup_id == IDLE_CG {
            return Some(Entity::idle());
        }
        if let Some(culprit) = self.culprit_owners.get(&cgroup_id) {
            return Some(culprit.clone());
        }
        let owner = match self.owners.get(&cgroup_id) {
            Some(owner) => owner.clone(),
            None => resolve(cgroup_id),
        };
        let culprit = match owner {
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

    fn update_pairs(acc: &mut Accumulator, entries: &[(u64, u64, u64)]) -> Vec<Pair> {
        acc.update_pairs(pairs(entries), resolve, &HashSet::new())
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
        update(&mut acc, &[(1, hist(&[3], 125 * MS))]);
        let entries = [(1, 2, 100 * MS), (1, 1, 20 * MS), (1, 3, 5 * MS)];
        assert!(update_pairs(&mut acc, &entries).is_empty());
        let web = &waits(&acc)["web"];
        assert_eq!(web.by_cause, [20 * MS, 100 * MS, 5 * MS, 0]);
        assert_eq!(
            by_culprit(web),
            [("api", 100 * MS), ("web", 20 * MS), ("_other", 5 * MS)]
        );
        assert_eq!(acc.cardinality().series(), 3 * COST + 2);
        let entries = [(1, 2, 150 * MS), (1, 1, 20 * MS), (1, 3, 5 * MS)];
        assert_eq!(update_pairs(&mut acc, &entries).len(), 2);
        assert_eq!(by_culprit(&waits(&acc)["web"])[0], ("api", 150 * MS));
        assert_eq!(acc.last_pairs.len(), 1);
    }

    #[test]
    fn cause_series_add_up_to_the_histogram_sum() {
        let mut acc = accumulator(1500);
        for scale in [1, 3] {
            update(&mut acc, &[(1, hist(&[4], scale * 200 * MS))]);
            let entries = [(1, 2, 100), (1, 1, 20), (1, 3, 30), (1, IDLE_CG, 50)];
            let entries = entries.map(|(v, c, ms)| (v, c, scale * ms * MS));
            update_pairs(&mut acc, &entries);
            let web = &waits(&acc)["web"];
            let hist = &totals(&acc)["web"];
            assert_eq!(web.by_cause.iter().sum::<u64>(), hist.sum_ns);
            assert_eq!(web.by_cause[3], scale * 50 * MS, "the idle cause");
            assert!(web.by_culprit.iter().all(|(c, _)| !c.is_idle()));
        }
    }

    #[test]
    fn system_and_overflow_victims_have_wait_series() {
        let mut acc = accumulator(1500);
        update_pairs(&mut acc, &[(3, 3, 7)]);
        let names: Vec<_> = acc.wait_snapshot().into_iter().collect();
        assert_eq!(names[0].0.workload, "_system");
        assert_eq!(names[0].1.by_cause, [0, 0, 7, 0]);
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
        acc.update_pairs(pairs(&[(1, 101, 30 * MS)]), resolve, &HashSet::new());
        assert!(!waits(&acc).contains_key("web"));
        assert_eq!(acc.pending_pairs.len(), 1);
        resolved.set(true);
        acc.update_pairs(pairs(&[(1, 101, 50 * MS)]), resolve, &HashSet::new());
        let web = &waits(&acc)["web"];
        assert_eq!(web.by_cause, [0, 50 * MS, 0, 0]);
        assert_eq!(by_culprit(web), [("late", 50 * MS), ("_other", 0)]);
    }

    #[test]
    fn unresolved_pair_drops_after_ten_reads() {
        let mut acc = accumulator(1500);
        update_pairs(&mut acc, &[(101, 1, 30 * MS)]);
        for _ in 1..PENDING_READS {
            assert_eq!(acc.pending_pairs.len(), 1);
            update_pairs(&mut acc, &[]);
        }
        assert!(acc.pending_pairs.is_empty());
        assert!(acc
            .wait_snapshot()
            .iter()
            .all(|(_, w)| w.by_cause == [0; 4]));
    }

    #[test]
    fn unresolved_idle_pair_ages_once_per_read() {
        let mut acc = accumulator(1500);
        for _ in 1..PENDING_READS {
            update_pairs(&mut acc, &[(1, 101, 30 * MS)]);
            assert_eq!(acc.pending_pairs.len(), 1);
        }
        update_pairs(&mut acc, &[(1, 101, 30 * MS)]);
        assert!(acc.pending_pairs.is_empty());
    }

    #[test]
    fn culprit_series_take_budget_and_overflow_to_other() {
        let mut acc = accumulator(3 * COST + 1);
        update(&mut acc, &[(1, hist(&[1], 1))]);
        update_pairs(&mut acc, &[(1, 2, 100 * MS), (1, 3, 100 * MS)]);
        assert_eq!(acc.cardinality().series(), 3 * COST + 1);
        let web = &waits(&acc)["web"];
        assert_eq!(web.by_culprit.len(), 2);
        assert_eq!(web.by_culprit[1], (Entity::other(), 100 * MS));
    }

    #[test]
    fn culprit_series_go_one_read_after_the_culprit_leaves() {
        let mut acc = accumulator(1500);
        let live = HashSet::from([entity("web"), entity("api")]);
        acc.update(read(&[(1, hist(&[1], 1))]), resolve, &live);
        acc.update_pairs(pairs(&[(1, 2, 100 * MS)]), resolve, &live);
        let series = acc.cardinality().series();
        let live = HashSet::from([entity("web")]);
        acc.update_pairs(pairs(&[(1, 2, 100 * MS)]), resolve, &live);
        acc.update_pairs(pairs(&[]), resolve, &live);
        assert_eq!(by_culprit(&waits(&acc)["web"])[0].0, "api", "one more read");
        acc.update_pairs(pairs(&[]), resolve, &live);
        assert_eq!(by_culprit(&waits(&acc)["web"]), [("_other", 0)]);
        assert_eq!(acc.cardinality().series(), series - 1);
    }

    #[test]
    fn victim_frees_its_culprit_series() {
        let mut acc = accumulator(1500);
        update(&mut acc, &[(1, hist(&[1], 1))]);
        update_pairs(&mut acc, &[(1, 2, 100 * MS), (1, 3, 100 * MS)]);
        assert_eq!(acc.cardinality().series(), 3 * COST + 2);
        update(&mut acc, &[]);
        update(&mut acc, &[]);
        assert!(!waits(&acc).contains_key("web"));
        assert_eq!(acc.cardinality().series(), 2 * COST);
    }
}
