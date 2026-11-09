//! The async side: one kube client per context, list+watch streams,
//! discovery, log streams and per-object event watches. It runs on tokio and
//! reports to the UI thread over a channel, waking its `session.next`.

use std::{collections::HashMap, sync::mpsc, time::Duration};

use futures::{AsyncBufReadExt, StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{
	Client, Config,
	api::{Api, ApiResource, DynamicObject, LogParams},
	config::{KubeConfigOptions, Kubeconfig},
	discovery::Discovery,
	runtime::{WatchStreamExt, watcher},
};
use tern_sdk::Waker;
use tokio::{sync::mpsc as tmpsc, task::JoinHandle};

use crate::kinds::{self, Kind};

/// What a watch covers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WatchKey {
	/// The kubeconfig context.
	pub context:   String,
	/// The kind id.
	pub kind:      String,
	/// One namespace, or all of them.
	pub namespace: Option<String>,
}

/// A request from the UI.
#[derive(Debug)]
pub enum Cmd {
	/// Start listing and watching (no-op when already running).
	Watch {
		/// What to watch.
		key:        WatchKey,
		/// The resource.
		ar:         ApiResource,
	},
	/// Stop a watch.
	Unwatch(WatchKey),
	/// Find custom resource kinds in a context.
	Discover(String),
	/// Stream a container's log, replacing any other log stream.
	Logs {
		/// Identifies the stream in updates.
		id:        u64,
		/// Context.
		context:   String,
		/// Namespace.
		namespace: String,
		/// Pod.
		pod:       String,
		/// Container (the default one when `None`).
		container: Option<String>,
		/// The previous instance's log.
		previous:  bool,
	},
	/// Stop the log stream.
	StopLogs,
	/// Watch the events of one object, replacing any other.
	Events {
		/// Identifies the watch in updates.
		id:        u64,
		/// Context.
		context:   String,
		/// The object's namespace (events of cluster objects live in `default`).
		namespace: Option<String>,
		/// The object's uid.
		uid:       String,
	},
	/// Stop the event watch.
	StopEvents,
}

/// What a watch reports.
#[derive(Debug)]
pub enum WatchEv {
	/// A (re)list starts: objects not re-listed by `InitDone` are gone.
	Init,
	/// An object of the list.
	InitApply(DynamicObject),
	/// The list is complete; live updates follow.
	InitDone,
	/// Added or changed.
	Apply(DynamicObject),
	/// Deleted.
	Delete(DynamicObject),
	/// The stream failed (it retries with backoff).
	Error {
		/// The message.
		message:   String,
		/// HTTP 403: the account may not list or watch this.
		forbidden: bool,
	},
}

/// A report to the UI.
#[derive(Debug)]
pub enum Update {
	/// A watch event.
	Watch(WatchKey, WatchEv),
	/// Custom resource kinds of a context.
	Discovered(String, Vec<Kind>),
	/// Discovery failed.
	DiscoveryFailed(String, String),
	/// Log lines, in order.
	LogLines(u64, Vec<String>),
	/// The log stream ended (an error message when it failed).
	LogEnded(u64, Option<String>),
	/// An event-watch event.
	Events(u64, WatchEv),
}

/// The UI's end of the update channel.
pub struct Outbox {
	tx:    mpsc::Sender<Update>,
	waker: Option<Waker>,
}

impl Clone for Outbox {
	fn clone(&self) -> Self {
		Self { tx: self.tx.clone(), waker: self.waker.clone() }
	}
}

impl Outbox {
	/// Sends `u` and wakes the UI. False once the UI is gone.
	fn send(&self, u: Update) -> bool {
		let ok = self.tx.send(u).is_ok();
		if let Some(w) = &self.waker {
			w.wake();
		}
		ok
	}
}

/// Starts the backend on `rt`; returns the command sender and update receiver.
pub fn start(
	rt: &tokio::runtime::Runtime,
	kubeconfig: Kubeconfig,
	waker: Option<Waker>,
) -> (tmpsc::UnboundedSender<Cmd>, mpsc::Receiver<Update>) {
	let (cmd_tx, cmd_rx) = tmpsc::unbounded_channel();
	let (tx, rx) = mpsc::channel();
	let out = Outbox { tx, waker };
	rt.spawn(run(kubeconfig, cmd_rx, out));
	(cmd_tx, rx)
}

