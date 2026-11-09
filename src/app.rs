//! UI state, the messages that change it, and key handling. Everything here
//! runs on the main thread; the backend only talks to it through `Update`s.
//!
//! Several contexts can be shown at once: each (context, kind) has its own
//! watch and store, and the table merges them. A row id is the context and
//! the object key joined by `SEP`, so every action knows its cluster.

use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	time::{Duration, Instant},
};

use kube::api::DynamicObject;
use serde_json::Value;
use tern_sdk::{Key, ui::SortDir};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	actions,
	backend::{Cmd, LogSource, Update, WatchEv, WatchKey},
	config::{Config, Protection},
	filter::Filter,
	kinds::{self, ColSpec, Kind, Projector},
	metrics, mutate, portforward,
	rows::{self, Row, Val},
	state::Saved,
};

/// Watches kept warm per shown context (Aptakube keeps its watchers too).
const WARM_PER_CONTEXT: usize = 6;
/// Log text kept before the oldest half is dropped.
const LOG_LIMIT: usize = 4 << 20;
/// How long an unlock of a protected context lasts.
const UNLOCK_FOR: Duration = Duration::from_secs(15 * 60);
/// Between the context and the object key in a row id.
pub const SEP: char = '\u{1f}';

/// The row id of `key` in `context`.
pub fn row_id(context: &str, key: &str) -> String {
	format!("{context}{SEP}{key}")
}

/// The context and object key of a row id.
pub fn split_id(id: &str) -> (&str, &str) {
	id.split_once(SEP).unwrap_or(("", id))
}

/// A program message: a UI event mapped by a node's handler.
#[derive(Clone, Debug)]
pub enum Msg {
	/// Show a kind.
	Kind(String),
	/// Make a context the primary one (and the only one shown).
	Context(String),
	/// Add or remove a context from the merged view.
	ToggleContext(String),
	/// Open the context picker.
	PickContexts,
	/// Filter by namespace (`""` for all).
	Namespace(String),
	/// A select opened or closed.
	SelectOpen(Which, bool),
	/// A row was clicked.
	Select(String),
	/// A row was double-clicked (or Enter): open its detail.
	Open(String),
	/// Close the detail pane.
	CloseDetail,
	/// Show a detail tab.
	Tab(Tab),
	/// Sort by a column.
	Sort(String, SortDir),
	/// The filter field asked for the keys.
	FocusFilter,
	/// Clear the filter.
	ClearFilter,
	/// Native edit of the filter text (UTF-16 range).
	EditFilter { from: usize, to: usize, text: String, cursor: usize },
	/// Run a row action (`act`) on a row (`None`: the open or selected one).
	Act(String, Option<String>),
	/// Show a container's log.
	Container(String),
	/// Toggle the previous container instance's log.
	Previous,
	/// Confirm the dialog.
	Confirm,
	/// Close the dialog.
	Cancel,
	/// Scale dialog: step the replicas.
	Step(i64),
	/// Port-forward dialog: pick a target port.
	ForwardPort(u16),
	/// Stop a port forward.
	StopForward(u64),
	/// Go to an object (owner or child link).
	Goto(Goto),
	/// History.
	Back,
	/// History.
	Forward,
}

/// An object to navigate to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Goto {
	/// Context.
	pub context: String,
	/// Kind id.
	pub kind:    String,
	/// Row key (`namespace/name` or `name`).
	pub key:     String,
}

/// The selects of the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
	/// The context select.
	Context,
	/// The namespace select.
	Namespace,
	/// The container select.
	Container,
}

/// A detail tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
	/// Summary.
	Overview,
	/// The object as YAML.
	Yaml,
	/// Its events.
	Events,
	/// Its pod's (or pods') log.
	Logs,
}

impl Tab {
	/// The tab id.
	pub const fn id(self) -> &'static str {
		match self {
			Self::Overview => "overview",
			Self::Yaml => "yaml",
			Self::Events => "events",
			Self::Logs => "logs",
		}
	}

	/// The tab of `id`.
	pub fn from_id(id: &str) -> Option<Self> {
		[Self::Overview, Self::Yaml, Self::Events, Self::Logs].into_iter().find(|t| t.id() == id)
	}
}

/// What a watch has delivered.
#[derive(Default)]
pub struct Store {
	/// Rows by key.
	pub rows:   HashMap<String, Row>,
	/// Objects by key.
	pub objs:   HashMap<String, DynamicObject>,
	/// The first list is complete.
	pub synced: bool,
	/// The last error, cleared by the next event.
	pub error:  Option<String>,
	/// Forbidden to list or watch.
	pub denied: bool,
	/// Keys seen during a relist.
	relisting:  Option<HashSet<String>>,
}

impl Store {
	/// Applies a watch event, projecting with `projector`.
	pub fn apply(&mut self, projector: Projector, ev: WatchEv) {
		match ev {
			WatchEv::Init => self.relisting = Some(HashSet::new()),
			WatchEv::InitApply(obj) => {
				let key = rows::key_of(&obj);
				if let Some(seen) = &mut self.relisting {
					seen.insert(key.clone());
				}
				self.put(projector, key, obj);
			},
			WatchEv::InitDone => {
				if let Some(seen) = self.relisting.take() {
					self.rows.retain(|k, _| seen.contains(k));
					self.objs.retain(|k, _| seen.contains(k));
				}
				self.synced = true;
				self.error = None;
				self.denied = false;
			},
			WatchEv::Apply(obj) => {
				let key = rows::key_of(&obj);
				self.put(projector, key, obj);
				self.error = None;
			},
			WatchEv::Delete(obj) => {
				let key = rows::key_of(&obj);
				self.rows.remove(&key);
				self.objs.remove(&key);
			},
			WatchEv::Error { message, forbidden } => {
				self.error = Some(message);
				self.denied = forbidden;
			},
		}
	}

	fn put(&mut self, projector: Projector, key: String, mut obj: DynamicObject) {
		// Nobody reads managed fields; they are often half of an object.
		obj.metadata.managed_fields = None;
		self.rows.insert(key.clone(), rows::project(projector, &obj));
		self.objs.insert(key, obj);
	}
}

/// The log of the open object.
#[derive(Default)]
pub struct Logs {
	/// Stream id; updates for other ids are stale.
	pub id:        u64,
	/// The container shown.
	pub container: Option<String>,
	/// The previous instance's log.
	pub previous:  bool,
	/// Pods streamed (one for a pod, several for a workload).
	pub pods:      usize,
	/// The text, appended to (so frames carry only the new tail).
	pub text:      String,
	/// Lines received.
	pub lines:     usize,
	/// Streaming.
	pub live:      bool,
	/// Why it ended, when it failed.
	pub error:     Option<String>,
}

/// The detail pane.
pub struct Detail {
	/// The row id.
	pub id:        String,
	/// The active tab.
	pub tab:       Tab,
	/// YAML of a resource version.
	pub yaml:      Option<(String, String)>,
	/// Event watch id.
	pub events_id: u64,
	/// The object's events.
	pub events:    Store,
	/// The log.
	pub logs:      Logs,
}

/// Which field has the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
	/// The table.
	List,
	/// The filter field.
	Filter,
}

/// The object an action acts on.
#[derive(Clone, Debug)]
pub struct Target {
	/// Context.
	pub context: String,
	/// Its kind.
	pub kind:    Kind,
	/// Row key.
	pub key:     String,
	/// The object.
	pub obj:     DynamicObject,
}

