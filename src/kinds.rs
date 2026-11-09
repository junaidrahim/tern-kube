//! The resource kinds the sidebar offers: built-ins with hand-made columns,
//! plus custom resources found by discovery (Name, Namespace, Age).

use kube::{
	api::{ApiResource, GroupVersionKind},
	discovery::Scope,
};

/// A sidebar group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
	/// Nodes, namespaces, events.
	Cluster,
	/// Pods and their controllers.
	Workloads,
	/// Services, ingresses, network policies.
	Network,
	/// ConfigMaps, secrets.
	Config,
	/// Volumes and claims.
	Storage,
	/// Service accounts, roles and bindings.
	Access,
	/// Custom resources.
	Custom,
}

impl Group {
	/// Every group, in sidebar order.
	pub const ALL: [Self; 7] =
		[Self::Workloads, Self::Network, Self::Config, Self::Storage, Self::Cluster, Self::Access, Self::Custom];

	/// The heading.
	pub const fn label(self) -> &'static str {
		match self {
			Self::Cluster => "Cluster",
			Self::Workloads => "Workloads",
			Self::Network => "Network",
			Self::Config => "Config",
			Self::Storage => "Storage",
			Self::Access => "Access Control",
			Self::Custom => "Custom Resources",
		}
	}
}

/// How a kind's objects become rows (and which columns it shows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projector {
	/// `v1/Pod`.
	Pod,
	/// `apps/v1/Deployment`.
	Deployment,
	/// `apps/v1/StatefulSet`.
	StatefulSet,
	/// `apps/v1/DaemonSet`.
	DaemonSet,
	/// `apps/v1/ReplicaSet`.
	ReplicaSet,
	/// `batch/v1/Job`.
	Job,
	/// `batch/v1/CronJob`.
	CronJob,
	/// `v1/Service`.
	Service,
	/// `networking.k8s.io/v1/Ingress`.
	Ingress,
	/// `v1/ConfigMap`.
	ConfigMap,
	/// `v1/Secret`.
	Secret,
	/// `v1/PersistentVolumeClaim`.
	Pvc,
	/// `v1/PersistentVolume`.
	Pv,
	/// `v1/Node`.
	Node,
	/// `v1/Namespace`.
	Namespace,
	/// `v1/Event`.
	Event,
	/// `autoscaling/v2/HorizontalPodAutoscaler`.
	Hpa,
	/// Anything else: Name, Namespace, Age.
	Generic,
}

/// A column of a kind's table.
#[derive(Clone, Copy, Debug)]
pub struct ColSpec {
	/// Id (the cell key).
	pub id:       &'static str,
	/// Header.
	pub head:     &'static str,
	/// Numbers: end-aligned, mono.
	pub numeric:  bool,
	/// Takes the spare width (the name column).
	pub grow:     bool,
	/// Lower hides first when narrow.
	pub priority: f64,
}

const fn col(id: &'static str, head: &'static str, priority: f64) -> ColSpec {
	ColSpec { id, head, numeric: false, grow: false, priority }
}

const fn num(id: &'static str, head: &'static str, priority: f64) -> ColSpec {
	ColSpec { id, head, numeric: true, grow: false, priority }
}

const NAME: ColSpec = ColSpec { id: "name", head: "Name", numeric: false, grow: true, priority: 100.0 };
const NS: ColSpec = col("namespace", "Namespace", 40.0);
const AGE: ColSpec = num("age", "Age", 60.0);
/// The context column of merged tables.
pub const CONTEXT: ColSpec = col("context", "Context", 85.0);
/// Metrics columns of pods and nodes.
pub const CPU: ColSpec = num("cpu", "CPU", 36.0);
/// Metrics columns of pods and nodes.
pub const MEMORY: ColSpec = num("memory", "Memory", 35.0);

