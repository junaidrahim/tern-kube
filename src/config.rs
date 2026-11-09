//! User configuration, `~/.config/tern-kube/config.json`:
//!
//! ```json
//! {
//!   "protected": ["*prod*"], "readonly": ["audit-*"], "unprotected": ["dev-prod-sandbox"],
//!   "contexts": { "gke_x_us-east4_frontend-production": { "alias": "frontend prod", "color": "red", "tags": ["prod", "gcp"] } }
//! }
//! ```
//!
//! Protection patterns are globs (`*` matches anything) over context names.
//! Without a file, contexts whose name contains `prod` are protected.
//! Aliases replace context names on screen only; commands always use the
//! real name.

use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;
use tern_sdk::ui::Tone;

/// How much a context allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
	/// Changes run after a confirmation.
	None,
	/// Changes need an unlock first (it lasts 15 minutes).
	Protected,
	/// No changes, no shells.
	ReadOnly,
}

/// How a context looks.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextLook {
	/// Shown instead of the name.
	pub alias: Option<String>,
	/// `red`, `orange`, `yellow`, `green`, `blue`, `purple` or `gray`.
	pub color: Option<String>,
	/// Tags; the context picker toggles every context of a tag at once.
	pub tags:  Vec<String>,
}

/// The parsed config file.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
	/// Protected context patterns.
	pub protected:   Vec<String>,
	/// Read-only context patterns.
	pub readonly:    Vec<String>,
	/// Exceptions to both.
	pub unprotected: Vec<String>,
	/// Per-context aliases, colors and tags.
	pub contexts:    BTreeMap<String, ContextLook>,
}

impl Default for Config {
	fn default() -> Self {
		Self { protected: vec!["*prod*".into()], readonly: vec![], unprotected: vec![], contexts: BTreeMap::new() }
	}
}

fn path() -> Option<PathBuf> {
	let base = std::env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
	Some(base.join("tern-kube/config.json"))
}

/// Whether `name` matches glob `pat` (only `*` is special; case-insensitive).
pub fn glob(pat: &str, name: &str) -> bool {
	let (pat, name) = (pat.to_lowercase(), name.to_lowercase());
	let parts: Vec<&str> = pat.split('*').collect();
	if parts.len() == 1 {
		return pat == name;
	}
	let mut rest = name.as_str();
	for (i, part) in parts.iter().enumerate() {
		if i == 0 {
			let Some(r) = rest.strip_prefix(part) else { return false };
			rest = r;
		} else if i == parts.len() - 1 {
			return rest.ends_with(part);
		} else {
			let Some(at) = rest.find(part) else { return false };
			rest = &rest[at + part.len()..];
		}
	}
	true
}

impl Config {
	/// The config file, or the default when there is none or it is invalid.
	pub fn load() -> Self {
		path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
	}

	/// The protection of `context`.
	pub fn protection(&self, context: &str) -> Protection {
		let any = |pats: &[String]| pats.iter().any(|p| glob(p, context));
		if any(&self.unprotected) {
			Protection::None
		} else if any(&self.readonly) {
			Protection::ReadOnly
		} else if any(&self.protected) {
			Protection::Protected
		} else {
			Protection::None
		}
	}

	/// What `context` is called on screen (its alias, else its name).
	pub fn display<'a>(&'a self, context: &'a str) -> &'a str {
		self.contexts.get(context).and_then(|l| l.alias.as_deref()).unwrap_or(context)
	}

	/// The tone of `context`'s color, if it has one.
	pub fn tone(&self, context: &str) -> Option<Tone> {
		Some(match self.contexts.get(context)?.color.as_deref()? {
			"red" => Tone::Error,
			"orange" | "yellow" => Tone::Warning,
			"green" => Tone::Success,
			"blue" => Tone::Info,
			"purple" => Tone::Accent,
			"gray" | "grey" => Tone::Muted,
			_ => return None,
		})
	}

	/// The tags of `context`.
	pub fn tags(&self, context: &str) -> &[String] {
		self.contexts.get(context).map_or(&[], |l| l.tags.as_slice())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn globs_anchor_both_ends_unless_starred() {
		assert!(glob("*prod*", "gke_x_frontend-production-us"));
		assert!(glob("prod-*", "PROD-eu"));
		assert!(!glob("prod-*", "my-prod-eu"));
		assert!(glob("a*c", "abbc"));
		assert!(!glob("a*c", "abcd"));
		assert!(glob("exact", "exact"));
	}

	#[test]
	fn exceptions_beat_readonly_beats_protected() {
		let c = Config {
			protected: vec!["*prod*".into()],
			readonly: vec!["*audit*".into()],
			unprotected: vec!["prod-sandbox".into()],
			..Config::default()
		};
		assert_eq!(c.protection("eu-prod"), Protection::Protected);
		assert_eq!(c.protection("prod-audit"), Protection::ReadOnly);
		assert_eq!(c.protection("prod-sandbox"), Protection::None);
		assert_eq!(c.protection("rancher-desktop"), Protection::None);
		assert_eq!(Config::default().protection("frontend-production"), Protection::Protected);
	}
}
