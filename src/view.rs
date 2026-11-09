//! The view: sidebar of kinds | toolbar + live table | detail pane, a status
//! dock, and dialogs in the layer. Rebuilt from `App` on every render; Tern
//! diffs it.

use kube::api::DynamicObject;
use serde_json::Value;
use tern_sdk::{
	spans,
	ui::{
		self, Bound, Column, Extent, Gap, Justify, MeterCell, Node, OverlaySize, PaneMin, SelectOption, Sort, SortDir,
		SplitDir, TableRow, TextAlign, TextCell, Thresholds, Tone, Token, View,
	},
};

use crate::{
	app::{self, App, Dialog, Focus, Goto, Msg, Op, Tab, Which},
	config::Protection,
	format,
	kinds::{Group, Kind, Projector},
	metrics,
	rows::{self, Row, Val},
};

/// The view of `app` at Unix time `now`.
pub fn view(app: &mut App, now: i64) -> View<Msg> {
	let side = sidebar(app);
	let body = body(app, now);
	// kube.css pins this split to the main region so each pane scrolls alone.
	let mut split = ui::split::<Msg>().key("layout").dir(SplitDir::Row).child(side).child(body);
	if app.detail.is_some() {
		split = split
			.child(detail(app, now))
			.sizes(vec![0.16, 0.44, 0.40])
			.min(PaneMin::Each(vec![Some(Extent::Ch(18.0)), Some(Extent::Ch(40.0)), Some(Extent::Ch(46.0))]));
	} else {
		split = split.sizes(vec![0.16, 0.84]).min(PaneMin::Each(vec![Some(Extent::Ch(18.0)), Some(Extent::Ch(40.0))]));
	}
	let mut v = View::new().main(ui::col().child(split.grow(1.0))).dock(dock(app));
	if let Some(layer) = dialog(app) {
		v = v.layer(ui::col().child(layer));
	}
	v
}

fn muted(t: impl Into<String>) -> ui::Span {
	ui::span(t).style(Token::Muted)
}

/// A list/item key may not contain dots (they separate node ids).
fn safe_key(id: &str) -> String {
	id.replace(['.', app::SEP], ":")
}

/// The node id of a sidebar group's list.
fn list_id(group: Group) -> String {
	format!("main.layout.side.g{}", group as u8)
}

/// The node id of `kind`'s sidebar item (for `reveal`).
pub fn sidebar_item_id(kind: &Kind) -> String {
	format!("{}.{}", list_id(kind.group), safe_key(&kind.id))
}

fn sidebar(app: &App) -> Node<Msg> {
	let mut col = ui::col::<Msg>().key("side").gap(Gap::Sm).child(context_bar(app));
	if !app.forwards.is_empty() {
		col = col.child(ui::text(muted("Port Forwards")).key("hpf")).child(forwards(app));
	}
	for group in Group::ALL {
		let kinds: Vec<_> = app.kinds().filter(|k| k.group == group).collect();
		let gkey = format!("g{}", group as u8);
		if kinds.is_empty() {
			if group == Group::Custom
				&& let Some(err) = app.discovery.get(app.primary())
			{
				col = col.child(ui::text(muted(format!("Custom resources: {err}"))).key("disc-err"));
			}
			continue;
		}
		let mut list = ui::list::<Msg>().key(gkey.clone());
		for k in kinds {
			let id = k.id.clone();
			let item = ui::item(k.label.clone()).key(safe_key(&k.id)).on_select(move |_| Msg::Kind(id.clone()));
			let item = if k.group == Group::Custom { item.detail(k.ar.group.clone()) } else { item };
			list = list.child(item);
			if k.id == app.kind {
				list = list.selected(sidebar_item_id(k));
			}
		}
		col = col.child(ui::text(muted(group.label())).key(format!("h{gkey}"))).child(list);
	}
	col.into()
}

fn context_bar(app: &App) -> Node<Msg> {
	let mut s = ui::select::<Msg>()
		.key("ctx")
		.label("Context")
		.value(app.primary().to_owned())
		.open(app.open == Some(Which::Context))
		.on_change(|c| Msg::Context(c.value.as_str().unwrap_or_default().to_owned()))
		.on_toggle(|collapsed| Msg::SelectOpen(Which::Context, !collapsed))
		.grow(1.0);
	for c in &app.contexts {
		s = s.option(SelectOption::new(c.clone(), c.clone()));
	}
	let more = if app.merged() { format!("+{}", app.active.len() - 1) } else { String::new() };
	let add = ui::button(more).icon("layers").size("sm").variant("quiet").aria("Show more contexts").on_click(Msg::PickContexts);
	ui::row::<Msg>().key("ctxbar").gap(Gap::Xs).child(s).child(add).into()
}

