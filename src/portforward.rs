//! Port forwards from 127.0.0.1 to a pod, or to a service's pods. A service
//! forward resolves a running, ready pod for every new connection, so it
//! follows the service when pods are replaced (as Aptakube's forwards do).

use k8s_openapi::api::core::v1::{Pod, Service};
use kube::{
	Client,
	api::{Api, ListParams},
};
use tokio::net::TcpListener;

/// What is forwarded to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
	/// A pod; the port is a container port.
	Pod(String),
	/// A service; the port is a service port.
	Service(String),
}

/// A forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
	/// Identifies it in reports.
	pub id:        u64,
	/// Context.
	pub context:   String,
	/// Namespace.
	pub namespace: String,
	/// The target.
	pub target:    Target,
	/// The target's port.
	pub port:      u16,
	/// The local port (0 picks a free one).
	pub local:     u16,
}

impl Spec {
	/// `svc/api:80 → localhost:8080`.
	pub fn label(&self, local: u16) -> String {
		let t = match &self.target {
			Target::Pod(n) => format!("pod/{n}"),
			Target::Service(n) => format!("svc/{n}"),
		};
		format!("{t}:{} → localhost:{local}", self.port)
	}
}

/// What a forward reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
	/// Accepting connections on this local port.
	Listening(u16),
	/// A connection is open to this pod (count of open connections).
	Active(String, u32),
	/// A connection failed; the forward keeps listening.
	Failed(String),
	/// The listener could not start; the forward is over.
	Dead(String),
}

fn describe(e: kube::Error) -> String {
	match e {
		kube::Error::Api(s) => s.message.clone(),
		other => other.to_string(),
	}
}

fn ready(p: &Pod) -> bool {
	let st = p.status.as_ref();
	st.and_then(|s| s.phase.as_deref()) == Some("Running")
		&& p.metadata.deletion_timestamp.is_none()
		&& st
			.and_then(|s| s.conditions.as_ref())
			.is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// The pod and its port behind `spec` right now.
async fn resolve(client: &Client, spec: &Spec) -> Result<(String, u16), String> {
	match &spec.target {
		Target::Pod(name) => Ok((name.clone(), spec.port)),
		Target::Service(name) => {
			let svcs: Api<Service> = Api::namespaced(client.clone(), &spec.namespace);
			let svc = svcs.get(name).await.map_err(describe)?;
			let svc_spec = svc.spec.unwrap_or_default();
			let selector = svc_spec.selector.unwrap_or_default();
			if selector.is_empty() {
				return Err(format!("service {name} has no selector"));
			}
			let port = svc_spec
				.ports
				.unwrap_or_default()
				.into_iter()
				.find(|p| p.port == i32::from(spec.port))
				.ok_or_else(|| format!("service {name} has no port {}", spec.port))?;
			let labels = selector.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
			let pods: Api<Pod> = Api::namespaced(client.clone(), &spec.namespace);
			let list = pods.list(&ListParams::default().labels(&labels)).await.map_err(describe)?;
			let pod = list.items.into_iter().find(ready).ok_or_else(|| format!("no ready pod behind service {name}"))?;
			let target = match port.target_port {
				None => spec.port,
				Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(n)) => {
					u16::try_from(n).map_err(|_| format!("bad target port {n}"))?
				},
				Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(named)) => pod
					.spec
					.as_ref()
					.into_iter()
					.flat_map(|s| s.containers.iter())
					.flat_map(|c| c.ports.iter().flatten())
					.find(|p| p.name.as_deref() == Some(named.as_str()))
					.and_then(|p| u16::try_from(p.container_port).ok())
					.ok_or_else(|| format!("no container port named {named}"))?,
			};
			Ok((pod.metadata.name.unwrap_or_default(), target))
		},
	}
}

/// Runs the forward until the task is aborted.
pub async fn run(client: Client, spec: Spec, report: impl Fn(Report) + Send + Sync + Clone + 'static) {
	let listener = match TcpListener::bind(("127.0.0.1", spec.local)).await {
		Ok(l) => l,
		Err(e) => {
			report(Report::Dead(format!("localhost:{}: {e}", spec.local)));
			return;
		},
	};
	let local = listener.local_addr().map_or(spec.local, |a| a.port());
	report(Report::Listening(local));
	let open = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
	loop {
		let Ok((mut tcp, _)) = listener.accept().await else { continue };
		let (client, spec, report, open) = (client.clone(), spec.clone(), report.clone(), open.clone());
		tokio::spawn(async move {
			let (pod, port) = match resolve(&client, &spec).await {
				Ok(x) => x,
				Err(e) => return report(Report::Failed(e)),
			};
			let pods: Api<Pod> = Api::namespaced(client, &spec.namespace);
			let mut pf = match pods.portforward(&pod, &[port]).await {
				Ok(pf) => pf,
				Err(e) => return report(Report::Failed(describe(e))),
			};
			let Some(mut upstream) = pf.take_stream(port) else {
				return report(Report::Failed("no stream for the port".into()));
			};
			let n = open.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
			report(Report::Active(pod.clone(), n));
			let result = tokio::io::copy_bidirectional(&mut tcp, &mut upstream).await;
			drop(upstream);
			let _ = pf.join().await;
			let n = open.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) - 1;
			match result {
				Ok(_) => report(Report::Active(pod, n)),
				Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => report(Report::Active(pod, n)),
				Err(e) => report(Report::Failed(e.to_string())),
			}
		});
	}
}
