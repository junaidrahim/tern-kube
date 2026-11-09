//! The async side: one kube client per context, list+watch streams,
//! discovery, log streams, per-object event watches, metrics polls,
//! mutations (with progress and cancel), object fetches and port forwards.
//! It runs on tokio and reports to the UI thread over a channel, waking its
//! `session.next`.
//!
//! Commands never wait on the network in the command loop: clients are
//! created once per context inside the tasks that need them, so a slow
//! credential plugin in one context does not hold up another. `Reconnect`
//! drops a context's client (after a sign-in) and restarts its watches.

use std::{
	collections::HashMap,
	future::Future,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
		mpsc,
	},
	time::Duration,
};

use futures::{AsyncBufReadExt, StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{
	Client, Config,
	api::{Api, ApiResource, DynamicObject, LogParams},
	config::{KubeConfigOptions, Kubeconfig},
	discovery::Discovery,
	runtime::{WatchStreamExt, watcher},
};
use serde_json::Value;
use tern_sdk::Waker;
use tokio::{
	sync::{OnceCell, mpsc as tmpsc},
	task::{JoinHandle, JoinSet},
};

use crate::{
	kinds::{self, Kind, PrinterCol},
	metrics, mutate, portforward,
};

/// How often metrics are polled (metrics-server scrapes every 15 s).
const METRICS_EVERY: Duration = Duration::from_secs(15);
/// Most pods one merged log streams at once.
const MAX_LOG_STREAMS: usize = 16;

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

/// One container log to stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSource {
	/// Context.
	pub context:   String,
	/// Namespace.
	pub namespace: String,
	/// Pod.
	pub pod:       String,
	/// Container (the default one when `None`).
	pub container: Option<String>,
}

