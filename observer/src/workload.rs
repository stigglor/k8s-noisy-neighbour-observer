//! The workload of a pod, from its controller owner reference, and the entity that labels its
//! series. Synthetic label values start with `_`, which no Kubernetes name can.

use k8s_openapi::api::core::v1::Pod;

use crate::config::Granularity;

/// The controller of a pod, as it appears in the `workload_kind` and `workload` labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    pub kind: String,
    pub name: String,
}

/// Derives the workload without extra API watches, from the owner name and the pod labels.
pub fn derive(pod: &Pod) -> Workload {
    let meta = &pod.metadata;
    let pod_name = meta.name.clone().unwrap_or_default();
    let owner = meta
        .owner_references
        .iter()
        .flatten()
        .find(|owner| owner.controller == Some(true));
    let Some(owner) = owner else {
        return workload("Pod", &pod_name);
    };
    let label = |key: &str| meta.labels.as_ref().and_then(|labels| labels.get(key));
    let name = owner.name.as_str();
    match owner.kind.as_str() {
        "ReplicaSet" => {
            for (key, kind) in [
                ("pod-template-hash", "Deployment"),
                ("rollouts-pod-template-hash", "Rollout"),
            ] {
                if let Some(base) = label(key).and_then(|hash| strip_suffix(name, hash)) {
                    return workload(kind, base);
                }
            }
            workload("ReplicaSet", name)
        }
        "Job" => match name.rsplit_once('-') {
            Some((base, time)) if time.len() >= 8 && time.bytes().all(|b| b.is_ascii_digit()) => {
                workload("CronJob", base)
            }
            _ => workload("Job", name),
        },
        "Node" => workload(
            "StaticPod",
            strip_suffix(&pod_name, name).unwrap_or(&pod_name),
        ),
        kind => workload(kind, name),
    }
}

/// `name` without a `-<suffix>` at its end, if it has one.
fn strip_suffix<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    name.strip_suffix(suffix)?
        .strip_suffix('-')
        .filter(|base| !base.is_empty())
}

fn workload(kind: &str, name: &str) -> Workload {
    Workload {
        kind: kind.to_string(),
        name: name.to_string(),
    }
}

/// The label values of one entity. `pod` and `container` are set only at the granularities that
/// export them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entity {
    pub namespace: String,
    pub workload_kind: String,
    pub workload: String,
    pub pod: Option<String>,
    pub container: Option<String>,
}

impl Entity {
    pub fn new(
        granularity: Granularity,
        namespace: &str,
        workload: &Workload,
        pod: &str,
        container: &str,
    ) -> Self {
        let pod_label = granularity != Granularity::Workload;
        Self {
            namespace: namespace.to_string(),
            workload_kind: workload.kind.clone(),
            workload: workload.name.clone(),
            pod: pod_label.then(|| pod.to_string()),
            container: (granularity == Granularity::Container).then(|| container.to_string()),
        }
    }

    /// Every cgroup outside a pod.
    pub fn system(granularity: Granularity) -> Self {
        Self::synthetic(granularity, "_system")
    }

    /// Every entity that did not fit in the series budget.
    pub fn overflow(granularity: Granularity) -> Self {
        Self::synthetic(granularity, "_overflow")
    }

    /// Every culprit without its own series.
    pub fn other() -> Self {
        Self::synthetic(Granularity::Workload, "_other")
    }

    /// The culprit of a wait that ends on an idle CPU. It never labels a series.
    pub fn idle() -> Self {
        Self::synthetic(Granularity::Workload, "_idle")
    }

    pub fn is_system(&self) -> bool {
        self.workload == "_system"
    }

    pub fn is_idle(&self) -> bool {
        self.workload == "_idle"
    }

    /// The same entity without its `pod` and `container` labels.
    pub fn to_workload(&self) -> Self {
        Self {
            pod: None,
            container: None,
            ..self.clone()
        }
    }

    /// Whether both entities belong to the same workload, whatever their pod or container.
    pub fn same_workload(&self, other: &Self) -> bool {
        self.namespace == other.namespace
            && self.workload_kind == other.workload_kind
            && self.workload == other.workload
    }

    fn synthetic(granularity: Granularity, value: &str) -> Self {
        Self::new(granularity, value, &workload(value, value), value, value)
    }