fn forwards(app: &App) -> Node<Msg> {
	let mut col = ui::col::<Msg>().key("pf").gap(Gap::Xs);
	for (id, f) in &app.forwards {
		let id = *id;
		let label = f.spec.label(f.local.unwrap_or(f.spec.local));
		let status = match (&f.error, f.dead, f.local, &f.active) {
			(Some(e), true, ..) => ui::span(format!("stopped: {e}")).style(Token::Error),
			(Some(e), false, ..) => ui::span(e.clone()).style(Token::Warning),
			(None, _, None, _) => muted("starting…"),
			(None, _, Some(_), Some((pod, n))) => muted(format!("{n} open → {pod}")),
			(None, _, Some(_), None) => ui::span("listening").style(Token::Success),
		};
		col = col.child(
			ui::row::<Msg>()
				.key(format!("f{id}"))
				.justify(Justify::Between)
				.child(ui::text(spans![ui::span(label).style(Token::Mono), "  ", status]))
				.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Stop").on_click(Msg::StopForward(id))),
		);
	}
	col.into()
}

fn namespace_select(app: &App) -> Node<Msg> {
	let mut s = ui::select::<Msg>()
		.key("ns")
		.value(app.namespace.clone().unwrap_or_default())
		.open(app.open == Some(Which::Namespace))
		.on_change(|c| Msg::Namespace(c.value.as_str().unwrap_or_default().to_owned()))
		.on_toggle(|collapsed| Msg::SelectOpen(Which::Namespace, !collapsed))
		.option(SelectOption::new("", "All namespaces"));
	for ns in app.namespaces() {
		s = s.option(SelectOption::new(ns.clone(), ns));
	}
	s.into()
}

fn cell(v: &Val, now: i64) -> ui::Cell {
	match v {
		Val::Dot(t, tone) => TextCell::new(t.clone()).dot(tone.clone()).into(),
		other => other.text(now).into(),
	}
}

/// A usage cell: a meter against the limit (else the request) with the
/// numbers in the tooltip, or the usage as text without either.
fn usage_cell(used: f64, of: Option<f64>, req: Option<f64>, fmt: fn(f64) -> String) -> ui::Cell {
	let mut title = format!("{} used", fmt(used));
	if let Some(r) = req {
		title.push_str(&format!(" · {} requested", fmt(r)));
	}
	match of.or(req).filter(|d| *d > 0.0) {
		Some(d) => {
			if let Some(l) = of {
				title.push_str(&format!(" · {} limit", fmt(l)));
			}
			MeterCell {
				value: Some((used / d).min(1.0)),
				thresholds: Some(Thresholds { warn: Some(0.8), bad: Some(0.95) }),
				title: Some(title),
				..MeterCell::default()
			}
			.into()
		},
		None => TextCell::new(fmt(used)).title(title).into(),
	}
}

fn metric_cell(app: &App, ctx: &str, row: &Row, col: &str) -> ui::Cell {
	let Some(u) = app.usage(ctx, row) else { return "".into() };
	let res = row.res.unwrap_or_default();
	if col == "cpu" {
		usage_cell(u.cpu, res.cpu_lim, res.cpu_req, metrics::fmt_cpu)
	} else {
		usage_cell(u.mem, res.mem_lim, res.mem_req, metrics::fmt_mem)
	}
}

fn table_row(app: &App, ctx: &str, r: &Row, cols: &[crate::kinds::ColSpec], acts: &[(&str, &str)], now: i64) -> TableRow {
	let mut row = TableRow::new(app::row_id(ctx, &r.key));
	for c in cols {
		let v = match c.id {
			"context" => ctx.into(),
			"cpu" | "memory" => metric_cell(app, ctx, r, c.id),
			id => cell(r.get(id), now),
		};
		row = row.cell(c.id, v);
	}
	for (act, _) in acts {
		row = row.menu(*act);
	}
	row
}