impl Target {
	/// `deployment/api`.
	pub fn label(&self) -> String {
		format!("{}/{}", self.kind.ar.kind.to_lowercase(), self.name())
	}

	/// The object's name.
	pub fn name(&self) -> String {
		self.obj.metadata.name.clone().unwrap_or_default()
	}
}

/// What a confirmation runs.
#[derive(Clone, Debug)]
pub enum Op {
	/// A change through the API.
	Change(mutate::Mutation),
	/// A terminal action (shell, edit) on a protected context.
	Terminal(actions::Action),
}

/// A modal dialog.
pub enum Dialog {
	/// Run `op` on `target` after a yes.
	Confirm {
		/// The object.
		target:   Target,
		/// What to do.
		op:       Op,
		/// The context is protected: confirming also unlocks it.
		unlock:   bool,
	},
	/// Pick a replica count.
	Scale {
		/// The object.
		target:   Target,
		/// Typed digits.
		replicas: String,
	},
	/// Pick ports for a forward.
	Forward {
		/// The pod or service.
		target: Target,
		/// Its ports: (port, label).
		ports:  Vec<(u16, String)>,
		/// The chosen port.
		port:   u16,
		/// Typed local port digits.
		local:  String,
	},
	/// Pick the contexts shown.
	Contexts,
}

/// A port forward the UI started.
pub struct Forward {
	/// Its spec.
	pub spec:   portforward::Spec,
	/// The bound local port, once listening.
	pub local:  Option<u16>,
	/// The pod connections go to, and how many are open.
	pub active: Option<(String, u32)>,
	/// The last failure.
	pub error:  Option<String>,
	/// The listener died.
	pub dead:   bool,
}

/// A place in history.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
	kind:   String,
	detail: Option<String>,
}

/// A transient message in the dock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
	/// The text.
	pub text:  String,
	/// An error (else information).
	pub error: bool,
}

/// The whole UI state.
pub struct App {
	/// Kubeconfig contexts.
	pub contexts:  Vec<String>,
	/// Shown contexts, the primary first (never empty).
	pub active:    Vec<String>,
	/// Namespace filter (`None`: all).
	pub namespace: Option<String>,
	/// Built-in kinds.
	builtins:      Vec<Kind>,
	/// Discovered custom kinds per context.
	pub custom:    HashMap<String, Vec<Kind>>,
	/// Discovery errors per context.
	pub discovery: HashMap<String, String>,
	/// The active kind id.
	pub kind:      String,
	/// Watch stores.
	pub stores:    HashMap<WatchKey, Store>,
	/// Most recently used watch first.
	lru:           VecDeque<WatchKey>,
	/// Filter text.
	pub query:     String,
	/// Caret in `query`, UTF-16.
	pub cursor:    usize,
	/// Compiled `query`.
	filter:        Filter,
	/// Keys' target.
	pub focus:     Focus,
	/// Sort column and direction.
	pub sort:      (String, SortDir),
	/// Selected row id.
	pub selected:  Option<String>,
	/// The open detail.
	pub detail:    Option<Detail>,
	/// Which select is open.
	pub open:      Option<Which>,
	/// The dialog shown.
	pub dialog:    Option<Dialog>,
	/// A transient message for the dock.
	pub notice:    Option<Notice>,
	/// Protection settings.
	config:        Config,
	/// Protected contexts unlocked until.
	unlocked:      HashMap<String, Instant>,
	/// Mutations in flight: id → what.
	pub pending:   BTreeMap<u64, String>,
	/// Metrics per context.
	pub metrics:   HashMap<String, Result<metrics::Snapshot, String>>,
	/// Contexts whose metrics are polled.
	metrics_on:    Vec<String>,
	/// Port forwards by id.
	pub forwards:  BTreeMap<u64, Forward>,
	history:       Vec<Place>,
	future:        Vec<Place>,
	/// `g` was pressed: the next key jumps to a kind.
	pub g_pending: bool,
	/// Open this row's detail once its kind's store has it.
	goto:          Option<String>,
	/// Visible row ids in order, rebuilt when stale.
	visible:       Vec<String>,
	/// `visible` needs a rebuild.
	stale:         bool,
	/// Id source for streams, mutations and forwards.
	next_id:       u64,
	cmd:           UnboundedSender<Cmd>,
	/// Exit requested.
	pub quit:      bool,
}

/// `g` + key jumps.
pub const JUMPS: [(&str, &str); 14] = [
	("p", "pods"),
	("d", "deployments.apps"),
	("t", "statefulsets.apps"),
	("a", "daemonsets.apps"),
	("r", "replicasets.apps"),
	("j", "jobs.batch"),
	("o", "cronjobs.batch"),
	("s", "services"),
	("i", "ingresses.networking.k8s.io"),
	("c", "configmaps"),
	("x", "secrets"),
	("v", "persistentvolumeclaims"),
	("n", "nodes"),
	("e", "events"),
];

impl App {
	/// A fresh app showing `active` (the primary first), restoring `saved`.
	pub fn new(
		contexts: Vec<String>,
		active: Vec<String>,
		saved: &Saved,
		config: Config,
		cmd: UnboundedSender<Cmd>,
	) -> Self {
		let builtins = kinds::builtins();
		let kind = saved.kind.clone().filter(|k| builtins.iter().any(|b| &b.id == k)).unwrap_or_else(|| "pods".into());
		let mut app = Self {
			contexts,
			active,
			namespace: saved.namespace.clone(),
			builtins,
			custom: HashMap::new(),
			discovery: HashMap::new(),
			kind,
			stores: HashMap::new(),
			lru: VecDeque::new(),
			query: String::new(),
			cursor: 0,
			filter: Filter::default(),
			focus: Focus::List,
			sort: ("name".into(), SortDir::Asc),
			selected: None,
			detail: None,
			open: None,
			dialog: None,
			notice: None,
			config,
			unlocked: HashMap::new(),
			pending: BTreeMap::new(),
			metrics: HashMap::new(),
			metrics_on: Vec::new(),
			forwards: BTreeMap::new(),
			history: Vec::new(),
			future: Vec::new(),
			g_pending: false,
			goto: None,
			visible: Vec::new(),
			stale: true,
			next_id: 0,
			cmd,
			quit: false,
		};
		app.activate_contexts();
		app
	}

	fn send(&self, cmd: Cmd) {
		// The backend lives as long as the runtime; a closed channel only
		// happens during shutdown.
		let _ = self.cmd.send(cmd);
	}

	fn id(&mut self) -> u64 {
		self.next_id += 1;
		self.next_id
	}

	fn info(&mut self, text: impl Into<String>) {
		self.notice = Some(Notice { text: text.into(), error: false });
	}

	fn error(&mut self, text: impl Into<String>) {
		self.notice = Some(Notice { text: text.into(), error: true });
	}

	/// What to save across runs.
	pub fn saved(&self) -> Saved {
		Saved {
			context:   Some(self.primary().to_owned()),
			contexts:  self.active.clone(),
			kind:      Some(self.kind.clone()),
			namespace: self.namespace.clone(),
		}
	}

	/// The primary context.
	pub fn primary(&self) -> &str {
		&self.active[0]
	}

	/// Whether several contexts are merged.
	pub fn merged(&self) -> bool {
		self.active.len() > 1
	}

