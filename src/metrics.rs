//! CPU and memory: Metrics API usage (what `kubectl top` reads) and the
//! requests and limits pods declare.

use std::collections::HashMap;

use kube::Client;
use serde_json::Value;

use crate::rows;

/// Usage of a pod or node.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
	/// Millicores.
	pub cpu: f64,
	/// Bytes.
	pub mem: f64,
}

/// One snapshot of a context's Metrics API.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
	/// Pods by `namespace/name`.
	pub pods:  HashMap<String, Usage>,
	/// Nodes by name.
	pub nodes: HashMap<String, Usage>,
}

/// Declared resources (summed over a pod's containers).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Resources {
	/// CPU request, millicores.
	pub cpu_req: Option<f64>,
	/// CPU limit, millicores.
	pub cpu_lim: Option<f64>,
	/// Memory request, bytes.
	pub mem_req: Option<f64>,
	/// Memory limit, bytes.
	pub mem_lim: Option<f64>,
}

/// Millicores of a CPU quantity: `250m`, `1`, `1.5`, `123456789n`, `500u`.
pub fn cpu_milli(q: &str) -> f64 {
	let q = q.trim();
	let (num, scale) = match q.as_bytes().last() {
		Some(b'n') => (&q[..q.len() - 1], 1e-6),
		Some(b'u') => (&q[..q.len() - 1], 1e-3),
		Some(b'm') => (&q[..q.len() - 1], 1.0),
		_ => (q, 1000.0),
	};
	num.parse::<f64>().unwrap_or(0.0) * scale
}

/// Bytes of a memory quantity: `128974848`, `129e6`, `123Mi`, `1G`.
pub fn mem_bytes(q: &str) -> f64 {
	let q = q.trim();
	if let Ok(x) = q.parse::<f64>() {
		return x;
	}
	rows::bytes(q) as f64
}

/// `120m`, `1.5` cores.
pub fn fmt_cpu(milli: f64) -> String {
	if milli < 1000.0 { format!("{milli:.0}m") } else { format!("{:.2}", milli / 1000.0).trim_end_matches('0').trim_end_matches('.').to_owned() }
}

/// `512Mi`, `1.5Gi`.
pub fn fmt_mem(bytes: f64) -> String {
	const UNITS: [&str; 5] = ["", "Ki", "Mi", "Gi", "Ti"];
	let mut v = bytes;
	let mut i = 0;
	while v >= 1024.0 && i < UNITS.len() - 1 {
		v /= 1024.0;
		i += 1;
	}
	if v >= 10.0 || i == 0 { format!("{v:.0}{}", UNITS[i]) } else { format!("{v:.1}{}", UNITS[i]) }
}

fn sum(containers: &[Value], kind: &str, res: &str, parse: fn(&str) -> f64) -> Option<f64> {
	let mut total = None;
	for c in containers {
		if let Some(q) = rows::at(c, &["resources", kind, res]).and_then(Value::as_str) {
			*total.get_or_insert(0.0) += parse(q);
		}
	}
	total
}

/// The requests and limits of a pod (`obj.data` of a Pod).
pub fn pod_resources(pod: &Value) -> Resources {
	let cs = rows::at(pod, &["spec", "containers"]).and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
	Resources {
		cpu_req: sum(cs, "requests", "cpu", cpu_milli),
		cpu_lim: sum(cs, "limits", "cpu", cpu_milli),
		mem_req: sum(cs, "requests", "memory", mem_bytes),
		mem_lim: sum(cs, "limits", "memory", mem_bytes),
	}
}

/// A node's allocatable CPU and memory as limits (`obj.data` of a Node).
pub fn node_resources(node: &Value) -> Resources {
	let q = |k: &str| rows::at(node, &["status", "allocatable", k]).and_then(Value::as_str);
	Resources {
		cpu_lim: q("cpu").map(cpu_milli),
		mem_lim: q("memory").map(mem_bytes),
		..Resources::default()
	}
}

fn usage(v: &Value) -> Usage {
	let q = |k: &str| rows::at(v, &["usage", k]).and_then(Value::as_str).unwrap_or("0");
	Usage { cpu: cpu_milli(q("cpu")), mem: mem_bytes(q("memory")) }
}

