//! Process config from `NNO_*` environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use observer_common::MAX_BOUNDS;

const DEFAULT_BIND_ADDR: &str = "[::]:9400";
const DEFAULT_READ_INTERVAL: &str = "5s";
const DEFAULT_MAX_CGROUPS: &str = "1024";
const DEFAULT_HISTOGRAM_BOUNDS: &str = "10us,100us,500us,1ms,5ms,10ms,100ms,1s";
const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup";
const DEFAULT_SERIES_BUDGET: &str = "1500";
const DEFAULT_CULPRITS_PER_VICTIM: &str = "3";
const DEFAULT_MAX_PAIRS: &str = "16384";

/// The labels that name an entity, the unit that gets its own series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Workload,
    Pod,
    Container,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub read_interval: Duration,
    /// Size of the per cgroup map. The least recently updated cgroup drops out when it is full.
    pub max_cgroups: u32,
    /// Strictly increasing, at most `MAX_BOUNDS`.
    pub histogram_bounds: Vec<Duration>,
    /// The node whose pods the agent watches. The probes need it, so only Linux requires it.
    pub node_name: Option<String>,
    pub cgroup_root: PathBuf,
    pub granularity: Granularity,
    /// The most per entity series the agent exports.
    pub series_budget: usize,
    /// Culprit workloads with their own series for each victim, at least 1.
    pub culprits_per_victim: usize,
    /// Size of the map of victim and culprit cgroup pairs.
    pub max_pairs: u32,
}

impl Config {
    /// Series that each entity always has: the histogram buckets with `+Inf`, sum and count, the
    /// 4 cause series and the `_other` culprit series. Each culprit series adds one more.
    pub fn series_per_entity(&self) -> usize {
        self.histogram_bounds.len() + 3 + 4 + 1
    }
}

impl Config {
    /// Fails on an invalid value, so a typo stops the pod at startup.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |key: &str, default: &str| get(key).unwrap_or_else(|| default.to_string());

        let raw = var("NNO_BIND_ADDR", DEFAULT_BIND_ADDR);
        let bind_addr = raw
            .parse()
            .with_context(|| format!("NNO_BIND_ADDR is not a socket address: {raw}"))?;

        let raw = var("NNO_READ_INTERVAL", DEFAULT_READ_INTERVAL);
        let read_interval =
            parse_duration(&raw).with_context(|| format!("NNO_READ_INTERVAL is invalid: {raw}"))?;
        if read_interval.is_zero() {
            bail!("NNO_READ_INTERVAL must be above zero");
        }

        let raw = var("NNO_MAX_CGROUPS", DEFAULT_MAX_CGROUPS);
        let max_cgroups: u32 = raw
            .parse()
            .with_context(|| format!("NNO_MAX_CGROUPS is not a number: {raw}"))?;
        if max_cgroups == 0 {
            bail!("NNO_MAX_CGROUPS must be above zero");
        }

        let raw = var("NNO_HISTOGRAM_BOUNDS", DEFAULT_HISTOGRAM_BOUNDS);
        let histogram_bounds = parse_bounds(&raw)
            .with_context(|| format!("NNO_HISTOGRAM_BOUNDS is invalid: {raw}"))?;

        let node_name = get("NNO_NODE_NAME").filter(|name| !name.is_empty());
        let cgroup_root = PathBuf::from(var("NNO_CGROUP_ROOT", DEFAULT_CGROUP_ROOT));

        let raw = var("NNO_GRANULARITY", "workload");
        let granularity = match raw.as_str() {
            "workload" => Granularity::Workload,
            "pod" => Granularity::Pod,
            "container" => Granularity::Container,
            _ => bail!("NNO_GRANULARITY must be workload, pod or container: {raw}"),
        };

        let raw = var("NNO_SERIES_BUDGET", DEFAULT_SERIES_BUDGET);
        let series_budget: usize = raw
            .parse()
            .with_context(|| format!("NNO_SERIES_BUDGET is not a number: {raw}"))?;

