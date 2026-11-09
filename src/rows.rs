//! Rows: each object projected once per change into the cells of its kind's
//! columns, plus what sorting and filtering need.

use std::cmp::Ordering;

use kube::api::DynamicObject;
use serde_json::Value;
use tern_sdk::ui::Tone;

use crate::{format, kinds::Projector};

/// A cell value.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
	/// Text, sorted naturally.
	Str(String),
	/// A number shown as text (`1/3`, `2.5Gi`), sorted by `n`.
	Num(i64, String),
	/// A status with a colored dot.
	Dot(String, Tone),
	/// Seconds since this Unix time, formatted when rendered.
	Age(i64),
	/// Nothing.
	Empty,
}

impl Val {
	fn rank(&self) -> u8 {
		match self {
			Self::Empty => 0,
			_ => 1,
		}
	}

	/// Sort order (empty cells first in ascending order).
	pub fn cmp(&self, other: &Self) -> Ordering {
		match (self, other) {
			(Self::Str(a), Self::Str(b)) | (Self::Dot(a, _), Self::Dot(b, _)) => format::natural_cmp(a, b),
			(Self::Num(a, _), Self::Num(b, _)) => a.cmp(b),
			// Ascending age: youngest first, so later timestamps first.
			(Self::Age(a), Self::Age(b)) => b.cmp(a),
			_ => self.rank().cmp(&other.rank()),
		}
	}

	/// The text, with `now` for ages.
	pub fn text(&self, now: i64) -> String {
		match self {
			Self::Str(s) | Self::Num(_, s) | Self::Dot(s, _) => s.clone(),
			Self::Age(t) => format::age(format::since(now, *t)),
			Self::Empty => String::new(),
		}
	}
}

/// One object as a table row.
#[derive(Clone, Debug)]
pub struct Row {
	/// Unique in its table: `namespace/name` or `name`.
	pub key:       String,
	/// Its namespace.
	pub namespace: Option<String>,
	/// One value per column id, in column order.
	pub cells:     Vec<(&'static str, Val)>,
	/// Lowercase fields joined by newlines, for the filter.
	pub haystack:  String,
}

impl Row {
	/// The value of column `id`.
	pub fn get(&self, id: &str) -> &Val {
		self.cells.iter().find(|(c, _)| *c == id).map_or(&Val::Empty, |(_, v)| v)
	}
}

/// The row key of an object: `namespace/name`, or `name` when cluster-wide.
pub fn key_of(obj: &DynamicObject) -> String {
	let name = obj.metadata.name.as_deref().unwrap_or_default();
	match obj.metadata.namespace.as_deref() {
		Some(ns) => format!("{ns}/{name}"),
		None => name.to_owned(),
	}
}

/// The value at `path` in `v`.
pub fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
	path.iter().try_fold(v, |v, k| v.get(*k))
}

fn s<'a>(v: &'a Value, path: &[&str]) -> &'a str {
	at(v, path).and_then(Value::as_str).unwrap_or_default()
}

fn i(v: &Value, path: &[&str]) -> i64 {
	at(v, path).and_then(Value::as_i64).unwrap_or_default()
}

fn arr<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
	at(v, path).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Unix seconds of an RFC 3339 timestamp.
pub fn unix(ts: &str) -> Option<i64> {
	ts.parse::<jiff::Timestamp>().ok().map(jiff::Timestamp::as_second)
}

fn ratio(a: i64, b: i64) -> Val {
	// Sorted by readiness: fully ready last.
	let n = if b == 0 { 0 } else { a * 1000 / b };
	Val::Num(n, format!("{a}/{b}"))
}

fn n(x: i64) -> Val {
	Val::Num(x, x.to_string())
}

fn st(x: impl Into<String>) -> Val {
	let x = x.into();
	if x.is_empty() { Val::Empty } else { Val::Str(x) }
}

fn age_at(ts: &str) -> Val {
	unix(ts).map_or(Val::Empty, Val::Age)
}

/// Parses a quantity like `10Gi` or `500M` into bytes, for sorting.
pub fn bytes(q: &str) -> i64 {
	let split = q.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(q.len());
	let (num, unit) = q.split_at(split);
	let x: f64 = num.parse().unwrap_or(0.0);
	let mult: f64 = match unit {
		"Ki" => 1024.0,
		"Mi" => 1024.0_f64.powi(2),
		"Gi" => 1024.0_f64.powi(3),
		"Ti" => 1024.0_f64.powi(4),
		"Pi" => 1024.0_f64.powi(5),
		"k" => 1e3,
		"M" => 1e6,
		"G" => 1e9,
		"T" => 1e12,
		"P" => 1e15,
		_ => 1.0,
	};
	(x * mult) as i64
}