fn body(app: &mut App, now: i64) -> Node<Msg> {
	let kind = app.kind().clone();
	let total = app.total();
	let shown = app.visible().len();
	let errors: Vec<String> = app.stores().filter_map(|(c, s)| s.error.as_ref().map(|e| format!("{c}: {e}"))).collect();
	let denied = app.stores().any(|(_, s)| s.denied);
	let synced = app.stores().count() == app.active.len() && app.stores().all(|(_, s)| s.synced);
	let (status, tone) = match () {
		() if !errors.is_empty() => (if denied { "forbidden" } else { "error" }, Tone::Error),
		() if synced => ("live", Tone::Success),
		() => ("loading", Tone::Pending),
	};
	let count = if shown == total { format!("{total}") } else { format!("{shown} / {total}") };
	let (back, fwd) = app.can_go();
	let mut bar = ui::row::<Msg>()
		.key("bar")
		.gap(Gap::Sm)
		.child(
			ui::button("")
				.icon("arrow-left")
				.size("sm")
				.variant("quiet")
				.aria("Back")
				.disabled(!back)
				.on_click(Msg::Back),
		)
		.child(
			ui::button("")
				.icon("arrow-right")
				.size("sm")
				.variant("quiet")
				.aria("Forward")
				.disabled(!fwd)
				.on_click(Msg::Forward),
		);
	if kind.namespaced {
		bar = bar.child(namespace_select(app));
	}
	let mut badge = ui::badge(status).tone(tone).key("live");
	if !errors.is_empty() {
		badge = badge.title(errors.join("\n"));
	}
	bar = bar
		.child(
			ui::input::<Msg>()
				.key("filter")
				.text(app.query.clone())
				.cursor(app.cursor)
				.placeholder(format!("Filter {}", kind.label.to_lowercase()))
				.icon("search")
				.clear(true)
				.keys(["/"])
				.on_action("clear", Msg::ClearFilter)
				.on_focus(Msg::FocusFilter)
				.on_edit(|e| Msg::EditFilter { from: e.from, to: e.to, text: e.text.clone(), cursor: e.cursor })
				.grow(1.0),
		)
		.child(badge)
		.child(ui::text(muted(count)).key("count"));

	let cols = app.columns();
	let mut table = ui::table::<Msg>()
		.key("table")
		.selectable(true)
		.sort(Sort { col: app.sort.0.clone(), dir: app.sort.1.clone() })
		.empty(empty_text(app))
		.on_select(|id| Msg::Select(id.to_owned()))
		.on_activate(|id| Msg::Open(id.to_owned()))
		.on_action_with("sort", |a| {
			let v = a.value.as_ref();
			let col = v.and_then(|v| v.get("col")).and_then(Value::as_str).unwrap_or("name").to_owned();
			let dir = match v.and_then(|v| v.get("dir")).and_then(Value::as_str) {
				Some("desc") => SortDir::Desc,
				_ => SortDir::Asc,
			};
			Msg::Sort(col, dir)
		});
	for act in ROW_ACTS {
		table = table.on_action_with(*act, move |a| Msg::Act((*act).to_owned(), a.item.clone()));
	}
	for c in &cols {
		let mut column = Column::new(c.id, c.head).priority(c.priority).sortable(true);
		if c.grow {
			column = column.grow(1.0);
		}
		if c.numeric {
			column = column.align(TextAlign::End).mono(true);
		}
		table = table.col(column);
	}
	if let Some(sel) = &app.selected {
		table = table.selected(sel.clone());
	}
	let visible = app.visible().to_vec();
	let rows: Vec<TableRow> = visible
		.iter()
		.filter_map(|id| {
			let (row, obj) = app.lookup(id)?;
			let (ctx, _) = app::split_id(id);
			let acts = app.acts(&kind, obj);
			Some(table_row(app, ctx, row, &cols, &acts, now))
		})
		.collect();
	ui::col::<Msg>().key("body").gap(Gap::Sm).child(bar).child(table.rows(rows).grow(1.0)).into()
}

/// Every row action id (each needs its handler on the table).
const ROW_ACTS: &[&str] = &[
	"shell", "logs", "forward", "scale", "restart", "trigger", "suspend", "resume", "cordon", "uncordon", "edit", "delete",
];

fn empty_text(app: &App) -> String {
	let stores: Vec<_> = app.stores().collect();
	if let Some((c, s)) = stores.iter().find(|(_, s)| s.error.is_some()) {
		let e = s.error.clone().unwrap_or_default();
		return if app.merged() { format!("{c}: {e}") } else { e };
	}
	if stores.len() < app.active.len() || stores.iter().any(|(_, s)| !s.synced) {
		return "Loading…".into();
	}
	if stores.iter().any(|(_, s)| !s.rows.is_empty()) {
		return "Nothing matches the filter".into();
	}
	format!("No {} here", app.kind().label.to_lowercase())
}

fn s<'a>(v: &'a Value, path: &[&str]) -> &'a str {
	rows::at(v, path).and_then(Value::as_str).unwrap_or_default()
}

fn arr<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
	rows::at(v, path).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn scalar(v: Option<&Value>) -> String {
	match v {
		None | Some(Value::Null) => String::new(),
		Some(Value::String(s)) => s.clone(),
		Some(other) => other.to_string(),
	}
}

