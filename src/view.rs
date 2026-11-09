//! The view: sidebar of kinds | toolbar + live table | detail pane, and a
//! status dock. Rebuilt from `App` on every render; Tern diffs it.

use kube::api::DynamicObject;
use serde_json::Value;
use tern_sdk::{
	spans,
	ui::{
		self, Bound, Column, Extent, Gap, Justify, Node, PaneMin, SelectOption, Sort, SortDir, SplitDir, TableRow,
		TextAlign, TextCell, Tone, Token, View,
	},
};

use crate::{
	app::{App, Focus, Msg, Tab, Which},
	format,
	kinds::{Group, Projector},
	rows::{self, Val},
};

/// The view of `app` at Unix time `now`.
pub fn view(app: &mut App, now: i64) -> View<Msg> {
	let side = sidebar(app);
	let body = body(app, now);
	// kube.css pins this split to the main region so each pane scrolls alone.
	let mut split = ui::split::<Msg>()
		.key("layout")
		.dir(SplitDir::Row)
		.child(side)
		.child(body);
	if app.detail.is_some() {
		split = split
			.child(detail(app, now))
			.sizes(vec![0.16, 0.44, 0.40])
			.min(PaneMin::Each(vec![Some(Extent::Ch(18.0)), Some(Extent::Ch(40.0)), Some(Extent::Ch(46.0))]));
	} else {
		split = split.sizes(vec![0.16, 0.84]).min(PaneMin::Each(vec![Some(Extent::Ch(18.0)), Some(Extent::Ch(40.0))]));
	}
	View::new().main(ui::col().child(split.grow(1.0))).dock(dock(app))
}

fn muted(t: impl Into<String>) -> ui::Span {
	ui::span(t).style(Token::Muted)
}

/// A list/item key may not contain dots (they separate node ids).
fn safe_key(id: &str) -> String {
	id.replace('.', ":")
}

/// The node id of a sidebar group's list.
fn list_id(group: Group) -> String {
	format!("main.layout.side.g{}", group as u8)
}

/// The node id of `kind`'s sidebar item (for `reveal`).
pub fn sidebar_item_id(kind: &crate::kinds::Kind) -> String {
	format!("{}.{}", list_id(kind.group), safe_key(&kind.id))
}