impl Projector {
	/// The columns, without Namespace (added for namespaced kinds).
	pub const fn columns(self) -> &'static [ColSpec] {
		/// A `'static` column table (const fn calls aren't promoted).
		macro_rules! cols {
			($($c:expr),* $(,)?) => {
				const { &[$($c),*] }
			};
		}
		match self {
			Self::Pod => cols![
				NAME,
				num("ready", "Ready", 90.0),
				col("status", "Status", 95.0),
				num("restarts", "Restarts", 80.0),
				col("node", "Node", 20.0),
				col("ip", "IP", 10.0),
				AGE,
			],
			Self::Deployment => cols![
				NAME,
				num("ready", "Ready", 90.0),
				num("updated", "Up-to-date", 50.0),
				num("available", "Available", 70.0),
				AGE,
			],
			Self::StatefulSet => cols![NAME, num("ready", "Ready", 90.0), AGE],
			Self::DaemonSet => cols![
				NAME,
				num("desired", "Desired", 70.0),
				num("current", "Current", 50.0),
				num("ready", "Ready", 90.0),
				num("updated", "Up-to-date", 45.0),
				num("available", "Available", 65.0),
				AGE,
			],
			Self::ReplicaSet => cols![
				NAME,
				num("desired", "Desired", 70.0),
				num("current", "Current", 50.0),
				num("ready", "Ready", 90.0),
				AGE,
			],
			Self::Job => cols![
				NAME,
				col("status", "Status", 95.0),
				num("completions", "Completions", 80.0),
				num("duration", "Duration", 50.0),
				AGE,
			],
			Self::CronJob => cols![
				NAME,
				col("schedule", "Schedule", 90.0),
				col("suspend", "Suspend", 50.0),
				num("active", "Active", 70.0),
				num("last", "Last Schedule", 60.0),
				AGE,
			],
			Self::Service => cols![
				NAME,
				col("type", "Type", 90.0),
				col("cluster_ip", "Cluster IP", 70.0),
				col("external_ip", "External IP", 50.0),
				col("ports", "Ports", 80.0),
				AGE,
			],
			Self::Ingress => cols![
				NAME,
				col("class", "Class", 50.0),
				col("hosts", "Hosts", 90.0),
				col("address", "Address", 60.0),
				AGE,
			],
			Self::ConfigMap => cols![NAME, num("data", "Data", 80.0), AGE],
			Self::Secret => cols![NAME, col("type", "Type", 80.0), num("data", "Data", 70.0), AGE],
			Self::Pvc => cols![
				NAME,
				col("status", "Status", 95.0),
				col("volume", "Volume", 30.0),
				num("capacity", "Capacity", 80.0),
				col("access", "Access Modes", 40.0),
				col("class", "Storage Class", 50.0),
				AGE,
			],
			Self::Pv => cols![
				NAME,
				num("capacity", "Capacity", 80.0),
				col("access", "Access Modes", 40.0),
				col("reclaim", "Reclaim", 30.0),
				col("status", "Status", 95.0),
				col("claim", "Claim", 70.0),
				col("class", "Storage Class", 50.0),
				AGE,
			],
			Self::Node => cols![
				NAME,
				col("status", "Status", 95.0),
				col("roles", "Roles", 70.0),
				col("version", "Version", 50.0),
				col("ip", "Internal IP", 40.0),
				AGE,
			],
			Self::Namespace => cols![NAME, col("status", "Status", 90.0), AGE],
			Self::Event => cols![
				col("type", "Type", 95.0),
				col("reason", "Reason", 90.0),
				col("object", "Object", 85.0),
				ColSpec { id: "message", head: "Message", numeric: false, grow: true, priority: 100.0 },
				num("count", "Count", 30.0),
				num("last", "Last Seen", 80.0),
			],
			Self::Hpa => cols![
				NAME,
				col("target", "Reference", 80.0),
				num("min", "Min", 50.0),
				num("max", "Max", 60.0),
				num("replicas", "Replicas", 90.0),
				AGE,
			],
			Self::Generic => cols![NAME, AGE],
		}
	}
}