/// A request from the UI.
#[derive(Debug)]
pub enum Cmd {
	/// Start listing and watching (no-op when already running).
	Watch {
		/// What to watch.
		key:    WatchKey,
		/// The resource.
		ar:     ApiResource,
		/// A field selector.
		fields: Option<String>,
	},
	/// Stop a watch.
	Unwatch(WatchKey),
	/// Forget a context's client (after a sign-in) and restart its watches.
	Reconnect(String),
	/// Find custom resource kinds in a context.
	Discover(String),
	/// Stream container logs, replacing any other log stream. Several
	/// sources (pods of a workload, picked pods, even across contexts) are
	/// merged, each line prefixed with its source.
	Logs {
		/// Identifies the stream in updates.
		id:         u64,
		/// What to stream.
		sources:    Vec<LogSource>,
		/// The previous instances' logs.
		previous:   bool,
		/// Ask the server for timestamps.
		timestamps: bool,
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
	/// Poll metrics of exactly these contexts.
	Metrics(Vec<String>),
	/// Change the cluster.
	Mutate {
		/// Identifies the result.
		id:      u64,
		/// Context.
		context: String,
		/// The object.
		obj:     mutate::Object,
		/// The change.
		change:  mutate::Mutation,
	},
	/// Stop a running change (a drain).
	Cancel(u64),
	/// Fetch one object (for a comparison).
	Get {
		/// Identifies the result.
		id:      u64,
		/// Context.
		context: String,
		/// The object.
		obj:     mutate::Object,
	},
	/// Start a port forward.
	Forward(portforward::Spec),
	/// Stop a port forward.
	StopForward(u64),
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
		/// The credentials failed (401, or the exec credential plugin).
		auth:      bool,
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
	/// A metrics snapshot of a context, or why there is none.
	Metrics(String, Result<metrics::Snapshot, String>),
	/// A running change's progress.
	Progress(u64, String),
	/// A mutation finished.
	Mutated(u64, Result<String, String>),
	/// A fetched object.
	Got(u64, Result<Box<DynamicObject>, String>),
	/// A port forward reported.
	Forward(u64, portforward::Report),
}

/// The UI's end of the update channel.
#[derive(Clone)]
pub struct Outbox {
	tx:    mpsc::Sender<Update>,
	waker: Option<Waker>,
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

type ClientCell = Arc<OnceCell<Result<Client, String>>>;

/// A running watch and what restarts it.
struct Running {
	task:   JoinHandle<()>,
	ar:     ApiResource,
	fields: Option<String>,
}

struct Backend {
	kubeconfig: Arc<Kubeconfig>,
	clients:    HashMap<String, ClientCell>,
	watches:    HashMap<WatchKey, Running>,
	logs:       Option<JoinHandle<()>>,
	events:     Option<JoinHandle<()>>,
	metrics:    HashMap<String, JoinHandle<()>>,
	changes:    HashMap<u64, JoinHandle<()>>,
	forwards:   HashMap<u64, JoinHandle<()>>,
	out:        Outbox,
}

async fn run(kubeconfig: Kubeconfig, mut rx: tmpsc::UnboundedReceiver<Cmd>, out: Outbox) {
	let mut b = Backend {
		kubeconfig: Arc::new(kubeconfig),
		clients: HashMap::new(),
		watches: HashMap::new(),
		logs: None,
		events: None,
		metrics: HashMap::new(),
		changes: HashMap::new(),
		forwards: HashMap::new(),
		out,
	};
	while let Some(cmd) = rx.recv().await {
		b.handle(cmd);
	}
}

/// Whether an error message says the credentials failed: an exec credential
/// plugin that could not get a token, an expired SSO session, or a 401.
pub fn is_auth_error(message: &str) -> bool {
	let m = message.to_lowercase();
	["unauthorized", "(401)", "exec", "credential", "token", "expired", "sso", "login", "authenticat"]
		.iter()
		.any(|w| m.contains(w))
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

fn abort(h: Option<JoinHandle<()>>) {
	if let Some(h) = h {
		h.abort();
	}
}

impl Backend {
	/// The client of `context`, created on first use by whoever awaits it.
	fn client(&mut self, context: &str) -> impl Future<Output = Result<Client, String>> + Send + 'static {
		let cell = self.clients.entry(context.to_owned()).or_default().clone();
		let kubeconfig = self.kubeconfig.clone();
		let context = context.to_owned();
		async move {
			cell.get_or_init(|| async move {
				// Re-read the kubeconfig: a sign-in may have rewritten it.
				let kc = Kubeconfig::read().unwrap_or_else(|_| (*kubeconfig).clone());
				let opts = KubeConfigOptions { context: Some(context), ..KubeConfigOptions::default() };
				let config = Config::from_custom_kubeconfig(kc, &opts).await.map_err(|e| format!("kubeconfig: {e}"))?;
				Client::try_from(config).map_err(|e| format!("client: {e}"))
			})
			.await
			.clone()
		}
	}

	fn start_watch(&mut self, key: WatchKey, ar: ApiResource, fields: Option<String>) {
		let client = self.client(&key.context);
		let (k, a, f, out) = (key.clone(), ar.clone(), fields.clone(), self.out.clone());
		let task = tokio::spawn(async move {
			match client.await {
				Ok(c) => watch(c, a, f, k, out).await,
				Err(message) => {
					let auth = is_auth_error(&message);
					out.send(Update::Watch(k, WatchEv::Error { message, forbidden: false, auth }));
				},
			}
		});
		if let Some(old) = self.watches.insert(key, Running { task, ar, fields }) {
			old.task.abort();
		}
	}

	fn handle(&mut self, cmd: Cmd) {
		let out = self.out.clone();
		match cmd {
			Cmd::Watch { key, ar, fields } => {
				if self.watches.get(&key).is_some_and(|r| !r.task.is_finished()) {
					return;
				}
				self.start_watch(key, ar, fields);
			},
			Cmd::Unwatch(key) => abort(self.watches.remove(&key).map(|r| r.task)),
			Cmd::Reconnect(context) => {
				self.clients.remove(&context);
				let keys: Vec<(WatchKey, ApiResource, Option<String>)> = self
					.watches
					.iter()
					.filter(|(k, _)| k.context == context)
					.map(|(k, r)| (k.clone(), r.ar.clone(), r.fields.clone()))
					.collect();
				for (k, ar, fields) in keys {
					self.start_watch(k, ar, fields);
				}
				if let Some(h) = self.metrics.remove(&context) {
					h.abort();
				}
			},
			Cmd::Discover(context) => {
				let client = self.client(&context);
				tokio::spawn(async move {
					let result = match client.await {
						Ok(c) => discover(c).await.map_err(|e| describe(&e).0),
						Err(e) => Err(e),
					};
					match result {
						Ok(kinds) => out.send(Update::Discovered(context, kinds)),
						Err(e) => out.send(Update::DiscoveryFailed(context, e)),
					};
				});
			},
			Cmd::Logs { id, sources, previous, timestamps } => {
				abort(self.logs.take());
				let mut contexts: Vec<String> = sources.iter().map(|s| s.context.clone()).collect();
				contexts.sort();
				contexts.dedup();
				let mut clients = Vec::with_capacity(contexts.len());
				for c in &contexts {
					clients.push((c.clone(), self.client(c)));
				}
				self.logs = Some(tokio::spawn(async move {
					let mut ready = HashMap::new();
					for (c, fut) in clients {
						match fut.await {
							Ok(cl) => {
								ready.insert(c, cl);
							},
							Err(e) => {
								out.send(Update::LogEnded(id, Some(format!("{c}: {e}"))));
								return;
							},
						}
					}
					logs(&ready, sources, previous, timestamps, id, out).await;
				}));
			},
			Cmd::StopLogs => abort(self.logs.take()),
			Cmd::Events { id, context, namespace, uid } => {
				abort(self.events.take());
				let client = self.client(&context);
				self.events = Some(tokio::spawn(async move {
					if let Ok(c) = client.await {
						events(c, namespace, uid, id, out).await;
					}
				}));
			},
			Cmd::StopEvents => abort(self.events.take()),
			Cmd::Metrics(contexts) => {
				self.metrics.retain(|ctx, h| {
					let keep = contexts.contains(ctx) && !h.is_finished();
					if !keep {
						h.abort();
					}
					keep
				});
				for ctx in contexts {
					if self.metrics.contains_key(&ctx) {
						continue;
					}
					let client = self.client(&ctx);
					let (out, c) = (out.clone(), ctx.clone());
					let task = tokio::spawn(async move {
						let client = match client.await {
							Ok(cl) => cl,
							Err(e) => {
								out.send(Update::Metrics(c, Err(e)));
								return;
							},
						};
						loop {
							let snap = metrics::fetch(&client).await;
							let unavailable = snap.is_err();
							if !out.send(Update::Metrics(c.clone(), snap)) {
								return;
							}
							// Without a Metrics API, check again only now and then.
							tokio::time::sleep(if unavailable { METRICS_EVERY * 8 } else { METRICS_EVERY }).await;
						}
					});
					self.metrics.insert(ctx, task);
				}
			},
			Cmd::Mutate { id, context, obj, change } => {
				let client = self.client(&context);
				let task = tokio::spawn(async move {
					let progress = {
						let out = out.clone();
						move |text: String| {
							out.send(Update::Progress(id, text));
						}
					};
					let result = match client.await {
						Ok(c) => mutate::run(c, &obj, &change, &progress).await,
						Err(e) => Err(e),
					};
					out.send(Update::Mutated(id, result));
				});
				self.changes.retain(|_, h| !h.is_finished());
				self.changes.insert(id, task);
			},
			Cmd::Cancel(id) => {
				if let Some(h) = self.changes.remove(&id) {
					h.abort();
					out.send(Update::Mutated(id, Err("Cancelled".into())));
				}
			},
			Cmd::Get { id, context, obj } => {
				let client = self.client(&context);
				tokio::spawn(async move {
					let result = match client.await {
						Ok(c) => {
							let api: Api<DynamicObject> = match &obj.namespace {
								Some(ns) => Api::namespaced_with(c, ns, &obj.ar),
								None => Api::all_with(c, &obj.ar),
							};
							api.get(&obj.name).await.map(Box::new).map_err(|e| describe(&e).0)
						},
						Err(e) => Err(e),
					};
					out.send(Update::Got(id, result));
				});
			},
			Cmd::Forward(spec) => {
				let client = self.client(&spec.context);
				let id = spec.id;
				let task = tokio::spawn(async move {
					match client.await {
						Ok(c) => {
							portforward::run(c, spec, move |r| {
								out.send(Update::Forward(id, r));
							})
							.await;
						},
						Err(e) => {
							out.send(Update::Forward(id, portforward::Report::Dead(e)));
						},
					}
				});
				abort(self.forwards.insert(id, task));
			},
			Cmd::StopForward(id) => abort(self.forwards.remove(&id)),
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
				let auth = !forbidden && is_auth_error(&message);
				WatchEv::Error { message, forbidden, auth }
			},
		};
		if !emit(ev) {
			return;
		}
	}
}

async fn watch(client: Client, ar: ApiResource, fields: Option<String>, key: WatchKey, out: Outbox) {
	let api: Api<DynamicObject> = match &key.namespace {
		Some(ns) => Api::namespaced_with(client, ns, &ar),
		None => Api::all_with(client, &ar),
	};
	let mut cfg = config();
	if let Some(f) = &fields {
		cfg = cfg.fields(f);
	}
	let stream = watcher(api, cfg).default_backoff();
	pump(stream, |ev| out.send(Update::Watch(key.clone(), ev))).await;
}

async fn events(client: Client, namespace: Option<String>, uid: String, id: u64, out: Outbox) {
	let ar = kinds::builtins().into_iter().find(|k| k.id == "events").map(|k| k.ar).expect("events kind");
	let ns = namespace.unwrap_or_else(|| "default".to_owned());
	let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &ar);
	let stream = watcher(api, config().fields(&format!("involvedObject.uid={uid}"))).default_backoff();
	pump(stream, |ev| out.send(Update::Events(id, ev))).await;
}