	/// The protection of `context`.
	pub fn protection(&self, context: &str) -> Protection {
		self.config.protection(context)
	}

	fn is_unlocked(&self, context: &str) -> bool {
		self.unlocked.get(context).is_some_and(|until| Instant::now() < *until)
	}

	/// Every kind of the shown contexts, built-ins first, customs once each.
	pub fn kinds(&self) -> impl Iterator<Item = &Kind> {
		let mut seen = HashSet::new();
		let custom = self.active.iter().flat_map(|c| self.custom.get(c).into_iter().flatten());
		self.builtins.iter().chain(custom.filter(move |k| seen.insert(k.id.clone())))
	}

	/// The active kind.
	pub fn kind(&self) -> &Kind {
		self.kinds().find(|k| k.id == self.kind).unwrap_or(&self.builtins[0])
	}

	fn kind_by_id(&self, id: &str) -> Option<Kind> {
		self.kinds().find(|k| k.id == id).cloned()
	}

	fn watch_key(context: &str, kind: &str) -> WatchKey {
		WatchKey { context: context.to_owned(), kind: kind.to_owned(), namespace: None }
	}

	/// The store of the active kind in `context`.
	pub fn store(&self, context: &str) -> Option<&Store> {
		self.stores.get(&Self::watch_key(context, &self.kind))
	}

	/// The active kind's stores, by shown context.
	pub fn stores(&self) -> impl Iterator<Item = (&str, &Store)> {
		self.active.iter().filter_map(|c| self.store(c).map(|s| (c.as_str(), s)))
	}

	/// Every row of the active kind across shown contexts.
	pub fn total(&self) -> usize {
		self.stores().map(|(_, s)| s.rows.len()).sum()
	}

	/// The row and object of row id `id`.
	pub fn lookup(&self, id: &str) -> Option<(&Row, &DynamicObject)> {
		let (ctx, key) = split_id(id);
		let s = self.store(ctx)?;
		Some((s.rows.get(key)?, s.objs.get(key)?))
	}

	/// Namespace names across shown contexts.
	pub fn namespaces(&self) -> Vec<String> {
		let mut set: HashSet<String> = HashSet::new();
		for ctx in &self.active {
			match self.stores.get(&Self::watch_key(ctx, "namespaces")) {
				Some(s) if !s.rows.is_empty() => set.extend(s.rows.keys().cloned()),
				_ => {
					if let Some(s) = self.store(ctx) {
						set.extend(s.rows.values().filter_map(|r| r.namespace.clone()));
					}
				},
			}
		}
		let mut names: Vec<String> = set.into_iter().collect();
		names.sort();
		names
	}

	/// The columns shown for the active kind.
	pub fn columns(&self) -> Vec<ColSpec> {
		let kind = self.kind();
		let mut cols = kind.columns();
		if self.merged() {
			let at = usize::from(cols.first().is_some_and(|c| c.id == "name"));
			cols.insert(at, kinds::CONTEXT);
		}
		if kind.has_metrics() && self.active.iter().any(|c| matches!(self.metrics.get(c), Some(Ok(_)))) {
			let age = cols.iter().position(|c| c.id == "age").unwrap_or(cols.len());
			cols.insert(age, kinds::MEMORY);
			cols.insert(age, kinds::CPU);
		}
		cols
	}

	/// CPU and memory usage of a pod or node row in `context`.
	pub fn usage(&self, context: &str, row: &Row) -> Option<metrics::Usage> {
		let snap = self.metrics.get(context)?.as_ref().ok()?;
		match self.kind().projector {
			Projector::Pod => snap.pods.get(&row.key).copied(),
			Projector::Node => snap.nodes.get(&row.key).copied(),
			_ => None,
		}
	}

	/// The sort value of column `col` of `row` (metrics and context included).
	fn sort_val(&self, context: &str, row: &Row, col: &str) -> Val {
		match col {
			"context" => Val::Str(context.to_owned()),
			"cpu" => self.usage(context, row).map_or(Val::Empty, |u| Val::Num((u.cpu * 1000.0) as i64, String::new())),
			"memory" => self.usage(context, row).map_or(Val::Empty, |u| Val::Num(u.mem as i64, String::new())),
			_ => row.get(col).clone(),
		}
	}

	/// Starts (or refreshes in the LRU) the watch of `key`.
	fn watch(&mut self, key: WatchKey, kind: &Kind) {
		if let Some(pos) = self.lru.iter().position(|k| *k == key) {
			self.lru.remove(pos);
		} else {
			self.stores.entry(key.clone()).or_default();
			self.send(Cmd::Watch { key: key.clone(), ar: kind.ar.clone() });
		}
		self.lru.push_front(key);
		let cap = WARM_PER_CONTEXT.max(2) * self.active.len() + 2;
		while self.lru.len() > cap {
			let Some(pos) = self.lru.iter().rposition(|k| k.kind != "namespaces" || !self.active.contains(&k.context))
			else {
				break;
			};
			let Some(old) = self.lru.remove(pos) else { break };
			self.stores.remove(&old);
			self.send(Cmd::Unwatch(old));
		}
	}

	fn activate_kind(&mut self) {
		let kind = self.kind().clone();
		for ctx in self.active.clone() {
			self.watch(Self::watch_key(&ctx, &kind.id), &kind);
		}
		self.selected = None;
		self.close_detail();
		self.sort = (kind.projector.columns()[0].id.into(), SortDir::Asc);
		self.stale = true;
		self.sync_metrics();
	}

	fn activate_contexts(&mut self) {
		let ns = self.builtins.iter().find(|k| k.id == "namespaces").cloned();
		for ctx in self.active.clone() {
			if !self.custom.contains_key(&ctx) {
				self.send(Cmd::Discover(ctx.clone()));
			}
			if let Some(ns) = &ns {
				self.watch(Self::watch_key(&ctx, "namespaces"), ns);
			}
		}
		self.activate_kind();
	}

	/// Polls metrics of the shown contexts while pods or nodes are listed.
	fn sync_metrics(&mut self) {
		let want = if self.kind().has_metrics() { self.active.clone() } else { Vec::new() };
		if want != self.metrics_on {
			self.metrics_on = want.clone();
			self.send(Cmd::Metrics(want));
		}
	}

	fn close_detail(&mut self) {
		if self.detail.take().is_some() {
			self.send(Cmd::StopLogs);
			self.send(Cmd::StopEvents);
		}
	}

	/// The kind a watch key belongs to.
	fn projector_of(&self, key: &WatchKey) -> Projector {
		self.builtins
			.iter()
			.chain(self.custom.get(&key.context).into_iter().flatten())
			.find(|k| k.id == key.kind)
			.map_or(Projector::Generic, |k| k.projector)
	}