struct Backend {
	kubeconfig: Kubeconfig,
	clients:    HashMap<String, Client>,
	watches:    HashMap<WatchKey, JoinHandle<()>>,
	logs:       Option<JoinHandle<()>>,
	events:     Option<JoinHandle<()>>,
	out:        Outbox,
}

async fn run(kubeconfig: Kubeconfig, mut rx: tmpsc::UnboundedReceiver<Cmd>, out: Outbox) {
	let mut b = Backend {
		kubeconfig,
		clients: HashMap::new(),
		watches: HashMap::new(),
		logs: None,
		events: None,
		out,
	};
	while let Some(cmd) = rx.recv().await {
		b.handle(cmd).await;
	}
}

fn describe(e: &kube::Error) -> (String, bool) {
	match e {
		kube::Error::Api(s) => (format!("{} ({})", s.message, s.code), s.code == 403),
		other => (other.to_string(), false),
	}
}

fn describe_watch(e: &watcher::Error) -> (String, bool) {
	match e {
		watcher::Error::InitialListFailed(e) | watcher::Error::WatchStartFailed(e) | watcher::Error::WatchFailed(e) => {
			describe(e)
		},
		watcher::Error::WatchError(s) => (format!("{} ({})", s.message, s.code), s.code == 403),
		watcher::Error::NoResourceVersion => (e.to_string(), false),
	}
}

impl Backend {
	async fn client(&mut self, context: &str) -> Result<Client, String> {
		if let Some(c) = self.clients.get(context) {
			return Ok(c.clone());
		}
		let opts = KubeConfigOptions { context: Some(context.to_owned()), ..KubeConfigOptions::default() };
		let config = Config::from_custom_kubeconfig(self.kubeconfig.clone(), &opts)
			.await
			.map_err(|e| format!("kubeconfig: {e}"))?;
		let client = Client::try_from(config).map_err(|e| format!("client: {e}"))?;
		self.clients.insert(context.to_owned(), client.clone());
		Ok(client)
	}

	async fn handle(&mut self, cmd: Cmd) {
		match cmd {
			Cmd::Watch { key, ar } => {
				if self.watches.get(&key).is_some_and(|h| !h.is_finished()) {
					return;
				}
				match self.client(&key.context).await {
					Ok(client) => {
						let task = tokio::spawn(watch(client, ar, key.clone(), self.out.clone()));
						self.watches.insert(key, task);
					},
					Err(message) => {
						self.out.send(Update::Watch(key, WatchEv::Error { message, forbidden: false }));
					},
				}
			},
			Cmd::Unwatch(key) => {
				if let Some(h) = self.watches.remove(&key) {
					h.abort();
				}
			},
			Cmd::Discover(context) => match self.client(&context).await {
				Ok(client) => {
					let out = self.out.clone();
					tokio::spawn(async move {
						match discover(client).await {
							Ok(kinds) => out.send(Update::Discovered(context, kinds)),
							Err(e) => out.send(Update::DiscoveryFailed(context, describe(&e).0)),
						};
					});
				},
				Err(e) => {
					self.out.send(Update::DiscoveryFailed(context, e));
				},
			},
			Cmd::Logs { id, context, namespace, pod, container, previous } => {
				if let Some(h) = self.logs.take() {
					h.abort();
				}
				match self.client(&context).await {
					Ok(client) => {
						let lp = LogParams {
							container,
							follow: !previous,
							previous,
							tail_lines: Some(1000),
							..LogParams::default()
						};
						self.logs = Some(tokio::spawn(logs(client, namespace, pod, lp, id, self.out.clone())));
					},
					Err(e) => {
						self.out.send(Update::LogEnded(id, Some(e)));
					},
				}
			},
			Cmd::StopLogs => {
				if let Some(h) = self.logs.take() {
					h.abort();
				}
			},
			Cmd::Events { id, context, namespace, uid } => {
				if let Some(h) = self.events.take() {
					h.abort();
				}
				if let Ok(client) = self.client(&context).await {
					self.events = Some(tokio::spawn(events(client, namespace, uid, id, self.out.clone())));
				}
			},
			Cmd::StopEvents => {
				if let Some(h) = self.events.take() {
					h.abort();
				}
			},
		}
	}
}