/// One kind the browser can list.
#[derive(Clone, Debug)]
pub struct Kind {
	/// Stable id: `plural` for the core group, else `plural.group`.
	pub id:         String,
	/// Sidebar label.
	pub label:      String,
	/// Sidebar group.
	pub group:      Group,
	/// The API coordinates.
	pub ar:         ApiResource,
	/// Namespaced or cluster-wide.
	pub namespaced: bool,
	/// Its row projector.
	pub projector:  Projector,
}

impl Kind {
	/// The id of the kind of `ar`.
	pub fn id_of(ar: &ApiResource) -> String {
		if ar.group.is_empty() { ar.plural.clone() } else { format!("{}.{}", ar.plural, ar.group) }
	}

	/// The columns shown: Namespace goes after Name for namespaced kinds.
	pub fn columns(&self) -> Vec<ColSpec> {
		let base = self.projector.columns();
		let mut out = Vec::with_capacity(base.len() + 1);
		for (i, c) in base.iter().enumerate() {
			out.push(*c);
			if i == 0 && self.namespaced {
				out.push(NS);
			}
		}
		out
	}

	/// Whether the object kind has pods with logs and a shell.
	pub fn is_pod(&self) -> bool {
		self.projector == Projector::Pod
	}

	/// Has a scale subresource the UI offers.
	pub fn scalable(&self) -> bool {
		matches!(self.projector, Projector::Deployment | Projector::StatefulSet | Projector::ReplicaSet)
	}

	/// Supports `rollout restart`.
	pub fn restartable(&self) -> bool {
		matches!(self.projector, Projector::Deployment | Projector::StatefulSet | Projector::DaemonSet)
	}

	/// Owns or selects pods (child pods, aggregated logs).
	pub fn selects_pods(&self) -> bool {
		matches!(
			self.projector,
			Projector::Deployment
				| Projector::StatefulSet
				| Projector::DaemonSet
				| Projector::ReplicaSet
				| Projector::Job
				| Projector::Service
		)
	}

	/// Has a Logs tab (its own pod, or the pods it selects).
	pub fn has_logs(&self) -> bool {
		self.is_pod() || self.selects_pods()
	}

	/// Shows CPU and memory.
	pub fn has_metrics(&self) -> bool {
		matches!(self.projector, Projector::Pod | Projector::Node)
	}
}

#[expect(clippy::too_many_arguments, reason = "one row of the kind table per call")]
fn builtin(
	group: Group,
	label: &str,
	g: &str,
	v: &str,
	kind: &str,
	plural: &str,
	namespaced: bool,
	projector: Projector,
) -> Kind {
	let ar = ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(g, v, kind), plural);
	Kind { id: Kind::id_of(&ar), label: label.to_owned(), group, ar, namespaced, projector }
}