	/// Applies a backend update; true when the view changed.
	pub fn update_from(&mut self, u: Update) -> bool {
		match u {
			Update::Watch(key, ev) => {
				let projector = self.projector_of(&key);
				let Some(store) = self.stores.get_mut(&key) else { return false };
				store.apply(projector, ev);
				let shown = self.active.contains(&key.context);
				let active = shown && key.kind == self.kind;
				if active {
					self.stale = true;
					if let Some(id) = self.goto.clone()
						&& self.lookup(&id).is_some()
					{
						self.goto = None;
						self.open_detail(id, false);
					}
				}
				// Child pods of an open workload come from the pods store.
				active || (shown && (key.kind == "namespaces" || key.kind == "pods" && self.detail.is_some()))
			},
			Update::Discovered(ctx, kinds) => {
				self.discovery.remove(&ctx);
				self.custom.insert(ctx.clone(), kinds);
				self.active.contains(&ctx)
			},
			Update::DiscoveryFailed(ctx, err) => {
				self.discovery.insert(ctx.clone(), err);
				self.active.contains(&ctx)
			},
			Update::LogLines(id, lines) => {
				let Some(logs) = self.detail.as_mut().map(|d| &mut d.logs).filter(|l| l.id == id) else {
					return false;
				};
				for line in lines {
					logs.text.push_str(&line);
					logs.text.push('\n');
					logs.lines += 1;
				}
				if logs.text.len() > LOG_LIMIT {
					// Dropping the head resets the viewer once; it then appends again.
					let cut = logs.text.len() - LOG_LIMIT / 2;
					let cut = logs.text[cut..].find('\n').map_or(cut, |i| cut + i + 1);
					logs.text.drain(..cut);
				}
				self.detail.as_ref().is_some_and(|d| d.tab == Tab::Logs)
			},
			Update::LogEnded(id, err) => {
				let Some(logs) = self.detail.as_mut().map(|d| &mut d.logs).filter(|l| l.id == id) else {
					return false;
				};
				logs.live = false;
				logs.error = err;
				true
			},
			Update::Events(id, ev) => {
				let Some(d) = self.detail.as_mut().filter(|d| d.events_id == id) else { return false };
				d.events.apply(Projector::Event, ev);
				true
			},
			Update::Metrics(ctx, snap) => {
				self.metrics.insert(ctx.clone(), snap);
				if matches!(self.sort.0.as_str(), "cpu" | "memory") {
					self.stale = true;
				}
				self.active.contains(&ctx) && self.kind().has_metrics()
			},
			Update::Mutated(id, result) => {
				self.pending.remove(&id);
				match result {
					Ok(text) => self.info(text),
					Err(e) => self.error(e),
				}
				true
			},
			Update::Forward(id, report) => {
				let Some(f) = self.forwards.get_mut(&id) else { return false };
				match report {
					portforward::Report::Listening(port) => f.local = Some(port),
					portforward::Report::Active(pod, n) => {
						f.active = Some((pod, n));
						f.error = None;
					},
					portforward::Report::Failed(e) => f.error = Some(e),
					portforward::Report::Dead(e) => {
						f.error = Some(e);
						f.dead = true;
					},
				}
				true
			},
		}
	}

	/// The visible row ids in order (filtered, sorted).
	pub fn visible(&mut self) -> &[String] {
		if self.stale {
			self.rebuild();
		}
		&self.visible
	}

	fn rebuild(&mut self) {
		self.stale = false;
		let ns = self.namespace.as_deref();
		let col = self.sort.0.clone();
		let mut rows: Vec<(Val, &str, &Row)> = Vec::new();
		for (ctx, store) in self.stores() {
			for r in store.rows.values() {
				if ns.is_some() && r.namespace.is_some() && r.namespace.as_deref() != ns {
					continue;
				}
				if !self.filter.is_empty() && !self.filter.matches(&r.haystack) && !self.filter.matches(&ctx.to_lowercase())
				{
					continue;
				}
				rows.push((self.sort_val(ctx, r, &col), ctx, r));
			}
		}
		let desc = self.sort.1 == SortDir::Desc;
		rows.sort_by(|a, b| {
			let ord = a.0.cmp(&b.0).then_with(|| crate::format::natural_cmp(&a.2.key, &b.2.key)).then_with(|| a.1.cmp(b.1));
			if desc { ord.reverse() } else { ord }
		});
		self.visible = rows.into_iter().map(|(_, ctx, r)| row_id(ctx, &r.key)).collect();
	}

	/// The open object and its context.
	pub fn detail_obj(&self) -> Option<(&str, &DynamicObject)> {
		let d = self.detail.as_ref()?;
		let (ctx, _) = split_id(&d.id);
		Some((ctx, self.lookup(&d.id)?.1))
	}

	fn place(&self) -> Place {
		Place { kind: self.kind.clone(), detail: self.detail.as_ref().map(|d| d.id.clone()) }
	}

	fn record(&mut self) {
		let here = self.place();
		if self.history.last() != Some(&here) {
			self.history.push(here);
			if self.history.len() > 100 {
				self.history.remove(0);
			}
		}
		self.future.clear();
	}

	fn go(&mut self, to: Place) {
		if to.kind != self.kind {
			self.kind = to.kind;
			self.activate_kind();
		}
		match to.detail {
			Some(id) if self.lookup(&id).is_some() => self.open_detail(id, false),
			Some(id) => self.goto = Some(id),
			None => self.close_detail(),
		}
	}

	fn set_kind(&mut self, id: String) {
		if id != self.kind && self.kind_by_id(&id).is_some() {
			self.record();
			self.kind = id;
			self.activate_kind();
		}
	}

	fn open_detail(&mut self, id: String, record: bool) {
		let Some((_, obj)) = self.lookup(&id) else { return };
		let uid = obj.metadata.uid.clone().unwrap_or_default();
		let namespace = obj.metadata.namespace.clone();
		let (ctx, _) = split_id(&id);
		let ctx = ctx.to_owned();
		if record && self.detail.as_ref().is_none_or(|d| d.id != id) {
			self.record();
		}
		let has_logs = self.kind().has_logs();
		let keep_tab = self.detail.as_ref().map(|d| d.tab).filter(|t| *t != Tab::Logs || has_logs);
		self.close_detail();
		let events_id = self.id();
		self.send(Cmd::Events { id: events_id, context: ctx.clone(), namespace, uid });
		self.selected = Some(id.clone());
		self.detail = Some(Detail {
			id,
			tab: keep_tab.unwrap_or(Tab::Overview),
			yaml: None,
			events_id,
			events: Store::default(),
			logs: Logs::default(),
		});
		if self.kind().selects_pods() {
			// Child pods and aggregated logs read the pods store.
			if let Some(pods) = self.kind_by_id("pods") {
				self.watch(Self::watch_key(&ctx, "pods"), &pods);
			}
		}
		if keep_tab == Some(Tab::Logs) {
			self.start_logs();
		}
	}

	/// Container names of the open object's pods (a pod's own, or its
	/// template's), init containers last.
	pub fn containers(&self) -> Vec<String> {
		let Some((_, obj)) = self.detail_obj() else { return vec![] };
		let spec = if self.kind().is_pod() {
			obj.data.get("spec")
		} else {
			obj.data.pointer("/spec/template/spec").or_else(|| obj.data.pointer("/spec/jobTemplate/spec/template/spec"))
		};
		spec.map_or_else(Vec::new, container_names)
	}

