//! The series budget. An entity that does not fit goes to `_overflow` for its whole life on the
//! node, because a move to its own series later would make the `_overflow` counters fall. A
//! culprit series takes one series beyond the entity cost, and the same rule applies to it.

use std::collections::HashMap;
use std::hash::Hash;

/// Where the samples of an entity go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Own,
    Overflow,
}

#[derive(Debug)]
pub struct Cardinality<K> {
    budget: usize,
    /// Series of one entity.
    cost: usize,
    used: usize,
    slots: HashMap<K, Slot>,
}

impl<K: Hash + Eq> Cardinality<K> {
    /// Reserves the series of `_system` and `_overflow`, which always exist.
    pub fn new(budget: usize, cost: usize) -> Self {
        Self {
            budget,
            cost,
            used: 2 * cost,
            slots: HashMap::new(),
        }
    }

    /// The slot of `key`. A new key gets its own series only if they fit in the budget, and
    /// keeps the slot until `release`.
    pub fn assign(&mut self, key: K) -> Slot {
        *self.slots.entry(key).or_insert_with(|| {
            if self.used + self.cost <= self.budget {
                self.used += self.cost;
                Slot::Own
            } else {
                Slot::Overflow
            }
        })
    }

    pub fn slot(&self, key: &K) -> Option<Slot> {
        self.slots.get(key).copied()
    }

    /// Forgets `key` and frees its series.
    pub fn release(&mut self, key: &K) {
        if self.slots.remove(key) == Some(Slot::Own) {
            self.used -= self.cost;
        }
    }

    /// Takes one series beyond the entity cost, if it fits in the budget.
    pub fn take_series(&mut self) -> bool {
        let fits = self.used < self.budget;
        if fits {
            self.used += 1;
        }
        fits
    }

    /// Frees series that `take_series` took.
    pub fn free_series(&mut self, count: usize) {
        self.used -= count;
    }

    /// Per entity series, the reserved ones and the culprit series included.
    pub fn series(&self) -> usize {
        self.used
    }

    pub fn overflowed(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| **slot == Slot::Overflow)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COST: usize = 16;

    #[test]
    fn five_hundred_workloads_stay_in_budget() {
        let mut budget = Cardinality::new(1500, COST);
        let slots: Vec<Slot> = (0..500).map(|w| budget.assign(w)).collect();
        let own = slots.iter().filter(|slot| **slot == Slot::Own).count();
        assert_eq!(own, (1500 - 2 * COST) / COST);
        assert!(budget.series() <= 1500);
        assert_eq!(budget.series(), (own + 2) * COST);
        assert_eq!(budget.overflowed(), 500 - own);
        // The first workloads to arrive keep their own series.
        assert!(slots[..own].iter().all(|slot| *slot == Slot::Own));
    }

    #[test]
    fn assignments_never_move() {
        let mut budget = Cardinality::new(1500, COST);
        let first: Vec<Slot> = (0..500).map(|w| budget.assign(w)).collect();
        // Releases free series, yet a workload in _overflow stays there.
        for w in 0..10 {
            budget.release(&w);
        }
        for _ in 0..3 {
            let again: Vec<Slot> = (10..500).map(|w| budget.assign(w)).collect();
            assert_eq!(again, first[10..]);
        }
        // New workloads take the freed series.
        assert_eq!(budget.assign(1000), Slot::Own);
        assert!(budget.series() <= 1500);
    }

    #[test]
    fn release_frees_only_own_series() {
        let mut budget = Cardinality::new(4 * COST, COST);
        assert_eq!(budget.assign("a"), Slot::Own);
        assert_eq!(budget.assign("b"), Slot::Own);
        assert_eq!(budget.assign("c"), Slot::Overflow);
        budget.release(&"c");
        assert_eq!(budget.series(), 4 * COST);
        budget.release(&"a");
        assert_eq!(budget.series(), 3 * COST);
        assert_eq!(budget.overflowed(), 0);
        assert_eq!(budget.assign("d"), Slot::Own);
    }

    #[test]
    fn extra_series_share_the_budget() {
        let mut budget = Cardinality::new(3 * COST + 2, COST);
        assert_eq!(budget.assign("a"), Slot::Own);
        assert!(budget.take_series());
        assert!(budget.take_series());
        assert!(!budget.take_series());
        assert_eq!(budget.series(), 3 * COST + 2);
        assert_eq!(budget.assign("b"), Slot::Overflow);
        budget.free_series(2);
        assert_eq!(budget.series(), 3 * COST);
    }

    #[test]
    fn reserved_budget_overflows_everything() {
        let mut budget = Cardinality::new(2 * COST, COST);
        assert_eq!(budget.assign(1), Slot::Overflow);
        assert_eq!(budget.series(), 2 * COST);
    }
}