/// Parses a `PodMetricsList` and a `NodeMetricsList`.
pub fn parse(pods: &Value, nodes: &Value) -> Snapshot {
	let items = |v: &Value| v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
	let mut snap = Snapshot::default();
	for p in items(pods) {
		let ns = rows::at(&p, &["metadata", "namespace"]).and_then(Value::as_str).unwrap_or_default();
		let name = rows::at(&p, &["metadata", "name"]).and_then(Value::as_str).unwrap_or_default();
		let mut u = Usage::default();
		for c in p.get("containers").and_then(Value::as_array).into_iter().flatten() {
			let cu = usage(c);
			u.cpu += cu.cpu;
			u.mem += cu.mem;
		}
		snap.pods.insert(format!("{ns}/{name}"), u);
	}
	for n in items(nodes) {
		let name = rows::at(&n, &["metadata", "name"]).and_then(Value::as_str).unwrap_or_default();
		snap.nodes.insert(name.to_owned(), usage(&n));
	}
	snap
}

async fn get(client: &Client, path: &str) -> Result<Value, kube::Error> {
	let req = http::Request::get(path).body(Vec::new()).map_err(kube::Error::HttpError)?;
	client.request::<Value>(req).await
}

/// Fetches pod and node metrics. `Err` names why (no Metrics API, forbidden).
pub async fn fetch(client: &Client) -> Result<Snapshot, String> {
	let base = "/apis/metrics.k8s.io/v1beta1";
	let (pods_path, nodes_path) = (format!("{base}/pods"), format!("{base}/nodes"));
	let (pods, nodes) = tokio::join!(get(client, &pods_path), get(client, &nodes_path));
	let describe = |e: kube::Error| match e {
		kube::Error::Api(s) if s.code == 404 => "metrics API unavailable".to_owned(),
		kube::Error::Api(s) => s.message.clone(),
		other => other.to_string(),
	};
	let pods = pods.map_err(describe)?;
	// Node metrics may be forbidden while pod metrics are not.
	let nodes = nodes.unwrap_or(Value::Null);
	Ok(parse(&pods, &nodes))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn quantities_parse_every_suffix() {
		assert_eq!(cpu_milli("250m"), 250.0);
		assert_eq!(cpu_milli("1.5"), 1500.0);
		assert_eq!(cpu_milli("123456789n"), 123.456789);
		assert_eq!(cpu_milli("500u"), 0.5);
		assert_eq!(mem_bytes("1Ki"), 1024.0);
		assert_eq!(mem_bytes("129e6"), 129e6);
		assert_eq!(mem_bytes("1G"), 1e9);
		assert_eq!(fmt_cpu(120.4), "120m");
		assert_eq!(fmt_cpu(1500.0), "1.5");
		assert_eq!(fmt_cpu(2000.0), "2");
		assert_eq!(fmt_mem(512.0 * 1024.0 * 1024.0), "512Mi");
		assert_eq!(fmt_mem(1.5 * 1024.0 * 1024.0 * 1024.0), "1.5Gi");
	}

	#[test]
	fn pod_resources_sum_containers_and_metrics_sum_containers() {
		let pod = serde_json::json!({"spec": {"containers": [
			{"resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"cpu": "1"}}},
			{"resources": {"requests": {"cpu": "50m"}}}
		]}});
		let r = pod_resources(&pod);
		assert_eq!(r.cpu_req, Some(150.0));
		assert_eq!(r.cpu_lim, Some(1000.0));
		assert_eq!(r.mem_req, Some(64.0 * 1024.0 * 1024.0));
		assert_eq!(r.mem_lim, None);
		let pods = serde_json::json!({"items": [{"metadata": {"namespace": "a", "name": "p"},
			"containers": [{"usage": {"cpu": "10m", "memory": "1Mi"}}, {"usage": {"cpu": "5000000n", "memory": "1Mi"}}]}]});
		let snap = parse(&pods, &Value::Null);
		assert_eq!(snap.pods["a/p"], Usage { cpu: 15.0, mem: 2.0 * 1024.0 * 1024.0 });
	}
}
