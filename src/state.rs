//! Preferences kept between runs: the shown contexts, kind, namespace, the
//! open object, saved port forwards and chosen label columns. Written on
//! every change, so a crash or reboot loses nothing.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// A port forward to start again.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedForward {
	/// Context.
	pub context:   String,
	/// Namespace.
	pub namespace: String,
	/// `pod` or `service`.
	pub kind:      String,
	/// Name.
	pub name:      String,
	/// Target port.
	pub port:      u16,
	/// Local port.
	pub local:     u16,
}

/// What is saved.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Saved {
	/// Last primary context.
	pub context:       Option<String>,
	/// Every context shown (the primary first).
	pub contexts:      Vec<String>,
	/// Last kind id.
	pub kind:          Option<String>,
	/// Last namespace filter.
	pub namespace:     Option<String>,
	/// The open object's row id.
	pub detail:        Option<String>,
	/// Port forwards.
	pub forwards:      Vec<SavedForward>,
	/// Label keys shown as columns, by kind id.
	pub label_columns: BTreeMap<String, Vec<String>>,
}

fn path() -> Option<PathBuf> {
	let base = std::env::var_os("XDG_STATE_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
	Some(base.join("tern-kube/state.json"))
}

impl Saved {
	/// The saved state, or the default when there is none.
	pub fn load() -> Self {
		path()
			.and_then(|p| std::fs::read(p).ok())
			.and_then(|b| serde_json::from_slice(&b).ok())
			.unwrap_or_default()
	}

	/// Writes the state; failures are ignored (it is only a convenience).
	pub fn store(&self) {
		let Some(p) = path() else { return };
		if let Some(dir) = p.parent() {
			let _ = std::fs::create_dir_all(dir);
		}
		if let Ok(json) = serde_json::to_vec_pretty(self) {
			let tmp = p.with_extension("tmp");
			if std::fs::write(&tmp, json).is_ok() {
				let _ = std::fs::rename(tmp, p);
			}
		}
	}
}