/// The watcher config every list uses: the first list is served from the
/// apiserver's watch cache (`resourceVersion=0`), the cheapest list there is.
fn config() -> watcher::Config {
	watcher::Config::default().any_semantic()
}

async fn pump(
	stream: impl futures::Stream<Item = Result<watcher::Event<DynamicObject>, watcher::Error>>,
	emit: impl Fn(WatchEv) -> bool,
) {
	let mut stream = std::pin::pin!(stream);
	while let Some(item) = stream.next().await {
		let ev = match item {
			Ok(watcher::Event::Init) => WatchEv::Init,
			Ok(watcher::Event::InitApply(o)) => WatchEv::InitApply(o),
			Ok(watcher::Event::InitDone) => WatchEv::InitDone,
			Ok(watcher::Event::Apply(o)) => WatchEv::Apply(o),
			Ok(watcher::Event::Delete(o)) => WatchEv::Delete(o),
			Err(e) => {
				let (message, forbidden) = describe_watch(&e);
				WatchEv::Error { message, forbidden }
			},
		};
		if !emit(ev) {
			return;
		}
	}
}

async fn watch(client: Client, ar: ApiResource, key: WatchKey, out: Outbox) {
	let api: Api<DynamicObject> = match &key.namespace {
		Some(ns) => Api::namespaced_with(client, ns, &ar),
		None => Api::all_with(client, &ar),
	};
	let stream = watcher(api, config()).default_backoff();
	pump(stream, |ev| out.send(Update::Watch(key.clone(), ev))).await;
}

async fn events(client: Client, namespace: Option<String>, uid: String, id: u64, out: Outbox) {
	let ar = kinds::builtins().into_iter().find(|k| k.id == "events").map(|k| k.ar).expect("events kind");
	let ns = namespace.unwrap_or_else(|| "default".to_owned());
	let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &ar);
	let stream = watcher(api, config().fields(&format!("involvedObject.uid={uid}"))).default_backoff();
	pump(stream, |ev| out.send(Update::Events(id, ev))).await;
}

async fn logs(client: Client, namespace: String, pod: String, lp: LogParams, id: u64, out: Outbox) {
	let api: Api<Pod> = Api::namespaced(client, &namespace);
	let reader = match api.log_stream(&pod, &lp).await {
		Ok(r) => r,
		Err(e) => {
			out.send(Update::LogEnded(id, Some(describe(&e).0)));
			return;
		},
	};
	let mut lines = reader.lines();
	let mut batch = Vec::new();
	loop {
		// Gather what arrives within 40 ms into one update.
		let next = if batch.is_empty() {
			lines.try_next().await.map(Some)
		} else {
			match tokio::time::timeout(Duration::from_millis(40), lines.try_next()).await {
				Ok(r) => r.map(Some),
				Err(_) => Ok(None),
			}
		};
		match next {
			Ok(Some(Some(line))) => {
				batch.push(line);
				if batch.len() < 2000 {
					continue;
				}
			},
			Ok(Some(None)) => {
				if !batch.is_empty() {
					out.send(Update::LogLines(id, std::mem::take(&mut batch)));
				}
				out.send(Update::LogEnded(id, None));
				return;
			},
			Ok(None) => {},
			Err(e) => {
				out.send(Update::LogEnded(id, Some(e.to_string())));
				return;
			},
		}
		if !batch.is_empty() && !out.send(Update::LogLines(id, std::mem::take(&mut batch))) {
			return;
		}
	}
}

async fn discover(client: Client) -> Result<Vec<Kind>, kube::Error> {
	let discovery = Discovery::new(client).run_aggregated().await?;
	let mut kinds: Vec<Kind> = discovery
		.groups()
		.filter(|g| !kinds::is_builtin_group(g.name()))
		.flat_map(|g| g.recommended_resources())
		.filter(|(_, caps)| caps.operations.iter().any(|o| o == "watch") && caps.operations.iter().any(|o| o == "list"))
		.map(|(ar, caps)| kinds::custom(ar, &caps.scope))
		.collect();
	kinds.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.ar.group.cmp(&b.ar.group)));
	Ok(kinds)
}
