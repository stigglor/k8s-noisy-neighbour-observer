//! The pods on this node, from a watch, joined to cgroups by pod UID and container id.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use futures_util::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{watcher, WatchStreamExt};
use kube::Api;

use crate::cgroup::Cgroup;
use crate::config::Granularity;
use crate::runq::Owner;
use crate::workload::{self, Entity, Workload};

/// The kubelet names the cgroup of a static pod after this hash, not after the mirror pod UID.
const MIRROR_ANNOTATION: &str = "kubernetes.io/config.mirror";

/// The `container` label of a pod cgroup that no container owns, such as CRI-O `conmon`.
const POD_CONTAINER: &str = "_pod";

/// The longest wait for the first pod list. Without it, every pod cgroup stays unresolved.
const FIRST_LIST_TIMEOUT: Duration = Duration::from_secs(60);

/// The pods of the node, and a generation that each watch event raises.
#[derive(Clone)]
pub struct Watch {
    pub store: Store<Pod>,
    generation: Arc<AtomicU64>,
}

impl Watch {
    /// Changes after each watch event, so a reader rebuilds its index only after a change.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// Starts a watch on the pods of `node` and waits for the first list. A missing RBAC rule or a
/// wrong node name fails here, not later as silent sample loss. The watch then retries with a
/// backoff after an error.
pub async fn watch(node: &str) -> anyhow::Result<Watch> {
    let client = kube::Client::try_default()
        .await
        .context("creating the Kubernetes client")?;
    let config = watcher::Config::default().fields(&format!("spec.nodeName={node}"));
    let (store, writer) = reflector::store();
    let generation = Arc::new(AtomicU64::new(0));
    let events = watcher(Api::<Pod>::all(client), config)
        .default_backoff()
        .reflect(writer);
    let counter = generation.clone();
    tokio::spawn(events.for_each(move |event| {
        match event {
            Ok(_) => {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => tracing::warn!(error = format!("{error:#}"), "the pod watch failed"),
        }
        std::future::ready(())
    }));
    tokio::time::timeout(FIRST_LIST_TIMEOUT, store.wait_until_ready())
        .await
        .with_context(|| {
            format!(
                "no pod list after {}s; check that the agent can list and watch pods",
                FIRST_LIST_TIMEOUT.as_secs()
            )
        })?
        .context("the pod watch stopped before its first list")?;
    // A node always runs at least the agent pod, so an empty list means a wrong node name.
    if store.is_empty() {
        bail!("no pods on node {node}; check NNO_NODE_NAME");
    }
    Ok(Watch { store, generation })
}

/// The pods of one moment, by the UID their cgroups carry.
pub struct PodIndex {
    granularity: Granularity,
    pods: HashMap<String, IndexedPod>,
}

struct IndexedPod {
    namespace: String,
    name: String,
    workload: Workload,
    /// Container id to container name.
    containers: HashMap<String, String>,
    /// A pod that completed keeps no cgroups, so it does not keep its entity alive.
    running: bool,
}

impl PodIndex {
    pub fn new<'a>(granularity: Granularity, pods: impl IntoIterator<Item = &'a Pod>) -> Self {
        let mut index = HashMap::new();
        for pod in pods {
            let meta = &pod.metadata;
            let status = pod.status.as_ref();
            let statuses = status.into_iter().flat_map(|status| {
                [
                    &status.container_statuses,
                    &status.init_container_statuses,
                    &status.ephemeral_container_statuses,
                ]
                .into_iter()
                .flatten()
                .flatten()
            });
            let containers = statuses
                .filter_map(|c| {
                    let id = c.container_id.as_deref()?;
                    let id = id.split_once("://").map_or(id, |(_, id)| id);
                    Some((id.to_string(), c.name.clone()))
                })
                .collect();
            let phase = status.and_then(|status| status.phase.as_deref());
            let indexed = IndexedPod {
                namespace: meta.namespace.clone().unwrap_or_default(),
                name: meta.name.clone().unwrap_or_default(),
                workload: workload::derive(pod),
                containers,
                running: !matches!(phase, Some("Succeeded" | "Failed")),
            };
            let mirror = meta
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(MIRROR_ANNOTATION));
            let uid = mirror.or(meta.uid.as_ref());
            if let Some(uid) = uid {
                index.insert(uid.to_ascii_lowercase(), indexed);
            }
        }
        Self {
            granularity,
            pods: index,
        }
    }

    /// The owner of a cgroup, or `Unresolved` while the watch does not know its pod or container.
    pub fn owner(&self, cgroup: Option<&Cgroup>) -> Owner {
        let (uid, container_id) = match cgroup {
            None => return Owner::Unresolved(None),
            Some(Cgroup::System) => return Owner::System,
            Some(Cgroup::Pod { uid, container }) => (uid, container.as_deref()),
        };
        let Some(pod) = self.pods.get(uid) else {
            return Owner::Unresolved(None);
        };
        let container = match container_id {
            Some(id) => match pod.containers.get(id) {
                Some(name) => name.as_str(),
                // A new container waits for its status. The pod sandbox never gets one, so its
                // samples go to `_pod` once the wait ends.
                None if self.granularity == Granularity::Container => {
                    return Owner::Unresolved(Some(pod.entity(self.granularity, POD_CONTAINER)));
                }
                None => "",
            },
            None => POD_CONTAINER,
        };
        Owner::Entity(pod.entity(self.granularity, container))
    }