fn detail(app: &mut App, now: i64) -> Node<Msg> {
	let kind = app.kind().clone();
	let Some((ctx, obj)) = app.detail_obj().map(|(c, o)| (c.to_owned(), o.clone())) else {
		return ui::col::<Msg>()
			.key("detail")
			.child(ui::empty("Gone").text("The object no longer exists."))
			.child(ui::button("Close").on_click(Msg::CloseDetail))
			.into();
	};
	let tab = app.detail.as_ref().map_or(Tab::Overview, |d| d.tab);
	let name = obj.metadata.name.clone().unwrap_or_default();
	let mut actions = ui::row::<Msg>().key("acts").gap(Gap::Xs).wrap(true);
	for (act, label) in app.acts(&kind, &obj) {
		if act == "logs" {
			continue;
		}
		let variant = if act == "delete" { "danger" } else { "quiet" };
		let label = label.trim_end_matches('…');
		actions = actions.child(ui::button(label).size("sm").variant(variant).key(act).on_click(Msg::Act(act.to_owned(), None)));
	}
	actions = actions.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Close").key("close").on_click(Msg::CloseDetail));
	let mut title = spans![ui::span(name).style(Token::Strong), muted(format!("  {}", obj_kind(&obj, &kind.ar.kind)))];
	if app.merged() {
		title.push(muted(format!("  · {ctx}")));
	}
	let head = ui::col::<Msg>().key("head").gap(Gap::Xs).child(ui::text(title)).child(actions);
	let warnings = app
		.detail
		.as_ref()
		.map_or(0, |d| d.events.rows.values().filter(|r| matches!(r.get("type"), Val::Dot(t, _) if t == "Warning")).count());
	let events_label = match warnings {
		0 => "Events".to_owned(),
		n => format!("Events ({n})"),
	};
	let mut tabs = ui::tabs::<Msg>()
		.key("tabs")
		.active(tab.id())
		.tab("overview", "Overview")
		.tab("yaml", "YAML")
		.tab("events", events_label);
	if kind.has_logs() {
		tabs = tabs.tab("logs", "Logs");
	}
	let tabs = tabs.on_select(|id| Msg::Tab(Tab::from_id(id).unwrap_or(Tab::Overview)));
	let content: Node<Msg> = match tab {
		Tab::Overview => overview(app, &ctx, &obj, kind.projector, now),
		Tab::Yaml => yaml(app, &obj, &kind.ar),
		Tab::Events => events(app, now),
		Tab::Logs => logs(app),
	};
	ui::col::<Msg>().key("detail").gap(Gap::Sm).child(head).child(tabs).child(content).into()
}

fn obj_kind(obj: &DynamicObject, fallback: &str) -> String {
	obj.types.as_ref().map_or_else(|| fallback.to_owned(), |t| t.kind.clone())
}