/// The colored `pod/container ` prefix of source `i` in a merged log.
pub fn log_prefix(i: usize, source: &LogSource) -> String {
	// Bright ANSI colors, skipping black and white.
	let color = 91 + (i % 6);
	match &source.container {
		Some(c) => format!("\x1b[{color}m{}/{c}\x1b[0m ", source.pod),
		None => format!("\x1b[{color}m{}\x1b[0m ", source.pod),
	}
}

async fn logs(
	clients: &HashMap<String, Client>,
	sources: Vec<LogSource>,
	previous: bool,
	timestamps: bool,
	id: u64,
	out: Outbox,
) {
	let merged = sources.len() > 1;
	let live = Arc::new(AtomicUsize::new(0));
	let mut set = JoinSet::new();
	for (i, src) in sources.into_iter().take(MAX_LOG_STREAMS).enumerate() {
		let Some(client) = clients.get(&src.context) else { continue };
		let prefix = merged.then(|| log_prefix(i, &src));
		let lp = LogParams {
			container: src.container.clone(),
			follow: !previous,
			previous,
			timestamps,
			// A merged log starts from fewer lines per pod.
			tail_lines: Some(if merged { 200 } else { 1000 }),
			..LogParams::default()
		};
		let (api, out, live) = (Api::<Pod>::namespaced(client.clone(), &src.namespace), out.clone(), live.clone());
		live.fetch_add(1, Ordering::Relaxed);
		set.spawn(async move {
			let err = stream_one(&api, &src.pod, &lp, prefix.as_deref(), id, &out).await;
			(live.fetch_sub(1, Ordering::Relaxed) == 1, err)
		});
	}
	let mut last_err = None;
	while let Some(done) = set.join_next().await {
		let Ok((was_last, err)) = done else { continue };
		if err.is_some() {
			last_err = err;
		}
		if was_last {
			out.send(Update::LogEnded(id, last_err.take()));
		}
	}
}

