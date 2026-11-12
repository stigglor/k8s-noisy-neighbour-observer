//! Maps cgroup ids to pods and containers. The kernel uses the inode of a cgroup v2 directory
//! as the cgroup id, so a walk of the tree gives each id its path. The parser finds a pod
//! component, then a container component below it, with no fixed prefix. So it reads the
//! systemd and cgroupfs drivers, and nested trees such as kind.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// What a cgroup belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cgroup {
    /// Outside every pod: the host, the runtime, the kubelet.
    System,
    /// In a pod. `container` is `None` for the pod cgroup itself and for CRI-O `conmon`.
    Pod {
        uid: String,
        container: Option<String>,
    },
}

static SYSTEM: Cgroup = Cgroup::System;

pub struct Resolver {
    root: PathBuf,
    ids: HashMap<u64, Cgroup>,
    /// Ids of the latest read that the walk after it did not find. Such a cgroup is gone or
    /// outside the mounted tree, so it can never resolve and maps to `System`.
    missing: HashSet<u64>,
}

impl Resolver {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ids: HashMap::new(),
            missing: HashSet::new(),
        }
    }

    pub fn get(&self, cgroup_id: u64) -> Option<&Cgroup> {
        match self.ids.get(&cgroup_id) {
            None if self.missing.contains(&cgroup_id) => Some(&SYSTEM),
            cgroup => cgroup,
        }
    }

    /// Rescans the tree when any of `cgroup_ids` is unknown. The reader calls it once per read,
    /// so the agent walks the tree at most once per read interval.
    pub fn refresh(&mut self, cgroup_ids: impl IntoIterator<Item = u64>) -> anyhow::Result<()> {
        let read: HashSet<u64> = cgroup_ids.into_iter().collect();
        self.missing.retain(|id| read.contains(id));
        if read.iter().all(|id| self.get(*id).is_some()) {
            return Ok(());
        }
        // Only a complete walk proves that an id is gone. After a failed read, the ids stay
        // unknown, so the next read scans again.
        if self.scan()? {
            let missing = read.into_iter().filter(|id| !self.ids.contains_key(id));
            self.missing.extend(missing.collect::<Vec<_>>());
        }
        Ok(())
    }

    /// Replaces the known ids with the current tree, so ids of removed cgroups go away. Returns
    /// whether the walk read every directory.
    pub fn scan(&mut self) -> anyhow::Result<bool> {
        let mut ids = HashMap::new();
        let metadata = std::fs::metadata(&self.root)
            .with_context(|| format!("reading the cgroup root {}", self.root.display()))?;
        ids.insert(metadata.ino(), Cgroup::System);
        let mut complete = true;
        walk(&self.root, &self.root, &mut ids, &mut complete);
        self.ids = ids;
        Ok(complete)
    }
}

/// A cgroup that goes away during the walk is no error. Any other failed read skips its subtree
/// and marks the walk as incomplete.
fn walk(root: &Path, dir: &Path, ids: &mut HashMap<u64, Cgroup>, complete: &mut bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => return failed(error, dir, complete),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failed(error, dir, complete);
                continue;
            }
        };
        // `file_type` comes from the directory entry, so files cost no extra syscall.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                failed(error, &path, complete);
                continue;
            }
        };
        let relative = path.strip_prefix(root).unwrap_or(&path);
        ids.insert(metadata.ino(), parse(relative));
        walk(root, &path, ids, complete);
    }
}

fn failed(error: std::io::Error, path: &Path, complete: &mut bool) {
    if error.kind() != std::io::ErrorKind::NotFound {
        tracing::warn!(%error, path = %path.display(), "reading the cgroup tree failed");
        *complete = false;
    }
}

/// Classifies a cgroup path. Only a component below the pod can name a container, because a
/// nested tree such as kind has a 64 hex docker id above the pod.
pub fn parse(path: &Path) -> Cgroup {
    let mut components = path.iter().filter_map(|c| c.to_str());
    let Some(uid) = components.by_ref().find_map(pod_uid) else {
        return Cgroup::System;
    };
    let container = components.find_map(container_id).flatten();
    Cgroup::Pod { uid, container }
}

