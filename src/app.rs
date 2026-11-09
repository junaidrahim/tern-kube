//! UI state, the messages that change it, and key handling. Everything here
//! runs on the main thread; the backend only talks to it through `Update`s.
//!
//! Several contexts can be shown at once: each (context, kind) has its own
//! watch and store, and the table merges them. A row id is the context and
//! the object key joined by `SEP`, so every action knows its cluster.

use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
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
	state::{Saved, SavedForward},
	textedit::{Area, Line},
};

/// Watches kept warm per shown context (Aptakube keeps its watchers too).
const WARM_PER_CONTEXT: usize = 8;
/// Log text kept before the oldest half is dropped.
const LOG_LIMIT: usize = 4 << 20;
/// How long an unlock of a protected context lasts.
const UNLOCK_FOR: Duration = Duration::from_secs(15 * 60);
/// A watch failing this long marks its data stale.
const STALE_AFTER: Duration = Duration::from_secs(20);
/// Between the context and the object key in a row id.
pub const SEP: char = '\u{1f}';
/// Kinds the Health page reads.
pub const HEALTH_KINDS: [&str; 8] = [
	"pods",
	"deployments.apps",
	"statefulsets.apps",
	"daemonsets.apps",
	"nodes",
	"events",
	"persistentvolumeclaims",
	"jobs.batch",
];

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
	/// Show exactly the contexts of a tag.
	ContextTag(String),
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
	/// The picked rows changed (multi-select).
	Pick(Vec<String>),
	/// Run an action on every picked row.
	Bulk(String),
	/// Close the detail pane.
	CloseDetail,
	/// Show a detail tab.
	Tab(Tab),
	/// Sort by a column.
	Sort(String, SortDir),
	/// A field asked for the keys.
	Focus(Focus),
	/// Clear the filter.
	ClearFilter,
	/// Native edit of a field (UTF-16 range).
	EditField(Focus, usize, usize, String, usize),
	/// Run a row action (`act`) on a row (`None`: the open or selected one).
	Act(String, Option<String>),
	/// Show a container's log.
	Container(String),
	/// Toggle the previous container instance's log.
	Previous,
	/// Toggle log timestamps.
	Timestamps,
	/// Pause or resume the log view.
	Pause,
	/// Copy text to the clipboard.
	Copy(String),
	/// Copy the (filtered) log.
	CopyLog,
	/// Show or hide one secret value (or all with `None`).
	Reveal(Option<String>),
	/// Confirm the dialog.
	Confirm,
	/// Close the dialog.
	Cancel,
	/// Scale dialog: step the replicas.
	Step(i64),
	/// Port-forward dialog: pick a target port.
	ForwardPort(u16),
	/// Compare dialog: pick the other context.
	CompareContext(String),
	/// Start a stopped port forward.
	StartForward(u64),
	/// Start every stopped port forward.
	StartAllForwards,
	/// Stop a port forward (it stays saved).
	StopForward(u64),
	/// Forget a port forward.
	RemoveForward(u64),
	/// Cancel a running change.
	CancelOp(u64),
	/// Go to an object (owner, child, health or palette link).
	Goto(Goto),
	/// History.
	Back,
	/// History.
	Forward,
	/// Sign in to a context in a terminal.
	SignIn(String),
	/// Recreate a context's client and restart its watches.
	Reconnect(String),
	/// Open the command palette.
	Palette,
	/// Run palette entry `i`.
	PaletteRun(usize),
	/// Toggle a label column.
	LabelColumn(String),
	/// Open the column picker.
	Columns,
	/// YAML editor: start editing.
	EditYaml,
	/// YAML editor: show the diff (true) or go back to editing.
	Review(bool),
	/// YAML editor: apply.
	ApplyYaml,
	/// YAML editor: discard.
	DiscardYaml,
	/// Roll a Helm release back to a revision (row id of the revision).
	HelmRollback(String),
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
	/// The object as YAML (and the editor).
	Yaml,
	/// Its events.
	Events,
	/// Its pod's (or pods') log.
	Logs,
	/// A comparison with another context or namespace.
	Compare,
}

impl Tab {
	/// The tab id.
	pub const fn id(self) -> &'static str {
		match self {
			Self::Overview => "overview",
			Self::Yaml => "yaml",
			Self::Events => "events",
			Self::Logs => "logs",
			Self::Compare => "compare",
		}
	}

	/// The tab of `id`.
	pub fn from_id(id: &str) -> Option<Self> {
		[Self::Overview, Self::Yaml, Self::Events, Self::Logs, Self::Compare].into_iter().find(|t| t.id() == id)
	}
}

/// What a watch has delivered.
#[derive(Default)]
pub struct Store {
	/// Rows by key.
	pub rows:    HashMap<String, Row>,
	/// Objects by key.
	pub objs:    HashMap<String, DynamicObject>,
	/// The first list is complete.
	pub synced:  bool,
	/// The last error, cleared by the next event.
	pub error:   Option<String>,
	/// Forbidden to list or watch.
	pub denied:  bool,
	/// The credentials failed.
	pub auth:    bool,
	/// Failing since.
	pub failing: Option<Instant>,
	/// Keys seen during a relist.
	relisting:   Option<HashSet<String>>,
}

impl Store {
	/// Applies a watch event, projecting with `kind`.
	pub fn apply(&mut self, kind: &Kind, ev: WatchEv) {
		match ev {
			WatchEv::Init => self.relisting = Some(HashSet::new()),
			WatchEv::InitApply(obj) => {
				let key = rows::key_of(&obj);
				if let Some(seen) = &mut self.relisting {
					seen.insert(key.clone());
				}
				self.put(kind, key, obj);
			},
			WatchEv::InitDone => {
				if let Some(seen) = self.relisting.take() {
					self.rows.retain(|k, _| seen.contains(k));
					self.objs.retain(|k, _| seen.contains(k));
				}
				self.synced = true;
				self.clear_error();
			},
			WatchEv::Apply(obj) => {
				let key = rows::key_of(&obj);
				self.put(kind, key, obj);
				self.clear_error();
			},
			WatchEv::Delete(obj) => {
				let key = rows::key_of(&obj);
				self.rows.remove(&key);
				self.objs.remove(&key);
			},
			WatchEv::Error { message, forbidden, auth } => {
				self.error = Some(message);
				self.denied = forbidden;
				self.auth = auth;
				self.failing.get_or_insert_with(Instant::now);
			},
		}
	}

	fn clear_error(&mut self) {
		self.error = None;
		self.denied = false;
		self.auth = false;
		self.failing = None;
	}

	/// The data may be out of date: the watch has been failing a while.
	pub fn stale(&self) -> bool {
		self.failing.is_some_and(|t| t.elapsed() > STALE_AFTER)
	}

	fn put(&mut self, kind: &Kind, key: String, mut obj: DynamicObject) {
		// Nobody reads managed fields; they are often half of an object.
		obj.metadata.managed_fields = None;
		self.rows.insert(key.clone(), rows::project(kind, &obj));
		self.objs.insert(key, obj);
	}
}

/// The log of the open object.
#[derive(Default)]
pub struct Logs {
	/// Stream id; updates for other ids are stale.
	pub id:         u64,
	/// The container shown.
	pub container:  Option<String>,
	/// The previous instance's log.
	pub previous:   bool,
	/// Server timestamps on each line.
	pub timestamps: bool,
	/// Pods streamed (one for a pod, several for a workload).
	pub pods:       usize,
	/// The text, appended to (so frames carry only the new tail).
	pub text:       String,
	/// Lines received.
	pub lines:      usize,
	/// Streaming.
	pub live:       bool,
	/// Why it ended, when it failed.
	pub error:      Option<String>,
	/// The view is frozen at this many bytes of `text`.
	pub paused:     Option<usize>,
	/// Lines must match this (the filter grammar).
	pub filter:     Line,
}

impl Logs {
	/// What the log view shows: the text up to the pause, filtered.
	pub fn shown(&self) -> Cow<'_, str> {
		let text = &self.text[..self.paused.unwrap_or(self.text.len()).min(self.text.len())];
		if self.filter.text.trim().is_empty() {
			return Cow::Borrowed(text);
		}
		let f = Filter::parse(&self.filter.text);
		let mut out = String::new();
		for line in text.lines() {
			if f.matches(&strip_ansi(line).to_lowercase()) {
				out.push_str(line);
				out.push('\n');
			}
		}
		Cow::Owned(out)
	}
}

/// `s` without ANSI escape sequences.
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
	if !s.contains('\x1b') {
		return Cow::Borrowed(s);
	}
	let mut out = String::with_capacity(s.len());
	let mut chars = s.chars();
	while let Some(c) = chars.next() {
		if c == '\x1b' {
			// CSI: ESC [ params final-byte.
			if chars.next() == Some('[') {
				for d in chars.by_ref() {
					if ('@'..='~').contains(&d) {
						break;
					}
				}
			}
		} else {
			out.push(c);
		}
	}
	Cow::Owned(out)
}

/// The YAML editor's state.
pub struct Edit {
	/// The text being edited.
	pub area:   Area,
	/// The YAML it started from.
	pub base:   String,
	/// The resource version it started from.
	pub rv:     String,
	/// Showing the diff.
	pub review: bool,
	/// Why the last apply or parse failed.
	pub error:  Option<String>,
	/// An apply is in flight (its mutation id).
	pub busy:   Option<u64>,
}