/// A pod's status the way `kubectl get pods` words it, and its tone.
pub fn pod_status(obj: &Value, deleting: bool) -> (String, Tone) {
	let phase = s(obj, &["status", "phase"]);
	let reason = s(obj, &["status", "reason"]);
	if deleting {
		return ("Terminating".into(), Tone::Warning);
	}
	if !reason.is_empty() {
		return (reason.to_owned(), Tone::Error);
	}
	// Init containers that have not finished.
	let inits = arr(obj, &["status", "initContainerStatuses"]);
	for (idx, c) in inits.iter().enumerate() {
		if s(c, &["state", "terminated", "reason"]) == "Completed" {
			continue;
		}
		let waiting = s(c, &["state", "waiting", "reason"]);
		if !waiting.is_empty() && waiting != "PodInitializing" {
			return (format!("Init:{waiting}"), Tone::Error);
		}
		let term = s(c, &["state", "terminated", "reason"]);
		if !term.is_empty() {
			return (format!("Init:{term}"), Tone::Error);
		}
		return (format!("Init:{idx}/{}", inits.len()), Tone::Pending);
	}
	let mut status = phase.to_owned();
	let mut tone = match phase {
		"Running" => Tone::Success,
		"Succeeded" => Tone::Muted,
		"Failed" => Tone::Error,
		_ => Tone::Pending,
	};
	let mut ready = true;
	for c in arr(obj, &["status", "containerStatuses"]) {
		let waiting = s(c, &["state", "waiting", "reason"]);
		let term = s(c, &["state", "terminated", "reason"]);
		if !waiting.is_empty() {
			status = waiting.to_owned();
			tone = if waiting == "ContainerCreating" { Tone::Pending } else { Tone::Error };
		} else if !term.is_empty() && phase != "Succeeded" {
			status = term.to_owned();
			tone = if term == "Completed" { Tone::Muted } else { Tone::Error };
		}
		ready &= c.get("ready").and_then(Value::as_bool).unwrap_or(false);
	}
	if phase == "Succeeded" {
		status = "Completed".into();
	}
	if status == "Running" && !ready {
		tone = Tone::Warning;
	}
	(status, tone)
}

fn tone_for(status: &str) -> Tone {
	match status {
		"Bound" | "Available" | "Active" | "Ready" | "Complete" | "Normal" => Tone::Success,
		"Pending" | "Released" | "Running" => Tone::Pending,
		"Lost" | "Failed" | "NotReady" | "Warning" => Tone::Error,
		"Terminating" | "Suspended" => Tone::Warning,
		_ => Tone::Neutral,
	}
}

fn dot(status: impl Into<String>) -> Val {
	let status = status.into();
	let tone = tone_for(&status);
	Val::Dot(status, tone)
}

fn join(values: impl IntoIterator<Item = String>) -> String {
	values.into_iter().filter(|v| !v.is_empty()).collect::<Vec<_>>().join(", ")
}

