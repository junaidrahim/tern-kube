//! The Health page's numbers: what in a context needs attention, from the
//! stores the page keeps watched (pods, workloads, nodes, events, PVCs,
//! jobs).

use tern_sdk::ui::Tone;

use crate::{
	app::App,
	rows::{Row, Val},
};

/// A problem object, linking to its detail.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
	/// Kind id.
	pub kind:      String,
	/// Row key.
	pub key:       String,
	/// What to show.
	pub name:      String,
	/// Namespace.
	pub namespace: Option<String>,
	/// Status text.
	pub status:    String,
	/// Its tone.
	pub tone:      Tone,
	/// Created (Unix seconds).
	pub age:       Option<i64>,
}

/// A recent warning event.
#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
	/// `kind/name`.
	pub object:  String,
	/// Reason.
	pub reason:  String,
	/// Message.
	pub message: String,
	/// How many times.
	pub count:   i64,
	/// Last seen (Unix seconds).
	pub last:    i64,
}

/// One context's health.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Health {
	/// Every store the page reads has listed.
	pub loaded:        bool,
	/// Pods.
	pub pods:          usize,
	/// Running and ready, or completed.
	pub pods_ok:       usize,
	/// Deployments, StatefulSets and DaemonSets.
	pub workloads:     usize,
	/// Fully ready.
	pub workloads_ok:  usize,
	/// Nodes.
	pub nodes:         usize,
	/// Ready.
	pub nodes_ok:      usize,
	/// Pods in trouble.
	pub bad_pods:      Vec<Item>,
	/// Workloads not fully ready.
	pub bad_workloads: Vec<Item>,
	/// Nodes not ready.
	pub bad_nodes:     Vec<Item>,
	/// Claims not bound.
	pub bad_pvcs:      Vec<Item>,
	/// Failed jobs.
	pub bad_jobs:      Vec<Item>,
	/// Warning events of the last hour, newest first.
	pub warnings:      Vec<Warning>,
}

impl Health {
	/// How many objects need attention.
	pub fn problems(&self) -> usize {
		self.bad_pods.len() + self.bad_workloads.len() + self.bad_nodes.len() + self.bad_pvcs.len() + self.bad_jobs.len()
	}
}

fn age(r: &Row) -> Option<i64> {
	match r.get("age") {
		Val::Age(t) => Some(*t),
		_ => None,
	}
}

fn item(kind: &str, r: &Row, status: String, tone: Tone) -> Item {
	Item {
		kind: kind.to_owned(),
		key: r.key.clone(),
		name: r.get("name").text(0),
		namespace: r.namespace.clone(),
		status,
		tone,
		age: age(r),
	}
}

/// Ready ratio of a `ready` cell (`1/3` → (1, 3)).
fn ratio(v: &Val) -> Option<(i64, i64)> {
	let t = v.text(0);
	let (a, b) = t.split_once('/')?;
	Some((a.parse().ok()?, b.parse().ok()?))
}

/// The health of `ctx` at Unix time `now`.
pub fn summarize(app: &App, ctx: &str, now: i64) -> Health {
	let mut h = Health::default();
	let store = |kind: &str| app.store_of(ctx, kind);
	h.loaded = crate::app::HEALTH_KINDS.iter().all(|k| store(k).is_some_and(|s| s.synced || s.error.is_some()));
	if let Some(s) = store("pods") {
		for r in s.rows.values() {
			h.pods += 1;
			let (status, tone) = match r.get("status") {
				Val::Dot(t, tone) => (t.clone(), tone.clone()),
				other => (other.text(0), Tone::Neutral),
			};
			let ok = matches!(tone, Tone::Success | Tone::Muted);
			let restarts = match r.get("restarts") {
				Val::Num(n, _) => *n,
				_ => 0,
			};
			if ok && restarts >= 10 && status == "Running" {
				h.bad_pods.push(item("pods", r, format!("{restarts} restarts"), Tone::Warning));
			}
			if ok {
				h.pods_ok += 1;
			} else {
				h.bad_pods.push(item("pods", r, status, tone));
			}
		}
	}
	for kind in ["deployments.apps", "statefulsets.apps", "daemonsets.apps"] {
		let Some(s) = store(kind) else { continue };
		for r in s.rows.values() {
			h.workloads += 1;
			let ready = if kind == "daemonsets.apps" {
				let n = |c: &str| match r.get(c) {
					Val::Num(n, _) => *n,
					_ => 0,
				};
				Some((n("ready"), n("desired")))
			} else {
				ratio(r.get("ready"))
			};
			match ready {
				Some((a, b)) if a >= b => h.workloads_ok += 1,
				Some((a, b)) => h.bad_workloads.push(item(kind, r, format!("{a}/{b} ready"), Tone::Warning)),
				None => h.workloads_ok += 1,
			}
		}
	}
	if let Some(s) = store("nodes") {
		for r in s.rows.values() {
			h.nodes += 1;
			match r.get("status") {
				Val::Dot(t, Tone::Success) => {
					let _ = t;
					h.nodes_ok += 1;
				},
				Val::Dot(t, tone) => h.bad_nodes.push(item("nodes", r, t.clone(), tone.clone())),
				other => h.bad_nodes.push(item("nodes", r, other.text(0), Tone::Neutral)),
			}
		}
	}
	if let Some(s) = store("persistentvolumeclaims") {
		for r in s.rows.values() {
			if let Val::Dot(t, _) = r.get("status")
				&& t != "Bound"
			{
				h.bad_pvcs.push(item("persistentvolumeclaims", r, t.clone(), Tone::Warning));
			}
		}
	}
	if let Some(s) = store("jobs.batch") {
		for r in s.rows.values() {
			if let Val::Dot(t, _) = r.get("status")
				&& t == "Failed"
			{
				h.bad_jobs.push(item("jobs.batch", r, t.clone(), Tone::Error));
			}
		}
	}
	if let Some(s) = store("events") {
		for r in s.rows.values() {
			let warn = matches!(r.get("type"), Val::Dot(t, _) if t == "Warning");
			let last = match r.get("last") {
				Val::Age(t) => *t,
				_ => continue,
			};
			if warn && now - last <= 3600 {
				h.warnings.push(Warning {
					object:  r.get("object").text(0),
					reason:  r.get("reason").text(0),
					message: r.get("message").text(0),
					count:   match r.get("count") {
						Val::Num(n, _) => *n,
						_ => 1,
					},
					last,
				});
			}
		}
	}
	let by_name = |a: &Item, b: &Item| crate::format::natural_cmp(&a.key, &b.key);
	for v in [&mut h.bad_pods, &mut h.bad_workloads, &mut h.bad_nodes, &mut h.bad_pvcs, &mut h.bad_jobs] {
		v.sort_by(by_name);
	}
	h.warnings.sort_by(|a, b| b.last.cmp(&a.last));
	h
}