/// A comparison of the open object with its twin elsewhere.
pub struct Compare {
	/// The other side, `context/namespace`.
	pub other: String,
	/// Fetch id; results for others are stale.
	pub id:    u64,
	/// The unified diff, or why there is none.
	pub diff:  Option<Result<String, String>>,
}

/// The detail pane.
pub struct Detail {
	/// The row id.
	pub id:        String,
	/// Picked rows whose merged log this shows (bulk logs), else empty.
	pub multi:     Vec<String>,
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
	/// Secret keys shown in clear.
	pub revealed:  BTreeSet<String>,
	/// The YAML editor.
	pub edit:      Option<Edit>,
	/// A comparison.
	pub compare:   Option<Compare>,
}

/// Which field has the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
	/// The table.
	List,
	/// The filter field.
	Filter,
	/// The log filter field.
	LogFilter,
	/// The YAML editor.
	Editor,
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

	fn object(&self) -> mutate::Object {
		mutate::Object {
			ar:        self.kind.ar.clone(),
			namespace: self.obj.metadata.namespace.clone(),
			name:      self.name(),
		}
	}
}

/// What a confirmation runs.
#[derive(Clone, Debug)]
pub enum Op {
	/// A change through the API.
	Change(mutate::Mutation),
	/// A terminal action (shell, edit, Helm) on a protected context.
	Terminal(actions::Action),
}

/// One command palette entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
	/// What it shows.
	pub label:  String,
	/// Muted detail.
	pub detail: String,
	/// An icon.
	pub icon:   &'static str,
	/// What it does.
	pub run:    PaletteRun,
}

/// What a palette entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteRun {
	/// Show a kind.
	Kind(String),
	/// Open an object.
	Goto(Goto),
	/// Send a message.
	Msg(&'static str),
}

/// A modal dialog.
pub enum Dialog {
	/// Run `op` on `targets` after a yes.
	Confirm {
		/// The objects.
		targets: Vec<Target>,
		/// What to do.
		op:      Op,
		/// A context is protected: confirming also unlocks it.
		unlock:  bool,
	},
	/// Pick a replica count.
	Scale {
		/// The objects.
		targets:  Vec<Target>,
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
	/// Jump anywhere.
	Palette {
		/// The query.
		query: Line,
		/// The highlighted entry.
		sel:   usize,
	},
	/// Pick where to compare the open object with.
	Compare {
		/// The object.
		target:    Target,
		/// The other context.
		context:   String,
		/// The other namespace.
		namespace: Line,
	},
	/// Pick label columns.
	Columns,
}

/// A port forward the UI knows.
pub struct Forward {
	/// Its spec.
	pub spec:    portforward::Spec,
	/// Running (else saved but stopped).
	pub running: bool,
	/// The bound local port, once listening.
	pub local:   Option<u16>,
	/// The pod connections go to, and how many are open.
	pub active:  Option<(String, u32)>,
	/// The last failure.
	pub error:   Option<String>,
	/// The listener died.
	pub dead:    bool,
}

/// A change in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
	/// What it is.
	pub label:    String,
	/// Its latest progress.
	pub progress: Option<String>,
	/// Long-running: offer a cancel.
	pub long:     bool,
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
	pub contexts:      Vec<String>,
	/// Shown contexts, the primary first (never empty).
	pub active:        Vec<String>,
	/// Namespace filter (`None`: all).
	pub namespace:     Option<String>,
	/// Built-in kinds.
	builtins:          Vec<Kind>,
	/// Discovered custom kinds per context.
	pub custom:        HashMap<String, Vec<Kind>>,
	/// Discovery errors per context.
	pub discovery:     HashMap<String, String>,
	/// The active kind id.
	pub kind:          String,
	/// Watch stores.
	pub stores:        HashMap<WatchKey, Store>,
	/// Most recently used watch first.
	lru:               VecDeque<WatchKey>,
	/// Filter field.
	pub query:         Line,
	/// Compiled `query`.
	filter:            Filter,
	/// Keys' target.
	pub focus:         Focus,
	/// Sort column and direction.
	pub sort:          (String, SortDir),
	/// Selected row id.
	pub selected:      Option<String>,
	/// Picked row ids (multi-select).
	pub picked:        Vec<String>,
	/// The open detail.
	pub detail:        Option<Detail>,
	/// Which select is open.
	pub open:          Option<Which>,
	/// The dialog shown.
	pub dialog:        Option<Dialog>,
	/// A transient message for the dock.
	pub notice:        Option<Notice>,
	/// Settings.
	pub config:        Config,
	/// Protected contexts unlocked until.
	unlocked:          HashMap<String, Instant>,
	/// Changes in flight by id.
	pub pending:       BTreeMap<u64, Pending>,
	/// Metrics per context.
	pub metrics:       HashMap<String, Result<metrics::Snapshot, String>>,
	/// Contexts whose metrics are polled.
	metrics_on:        Vec<String>,
	/// Port forwards by id.
	pub forwards:      BTreeMap<u64, Forward>,
	/// Label keys shown as columns, by kind id.
	pub label_columns: BTreeMap<String, Vec<String>>,
	history:           Vec<Place>,
	future:            Vec<Place>,
	/// `g` was pressed: the next key jumps to a kind.
	pub g_pending:     bool,
	/// Open this row's detail once its kind's store has it.
	goto:              Option<String>,
	/// Text to put on the clipboard (the main loop sends it).
	pub copy:          Option<String>,
	/// Visible row ids in order, rebuilt when stale.
	visible:           Vec<String>,
	/// `visible` needs a rebuild.
	stale:             bool,
	/// Id source for streams, mutations and forwards.
	next_id:           u64,
	cmd:               UnboundedSender<Cmd>,
	/// Exit requested.
	pub quit:          bool,
}

/// `g` + key jumps.
pub const JUMPS: [(&str, &str); 16] = [
	("h", "health"),
	("p", "pods"),
	("d", "deployments.apps"),
	("t", "statefulsets.apps"),
	("a", "daemonsets.apps"),
	("r", "replicasets.apps"),
	("j", "jobs.batch"),
	("o", "cronjobs.batch"),
	("l", "helmreleases"),
	("s", "services"),
	("i", "ingresses.networking.k8s.io"),
	("c", "configmaps"),
	("x", "secrets"),
	("v", "persistentvolumeclaims"),
	("n", "nodes"),
	("e", "events"),
];