/// Projects `obj` with `projector`.
pub fn project(projector: Projector, obj: &DynamicObject) -> Row {
	let meta = &obj.metadata;
	let name = meta.name.clone().unwrap_or_default();
	let namespace = meta.namespace.clone();
	let created = meta.creation_timestamp.as_ref().map(|t| t.0.as_second());
	let age = created.map_or(Val::Empty, Val::Age);
	let d = &obj.data;
	let deleting = meta.deletion_timestamp.is_some();
	let mut cells: Vec<(&'static str, Val)> = vec![("name", Val::Str(name.clone()))];
	if let Some(ns) = &namespace {
		cells.push(("namespace", Val::Str(ns.clone())));
	}
	let more: Vec<(&'static str, Val)> = match projector {
		Projector::Pod => {
			let statuses = arr(d, &["status", "containerStatuses"]);
			let total = arr(d, &["spec", "containers"]).len() as i64;
			let ready = statuses
				.iter()
				.filter(|c| c.get("ready").and_then(Value::as_bool).unwrap_or(false))
				.count() as i64;
			let restarts: i64 = statuses.iter().map(|c| i(c, &["restartCount"])).sum();
			let (status, tone) = pod_status(d, deleting);
			vec![
				("ready", ratio(ready, total)),
				("status", Val::Dot(status, tone)),
				("restarts", n(restarts)),
				("node", st(s(d, &["spec", "nodeName"]))),
				("ip", st(s(d, &["status", "podIP"]))),
			]
		},
		Projector::Deployment => vec![
			("ready", ratio(i(d, &["status", "readyReplicas"]), i(d, &["spec", "replicas"]))),
			("updated", n(i(d, &["status", "updatedReplicas"]))),
			("available", n(i(d, &["status", "availableReplicas"]))),
		],
		Projector::StatefulSet => {
			vec![("ready", ratio(i(d, &["status", "readyReplicas"]), i(d, &["spec", "replicas"])))]
		},
		Projector::DaemonSet => vec![
			("desired", n(i(d, &["status", "desiredNumberScheduled"]))),
			("current", n(i(d, &["status", "currentNumberScheduled"]))),
			("ready", n(i(d, &["status", "numberReady"]))),
			("updated", n(i(d, &["status", "updatedNumberScheduled"]))),
			("available", n(i(d, &["status", "numberAvailable"]))),
		],
		Projector::ReplicaSet => vec![
			("desired", n(i(d, &["spec", "replicas"]))),
			("current", n(i(d, &["status", "replicas"]))),
			("ready", n(i(d, &["status", "readyReplicas"]))),
		],
		Projector::Job => {
			let succeeded = i(d, &["status", "succeeded"]);
			let completions = at(d, &["spec", "completions"]).and_then(Value::as_i64).unwrap_or(1);
			let conds = arr(d, &["status", "conditions"]);
			let has = |t: &str| conds.iter().any(|c| s(c, &["type"]) == t && s(c, &["status"]) == "True");
			let status = if has("Complete") {
				Val::Dot("Complete".into(), Tone::Success)
			} else if has("Failed") {
				Val::Dot("Failed".into(), Tone::Error)
			} else if at(d, &["spec", "suspend"]).and_then(Value::as_bool).unwrap_or(false) {
				Val::Dot("Suspended".into(), Tone::Warning)
			} else {
				Val::Dot("Running".into(), Tone::Pending)
			};
			let start = unix(s(d, &["status", "startTime"]));
			let end = unix(s(d, &["status", "completionTime"]));
			let duration = match (start, end) {
				(Some(a), Some(b)) => Val::Num(b - a, format::age(b - a)),
				_ => Val::Empty,
			};
			vec![("status", status), ("completions", ratio(succeeded, completions)), ("duration", duration)]
		},
		Projector::CronJob => {
			let suspended = at(d, &["spec", "suspend"]).and_then(Value::as_bool).unwrap_or(false);
			vec![
				("schedule", st(s(d, &["spec", "schedule"]))),
				("suspend", if suspended { Val::Dot("True".into(), Tone::Warning) } else { st("False") }),
				("active", n(arr(d, &["status", "active"]).len() as i64)),
				("last", age_at(s(d, &["status", "lastScheduleTime"]))),
			]
		},
		Projector::Service => {
			let ports = arr(d, &["spec", "ports"])
				.iter()
				.map(|p| {
					let port = i(p, &["port"]);
					let proto = s(p, &["protocol"]);
					match at(p, &["nodePort"]).and_then(Value::as_i64) {
						Some(np) => format!("{port}:{np}/{proto}"),
						None => format!("{port}/{proto}"),
					}
				})
				.collect::<Vec<_>>()
				.join(",");
			let lb = arr(d, &["status", "loadBalancer", "ingress"])
				.iter()
				.map(|e| format!("{}{}", s(e, &["ip"]), s(e, &["hostname"])));
			let external = join(
				arr(d, &["spec", "externalIPs"])
					.iter()
					.filter_map(|v| v.as_str().map(str::to_owned))
					.chain(lb),
			);
			vec![
				("type", st(s(d, &["spec", "type"]))),
				("cluster_ip", st(s(d, &["spec", "clusterIP"]))),
				("external_ip", st(external)),
				("ports", st(ports)),
			]
		},
		Projector::Ingress => {
			let hosts = join(arr(d, &["spec", "rules"]).iter().map(|r| s(r, &["host"]).to_owned()));
			let address = join(
				arr(d, &["status", "loadBalancer", "ingress"])
					.iter()
					.map(|e| format!("{}{}", s(e, &["ip"]), s(e, &["hostname"]))),
			);
			vec![
				("class", st(s(d, &["spec", "ingressClassName"]))),
				("hosts", st(hosts)),
				("address", st(address)),
			]
		},
		Projector::ConfigMap => {
			let count = |k: &str| at(d, &[k]).and_then(Value::as_object).map_or(0, |m| m.len() as i64);
			vec![("data", n(count("data") + count("binaryData")))]
		},
		Projector::Secret => {
			let count = at(d, &["data"]).and_then(Value::as_object).map_or(0, |m| m.len() as i64);
			vec![("type", st(s(d, &["type"]))), ("data", n(count))]
		},
		Projector::Pvc => {
			let cap = s(d, &["status", "capacity", "storage"]);
			vec![
				("status", dot(s(d, &["status", "phase"]))),
				("volume", st(s(d, &["spec", "volumeName"]))),
				("capacity", if cap.is_empty() { Val::Empty } else { Val::Num(bytes(cap), cap.to_owned()) }),
				("access", st(access_modes(arr(d, &["spec", "accessModes"])))),
				("class", st(s(d, &["spec", "storageClassName"]))),
			]
		},
		Projector::Pv => {
			let cap = s(d, &["spec", "capacity", "storage"]);
			let claim = match (s(d, &["spec", "claimRef", "namespace"]), s(d, &["spec", "claimRef", "name"])) {
				(_, "") => String::new(),
				(ns, name) => format!("{ns}/{name}"),
			};
			vec![
				("capacity", if cap.is_empty() { Val::Empty } else { Val::Num(bytes(cap), cap.to_owned()) }),
				("access", st(access_modes(arr(d, &["spec", "accessModes"])))),
				("reclaim", st(s(d, &["spec", "persistentVolumeReclaimPolicy"]))),
				("status", dot(s(d, &["status", "phase"]))),
				("claim", st(claim)),
				("class", st(s(d, &["spec", "storageClassName"]))),
			]
		},
		Projector::Node => {
			let ready = arr(d, &["status", "conditions"])
				.iter()
				.find(|c| s(c, &["type"]) == "Ready")
				.map(|c| s(c, &["status"]) == "True");
			let mut status = match ready {
				Some(true) => "Ready".to_owned(),
				Some(false) => "NotReady".to_owned(),
				None => "Unknown".to_owned(),
			};
			if at(d, &["spec", "unschedulable"]).and_then(Value::as_bool).unwrap_or(false) {
				status.push_str(",SchedulingDisabled");
			}
			let tone = match (ready, status.contains("Scheduling")) {
				(Some(true), false) => Tone::Success,
				(Some(true), true) => Tone::Warning,
				_ => Tone::Error,
			};
			let mut roles: Vec<String> = meta
				.labels
				.iter()
				.flatten()
				.filter_map(|(k, _)| k.strip_prefix("node-role.kubernetes.io/").map(str::to_owned))
				.collect();
			roles.sort();
			let ip = arr(d, &["status", "addresses"])
				.iter()
				.find(|a| s(a, &["type"]) == "InternalIP")
				.map(|a| s(a, &["address"]).to_owned())
				.unwrap_or_default();
			vec![
				("status", Val::Dot(status, tone)),
				("roles", st(if roles.is_empty() { "<none>".to_owned() } else { roles.join(",") })),
				("version", st(s(d, &["status", "nodeInfo", "kubeletVersion"]))),
				("ip", st(ip)),
			]
		},
		Projector::Namespace => vec![("status", dot(s(d, &["status", "phase"])))],
		Projector::Event => {
			let kind = s(d, &["involvedObject", "kind"]);
			let oname = s(d, &["involvedObject", "name"]);
			let typ = s(d, &["type"]);
			let tone = if typ == "Warning" { Tone::Warning } else { Tone::Muted };
			let last = [
				s(d, &["lastTimestamp"]),
				s(d, &["series", "lastObservedTime"]),
				s(d, &["eventTime"]),
				s(d, &["firstTimestamp"]),
			]
			.into_iter()
			.find_map(unix)
			.or(created);
			// Events replace Name: drop it and show the involved object.
			cells.retain(|(c, _)| *c != "name");
			vec![
				("type", Val::Dot(typ.to_owned(), tone)),
				("reason", st(s(d, &["reason"]))),
				("object", st(format!("{}/{oname}", kind.to_lowercase()))),
				("message", st(s(d, &["message"]).replace('\n', " "))),
				("count", n(i(d, &["count"]).max(i(d, &["series", "count"])).max(1))),
				("last", last.map_or(Val::Empty, Val::Age)),
			]
		},
		Projector::Hpa => {
			let target = format!("{}/{}", s(d, &["spec", "scaleTargetRef", "kind"]), s(d, &["spec", "scaleTargetRef", "name"]));
			vec![
				("target", st(target)),
				("min", n(at(d, &["spec", "minReplicas"]).and_then(Value::as_i64).unwrap_or(1))),
				("max", n(i(d, &["spec", "maxReplicas"]))),
				("replicas", n(i(d, &["status", "currentReplicas"]))),
			]
		},
		Projector::Generic => vec![],
	};
	cells.extend(more);
	cells.push(("age", age));
	let mut haystack = String::new();
	for (_, v) in &cells {
		if let Val::Str(t) | Val::Num(_, t) | Val::Dot(t, _) = v {
			haystack.push_str(&t.to_lowercase());
			haystack.push('\n');
		}
	}
	if let Some(uid) = &meta.uid {
		haystack.push_str(uid);
	}
	Row { key: key_of(obj), namespace, cells, haystack }
}

fn access_modes(modes: &[Value]) -> String {
	modes
		.iter()
		.filter_map(Value::as_str)
		.map(|m| match m {
			"ReadWriteOnce" => "RWO",
			"ReadOnlyMany" => "ROX",
			"ReadWriteMany" => "RWX",
			"ReadWriteOncePod" => "RWOP",
			other => other,
		})
		.collect::<Vec<_>>()
		.join(",")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn obj(v: Value) -> DynamicObject {
		serde_json::from_value(v).unwrap()
	}

	#[test]
	fn crashing_pod_reads_like_kubectl() {
		let o = obj(serde_json::json!({
			"apiVersion": "v1", "kind": "Pod",
			"metadata": {"name": "worker-1", "namespace": "prod", "uid": "U1",
				"creationTimestamp": "2026-01-01T00:00:00Z"},
			"spec": {"nodeName": "n1", "containers": [{"name": "a"}, {"name": "b"}]},
			"status": {"phase": "Running", "podIP": "10.0.0.5", "containerStatuses": [
				{"name": "a", "ready": true, "restartCount": 3, "state": {"running": {}}},
				{"name": "b", "ready": false, "restartCount": 9,
					"state": {"waiting": {"reason": "CrashLoopBackOff"}}}
			]}
		}));
		let r = project(Projector::Pod, &o);
		assert_eq!(r.key, "prod/worker-1");
		assert_eq!(r.get("ready"), &Val::Num(500, "1/2".into()));
		assert_eq!(r.get("status"), &Val::Dot("CrashLoopBackOff".into(), Tone::Error));
		assert_eq!(r.get("restarts"), &Val::Num(12, "12".into()));
		assert_eq!(r.get("age"), &Val::Age(1_767_225_600));
		assert!(r.haystack.contains("crashloopbackoff") && r.haystack.ends_with("U1"));
	}

	#[test]
	fn pod_status_covers_init_completion_and_unready() {
		let init = serde_json::json!({"status": {"phase": "Pending", "initContainerStatuses": [
			{"state": {"terminated": {"reason": "Completed"}}},
			{"state": {"running": {}}}
		]}});
		assert_eq!(pod_status(&init, false), ("Init:1/2".into(), Tone::Pending));
		let done = serde_json::json!({"status": {"phase": "Succeeded", "containerStatuses": [
			{"ready": false, "state": {"terminated": {"reason": "Completed"}}}]}});
		assert_eq!(pod_status(&done, false), ("Completed".into(), Tone::Muted));
		let unready = serde_json::json!({"status": {"phase": "Running", "containerStatuses": [
			{"ready": false, "state": {"running": {}}}]}});
		assert_eq!(pod_status(&unready, false), ("Running".into(), Tone::Warning));
		assert_eq!(pod_status(&unready, true).0, "Terminating");
	}

	#[test]
	fn sorting_orders_ages_youngest_first_and_empties_first() {
		assert_eq!(Val::Age(200).cmp(&Val::Age(100)), Ordering::Less);
		assert_eq!(Val::Empty.cmp(&Val::Str("a".into())), Ordering::Less);
		assert_eq!(Val::Num(2, "2".into()).cmp(&Val::Num(10, "10".into())), Ordering::Less);
		assert_eq!(bytes("10Gi"), 10 * 1024 * 1024 * 1024);
		assert!(bytes("500M") < bytes("1Gi"));
	}

	#[test]
	fn events_show_the_involved_object_instead_of_a_name() {
		let o = obj(serde_json::json!({
			"apiVersion": "v1", "kind": "Event",
			"metadata": {"name": "x.123", "namespace": "prod"},
			"type": "Warning", "reason": "BackOff", "message": "Back-off\nrestarting",
			"involvedObject": {"kind": "Pod", "name": "worker-1"}, "count": 4,
			"lastTimestamp": "2026-01-01T00:00:10Z"
		}));
		let r = project(Projector::Event, &o);
		assert_eq!(r.get("name"), &Val::Empty);
		assert_eq!(r.get("object"), &Val::Str("pod/worker-1".into()));
		assert_eq!(r.get("message"), &Val::Str("Back-off restarting".into()));
		assert_eq!(r.get("type"), &Val::Dot("Warning".into(), Tone::Warning));
		assert_eq!(r.get("last"), &Val::Age(1_767_225_610));
	}
}
