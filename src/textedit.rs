//! Text fields the program owns (Tern sends keys, not edits): single-line
//! fields with a UTF-16 caret, as `input` nodes expect, and the multi-line
//! YAML editor with a byte caret.

use tern_sdk::Key;

/// UTF-16 length of `s`.
pub fn utf16_len(s: &str) -> usize {
	s.encode_utf16().count()
}

/// The byte offset of UTF-16 offset `u` in `s`.
pub fn byte_at(s: &str, u: usize) -> usize {
	let mut n = 0;
	for (i, c) in s.char_indices() {
		if n >= u {
			return i;
		}
		n += c.len_utf16();
	}
	s.len()
}

/// The UTF-16 offset of byte offset `b` in `s`.
pub fn utf16_at(s: &str, b: usize) -> usize {
	s[..b.min(s.len())].encode_utf16().count()
}

/// `s` with UTF-16 range `[from, to)` replaced by `text`.
pub fn splice_utf16(s: &str, from: usize, to: usize, text: &str) -> String {
	let (a, b) = (byte_at(s, from.min(to)), byte_at(s, to.max(from)));
	let mut out = String::with_capacity(s.len() + text.len());
	out.push_str(&s[..a]);
	out.push_str(text);
	out.push_str(&s[b..]);
	out
}

/// A single-line field: its text and UTF-16 caret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
	/// The text.
	pub text:   String,
	/// The caret, UTF-16.
	pub cursor: usize,
}

impl Line {
	/// A field holding `text` with the caret at its end.
	pub fn new(text: impl Into<String>) -> Self {
		let text = text.into();
		Self { cursor: utf16_len(&text), text }
	}

	/// Replaces UTF-16 `[from, to)` with `text` and puts the caret at `cursor`
	/// (a native edit).
	pub fn splice(&mut self, from: usize, to: usize, text: &str, cursor: usize) {
		self.text = splice_utf16(&self.text, from, to, text);
		self.cursor = cursor.min(utf16_len(&self.text));
	}

	fn insert(&mut self, t: &str) {
		let c = self.cursor;
		self.splice(c, c, t, c + utf16_len(t));
	}

	/// Applies an editing key; `false` when it is not one (Enter, Esc, arrows
	/// up and down, Tab are left to the caller).
	pub fn key(&mut self, key: &Key) -> bool {
		let len = utf16_len(&self.text);
		match key.name.as_str() {
			"backspace" if self.cursor > 0 => {
				let c = self.cursor;
				self.splice(c - 1, c, "", c - 1);
			},
			"backspace" => {},
			"delete" if self.cursor < len => {
				let c = self.cursor;
				self.splice(c, c + 1, "", c);
			},
			"delete" => {},
			"left" => self.cursor = self.cursor.saturating_sub(1),
			"right" => self.cursor = (self.cursor + 1).min(len),
			"home" => self.cursor = 0,
			"end" => self.cursor = len,
			"u" if key.ctrl => *self = Self::default(),
			"paste" => {
				let t = key.text.clone().unwrap_or_default().replace(['\n', '\r'], " ");
				self.insert(&t);
			},
			_ => {
				let Some(t) = key.typed() else { return false };
				let t = t.to_owned();
				self.insert(&t);
			},
		}
		true
	}
}

/// The multi-line editor: text and a byte caret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Area {
	/// The text.
	pub text:   String,
	/// The caret, bytes (always on a char boundary).
	pub cursor: usize,
	/// Column wanted by up/down, in chars.
	goal:       Option<usize>,
}

impl Area {
	/// An editor holding `text` with the caret at the start.
	pub fn new(text: impl Into<String>) -> Self {
		Self { text: text.into(), cursor: 0, goal: None }
	}

	/// The caret in UTF-16, for the view.
	pub fn cursor16(&self) -> usize {
		utf16_at(&self.text, self.cursor)
	}

	fn line_start(&self, at: usize) -> usize {
		self.text[..at].rfind('\n').map_or(0, |i| i + 1)
	}

	fn line_end(&self, at: usize) -> usize {
		self.text[at..].find('\n').map_or(self.text.len(), |i| at + i)
	}

	fn col(&self) -> usize {
		self.text[self.line_start(self.cursor)..self.cursor].chars().count()
	}

	fn at_col(&self, start: usize, col: usize) -> usize {
		let end = self.line_end(start);
		self.text[start..end].char_indices().nth(col).map_or(end, |(i, _)| start + i)
	}

	fn insert(&mut self, t: &str) {
		self.text.insert_str(self.cursor, t);
		self.cursor += t.len();
		self.goal = None;
	}

