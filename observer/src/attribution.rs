//! Blame for the run queue wait of one victim entity. Each wait has a culprit workload: the task
//! that preempted the victim, or after a wakeup the last occupant of the CPU. See "Culprit
//! attribution" in `CLAUDE.md`.

use std::collections::{BTreeMap, HashMap};

use crate::workload::Entity;

/// Weight that the recent wait keeps at each read, so the window spans about 10 reads.
const DECAY: f64 = 0.9;

/// Share of the recent wait of a victim that a new culprit needs for its own series.
const MIN_SHARE: f64 = 0.1;

/// Recent wait that a new culprit needs for its own series, so that the small waits of a quiet
/// victim cannot take its slots.
const MIN_RECENT_NS: f64 = 10_000_000.0;

/// The agent forgets a recent wait below this, so only active culprits use memory.
const FORGET_NS: f64 = 1_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    SameWorkload,
    OtherWorkload,
    System,
    Idle,
}

impl Cause {
    pub const ALL: [Cause; 4] = [
        Cause::SameWorkload,
        Cause::OtherWorkload,
        Cause::System,
        Cause::Idle,
    ];

    /// `culprit` is at workload level. The pod and the container of `victim` do not count.
    pub fn of(victim: &Entity, culprit: &Entity) -> Self {
        if culprit.is_idle() {
            Cause::Idle
        } else if culprit.is_system() {
            Cause::System
        } else if victim.same_workload(culprit) {
            Cause::SameWorkload
        } else {
            Cause::OtherWorkload
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Cause::SameWorkload => "same_workload",
            Cause::OtherWorkload => "other_workload",
            Cause::System => "system",
            Cause::Idle => "idle",
        }
    }
}

/// The wait totals of one victim in nanoseconds, as the metrics show them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wait {
    /// In the order of `Cause::ALL`.
    pub by_cause: [u64; 4],
    /// The culprits with their own series, then `_other`. The idle wait is not here.
    pub by_culprit: Vec<(Entity, u64)>,
}

#[derive(Debug, Default)]
pub struct Blame {
    by_cause: [u64; 4],
    /// Culprits with their own series. A culprit keeps its series until it or the victim leaves
    /// the node, because a move to `_other` would make its counter fall.
    culprits: BTreeMap<Entity, u64>,
    other: u64,
    /// Decayed wait by culprit, which decides who gets a free slot.
    recent: HashMap<Entity, f64>,
    recent_total: f64,
    /// Increases of the current read, which `end_read` adds to the series.
    read: HashMap<Entity, u64>,
}

impl Blame {
    /// An idle wait counts only by cause, because no workload took the CPU.
    pub fn add(&mut self, cause: Cause, culprit: Entity, wait_ns: u64) {
        self.by_cause[cause as usize] += wait_ns;
        if cause != Cause::Idle {
            *self.read.entry(culprit).or_default() += wait_ns;
        }
    }

    /// Ages the recent wait and adds the increases of the read to the series. A new culprit gets
    /// its own series only while fewer than `limit` culprits have one, it caused a meaningful
    /// share of the recent wait, and `take` finds room in the budget. Else it goes to `_other`.
    pub fn end_read(&mut self, limit: usize, mut take: impl FnMut() -> bool) {
        self.recent_total *= DECAY;
        self.recent.retain(|_, ns| {
            *ns *= DECAY;
            *ns >= FORGET_NS
        });
        if self.recent.is_empty() {
            self.recent_total = 0.0;
        }
        let mut read: Vec<(Entity, u64, f64)> = Vec::with_capacity(self.read.len());
        for (culprit, ns) in self.read.drain() {
            let recent = self.recent.entry(culprit.clone()).or_default();
            *recent += ns as f64;
            self.recent_total += ns as f64;
            read.push((culprit, ns, *recent));
        }
        // The largest recent culprits first, so they take the free slots.
        read.sort_by(|a, b| b.2.total_cmp(&a.2));
        for (culprit, ns, recent) in read {
            if let Some(total) = self.culprits.get_mut(&culprit) {
                *total += ns;
                continue;
            }
            let admit = self.culprits.len() < limit
                && recent >= MIN_RECENT_NS
                && recent >= MIN_SHARE * self.recent_total
                && take();
            if admit {
                self.culprits.insert(culprit, ns);
            } else {
                self.other += ns;
            }
        }
    }

    /// Drops the series of a culprit that left the node. Returns whether it had one.
    pub fn forget(&mut self, culprit: &Entity) -> bool {
        self.recent.remove(culprit);
        self.culprits.remove(culprit).is_some()
    }

    /// The culprits with their own series.
    pub fn culprits(&self) -> impl Iterator<Item = &Entity> {
        self.culprits.keys()
    }