/// `kubepods-burstable-pod<uid>.slice` with `_` for `-` (systemd) or `pod<uid>` (cgroupfs).
fn pod_uid(component: &str) -> Option<String> {
    let name = match component.strip_suffix(".slice") {
        Some(unit) => unit.rsplit('-').next()?,
        None => component,
    };
    let uid = name.strip_prefix("pod")?;
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    let valid = match uid.len() {
        32 => hex(uid),
        36 => uid.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-' || c == '_',
            _ => c.is_ascii_hexdigit(),
        }),
        _ => false,
    };
    valid.then(|| uid.replace('_', "-").to_ascii_lowercase())
}

/// `Some(Some(id))` for a container, `Some(None)` for CRI-O `conmon`, `None` for anything else.
/// The systemd driver adds `.scope`, and CRI-O keeps its prefix under the cgroupfs driver too.
fn container_id(component: &str) -> Option<Option<String>> {
    let unit = component.strip_suffix(".scope").unwrap_or(component);
    if unit.starts_with("crio-conmon-") {
        return Some(None);
    }
    let id = ["cri-containerd-", "crio-", "docker-"]
        .iter()
        .find_map(|prefix| unit.strip_prefix(prefix))
        .unwrap_or(unit);
    let valid = id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| Some(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND: &str = "/docker/849b5e85ee5fbabea3ae275184cbdd20d1d0f6707cfe786cb675e65361819027";
    const ID: &str = "8c7cb929c00c65e270726bcccd699e6a31e3d7d15466ad078bfee8bbdb40ed9b";
    const UID: &str = "463132a4-d89a-4967-8346-29ae3d53d96e";

    fn pod(uid: &str, container: Option<&str>) -> Cgroup {
        Cgroup::Pod {
            uid: uid.to_string(),
            container: container.map(str::to_string),
        }
    }

    fn parsed(path: &str) -> Cgroup {
        parse(Path::new(path))
    }

    #[test]
    fn kind_systemd_burstable() {
        let path = format!(
            "{KIND}/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-burstable.slice/\
             kubelet-kubepods-burstable-pod463132a4_d89a_4967_8346_29ae3d53d96e.slice/\
             cri-containerd-{ID}.scope"
        );
        assert_eq!(parsed(&path), pod(UID, Some(ID)));
    }

    #[test]
    fn kind_static_pod_uid_has_no_dashes() {
        let path = format!(
            "{KIND}/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-burstable.slice/\
             kubelet-kubepods-burstable-pod047f8b71cf514e178adf1f4d2892e1cc.slice/\
             cri-containerd-{ID}.scope"
        );
        assert_eq!(
            parsed(&path),
            pod("047f8b71cf514e178adf1f4d2892e1cc", Some(ID))
        );
    }

    #[test]
    fn kind_besteffort() {
        let path = format!(
            "{KIND}/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/\
             kubelet-kubepods-besteffort-pod007e225a_009e_4fb6_822e_a6483e9c2e86.slice/\
             cri-containerd-{ID}.scope"
        );
        assert_eq!(
            parsed(&path),
            pod("007e225a-009e-4fb6-822e-a6483e9c2e86", Some(ID))
        );
    }

    #[test]
    fn kind_guaranteed_has_no_qos_slice() {
        let path = format!(
            "{KIND}/kubelet.slice/kubelet-kubepods.slice/\
             kubelet-kubepods-pode742efbf_8ffa_4b0f_8a1f_e2bc900e79f2.slice/\
             cri-containerd-{ID}.scope"
        );
        assert_eq!(
            parsed(&path),
            pod("e742efbf-8ffa-4b0f-8a1f-e2bc900e79f2", Some(ID))
        );
    }

    #[test]
    fn systemd_on_a_host() {
        let path = format!(
            "/kubepods.slice/kubepods-burstable.slice/\
             kubepods-burstable-pod463132a4_d89a_4967_8346_29ae3d53d96e.slice/\
             cri-containerd-{ID}.scope"
        );
        assert_eq!(parsed(&path), pod(UID, Some(ID)));
    }

    #[test]
    fn cgroupfs_driver() {
        assert_eq!(
            parsed(&format!("/kubepods/burstable/pod{UID}/{ID}")),
            pod(UID, Some(ID))
        );
        assert_eq!(
            parsed(&format!("/kubepods/pod{UID}/{ID}")),
            pod(UID, Some(ID))
        );
    }

    #[test]
    fn crio_and_docker_scopes() {
        let slice = "/kubepods.slice/kubepods-pod463132a4_d89a_4967_8346_29ae3d53d96e.slice";
        assert_eq!(
            parsed(&format!("{slice}/crio-{ID}.scope")),
            pod(UID, Some(ID))
        );
        assert_eq!(
            parsed(&format!("{slice}/docker-{ID}.scope")),
            pod(UID, Some(ID))
        );
        assert_eq!(
            parsed(&format!("{slice}/crio-conmon-{ID}.scope")),
            pod(UID, None)
        );
    }

    #[test]
    fn crio_under_the_cgroupfs_driver() {
        assert_eq!(
            parsed(&format!("/kubepods/burstable/pod{UID}/crio-{ID}")),
            pod(UID, Some(ID))
        );
        assert_eq!(
            parsed(&format!("/kubepods/burstable/pod{UID}/crio-conmon-{ID}")),
            pod(UID, None)
        );
    }

    #[test]
    fn pod_cgroup_maps_to_its_pod() {
        let path = format!(
            "{KIND}/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-burstable.slice/\
             kubelet-kubepods-burstable-pod463132a4_d89a_4967_8346_29ae3d53d96e.slice"
        );
        assert_eq!(parsed(&path), pod(UID, None));
    }

    #[test]
    fn cgroup_below_a_container_maps_to_it() {
        let path = format!("/kubepods/pod{UID}/{ID}/init");
        assert_eq!(parsed(&path), pod(UID, Some(ID)));
    }

    #[test]
    fn everything_outside_a_pod_is_system() {
        for path in [
            "",
            "/init.scope",
            "/.lxc",
            KIND,
            &format!("{KIND}/kubelet.slice/kubelet.service"),
            &format!("{KIND}/system.slice/containerd.service"),
            &format!("{KIND}/kubelet.slice/kubelet-kubepods.slice"),
            &format!(
                "{KIND}/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-burstable.slice"
            ),
            "/kubepods/besteffort",
            "/system.slice/podman-pod1234.slice",
        ] {
            assert_eq!(parsed(path), Cgroup::System, "{path}");
        }
    }

    #[test]
    fn scan_maps_inodes_to_paths() {
        let root = std::env::temp_dir().join(format!("nno-cgroup-{}", std::process::id()));
        let container = root.join(format!("kubepods/pod{UID}/{ID}"));
        std::fs::create_dir_all(&container).unwrap();
        let mut resolver = Resolver::new(&root);
        let ino = |path: &Path| std::fs::metadata(path).unwrap().ino();
        resolver.refresh([ino(&container), u64::MAX]).unwrap();
        assert_eq!(resolver.get(ino(&container)), Some(&pod(UID, Some(ID))));
        assert_eq!(resolver.get(ino(&root)), Some(&Cgroup::System));
        assert_eq!(
            resolver.get(ino(&root.join("kubepods"))),
            Some(&Cgroup::System)
        );
        assert_eq!(
            resolver.get(u64::MAX),
            Some(&Cgroup::System),
            "not in the tree"
        );
        resolver.refresh([]).unwrap();
        assert_eq!(
            resolver.get(u64::MAX),
            None,
            "forgotten once it leaves the map"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
