//! Run queue latency on a real kernel. The test needs Linux, root, BTF and cgroup v2, so it is
//! ignored by default. `make integration` runs it.
//!
//! A victim and a hog run in separate cgroups on one CPU. Each is a child process that runs the
//! `workload` test of this binary, because a domain cgroup moves whole processes. The victim must
//! wait longer under the hog, and must blame the hog for that wait. Then a task under a CPU
//! quota must show its wait as throttled.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use observer::bpf::Probes;
use observer::cardinality::Cardinality;
use observer::cgroup::{self, Resolver};
use observer::config::{Config, Granularity};
use observer::runq::{Accumulator, Owner};
use observer_common::{Pair, RunqHist, IDLE_CG, THROTTLED_CG};

const ROLE: &str = "NNO_TEST_ROLE";
const CGROUP: &str = "NNO_TEST_CGROUP";
const CPU: &str = "NNO_TEST_CPU";
const HOG_THREADS: usize = 4;
const WINDOW: Duration = Duration::from_secs(3);

#[test]
#[ignore = "needs Linux, root, BTF and cgroup v2: run make integration"]
fn victim_waits_longer_under_a_hog() {
    let cpu = last_allowed_cpu();
    let victim_cgroup = Cgroup::create("victim");
    let hog_cgroup = Cgroup::create("hog");
    let victim_id = victim_cgroup.id();
    let hog_id = hog_cgroup.id();
    let config = Config::from_env().unwrap();
    let probes = Probes::load(&config).expect("load and attach");
    let cardinality = Cardinality::new(config.series_budget, config.series_per_entity());
    let mut accumulator = Accumulator::new(
        Granularity::Workload,
        cardinality,
        config.culprits_per_victim,
    );
    let mut resolver = Resolver::new(&config.cgroup_root);
    let mut read = || {
        let read = probes.read_runq().unwrap();
        resolver.refresh(read.keys().copied()).unwrap();
        // No pod watch here, so only the cgroups outside pods resolve.
        let resolve = |id| match resolver.get(id) {
            Some(cgroup::Cgroup::System) => Owner::System,
            _ => Owner::Unresolved(None),
        };
        let pairs = probes.read_pairs().unwrap();
        accumulator.update(read.clone(), Some(pairs.clone()), resolve, &HashSet::new());
        let mut node = RunqHist::default();
        for (_, hist) in accumulator.snapshot() {
            node.bins
                .iter_mut()
                .zip(hist.bins)
                .for_each(|(n, c)| *n += c);
        }
        ((read, pairs), node, resolver.get(victim_id).cloned())
    };

    let victim_load = Workload::spawn("victim", &victim_cgroup, cpu);
    std::thread::sleep(Duration::from_secs(1));
    let ((start, _), node_start, _) = read();
    std::thread::sleep(WINDOW);
    let ((end, _), node_mid, victim) = read();
    assert_eq!(
        victim,
        Some(cgroup::Cgroup::System),
        "a cgroup outside a pod"
    );
    assert!(
        end.contains_key(&victim_id),
        "no RUNQ entry for the victim cgroup id {victim_id} (inode of {:?})",
        victim_cgroup.path
    );
    let baseline = mean(&start, &end, victim_id);

    let hog_load = Workload::spawn("hog", &hog_cgroup, cpu);
    std::thread::sleep(Duration::from_secs(1));
    let ((start, pairs_start), _, _) = read();
    std::thread::sleep(WINDOW);
    let ((end, pairs_end), node_end, _) = read();
    let contended = mean(&start, &end, victim_id);
    let window = delta(&pairs_start, &pairs_end);
    let blame = |victim_cg, culprit_cg| {
        let pair = Pair {
            victim_cg,
            culprit_cg,
        };
        window.get(&pair).copied().unwrap_or_default()
    };
    // Shares count only attributed wait, so the idle and throttled causes stay out of the total.
    let share = |victim_cg, culprit_cg| {
        let attributed = |c| c != IDLE_CG && c != THROTTLED_CG;
        let total: u64 = window
            .iter()
            .filter(|(pair, _)| pair.victim_cg == victim_cg && attributed(pair.culprit_cg))
            .map(|(_, ns)| ns)
            .sum();
        blame(victim_cg, culprit_cg) as f64 / total.max(1) as f64
    };
    let top = |victim_cg| {
        let culprits = window.keys().filter(|pair| pair.victim_cg == victim_cg);
        culprits
            .map(|pair| pair.culprit_cg)
            .max_by_key(|culprit| blame(victim_cg, *culprit))
    };
    let ms = |ns: u64| ns / 1_000_000;

    println!("victim mean wait: baseline {baseline:?}, under the hog {contended:?}");
    println!(
        "wait in {WINDOW:?} in ms: victim behind the hog {}, victim behind itself {}, \
         hog behind the victim {}, hog behind itself {}",
        ms(blame(victim_id, hog_id)),
        ms(blame(victim_id, victim_id)),
        ms(blame(hog_id, victim_id)),
        ms(blame(hog_id, hog_id)),
    );
    let victim_share = share(victim_id, hog_id);
    let hog_share = share(hog_id, victim_id);
    println!("share of the other cgroup: victim {victim_share:.2}, hog {hog_share:.2} (info)");
    assert!(
        contended > baseline * 4 && contended > baseline + Duration::from_micros(500),
        "baseline {baseline:?}, under the hog {contended:?}"
    );
    // The CPU heavy victim is a real neighbour to the hog threads, so the hog share varies with
    // CPU speed and thread order. Only the victim side is an invariant.
    assert_eq!(top(victim_id), Some(hog_id), "the hog is the top culprit");
    assert!(victim_share >= 0.9, "victim {victim_share:.2}");
    // Each sample goes to RUNQ and to one pair, so the sums match up to the dropped samples and
    // the samples between the two map reads.
    let runq_ns = end[&victim_id].sum_ns - start[&victim_id].sum_ns;
    let pairs_ns: u64 = window
        .iter()
        .filter(|(pair, _)| pair.victim_cg == victim_id)
        .map(|(_, ns)| ns)
        .sum();
    let dropped = probes.read_dropped().unwrap();
    println!("victim wait: RUNQ {runq_ns} ns, pairs {pairs_ns} ns, dropped {dropped:?}");
    assert!(dropped.iter().all(|(_, count)| *count == 0));
    assert!(
        runq_ns.abs_diff(pairs_ns) * 100 < runq_ns,
        "RUNQ {runq_ns} ns, pairs {pairs_ns} ns"
    );
    assert!(count(&node_start) > 0);
    assert!(count(&node_mid) > count(&node_start));
    assert!(count(&node_end) > count(&node_mid));

    // Alone on the CPU under 10 ms per 100 ms, the task waits mostly in throttle, and no other
    // cgroup takes the blame for it.
    drop(victim_load);
    drop(hog_load);
    enable_cpu_controller();
    let throttled_cgroup = Cgroup::create("throttled");
    std::fs::write(throttled_cgroup.path.join("cpu.max"), "10000 100000").unwrap();
    let throttled_id = throttled_cgroup.id();
    let _spin = Workload::spawn("spin", &throttled_cgroup, cpu);
    std::thread::sleep(Duration::from_secs(1));
    let ((_, pairs_start), _, _) = read();
    std::thread::sleep(WINDOW);
    let ((_, pairs_end), _, _) = read();
    let waits: Vec<(u64, u64)> = delta(&pairs_start, &pairs_end)
        .into_iter()
        .filter(|(pair, _)| pair.victim_cg == throttled_id)
        .map(|(pair, ns)| (pair.culprit_cg, ns))
        .collect();
    let total: u64 = waits.iter().map(|(_, ns)| ns).sum();
    let sum_of = |keep: &dyn Fn(u64) -> bool| -> u64 {
        waits
            .iter()
            .filter(|(c, _)| keep(*c))
            .map(|(_, ns)| ns)
            .sum()
    };
    let throttled = sum_of(&|c| c == THROTTLED_CG);
    let others = sum_of(&|c| ![THROTTLED_CG, IDLE_CG, throttled_id].contains(&c));
    println!(
        "throttled task wait in {WINDOW:?} in ms: total {}, throttled {}, other cgroups {}",
        ms(total),
        ms(throttled),
        ms(others),
    );
    assert!(
        throttled * 10 >= total * 8,
        "throttled {throttled} of {total} ns"
    );
    assert!(others * 10 <= total, "other cgroups {others} of {total} ns");
}

