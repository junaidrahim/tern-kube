//! UI state, the messages that change it, and key handling. Everything here
//! runs on the main thread; the backend only talks to it through `Update`s.

use std::collections::{HashMap, HashSet, VecDeque};

use kube::api::DynamicObject;
use tern_sdk::{Key, ui::SortDir};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	actions,
	backend::{Cmd, Update, WatchEv, WatchKey},
	filter::Filter,
	kinds::{self, Kind, Projector},
	rows::{self, Row},
	state::Saved,
};

/// How many watches stay warm (Aptakube keeps its watchers too).
const WARM_WATCHES: usize = 8;
/// Log text kept before the oldest half is dropped.
const LOG_LIMIT: usize = 4 << 20;

/// A program message: a UI event mapped by a node's handler.
#[derive(Clone, Debug)]
pub enum Msg {
	/// Show a kind.
	Kind(String),
	/// Switch context.
	Context(String),
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
	/// Shell into the selected pod.
	Shell,
	/// `kubectl edit` the open object.
	Edit,
	/// `kubectl logs -f` the open pod in a terminal.
	FollowInTerminal,
	/// Show a container's log.
	Container(String),
	/// Toggle the previous container instance's log.
	Previous,
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
	/// Its pod's log.
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
	pub rows:    HashMap<String, Row>,
	/// Objects by key.
	pub objs:    HashMap<String, DynamicObject>,
	/// The first list is complete.
	pub synced:  bool,
	/// The last error, cleared by the next event.
	pub error:   Option<String>,
	/// Forbidden to list or watch.
	pub denied:  bool,
	/// Keys seen during a relist.
	relisting:   Option<HashSet<String>>,
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

/// The log of the open pod.
#[derive(Default)]
pub struct Logs {
	/// Stream id; updates for other ids are stale.
	pub id:        u64,
	/// The container shown.
	pub container: Option<String>,
	/// The previous instance's log.
	pub previous:  bool,
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
	/// The row key.
	pub key:     String,
	/// The active tab.
	pub tab:     Tab,
	/// YAML of a resource version.
	pub yaml:    Option<(String, String)>,
	/// Event watch id.
	pub events_id: u64,
	/// The object's events.
	pub events:  Store,
	/// The pod log.
	pub logs:    Logs,
}

/// Which field has the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
	/// The table.
	List,
	/// The filter field.
	Filter,
}

/// The whole UI state.
pub struct App {
	/// Kubeconfig contexts.
	pub contexts:   Vec<String>,
	/// The active context.
	pub context:    String,
	/// Namespace filter (`None`: all).
	pub namespace:  Option<String>,
	/// Built-in kinds.
	builtins:       Vec<Kind>,
	/// Discovered custom kinds per context.
	pub custom:     HashMap<String, Vec<Kind>>,
	/// Discovery errors per context.
	pub discovery:  HashMap<String, String>,
	/// The active kind id.
	pub kind:       String,
	/// Watch stores.
	pub stores:     HashMap<WatchKey, Store>,
	/// Most recently used watch first.
	lru:            VecDeque<WatchKey>,
	/// Filter text.
	pub query:      String,
	/// Caret in `query`, UTF-16.
	pub cursor:     usize,
	/// Compiled `query`.
	filter:         Filter,
	/// Keys' target.
	pub focus:      Focus,
	/// Sort column and direction.
	pub sort:       (String, SortDir),
	/// Selected row key.
	pub selected:   Option<String>,
	/// The open detail.
	pub detail:     Option<Detail>,
	/// Which select is open.
	pub open:       Option<Which>,
	/// A transient message for the dock.
	pub notice:     Option<String>,
	/// Visible row keys in order, rebuilt when stale.
	visible:        Vec<String>,
	/// `visible` needs a rebuild.
	stale:          bool,
	/// Id source for log and event streams.
	next_id:        u64,
	cmd:            UnboundedSender<Cmd>,
	/// Exit requested.
	pub quit:       bool,
}