	/// Pods of the open object: itself, its selected pods, or a node's pods.
	pub fn child_pods(&self) -> Vec<(String, &Row)> {
		let Some((ctx, obj)) = self.detail_obj() else { return vec![] };
		let kind = self.kind();
		let Some(pods) = self.stores.get(&Self::watch_key(ctx, "pods")) else { return vec![] };
		let ns = obj.metadata.namespace.as_deref();
		let matches: Box<dyn Fn(&DynamicObject) -> bool> = match kind.projector {
			Projector::Node => {
				let name = obj.metadata.name.clone().unwrap_or_default();
				Box::new(move |p| p.data.pointer("/spec/nodeName").and_then(Value::as_str) == Some(name.as_str()))
			},
			_ if kind.selects_pods() => {
				let sel = if kind.projector == Projector::Service {
					obj.data.pointer("/spec/selector")
				} else {
					obj.data.pointer("/spec/selector/matchLabels")
				};
				let sel: Vec<(String, String)> = sel
					.and_then(Value::as_object)
					.into_iter()
					.flatten()
					.filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
					.collect();
				if sel.is_empty() {
					return vec![];
				}
				let ns = ns.map(str::to_owned);
				Box::new(move |p| {
					p.metadata.namespace == ns
						&& sel.iter().all(|(k, v)| p.metadata.labels.as_ref().and_then(|l| l.get(k)) == Some(v))
				})
			},
			_ => return vec![],
		};
		let mut out: Vec<(String, &Row)> = pods
			.objs
			.iter()
			.filter(|(_, p)| matches(p))
			.filter_map(|(k, _)| pods.rows.get(k).map(|r| (row_id(ctx, k), r)))
			.collect();
		out.sort_by(|a, b| crate::format::natural_cmp(&a.1.key, &b.1.key));
		out
	}

	fn start_logs(&mut self) {
		let Some((ctx, obj)) = self.detail_obj() else { return };
		let ctx = ctx.to_owned();
		let namespace = obj.metadata.namespace.clone().unwrap_or_default();
		let pod_name = obj.metadata.name.clone().unwrap_or_default();
		let chosen = self.detail.as_ref().and_then(|d| d.logs.container.clone());
		let container = chosen.or_else(|| self.containers().into_iter().next());
		let sources: Vec<LogSource> = if self.kind().is_pod() {
			vec![LogSource { pod: pod_name, container: container.clone() }]
		} else {
			self.child_pods()
				.into_iter()
				.map(|(_, r)| LogSource { pod: r.key.rsplit('/').next().unwrap_or_default().to_owned(), container: container.clone() })
				.collect()
		};
		let id = self.id();
		let Some(d) = self.detail.as_mut() else { return };
		let previous = d.logs.previous;
		if sources.is_empty() {
			d.logs = Logs { id, container, previous, error: Some("No pods (yet)".into()), ..Logs::default() };
			return;
		}
		d.logs = Logs { id, container, previous, pods: sources.len(), live: true, ..Logs::default() };
		self.send(Cmd::Logs { id, context: ctx, namespace, sources, previous });
	}

	fn set_tab(&mut self, tab: Tab) {
		let Some(d) = self.detail.as_mut() else { return };
		if d.tab == tab {
			return;
		}
		d.tab = tab;
		if tab == Tab::Logs && d.logs.id == 0 {
			self.start_logs();
		}
	}

	/// The detail tabs of the active kind.
	pub fn tabs(&self) -> Vec<Tab> {
		let mut t = vec![Tab::Overview, Tab::Yaml, Tab::Events];
		if self.kind().has_logs() {
			t.push(Tab::Logs);
		}
		t
	}

	fn set_query(&mut self, query: String, cursor: usize) {
		self.query = query;
		self.cursor = cursor.min(utf16_len(&self.query));
		self.filter = Filter::parse(&self.query);
		self.stale = true;
	}

	/// The target of an action: row `id`, else the open object, else the
	/// selected row.
	fn target(&self, id: Option<&str>) -> Option<Target> {
		let id = id.or_else(|| self.detail.as_ref().map(|d| d.id.as_str())).or(self.selected.as_deref())?;
		let (_, obj) = self.lookup(id)?;
		let (ctx, key) = split_id(id);
		Some(Target { context: ctx.to_owned(), kind: self.kind().clone(), key: key.to_owned(), obj: obj.clone() })
	}

	/// The row actions offered for `target`, as `(act, label)`.
	pub fn acts(&self, kind: &Kind, obj: &DynamicObject) -> Vec<(&'static str, &'static str)> {
		let mut v = Vec::new();
		if kind.is_pod() {
			v.extend([("shell", "Shell"), ("logs", "Logs")]);
		}
		if kind.is_pod() || kind.projector == Projector::Service {
			v.push(("forward", "Port forward…"));
		}
		if kind.selects_pods() && !kind.is_pod() {
			v.push(("logs", "Logs"));
		}
		if kind.scalable() {
			v.push(("scale", "Scale…"));
		}
		if kind.restartable() {
			v.push(("restart", "Restart"));
		}
		let flag = |p: &str| obj.data.pointer(p).and_then(Value::as_bool).unwrap_or(false);
		if kind.projector == Projector::CronJob {
			v.push(("trigger", "Trigger now"));
			v.push(if flag("/spec/suspend") { ("resume", "Resume") } else { ("suspend", "Suspend") });
		}
		if kind.projector == Projector::Node {
			v.push(if flag("/spec/unschedulable") { ("uncordon", "Uncordon") } else { ("cordon", "Cordon") });
		}
		v.push(("edit", "Edit YAML"));
		v.push(("delete", "Delete"));
		v
	}

	/// Starts action `act` on row `id` (or the open/selected row).
	fn act(&mut self, act: &str, id: Option<String>) {
		let Some(target) = self.target(id.as_deref()) else {
			self.error("Select a row first");
			return;
		};
		let protection = self.protection(&target.context);
		let changes = !matches!(act, "logs" | "forward" | "follow");
		if changes && protection == Protection::ReadOnly {
			self.error(format!("{} is read-only", target.context));
			return;
		}
		let locked = changes && protection == Protection::Protected && !self.is_unlocked(&target.context);
		let confirm = |op| Some(Dialog::Confirm { target: target.clone(), op, unlock: locked });
		self.dialog = match act {
			"shell" | "edit" => {
				let action = if act == "shell" { actions::Action::Shell } else { actions::Action::Edit };
				if locked {
					confirm(Op::Terminal(action))
				} else {
					self.terminal(action, &target);
					None
				}
			},
			"follow" => {
				self.terminal(actions::Action::FollowLogs, &target);
				None
			},
			"logs" => {
				let id = row_id(&target.context, &target.key);
				if self.detail.as_ref().is_none_or(|d| d.id != id) {
					self.open_detail(id, true);
				}
				self.set_tab(Tab::Logs);
				None
			},
			"forward" => self.forward_dialog(target),
			"scale" => {
				let n = target.obj.data.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
				Some(Dialog::Scale { target, replicas: n.to_string() })
			},
			"restart" => confirm(Op::Change(mutate::Mutation::Restart)),
			"delete" => confirm(Op::Change(mutate::Mutation::Delete)),
			"trigger" => confirm(Op::Change(mutate::Mutation::Trigger)),
			"suspend" | "resume" => confirm(Op::Change(mutate::Mutation::Suspend(act == "suspend"))),
			"cordon" | "uncordon" => confirm(Op::Change(mutate::Mutation::Cordon(act == "cordon"))),
			_ => None,
		};
	}