fn overview(app: &App, ctx: &str, obj: &DynamicObject, projector: Projector, now: i64) -> Node<Msg> {
	let m = &obj.metadata;
	let d = &obj.data;
	let mut kv = ui::kv::<Msg>().key("meta").item("Name", m.name.clone().unwrap_or_default());
	if let Some(ns) = &m.namespace {
		kv = kv.item("Namespace", ns.clone());
	}
	if let Some(t) = &m.creation_timestamp {
		let secs = t.0.as_second();
		kv = kv.item("Created", format!("{} ago ({})", format::age(now - secs), t.0));
	}
	kv = kv.item("UID", ui::span(m.uid.clone().unwrap_or_default()).style(Token::Mono));
	let mut col = ui::col::<Msg>().key("overview").gap(Gap::Md).child(kv);

	// Owners link to their detail.
	let owners: Vec<_> = m.owner_references.iter().flatten().collect();
	if !owners.is_empty() {
		let mut row = ui::row::<Msg>().key("owners").gap(Gap::Xs).wrap(true).child(ui::text(muted("Controlled by")));
		for (i, o) in owners.iter().enumerate() {
			let label = format!("{}/{}", o.kind, o.name);
			let button = ui::button(label).size("sm").variant("quiet").icon("arrow-up-right").key(format!("o{i}"));
			row = row.child(match app.kind_of_owner(&o.api_version, &o.kind) {
				Some(kind) => {
					let key = match &m.namespace {
						Some(ns) => format!("{ns}/{}", o.name),
						None => o.name.clone(),
					};
					button.on_click(Msg::Goto(Goto { context: ctx.to_owned(), kind, key }))
				},
				None => button.disabled("Not listed here"),
			});
		}
		col = col.child(row);
	}

	let mut spec: Vec<(String, String)> = Vec::new();
	let mut push = |k: &str, v: String| {
		if !v.is_empty() {
			spec.push((k.to_owned(), v));
		}
	};
	match projector {
		Projector::Pod => {
			push("Status", rows::pod_status(d, m.deletion_timestamp.is_some()).0);
			push("Node", s(d, &["spec", "nodeName"]).into());
			push("Pod IP", s(d, &["status", "podIP"]).into());
			push("Host IP", s(d, &["status", "hostIP"]).into());
			push("QoS", s(d, &["status", "qosClass"]).into());
			push("Service account", s(d, &["spec", "serviceAccountName"]).into());
			push("Priority class", s(d, &["spec", "priorityClassName"]).into());
		},
		Projector::Deployment | Projector::StatefulSet | Projector::ReplicaSet | Projector::DaemonSet => {
			push("Replicas", scalar(rows::at(d, &["spec", "replicas"])));
			push("Ready", scalar(rows::at(d, &["status", "readyReplicas"])));
			push("Available", scalar(rows::at(d, &["status", "availableReplicas"])));
			push("Strategy", s(d, &["spec", "strategy", "type"]).to_owned() + s(d, &["spec", "updateStrategy", "type"]));
			push("Selector", selector(rows::at(d, &["spec", "selector", "matchLabels"])));
			let images: Vec<String> = arr(d, &["spec", "template", "spec", "containers"])
				.iter()
				.map(|c| s(c, &["image"]).to_owned())
				.collect();
			push("Images", images.join("\n"));
		},
		Projector::Service => {
			push("Type", s(d, &["spec", "type"]).into());
			push("Cluster IP", s(d, &["spec", "clusterIP"]).into());
			push("Selector", selector(rows::at(d, &["spec", "selector"])));
			push("Session affinity", s(d, &["spec", "sessionAffinity"]).into());
		},
		Projector::Node => {
			let info = |k: &str| s(d, &["status", "nodeInfo", k]).to_owned();
			push("OS image", info("osImage"));
			push("Kernel", info("kernelVersion"));
			push("Kubelet", info("kubeletVersion"));
			push("Runtime", info("containerRuntimeVersion"));
			push("Architecture", info("architecture"));
			let res = |k: &str| {
				format!(
					"{} allocatable of {}",
					scalar(rows::at(d, &["status", "allocatable", k])),
					scalar(rows::at(d, &["status", "capacity", k]))
				)
			};
			push("CPU", res("cpu"));
			push("Memory", res("memory"));
			push("Pods", res("pods"));
		},
		Projector::CronJob => {
			push("Schedule", s(d, &["spec", "schedule"]).into());
			push("Concurrency", s(d, &["spec", "concurrencyPolicy"]).into());
			push("Suspended", scalar(rows::at(d, &["spec", "suspend"])));
			push("Last schedule", s(d, &["status", "lastScheduleTime"]).into());
		},
		_ => {},
	}
	if let Some(usage) = usage_line(app, ctx, obj, projector) {
		push("Usage", usage);
	}
	if !spec.is_empty() {
		let kv = spec.into_iter().fold(ui::kv::<Msg>().key("spec"), |kv, (k, v)| kv.item(k, v));
		col = col.child(ui::section().key("details").head("Details").child(kv));
	}
	if projector == Projector::Pod {
		col = col.child(containers(d));
	}
	if let Some(pods) = child_pods(app, now) {
		col = col.child(pods);
	}
	let conditions = arr(d, &["status", "conditions"]);
	if !conditions.is_empty() {
		let mut t = ui::table::<Msg>()
			.key("conditions")
			.col(Column::new("type", "Type").priority(100.0))
			.col(Column::new("status", "Status").priority(90.0))
			.col(Column::new("reason", "Reason").priority(50.0))
			.col(Column::new("message", "Message").grow(1.0).priority(45.0))
			.col(Column::new("age", "Age").align(TextAlign::End).mono(true).priority(40.0));
		for (i, c) in conditions.iter().enumerate() {
			let ok = s(c, &["status"]) == "True";
			let age = rows::unix(s(c, &["lastTransitionTime"])).map(|t| format::age(now - t)).unwrap_or_default();
			t = t.row(
				TableRow::new(format!("c{i}"))
					.cell("type", s(c, &["type"]))
					.cell("status", TextCell::new(s(c, &["status"])).dot(if ok { Tone::Success } else { Tone::Muted }))
					.cell("reason", s(c, &["reason"]))
					.cell("message", s(c, &["message"]))
					.cell("age", age),
			);
		}
		col = col.child(ui::section().key("conds").head("Conditions").child(t));
	}
	if let Some(labels) = &m.labels
		&& !labels.is_empty()
	{
		let mut kv = ui::kv::<Msg>().key("labels");
		for (k, v) in labels {
			kv = kv.item(ui::span(k.clone()).style(Token::Mono), v.clone());
		}
		col = col.child(ui::section().key("labels").head(format!("Labels ({})", labels.len())).child(kv));
	}
	if let Some(ann) = &m.annotations
		&& !ann.is_empty()
	{
		let mut kv = ui::kv::<Msg>().key("ann");
		for (k, v) in ann {
			let v = if v.len() > 300 { format!("{}…", &v[..v.floor_char_boundary(300)]) } else { v.clone() };
			kv = kv.item(ui::span(k.clone()).style(Token::Mono), v);
		}
		col = col.child(
			ui::section()
				.key("annotations")
				.head(format!("Annotations ({})", ann.len()))
				.collapsible(true)
				.collapsed(true)
				.child(kv),
		);
	}
	col.into()
}

/// `120m CPU · 256Mi memory` of a pod or node, when metrics are in.
fn usage_line(app: &App, ctx: &str, obj: &DynamicObject, projector: Projector) -> Option<String> {
	let snap = app.metrics.get(ctx)?.as_ref().ok()?;
	let u = match projector {
		Projector::Pod => snap.pods.get(&rows::key_of(obj))?,
		Projector::Node => snap.nodes.get(obj.metadata.name.as_deref()?)?,
		_ => return None,
	};
	Some(format!("{} CPU · {} memory", metrics::fmt_cpu(u.cpu), metrics::fmt_mem(u.mem)))
}