impl App {
	/// A fresh app on `context`, restoring `saved`.
	pub fn new(contexts: Vec<String>, context: String, saved: &Saved, cmd: UnboundedSender<Cmd>) -> Self {
		let builtins = kinds::builtins();
		let kind = saved.kind.clone().filter(|k| builtins.iter().any(|b| &b.id == k)).unwrap_or_else(|| "pods".into());
		let mut app = Self {
			contexts,
			context,
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
			notice: None,
			visible: Vec::new(),
			stale: true,
			next_id: 0,
			cmd,
			quit: false,
		};
		app.activate_context();
		app
	}

	fn send(&self, cmd: Cmd) {
		// The backend lives as long as the runtime; a closed channel only
		// happens during shutdown.
		let _ = self.cmd.send(cmd);
	}

	/// What to save across runs.
	pub fn saved(&self) -> Saved {
		Saved { context: Some(self.context.clone()), kind: Some(self.kind.clone()), namespace: self.namespace.clone() }
	}

	/// Every kind of the active context, built-ins first.
	pub fn kinds(&self) -> impl Iterator<Item = &Kind> {
		self.builtins.iter().chain(self.custom.get(&self.context).into_iter().flatten())
	}

	/// The active kind.
	pub fn kind(&self) -> &Kind {
		self.kinds().find(|k| k.id == self.kind).unwrap_or(&self.builtins[0])
	}

	/// The watch behind the active kind.
	pub fn watch_key(&self) -> WatchKey {
		WatchKey { context: self.context.clone(), kind: self.kind.clone(), namespace: None }
	}

	/// The active store, if its watch started.
	pub fn store(&self) -> Option<&Store> {
		self.stores.get(&self.watch_key())
	}

	fn namespaces_key(&self) -> WatchKey {
		WatchKey { context: self.context.clone(), kind: "namespaces".into(), namespace: None }
	}

	/// Namespace names: from the namespaces watch, else from the active rows.
	pub fn namespaces(&self) -> Vec<String> {
		let mut names: Vec<String> = match self.stores.get(&self.namespaces_key()) {
			Some(s) if !s.rows.is_empty() => s.rows.keys().cloned().collect(),
			_ => {
				let set: HashSet<&String> =
					self.store().into_iter().flat_map(|s| s.rows.values()).filter_map(|r| r.namespace.as_ref()).collect();
				set.into_iter().cloned().collect()
			},
		};
		names.sort();
		names
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
		let pinned = self.namespaces_key();
		while self.lru.len() > WARM_WATCHES {
			let Some(pos) = self.lru.iter().rposition(|k| *k != pinned) else { break };
			let Some(old) = self.lru.remove(pos) else { break };
			self.stores.remove(&old);
			self.send(Cmd::Unwatch(old));
		}
	}

	fn activate_kind(&mut self) {
		let kind = self.kind().clone();
		self.watch(self.watch_key(), &kind);
		self.selected = None;
		self.close_detail();
		self.sort = (kind.projector.columns()[0].id.into(), SortDir::Asc);
		self.stale = true;
	}