	fn terminal(&mut self, action: actions::Action, t: &Target) {
		let container = self.detail.as_ref().and_then(|d| d.logs.container.clone());
		let target = actions::Target {
			context:   t.context.clone(),
			resource:  t.kind.id.clone(),
			namespace: t.obj.metadata.namespace.clone(),
			name:      t.name(),
			container: container.or_else(|| t.obj.data.get("spec").map(container_names).and_then(|c| c.into_iter().next())),
			is_pod:    t.kind.is_pod(),
		};
		if let Err(e) = actions::launch(action, &target) {
			self.error(e);
		}
	}

	fn forward_dialog(&mut self, target: Target) -> Option<Dialog> {
		let ports: Vec<(u16, String)> = if target.kind.is_pod() {
			target
				.obj
				.data
				.pointer("/spec/containers")
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.flat_map(|c| c.get("ports").and_then(Value::as_array).cloned().unwrap_or_default())
				.filter_map(|p| {
					let n = u16::try_from(p.get("containerPort")?.as_u64()?).ok()?;
					let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
					Some((n, if name.is_empty() { n.to_string() } else { format!("{n} ({name})") }))
				})
				.collect()
		} else {
			target
				.obj
				.data
				.pointer("/spec/ports")
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.filter_map(|p| {
					let n = u16::try_from(p.get("port")?.as_u64()?).ok()?;
					let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
					Some((n, if name.is_empty() { n.to_string() } else { format!("{n} ({name})") }))
				})
				.collect()
		};
		let Some(&(port, _)) = ports.first() else {
			self.error(format!("{} declares no ports", target.label()));
			return None;
		};
		Some(Dialog::Forward { target, ports, port, local: port.to_string() })
	}

	fn confirm(&mut self) {
		let Some(dialog) = self.dialog.take() else { return };
		match dialog {
			Dialog::Confirm { target, op, unlock } => {
				if unlock {
					self.unlocked.insert(target.context.clone(), Instant::now() + UNLOCK_FOR);
				}
				match op {
					Op::Terminal(action) => self.terminal(action, &target),
					Op::Change(change) => self.mutate(&target, change),
				}
			},
			Dialog::Scale { target, replicas } => match replicas.parse::<i64>() {
				Ok(n) if n >= 0 => {
					let protection = self.protection(&target.context);
					if protection == Protection::Protected && !self.is_unlocked(&target.context) {
						self.dialog = Some(Dialog::Confirm { target, op: Op::Change(mutate::Mutation::Scale(n)), unlock: true });
					} else {
						self.mutate(&target, mutate::Mutation::Scale(n));
					}
				},
				_ => {
					self.error("Replicas must be a whole number");
					self.dialog = Some(Dialog::Scale { target, replicas });
				},
			},
			Dialog::Forward { target, port, local, .. } => {
				let Ok(local) = local.parse::<u16>() else {
					self.error("Local port must be 0–65535");
					return;
				};
				let id = self.id();
				let spec = portforward::Spec {
					id,
					context: target.context.clone(),
					namespace: target.obj.metadata.namespace.clone().unwrap_or_default(),
					target: if target.kind.is_pod() {
						portforward::Target::Pod(target.name())
					} else {
						portforward::Target::Service(target.name())
					},
					port,
					local,
				};
				self.forwards.insert(id, Forward { spec: spec.clone(), local: None, active: None, error: None, dead: false });
				self.send(Cmd::Forward(spec));
			},
			Dialog::Contexts => {},
		}
	}

	fn mutate(&mut self, target: &Target, change: mutate::Mutation) {
		let id = self.id();
		self.pending.insert(id, format!("{} {}", change.verb(), target.label()));
		if change == mutate::Mutation::Delete
			&& self.detail.as_ref().is_some_and(|d| d.id == row_id(&target.context, &target.key))
		{
			self.close_detail();
		}
		self.send(Cmd::Mutate {
			id,
			context: target.context.clone(),
			obj: mutate::Object {
				ar:        target.kind.ar.clone(),
				namespace: target.obj.metadata.namespace.clone(),
				name:      target.name(),
			},
			change,
		});
	}

	fn goto(&mut self, g: Goto) {
		if !self.active.contains(&g.context) {
			return;
		}
		let id = row_id(&g.context, &g.key);
		self.record();
		if g.kind != self.kind {
			self.kind = g.kind;
			self.activate_kind();
		}
		if self.lookup(&id).is_some() {
			self.open_detail(id, false);
		} else {
			self.goto = Some(id);
		}
	}

	/// The kind id of an owner reference (`apps/v1`, `ReplicaSet`).
	pub fn kind_of_owner(&self, api_version: &str, kind: &str) -> Option<String> {
		let group = api_version.rsplit_once('/').map_or("", |(g, _)| g);
		self.kinds().find(|k| k.ar.kind == kind && k.ar.group == group).map(|k| k.id.clone())
	}

	fn toggle_context(&mut self, ctx: String) {
		if let Some(pos) = self.active.iter().position(|c| *c == ctx) {
			if self.active.len() > 1 {
				self.active.remove(pos);
				if self.selected.as_deref().is_some_and(|s| split_id(s).0 == ctx) {
					self.selected = None;
				}
				if self.detail.as_ref().is_some_and(|d| split_id(&d.id).0 == ctx) {
					self.close_detail();
				}
				self.stale = true;
				self.sync_metrics();
			}
		} else if self.contexts.contains(&ctx) {
			self.active.push(ctx);
			self.activate_contexts();
		}
	}

	/// Handles a message.
	pub fn update(&mut self, msg: Msg) {
		if !matches!(msg, Msg::SelectOpen(..)) {
			self.notice = None;
		}
		match msg {
			Msg::Kind(id) => self.set_kind(id),
			Msg::Context(ctx) => {
				self.open = None;
				if self.contexts.contains(&ctx) && (self.active.len() > 1 || ctx != self.active[0]) {
					self.active = vec![ctx];
					if self.kind_by_id(&self.kind).is_none() {
						self.kind = "pods".into();
					}
					self.namespace = None;
					self.activate_contexts();
				}
			},
			Msg::ToggleContext(ctx) => self.toggle_context(ctx),
			Msg::PickContexts => {
				self.open = None;
				self.dialog = Some(Dialog::Contexts);
			},
			Msg::Namespace(ns) => {
				self.open = None;
				self.namespace = (!ns.is_empty()).then_some(ns);
				self.stale = true;
			},
			Msg::SelectOpen(which, open) => {
				self.open = if open { Some(which) } else { self.open.filter(|w| *w != which) };
			},
			Msg::Select(id) => {
				self.focus = Focus::List;
				if self.detail.is_some() {
					self.open_detail(id, true);
				} else {
					self.selected = Some(id);
				}
			},
			Msg::Open(id) => {
				self.focus = Focus::List;
				self.open_detail(id, true);
			},
			Msg::CloseDetail => {
				self.record();
				self.close_detail();
			},
			Msg::Tab(tab) => self.set_tab(tab),
			Msg::Sort(col, dir) => {
				self.sort = (col, dir);
				self.stale = true;
			},
			Msg::FocusFilter => self.focus = Focus::Filter,
			Msg::ClearFilter => self.set_query(String::new(), 0),
			Msg::EditFilter { from, to, text, cursor } => {
				let q = splice_utf16(&self.query, from, to, &text);
				self.set_query(q, cursor);
				self.focus = Focus::Filter;
			},
			Msg::Act(act, id) => self.act(&act, id),
			Msg::Container(c) => {
				self.open = None;
				if let Some(d) = self.detail.as_mut() {
					d.logs.container = Some(c);
				}
				self.start_logs();
			},
			Msg::Previous => {
				if let Some(d) = self.detail.as_mut() {
					d.logs.previous = !d.logs.previous;
				}
				self.start_logs();
			},
			Msg::Confirm => self.confirm(),
			Msg::Cancel => self.dialog = None,
			Msg::Step(by) => {
				if let Some(Dialog::Scale { replicas, .. }) = &mut self.dialog {
					let n = replicas.parse::<i64>().unwrap_or(0).saturating_add(by).max(0);
					*replicas = n.to_string();
				}
			},
			Msg::ForwardPort(p) => {
				if let Some(Dialog::Forward { port, local, .. }) = &mut self.dialog {
					*port = p;
					*local = p.to_string();
				}
			},
			Msg::StopForward(id) => {
				if self.forwards.remove(&id).is_some() {
					self.send(Cmd::StopForward(id));
				}
			},
			Msg::Goto(g) => self.goto(g),
			Msg::Back => {
				if let Some(to) = self.history.pop() {
					let here = self.place();
					self.future.push(here);
					self.go(to);
				}
			},
			Msg::Forward => {
				if let Some(to) = self.future.pop() {
					let here = self.place();
					self.history.push(here);
					self.go(to);
				}
			},
		}
	}