/// The built-in kinds, in sidebar order within their groups.
pub fn builtins() -> Vec<Kind> {
	use Group::*;
	use Projector as P;
	vec![
		builtin(Workloads, "Pods", "", "v1", "Pod", "pods", true, P::Pod),
		builtin(Workloads, "Deployments", "apps", "v1", "Deployment", "deployments", true, P::Deployment),
		builtin(Workloads, "StatefulSets", "apps", "v1", "StatefulSet", "statefulsets", true, P::StatefulSet),
		builtin(Workloads, "DaemonSets", "apps", "v1", "DaemonSet", "daemonsets", true, P::DaemonSet),
		builtin(Workloads, "ReplicaSets", "apps", "v1", "ReplicaSet", "replicasets", true, P::ReplicaSet),
		builtin(Workloads, "Jobs", "batch", "v1", "Job", "jobs", true, P::Job),
		builtin(Workloads, "CronJobs", "batch", "v1", "CronJob", "cronjobs", true, P::CronJob),
		builtin(
			Workloads,
			"HPAs",
			"autoscaling",
			"v2",
			"HorizontalPodAutoscaler",
			"horizontalpodautoscalers",
			true,
			P::Hpa,
		),
		builtin(Network, "Services", "", "v1", "Service", "services", true, P::Service),
		builtin(Network, "Ingresses", "networking.k8s.io", "v1", "Ingress", "ingresses", true, P::Ingress),
		builtin(Network, "Endpoint Slices", "discovery.k8s.io", "v1", "EndpointSlice", "endpointslices", true, P::Generic),
		builtin(
			Network,
			"Network Policies",
			"networking.k8s.io",
			"v1",
			"NetworkPolicy",
			"networkpolicies",
			true,
			P::Generic,
		),
		builtin(Config, "ConfigMaps", "", "v1", "ConfigMap", "configmaps", true, P::ConfigMap),
		builtin(Config, "Secrets", "", "v1", "Secret", "secrets", true, P::Secret),
		builtin(Storage, "PVCs", "", "v1", "PersistentVolumeClaim", "persistentvolumeclaims", true, P::Pvc),
		builtin(Storage, "PVs", "", "v1", "PersistentVolume", "persistentvolumes", false, P::Pv),
		builtin(Storage, "Storage Classes", "storage.k8s.io", "v1", "StorageClass", "storageclasses", false, P::Generic),
		builtin(Cluster, "Nodes", "", "v1", "Node", "nodes", false, P::Node),
		builtin(Cluster, "Namespaces", "", "v1", "Namespace", "namespaces", false, P::Namespace),
		builtin(Cluster, "Events", "", "v1", "Event", "events", true, P::Event),
		builtin(Access, "Service Accounts", "", "v1", "ServiceAccount", "serviceaccounts", true, P::Generic),
		builtin(Access, "Roles", "rbac.authorization.k8s.io", "v1", "Role", "roles", true, P::Generic),
		builtin(Access, "Role Bindings", "rbac.authorization.k8s.io", "v1", "RoleBinding", "rolebindings", true, P::Generic),
		builtin(Access, "Cluster Roles", "rbac.authorization.k8s.io", "v1", "ClusterRole", "clusterroles", false, P::Generic),
		builtin(
			Access,
			"Cluster Role Bindings",
			"rbac.authorization.k8s.io",
			"v1",
			"ClusterRoleBinding",
			"clusterrolebindings",
			false,
			P::Generic,
		),
	]
}

/// A discovered custom resource kind.
pub fn custom(ar: ApiResource, scope: &Scope) -> Kind {
	Kind {
		id:         Kind::id_of(&ar),
		label:      ar.kind.clone(),
		group:      Group::Custom,
		namespaced: matches!(scope, Scope::Namespaced),
		projector:  Projector::Generic,
		ar,
	}
}

/// API groups served by Kubernetes itself; everything else discovery finds is
/// listed under Custom Resources (Gateway API's `gateway.networking.k8s.io`
/// included).
pub fn is_builtin_group(group: &str) -> bool {
	matches!(
		group,
		"" | "apps"
			| "batch"
			| "autoscaling"
			| "policy"
			| "networking.k8s.io"
			| "discovery.k8s.io"
			| "storage.k8s.io"
			| "rbac.authorization.k8s.io"
			| "apiextensions.k8s.io"
			| "admissionregistration.k8s.io"
			| "certificates.k8s.io"
			| "coordination.k8s.io"
			| "scheduling.k8s.io"
			| "node.k8s.io"
			| "flowcontrol.apiserver.k8s.io"
			| "events.k8s.io"
			| "authentication.k8s.io"
			| "authorization.k8s.io"
			| "apiregistration.k8s.io"
			| "metrics.k8s.io"
			| "resource.k8s.io"
			| "storagemigration.k8s.io"
			| "internal.apiserver.k8s.io"
	)
}