	fn activate_context(&mut self) {
		if !self.custom.contains_key(&self.context) {
			self.send(Cmd::Discover(self.context.clone()));
		}
		if let Some(ns) = self.builtins.iter().find(|k| k.id == "namespaces").cloned() {
			self.watch(self.namespaces_key(), &ns);
		}
		self.activate_kind();
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
				let active = key == self.watch_key();
				if active {
					self.stale = true;
				}
				active || key == self.namespaces_key()
			},
			Update::Discovered(ctx, kinds) => {
				self.discovery.remove(&ctx);
				self.custom.insert(ctx.clone(), kinds);
				ctx == self.context
			},
			Update::DiscoveryFailed(ctx, err) => {
				self.discovery.insert(ctx.clone(), err);
				ctx == self.context
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
		}
	}

	/// The visible rows in order (filtered, sorted).
	pub fn visible(&mut self) -> &[String] {
		if self.stale {
			self.rebuild();
		}
		&self.visible
	}

	fn rebuild(&mut self) {
		self.stale = false;
		let Some(store) = self.stores.get(&self.watch_key()) else {
			self.visible.clear();
			return;
		};
		let ns = self.namespace.as_deref();
		let mut rows: Vec<&Row> = store
			.rows
			.values()
			.filter(|r| ns.is_none() || r.namespace.is_none() || r.namespace.as_deref() == ns)
			.filter(|r| self.filter.is_empty() || self.filter.matches(&r.haystack))
			.collect();
		let (col, dir) = (&self.sort.0, self.sort.1.clone());
		rows.sort_by(|a, b| {
			let ord = a.get(col).cmp(b.get(col)).then_with(|| crate::format::natural_cmp(&a.key, &b.key));
			if dir == SortDir::Desc { ord.reverse() } else { ord }
		});
		self.visible = rows.into_iter().map(|r| r.key.clone()).collect();
	}

	/// The open object.
	pub fn detail_obj(&self) -> Option<&DynamicObject> {
		let d = self.detail.as_ref()?;
		self.store()?.objs.get(&d.key)
	}

	fn open_detail(&mut self, key: String) {
		let Some(obj) = self.store().and_then(|s| s.objs.get(&key)) else { return };
		let uid = obj.metadata.uid.clone().unwrap_or_default();
		let namespace = obj.metadata.namespace.clone();
		let keep_tab = self.detail.as_ref().map(|d| d.tab).filter(|t| *t != Tab::Logs || self.kind().is_pod());
		self.close_detail();
		self.next_id += 1;
		let events_id = self.next_id;
		self.send(Cmd::Events { id: events_id, context: self.context.clone(), namespace, uid });
		self.selected = Some(key.clone());
		self.detail = Some(Detail {
			key,
			tab: keep_tab.unwrap_or(Tab::Overview),
			yaml: None,
			events_id,
			events: Store::default(),
			logs: Logs::default(),
		});
		if keep_tab == Some(Tab::Logs) {
			self.start_logs();
		}
	}

	/// Containers of the open pod (init containers last).
	pub fn containers(&self) -> Vec<String> {
		self.detail_obj().map_or_else(Vec::new, container_names)
	}

	fn start_logs(&mut self) {
		let container = self.detail.as_ref().and_then(|d| d.logs.container.clone()).or_else(|| self.containers().into_iter().next());
		let Some(obj) = self.detail_obj() else { return };
		let (namespace, pod) =
			(obj.metadata.namespace.clone().unwrap_or_default(), obj.metadata.name.clone().unwrap_or_default());
		self.next_id += 1;
		let id = self.next_id;
		let Some(d) = self.detail.as_mut() else { return };
		let previous = d.logs.previous;
		d.logs = Logs { id, container: container.clone(), previous, live: true, ..Logs::default() };
		self.send(Cmd::Logs { id, context: self.context.clone(), namespace, pod, container, previous });
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

	fn tabs(&self) -> Vec<Tab> {
		let mut t = vec![Tab::Overview, Tab::Yaml, Tab::Events];
		if self.kind().is_pod() {
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

	/// Handles a message.
	pub fn update(&mut self, msg: Msg) {
		self.notice = None;
		match msg {
			Msg::Kind(id) => {
				if id != self.kind {
					self.kind = id;
					self.activate_kind();
				}
			},
			Msg::Context(ctx) => {
				self.open = None;
				if ctx != self.context && self.contexts.contains(&ctx) {
					self.context = ctx;
					if !self.kinds().any(|k| k.id == self.kind) {
						self.kind = "pods".into();
					}
					self.namespace = None;
					self.activate_context();
				}
			},
			Msg::Namespace(ns) => {
				self.open = None;
				self.namespace = (!ns.is_empty()).then_some(ns);
				self.stale = true;
			},
			Msg::SelectOpen(which, open) => {
				self.open = if open { Some(which) } else { self.open.filter(|w| *w != which) };
			},
			Msg::Select(key) => {
				self.focus = Focus::List;
				if self.detail.is_some() {
					self.open_detail(key);
				} else {
					self.selected = Some(key);
				}
			},
			Msg::Open(key) => {
				self.focus = Focus::List;
				self.open_detail(key);
			},
			Msg::CloseDetail => self.close_detail(),
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
			Msg::Shell => self.run(actions::Action::Shell),
			Msg::Edit => self.run(actions::Action::Edit),
			Msg::FollowInTerminal => self.run(actions::Action::FollowLogs),
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
		}
	}

	/// The target of an action: the open object, else the selected row.
	fn target(&self) -> Option<actions::Target> {
		let key = self.detail.as_ref().map(|d| &d.key).or(self.selected.as_ref())?;
		let obj = self.store()?.objs.get(key)?;
		let container = self.detail.as_ref().and_then(|d| d.logs.container.clone());
		Some(actions::Target {
			context:   self.context.clone(),
			resource:  self.kind().id.clone(),
			namespace: obj.metadata.namespace.clone(),
			name:      obj.metadata.name.clone().unwrap_or_default(),
			container: container.or_else(|| container_names(obj).into_iter().next()),
			is_pod:    self.kind().is_pod(),
		})
	}

	fn run(&mut self, action: actions::Action) {
		let Some(target) = self.target() else {
			self.notice = Some("Select a row first".into());
			return;
		};
		if let Err(e) = actions::launch(action, &target) {
			self.notice = Some(e);
		}
	}

	fn move_selection(&mut self, delta: isize) {
		let current = self.selected.clone();
		let visible = self.visible();
		if visible.is_empty() {
			return;
		}
		let pos = current.and_then(|k| visible.iter().position(|v| *v == k));
		let next = match pos {
			None => if delta >= 0 { 0 } else { visible.len() - 1 },
			Some(p) => p.saturating_add_signed(delta).min(visible.len() - 1),
		};
		let key = visible[next].clone();
		self.update(Msg::Select(key));
	}

	/// Handles a key; false when it did nothing.
	pub fn key(&mut self, key: &Key) -> bool {
		if key.is("ctrl+c") {
			self.quit = true;
			return true;
		}
		if self.focus == Focus::Filter {
			return self.filter_key(key);
		}
		let tabs = self.tabs();
		let tab_step = |app: &mut Self, step: isize| {
			if let Some(d) = &app.detail {
				let i = tabs.iter().position(|t| *t == d.tab).unwrap_or(0) as isize;
				let n = tabs.len() as isize;
				app.set_tab(tabs[((i + step).rem_euclid(n)) as usize]);
			}
		};
		match key.name.as_str() {
			"down" | "j" if !key.ctrl => self.move_selection(1),
			"up" | "k" if !key.ctrl => self.move_selection(-1),
			"page_down" => self.move_selection(20),
			"page_up" => self.move_selection(-20),
			"home" => self.move_selection(isize::MIN / 2),
			"end" => self.move_selection(isize::MAX / 2),
			"enter" => {
				if let Some(k) = self.selected.clone() {
					self.open_detail(k);
				}
			},
			"escape" => {
				if self.open.is_some() {
					self.open = None;
				} else if self.detail.is_some() {
					self.close_detail();
				} else if !self.query.is_empty() {
					self.set_query(String::new(), 0);
				} else {
					return false;
				}
			},
			"/" => self.focus = Focus::Filter,
			"tab" if key.shift => tab_step(self, -1),
			"tab" => tab_step(self, 1),
			"1" => self.set_tab(Tab::Overview),
			"2" | "y" => self.set_tab(Tab::Yaml),
			"3" => self.set_tab(Tab::Events),
			"4" | "l" if self.kind().is_pod() => {
				if self.detail.is_none()
					&& let Some(k) = self.selected.clone()
				{
					self.open_detail(k);
				}
				self.set_tab(Tab::Logs);
			},
			"s" if self.kind().is_pod() => self.run(actions::Action::Shell),
			"e" => self.run(actions::Action::Edit),
			"[" | "]" => {
				let ids: Vec<String> = self.kinds().map(|k| k.id.clone()).collect();
				let i = ids.iter().position(|k| *k == self.kind).unwrap_or(0) as isize;
				let step = if key.name == "]" { 1 } else { -1 };
				let id = ids[(i + step).rem_euclid(ids.len() as isize) as usize].clone();
				self.update(Msg::Kind(id));
			},
			"q" => self.quit = true,
			_ => return false,
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

/// A pod's container names, init containers last.
fn container_names(obj: &DynamicObject) -> Vec<String> {
	["containers", "initContainers"]
		.into_iter()
		.flat_map(|k| rows::at(&obj.data, &["spec", k]).and_then(|v| v.as_array()).into_iter().flatten())
		.filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(str::to_owned))
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
}