/// Streams one container log; returns an error message when it failed.
async fn stream_one(
	api: &Api<Pod>,
	pod: &str,
	lp: &LogParams,
	prefix: Option<&str>,
	id: u64,
	out: &Outbox,
) -> Option<String> {
	let reader = match api.log_stream(pod, lp).await {
		Ok(r) => r,
		Err(e) => return Some(format!("{pod}: {}", describe(&e).0)),
	};
	let mut lines = reader.lines();
	let mut batch = Vec::new();
	let tag = |line: String| match prefix {
		Some(p) => format!("{p}{line}"),
		None => line,
	};
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
				batch.push(tag(line));
				if batch.len() < 2000 {
					continue;
				}
			},
			Ok(Some(None)) => {
				if !batch.is_empty() {
					out.send(Update::LogLines(id, std::mem::take(&mut batch)));
				}
				return None;
			},
			Ok(None) => {},
			Err(e) => return Some(e.to_string()),
		}
		if !batch.is_empty() && !out.send(Update::LogLines(id, std::mem::take(&mut batch))) {
			return None;
		}
	}
}

/// Printer columns of each CRD, by (group, plural): the served storage
/// version's priority-0 columns.
async fn printer_columns(client: &Client) -> HashMap<(String, String), Vec<PrinterCol>> {
	let crd_ar = ApiResource::from_gvk_with_plural(
		&kube::api::GroupVersionKind::gvk("apiextensions.k8s.io", "v1", "CustomResourceDefinition"),
		"customresourcedefinitions",
	);
	let api: Api<DynamicObject> = Api::all_with(client.clone(), &crd_ar);
	let Ok(list) = api.list(&kube::api::ListParams::default()).await else { return HashMap::new() };
	list.items.iter().map(|crd| (crd_key(&crd.data), crd_printer(&crd.data))).collect()
}