    pub fn wait(&self) -> Wait {
        let by_culprit = self
            .culprits
            .iter()
            .map(|(culprit, ns)| (culprit.clone(), *ns))
            .chain([(Entity::other(), self.other)])
            .collect();
        Wait {
            by_cause: self.by_cause,
            by_culprit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Granularity;
    use crate::workload::Workload;

    const MS: u64 = 1_000_000;

    fn workload(name: &str) -> Entity {
        let workload = Workload {
            kind: "Deployment".into(),
            name: name.into(),
        };
        Entity::new(Granularity::Workload, "shop", &workload, "", "")
    }

    fn read(blame: &mut Blame, waits: &[(&str, u64)], limit: usize, budget: &mut usize) {
        for (name, ns) in waits {
            blame.add(Cause::OtherWorkload, workload(name), *ns);
        }
        blame.end_read(limit, || {
            let fits = *budget > 0;
            *budget = budget.saturating_sub(1);
            fits
        });
    }

    fn named(blame: &Blame) -> Vec<String> {
        blame.culprits().map(|c| c.workload.clone()).collect()
    }

    #[test]
    fn cause_follows_the_culprit_workload() {
        let web = Entity::new(
            Granularity::Pod,
            "shop",
            &Workload {
                kind: "Deployment".into(),
                name: "web".into(),
            },
            "web-1",
            "",
        );
        assert_eq!(Cause::of(&web, &workload("web")), Cause::SameWorkload);
        assert_eq!(Cause::of(&web, &workload("api")), Cause::OtherWorkload);
        let system = Entity::system(Granularity::Workload);
        assert_eq!(Cause::of(&web, &system), Cause::System);
        assert_eq!(Cause::of(&system, &system), Cause::System);
        assert_eq!(Cause::of(&web, &Entity::idle()), Cause::Idle);
    }

    #[test]
    fn small_culprits_cannot_take_the_slots_before_the_hog() {
        let mut blame = Blame::default();
        let mut budget = usize::MAX;
        let small: Vec<String> = (0..20).map(|i| format!("small-{i}")).collect();
        let waits: Vec<(&str, u64)> = small.iter().map(|n| (n.as_str(), MS / 10)).collect();
        for _ in 0..30 {
            read(&mut blame, &waits, 3, &mut budget);
        }
        assert!(named(&blame).is_empty(), "{:?}", named(&blame));
        let mut with_hog = waits.clone();
        with_hog.push(("hog", 50 * MS));
        read(&mut blame, &with_hog, 3, &mut budget);
        assert_eq!(named(&blame), ["hog"]);
        assert_eq!(blame.wait().by_culprit[0], (workload("hog"), 50 * MS));
    }

    #[test]
    fn culprit_needs_a_meaningful_share() {
        let mut blame = Blame::default();
        let mut budget = usize::MAX;
        read(
            &mut blame,
            &[("hog", 900 * MS), ("minor", 50 * MS)],
            3,
            &mut budget,
        );
        assert_eq!(named(&blame), ["hog"]);
        read(&mut blame, &[("minor", 200 * MS)], 3, &mut budget);
        assert_eq!(named(&blame), ["hog", "minor"]);
    }

    #[test]
    fn series_never_move() {
        let mut blame = Blame::default();
        let mut budget = usize::MAX;
        read(&mut blame, &[("a", 100 * MS)], 2, &mut budget);
        read(&mut blame, &[("b", 100 * MS)], 2, &mut budget);
        let mut last = blame.wait();
        assert_eq!(named(&blame), ["a", "b"]);
        // A larger culprit arrives, and a and b fall below every threshold.
        for _ in 0..50 {
            read(
                &mut blame,
                &[("a", 1), ("b", 1), ("c", 500 * MS)],
                2,
                &mut budget,
            );
            let now = blame.wait();
            assert_eq!(named(&blame), ["a", "b"]);
            for ((culprit, before), (same, after)) in last.by_culprit.iter().zip(&now.by_culprit) {
                assert_eq!(culprit, same);
                assert!(after >= before, "{culprit:?} fell");
            }
            last = now;
        }
        assert_eq!(last.by_culprit[2], (Entity::other(), 50 * 500 * MS));
        // A slot frees when a culprit leaves, and c takes it from zero.
        assert!(blame.forget(&workload("a")));
        read(&mut blame, &[("c", 500 * MS)], 2, &mut budget);
        assert_eq!(named(&blame), ["b", "c"]);
        let wait = blame.wait();
        assert_eq!(wait.by_culprit[1], (workload("c"), 500 * MS));
        assert_eq!(wait.by_culprit[2].1, 50 * 500 * MS);
    }

    #[test]
    fn idle_wait_counts_only_by_cause() {
        let mut blame = Blame::default();
        blame.add(Cause::Idle, Entity::idle(), 500 * MS);
        blame.add(Cause::OtherWorkload, workload("hog"), 50 * MS);
        blame.end_read(3, || true);
        let wait = blame.wait();
        assert_eq!(wait.by_cause, [0, 50 * MS, 0, 500 * MS]);
        let by_culprit: Vec<_> = wait.by_culprit.iter().map(|(c, _)| &c.workload).collect();
        assert_eq!(by_culprit, ["hog", "_other"]);
    }

    #[test]
    fn full_budget_sends_new_culprits_to_other() {
        let mut blame = Blame::default();
        let mut budget = 1;
        read(&mut blame, &[("a", 100 * MS)], 3, &mut budget);
        read(&mut blame, &[("b", 100 * MS)], 3, &mut budget);
        assert_eq!(named(&blame), ["a"]);
        let wait = blame.wait();
        assert_eq!(wait.by_culprit.last(), Some(&(Entity::other(), 100 * MS)));
        assert_eq!(wait.by_cause, [0, 200 * MS, 0, 0]);
    }
}