/// The pods a workload, service or node has, each linking to its detail.
fn child_pods(app: &App, now: i64) -> Option<Node<Msg>> {
	let kind = app.kind();
	if !kind.selects_pods() && kind.projector != Projector::Node {
		return None;
	}
	let pods = app.child_pods();
	let mut t = ui::table::<Msg>()
		.key("pods")
		.selectable(true)
		.empty("No pods")
		.col(Column::new("name", "Pod").grow(1.0).priority(100.0))
		.col(Column::new("ready", "Ready").align(TextAlign::End).mono(true).priority(80.0))
		.col(Column::new("status", "Status").priority(90.0))
		.col(Column::new("restarts", "Restarts").align(TextAlign::End).mono(true).priority(50.0))
		.col(Column::new("age", "Age").align(TextAlign::End).mono(true).priority(60.0))
		.on_activate(|id| {
			let (context, key) = app::split_id(id);
			Msg::Goto(Goto { context: context.to_owned(), kind: "pods".into(), key: key.to_owned() })
		});
	let n = pods.len();
	for (id, r) in pods {
		let mut row = TableRow::new(id);
		for c in ["name", "ready", "status", "restarts", "age"] {
			row = row.cell(c, cell(r.get(c), now));
		}
		t = t.row(row);
	}
	Some(ui::section().key("kids").head(format!("Pods ({n})")).child(t).into())
}

fn selector(v: Option<&Value>) -> String {
	v.and_then(Value::as_object)
		.map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect::<Vec<_>>().join(", "))
		.unwrap_or_default()
}

fn containers(d: &Value) -> Node<Msg> {
	let statuses = arr(d, &["status", "containerStatuses"]);
	let mut t = ui::table::<Msg>()
		.key("containers")
		.col(Column::new("name", "Container").priority(100.0))
		.col(Column::new("image", "Image").grow(1.0).truncate("start").priority(30.0))
		.col(Column::new("state", "State").priority(90.0))
		.col(Column::new("restarts", "Restarts").align(TextAlign::End).mono(true).priority(60.0));
	for c in arr(d, &["spec", "containers"]) {
		let name = s(c, &["name"]);
		let st = statuses.iter().find(|x| s(x, &["name"]) == name);
		let (state, tone) = st.map_or(("Unknown".to_owned(), Tone::Muted), |x| {
			let ready = x.get("ready").and_then(Value::as_bool).unwrap_or(false);
			if let Some(w) = rows::at(x, &["state", "waiting"]) {
				(s(w, &["reason"]).to_owned(), Tone::Error)
			} else if let Some(term) = rows::at(x, &["state", "terminated"]) {
				(s(term, &["reason"]).to_owned(), Tone::Muted)
			} else if ready {
				("Running".to_owned(), Tone::Success)
			} else {
				("Running (not ready)".to_owned(), Tone::Warning)
			}
		});
		let restarts = st.and_then(|x| x.get("restartCount")).and_then(Value::as_i64).unwrap_or(0);
		t = t.row(
			TableRow::new(name.to_owned())
				.cell("name", name)
				.cell("image", s(c, &["image"]))
				.cell("state", TextCell::new(state).dot(tone))
				.cell("restarts", restarts.to_string()),
		);
	}
	ui::section().key("ctrs").head("Containers").child(t).into()
}

fn yaml(app: &mut App, obj: &DynamicObject, ar: &kube::api::ApiResource) -> Node<Msg> {
	let rv = obj.metadata.resource_version.clone().unwrap_or_default();
	let cached = app.detail.as_ref().and_then(|d| d.yaml.as_ref()).filter(|(v, _)| *v == rv).map(|(_, y)| y.clone());
	let text = cached.unwrap_or_else(|| {
		// List responses omit each item's apiVersion and kind; put them back.
		let mut full = obj.clone();
		full.types.get_or_insert_with(|| kube::api::TypeMeta { api_version: ar.api_version.clone(), kind: ar.kind.clone() });
		let y = serde_yaml_ng::to_string(&full).unwrap_or_else(|e| format!("# {e}"));
		if let Some(d) = app.detail.as_mut() {
			d.yaml = Some((rv, y.clone()));
		}
		y
	});
	ui::code::<Msg>(text).key("yaml").lang("yaml").numbers(true).into()
}