fn crd_key(d: &Value) -> (String, String) {
	let s = |p: &str| d.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
	(s("/spec/group"), s("/spec/names/plural"))
}

/// The printer columns of a CRD (`obj.data`): its storage version's,
/// priority 0, at most eight, without the Age column (always shown).
pub fn crd_printer(d: &Value) -> Vec<PrinterCol> {
	let versions = d.pointer("/spec/versions").and_then(Value::as_array).cloned().unwrap_or_default();
	let version = versions
		.iter()
		.find(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
		.or_else(|| versions.first());
	version
		.and_then(|v| v.get("additionalPrinterColumns"))
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(|c| {
			let name = c.get("name")?.as_str()?.to_owned();
			let path = c.get("jsonPath")?.as_str()?.to_owned();
			let priority = c.get("priority").and_then(Value::as_i64).unwrap_or(0);
			let kind = c.get("type").and_then(Value::as_str).unwrap_or("string").to_owned();
			(priority == 0 && !(name == "Age" && path == ".metadata.creationTimestamp"))
				.then_some(PrinterCol { name, path, kind, priority })
		})
		.take(kinds::PRINTER_IDS.len())
		.collect()
}

async fn discover(client: Client) -> Result<Vec<Kind>, kube::Error> {
	let (discovery, printers) = tokio::join!(Discovery::new(client.clone()).run_aggregated(), printer_columns(&client));
	let discovery = discovery?;
	let mut kinds: Vec<Kind> = discovery
		.groups()
		.filter(|g| !kinds::is_builtin_group(g.name()))
		.flat_map(|g| g.recommended_resources())
		.filter(|(_, caps)| caps.operations.iter().any(|o| o == "watch") && caps.operations.iter().any(|o| o == "list"))
		.map(|(ar, caps)| {
			let printer = printers.get(&(ar.group.clone(), ar.plural.clone())).cloned().unwrap_or_default();
			kinds::custom(ar, &caps.scope, printer)
		})
		.collect();
	kinds.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.ar.group.cmp(&b.ar.group)));
	Ok(kinds)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn merged_logs_prefix_each_source_with_a_rotating_color() {
		let s = LogSource {
			context:   "c".into(),
			namespace: "n".into(),
			pod:       "api-1".into(),
			container: Some("app".into()),
		};
		assert_eq!(log_prefix(0, &s), "\x1b[91mapi-1/app\x1b[0m ");
		assert_eq!(log_prefix(6, &LogSource { container: None, ..s }), "\x1b[91mapi-1\x1b[0m ");
	}

	#[test]
	fn crd_printer_columns_come_from_the_storage_version() {
		let crd = serde_json::json!({"spec": {"group": "x.io", "names": {"plural": "widgets"}, "versions": [
			{"name": "v1alpha1", "storage": false, "additionalPrinterColumns": [{"name": "Old", "jsonPath": ".a"}]},
			{"name": "v1", "storage": true, "additionalPrinterColumns": [
				{"name": "Ready", "type": "string", "jsonPath": ".status.ready"},
				{"name": "Wide", "type": "string", "jsonPath": ".x", "priority": 1},
				{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}
			]}
		]}});
		let cols = crd_printer(&crd);
		assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Ready"]);
		assert_eq!(crd_key(&crd), ("x.io".to_owned(), "widgets".to_owned()));
	}

	#[test]
	fn auth_failures_are_told_from_other_errors() {
		assert!(is_auth_error("exec: executable tsh failed with exit code 1"));
		assert!(is_auth_error("Unauthorized (401)"));
		assert!(!is_auth_error("connection refused"));
	}
}
