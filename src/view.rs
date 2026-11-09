//! The view: sidebar of kinds | toolbar + live table (or the Health page) |
//! detail pane, a status dock, and dialogs in the layer. Rebuilt from `App`
//! on every render; Tern diffs it.

use kube::api::DynamicObject;
use serde_json::Value;
use tern_sdk::{
	spans,
	ui::{
		self, ActButton, Bound, Column, Extent, Gap, Justify, MeterCell, Node, OverlaySize, PaneMin, SelectOption,
		Sort, SortDir, SplitDir, StatItem, TableRow, TextAlign, TextCell, Thresholds, Tone, Token, View,
	},
};

use crate::{
	app::{self, App, Dialog, Focus, Goto, Msg, Op, Tab, Which},
	config::Protection,
	decode, format,
	health,
	kinds::{ColSpec, Group, Kind, Projector},
	metrics,
	rows::{self, Row, Val},
};

/// The view of `app` at Unix time `now`.
pub fn view(app: &mut App, now: i64) -> View<Msg> {
	let side = sidebar(app);
	let body = if app.kind().is_page() { health_page(app, now) } else { body(app, now) };
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
	id.replace(['.', app::SEP, '/'], ":")
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
	if !app.pending.is_empty() {
		col = col.child(ui::text(muted("Running")).key("hops")).child(operations(app));
	}
	if !app.forwards.is_empty() {
		let stopped = app.forwards.values().any(|f| !f.running);
		let mut head = ui::row::<Msg>().key("hpf").justify(Justify::Between).child(ui::text(muted("Port Forwards")));
		if stopped {
			head = head.child(ui::button("Start all").size("sm").variant("quiet").on_click(Msg::StartAllForwards));
		}
		col = col.child(head).child(forwards(app));
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
		s = s.option(SelectOption::new(c.clone(), app.config.display(c).to_owned()));
	}
	let more = if app.merged() { format!("+{}", app.active.len() - 1) } else { String::new() };
	let add = ui::button(more).icon("layers").size("sm").variant("quiet").aria("Show more contexts").on_click(Msg::PickContexts);
	ui::row::<Msg>().key("ctxbar").gap(Gap::Xs).child(s).child(add).into()
}

fn operations(app: &App) -> Node<Msg> {
	let mut col = ui::col::<Msg>().key("ops").gap(Gap::Xs);
	for (id, p) in &app.pending {
		let id = *id;
		let text = p.progress.clone().map_or_else(|| format!("{}…", p.label), |pr| format!("{} · {pr}", p.label));
		let mut row = ui::row::<Msg>().key(format!("op{id}")).justify(Justify::Between).child(ui::text(spans![ui::span(text)]));
		if p.long {
			row = row.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Cancel").on_click(Msg::CancelOp(id)));
		}
		col = col.child(row);
	}
	col.into()
}