/// Bulk actions on picked rows: (act, label).
pub const BULK: [(&str, &str); 4] =
	[("bulk-logs", "Logs"), ("bulk-restart", "Restart"), ("bulk-scale", "Scale"), ("bulk-delete", "Delete")];

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
			query: Line::default(),
			filter: Filter::default(),
			focus: Focus::List,
			sort: ("name".into(), SortDir::Asc),
			selected: None,
			picked: Vec::new(),
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
			label_columns: saved.label_columns.clone(),
			history: Vec::new(),
			future: Vec::new(),
			g_pending: false,
			goto: None,
			copy: None,
			visible: Vec::new(),
			stale: true,
			next_id: 0,
			cmd,
			quit: false,
		};
		for f in &saved.forwards {
			app.add_forward(f, false);
		}
		app.activate_contexts();
		app.goto = saved.detail.clone().filter(|id| app.active.iter().any(|c| c == split_id(id).0));
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
			context:       Some(self.primary().to_owned()),
			contexts:      self.active.clone(),
			kind:          Some(self.kind.clone()),
			namespace:     self.namespace.clone(),
			detail:        self.detail.as_ref().filter(|d| d.multi.is_empty()).map(|d| d.id.clone()),
			forwards:      self.forwards.values().map(|f| saved_forward(&f.spec)).collect(),
			label_columns: self.label_columns.clone(),
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

	/// The kind with id `id`.
	pub fn kind_by_id(&self, id: &str) -> Option<Kind> {
		self.kinds().find(|k| k.id == id).cloned()
	}

	/// The watch key of `kind` in `context`.
	pub fn watch_key(context: &str, kind: &str) -> WatchKey {
		WatchKey { context: context.to_owned(), kind: kind.to_owned(), namespace: None }
	}

	/// The store of `kind` in `context`.
	pub fn store_of(&self, context: &str, kind: &str) -> Option<&Store> {
		self.stores.get(&Self::watch_key(context, kind))
	}

	/// The store of the active kind in `context`.
	pub fn store(&self, context: &str) -> Option<&Store> {
		self.store_of(context, &self.kind)
	}

	/// The active kind's stores, by shown context.
	pub fn stores(&self) -> impl Iterator<Item = (&str, &Store)> {
		self.active.iter().filter_map(|c| self.store(c).map(|s| (c.as_str(), s)))
	}

	/// Every row of the active kind across shown contexts.
	pub fn total(&self) -> usize {
		self.stores().map(|(_, s)| s.rows.len()).sum()
	}

	/// The row and object of row id `id` (active kind).
	pub fn lookup(&self, id: &str) -> Option<(&Row, &DynamicObject)> {
		let (ctx, key) = split_id(id);
		let s = self.store(ctx)?;
		Some((s.rows.get(key)?, s.objs.get(key)?))
	}

	/// Namespace names across shown contexts.
	pub fn namespaces(&self) -> Vec<String> {
		let mut set: HashSet<String> = HashSet::new();
		for ctx in &self.active {
			match self.store_of(ctx, "namespaces") {
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
		let age = cols.iter().position(|c| c.id == "age").unwrap_or(cols.len());
		if kind.has_metrics() && self.active.iter().any(|c| matches!(self.metrics.get(c), Some(Ok(_)))) {
			cols.insert(age, kinds::MEMORY);
			cols.insert(age, kinds::CPU);
		}
		for key in self.label_columns.get(&kind.id).into_iter().flatten() {
			let at = cols.iter().position(|c| c.id == "age").unwrap_or(cols.len());
			cols.insert(at, ColSpec {
				id:       Cow::Owned(format!("label:{key}")),
				head:     Cow::Owned(key.rsplit('/').next().unwrap_or(key).to_owned()),
				numeric:  false,
				grow:     false,
				priority: 30.0,
			});
		}
		cols
	}

	/// Label keys on the active kind's objects, most common first (for the
	/// column picker).
	pub fn label_keys(&self) -> Vec<String> {
		let mut count: HashMap<&str, usize> = HashMap::new();
		for (_, s) in self.stores() {
			for o in s.objs.values() {
				for k in o.metadata.labels.iter().flatten().map(|(k, _)| k.as_str()) {
					*count.entry(k).or_default() += 1;
				}
			}
		}
		let mut keys: Vec<(&str, usize)> = count.into_iter().collect();
		keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
		keys.into_iter().take(40).map(|(k, _)| k.to_owned()).collect()
	}

	/// Pods of `obj` (a workload or service) in `context`, by selector.
	pub fn selected_pods<'a>(&'a self, context: &str, kind: &Kind, obj: &DynamicObject) -> Vec<(String, &'a Row, &'a DynamicObject)> {
		let Some(pods) = self.store_of(context, "pods") else { return vec![] };
		let ns = obj.metadata.namespace.as_deref();
		let sel = selector(kind, obj);
		if sel.is_empty() && kind.projector != Projector::Node {
			return vec![];
		}
		let node = (kind.projector == Projector::Node).then(|| obj.metadata.name.clone().unwrap_or_default());
		let mut out: Vec<_> = pods
			.objs
			.iter()
			.filter(|(_, p)| match &node {
				Some(n) => p.data.pointer("/spec/nodeName").and_then(Value::as_str) == Some(n.as_str()),
				None => {
					p.metadata.namespace.as_deref() == ns
						&& sel.iter().all(|(k, v)| p.metadata.labels.as_ref().and_then(|l| l.get(k)) == Some(v))
				},
			})
			.filter_map(|(k, p)| pods.rows.get(k).map(|r| (row_id(context, k), r, p)))
			.collect();
		out.sort_by(|a, b| crate::format::natural_cmp(&a.1.key, &b.1.key));
		out
	}

	/// CPU and memory of a row in `context`, with what it is measured against:
	/// a pod's or node's own, or the sum of a workload's pods.
	pub fn usage(&self, context: &str, row: &Row) -> Option<(metrics::Usage, metrics::Resources)> {
		let snap = self.metrics.get(context)?.as_ref().ok()?;
		let kind = self.kind();
		match kind.projector {
			Projector::Pod => Some((*snap.pods.get(&row.key)?, row.res.unwrap_or_default())),
			Projector::Node => Some((*snap.nodes.get(&row.key)?, row.res.unwrap_or_default())),
			_ if kind.sums_pods() => {
				let obj = self.store(context)?.objs.get(&row.key)?;
				let pods = self.selected_pods(context, kind, obj);
				if pods.is_empty() {
					return None;
				}
				let mut u = metrics::Usage::default();
				let mut res = metrics::Resources::default();
				let add = |a: &mut Option<f64>, b: Option<f64>| {
					if let Some(b) = b {
						*a.get_or_insert(0.0) += b;
					}
				};
				let mut any = false;
				for (_, r, _) in pods {
					if let Some(pu) = snap.pods.get(&r.key) {
						u.cpu += pu.cpu;
						u.mem += pu.mem;
						any = true;
					}
					let pr = r.res.unwrap_or_default();
					add(&mut res.cpu_req, pr.cpu_req);
					add(&mut res.cpu_lim, pr.cpu_lim);
					add(&mut res.mem_req, pr.mem_req);
					add(&mut res.mem_lim, pr.mem_lim);
				}
				any.then_some((u, res))
			},
			_ => None,
		}
	}

	/// A label column's value.
	pub fn label_value(&self, context: &str, row: &Row, col: &str) -> Option<String> {
		let key = col.strip_prefix("label:")?;
		let obj = self.store(context)?.objs.get(&row.key)?;
		obj.metadata.labels.as_ref()?.get(key).cloned()
	}

	/// The sort value of column `col` of `row` (metrics, labels and context
	/// included).
	fn sort_val(&self, context: &str, row: &Row, col: &str) -> Val {
		match col {
			"context" => Val::Str(self.config.display(context).to_owned()),
			"cpu" => self.usage(context, row).map_or(Val::Empty, |(u, _)| Val::Num((u.cpu * 1000.0) as i64, String::new())),
			"memory" => self.usage(context, row).map_or(Val::Empty, |(u, _)| Val::Num(u.mem as i64, String::new())),
			c if c.starts_with("label:") => self.label_value(context, row, c).map_or(Val::Empty, Val::Str),
			_ => row.get(col).clone(),
		}
	}

	/// Starts (or refreshes in the LRU) the watch of `key`.
	fn watch(&mut self, key: WatchKey, kind: &Kind) {
		if let Some(pos) = self.lru.iter().position(|k| *k == key) {
			self.lru.remove(pos);
		} else {
			self.stores.entry(key.clone()).or_default();
			self.send(Cmd::Watch { key: key.clone(), ar: kind.ar.clone(), fields: kind.fields.clone() });
		}
		self.lru.push_front(key);
		let cap = WARM_PER_CONTEXT * self.active.len() + HEALTH_KINDS.len();
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

	fn watch_kind(&mut self, id: &str) {
		if let Some(kind) = self.kind_by_id(id) {
			for ctx in self.active.clone() {
				self.watch(Self::watch_key(&ctx, &kind.id), &kind);
			}
		}
	}

	fn activate_kind(&mut self) {
		let kind = self.kind().clone();
		if kind.is_page() {
			for k in HEALTH_KINDS {
				self.watch_kind(k);
			}
		} else {
			self.watch_kind(&kind.id);
		}
		if kind.sums_pods() {
			// Workload CPU and memory are their pods' sums.
			self.watch_kind("pods");
		}
		self.selected = None;
		self.picked.clear();
		self.close_detail();
		let first = kind.projector.columns().first().map_or(Cow::Borrowed("name"), |c| c.id.clone());
		self.sort = (first.into_owned(), SortDir::Asc);
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

	/// Polls metrics of the shown contexts while pods, nodes or workloads are
	/// listed.
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
			if self.focus == Focus::Editor || self.focus == Focus::LogFilter {
				self.focus = Focus::List;
			}
		}
	}

	/// The kind a watch key belongs to.
	fn kind_of(&self, key: &WatchKey) -> Option<Kind> {
		self.builtins
			.iter()
			.chain(self.custom.get(&key.context).into_iter().flatten())
			.find(|k| k.id == key.kind)
			.cloned()
	}

	/// Applies a backend update; true when the view changed.
	pub fn update_from(&mut self, u: Update) -> bool {
		match u {
			Update::Watch(key, ev) => {
				let Some(kind) = self.kind_of(&key) else { return false };
				let Some(store) = self.stores.get_mut(&key) else { return false };
				store.apply(&kind, ev);
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
				let page = self.kind().is_page() && HEALTH_KINDS.contains(&key.kind.as_str());
				// Child pods and workload sums read the pods store.
				let pods = key.kind == "pods" && (self.detail.is_some() || self.kind().sums_pods());
				active || (shown && (key.kind == "namespaces" || pods || page))
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
					logs.paused = logs.paused.map(|p| p.saturating_sub(cut));
				}
				self.detail.as_ref().is_some_and(|d| d.tab == Tab::Logs && d.logs.paused.is_none())
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
				let events = self.kind_by_id("events");
				let Some(d) = self.detail.as_mut().filter(|d| d.events_id == id) else { return false };
				if let Some(k) = events {
					d.events.apply(&k, ev);
				}
				true
			},
			Update::Metrics(ctx, snap) => {
				self.metrics.insert(ctx.clone(), snap);
				if matches!(self.sort.0.as_str(), "cpu" | "memory") {
					self.stale = true;
				}
				self.active.contains(&ctx) && self.kind().has_metrics()
			},
			Update::Progress(id, text) => {
				if let Some(p) = self.pending.get_mut(&id) {
					p.progress = Some(text);
				}
				true
			},
			Update::Mutated(id, result) => {
				self.pending.remove(&id);
				let editing = self.detail.as_mut().and_then(|d| d.edit.as_mut()).filter(|e| e.busy == Some(id));
				match (editing, result) {
					(Some(_), Ok(text)) => {
						if let Some(d) = self.detail.as_mut() {
							d.edit = None;
							d.yaml = None;
						}
						if self.focus == Focus::Editor {
							self.focus = Focus::List;
						}
						self.info(text);
					},
					(Some(e), Err(err)) => {
						e.busy = None;
						e.error = Some(err);
					},
					(None, Ok(text)) => self.info(text),
					(None, Err(e)) => self.error(e),
				}
				true
			},
			Update::Got(id, result) => {
				if !self.detail.as_ref().and_then(|d| d.compare.as_ref()).is_some_and(|c| c.id == id) {
					return false;
				}
				let ours = self.detail_obj().map(|(_, o)| o.clone());
				let diff = match (ours, result) {
					(Some(a), Ok(b)) => Ok(compare_yaml(&a, &b)),
					(_, Err(e)) => Err(e),
					(None, _) => Err("The object is gone".into()),
				};
				if let Some(c) = self.detail.as_mut().and_then(|d| d.compare.as_mut()) {
					c.diff = Some(diff);
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
						f.running = false;
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
		let helm = self.kind().projector == Projector::Helm;
		let mut rows: Vec<(Val, &str, &Row)> = Vec::new();
		for (ctx, store) in self.stores() {
			// Helm keeps one secret per revision: list the newest of each release.
			let newest: Option<HashMap<(Option<&str>, String), i64>> = helm.then(|| {
				let mut m = HashMap::new();
				for r in store.rows.values() {
					let rev = match r.get("revision") {
						Val::Num(n, _) => *n,
						_ => 0,
					};
					let e = m.entry((r.namespace.as_deref(), r.get("name").text(0))).or_insert(rev);
					*e = (*e).max(rev);
				}
				m
			});
			for r in store.rows.values() {
				if ns.is_some() && r.namespace.is_some() && r.namespace.as_deref() != ns {
					continue;
				}
				if let Some(m) = &newest {
					let rev = match r.get("revision") {
						Val::Num(n, _) => *n,
						_ => 0,
					};
					if m.get(&(r.namespace.as_deref(), r.get("name").text(0))) != Some(&rev) {
						continue;
					}
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
		let visible: HashSet<&String> = self.visible.iter().collect();
		self.picked.retain(|p| visible.contains(p));
	}

	/// The open object and its context.
	pub fn detail_obj(&self) -> Option<(&str, &DynamicObject)> {
		let d = self.detail.as_ref()?;
		let (ctx, _) = split_id(&d.id);
		Some((ctx, self.lookup(&d.id)?.1))
	}

	fn place(&self) -> Place {
		Place { kind: self.kind.clone(), detail: self.detail.as_ref().filter(|d| d.multi.is_empty()).map(|d| d.id.clone()) }
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

	fn new_detail(&mut self, id: String, tab: Tab) -> Detail {
		let events_id = self.id();
		Detail {
			id,
			multi: Vec::new(),
			tab,
			yaml: None,
			events_id,
			events: Store::default(),
			logs: Logs::default(),
			revealed: BTreeSet::new(),
			edit: None,
			compare: None,
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
		let keep_tab = self
			.detail
			.as_ref()
			.filter(|d| d.multi.is_empty())
			.map(|d| d.tab)
			.filter(|t| (*t != Tab::Logs || has_logs) && *t != Tab::Compare);
		self.close_detail();
		let detail = self.new_detail(id.clone(), keep_tab.unwrap_or(Tab::Overview));
		self.send(Cmd::Events { id: detail.events_id, context: ctx.clone(), namespace, uid });
		self.selected = Some(id);
		self.detail = Some(detail);
		if self.kind().selects_pods() || self.kind().projector == Projector::Node {
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

	/// Pods of the open object: its selected pods, or a node's pods.
	pub fn child_pods(&self) -> Vec<(String, &Row)> {
		let Some((ctx, obj)) = self.detail_obj() else { return vec![] };
		self.selected_pods(ctx, self.kind(), obj).into_iter().map(|(id, r, _)| (id, r)).collect()
	}

	/// The log sources of the open object (or of the picked rows).
	fn log_sources(&self, container: Option<&String>) -> Vec<LogSource> {
		let Some(d) = &self.detail else { return vec![] };
		let kind = self.kind();
		let ids: Vec<String> = if d.multi.is_empty() { vec![d.id.clone()] } else { d.multi.clone() };
		let mut out = Vec::new();
		for id in ids {
			let Some((_, obj)) = self.lookup(&id) else { continue };
			let (ctx, _) = split_id(&id);
			let ns = obj.metadata.namespace.clone().unwrap_or_default();
			if kind.is_pod() {
				let c = container.cloned().or_else(|| obj.data.get("spec").map(container_names).and_then(|c| c.into_iter().next()));
				out.push(LogSource { context: ctx.to_owned(), namespace: ns, pod: obj.metadata.name.clone().unwrap_or_default(), container: c });
			} else {
				for (_, _, p) in self.selected_pods(ctx, kind, obj) {
					let c = container.cloned().or_else(|| p.data.get("spec").map(container_names).and_then(|c| c.into_iter().next()));
					out.push(LogSource {
						context:   ctx.to_owned(),
						namespace: ns.clone(),
						pod:       p.metadata.name.clone().unwrap_or_default(),
						container: c,
					});
				}
			}
		}
		out
	}

	fn start_logs(&mut self) {
		let Some(d) = &self.detail else { return };
		let (chosen, previous, timestamps, filter) =
			(d.logs.container.clone(), d.logs.previous, d.logs.timestamps, d.logs.filter.clone());
		let container = if d.multi.is_empty() { chosen.or_else(|| self.containers().into_iter().next()) } else { chosen };
		let sources = self.log_sources(container.as_ref());
		let id = self.id();
		let Some(d) = self.detail.as_mut() else { return };
		let base = Logs { id, container, previous, timestamps, filter, ..Logs::default() };
		if sources.is_empty() {
			d.logs = Logs { error: Some("No pods (yet)".into()), ..base };
			return;
		}
		d.logs = Logs { pods: sources.len(), live: true, ..base };
		self.send(Cmd::Logs { id, sources, previous, timestamps });
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
		if self.detail.as_ref().is_some_and(|d| !d.multi.is_empty()) {
			return vec![Tab::Logs];
		}
		let mut t = vec![Tab::Overview, Tab::Yaml, Tab::Events];
		if self.kind().has_logs() {
			t.push(Tab::Logs);
		}
		if self.detail.as_ref().is_some_and(|d| d.compare.is_some()) {
			t.push(Tab::Compare);
		}
		t
	}

	fn set_query(&mut self, query: Line) {
		self.query = query;
		self.filter = Filter::parse(&self.query.text);
		self.stale = true;
	}

	/// The target of row `id`, else the open object, else the selected row.
	fn target(&self, id: Option<&str>) -> Option<Target> {
		let id = id
			.or_else(|| self.detail.as_ref().filter(|d| d.multi.is_empty()).map(|d| d.id.as_str()))
			.or(self.selected.as_deref())?;
		let (_, obj) = self.lookup(id)?;
		let (ctx, key) = split_id(id);
		Some(Target { context: ctx.to_owned(), kind: self.kind().clone(), key: key.to_owned(), obj: obj.clone() })
	}

	/// The row actions offered for an object of `kind`, as `(act, label)`.
	pub fn acts(&self, kind: &Kind, obj: &DynamicObject) -> Vec<(&'static str, &'static str)> {
		let mut v = Vec::new();
		if kind.projector == Projector::Helm {
			return vec![("helm-uninstall", "Uninstall")];
		}
		if kind.is_pod() {
			v.extend([("shell", "Shell"), ("debug", "Debug"), ("logs", "Logs")]);
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
		match kind.projector {
			Projector::CronJob => {
				v.push(("trigger", "Trigger now"));
				v.push(if flag("/spec/suspend") { ("resume", "Resume") } else { ("suspend", "Suspend") });
			},
			Projector::Node => {
				v.push(if flag("/spec/unschedulable") { ("uncordon", "Uncordon") } else { ("cordon", "Cordon") });
				v.push(("drain", "Drain…"));
			},
			Projector::Flux => v.push(("reconcile", "Reconcile")),
			Projector::ArgoApp => v.push(("refresh", "Refresh")),
			_ => {},
		}
		v.push(("compare", "Compare…"));
		v.push(("edit", "Edit YAML"));
		v.push(("delete", "Delete"));
		v
	}

	/// Whether `act` changes the cluster or runs something in it.
	fn changes(act: &str) -> bool {
		!matches!(act, "logs" | "forward" | "follow" | "compare")
	}

	/// Starts action `act` on row `id` (or the open/selected row).
	fn act(&mut self, act: &str, id: Option<String>) {
		let Some(target) = self.target(id.as_deref()) else {
			self.error("Select a row first");
			return;
		};
		let protection = self.protection(&target.context);
		if Self::changes(act) && protection == Protection::ReadOnly {
			self.error(format!("{} is read-only", self.config.display(&target.context)));
			return;
		}
		let locked = Self::changes(act) && protection == Protection::Protected && !self.is_unlocked(&target.context);
		let confirm = |op| Some(Dialog::Confirm { targets: vec![target.clone()], op, unlock: locked });
		let terminal = |app: &mut Self, action| {
			if locked {
				confirm(Op::Terminal(action))
			} else {
				app.terminal(action, &target);
				None
			}
		};
		let annotate = |k: &str, v: String| Op::Change(mutate::Mutation::Annotate(k.to_owned(), v));
		self.dialog = match act {
			"shell" => terminal(self, actions::Action::Shell),
			"debug" => terminal(self, actions::Action::Debug),
			"edit" => terminal(self, actions::Action::Edit),
			"helm-uninstall" => confirm(Op::Terminal(actions::Action::HelmUninstall)),
			"follow" => {
				self.terminal(actions::Action::FollowLogs, &target);
				None
			},
			"logs" => {
				let id = row_id(&target.context, &target.key);
				if self.detail.as_ref().is_none_or(|d| d.id != id || !d.multi.is_empty()) {
					self.open_detail(id, true);
				}
				self.set_tab(Tab::Logs);
				None
			},
			"forward" => self.forward_dialog(target),
			"compare" => {
				let ns = target.obj.metadata.namespace.clone().unwrap_or_default();
				let other = self.active.iter().chain(&self.contexts).find(|c| **c != target.context).cloned();
				Some(Dialog::Compare {
					context: other.unwrap_or_else(|| target.context.clone()),
					namespace: Line::new(ns),
					target,
				})
			},
			"scale" => {
				let n = target.obj.data.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
				Some(Dialog::Scale { targets: vec![target], replicas: n.to_string() })
			},
			"restart" => confirm(Op::Change(mutate::Mutation::Restart)),
			"delete" => confirm(Op::Change(mutate::Mutation::Delete)),
			"trigger" => confirm(Op::Change(mutate::Mutation::Trigger)),
			"suspend" | "resume" => confirm(Op::Change(mutate::Mutation::Suspend(act == "suspend"))),
			"cordon" | "uncordon" => confirm(Op::Change(mutate::Mutation::Cordon(act == "cordon"))),
			"drain" => confirm(Op::Change(mutate::Mutation::Drain)),
			"reconcile" => confirm(annotate("reconcile.fluxcd.io/requestedAt", jiff::Timestamp::now().to_string())),
			"refresh" => confirm(annotate("argocd.argoproj.io/refresh", "normal".into())),
			_ => None,
		};
	}

	/// The picked rows as targets.
	fn picked_targets(&self) -> Vec<Target> {
		self.picked.iter().filter_map(|id| self.target(Some(id))).collect()
	}

	fn bulk(&mut self, act: &str) {
		let targets = self.picked_targets();
		if targets.is_empty() {
			self.error("Pick rows first");
			return;
		}
		let kind = self.kind().clone();
		if act == "bulk-logs" {
			if !kind.has_logs() {
				self.error(format!("{} have no logs", kind.label));
				return;
			}
			self.close_detail();
			let first = self.picked[0].clone();
			let mut d = self.new_detail(first, Tab::Logs);
			d.multi = self.picked.clone();
			self.detail = Some(d);
			self.start_logs();
			return;
		}
		if let Some(ctx) = targets.iter().map(|t| t.context.clone()).find(|c| self.protection(c) == Protection::ReadOnly) {
			self.error(format!("{} is read-only", self.config.display(&ctx)));
			return;
		}
		let unlock =
			targets.iter().any(|t| self.protection(&t.context) == Protection::Protected && !self.is_unlocked(&t.context));
		self.dialog = match act {
			"bulk-delete" => Some(Dialog::Confirm { targets, op: Op::Change(mutate::Mutation::Delete), unlock }),
			"bulk-restart" if kind.restartable() => {
				Some(Dialog::Confirm { targets, op: Op::Change(mutate::Mutation::Restart), unlock })
			},
			"bulk-scale" if kind.scalable() => Some(Dialog::Scale { targets, replicas: "1".into() }),
			_ => {
				self.error(format!("{} can't do that", kind.label));
				None
			},
		};
	}

	fn terminal(&mut self, action: actions::Action, t: &Target) {
		let container = self.detail.as_ref().and_then(|d| d.logs.container.clone());
		let name = if t.kind.projector == Projector::Helm { t.obj_release_name() } else { t.name() };
		let target = actions::Target {
			context: t.context.clone(),
			resource: t.kind.id.clone(),
			namespace: t.obj.metadata.namespace.clone(),
			name,
			container: container.or_else(|| t.obj.data.get("spec").map(container_names).and_then(|c| c.into_iter().next())),
			is_pod: t.kind.is_pod(),
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
				.filter_map(|p| port_entry(&p, "containerPort"))
				.collect()
		} else {
			target
				.obj
				.data
				.pointer("/spec/ports")
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.filter_map(|p| port_entry(p, "port"))
				.collect()
		};
		let Some(&(port, _)) = ports.first() else {
			self.error(format!("{} declares no ports", target.label()));
			return None;
		};
		Some(Dialog::Forward { target, ports, port, local: port.to_string() })
	}

	fn add_forward(&mut self, f: &SavedForward, run: bool) -> u64 {
		let id = self.id();
		let spec = portforward::Spec {
			id,
			context: f.context.clone(),
			namespace: f.namespace.clone(),
			target: if f.kind == "service" {
				portforward::Target::Service(f.name.clone())
			} else {
				portforward::Target::Pod(f.name.clone())
			},
			port: f.port,
			local: f.local,
		};
		self.forwards.insert(id, Forward { spec, running: false, local: None, active: None, error: None, dead: false });
		if run {
			self.start_forward(id);
		}
		id
	}

	fn start_forward(&mut self, id: u64) {
		let Some(f) = self.forwards.get_mut(&id) else { return };
		if f.running {
			return;
		}
		f.running = true;
		f.dead = false;
		f.error = None;
		f.local = None;
		f.active = None;
		let spec = f.spec.clone();
		self.send(Cmd::Forward(spec));
	}

	fn stop_forward(&mut self, id: u64) {
		if let Some(f) = self.forwards.get_mut(&id)
			&& f.running
		{
			f.running = false;
			f.local = None;
			f.active = None;
			self.send(Cmd::StopForward(id));
		}
	}

	fn confirm(&mut self) {
		let Some(dialog) = self.dialog.take() else { return };
		match dialog {
			Dialog::Confirm { targets, op, unlock } => {
				if unlock {
					for t in &targets {
						self.unlocked.insert(t.context.clone(), Instant::now() + UNLOCK_FOR);
					}
				}
				for t in &targets {
					match &op {
						Op::Terminal(action) => self.terminal(*action, t),
						Op::Change(change) => {
							self.mutate(t, change.clone());
						},
					}
				}
			},
			Dialog::Scale { targets, replicas } => match replicas.parse::<i64>() {
				Ok(n) if n >= 0 => {
					let unlock = targets
						.iter()
						.any(|t| self.protection(&t.context) == Protection::Protected && !self.is_unlocked(&t.context));
					if unlock {
						self.dialog = Some(Dialog::Confirm { targets, op: Op::Change(mutate::Mutation::Scale(n)), unlock });
					} else {
						for t in &targets {
							self.mutate(t, mutate::Mutation::Scale(n));
						}
					}
				},
				_ => {
					self.error("Replicas must be a whole number");
					self.dialog = Some(Dialog::Scale { targets, replicas });
				},
			},
			Dialog::Forward { target, port, local, .. } => {
				let Ok(local) = local.parse::<u16>() else {
					self.error("Local port must be 0–65535");
					return;
				};
				let saved = SavedForward {
					context: target.context.clone(),
					namespace: target.obj.metadata.namespace.clone().unwrap_or_default(),
					kind: if target.kind.is_pod() { "pod".into() } else { "service".into() },
					name: target.name(),
					port,
					local,
				};
				self.add_forward(&saved, true);
			},
			Dialog::Palette { query, sel } => {
				let items = self.palette_items(&query.text);
				if let Some(item) = items.get(sel).or_else(|| items.first()).cloned() {
					self.run_palette(item.run);
				}
			},
			Dialog::Compare { target, context, namespace } => {
				let id = self.id();
				let other = format!("{}/{}", self.config.display(&context), namespace.text);
				let ns = (!namespace.text.is_empty()).then(|| namespace.text.clone());
				self.send(Cmd::Get {
					id,
					context,
					obj: mutate::Object { ar: target.kind.ar.clone(), namespace: ns, name: target.name() },
				});
				if let Some(d) = self.detail.as_mut() {
					d.compare = Some(Compare { other, id, diff: None });
					d.tab = Tab::Compare;
				}
			},
			Dialog::Contexts | Dialog::Columns => {},
		}
	}

	fn mutate(&mut self, target: &Target, change: mutate::Mutation) -> u64 {
		let id = self.id();
		let long = matches!(change, mutate::Mutation::Drain);
		self.pending.insert(id, Pending { label: format!("{} {}", change.verb(), target.label()), progress: None, long });
		if matches!(change, mutate::Mutation::Delete)
			&& self.detail.as_ref().is_some_and(|d| d.id == row_id(&target.context, &target.key))
		{
			self.close_detail();
		}
		self.send(Cmd::Mutate { id, context: target.context.clone(), obj: target.object(), change });
		id
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

	/// The palette's entries for `query`: kinds, actions and loaded objects.
	pub fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
		let q = query.trim().to_lowercase();
		let hit = |s: &str| q.is_empty() || q.split_whitespace().all(|w| s.to_lowercase().contains(w));
		let mut out = Vec::new();
		for k in self.kinds() {
			let detail = if k.group == kinds::Group::Custom { k.ar.group.clone() } else { k.group.label().to_owned() };
			if hit(&format!("{} {} {}", k.label, k.ar.kind, k.id)) {
				out.push(PaletteItem { label: k.label.clone(), detail, icon: "list", run: PaletteRun::Kind(k.id.clone()) });
			}
		}
		for (label, msg) in [
			("Pick contexts", "contexts"),
			("Back", "back"),
			("Forward", "forward"),
			("Choose label columns", "columns"),
			("Start all port forwards", "forwards"),
		] {
			if hit(label) {
				out.push(PaletteItem { label: label.into(), detail: "Action".into(), icon: "zap", run: PaletteRun::Msg(msg) });
			}
		}
		if q.len() >= 2 {
			let mut objs = Vec::new();
			for (key, store) in &self.stores {
				if !self.active.contains(&key.context) || key.kind == "events" {
					continue;
				}
				let Some(kind) = self.kind_of(key) else { continue };
				for r in store.rows.values() {
					if hit(&r.key) {
						objs.push(PaletteItem {
							label:  r.key.clone(),
							detail: format!("{} · {}", kind.ar.kind, self.config.display(&key.context)),
							icon:   "box",
							run:    PaletteRun::Goto(Goto {
								context: key.context.clone(),
								kind:    kind.id.clone(),
								key:     r.key.clone(),
							}),
						});
					}
				}
			}
			objs.sort_by(|a, b| crate::format::natural_cmp(&a.label, &b.label));
			out.extend(objs.into_iter().take(60));
		}
		out
	}

	fn run_palette(&mut self, run: PaletteRun) {
		match run {
			PaletteRun::Kind(id) => self.set_kind(id),
			PaletteRun::Goto(g) => self.goto(g),
			PaletteRun::Msg(m) => self.update(match m {
				"contexts" => Msg::PickContexts,
				"back" => Msg::Back,
				"forward" => Msg::Forward,
				"columns" => Msg::Columns,
				_ => Msg::StartAllForwards,
			}),
		}
	}

	fn start_edit(&mut self) {
		let Some((_, obj)) = self.detail_obj() else { return };
		let rv = obj.metadata.resource_version.clone().unwrap_or_default();
		let yaml = editable_yaml(obj, &self.kind().ar);
		if let Some(d) = self.detail.as_mut() {
			d.tab = Tab::Yaml;
			d.edit = Some(Edit { area: Area::new(yaml.clone()), base: yaml, rv, review: false, error: None, busy: None });
		}
		self.focus = Focus::Editor;
	}

	fn apply_edit(&mut self) {
		let Some(target) = self.target(None) else { return };
		let protection = self.protection(&target.context);
		let unlocked = self.is_unlocked(&target.context);
		let Some(edit) = self.detail.as_mut().and_then(|d| d.edit.as_mut()) else { return };
		let mut obj: DynamicObject = match serde_yaml_ng::from_str(&edit.area.text) {
			Ok(o) => o,
			Err(e) => {
				edit.error = Some(format!("YAML: {e}"));
				return;
			},
		};
		if obj.metadata.name.as_deref() != target.obj.metadata.name.as_deref() {
			edit.error = Some("The name can't change; create a new object instead.".into());
			return;
		}
		// The version edited from: the server refuses the replace if it moved.
		obj.metadata.resource_version = Some(edit.rv.clone());
		if protection == Protection::ReadOnly {
			edit.error = Some(format!("{} is read-only", target.context));
			return;
		}
		let change = mutate::Mutation::Apply(Box::new(obj));
		if protection == Protection::Protected && !unlocked {
			self.dialog = Some(Dialog::Confirm { targets: vec![target], op: Op::Change(change), unlock: true });
			return;
		}
		let id = self.mutate(&target, change);
		if let Some(e) = self.detail.as_mut().and_then(|d| d.edit.as_mut()) {
			e.busy = Some(id);
			e.error = None;
		}
	}

	/// Handles a message.
	pub fn update(&mut self, msg: Msg) {
		if !matches!(msg, Msg::SelectOpen(..) | Msg::Pick(_) | Msg::EditField(..)) {
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
			Msg::ContextTag(tag) => {
				let with: Vec<String> = self.contexts.iter().filter(|c| self.config.tags(c).contains(&tag)).cloned().collect();
				if !with.is_empty() {
					self.active = with;
					self.namespace = None;
					self.activate_contexts();
				}
			},
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
			Msg::Pick(ids) => self.picked = ids,
			Msg::Bulk(act) => self.bulk(&act),
			Msg::CloseDetail => {
				self.record();
				self.close_detail();
			},
			Msg::Tab(tab) => self.set_tab(tab),
			Msg::Sort(col, dir) => {
				self.sort = (col, dir);
				self.stale = true;
			},
			Msg::Focus(f) => self.focus = f,
			Msg::ClearFilter => self.set_query(Line::default()),
			Msg::EditField(field, from, to, text, cursor) => {
				match field {
					Focus::Filter => {
						let mut q = self.query.clone();
						q.splice(from, to, &text, cursor);
						self.set_query(q);
					},
					Focus::LogFilter => {
						if let Some(d) = self.detail.as_mut() {
							d.logs.filter.splice(from, to, &text, cursor);
						}
					},
					Focus::Editor => {
						if let Some(e) = self.detail.as_mut().and_then(|d| d.edit.as_mut()) {
							e.area.splice(from, to, &text, cursor);
						}
					},
					Focus::List => {},
				}
				self.focus = field;
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
			Msg::Timestamps => {
				if let Some(d) = self.detail.as_mut() {
					d.logs.timestamps = !d.logs.timestamps;
				}
				self.start_logs();
			},
			Msg::Pause => {
				if let Some(l) = self.detail.as_mut().map(|d| &mut d.logs) {
					l.paused = if l.paused.is_some() { None } else { Some(l.text.len()) };
				}
			},
			Msg::Copy(text) => {
				self.copy = Some(text);
				self.info("Copied");
			},
			Msg::CopyLog => {
				if let Some(l) = self.detail.as_ref().map(|d| &d.logs) {
					self.copy = Some(strip_ansi(&l.shown()).into_owned());
					self.info("Copied the log");
				}
			},
			Msg::Reveal(key) => {
				let all = self.secret_keys();
				if let Some(d) = self.detail.as_mut() {
					match key {
						Some(k) => {
							if !d.revealed.remove(&k) {
								d.revealed.insert(k);
							}
						},
						None if d.revealed.len() == all.len() => d.revealed.clear(),
						None => d.revealed = all.into_iter().collect(),
					}
				}
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
			Msg::CompareContext(c) => {
				self.open = None;
				if let Some(Dialog::Compare { context, .. }) = &mut self.dialog {
					*context = c;
				}
			},
			Msg::StartForward(id) => self.start_forward(id),
			Msg::StartAllForwards => {
				let ids: Vec<u64> = self.forwards.iter().filter(|(_, f)| !f.running).map(|(id, _)| *id).collect();
				for id in ids {
					self.start_forward(id);
				}
			},
			Msg::StopForward(id) => self.stop_forward(id),
			Msg::RemoveForward(id) => {
				self.stop_forward(id);
				self.forwards.remove(&id);
			},
			Msg::CancelOp(id) => self.send(Cmd::Cancel(id)),
			Msg::Goto(g) => {
				self.dialog = None;
				self.goto(g);
			},
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
			Msg::SignIn(ctx) => {
				let t = actions::Target {
					context:   ctx,
					resource:  String::new(),
					namespace: None,
					name:      String::new(),
					container: None,
					is_pod:    false,
				};
				match actions::launch(actions::Action::SignIn, &t) {
					Ok(()) => self.info("Sign in in the terminal below, then press Reconnect"),
					Err(e) => self.error(e),
				}
			},
			Msg::Reconnect(ctx) => {
				for (k, s) in &mut self.stores {
					if k.context == ctx {
						s.error = None;
						s.auth = false;
						s.failing = None;
					}
				}
				self.metrics_on.retain(|c| *c != ctx);
				self.send(Cmd::Reconnect(ctx));
				self.sync_metrics();
			},
			Msg::Palette => {
				self.open = None;
				self.dialog = Some(Dialog::Palette { query: Line::default(), sel: 0 });
			},
			Msg::PaletteRun(i) => {
				if let Some(Dialog::Palette { sel, .. }) = &mut self.dialog {
					*sel = i;
				}
				self.confirm();
			},
			Msg::LabelColumn(key) => {
				let cols = self.label_columns.entry(self.kind.clone()).or_default();
				if let Some(pos) = cols.iter().position(|k| *k == key) {
					cols.remove(pos);
				} else {
					cols.push(key);
				}
				self.label_columns.retain(|_, v| !v.is_empty());
			},
			Msg::Columns => self.dialog = Some(Dialog::Columns),
			Msg::EditYaml => {
				if self.protection(self.target(None).map(|t| t.context).unwrap_or_default().as_str()) == Protection::ReadOnly {
					self.error("This context is read-only");
				} else {
					self.start_edit();
				}
			},
			Msg::Review(on) => {
				if let Some(e) = self.detail.as_mut().and_then(|d| d.edit.as_mut()) {
					e.review = on;
				}
				self.focus = if on { Focus::List } else { Focus::Editor };
			},
			Msg::ApplyYaml => self.apply_edit(),
			Msg::DiscardYaml => {
				if let Some(d) = self.detail.as_mut() {
					d.edit = None;
				}
				self.focus = Focus::List;
			},
			Msg::HelmRollback(id) => {
				let Some(target) = self.target(None) else { return };
				let rev = self
					.store(&target.context)
					.and_then(|s| s.rows.get(split_id(&id).1))
					.and_then(|r| match r.get("revision") {
						Val::Num(n, _) => Some(*n),
						_ => None,
					});
				if let Some(rev) = rev {
					let unlock = self.protection(&target.context) == Protection::Protected && !self.is_unlocked(&target.context);
					if self.protection(&target.context) == Protection::ReadOnly {
						self.error("This context is read-only");
					} else {
						self.dialog =
							Some(Dialog::Confirm { targets: vec![target], op: Op::Terminal(actions::Action::HelmRollback(rev)), unlock });
					}
				}
			},
		}
	}

	/// Keys of the open Secret's data.
	pub fn secret_keys(&self) -> Vec<String> {
		self.detail_obj()
			.and_then(|(_, o)| o.data.get("data")?.as_object().map(|m| m.keys().cloned().collect()))
			.unwrap_or_default()
	}

	/// Revisions of the open Helm release, newest first: (row id, row).
	pub fn helm_history(&self) -> Vec<(String, &Row)> {
		let Some(d) = &self.detail else { return vec![] };
		let (ctx, key) = split_id(&d.id);
		let Some(store) = self.store(ctx) else { return vec![] };
		let Some(row) = store.rows.get(key) else { return vec![] };
		let name = row.get("name").text(0);
		let mut out: Vec<(String, &Row)> = store
			.rows
			.iter()
			.filter(|(_, r)| r.namespace == row.namespace && r.get("name").text(0) == name)
			.map(|(k, r)| (row_id(ctx, k), r))
			.collect();
		out.sort_by(|a, b| b.1.get("revision").cmp(a.1.get("revision")));
		out
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
		if key.is("ctrl+k") || key.is("meta+k") {
			self.update(Msg::Palette);
			return true;
		}
		match self.focus {
			Focus::Filter => return self.filter_key(key),
			Focus::LogFilter => return self.log_filter_key(key),
			Focus::Editor => return self.editor_key(key),
			Focus::List => {},
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
		let kind = self.kind().clone();
		match key.name.as_str() {
			// Option+←/→ arrives as the arrow with Alt, or as Esc-b / Esc-f
			// (Alt+b / Alt+f) from terminals that send word motions.
			"left" | "b" if key.alt => self.update(Msg::Back),
			"right" | "f" if key.alt => self.update(Msg::Forward),
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
				} else if !self.picked.is_empty() {
					self.picked.clear();
				} else if self.detail.is_some() {
					self.update(Msg::CloseDetail);
				} else if !self.query.text.is_empty() {
					self.set_query(Line::default());
				} else {
					return false;
				}
			},
			":" => self.update(Msg::Palette),
			"/" => self.focus = Focus::Filter,
			"g" if !shift => self.g_pending = true,
			"tab" if shift => tab_step(self, -1),
			"tab" => tab_step(self, 1),
			"1" => self.set_tab(Tab::Overview),
			"2" | "y" => self.set_tab(Tab::Yaml),
			"3" => self.set_tab(Tab::Events),
			"4" | "l" if kind.has_logs() => self.act("logs", None),
			"space" if !self.picked.is_empty() || key.ctrl => {
				if let Some(id) = self.selected.clone() {
					if let Some(pos) = self.picked.iter().position(|p| *p == id) {
						self.picked.remove(pos);
					} else {
						self.picked.push(id);
					}
				}
			},
			"x" if !self.picked.is_empty() || key.ctrl => {},
			"s" if shift && kind.scalable() => self.act("scale", None),
			"r" if shift && kind.restartable() => self.act("restart", None),
			"s" if kind.is_pod() => self.act("shell", None),
			"d" if shift && kind.is_pod() => self.act("debug", None),
			"f" if kind.is_pod() || kind.projector == Projector::Service => self.act("forward", None),
			"t" if kind.projector == Projector::CronJob => self.act("trigger", None),
			"e" if shift => self.update(Msg::EditYaml),
			"e" => self.act("edit", None),
			"c" if shift => self.update(Msg::PickContexts),
			"=" => self.act("compare", None),
			"delete" | "backspace" if key.ctrl || key.meta || key.name == "delete" => {
				if self.picked.is_empty() {
					self.act("delete", None);
				} else {
					self.bulk("bulk-delete");
				}
			},
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
		if let Some(Dialog::Palette { query, sel }) = &mut self.dialog {
			match key.name.as_str() {
				"escape" => self.dialog = None,
				"enter" => self.confirm(),
				"down" => *sel += 1,
				"up" => *sel = sel.saturating_sub(1),
				_ => {
					if !query.key(key) {
						return false;
					}
					*sel = 0;
				},
			}
			if let Some(Dialog::Palette { query, sel }) = &self.dialog {
				let n = self.palette_items(&query.text).len();
				let clamped = (*sel).min(n.saturating_sub(1));
				if let Some(Dialog::Palette { sel, .. }) = &mut self.dialog {
					*sel = clamped;
				}
			}
			return true;
		}
		if let Some(Dialog::Compare { namespace, .. }) = &mut self.dialog
			&& !matches!(key.name.as_str(), "escape" | "enter")
		{
			return namespace.key(key);
		}
		match key.name.as_str() {
			"escape" => self.dialog = None,
			"enter" => {
				if matches!(self.dialog, Some(Dialog::Contexts | Dialog::Columns)) {
					self.dialog = None;
				} else {
					self.confirm();
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
		match key.name.as_str() {
			"escape" | "enter" | "down" | "tab" => {
				self.focus = Focus::List;
				if key.name == "escape" && !self.query.text.is_empty() {
					self.set_query(Line::default());
				}
				if key.name == "down" {
					self.move_selection(1);
				}
				true
			},
			_ => {
				let mut q = self.query.clone();
				let handled = q.key(key);
				if handled {
					self.set_query(q);
				}
				handled
			},
		}
	}

	fn log_filter_key(&mut self, key: &Key) -> bool {
		let Some(d) = self.detail.as_mut() else {
			self.focus = Focus::List;
			return false;
		};
		match key.name.as_str() {
			"escape" | "enter" | "tab" => {
				if key.name == "escape" {
					d.logs.filter = Line::default();
				}
				self.focus = Focus::List;
				true
			},
			_ => d.logs.filter.key(key),
		}
	}

	fn editor_key(&mut self, key: &Key) -> bool {
		if key.name == "escape" {
			self.focus = Focus::List;
			return true;
		}
		if key.is("ctrl+s") || key.is("meta+s") {
			self.update(Msg::Review(true));
			return true;
		}
		let Some(e) = self.detail.as_mut().and_then(|d| d.edit.as_mut()) else {
			self.focus = Focus::List;
			return false;
		};
		e.area.key(key)
	}

	/// The node id of the filter field (it must match the view's keys).
	pub const FILTER_ID: &'static str = "main.layout.body.bar.filter";
	/// The node id of the log filter field.
	pub const LOG_FILTER_ID: &'static str = "main.layout.detail.logs.logbar.find";
	/// The node id of the YAML editor.
	pub const EDITOR_ID: &'static str = "main.layout.detail.edit.area";
}

impl Target {
	/// A Helm secret's release name (its row shows it as the name).
	fn obj_release_name(&self) -> String {
		self.obj
			.metadata
			.labels
			.as_ref()
			.and_then(|l| l.get("name").cloned())
			.unwrap_or_else(|| self.name())
	}
}

fn port_entry(p: &Value, field: &str) -> Option<(u16, String)> {
	let n = u16::try_from(p.get(field)?.as_u64()?).ok()?;
	let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
	Some((n, if name.is_empty() { n.to_string() } else { format!("{n} ({name})") }))
}

fn saved_forward(s: &portforward::Spec) -> SavedForward {
	let (kind, name) = match &s.target {
		portforward::Target::Pod(n) => ("pod", n.clone()),
		portforward::Target::Service(n) => ("service", n.clone()),
	};
	SavedForward {
		context: s.context.clone(),
		namespace: s.namespace.clone(),
		kind: kind.into(),
		name,
		port: s.port,
		local: s.local,
	}
}

/// The label selector of a workload or service.
fn selector(kind: &Kind, obj: &DynamicObject) -> Vec<(String, String)> {
	let sel = if kind.projector == Projector::Service {
		obj.data.pointer("/spec/selector")
	} else {
		obj.data.pointer("/spec/selector/matchLabels")
	};
	sel.and_then(Value::as_object)
		.into_iter()
		.flatten()
		.filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
		.collect()
}

/// Container names of a pod spec, init containers last.
fn container_names(spec: &Value) -> Vec<String> {
	["containers", "initContainers"]
		.into_iter()
		.flat_map(|k| spec.get(k).and_then(Value::as_array).into_iter().flatten())
		.filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_owned))
		.collect()
}

/// The YAML users edit: with apiVersion and kind, without status and the
/// server-owned metadata.
pub fn editable_yaml(obj: &DynamicObject, ar: &kube::api::ApiResource) -> String {
	let mut o = obj.clone();
	o.types.get_or_insert_with(|| kube::api::TypeMeta { api_version: ar.api_version.clone(), kind: ar.kind.clone() });
	o.metadata.managed_fields = None;
	o.metadata.resource_version = None;
	o.metadata.uid = None;
	o.metadata.creation_timestamp = None;
	o.metadata.generation = None;
	if let Some(map) = o.data.as_object_mut() {
		map.remove("status");
	}
	serde_yaml_ng::to_string(&o).unwrap_or_else(|e| format!("# {e}"))
}

/// The YAML a comparison diffs: no status, no server-owned or per-cluster
/// metadata.
fn comparable(obj: &DynamicObject) -> String {
	let mut o = obj.clone();
	o.metadata.managed_fields = None;
	o.metadata.resource_version = None;
	o.metadata.uid = None;
	o.metadata.creation_timestamp = None;
	o.metadata.generation = None;
	o.metadata.owner_references = None;
	if let Some(a) = o.metadata.annotations.as_mut() {
		a.remove("kubectl.kubernetes.io/last-applied-configuration");
		a.remove("deployment.kubernetes.io/revision");
	}
	if let Some(map) = o.data.as_object_mut() {
		map.remove("status");
	}
	o.types = None;
	serde_yaml_ng::to_string(&o).unwrap_or_default()
}

/// A unified diff of two objects (empty when they match).
pub fn compare_yaml(a: &DynamicObject, b: &DynamicObject) -> String {
	let (a, b) = (comparable(a), comparable(b));
	similar::TextDiff::from_lines(&a, &b).unified_diff().context_radius(3).header("this", "other").to_string()
}

/// A unified diff of two texts.
pub fn diff_text(a: &str, b: &str) -> String {
	similar::TextDiff::from_lines(a, b).unified_diff().context_radius(3).header("server", "edited").to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

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
			"spec": {"containers": [{"name": "app", "resources": {"requests": {"cpu": "100m"}}}]},
			"status": {"phase": "Running"}
		}))
		.unwrap()
	}

	fn deploy(name: &str) -> DynamicObject {
		serde_json::from_value(serde_json::json!({
			"apiVersion": "apps/v1", "kind": "Deployment",
			"metadata": {"name": name, "namespace": "default", "uid": "D", "resourceVersion": "7"},
			"spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {"spec": {"containers": [{"name": "web"}]}}},
			"status": {"readyReplicas": 2}
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
		let config = Config { readonly: vec!["audit".into()], ..Config::default() };
		let mut app = App::new(ctxs.clone(), ctxs, &Saved::default(), config, tx);
		feed(&mut app, "eu-prod", "pods", pod("default", "api", serde_json::json!({})));
		feed(&mut app, "audit", "pods", pod("default", "api", serde_json::json!({})));
		app.update(Msg::Act("delete".into(), Some(row_id("eu-prod", "default/api"))));
		assert!(matches!(app.dialog, Some(Dialog::Confirm { unlock: true, .. })));
		app.update(Msg::Cancel);
		app.update(Msg::Act("delete".into(), Some(row_id("audit", "default/api"))));
		assert!(app.dialog.is_none());
		assert!(app.notice.as_ref().is_some_and(|n| n.error && n.text.contains("read-only")));
		// Bulk actions refuse when any picked row is read-only.
		app.update(Msg::Pick(vec![row_id("eu-prod", "default/api"), row_id("audit", "default/api")]));
		app.update(Msg::Bulk("bulk-delete".into()));
		assert!(app.dialog.is_none());
	}

	#[test]
	fn workloads_find_their_pods_and_sum_their_metrics() {
		let (mut app, _rx) = app();
		app.update(Msg::Kind("deployments.apps".into()));
		feed(&mut app, "a", "deployments.apps", deploy("api"));
		feed(&mut app, "a", "pods", pod("default", "api-1", serde_json::json!({"app": "api"})));
		feed(&mut app, "a", "pods", pod("default", "api-2", serde_json::json!({"app": "api"})));
		feed(&mut app, "a", "pods", pod("default", "other", serde_json::json!({"app": "db"})));
		feed(&mut app, "a", "pods", pod("staging", "api-3", serde_json::json!({"app": "api"})));
		app.update(Msg::Open(row_id("a", "default/api")));
		let kids: Vec<String> = app.child_pods().into_iter().map(|(id, _)| id).collect();
		assert_eq!(kids, [row_id("a", "default/api-1"), row_id("a", "default/api-2")]);
		assert_eq!(app.containers(), ["web"]);
		let mut snap = metrics::Snapshot::default();
		snap.pods.insert("default/api-1".into(), metrics::Usage { cpu: 10.0, mem: 1.0 });
		snap.pods.insert("default/api-2".into(), metrics::Usage { cpu: 30.0, mem: 2.0 });
		app.update_from(Update::Metrics("a".into(), Ok(snap)));
		let (row, _) = app.lookup(&row_id("a", "default/api")).unwrap();
		let (u, res) = app.usage("a", row).unwrap();
		assert_eq!((u.cpu, u.mem), (40.0, 3.0));
		assert_eq!(res.cpu_req, Some(200.0));
		assert!(app.columns().iter().any(|c| c.id == "cpu"));
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

	#[test]
	fn the_editor_applies_with_the_version_it_started_from() {
		let (mut app, mut rx) = app();
		app.update(Msg::Kind("deployments.apps".into()));
		feed(&mut app, "a", "deployments.apps", deploy("api"));
		app.update(Msg::Open(row_id("a", "default/api")));
		app.update(Msg::EditYaml);
		assert_eq!(app.focus, Focus::Editor);
		let e = app.detail.as_mut().and_then(|d| d.edit.as_mut()).unwrap();
		assert!(!e.area.text.contains("status") && !e.area.text.contains("resourceVersion"));
		e.area.text = e.area.text.replace("replicas: 2", "replicas: 5");
		app.update(Msg::ApplyYaml);
		let applied = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|c| match c {
			Cmd::Mutate { change: mutate::Mutation::Apply(o), .. } => Some(o),
			_ => None,
		});
		let o = applied.expect("an apply");
		assert_eq!(o.metadata.resource_version.as_deref(), Some("7"));
		assert_eq!(o.data.pointer("/spec/replicas"), Some(&serde_json::json!(5)));
	}

	#[test]
	fn helm_lists_the_newest_revision_of_each_release() {
		use base64::{Engine, engine::general_purpose::STANDARD};
		let (mut app, _rx) = app();
		app.update(Msg::Kind("helmreleases".into()));
		let secret = |rev: i64, status: &str| -> DynamicObject {
			let record = serde_json::json!({"name": "web", "version": rev, "info": {"status": status},
				"chart": {"metadata": {"name": "web", "version": "1.0.0"}}});
			let inner = STANDARD.encode(record.to_string());
			serde_json::from_value(serde_json::json!({
				"apiVersion": "v1", "kind": "Secret",
				"metadata": {"name": format!("sh.helm.release.v1.web.v{rev}"), "namespace": "apps", "labels": {"name": "web"}},
				"type": "helm.sh/release.v1",
				"data": {"release": STANDARD.encode(inner)}
			}))
			.unwrap()
		};
		feed(&mut app, "a", "helmreleases", secret(1, "superseded"));
		feed(&mut app, "a", "helmreleases", secret(2, "deployed"));
		let visible = app.visible().to_vec();
		assert_eq!(visible, [row_id("a", "apps/sh.helm.release.v1.web.v2")]);
		app.update(Msg::Open(visible[0].clone()));
		let revs: Vec<String> = app.helm_history().iter().map(|(_, r)| r.get("revision").text(0)).collect();
		assert_eq!(revs, ["2", "1"]);
	}

	#[test]
	fn log_filters_hide_lines_and_ignore_colors() {
		let logs = Logs {
			text: "\x1b[91mapi\x1b[0m GET /health\nERROR boom\nGET /users\n".into(),
			filter: Line::new("get !health"),
			..Logs::default()
		};
		assert_eq!(logs.shown(), "GET /users\n");
		let paused = Logs { paused: Some(11), ..Logs { text: "line one\nline two\n".into(), ..Logs::default() } };
		assert_eq!(paused.shown(), "line one\nli");
	}

	#[test]
	fn comparisons_ignore_status_and_server_metadata() {
		let a = deploy("api");
		let mut b = deploy("api");
		b.metadata.resource_version = Some("99".into());
		b.data["status"] = serde_json::json!({"readyReplicas": 0});
		assert_eq!(compare_yaml(&a, &b), "");
		b.data["spec"]["replicas"] = serde_json::json!(3);
		assert!(compare_yaml(&a, &b).contains("-  replicas: 2\n+  replicas: 3"));
	}
}
