//! The async side: one kube client per context, list+watch streams,
//! discovery, log streams, per-object event watches, metrics polls,
//! mutations and port forwards. It runs on tokio and reports to the UI
//! thread over a channel, waking its `session.next`.
//!
//! Commands never wait on the network in the command loop: clients are
//! created once per context inside the tasks that need them, so a slow
//! credential plugin in one context does not hold up another.

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
use tern_sdk::Waker;
use tokio::{
	sync::{OnceCell, mpsc as tmpsc},
	task::{JoinHandle, JoinSet},
};

use crate::{
	kinds::{self, Kind},
	metrics, mutate, portforward,
};

/// How often metrics are polled (metrics-server scrapes every 15 s).
const METRICS_EVERY: Duration = Duration::from_secs(15);
/// Most pods one aggregated log streams at once.
const MAX_LOG_STREAMS: usize = 12;

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
		key: WatchKey,
		/// The resource.
		ar:  ApiResource,
	},
	/// Stop a watch.
	Unwatch(WatchKey),
	/// Find custom resource kinds in a context.
	Discover(String),
	/// Stream container logs, replacing any other log stream. Several
	/// sources are merged, each line prefixed with its source.
	Logs {
		/// Identifies the stream in updates.
		id:        u64,
		/// Context.
		context:   String,
		/// Namespace.
		namespace: String,
		/// What to stream.
		sources:   Vec<LogSource>,
		/// The previous instances' logs.
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
	/// A mutation finished.
	Mutated(u64, Result<String, String>),
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

struct Backend {
	kubeconfig: Arc<Kubeconfig>,
	clients:    HashMap<String, ClientCell>,
	watches:    HashMap<WatchKey, JoinHandle<()>>,
	logs:       Option<JoinHandle<()>>,
	events:     Option<JoinHandle<()>>,
	metrics:    HashMap<String, JoinHandle<()>>,
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
		forwards: HashMap::new(),
		out,
	};
	while let Some(cmd) = rx.recv().await {
		b.handle(cmd);
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
				let opts = KubeConfigOptions { context: Some(context), ..KubeConfigOptions::default() };
				let config = Config::from_custom_kubeconfig((*kubeconfig).clone(), &opts)
					.await
					.map_err(|e| format!("kubeconfig: {e}"))?;
				Client::try_from(config).map_err(|e| format!("client: {e}"))
			})
			.await
			.clone()
		}
	}

	fn handle(&mut self, cmd: Cmd) {
		let out = self.out.clone();
		match cmd {
			Cmd::Watch { key, ar } => {
				if self.watches.get(&key).is_some_and(|h| !h.is_finished()) {
					return;
				}
				let client = self.client(&key.context);
				let k = key.clone();
				let task = tokio::spawn(async move {
					match client.await {
						Ok(c) => watch(c, ar, k, out).await,
						Err(message) => {
							out.send(Update::Watch(k, WatchEv::Error { message, forbidden: false }));
						},
					}
				});
				self.watches.insert(key, task);
			},
			Cmd::Unwatch(key) => abort(self.watches.remove(&key)),
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
			Cmd::Logs { id, context, namespace, sources, previous } => {
				abort(self.logs.take());
				let client = self.client(&context);
				self.logs = Some(tokio::spawn(async move {
					match client.await {
						Ok(c) => logs(c, namespace, sources, previous, id, out).await,
						Err(e) => {
							out.send(Update::LogEnded(id, Some(e)));
						},
					}
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
					let keep = contexts.contains(ctx);
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
				tokio::spawn(async move {
					let result = match client.await {
						Ok(c) => mutate::run(c, &obj, &change).await,
						Err(e) => Err(e),
					};
					out.send(Update::Mutated(id, result));
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

/// The colored `pod/container ` prefix of source `i` in a merged log.
pub fn log_prefix(i: usize, source: &LogSource) -> String {
	// Bright ANSI colors, skipping black and white.
	let color = 91 + (i % 6);
	match &source.container {
		Some(c) => format!("\x1b[{color}m{}/{c}\x1b[0m ", source.pod),
		None => format!("\x1b[{color}m{}\x1b[0m ", source.pod),
	}
}

async fn logs(client: Client, namespace: String, sources: Vec<LogSource>, previous: bool, id: u64, out: Outbox) {
	let merged = sources.len() > 1;
	let live = Arc::new(AtomicUsize::new(0));
	let mut set = JoinSet::new();
	for (i, src) in sources.into_iter().take(MAX_LOG_STREAMS).enumerate() {
		let prefix = merged.then(|| log_prefix(i, &src));
		let lp = LogParams {
			container: src.container.clone(),
			follow: !previous,
			previous,
			// A merged log starts from fewer lines per pod.
			tail_lines: Some(if merged { 200 } else { 1000 }),
			..LogParams::default()
		};
		let (api, out, live) = (Api::<Pod>::namespaced(client.clone(), &namespace), out.clone(), live.clone());
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn merged_logs_prefix_each_source_with_a_rotating_color() {
		let s = LogSource { pod: "api-1".into(), container: Some("app".into()) };
		assert_eq!(log_prefix(0, &s), "\x1b[91mapi-1/app\x1b[0m ");
		assert_eq!(log_prefix(6, &LogSource { container: None, ..s }), "\x1b[91mapi-1\x1b[0m ");
	}
}