fn events(app: &App, now: i64) -> Node<Msg> {
	let Some(d) = &app.detail else { return ui::text("").into() };
	let mut list: Vec<_> = d.events.rows.values().collect();
	list.sort_by(|a, b| a.get("last").cmp(b.get("last")));
	let mut t = ui::table::<Msg>()
		.key("events")
		.empty(if d.events.synced { "No events" } else { "Loading…" })
		.col(Column::new("type", "Type").priority(90.0))
		.col(Column::new("reason", "Reason").priority(80.0))
		.col(Column::new("message", "Message").grow(1.0).priority(100.0))
		.col(Column::new("count", "Count").align(TextAlign::End).mono(true).priority(30.0))
		.col(Column::new("last", "Last Seen").align(TextAlign::End).mono(true).priority(60.0));
	for r in list {
		let mut row = TableRow::new(r.key.clone());
		for c in ["type", "reason", "message", "count", "last"] {
			row = row.cell(c, cell(r.get(c), now));
		}
		t = t.row(row);
	}
	t.into()
}

fn logs(app: &App) -> Node<Msg> {
	let Some(d) = &app.detail else { return ui::text("").into() };
	let l = &d.logs;
	let containers = app.containers();
	let mut bar = ui::row::<Msg>().key("logbar").gap(Gap::Sm).wrap(true);
	if containers.len() > 1 {
		let mut sel = ui::select::<Msg>()
			.key("container")
			.label("Container")
			.value(l.container.clone().unwrap_or_default())
			.open(app.open == Some(Which::Container))
			.on_change(|c| Msg::Container(c.value.as_str().unwrap_or_default().to_owned()))
			.on_toggle(|collapsed| Msg::SelectOpen(Which::Container, !collapsed));
		for c in containers {
			sel = sel.option(SelectOption::new(c.clone(), c));
		}
		bar = bar.child(sel);
	}
	let pods = if l.pods > 1 { format!(" from {} pods", l.pods) } else { String::new() };
	let state = match (&l.error, l.live) {
		(Some(e), _) => ui::span(e.clone()).style(Token::Error),
		(None, true) => ui::span(format!("{} lines{pods} · following", l.lines)).style(Token::Muted),
		(None, false) => ui::span(format!("{} lines{pods} · ended", l.lines)).style(Token::Muted),
	};
	bar = bar.child(ui::button("Previous").size("sm").variant("quiet").on(l.previous).on_click(Msg::Previous));
	if app.kind().is_pod() {
		bar = bar.child(
			ui::button("Follow in terminal")
				.icon("terminal")
				.size("sm")
				.variant("quiet")
				.on_click(Msg::Act("follow".into(), None)),
		);
	}
	bar = bar.child(ui::text(state).key("state"));
	ui::col::<Msg>()
		.key("logs")
		.gap(Gap::Sm)
		.child(bar)
		.child(ui::ansi::<Msg>(l.text.clone()).key("log").follow(true).max(Bound::h(Extent::Lines(60.0))))
		.into()
}

