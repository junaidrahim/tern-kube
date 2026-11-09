//! The list filter, Aptakube's grammar: words separated by spaces must all
//! match (`api prod`), `a|b` matches either, `!x` excludes, `*` is a wildcard
//! (`kube*exporter`), and `/re/` is a case-insensitive regular expression.
//! Matching ignores case.

use regex::{Regex, RegexBuilder};

/// A compiled filter.
#[derive(Debug, Default)]
pub struct Filter {
	/// Every clause must hold.
	clauses: Vec<Clause>,
}

#[derive(Debug)]
struct Clause {
	negate: bool,
	/// Any alternative may match.
	any:    Vec<Pattern>,
}

#[derive(Debug)]
enum Pattern {
	Sub(String),
	Re(Regex),
}

impl Pattern {
	fn matches(&self, hay: &str) -> bool {
		match self {
			Self::Sub(s) => hay.contains(s.as_str()),
			Self::Re(r) => r.is_match(hay),
		}
	}
}

/// Regexes run in multi-line mode: a row's haystack is its fields joined by
/// newlines, so `^` and `$` anchor to one field.
fn regex(src: &str) -> Option<Regex> {
	RegexBuilder::new(src).case_insensitive(true).multi_line(true).size_limit(1 << 20).build().ok()
}

fn glob(word: &str) -> Pattern {
	if !word.contains('*') {
		return Pattern::Sub(word.to_owned());
	}
	let src = word.split('*').map(regex::escape).collect::<Vec<_>>().join(".*");
	regex(&src).map_or_else(|| Pattern::Sub(word.replace('*', "")), Pattern::Re)
}

impl Filter {
	/// Parses `query`. An invalid `/re/` falls back to matching its text.
	pub fn parse(query: &str) -> Self {
		let q = query.trim();
		if q.len() >= 2 && q.starts_with('/') && q.ends_with('/') {
			let src = &q[1..q.len() - 1];
			let pat = regex(src).map_or_else(|| Pattern::Sub(src.to_lowercase()), Pattern::Re);
			return Self { clauses: vec![Clause { negate: false, any: vec![pat] }] };
		}
		let clauses = q
			.split_whitespace()
			.filter_map(|word| {
				let word = word.to_lowercase();
				let (negate, body) = word.strip_prefix('!').map_or((false, word.as_str()), |b| (true, b));
				let any: Vec<Pattern> = body.split('|').filter(|a| !a.is_empty()).map(glob).collect();
				(!any.is_empty()).then_some(Clause { negate, any })
			})
			.collect();
		Self { clauses }
	}

	/// Whether nothing is filtered.
	pub const fn is_empty(&self) -> bool {
		self.clauses.is_empty()
	}

	/// Whether `hay` (already lowercase) passes.
	pub fn matches(&self, hay: &str) -> bool {
		self.clauses.iter().all(|c| c.any.iter().any(|p| p.matches(hay)) != c.negate)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn m(q: &str, hay: &str) -> bool {
		Filter::parse(q).matches(hay)
	}

	#[test]
	fn words_are_anded_and_case_insensitive() {
		assert!(m("API Prod", "api-7d9f prod running"));
		assert!(!m("api staging", "api-7d9f prod running"));
		assert!(m("", "anything"));
	}

	#[test]
	fn pipes_or_bangs_negate_stars_glob() {
		assert!(m("worker|webapp", "webapp-1"));
		assert!(!m("!completed", "job-1 completed"));
		assert!(m("!completed", "job-1 running"));
		assert!(m("kube*exporter", "kube-state-metrics-exporter"));
		assert!(!m("kube*exporter", "exporter-kube"));
		assert!(m("!a|b", "c"));
		assert!(!m("!a|b", "b"));
	}

	#[test]
	fn slashes_make_a_regex_and_bad_regex_matches_text() {
		assert!(m(r"/^api-\d+$/", "api-12"));
		assert!(!m(r"/^api-\d+$/", "api-x"));
		assert!(m("/(/", "a(b"));
	}
}