    /// The entities of the pods that run on the node. At container granularity, these are the
    /// containers with a status and the `_pod` entity of each pod.
    pub fn live(&self) -> HashSet<Entity> {
        let running = self.pods.values().filter(|pod| pod.running);
        match self.granularity {
            Granularity::Container => running
                .flat_map(|pod| {
                    let names = pod.containers.values().map(String::as_str);
                    let names = names.chain([POD_CONTAINER]);
                    names.map(|name| pod.entity(Granularity::Container, name))
                })
                .collect(),
            granularity => running.map(|pod| pod.entity(granularity, "")).collect(),
        }
    }
}

impl IndexedPod {
    fn entity(&self, granularity: Granularity, container: &str) -> Entity {
        Entity::new(
            granularity,
            &self.namespace,
            &self.workload,
            &self.name,
            container,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UID: &str = "463132a4-d89a-4967-8346-29ae3d53d96e";
    const ID: &str = "8c7cb929c00c65e270726bcccd699e6a31e3d7d15466ad078bfee8bbdb40ed9b";

    fn web_pod(phase: &str) -> Pod {
        serde_json::from_value(json!({
            "metadata": {
                "name": "web-5d4f-x2x9q", "namespace": "shop", "uid": UID,
                "labels": { "pod-template-hash": "5d4f" },
                "ownerReferences": [{
                    "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-5d4f",
                    "uid": "rs", "controller": true
                }]
            },
            "status": {
                "phase": phase,
                "containerStatuses": [{
                    "name": "app", "containerID": format!("containerd://{ID}"),
                    "image": "", "imageID": "", "ready": true, "restartCount": 0
                }]
            }
        }))
        .unwrap()
    }

    fn in_pod(uid: &str, container: Option<&str>) -> Cgroup {
        Cgroup::Pod {
            uid: uid.to_string(),
            container: container.map(str::to_string),
        }
    }

    fn entity(granularity: Granularity, container: &str) -> Entity {
        let web = Workload {
            kind: "Deployment".into(),
            name: "web".into(),
        };
        Entity::new(granularity, "shop", &web, "web-5d4f-x2x9q", container)
    }

    #[test]
    fn workload_granularity_joins_by_pod_uid() {
        let pod = web_pod("Running");
        let index = PodIndex::new(Granularity::Workload, [&pod]);
        let expected = Owner::Entity(entity(Granularity::Workload, ""));
        assert_eq!(index.owner(Some(&in_pod(UID, Some(ID)))), expected);
        assert_eq!(index.owner(Some(&in_pod(UID, Some("unknown")))), expected);
        assert_eq!(index.owner(Some(&in_pod(UID, None))), expected);
        assert_eq!(
            index.owner(Some(&in_pod("other", None))),
            Owner::Unresolved(None)
        );
        assert_eq!(index.owner(Some(&Cgroup::System)), Owner::System);
        assert_eq!(index.owner(None), Owner::Unresolved(None));
        assert_eq!(
            index.live(),
            HashSet::from([entity(Granularity::Workload, "")])
        );
    }

    #[test]
    fn container_granularity_needs_the_container_status_or_falls_back_to_pod() {
        let pod = web_pod("Running");
        let index = PodIndex::new(Granularity::Container, [&pod]);
        assert_eq!(
            index.owner(Some(&in_pod(UID, Some(ID)))),
            Owner::Entity(entity(Granularity::Container, "app"))
        );
        assert_eq!(
            index.owner(Some(&in_pod(UID, Some("unknown")))),
            Owner::Unresolved(Some(entity(Granularity::Container, "_pod")))
        );
        assert_eq!(
            index.owner(Some(&in_pod(UID, None))),
            Owner::Entity(entity(Granularity::Container, "_pod"))
        );
        assert_eq!(
            index.live(),
            HashSet::from([
                entity(Granularity::Container, "app"),
                entity(Granularity::Container, "_pod")
            ])
        );
    }

    #[test]
    fn completed_pod_resolves_but_is_not_live() {
        let pod = web_pod("Succeeded");
        let index = PodIndex::new(Granularity::Pod, [&pod]);
        assert_eq!(
            index.owner(Some(&in_pod(UID, Some(ID)))),
            Owner::Entity(entity(Granularity::Pod, ""))
        );
        assert!(index.live().is_empty());
    }

    #[test]
    fn mirror_pod_joins_by_the_static_pod_hash() {
        let pod: Pod = serde_json::from_value(json!({
            "metadata": {
                "name": "kube-apiserver-kind-control-plane", "namespace": "kube-system",
                "uid": "11111111-2222-3333-4444-555555555555",
                "annotations": { MIRROR_ANNOTATION: "047f8b71cf514e178adf1f4d2892e1cc" },
                "ownerReferences": [{
                    "apiVersion": "v1", "kind": "Node", "name": "kind-control-plane",
                    "uid": "node", "controller": true
                }]
            }
        }))
        .unwrap();
        let index = PodIndex::new(Granularity::Workload, [&pod]);
        let Owner::Entity(entity) =
            index.owner(Some(&in_pod("047f8b71cf514e178adf1f4d2892e1cc", Some(ID))))
        else {
            panic!("the static pod did not resolve");
        };
        assert_eq!(entity.workload_kind, "StaticPod");
        assert_eq!(entity.workload, "kube-apiserver");
    }
}
