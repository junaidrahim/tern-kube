//! tern-kube: a native Kubernetes browser for Tern.
//!
//! The main thread owns the Tern session and the UI state; a tokio runtime
//! runs the kube watches and wakes the main thread when updates arrive.
//! Renders are batched so a burst of watch events costs one frame.

mod actions;
mod app;
mod backend;
mod config;
mod decode;
mod filter;
mod format;
mod health;
mod kinds;
mod metrics;
mod mutate;
mod portforward;
mod rows;
mod state;
mod textedit;
mod view;

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use kube::config::Kubeconfig;
use tern_sdk::{Font, Input, Options, Session, SurfaceOptions};

use crate::{app::App, config::Config, state::Saved};

const USAGE: &str = "usage: tern-kube [--context NAME]... [--namespace NAME]

A native Kubernetes browser. Run it in a Tern pane. Give --context more
than once to merge several clusters into one view.";

struct Args {
	contexts:  Vec<String>,
	namespace: Option<String>,
}

fn args() -> Result<Args> {
	let mut out = Args { contexts: Vec::new(), namespace: None };
	let mut it = std::env::args().skip(1);
	while let Some(a) = it.next() {
		match a.as_str() {
			"--context" => out.contexts.push(it.next().context("--context needs a value")?),
			"--namespace" | "-n" => out.namespace = Some(it.next().context("--namespace needs a value")?),
			"-h" | "--help" => {
				println!("{USAGE}");
				std::process::exit(0);
			},
			other => bail!("unknown argument {other:?}\n{USAGE}"),
		}
	}
	Ok(out)
}

/// The quiet time a render waits for after a watch update, by row count:
/// a resend of every row costs more the bigger the table.
fn batch_window(rows: usize) -> Duration {
	Duration::from_millis((16 + rows as u64 / 10).min(500))
}

fn unix_now() -> i64 {
	jiff::Timestamp::now().as_second()
}

fn main() -> Result<()> {
	let args = args()?;
	let kubeconfig = Kubeconfig::read().context("reading kubeconfig")?;
	let contexts: Vec<String> = kubeconfig.contexts.iter().map(|c| c.name.clone()).collect();
	if contexts.is_empty() {
		bail!("the kubeconfig has no contexts");
	}
	let mut saved = Saved::load();
	if args.namespace.is_some() {
		saved.namespace = args.namespace.clone();
	}
	// Contexts shown: the arguments, else the last run's, else the current one.
	let mut active: Vec<String> = if args.contexts.is_empty() {
		let mut last = saved.contexts.clone();
		if last.is_empty() {
			last.extend(saved.context.clone());
		}
		last
	} else {
		args.contexts
	};
	active.retain(|c| contexts.contains(c));
	active.dedup();
	if active.is_empty() {
		active.push(
			kubeconfig.current_context.clone().filter(|c| contexts.contains(c)).unwrap_or_else(|| contexts[0].clone()),
		);
	}

	let Some(mut session) = Session::<app::Msg>::connect(Options::new().app("tern-kube"))? else {
		bail!("tern-kube draws a native UI: run it in a Tern pane");
	};
	let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
	let (cmd, updates) = backend::start(&rt, kubeconfig, session.waker());
	let mut app = App::new(contexts, active, &saved, Config::load(), cmd);
	let sf = session.open(
		SurfaceOptions::screen().title("Kubernetes").role("tern-kube").font(Font::Sans).closable(true),
	)?;
	session.stylesheet(sf, "tern-kube", Some(include_str!("kube.css")))?;

	session.render(sf, view::view(&mut app, unix_now()))?;
	let mut last_render = Instant::now();
	let mut dirty = false;
	let mut focus_sent: Option<&'static str> = None;
	let mut kind_shown = app.kind.clone();
	let mut saved_last = app.saved();
	let tick = Duration::from_secs(5);
	loop {
		// Drain backend updates first: one render covers all of them.
		while let Ok(u) = updates.try_recv() {
			dirty |= app.update_from(u);
		}
		let window = batch_window(app.total());
		let due = last_render + if dirty { window } else { tick };
		let wait = due.saturating_duration_since(Instant::now());
		let mut immediate = false;
		match session.next(Some(wait))? {
			None => {},
			Some(Input::Msg(msg, _)) => {
				app.update(msg);
				immediate = true;
			},
			Some(Input::Key(key)) => immediate = app.key(&key),
			Some(Input::Event(_) | Input::Clipboard(_)) => {},
		}
		if app.quit {
			break;
		}
		while let Ok(u) = updates.try_recv() {
			dirty |= app.update_from(u);
		}
		let now = Instant::now();
		// Input renders at once; updates wait for the batch window; ages
		// refresh on the tick.
		if immediate || (dirty && now >= last_render + window) || now >= last_render + tick {
			session.render(sf, view::view(&mut app, unix_now()))?;
			last_render = now;
			dirty = false;
			let focus = focus_target(&app);
			if focus != focus_sent {
				focus_sent = focus;
				session.focus(sf, focus)?;
			}
			if let Some(text) = app.copy.take() {
				session.write_raw(osc52(&text).as_bytes())?;
			}
			// Save on every change, so a crash or reboot restores this view.
			let saved = app.saved();
			if saved != saved_last {
				saved.store();
				saved_last = saved;
			}
			if app.kind != kind_shown {
				kind_shown = app.kind.clone();
				session.reveal(sf, &view::sidebar_item_id(app.kind()), tern_sdk::wire::RevealAt::Nearest)?;
			}
		}
	}
	app.saved().store();
	session.close()?;
	rt.shutdown_timeout(Duration::from_millis(200));
	Ok(())
}

/// The node that should have the keys' caret.
fn focus_target(app: &App) -> Option<&'static str> {
	match &app.dialog {
		Some(app::Dialog::Palette { .. }) => return Some("layer.palette.body.q"),
		Some(app::Dialog::Compare { .. }) => return Some("layer.cmp.body.ns"),
		Some(_) => return None,
		None => {},
	}
	match app.focus {
		app::Focus::List => None,
		app::Focus::Filter => Some(App::FILTER_ID),
		app::Focus::LogFilter => Some(App::LOG_FILTER_ID),
		app::Focus::Editor => Some(App::EDITOR_ID),
	}
}

/// OSC 52: put `text` on the system clipboard.
fn osc52(text: &str) -> String {
	use base64::{Engine, engine::general_purpose::STANDARD};
	format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}