/// The wait of each pair between two reads.
fn delta(start: &HashMap<Pair, u64>, end: &HashMap<Pair, u64>) -> HashMap<Pair, u64> {
    end.iter()
        .map(|(pair, ns)| (*pair, ns - start.get(pair).copied().unwrap_or_default()))
        .collect()
}

/// A quota needs the cpu controller in the children of the cgroup root.
fn enable_cpu_controller() {
    let control = "/sys/fs/cgroup/cgroup.subtree_control";
    if let Err(error) = std::fs::write(control, "+cpu") {
        panic!("enabling the cpu controller in {control} failed: {error}");
    }
}

/// The body of a child process. Returns at once in the parent, where `NNO_TEST_ROLE` is unset.
#[test]
#[ignore = "runs as a child of victim_waits_longer_under_a_hog"]
fn workload() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let cgroup = PathBuf::from(std::env::var(CGROUP).unwrap());
    let cpu: usize = std::env::var(CPU).unwrap().parse().unwrap();
    std::fs::write(cgroup.join("cgroup.procs"), std::process::id().to_string()).unwrap();
    pin_to(cpu);
    match role.as_str() {
        // A light sleeper wins on scheduler credit, so the victim must do real work.
        "victim" => loop {
            spin_cpu(Duration::from_millis(5));
            std::thread::sleep(Duration::from_millis(1));
        },
        "spin" => loop {
            spin_cpu(Duration::from_secs(1));
        },
        "hog" => {
            let threads: Vec<_> = (0..HOG_THREADS)
                .map(|_| {
                    std::thread::spawn(|| loop {
                        spin_cpu(Duration::from_secs(1));
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
        }
        other => panic!("unknown role {other}"),
    }
}

struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    fn create(name: &str) -> Self {
        let path = PathBuf::from(format!(
            "/sys/fs/cgroup/nno-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }

    /// The kernel uses the inode of a cgroup v2 directory as the cgroup id.
    fn id(&self) -> u64 {
        std::fs::metadata(&self.path).unwrap().ino()
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // The kernel empties the cgroup a moment after the last process exits.
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::remove_dir(&self.path).is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct Workload {
    child: Child,
}

impl Workload {
    fn spawn(role: &str, cgroup: &Cgroup, cpu: usize) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["workload", "--exact", "--ignored", "--nocapture"])
            .env(ROLE, role)
            .env(CGROUP, &cgroup.path)
            .env(CPU, cpu.to_string())
            .spawn()
            .unwrap();
        let workload = Self { child };
        let procs = cgroup.path.join("cgroup.procs");
        let pid = workload.child.id().to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&procs)
            .unwrap()
            .lines()
            .any(|l| l == pid)
        {
            assert!(Instant::now() < deadline, "{role} did not join {procs:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        workload
    }
}

impl Drop for Workload {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mean(start: &HashMap<u64, RunqHist>, end: &HashMap<u64, RunqHist>, id: u64) -> Duration {
    let before = start.get(&id).copied().unwrap_or_default();
    let after = end.get(&id).copied().unwrap_or_default();
    let samples = count(&after) - count(&before);
    assert!(samples > 0, "no samples for cgroup {id}");
    Duration::from_nanos((after.sum_ns - before.sum_ns) / samples)
}

fn count(hist: &RunqHist) -> u64 {
    hist.bins.iter().sum()
}

fn last_allowed_cpu() -> usize {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set),
            0
        );
        (0..libc::CPU_SETSIZE as usize)
            .rev()
            .find(|&cpu| libc::CPU_ISSET(cpu, &set))
            .unwrap()
    }
}

fn pin_to(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set),
            0
        );
    }
}

/// Burns `budget` of CPU time on this thread. Wall time is longer when the thread waits.
fn spin_cpu(budget: Duration) {
    let start = thread_cpu_time();
    while thread_cpu_time() - start < budget {
        std::hint::spin_loop();
    }
}

fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) },
        0
    );
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}