	/// Whether history has somewhere to go.
	pub fn can_go(&self) -> (bool, bool) {
		(!self.history.is_empty(), !self.future.is_empty())
	}

	fn move_selection(&mut self, delta: isize) {
		let current = self.selected.clone();
		let visible = self.visible();
		if visible.is_empty() {
			return;
		}
		let pos = current.and_then(|k| visible.iter().position(|v| *v == k));
		let next = match pos {
			None => {
				if delta >= 0 {
					0
				} else {
					visible.len() - 1
				}
			},
			Some(p) => p.saturating_add_signed(delta).min(visible.len() - 1),
		};
		let id = visible[next].clone();
		self.update(Msg::Select(id));
	}

	/// Handles a key; false when it did nothing.
	pub fn key(&mut self, key: &Key) -> bool {
		if key.is("ctrl+c") {
			self.quit = true;
			return true;
		}
		if self.dialog.is_some() {
			return self.dialog_key(key);
		}
		if self.focus == Focus::Filter {
			return self.filter_key(key);
		}
		if std::mem::take(&mut self.g_pending) {
			if let Some((_, id)) = JUMPS.iter().find(|(k, _)| key.name == *k && !key.shift) {
				self.set_kind((*id).to_owned());
			}
			return true;
		}
		let tabs = self.tabs();
		let tab_step = |app: &mut Self, step: isize| {
			if let Some(d) = &app.detail {
				let i = tabs.iter().position(|t| *t == d.tab).unwrap_or(0) as isize;
				let n = tabs.len() as isize;
				app.set_tab(tabs[((i + step).rem_euclid(n)) as usize]);
			}
		};
		let shift = key.shift;
		match key.name.as_str() {
			"left" if key.alt => self.update(Msg::Back),
			"right" if key.alt => self.update(Msg::Forward),
			"down" | "j" if !key.ctrl => self.move_selection(1),
			"up" | "k" if !key.ctrl => self.move_selection(-1),
			"page_down" => self.move_selection(20),
			"page_up" => self.move_selection(-20),
			"home" => self.move_selection(isize::MIN / 2),
			"end" => self.move_selection(isize::MAX / 2),
			"enter" => {
				if let Some(k) = self.selected.clone() {
					self.open_detail(k, true);
				}
			},
			"escape" => {
				if self.open.is_some() {
					self.open = None;
				} else if self.detail.is_some() {
					self.update(Msg::CloseDetail);
				} else if !self.query.is_empty() {
					self.set_query(String::new(), 0);
				} else {
					return false;
				}
			},
			"/" => self.focus = Focus::Filter,
			"g" if !shift => self.g_pending = true,
			"tab" if shift => tab_step(self, -1),
			"tab" => tab_step(self, 1),
			"1" => self.set_tab(Tab::Overview),
			"2" | "y" => self.set_tab(Tab::Yaml),
			"3" => self.set_tab(Tab::Events),
			"4" | "l" if self.kind().has_logs() => self.act("logs", None),
			"s" if shift && self.kind().scalable() => self.act("scale", None),
			"r" if shift && self.kind().restartable() => self.act("restart", None),
			"s" if self.kind().is_pod() => self.act("shell", None),
			"f" if self.kind().is_pod() || self.kind().projector == Projector::Service => self.act("forward", None),
			"t" if self.kind().projector == Projector::CronJob => self.act("trigger", None),
			"e" => self.act("edit", None),
			"c" if shift => self.update(Msg::PickContexts),
			"delete" | "backspace" if key.ctrl || key.meta || key.name == "delete" => self.act("delete", None),
			"[" | "]" => {
				let ids: Vec<String> = self.kinds().map(|k| k.id.clone()).collect();
				let i = ids.iter().position(|k| *k == self.kind).unwrap_or(0) as isize;
				let step = if key.name == "]" { 1 } else { -1 };
				let id = ids[(i + step).rem_euclid(ids.len() as isize) as usize].clone();
				self.set_kind(id);
			},
			"q" => self.quit = true,
			_ => return false,
		}
		true
	}

	fn dialog_key(&mut self, key: &Key) -> bool {
		match key.name.as_str() {
			"escape" => self.dialog = None,
			"enter" => {
				if !matches!(self.dialog, Some(Dialog::Contexts)) {
					self.confirm();
				} else {
					self.dialog = None;
				}
			},
			"up" if matches!(self.dialog, Some(Dialog::Scale { .. })) => self.update(Msg::Step(1)),
			"down" if matches!(self.dialog, Some(Dialog::Scale { .. })) => self.update(Msg::Step(-1)),
			"up" | "down" => {
				if let Some(Dialog::Forward { ports, port, local, .. }) = &mut self.dialog {
					let i = ports.iter().position(|(p, _)| p == port).unwrap_or(0) as isize;
					let step = if key.name == "down" { 1 } else { -1 };
					let (p, _) = ports[(i + step).rem_euclid(ports.len() as isize) as usize];
					*port = p;
					*local = p.to_string();
				}
			},
			"backspace" => match &mut self.dialog {
				Some(Dialog::Scale { replicas: s, .. } | Dialog::Forward { local: s, .. }) => {
					s.pop();
				},
				_ => return false,
			},
			_ => {
				let Some(text) = key.typed() else { return false };
				match &mut self.dialog {
					Some(Dialog::Scale { replicas: s, .. } | Dialog::Forward { local: s, .. })
						if text.chars().all(|c| c.is_ascii_digit()) && s.len() < 5 =>
					{
						s.push_str(text);
					},
					_ => return false,
				}
			},
		}
		true
	}