        let raw = var("NNO_CULPRITS_PER_VICTIM", DEFAULT_CULPRITS_PER_VICTIM);
        let culprits_per_victim: usize = raw
            .parse()
            .with_context(|| format!("NNO_CULPRITS_PER_VICTIM is not a number: {raw}"))?;
        if culprits_per_victim == 0 {
            bail!("NNO_CULPRITS_PER_VICTIM must be at least 1");
        }

        let raw = var("NNO_MAX_PAIRS", DEFAULT_MAX_PAIRS);
        let max_pairs: u32 = raw
            .parse()
            .with_context(|| format!("NNO_MAX_PAIRS is not a number: {raw}"))?;
        if max_pairs == 0 {
            bail!("NNO_MAX_PAIRS must be above zero");
        }

        let config = Self {
            bind_addr,
            read_interval,
            max_cgroups,
            histogram_bounds,
            node_name,
            cgroup_root,
            granularity,
            series_budget,
            culprits_per_victim,
            max_pairs,
        };
        // The _system and _overflow entities always hold their series.
        let reserved = 2 * config.series_per_entity();
        if config.series_budget < reserved {
            bail!("NNO_SERIES_BUDGET must be at least {reserved} for these histogram bounds");
        }
        Ok(config)
    }
}

fn parse_bounds(raw: &str) -> anyhow::Result<Vec<Duration>> {
    let bounds = raw
        .split(',')
        .map(|item| parse_duration(item.trim()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if bounds.len() > MAX_BOUNDS {
        bail!("{} bounds, the limit is {MAX_BOUNDS}", bounds.len());
    }
    if bounds[0].is_zero() {
        bail!("the first bound must be above zero");
    }
    if bounds.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("bounds must be strictly increasing");
    }
    Ok(bounds)
}

/// An integer with a unit: `ns`, `us`, `ms` or `s`.
fn parse_duration(raw: &str) -> anyhow::Result<Duration> {
    let split = raw
        .find(|c: char| !c.is_ascii_digit())
        .with_context(|| format!("{raw:?} has no unit (ns, us, ms, s)"))?;
    let (number, unit) = raw.split_at(split);
    let number: u64 = number
        .parse()
        .with_context(|| format!("{raw:?} does not start with a number"))?;
    match unit {
        "ns" => Ok(Duration::from_nanos(number)),
        "us" => Ok(Duration::from_micros(number)),
        "ms" => Ok(Duration::from_millis(number)),
        "s" => Ok(Duration::from_secs(number)),
        _ => bail!("{raw:?} has an unknown unit, use ns, us, ms or s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn defaults() {
        let config = Config::from_lookup(|_| None).unwrap();
        assert_eq!(config.bind_addr.to_string(), "[::]:9400");
        assert_eq!(config.read_interval, Duration::from_secs(5));
        assert_eq!(config.max_cgroups, 1024);
        assert_eq!(config.histogram_bounds.len(), 8);
        assert_eq!(config.histogram_bounds[0], Duration::from_micros(10));
        assert_eq!(config.histogram_bounds[7], Duration::from_secs(1));
        assert_eq!(config.node_name, None);
        assert_eq!(config.cgroup_root, PathBuf::from("/sys/fs/cgroup"));
        assert_eq!(config.granularity, Granularity::Workload);
        assert_eq!(config.series_budget, 1500);
        assert_eq!(config.series_per_entity(), 16);
        assert_eq!(config.culprits_per_victim, 3);
        assert_eq!(config.max_pairs, 16384);
    }

    #[test]
    fn env_overrides_default() {
        let config = Config::from_lookup(lookup(&[
            ("NNO_BIND_ADDR", "127.0.0.1:8080"),
            ("NNO_READ_INTERVAL", "250ms"),
            ("NNO_MAX_CGROUPS", "64"),
            ("NNO_HISTOGRAM_BOUNDS", "500ns, 2us,1ms"),
            ("NNO_NODE_NAME", "node-a"),
            ("NNO_CGROUP_ROOT", "/host/sys/fs/cgroup"),
            ("NNO_GRANULARITY", "container"),
            ("NNO_SERIES_BUDGET", "22"),
            ("NNO_CULPRITS_PER_VICTIM", "1"),
            ("NNO_MAX_PAIRS", "128"),
        ]))
        .unwrap();
        assert_eq!(config.bind_addr.port(), 8080);
        assert_eq!(config.read_interval, Duration::from_millis(250));
        assert_eq!(config.max_cgroups, 64);
        assert_eq!(
            config.histogram_bounds,
            [
                Duration::from_nanos(500),
                Duration::from_micros(2),
                Duration::from_millis(1)
            ]
        );
        assert_eq!(config.node_name.as_deref(), Some("node-a"));
        assert_eq!(config.cgroup_root, PathBuf::from("/host/sys/fs/cgroup"));
        assert_eq!(config.granularity, Granularity::Container);
        assert_eq!(config.series_budget, 22);
        assert_eq!(config.culprits_per_victim, 1);
        assert_eq!(config.max_pairs, 128);
    }

    #[test]
    fn empty_node_name_is_unset() {
        let config = Config::from_lookup(lookup(&[("NNO_NODE_NAME", "")])).unwrap();
        assert_eq!(config.node_name, None);
    }

    #[test]
    fn granularity_values() {
        for (raw, granularity) in [
            ("workload", Granularity::Workload),
            ("pod", Granularity::Pod),
            ("container", Granularity::Container),
        ] {
            let config = Config::from_lookup(lookup(&[("NNO_GRANULARITY", raw)])).unwrap();
            assert_eq!(config.granularity, granularity);
        }
        assert!(Config::from_lookup(lookup(&[("NNO_GRANULARITY", "node")])).is_err());
    }

    #[test]
    fn budget_must_fit_the_reserved_entities() {
        assert!(Config::from_lookup(lookup(&[("NNO_SERIES_BUDGET", "32")])).is_ok());
        for raw in ["31", "0", "-1", "many"] {
            let result = Config::from_lookup(lookup(&[("NNO_SERIES_BUDGET", raw)]));
            assert!(result.is_err(), "{raw}");
        }
    }

    #[test]
    fn invalid_bind_addr_fails() {
        assert!(Config::from_lookup(lookup(&[("NNO_BIND_ADDR", "not-an-address")])).is_err());
    }

    #[test]
    fn invalid_read_interval_fails() {
        for raw in ["5", "0s", "5m", "s", "-1s"] {
            let result = Config::from_lookup(lookup(&[("NNO_READ_INTERVAL", raw)]));
            assert!(result.is_err(), "{raw}");
        }
    }

    #[test]
    fn invalid_max_cgroups_fails() {
        for raw in ["0", "-1", "many"] {
            let result = Config::from_lookup(lookup(&[("NNO_MAX_CGROUPS", raw)]));
            assert!(result.is_err(), "{raw}");
        }
    }

    #[test]
    fn invalid_culprits_per_victim_fails() {
        for raw in ["0", "-1", "many"] {
            let result = Config::from_lookup(lookup(&[("NNO_CULPRITS_PER_VICTIM", raw)]));
            assert!(result.is_err(), "{raw}");
        }
    }

    #[test]
    fn invalid_max_pairs_fails() {
        for raw in ["0", "-1", "many", "4294967296"] {
            let result = Config::from_lookup(lookup(&[("NNO_MAX_PAIRS", raw)]));
            assert!(result.is_err(), "{raw}");
        }
    }

    #[test]
    fn invalid_bounds_fail() {
        let thirteen = "1us,2us,3us,4us,5us,6us,7us,8us,9us,10us,11us,12us,13us";
        for raw in ["", "1ms,1ms", "1ms,500us", "0us,1ms", "1ms,,2ms", thirteen] {
            let result = Config::from_lookup(lookup(&[("NNO_HISTOGRAM_BOUNDS", raw)]));
            assert!(result.is_err(), "{raw}");
        }
    }

    #[test]
    fn twelve_bounds_pass() {
        let raw = "1us,2us,3us,4us,5us,6us,7us,8us,9us,10us,11us,12us";
        let config = Config::from_lookup(lookup(&[("NNO_HISTOGRAM_BOUNDS", raw)])).unwrap();
        assert_eq!(config.histogram_bounds.len(), MAX_BOUNDS);
    }
}