fn dialog(app: &App) -> Option<Node<Msg>> {
	let d = app.dialog.as_ref()?;
	let cancel = ui::button("Cancel").variant("quiet").keys(["esc"]).key("cancel").on_click(Msg::Cancel);
	let node: Node<Msg> = match d {
		Dialog::Confirm { target, op, unlock } => {
			let verb = match op {
				Op::Change(m) => m.verb(),
				Op::Terminal(crate::actions::Action::Shell) => "Open a shell in".into(),
				Op::Terminal(crate::actions::Action::Edit) => "Edit".into(),
				Op::Terminal(crate::actions::Action::FollowLogs) => "Follow the log of".into(),
			};
			let danger = matches!(op, Op::Change(m) if m.destructive());
			let go_label = if *unlock { format!("Unlock for 15 min and {}", verb.to_lowercase()) } else { verb.clone() };
			let go = ui::button(go_label)
				.variant(if danger { "danger" } else { "primary" })
				.keys(["enter"])
				.key("go")
				.on_click(Msg::Confirm);
			let mut body = ui::col::<Msg>().key("body").gap(Gap::Sm).child(ui::text(spans![
				format!("{verb} "),
				ui::span(target.label()).style(Token::Strong),
				" in context ",
				ui::span(target.context.clone()).style(Token::Strong),
				target.obj.metadata.namespace.as_ref().map_or_else(String::new, |ns| format!(", namespace {ns}")),
				"?"
			]));
			if *unlock {
				body = body.child(ui::text(ui::span("This context is protected.").style(Token::Warning)).key("warn"));
			}
			ui::overlay::<Msg>()
				.key("confirm")
				.modal(true)
				.size(OverlaySize::Sm)
				.head(format!("{verb} {}", target.label()))
				.icon(if danger { "trash" } else { "alert-triangle" })
				.foot("confirm.foot")
				.child(body)
				.child(ui::row::<Msg>().key("foot").justify(Justify::End).gap(Gap::Sm).child(cancel).child(go))
				.into()
		},
		Dialog::Scale { target, replicas } => ui::overlay::<Msg>()
			.key("scale")
			.modal(true)
			.size(OverlaySize::Sm)
			.head(format!("Scale {}", target.label()))
			.icon("layers")
			.foot("scale.foot")
			.child(
				ui::col::<Msg>().key("body").gap(Gap::Sm).child(ui::text(muted(format!("Context {}", target.context)))).child(
					ui::input::<Msg>()
						.key("replicas")
						.text(replicas.clone())
						.cursor(replicas.len())
						.unit("replicas")
						.stepper(true)
						.mono(true)
						.on_action("inc", Msg::Step(1))
						.on_action("dec", Msg::Step(-1)),
				),
			)
			.child(
				ui::row::<Msg>().key("foot").justify(Justify::End).gap(Gap::Sm).child(cancel).child(
					ui::button("Scale").variant("primary").keys(["enter"]).key("go").on_click(Msg::Confirm),
				),
			)
			.into(),
		Dialog::Forward { target, ports, port, local } => {
			let mut list = ui::list::<Msg>().key("ports").selected(format!("layer.forward.body.ports.p{port}"));
			for (p, label) in ports {
				let p = *p;
				list = list.child(ui::item(label.clone()).key(format!("p{p}")).on_select(move |_| Msg::ForwardPort(p)));
			}
			ui::overlay::<Msg>()
				.key("forward")
				.modal(true)
				.size(OverlaySize::Sm)
				.head(format!("Port forward {}", target.label()))
				.icon("plug")
				.foot("forward.foot")
				.child(
					ui::col::<Msg>()
						.key("body")
						.gap(Gap::Sm)
						.child(ui::text(muted("Remote port")).key("h1"))
						.child(list)
						.child(ui::text(muted("Local port (0 picks a free one)")).key("h2"))
						.child(ui::input::<Msg>().key("local").text(local.clone()).cursor(local.len()).prefix("localhost:").mono(true)),
				)
				.child(
					ui::row::<Msg>().key("foot").justify(Justify::End).gap(Gap::Sm).child(cancel).child(
						ui::button("Forward").variant("primary").keys(["enter"]).key("go").on_click(Msg::Confirm),
					),
				)
				.into()
		},
		Dialog::Contexts => {
			let mut list = ui::list::<Msg>().key("ctxs");
			for c in &app.contexts {
				let on = app.active.contains(c);
				let ctx = c.clone();
				let detail = match app.protection(c) {
					Protection::None => "",
					Protection::Protected => "protected",
					Protection::ReadOnly => "read-only",
				};
				list = list.child(
					ui::item(c.clone())
						.key(safe_key(c))
						.icon(if on { "check-square" } else { "square" })
						.detail(detail)
						.on_select(move |_| Msg::ToggleContext(ctx.clone())),
				);
			}
			ui::overlay::<Msg>()
				.key("contexts")
				.modal(true)
				.size(OverlaySize::Md)
				.head("Contexts")
				.meta(format!("{} shown", app.active.len()))
				.icon("layers")
				.foot("contexts.foot")
				.child(ui::col::<Msg>().key("body").gap(Gap::Sm).child(ui::text(muted("Every checked context is merged into one table."))).child(list))
				.child(
					ui::row::<Msg>()
						.key("foot")
						.justify(Justify::End)
						.child(ui::button("Done").variant("primary").keys(["esc"]).key("done").on_click(Msg::Cancel)),
				)
				.into()
		},
	};
	Some(node)
}

fn dock(app: &App) -> Node<Msg> {
	let kind = app.kind();
	let hint = if app.dialog.is_some() {
		"↵ confirm · esc cancel".to_owned()
	} else if app.g_pending {
		app::JUMPS.iter().map(|(k, id)| format!("{k} {}", id.split('.').next().unwrap_or(id))).collect::<Vec<_>>().join(" · ")
	} else if app.focus == Focus::Filter {
		"type to filter · ↵ done · esc clear".to_owned()
	} else {
		let mut h = vec!["↑↓ select", "↵ open", "/ filter", "g jump", "⌥←→ history"];
		if kind.has_logs() {
			h.push("l logs");
		}
		if kind.is_pod() {
			h.extend(["s shell", "f forward"]);
		}
		if kind.scalable() {
			h.push("S scale");
		}
		if kind.restartable() {
			h.push("R restart");
		}
		h.extend(["e edit", "⌦ delete", "C contexts"]);
		h.join(" · ")
	};
	let ctx_label = if app.merged() { format!("{} contexts", app.active.len()) } else { app.primary().to_owned() };
	let mut status = ui::status::<Msg>().child(ui::seg(ctx_label).icon("cloud"));
	if let Some(p) = app.pending.values().next_back() {
		status = status.child(ui::seg(format!("{p}…")).icon("loader"));
	}
	if let Some(n) = &app.notice {
		status = status.child(ui::seg(n.text.clone()).icon(if n.error { "alert-triangle" } else { "check" }));
	} else if let Some((c, e)) = app.stores().find_map(|(c, s)| s.error.as_ref().map(|e| (c, e))) {
		status = status.child(ui::seg(format!("{c}: {e}")).icon("alert-triangle"));
	}
	status.child(ui::seg(hint)).into()
}