    pub fn labels(&self) -> Vec<(&'static str, &str)> {
        let mut labels = vec![
            ("namespace", self.namespace.as_str()),
            ("workload_kind", self.workload_kind.as_str()),
            ("workload", self.workload.as_str()),
        ];
        labels.extend(self.pod.as_deref().map(|pod| ("pod", pod)));
        labels.extend(self.container.as_deref().map(|c| ("container", c)));
        labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(name: &str, labels: serde_json::Value, owner: Option<(&str, &str)>) -> Pod {
        let owners: Vec<_> = owner
            .map(|(kind, name)| {
                json!({
                    "apiVersion": "apps/v1", "kind": kind, "name": name,
                    "uid": "u", "controller": true
                })
            })
            .into_iter()
            .collect();
        serde_json::from_value(json!({
            "metadata": { "name": name, "labels": labels, "ownerReferences": owners }
        }))
        .unwrap()
    }

    fn derived(name: &str, labels: serde_json::Value, owner: Option<(&str, &str)>) -> Workload {
        derive(&pod(name, labels, owner))
    }

    #[test]
    fn deployment_strips_the_template_hash() {
        let labels = json!({ "pod-template-hash": "5d4f8c7b9" });
        assert_eq!(
            derived(
                "web-5d4f8c7b9-x2x9q",
                labels,
                Some(("ReplicaSet", "web-5d4f8c7b9"))
            ),
            workload("Deployment", "web")
        );
    }

    #[test]
    fn rollout_strips_its_template_hash() {
        let labels = json!({ "rollouts-pod-template-hash": "6b7c" });
        assert_eq!(
            derived("api-6b7c-abcde", labels, Some(("ReplicaSet", "api-6b7c"))),
            workload("Rollout", "api")
        );
    }

    #[test]
    fn bare_replicaset_keeps_its_name() {
        assert_eq!(
            derived("rs-abcde", json!({}), Some(("ReplicaSet", "rs"))),
            workload("ReplicaSet", "rs")
        );
        let labels = json!({ "pod-template-hash": "other" });
        assert_eq!(
            derived("rs-abcde", labels, Some(("ReplicaSet", "rs-5d4f"))),
            workload("ReplicaSet", "rs-5d4f")
        );
    }

    #[test]
    fn statefulset_and_daemonset_are_direct() {
        assert_eq!(
            derived("db-0", json!({}), Some(("StatefulSet", "db"))),
            workload("StatefulSet", "db")
        );
        assert_eq!(
            derived("kindnet-abcde", json!({}), Some(("DaemonSet", "kindnet"))),
            workload("DaemonSet", "kindnet")
        );
    }

    #[test]
    fn cronjob_strips_the_schedule_time() {
        assert_eq!(
            derived(
                "backup-29012345-x",
                json!({}),
                Some(("Job", "backup-29012345"))
            ),
            workload("CronJob", "backup")
        );
    }

    #[test]
    fn job_with_a_short_number_stays_a_job() {
        assert_eq!(
            derived("migrate-2-x", json!({}), Some(("Job", "migrate-2"))),
            workload("Job", "migrate-2")
        );
        assert_eq!(
            derived("migrate-x", json!({}), Some(("Job", "migrate"))),
            workload("Job", "migrate")
        );
    }

    #[test]
    fn static_pod_strips_the_node_name() {
        assert_eq!(
            derived(
                "kube-apiserver-nno-pr3-control-plane",
                json!({}),
                Some(("Node", "nno-pr3-control-plane"))
            ),
            workload("StaticPod", "kube-apiserver")
        );
    }

    #[test]
    fn bare_pod_uses_its_name() {
        assert_eq!(derived("debug", json!({}), None), workload("Pod", "debug"));
    }

    #[test]
    fn other_owner_kind_is_direct() {
        assert_eq!(
            derived("runner-x", json!({}), Some(("Workflow", "runner"))),
            workload("Workflow", "runner")
        );
    }

    #[test]
    fn owner_that_is_not_the_controller_is_ignored() {
        let mut pod = pod("debug", json!({}), Some(("DaemonSet", "ds")));
        pod.metadata.owner_references.as_mut().unwrap()[0].controller = None;
        assert_eq!(derive(&pod), workload("Pod", "debug"));
    }

    #[test]
    fn entity_labels_follow_the_granularity() {
        let web = workload("Deployment", "web");
        let names = |g| {
            let entity = Entity::new(g, "shop", &web, "web-1", "app");
            entity
                .labels()
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(Granularity::Workload),
            ["namespace", "workload_kind", "workload"]
        );
        assert_eq!(
            names(Granularity::Pod),
            ["namespace", "workload_kind", "workload", "pod"]
        );
        assert_eq!(
            names(Granularity::Container),
            ["namespace", "workload_kind", "workload", "pod", "container"]
        );
        let overflow = Entity::overflow(Granularity::Container);
        assert!(overflow
            .labels()
            .iter()
            .all(|(_, value)| *value == "_overflow"));
    }
}
