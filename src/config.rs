//! User configuration: change protection per context, as in Aptakube.
//! `~/.config/tern-kube/config.json`:
//!
//! ```json
//! { "protected": ["*prod*"], "readonly": ["audit-*"], "unprotected": ["dev-prod-sandbox"] }
//! ```
//!
//! Patterns are globs (`*` matches anything) over context names. Without a
//! file, contexts whose name contains `prod` are protected.

use std::path::PathBuf;

use serde::Deserialize;

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
}

impl Default for Config {
	fn default() -> Self {
		Self { protected: vec!["*prod*".into()], readonly: vec![], unprotected: vec![] }
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
		let c = Config { protected: vec!["*prod*".into()], readonly: vec!["*audit*".into()], unprotected: vec!["prod-sandbox".into()] };
		assert_eq!(c.protection("eu-prod"), Protection::Protected);
		assert_eq!(c.protection("prod-audit"), Protection::ReadOnly);
		assert_eq!(c.protection("prod-sandbox"), Protection::None);
		assert_eq!(c.protection("rancher-desktop"), Protection::None);
		assert_eq!(Config::default().protection("frontend-production"), Protection::Protected);
	}
}