	/// Replaces UTF-16 `[from, to)` with `text`, caret at UTF-16 `cursor`.
	pub fn splice(&mut self, from: usize, to: usize, text: &str, cursor: usize) {
		self.text = splice_utf16(&self.text, from, to, text);
		self.cursor = byte_at(&self.text, cursor);
		self.goal = None;
	}

	fn vertical(&mut self, lines: isize) {
		let goal = match self.goal {
			Some(g) => g,
			None => {
				let g = self.col();
				self.goal = Some(g);
				g
			},
		};
		let mut start = self.line_start(self.cursor);
		for _ in 0..lines.unsigned_abs() {
			if lines < 0 {
				if start == 0 {
					break;
				}
				start = self.line_start(start - 1);
			} else {
				let end = self.line_end(start);
				if end == self.text.len() {
					break;
				}
				start = end + 1;
			}
		}
		self.cursor = self.at_col(start, goal);
	}

	/// Applies an editing key; `false` when it is not one.
	pub fn key(&mut self, key: &Key) -> bool {
		let prev = |s: &str, at: usize| s[..at].chars().next_back().map_or(at, |c| at - c.len_utf8());
		let next = |s: &str, at: usize| s[at..].chars().next().map_or(at, |c| at + c.len_utf8());
		match key.name.as_str() {
			"enter" => {
				// Keep the line's indentation.
				let start = self.line_start(self.cursor);
				let indent: String = self.text[start..].chars().take_while(|c| *c == ' ').collect();
				self.insert(&format!("\n{indent}"));
			},
			"tab" => self.insert("  "),
			"backspace" if self.cursor > 0 => {
				let p = prev(&self.text, self.cursor);
				self.text.replace_range(p..self.cursor, "");
				self.cursor = p;
				self.goal = None;
			},
			"delete" if self.cursor < self.text.len() => {
				let n = next(&self.text, self.cursor);
				self.text.replace_range(self.cursor..n, "");
				self.goal = None;
			},
			"backspace" | "delete" => {},
			"left" => {
				self.cursor = prev(&self.text, self.cursor);
				self.goal = None;
			},
			"right" => {
				self.cursor = next(&self.text, self.cursor);
				self.goal = None;
			},
			"up" => self.vertical(-1),
			"down" => self.vertical(1),
			"page_up" => self.vertical(-20),
			"page_down" => self.vertical(20),
			"home" => {
				self.cursor = self.line_start(self.cursor);
				self.goal = None;
			},
			"end" => {
				self.cursor = self.line_end(self.cursor);
				self.goal = None;
			},
			"paste" => {
				let t = key.text.clone().unwrap_or_default().replace("\r\n", "\n");
				self.insert(&t);
			},
			_ => {
				let Some(t) = key.typed() else { return false };
				let t = t.to_owned();
				self.insert(&t);
			},
		}
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn k(name: &str) -> Key {
		Key::named(name)
	}

	fn typed(t: &str) -> Key {
		Key { text: Some(t.into()), ..Key::named(t) }
	}

	#[test]
	fn utf16_splices_respect_wide_characters() {
		// UTF-16 units: a=1, é=1, 😀=2, b=1.
		assert_eq!(splice_utf16("aé😀b", 2, 4, "x"), "aéxb");
		assert_eq!(splice_utf16("aé😀b", 5, 5, "!"), "aé😀b!");
		assert_eq!(utf16_len("aé😀b"), 5);
		assert_eq!(utf16_at("aé😀b", "aé😀".len()), 4);
	}

	#[test]
	fn line_fields_edit_at_the_caret() {
		let mut l = Line::new("pod");
		l.key(&k("home"));
		l.key(&typed("a"));
		assert_eq!((l.text.as_str(), l.cursor), ("apod", 1));
		l.key(&k("end"));
		l.key(&k("backspace"));
		assert_eq!(l.text, "apo");
		assert!(!l.key(&k("enter")));
	}

	#[test]
	fn the_editor_keeps_indent_and_column_across_lines() {
		let mut a = Area::new("spec:\n  replicas: 1\nx");
		a.key(&k("down"));
		a.key(&k("end"));
		a.key(&k("enter"));
		a.key(&typed("y"));
		assert_eq!(a.text, "spec:\n  replicas: 1\n  y\nx");
		// Up from the end of a long line lands at the same column or the end.
		a.key(&k("up"));
		assert_eq!(&a.text[a.line_start(a.cursor)..a.cursor], "  r");
		a.key(&k("down"));
		a.key(&k("down"));
		assert_eq!(a.cursor, a.text.len());
		a.key(&k("backspace"));
		assert!(a.text.ends_with("y\n"));
	}
}