fn sidebar(app: &App) -> Node<Msg> {
	let mut col = ui::col::<Msg>().key("side").gap(Gap::Sm).child(context_select(app));
	for group in Group::ALL {
		let kinds: Vec<_> = app.kinds().filter(|k| k.group == group).collect();
		let gkey = format!("g{}", group as u8);
		if kinds.is_empty() {
			if group == Group::Custom
				&& let Some(err) = app.discovery.get(&app.context)
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

fn context_select(app: &App) -> Node<Msg> {
	let mut s = ui::select::<Msg>()
		.key("ctx")
		.label("Context")
		.value(app.context.clone())
		.open(app.open == Some(Which::Context))
		.on_change(|c| Msg::Context(c.value.as_str().unwrap_or_default().to_owned()))
		.on_toggle(|collapsed| Msg::SelectOpen(Which::Context, !collapsed));
	for c in &app.contexts {
		s = s.option(SelectOption::new(c.clone(), c.clone()));
	}
	s.into()
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

fn body(app: &mut App, now: i64) -> Node<Msg> {
	let kind = app.kind().clone();
	let namespaced = kind.namespaced;
	let total = app.store().map_or(0, |s| s.rows.len());
	let shown = app.visible().len();
	let (status, tone) = match app.store() {
		Some(s) if s.error.is_some() => (if s.denied { "forbidden" } else { "error" }, Tone::Error),
		Some(s) if s.synced => ("live", Tone::Success),
		_ => ("loading", Tone::Pending),
	};
	let count = if shown == total { format!("{total}") } else { format!("{shown} / {total}") };
	let mut bar = ui::row::<Msg>().key("bar").gap(Gap::Sm);
	if namespaced {
		bar = bar.child(namespace_select(app));
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
		.child(ui::badge(status).tone(tone).key("live"))
		.child(ui::text(muted(count)).key("count"));

	let cols = kind.columns();
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
	let rows: Vec<TableRow> = app.store().map_or_else(Vec::new, |store| {
		visible
			.iter()
			.filter_map(|k| store.rows.get(k))
			.map(|r| {
				let mut row = TableRow::new(r.key.clone());
				for c in &cols {
					row = row.cell(c.id, cell(r.get(c.id), now));
				}
				row
			})
			.collect()
	});
	ui::col::<Msg>().key("body").gap(Gap::Sm).child(bar).child(table.rows(rows).grow(1.0)).into()
}

fn empty_text(app: &App) -> String {
	match app.store() {
		Some(s) if s.error.is_some() => s.error.clone().unwrap_or_default(),
		Some(s) if !s.synced => "Loading…".into(),
		Some(s) if !s.rows.is_empty() => "Nothing matches the filter".into(),
		_ => format!("No {} here", app.kind().label.to_lowercase()),
	}
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
	let Some(obj) = app.detail_obj().cloned() else {
		return ui::col::<Msg>()
			.key("detail")
			.child(ui::empty("Deleted").text("The object no longer exists."))
			.child(ui::button("Close").on_click(Msg::CloseDetail))
			.into();
	};
	let tab = app.detail.as_ref().map_or(Tab::Overview, |d| d.tab);
	let name = obj.metadata.name.clone().unwrap_or_default();
	let mut actions = ui::row::<Msg>().key("acts").gap(Gap::Xs);
	if kind.is_pod() {
		actions = actions.child(ui::button("Shell").icon("terminal").size("sm").keys(["s"]).on_click(Msg::Shell));
	}
	actions = actions
		.child(ui::button("Edit").icon("pencil").size("sm").variant("quiet").keys(["e"]).on_click(Msg::Edit))
		.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Close").on_click(Msg::CloseDetail));
	let head = ui::row::<Msg>()
		.key("head")
		.justify(Justify::Between)
		.child(ui::text(spans![ui::span(name).style(Token::Strong), muted(format!("  {}", obj_kind(&obj, &kind.ar.kind)))]))
		.child(actions);
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
	if kind.is_pod() {
		tabs = tabs.tab("logs", "Logs");
	}
	let tabs = tabs.on_select(|id| Msg::Tab(Tab::from_id(id).unwrap_or(Tab::Overview)));
	let content: Node<Msg> = match tab {
		Tab::Overview => overview(&obj, kind.projector, now),
		Tab::Yaml => yaml(app, &obj, &kind.ar),
		Tab::Events => events(app, now),
		Tab::Logs => logs(app),
	};
	ui::col::<Msg>().key("detail").gap(Gap::Sm).child(head).child(tabs).child(content).into()
}

fn obj_kind(obj: &DynamicObject, fallback: &str) -> String {
	obj.types.as_ref().map_or_else(|| fallback.to_owned(), |t| t.kind.clone())
}

fn overview(obj: &DynamicObject, projector: Projector, now: i64) -> Node<Msg> {
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
	if let Some(owners) = &m.owner_references
		&& !owners.is_empty()
	{
		let text = owners.iter().map(|o| format!("{}/{}", o.kind, o.name)).collect::<Vec<_>>().join(", ");
		kv = kv.item("Controlled by", text);
	}
	kv = kv.item("UID", ui::span(m.uid.clone().unwrap_or_default()).style(Token::Mono));
	let mut col = ui::col::<Msg>().key("overview").gap(Gap::Md).child(kv);

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
			push("Last schedule", s(d, &["status", "lastScheduleTime"]).into());
		},
		_ => {},
	}
	if !spec.is_empty() {
		let kv = spec.into_iter().fold(ui::kv::<Msg>().key("spec"), |kv, (k, v)| kv.item(k, v));
		col = col.child(ui::section().key("details").head("Details").child(kv));
	}
	if projector == Projector::Pod {
		col = col.child(containers(d));
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
	let mut bar = ui::row::<Msg>().key("logbar").gap(Gap::Sm);
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
	let state = match (&l.error, l.live) {
		(Some(e), _) => ui::span(e.clone()).style(Token::Error),
		(None, true) => ui::span(format!("{} lines · following", l.lines)).style(Token::Muted),
		(None, false) => ui::span(format!("{} lines · ended", l.lines)).style(Token::Muted),
	};
	bar = bar
		.child(ui::button("Previous").size("sm").variant("quiet").on(l.previous).on_click(Msg::Previous))
		.child(ui::button("Follow in terminal").icon("terminal").size("sm").variant("quiet").on_click(Msg::FollowInTerminal))
		.child(ui::text(state).key("state"));
	ui::col::<Msg>()
		.key("logs")
		.gap(Gap::Sm)
		.child(bar)
		.child(ui::ansi::<Msg>(l.text.clone()).key("log").follow(true).max(Bound::h(Extent::Lines(60.0))))
		.into()
}

fn dock(app: &App) -> Node<Msg> {
	let pod = app.kind().is_pod();
	let hint = match (app.focus, app.detail.is_some(), pod) {
		(Focus::Filter, ..) => "type to filter · ↵ done · esc clear",
		(_, true, true) => "↑↓ select · tab next tab · l logs · s shell · e edit · esc close",
		(_, true, false) => "↑↓ select · tab next tab · e edit · esc close",
		(_, false, true) => "↑↓ select · ↵ open · / filter · l logs · s shell · [ ] kind · q quit",
		(_, false, false) => "↑↓ select · ↵ open · / filter · [ ] kind · q quit",
	};
	let mut status = ui::status::<Msg>().child(ui::seg(app.context.clone()).icon("cloud"));
	if let Some(n) = &app.notice {
		status = status.child(ui::seg(n.clone()).icon("alert-triangle"));
	} else if let Some(err) = app.store().and_then(|s| s.error.clone()) {
		status = status.child(ui::seg(err).icon("alert-triangle"));
	}
	status.child(ui::seg(hint)).into()
}
