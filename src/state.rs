//! Preferences kept between runs: the last context, kind and namespace.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What is saved.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Saved {
	/// Last primary context.
	pub context:   Option<String>,
	/// Every context shown (the primary first).
	pub contexts:  Vec<String>,
	/// Last kind id.
	pub kind:      Option<String>,
	/// Last namespace filter.
	pub namespace: Option<String>,
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