	fn filter_key(&mut self, key: &Key) -> bool {
		let len = utf16_len(&self.query);
		match key.name.as_str() {
			"escape" | "enter" | "down" | "tab" => {
				self.focus = Focus::List;
				if key.name == "escape" && !self.query.is_empty() {
					self.set_query(String::new(), 0);
				}
				if key.name == "down" {
					self.move_selection(1);
				}
			},
			"backspace" if self.cursor > 0 => {
				let q = splice_utf16(&self.query, self.cursor - 1, self.cursor, "");
				self.set_query(q, self.cursor - 1);
			},
			"delete" if self.cursor < len => {
				let q = splice_utf16(&self.query, self.cursor, self.cursor + 1, "");
				self.set_query(q, self.cursor);
			},
			"left" => self.cursor = self.cursor.saturating_sub(1),
			"right" => self.cursor = (self.cursor + 1).min(len),
			"home" => self.cursor = 0,
			"end" => self.cursor = len,
			"u" if key.ctrl => self.set_query(String::new(), 0),
			"paste" => {
				let text = key.text.clone().unwrap_or_default().replace(['\n', '\r'], " ");
				let q = splice_utf16(&self.query, self.cursor, self.cursor, &text);
				let c = self.cursor + utf16_len(&text);
				self.set_query(q, c);
			},
			_ => {
				let Some(text) = key.typed().map(str::to_owned) else { return false };
				let q = splice_utf16(&self.query, self.cursor, self.cursor, &text);
				let c = self.cursor + utf16_len(&text);
				self.set_query(q, c);
			},
		}
		true
	}

	/// The node id of the filter field (it must match the view's keys).
	pub const FILTER_ID: &'static str = "main.layout.body.bar.filter";
}

/// Container names of a pod spec, init containers last.
fn container_names(spec: &Value) -> Vec<String> {
	["containers", "initContainers"]
		.into_iter()
		.flat_map(|k| spec.get(k).and_then(Value::as_array).into_iter().flatten())
		.filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_owned))
		.collect()
}

fn utf16_len(s: &str) -> usize {
	s.encode_utf16().count()
}

/// The byte offset of UTF-16 offset `u` in `s`.
fn byte_at(s: &str, u: usize) -> usize {
	let mut n = 0;
	for (i, c) in s.char_indices() {
		if n >= u {
			return i;
		}
		n += c.len_utf16();
	}
	s.len()
}

/// `s` with UTF-16 range `[from, to)` replaced by `text`.
fn splice_utf16(s: &str, from: usize, to: usize, text: &str) -> String {
	let (a, b) = (byte_at(s, from.min(to)), byte_at(s, to.max(from)));
	let mut out = String::with_capacity(s.len() + text.len());
	out.push_str(&s[..a]);
	out.push_str(text);
	out.push_str(&s[b..]);
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn utf16_splices_respect_wide_characters() {
		// UTF-16 units: a=1, é=1, 😀=2, b=1.
		assert_eq!(splice_utf16("aé😀b", 2, 4, "x"), "aéxb");
		assert_eq!(splice_utf16("aé😀b", 5, 5, "!"), "aé😀b!");
		assert_eq!(splice_utf16("aé😀b", 0, 1, ""), "é😀b");
		assert_eq!(utf16_len("aé😀b"), 5);
	}

	#[test]
	fn row_ids_keep_contexts_with_slashes_apart_from_keys() {
		let id = row_id("ocp1-on-aws/me@x.com", "prod/api-1");
		assert_eq!(split_id(&id), ("ocp1-on-aws/me@x.com", "prod/api-1"));
	}

	fn app() -> (App, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let ctxs = vec!["a".to_owned(), "b".to_owned()];
		(App::new(ctxs.clone(), ctxs, &Saved::default(), Config::default(), tx), rx)
	}

	fn pod(ns: &str, name: &str, labels: Value) -> DynamicObject {
		serde_json::from_value(serde_json::json!({
			"apiVersion": "v1", "kind": "Pod",
			"metadata": {"name": name, "namespace": ns, "uid": name, "labels": labels},
			"spec": {"containers": [{"name": "app"}]}, "status": {"phase": "Running"}
		}))
		.unwrap()
	}

	fn feed(app: &mut App, ctx: &str, kind: &str, obj: DynamicObject) {
		app.update_from(Update::Watch(App::watch_key(ctx, kind), WatchEv::Apply(obj)));
	}

	#[test]
	fn merged_contexts_list_rows_from_each_with_a_context_column() {
		let (mut app, _rx) = app();
		feed(&mut app, "a", "pods", pod("default", "api", serde_json::json!({})));
		feed(&mut app, "b", "pods", pod("default", "api", serde_json::json!({})));
		let visible = app.visible().to_vec();
		assert_eq!(visible, [row_id("a", "default/api"), row_id("b", "default/api")]);
		assert_eq!(app.columns()[1].id, "context");
		app.update(Msg::Sort("context".into(), SortDir::Desc));
		assert_eq!(app.visible()[0], row_id("b", "default/api"));
	}

	#[test]
	fn protected_contexts_ask_and_read_only_contexts_refuse() {
		let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
		let ctxs = vec!["eu-prod".to_owned(), "audit".to_owned()];
		let config = Config { protected: vec!["*prod*".into()], readonly: vec!["audit".into()], unprotected: vec![] };
		let mut app = App::new(ctxs.clone(), ctxs, &Saved::default(), config, tx);
		feed(&mut app, "eu-prod", "pods", pod("default", "api", serde_json::json!({})));
		feed(&mut app, "audit", "pods", pod("default", "api", serde_json::json!({})));
		app.update(Msg::Act("delete".into(), Some(row_id("eu-prod", "default/api"))));
		assert!(matches!(app.dialog, Some(Dialog::Confirm { unlock: true, .. })));
		app.update(Msg::Cancel);
		app.update(Msg::Act("delete".into(), Some(row_id("audit", "default/api"))));
		assert!(app.dialog.is_none());
		assert!(app.notice.as_ref().is_some_and(|n| n.error && n.text.contains("read-only")));
	}

	#[test]
	fn workloads_find_their_pods_by_selector() {
		let (mut app, _rx) = app();
		app.update(Msg::Kind("deployments.apps".into()));
		let deploy: DynamicObject = serde_json::from_value(serde_json::json!({
			"apiVersion": "apps/v1", "kind": "Deployment",
			"metadata": {"name": "api", "namespace": "default", "uid": "D"},
			"spec": {"selector": {"matchLabels": {"app": "api"}}, "template": {"spec": {"containers": [{"name": "web"}]}}}
		}))
		.unwrap();
		feed(&mut app, "a", "deployments.apps", deploy);
		app.update(Msg::Open(row_id("a", "default/api")));
		feed(&mut app, "a", "pods", pod("default", "api-1", serde_json::json!({"app": "api"})));
		feed(&mut app, "a", "pods", pod("default", "other", serde_json::json!({"app": "db"})));
		feed(&mut app, "a", "pods", pod("staging", "api-2", serde_json::json!({"app": "api"})));
		let kids: Vec<String> = app.child_pods().into_iter().map(|(id, _)| id).collect();
		assert_eq!(kids, [row_id("a", "default/api-1")]);
		assert_eq!(app.containers(), ["web"]);
	}

	#[test]
	fn history_returns_to_the_previous_kind_and_detail() {
		let (mut app, _rx) = app();
		feed(&mut app, "a", "pods", pod("default", "api", serde_json::json!({})));
		app.update(Msg::Open(row_id("a", "default/api")));
		app.update(Msg::Kind("services".into()));
		assert!(app.detail.is_none());
		app.update(Msg::Back);
		assert_eq!(app.kind, "pods");
		assert_eq!(app.detail.as_ref().map(|d| d.id.clone()), Some(row_id("a", "default/api")));
		app.update(Msg::Forward);
		assert_eq!(app.kind, "services");
	}
}