fn forwards(app: &App) -> Node<Msg> {
	let mut col = ui::col::<Msg>().key("pf").gap(Gap::Xs);
	for (id, f) in &app.forwards {
		let id = *id;
		let label = f.spec.label(f.local.unwrap_or(f.spec.local));
		let status = match (f.running, &f.error, f.dead, f.local, &f.active) {
			(false, Some(e), true, ..) => ui::span(format!("stopped: {e}")).style(Token::Error),
			(false, ..) => muted("stopped"),
			(true, Some(e), ..) => ui::span(e.clone()).style(Token::Warning),
			(true, None, _, None, _) => muted("starting…"),
			(true, None, _, Some(_), Some((pod, n))) => muted(format!("{n} open → {pod}")),
			(true, None, _, Some(_), None) => ui::span("listening").style(Token::Success),
		};
		let toggle = if f.running {
			ui::button("").icon("square").size("sm").variant("quiet").aria("Stop").on_click(Msg::StopForward(id))
		} else {
			ui::button("").icon("play").size("sm").variant("quiet").aria("Start").on_click(Msg::StartForward(id))
		};
		col = col.child(
			ui::row::<Msg>()
				.key(format!("f{id}"))
				.justify(Justify::Between)
				.child(ui::text(spans![ui::span(label).style(Token::Mono), "  ", status]))
				.child(
					ui::row::<Msg>()
						.key("btns")
						.gap(Gap::Xs)
						.child(toggle)
						.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Forget").on_click(Msg::RemoveForward(id))),
				),
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
	let Some((u, res)) = app.usage(ctx, row) else { return "".into() };
	if col == "cpu" {
		usage_cell(u.cpu, res.cpu_lim, res.cpu_req, metrics::fmt_cpu)
	} else {
		usage_cell(u.mem, res.mem_lim, res.mem_req, metrics::fmt_mem)
	}
}

fn context_cell(app: &App, ctx: &str) -> ui::Cell {
	let name = app.config.display(ctx).to_owned();
	match app.config.tone(ctx) {
		Some(t) => TextCell::new(name).dot(t).title(ctx.to_owned()).into(),
		None => TextCell::new(name).title(ctx.to_owned()).into(),
	}
}

fn table_row(app: &App, ctx: &str, r: &Row, cols: &[ColSpec], acts: &[(&str, &str)], now: i64) -> TableRow {
	let mut row = TableRow::new(app::row_id(ctx, &r.key));
	for c in cols {
		let id = c.id.as_ref();
		let v = match id {
			"context" => context_cell(app, ctx),
			"cpu" | "memory" => metric_cell(app, ctx, r, id),
			l if l.starts_with("label:") => app.label_value(ctx, r, l).unwrap_or_default().into(),
			_ => cell(r.get(id), now),
		};
		row = row.cell(c.id.clone().into_owned(), v);
	}
	for (act, _) in acts {
		row = row.menu(*act);
	}
	row
}

/// The stale/sign-in banner of contexts whose watches keep failing.
fn trouble(app: &App) -> Option<Node<Msg>> {
	let bad: Vec<(&str, &app::Store)> = app.stores().filter(|(_, s)| s.auth || s.stale()).collect();
	if bad.is_empty() {
		return None;
	}
	let mut col = ui::col::<Msg>().key("trouble").gap(Gap::Xs);
	for (ctx, s) in bad {
		let name = app.config.display(ctx).to_owned();
		let what = if s.auth { "Sign-in needed" } else { "Showing stale data" };
		let mut row = ui::row::<Msg>()
			.key(format!("t{}", safe_key(ctx)))
			.gap(Gap::Sm)
			.child(ui::badge(what).tone(if s.auth { Tone::Error } else { Tone::Warning }))
			.child(
				ui::text(spans![
					ui::span(name).style(Token::Strong),
					muted(format!("  {}", s.error.clone().unwrap_or_default()))
				])
				.grow(1.0),
			);
		if s.auth {
			row = row.child(ui::button("Sign in").icon("log-in").size("sm").variant("primary").on_click(Msg::SignIn(ctx.to_owned())));
		}
		row = row.child(ui::button("Reconnect").icon("refresh-cw").size("sm").variant("quiet").on_click(Msg::Reconnect(ctx.to_owned())));
		col = col.child(row);
	}
	Some(col.into())
}

fn body(app: &mut App, now: i64) -> Node<Msg> {
	let kind = app.kind().clone();
	let total = app.total();
	let shown = app.visible().len();
	let errors: Vec<String> = app.stores().filter_map(|(c, s)| s.error.as_ref().map(|e| format!("{c}: {e}"))).collect();
	let denied = app.stores().any(|(_, s)| s.denied);
	let stale = app.stores().any(|(_, s)| s.stale());
	let synced = app.stores().count() == app.active.len() && app.stores().all(|(_, s)| s.synced);
	let (status, tone) = match () {
		() if stale => ("stale", Tone::Warning),
		() if !errors.is_empty() => (if denied { "forbidden" } else { "error" }, Tone::Error),
		() if synced => ("live", Tone::Success),
		() => ("loading", Tone::Pending),
	};
	let count = if shown == total { format!("{total}") } else { format!("{shown} / {total}") };
	let (back, fwd) = app.can_go();
	let mut bar = ui::row::<Msg>()
		.key("bar")
		.gap(Gap::Sm)
		.child(ui::button("").icon("arrow-left").size("sm").variant("quiet").aria("Back").disabled(!back).on_click(Msg::Back))
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
				.text(app.query.text.clone())
				.cursor(app.query.cursor)
				.placeholder(format!("Filter {}", kind.label.to_lowercase()))
				.icon("search")
				.clear(true)
				.keys(["/"])
				.on_action("clear", Msg::ClearFilter)
				.on_focus(Msg::Focus(Focus::Filter))
				.on_edit(|e| Msg::EditField(Focus::Filter, e.from, e.to, e.text.clone(), e.cursor))
				.grow(1.0),
		)
		.child(badge)
		.child(ui::text(muted(count)).key("count"))
		.child(ui::button("").icon("columns").size("sm").variant("quiet").aria("Label columns").on_click(Msg::Columns))
		.child(ui::button("").icon("command").size("sm").variant("quiet").aria("Command palette").keys(["⌃K"]).on_click(Msg::Palette));

	let cols = app.columns();
	let mut table = ui::table::<Msg>()
		.key("table")
		.selectable(true)
		.multi(true)
		.variant("bar")
		.picked(app.picked.clone())
		.sort(Sort { col: app.sort.0.clone(), dir: app.sort.1.clone() })
		.empty(empty_text(app))
		.on_select(|id| Msg::Select(id.to_owned()))
		.on_activate(|id| Msg::Open(id.to_owned()))
		.on_change(|c| {
			let ids = c.value.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
			Msg::Pick(ids)
		})
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
	for (act, label) in app::BULK {
		let ok = match act {
			"bulk-logs" => kind.has_logs(),
			"bulk-restart" => kind.restartable(),
			"bulk-scale" => kind.scalable(),
			_ => kind.projector != Projector::Helm,
		};
		if ok {
			let mut b = ActButton::new(act, label);
			if act == "bulk-delete" {
				b.variant = Some("danger".into());
			}
			table = table.bar_button(b).on_action(act, Msg::Bulk(act.to_owned()));
		}
	}
	for c in &cols {
		let mut column = Column::new(c.id.clone().into_owned(), c.head.clone().into_owned()).priority(c.priority).sortable(true);
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
	let mut col = ui::col::<Msg>().key("body").gap(Gap::Sm).child(bar);
	if let Some(t) = trouble(app) {
		col = col.child(t);
	}
	col.child(table.rows(rows).grow(1.0)).into()
}

/// Every row action id (each needs its handler on the table).
const ROW_ACTS: &[&str] = &[
	"shell", "debug", "logs", "forward", "scale", "restart", "trigger", "suspend", "resume", "cordon", "uncordon", "drain",
	"reconcile", "refresh", "compare", "edit", "delete", "helm-uninstall",
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

/// The Health page: per shown context, what needs attention.
fn health_page(app: &App, now: i64) -> Node<Msg> {
	let mut col = ui::col::<Msg>().key("body").gap(Gap::Md);
	if let Some(t) = trouble(app) {
		col = col.child(t);
	}
	for ctx in &app.active {
		let h = health::summarize(app, ctx, now);
		let name = app.config.display(ctx).to_owned();
		let tone = if h.problems() == 0 { Tone::Success } else { Tone::Warning };
		let stats = ui::stats([
			StatItem::new("Pods", format!("{}/{}", h.pods_ok, h.pods)),
			StatItem::new("Workloads ready", format!("{}/{}", h.workloads_ok, h.workloads)),
			StatItem::new("Nodes ready", format!("{}/{}", h.nodes_ok, h.nodes)),
			StatItem::new("Warnings (1h)", h.warnings.len().to_string()),
		]);
		let mut card = ui::col::<Msg>().key(format!("c{}", safe_key(ctx))).gap(Gap::Sm).child(
			ui::row::<Msg>()
				.key("head")
				.gap(Gap::Sm)
				.child(ui::text(ui::span(name).style(Token::Strong)))
				.child(ui::badge(if h.problems() == 0 { "healthy".to_owned() } else { format!("{} problems", h.problems()) }).tone(tone)),
		);
		card = card.child(stats.key("stats"));
		let link_table = |key: &str, head: &str, items: &[health::Item]| -> Option<Node<Msg>> {
			if items.is_empty() {
				return None;
			}
			let mut t = ui::table::<Msg>()
				.key(key.to_owned())
				.selectable(true)
				.col(Column::new("what", "Object").grow(1.0).priority(100.0))
				.col(Column::new("ns", "Namespace").priority(40.0))
				.col(Column::new("status", "Status").priority(90.0))
				.col(Column::new("age", "Age").align(TextAlign::End).mono(true).priority(30.0));
			for it in items.iter().take(30) {
				t = t.row(
					TableRow::new(app::row_id(ctx, &format!("{}|{}", it.kind, it.key)))
						.cell("what", it.name.clone())
						.cell("ns", it.namespace.clone().unwrap_or_default())
						.cell("status", TextCell::new(it.status.clone()).dot(it.tone.clone()))
						.cell("age", it.age.map(|a| format::age(now - a)).unwrap_or_default()),
				);
			}
			let t = t.on_activate(|id| {
				let (context, rest) = app::split_id(id);
				let (kind, key) = rest.split_once('|').unwrap_or(("pods", rest));
				Msg::Goto(Goto { context: context.to_owned(), kind: kind.to_owned(), key: key.to_owned() })
			});
			Some(ui::section().key(format!("s{key}")).head(format!("{head} ({})", items.len())).child(t).into())
		};
		for (key, head, items) in [
			("pods", "Pods with problems", &h.bad_pods),
			("workloads", "Unready workloads", &h.bad_workloads),
			("nodes", "Nodes not ready", &h.bad_nodes),
			("pvcs", "Pending volume claims", &h.bad_pvcs),
			("jobs", "Failed jobs", &h.bad_jobs),
		] {
			if let Some(t) = link_table(key, head, items) {
				card = card.child(t);
			}
		}
		if !h.warnings.is_empty() {
			let mut t = ui::table::<Msg>()
				.key("warn")
				.col(Column::new("object", "Object").priority(80.0))
				.col(Column::new("reason", "Reason").priority(90.0))
				.col(Column::new("message", "Message").grow(1.0).priority(100.0))
				.col(Column::new("count", "Count").align(TextAlign::End).mono(true).priority(30.0))
				.col(Column::new("last", "Last Seen").align(TextAlign::End).mono(true).priority(60.0));
			for (i, w) in h.warnings.iter().take(40).enumerate() {
				t = t.row(
					TableRow::new(format!("w{i}"))
						.cell("object", w.object.clone())
						.cell("reason", w.reason.clone())
						.cell("message", w.message.clone())
						.cell("count", w.count.to_string())
						.cell("last", format::age(now - w.last)),
				);
			}
			card = card.child(ui::section().key("swarn").head(format!("Warning events ({})", h.warnings.len())).child(t));
		}
		if h.problems() == 0 && h.warnings.is_empty() {
			card = card.child(ui::text(muted(if h.loaded { "Nothing needs attention." } else { "Loading…" })).key("ok"));
		}
		col = col.child(card);
	}
	col.into()
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
	let multi = app.detail.as_ref().map_or(0, |d| d.multi.len());
	if multi > 0 {
		let head = ui::row::<Msg>()
			.key("head")
			.justify(Justify::Between)
			.child(ui::text(spans![ui::span(format!("{multi} picked")).style(Token::Strong), muted(format!("  {}", kind.label))]))
			.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Close").on_click(Msg::CloseDetail));
		return ui::col::<Msg>().key("detail").gap(Gap::Sm).child(head).child(logs(app)).into();
	}
	let Some((ctx, obj)) = app.detail_obj().map(|(c, o)| (c.to_owned(), o.clone())) else {
		return ui::col::<Msg>()
			.key("detail")
			.child(ui::empty("Gone").text("The object no longer exists."))
			.child(ui::button("Close").on_click(Msg::CloseDetail))
			.into();
	};
	let tab = app.detail.as_ref().map_or(Tab::Overview, |d| d.tab);
	let name = if kind.projector == Projector::Helm {
		obj.metadata.labels.as_ref().and_then(|l| l.get("name").cloned()).unwrap_or_default()
	} else {
		obj.metadata.name.clone().unwrap_or_default()
	};
	let mut actions = ui::row::<Msg>().key("acts").gap(Gap::Xs).wrap(true);
	for (act, label) in app.acts(&kind, &obj) {
		if act == "logs" {
			continue;
		}
		let variant = if matches!(act, "delete" | "drain" | "helm-uninstall") { "danger" } else { "quiet" };
		let label = label.trim_end_matches('…');
		actions = actions.child(ui::button(label).size("sm").variant(variant).key(act).on_click(Msg::Act(act.to_owned(), None)));
	}
	actions = actions.child(ui::button("").icon("x").size("sm").variant("quiet").aria("Close").key("close").on_click(Msg::CloseDetail));
	let kind_name = if kind.projector == Projector::Helm { "Helm release".to_owned() } else { obj_kind(&obj, &kind.ar.kind) };
	let mut title = spans![ui::span(name).style(Token::Strong), muted(format!("  {kind_name}"))];
	if app.merged() {
		title.push(muted(format!("  · {}", app.config.display(&ctx))));
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
	let mut tabs = ui::tabs::<Msg>().key("tabs").active(tab.id());
	for t in app.tabs() {
		let label = match t {
			Tab::Overview => "Overview".to_owned(),
			Tab::Yaml => "YAML".to_owned(),
			Tab::Events => events_label.clone(),
			Tab::Logs => "Logs".to_owned(),
			Tab::Compare => "Compare".to_owned(),
		};
		tabs = tabs.tab(t.id(), label);
	}
	let tabs = tabs.on_select(|id| Msg::Tab(Tab::from_id(id).unwrap_or(Tab::Overview)));
	let content: Node<Msg> = match tab {
		Tab::Overview => overview(app, &ctx, &obj, &kind, now),
		Tab::Yaml => yaml(app, &obj, &kind.ar),
		Tab::Events => events(app, now),
		Tab::Logs => logs(app),
		Tab::Compare => compare(app),
	};
	ui::col::<Msg>().key("detail").gap(Gap::Sm).child(head).child(tabs).child(content).into()
}

fn obj_kind(obj: &DynamicObject, fallback: &str) -> String {
	obj.types.as_ref().map_or_else(|| fallback.to_owned(), |t| t.kind.clone())
}

fn overview(app: &App, ctx: &str, obj: &DynamicObject, kind: &Kind, now: i64) -> Node<Msg> {
	let projector = kind.projector;
	if projector == Projector::Helm {
		return helm_overview(app, obj, now);
	}
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
		Projector::ArgoApp => {
			push("Sync", s(d, &["status", "sync", "status"]).into());
			push("Health", s(d, &["status", "health", "status"]).into());
			push("Revision", s(d, &["status", "sync", "revision"]).into());
			push("Source", s(d, &["spec", "source", "repoURL"]).to_owned() + " " + s(d, &["spec", "source", "path"]));
			push("Destination", format!("{} {}", s(d, &["spec", "destination", "server"]), s(d, &["spec", "destination", "namespace"])));
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
	match projector {
		Projector::Pod => col = col.child(containers(d)),
		Projector::Secret => col = col.child(secret_data(app, obj, now)),
		Projector::ConfigMap => col = col.child(configmap_data(obj)),
		_ => {},
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

/// A Secret's values: hidden until revealed, decoded, with copy buttons;
/// TLS certificates are decoded too.
fn secret_data(app: &App, obj: &DynamicObject, now: i64) -> Node<Msg> {
	let revealed = app.detail.as_ref().map(|d| &d.revealed);
	let data = obj.data.get("data").and_then(Value::as_object);
	let n = data.map_or(0, serde_json::Map::len);
	let all = revealed.is_some_and(|r| r.len() == n && n > 0);
	let mut col = ui::col::<Msg>().key("data").gap(Gap::Sm);
	for (k, v) in data.into_iter().flatten() {
		let shown = revealed.is_some_and(|r| r.contains(k));
		let decoded = v.as_str().map_or_else(|| Err("not text".into()), decode::secret_value);
		let value: Node<Msg> = match (&decoded, shown) {
			(Ok(text), true) => ui::code::<Msg>(text.clone()).key("v").wrap(true).into(),
			(Err(e), true) => ui::text(muted(e.clone())).key("v").into(),
			(_, false) => ui::text(muted("•".repeat(12))).key("v").into(),
		};
		let mut buttons = ui::row::<Msg>()
			.key("b")
			.gap(Gap::Xs)
			.child(ui::button(if shown { "Hide" } else { "Reveal" }).size("sm").variant("quiet").on_click(Msg::Reveal(Some(k.clone()))));
		if let Ok(text) = &decoded {
			buttons = buttons.child(ui::button("Copy").icon("copy").size("sm").variant("quiet").on_click(Msg::Copy(text.clone())));
		}
		col = col.child(
			ui::col::<Msg>()
				.key(format!("k{}", safe_key(k)))
				.gap(Gap::Xs)
				.child(ui::row::<Msg>().key("h").justify(Justify::Between).child(ui::text(ui::span(k.clone()).style(Token::Mono))).child(buttons))
				.child(value),
		);
		if (k == "tls.crt" || k.ends_with(".crt") || k.ends_with(".pem"))
			&& let Ok(pem) = &decoded
		{
			for (i, c) in decode::certs(pem).into_iter().enumerate() {
				let left = c.expires - now;
				let tone = if left < 0 {
					Token::Error
				} else if left < 30 * 86_400 {
					Token::Warning
				} else {
					Token::Success
				};
				let kv = ui::kv::<Msg>()
					.key(format!("cert{i}"))
					.item("Subject", c.subject)
					.item("Issuer", c.issuer)
					.item("Valid", format!("{} → {}", c.not_before, c.not_after))
					.item(
						"Expires",
						ui::span(if left < 0 { format!("expired {} ago", format::age(-left)) } else { format!("in {}", format::age(left)) })
							.style(tone),
					)
					.item("Names", c.sans.join(", "));
				col = col.child(kv);
			}
		}
	}
	ui::section()
		.key("secret")
		.head(format!("Data ({n})"))
		.child(
			ui::row::<Msg>().key("all").justify(Justify::End).child(
				ui::button(if all { "Hide all" } else { "Reveal all" }).size("sm").variant("quiet").on_click(Msg::Reveal(None)),
			),
		)
		.child(col)
		.into()
}

/// A ConfigMap's data: each key with its value highlighted by its file type.
fn configmap_data(obj: &DynamicObject) -> Node<Msg> {
	let data = obj.data.get("data").and_then(Value::as_object);
	let n = data.map_or(0, serde_json::Map::len);
	let mut col = ui::col::<Msg>().key("data").gap(Gap::Sm);
	for (k, v) in data.into_iter().flatten() {
		let text = v.as_str().unwrap_or_default().to_owned();
		let lang = k.rsplit_once('.').map_or("text", |(_, ext)| ext);
		col = col.child(
			ui::col::<Msg>()
				.key(format!("k{}", safe_key(k)))
				.gap(Gap::Xs)
				.child(
					ui::row::<Msg>()
						.key("h")
						.justify(Justify::Between)
						.child(ui::text(ui::span(k.clone()).style(Token::Mono)))
						.child(ui::button("Copy").icon("copy").size("sm").variant("quiet").on_click(Msg::Copy(text.clone()))),
				)
				.child(ui::code::<Msg>(text).key("v").lang(lang).numbers(true)),
		);
	}
	if let Some(bin) = obj.data.get("binaryData").and_then(Value::as_object) {
		for k in bin.keys() {
			col = col.child(ui::text(spans![ui::span(k.clone()).style(Token::Mono), muted("  binary")]).key(format!("b{}", safe_key(k))));
		}
	}
	ui::section().key("cm").head(format!("Data ({n})")).child(col).into()
}

/// A Helm release: chart, status, history (with rollback), values and notes.
fn helm_overview(app: &App, obj: &DynamicObject, now: i64) -> Node<Msg> {
	let release = obj.data.pointer("/data/release").and_then(Value::as_str).and_then(|r| decode::helm_release(r).ok());
	let Some(r) = release else { return ui::text(muted("This release record can't be read.")).into() };
	let kv = ui::kv::<Msg>()
		.key("meta")
		.item("Release", s(&r, &["name"]).to_owned())
		.item("Namespace", s(&r, &["namespace"]).to_owned())
		.item("Chart", format!("{} {}", s(&r, &["chart", "metadata", "name"]), s(&r, &["chart", "metadata", "version"])))
		.item("App version", s(&r, &["chart", "metadata", "appVersion"]).to_owned())
		.item("Status", s(&r, &["info", "status"]).to_owned())
		.item("Revision", scalar(r.get("version")))
		.item("Deployed", s(&r, &["info", "last_deployed"]).to_owned())
		.item("Description", s(&r, &["info", "description"]).to_owned());
	let mut col = ui::col::<Msg>().key("overview").gap(Gap::Md).child(kv);
	let history = app.helm_history();
	let current = scalar(r.get("version"));
	let mut t = ui::table::<Msg>()
		.key("history")
		.col(Column::new("revision", "Revision").align(TextAlign::End).mono(true).priority(100.0))
		.col(Column::new("status", "Status").priority(90.0))
		.col(Column::new("chart", "Chart").grow(1.0).priority(60.0))
		.col(Column::new("updated", "Updated").align(TextAlign::End).mono(true).priority(50.0))
		.on_action_with("rollback", |a| Msg::HelmRollback(a.item.clone().unwrap_or_default()));
	for (id, row) in history {
		let mut tr = TableRow::new(id);
		for c in ["revision", "status", "chart", "updated"] {
			tr = tr.cell(c, cell(row.get(c), now));
		}
		if row.get("revision").text(0) != current {
			tr = tr.menu("rollback");
		}
		t = t.row(tr);
	}
	col = col.child(ui::section().key("hist").head("History (right-click to roll back)").child(t));
	if let Some(values) = r.get("config").filter(|v| v.as_object().is_some_and(|m| !m.is_empty())) {
		let yaml = serde_yaml_ng::to_string(values).unwrap_or_default();
		col = col.child(ui::section().key("values").head("Values").child(ui::code::<Msg>(yaml).key("v").lang("yaml").numbers(true)));
	}
	let notes = s(&r, &["info", "notes"]);
	if !notes.is_empty() {
		col = col.child(
			ui::section()
				.key("notes")
				.head("Notes")
				.collapsible(true)
				.collapsed(true)
				.child(ui::code::<Msg>(notes.to_owned()).key("n").wrap(true)),
		);
	}
	col.into()
}

/// `120m CPU · 256Mi memory` of a pod, node or workload, when metrics are in.
fn usage_line(app: &App, ctx: &str, obj: &DynamicObject, projector: Projector) -> Option<String> {
	let _ = projector;
	let row = app.store(ctx)?.rows.get(&rows::key_of(obj))?;
	let (u, _) = app.usage(ctx, row)?;
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
	let editing = app.detail.as_ref().and_then(|d| d.edit.as_ref());
	if let Some(e) = editing {
		let mut bar = ui::row::<Msg>().key("editbar").gap(Gap::Sm).wrap(true);
		if e.review {
			bar = bar
				.child(ui::button("Back to editing").size("sm").variant("quiet").on_click(Msg::Review(false)))
				.child(ui::button("Apply").size("sm").variant("primary").busy(e.busy.is_some()).on_click(Msg::ApplyYaml));
		} else {
			bar = bar
				.child(ui::button("Review changes").size("sm").variant("primary").keys(["⌃S"]).on_click(Msg::Review(true)))
				.child(ui::button("Apply").size("sm").variant("quiet").busy(e.busy.is_some()).on_click(Msg::ApplyYaml));
		}
		bar = bar.child(ui::button("Discard").size("sm").variant("quiet").on_click(Msg::DiscardYaml));
		let changed = e.area.text != e.base;
		bar = bar.child(ui::text(muted(if changed { "edited" } else { "no changes" })).key("state"));
		let server_rv = obj.metadata.resource_version.clone().unwrap_or_default();
		let mut col = ui::col::<Msg>().key("edit").gap(Gap::Sm).child(bar);
		if server_rv != e.rv {
			col = col.child(
				ui::text(ui::span("The object changed on the server since you started; applying will fail. Discard and edit again.").style(Token::Warning))
					.key("moved"),
			);
		}
		if let Some(err) = &e.error {
			col = col.child(ui::text(ui::span(err.clone()).style(Token::Error)).key("err"));
		}
		col = if e.review {
			let diff = app::diff_text(&e.base, &e.area.text);
			col.child(if diff.is_empty() {
				ui::text(muted("No changes.")).key("diff").into()
			} else {
				Node::from(ui::diff::<Msg>().key("diff").text(diff).lang("yaml"))
			})
		} else {
			col.child(
				ui::editor::<Msg>()
					.key("area")
					.text(e.area.text.clone())
					.cursor(e.area.cursor16())
					.lang("yaml")
					.max_lines(60_u32)
					.on_focus(Msg::Focus(Focus::Editor))
					.on_edit(|ed| Msg::EditField(Focus::Editor, ed.from, ed.to, ed.text.clone(), ed.cursor)),
			)
		};
		return col.into();
	}
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
	let bar = ui::row::<Msg>()
		.key("yamlbar")
		.gap(Gap::Sm)
		.child(ui::button("Edit").icon("pencil").size("sm").variant("quiet").keys(["E"]).on_click(Msg::EditYaml))
		.child(ui::button("Copy").icon("copy").size("sm").variant("quiet").on_click(Msg::Copy(text.clone())));
	ui::col::<Msg>()
		.key("yamlview")
		.gap(Gap::Sm)
		.child(bar)
		.child(ui::code::<Msg>(text).key("yaml").lang("yaml").numbers(true))
		.into()
}

fn compare(app: &App) -> Node<Msg> {
	let Some(c) = app.detail.as_ref().and_then(|d| d.compare.as_ref()) else { return ui::text("").into() };
	let head = ui::text(spans![muted("Compared with "), ui::span(c.other.clone()).style(Token::Strong)]).key("h");
	let body: Node<Msg> = match &c.diff {
		None => ui::text(muted("Fetching…")).key("d").into(),
		Some(Err(e)) => ui::text(ui::span(e.clone()).style(Token::Error)).key("d").into(),
		Some(Ok(d)) if d.is_empty() => ui::text(ui::span("Identical (status and server fields ignored).").style(Token::Success)).key("d").into(),
		Some(Ok(d)) => ui::diff::<Msg>().key("d").text(d.clone()).lang("yaml").into(),
	};
	ui::col::<Msg>().key("compare").gap(Gap::Sm).child(head).child(body).into()
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
	let containers = if d.multi.is_empty() { app.containers() } else { vec![] };
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
	bar = bar.child(
		ui::input::<Msg>()
			.key("find")
			.text(l.filter.text.clone())
			.cursor(l.filter.cursor)
			.placeholder("Filter lines (words, a|b, !x, /re/)")
			.icon("search")
			.clear(true)
			.on_action("clear", Msg::EditField(Focus::LogFilter, 0, usize::MAX / 4, String::new(), 0))
			.on_focus(Msg::Focus(Focus::LogFilter))
			.on_edit(|e| Msg::EditField(Focus::LogFilter, e.from, e.to, e.text.clone(), e.cursor))
			.grow(1.0),
	);
	let pods = if l.pods > 1 { format!(" from {} pods", l.pods) } else { String::new() };
	let state = match (&l.error, l.live, l.paused.is_some()) {
		(Some(e), ..) => ui::span(e.clone()).style(Token::Error),
		(None, _, true) => ui::span(format!("{} lines{pods} · paused", l.lines)).style(Token::Warning),
		(None, true, false) => ui::span(format!("{} lines{pods} · following", l.lines)).style(Token::Muted),
		(None, false, false) => ui::span(format!("{} lines{pods} · ended", l.lines)).style(Token::Muted),
	};
	bar = bar
		.child(ui::button(if l.paused.is_some() { "Resume" } else { "Pause" }).size("sm").variant("quiet").on_click(Msg::Pause))
		.child(ui::button("Timestamps").size("sm").variant("quiet").on(l.timestamps).on_click(Msg::Timestamps))
		.child(ui::button("Previous").size("sm").variant("quiet").on(l.previous).on_click(Msg::Previous))
		.child(ui::button("Copy").icon("copy").size("sm").variant("quiet").on_click(Msg::CopyLog));
	if app.kind().is_pod() && d.multi.is_empty() {
		bar = bar.child(
			ui::button("Follow in terminal").icon("terminal").size("sm").variant("quiet").on_click(Msg::Act("follow".into(), None)),
		);
	}
	bar = bar.child(ui::text(state).key("state"));
	ui::col::<Msg>()
		.key("logs")
		.gap(Gap::Sm)
		.child(bar)
		.child(ui::ansi::<Msg>(l.shown().into_owned()).key("log").follow(true).max(Bound::h(Extent::Lines(60.0))))
		.into()
}

fn dialog(app: &App) -> Option<Node<Msg>> {
	let d = app.dialog.as_ref()?;
	let cancel = ui::button("Cancel").variant("quiet").keys(["esc"]).key("cancel").on_click(Msg::Cancel);
	let node: Node<Msg> = match d {
		Dialog::Confirm { targets, op, unlock } => {
			let verb = match op {
				Op::Change(m) => m.verb(),
				Op::Terminal(crate::actions::Action::Shell) => "Open a shell in".into(),
				Op::Terminal(crate::actions::Action::Debug) => "Debug".into(),
				Op::Terminal(crate::actions::Action::Edit) => "Edit".into(),
				Op::Terminal(crate::actions::Action::FollowLogs) => "Follow the log of".into(),
				Op::Terminal(crate::actions::Action::SignIn) => "Sign in to".into(),
				Op::Terminal(crate::actions::Action::HelmRollback(rev)) => format!("Roll back to revision {rev}:"),
				Op::Terminal(crate::actions::Action::HelmUninstall) => "Uninstall".into(),
			};
			let danger = matches!(op, Op::Change(m) if m.destructive())
				|| matches!(op, Op::Terminal(crate::actions::Action::HelmUninstall));
			let go_label = if *unlock { format!("Unlock for 15 min and {}", verb.to_lowercase()) } else { verb.clone() };
			let go = ui::button(go_label)
				.variant(if danger { "danger" } else { "primary" })
				.keys(["enter"])
				.key("go")
				.on_click(Msg::Confirm);
			let mut body = ui::col::<Msg>().key("body").gap(Gap::Sm);
			if let [t] = targets.as_slice() {
				body = body.child(ui::text(spans![
					format!("{verb} "),
					ui::span(t.label()).style(Token::Strong),
					" in context ",
					ui::span(app.config.display(&t.context).to_owned()).style(Token::Strong),
					t.obj.metadata.namespace.as_ref().map_or_else(String::new, |ns| format!(", namespace {ns}")),
					"?"
				]));
			} else {
				body = body.child(ui::text(format!("{verb} these {} objects?", targets.len())));
				let mut list = ui::col::<Msg>().key("list").gap(Gap::Xs);
				for (i, t) in targets.iter().take(20).enumerate() {
					list = list.child(
						ui::text(spans![ui::span(t.label()).style(Token::Mono), muted(format!("  {}", app.config.display(&t.context)))])
							.key(format!("t{i}")),
					);
				}
				body = body.child(list);
			}
			if *unlock {
				body = body.child(ui::text(ui::span("This context is protected.").style(Token::Warning)).key("warn"));
			}
			let head = match targets.as_slice() {
				[t] => format!("{verb} {}", t.label()),
				ts => format!("{verb} {} objects", ts.len()),
			};
			ui::overlay::<Msg>()
				.key("confirm")
				.modal(true)
				.size(OverlaySize::Sm)
				.head(head)
				.icon(if danger { "trash" } else { "alert-triangle" })
				.foot("confirm.foot")
				.child(body)
				.child(ui::row::<Msg>().key("foot").justify(Justify::End).gap(Gap::Sm).child(cancel).child(go))
				.into()
		},
		Dialog::Scale { targets, replicas } => {
			let head = match targets.as_slice() {
				[t] => format!("Scale {}", t.label()),
				ts => format!("Scale {} objects", ts.len()),
			};
			let contexts: Vec<&str> = targets.iter().map(|t| app.config.display(&t.context)).collect();
			ui::overlay::<Msg>()
				.key("scale")
				.modal(true)
				.size(OverlaySize::Sm)
				.head(head)
				.icon("layers")
				.foot("scale.foot")
				.child(
					ui::col::<Msg>().key("body").gap(Gap::Sm).child(ui::text(muted(format!("Context {}", contexts.join(", "))))).child(
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
				.into()
		},
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
				let mut detail: Vec<String> = app.config.tags(c).iter().map(|t| format!("#{t}")).collect();
				match app.protection(c) {
					Protection::None => {},
					Protection::Protected => detail.push("protected".into()),
					Protection::ReadOnly => detail.push("read-only".into()),
				}
				if app.config.display(c) != c {
					detail.push(c.clone());
				}
				list = list.child(
					ui::item(app.config.display(c).to_owned())
						.key(safe_key(c))
						.icon(if on { "check-square" } else { "square" })
						.detail(detail.join("  "))
						.on_select(move |_| Msg::ToggleContext(ctx.clone())),
				);
			}
			let mut tags: Vec<&String> = app.contexts.iter().flat_map(|c| app.config.tags(c)).collect();
			tags.sort();
			tags.dedup();
			let mut body = ui::col::<Msg>().key("body").gap(Gap::Sm).child(ui::text(muted("Every checked context is merged into one table.")));
			if !tags.is_empty() {
				let mut row = ui::row::<Msg>().key("tags").gap(Gap::Xs).wrap(true).child(ui::text(muted("Show a tag:")));
				for t in tags {
					row = row.child(ui::button(format!("#{t}")).size("sm").variant("quiet").key(safe_key(t)).on_click(Msg::ContextTag(t.clone())));
				}
				body = body.child(row);
			}
			ui::overlay::<Msg>()
				.key("contexts")
				.modal(true)
				.size(OverlaySize::Md)
				.head("Contexts")
				.meta(format!("{} shown", app.active.len()))
				.icon("layers")
				.foot("contexts.foot")
				.child(body.child(list))
				.child(
					ui::row::<Msg>()
						.key("foot")
						.justify(Justify::End)
						.child(ui::button("Done").variant("primary").keys(["esc"]).key("done").on_click(Msg::Cancel)),
				)
				.into()
		},
		Dialog::Palette { query, sel } => {
			let items = app.palette_items(&query.text);
			let mut list = ui::list::<Msg>().key("items").virtualize(items.len() > 200);
			if !items.is_empty() {
				list = list.selected(format!("layer.palette.body.items.i{}", (*sel).min(items.len() - 1)));
			}
			for (i, it) in items.iter().enumerate() {
				list = list.child(
					ui::item(it.label.clone()).key(format!("i{i}")).icon(it.icon).detail(it.detail.clone()).on_select(move |_| Msg::PaletteRun(i)),
				);
			}
			ui::overlay::<Msg>()
				.key("palette")
				.modal(true)
				.size(OverlaySize::Md)
				.head("Go to")
				.icon("command")
				.child(
					ui::col::<Msg>()
						.key("body")
						.gap(Gap::Sm)
						.child(
							ui::input::<Msg>()
								.key("q")
								.text(query.text.clone())
								.cursor(query.cursor)
								.placeholder("Kinds, objects and actions")
								.icon("search"),
						)
						.child(list.empty("No matches")),
				)
				.into()
		},
		Dialog::Compare { target, context, namespace } => {
			let mut s = ui::select::<Msg>()
				.key("ctx")
				.label("Context")
				.value(context.clone())
				.open(app.open == Some(Which::Context))
				.on_change(|c| Msg::CompareContext(c.value.as_str().unwrap_or_default().to_owned()))
				.on_toggle(|collapsed| Msg::SelectOpen(Which::Context, !collapsed));
			for c in &app.contexts {
				s = s.option(SelectOption::new(c.clone(), app.config.display(c).to_owned()));
			}
			ui::overlay::<Msg>()
				.key("cmp")
				.modal(true)
				.size(OverlaySize::Sm)
				.head(format!("Compare {}", target.label()))
				.icon("git-compare")
				.foot("cmp.foot")
				.child(
					ui::col::<Msg>()
						.key("body")
						.gap(Gap::Sm)
						.child(ui::text(muted("With the object of the same name in:")).key("h"))
						.child(s)
						.child(
							ui::input::<Msg>()
								.key("ns")
								.text(namespace.text.clone())
								.cursor(namespace.cursor)
								.prefix("namespace")
								.mono(true),
						),
				)
				.child(
					ui::row::<Msg>().key("foot").justify(Justify::End).gap(Gap::Sm).child(cancel).child(
						ui::button("Compare").variant("primary").keys(["enter"]).key("go").on_click(Msg::Confirm),
					),
				)
				.into()
		},
		Dialog::Columns => {
			let chosen = app.label_columns.get(&app.kind).cloned().unwrap_or_default();
			let mut list = ui::list::<Msg>().key("keys").empty("No labels on these objects");
			let mut keys = app.label_keys();
			for c in &chosen {
				if !keys.contains(c) {
					keys.insert(0, c.clone());
				}
			}
			for k in keys {
				let on = chosen.contains(&k);
				let key = k.clone();
				list = list.child(
					ui::item(k.clone())
						.key(safe_key(&k))
						.icon(if on { "check-square" } else { "square" })
						.on_select(move |_| Msg::LabelColumn(key.clone())),
				);
			}
			ui::overlay::<Msg>()
				.key("cols")
				.modal(true)
				.size(OverlaySize::Md)
				.head("Label columns")
				.meta(app.kind().label.clone())
				.icon("columns")
				.foot("cols.foot")
				.child(ui::col::<Msg>().key("body").gap(Gap::Sm).child(ui::text(muted("Checked labels show as columns."))).child(list))
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
	} else {
		match app.focus {
			Focus::Filter | Focus::LogFilter => "type to filter · ↵ done · esc clear".to_owned(),
			Focus::Editor => "editing YAML · ⌃S review · esc leave the editor".to_owned(),
			Focus::List => {
				let mut h = vec!["⌃K go to", "/ filter", "g jump", "⌥←→ history"];
				if !app.picked.is_empty() {
					h.insert(0, "space pick · esc clear picks");
				}
				if kind.has_logs() {
					h.push("l logs");
				}
				if kind.is_pod() {
					h.extend(["s shell", "D debug", "f forward"]);
				}
				if kind.scalable() {
					h.push("S scale");
				}
				if kind.restartable() {
					h.push("R restart");
				}
				h.extend(["e edit", "E edit here", "= compare", "⌦ delete", "C contexts"]);
				h.join(" · ")
			},
		}
	};
	let ctx_label = if app.merged() { format!("{} contexts", app.active.len()) } else { app.config.display(app.primary()).to_owned() };
	let mut status = ui::status::<Msg>().child(ui::seg(ctx_label).icon("cloud"));
	if let Some(p) = app.pending.values().next_back() {
		status = status.child(ui::seg(p.progress.clone().unwrap_or_else(|| format!("{}…", p.label))).icon("loader"));
	}
	if let Some(n) = &app.notice {
		status = status.child(ui::seg(n.text.clone()).icon(if n.error { "alert-triangle" } else { "check" }));
	} else if let Some((c, e)) = app.stores().find_map(|(c, s)| s.error.as_ref().map(|e| (c, e))) {
		status = status.child(ui::seg(format!("{}: {e}", app.config.display(c))).icon("alert-triangle"));
	}
	status.child(ui::seg(hint)).into()
}
