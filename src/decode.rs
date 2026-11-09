//! Decoders: Secret values, TLS certificates, Helm release records and the
//! JSONPath subset CRD printer columns use.

use std::io::Read;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

/// A Secret value decoded from base64: text when it is UTF-8.
pub fn secret_value(b64: &str) -> Result<String, String> {
	let bytes = STANDARD.decode(b64.trim()).map_err(|e| e.to_string())?;
	String::from_utf8(bytes).map_err(|e| format!("{} bytes of binary data", e.as_bytes().len()))
}

/// What a certificate says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert {
	/// Subject DN.
	pub subject:   String,
	/// Issuer DN.
	pub issuer:    String,
	/// Valid from (RFC 3339).
	pub not_before: String,
	/// Valid until (RFC 3339).
	pub not_after: String,
	/// Unix seconds of `not_after`.
	pub expires:   i64,
	/// DNS and IP subject alternative names.
	pub sans:      Vec<String>,
}

/// Every certificate in a PEM bundle (`tls.crt`).
pub fn certs(pem: &str) -> Vec<Cert> {
	let mut out = Vec::new();
	for block in x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()).flatten() {
		if block.label != "CERTIFICATE" {
			continue;
		}
		let Ok(x509) = block.parse_x509() else { continue };
		let sans = x509
			.subject_alternative_name()
			.ok()
			.flatten()
			.map(|ext| {
				ext.value
					.general_names
					.iter()
					.filter_map(|n| match n {
						x509_parser::extensions::GeneralName::DNSName(d) => Some((*d).to_owned()),
						x509_parser::extensions::GeneralName::IPAddress(ip) => match ip.len() {
							4 => Some(std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]).to_string()),
							16 => <[u8; 16]>::try_from(*ip).ok().map(|b| std::net::Ipv6Addr::from(b).to_string()),
							_ => None,
						},
						_ => None,
					})
					.collect()
			})
			.unwrap_or_default();
		let v = x509.validity();
		let rfc = |t: &x509_parser::time::ASN1Time| {
			jiff::Timestamp::from_second(t.timestamp()).map(|t| t.to_string()).unwrap_or_default()
		};
		out.push(Cert {
			subject: x509.subject().to_string(),
			issuer: x509.issuer().to_string(),
			not_before: rfc(&v.not_before),
			not_after: rfc(&v.not_after),
			expires: v.not_after.timestamp(),
			sans,
		});
	}
	out
}

/// A Helm release record from its secret's `data.release`: base64 (the
/// secret encoding) of base64 of gzip of JSON.
pub fn helm_release(data_release: &str) -> Result<Value, String> {
	let once = STANDARD.decode(data_release.trim()).map_err(|e| e.to_string())?;
	let twice = STANDARD.decode(&once).map_err(|e| e.to_string())?;
	let json = if twice.starts_with(&[0x1f, 0x8b]) {
		let mut s = Vec::new();
		flate2::read::GzDecoder::new(twice.as_slice()).read_to_end(&mut s).map_err(|e| e.to_string())?;
		s
	} else {
		twice
	};
	serde_json::from_slice(&json).map_err(|e| e.to_string())
}

/// Evaluates the JSONPath subset of printer columns on `v`: `.a.b`,
/// `.a[0]`, `.a[*].b` (joined with `,`), `.a[?(@.k=="v")].b`, and
/// `['dotted.key']`.
pub fn jsonpath(v: &Value, path: &str) -> Vec<Value> {
	let mut cur = vec![v.clone()];
	let mut rest = path.trim().trim_start_matches('{').trim_end_matches('}').trim_start_matches('$');
	while !rest.is_empty() {
		if let Some(r) = rest.strip_prefix('.') {
			let end = r.find(['.', '[']).unwrap_or(r.len());
			let key = &r[..end];
			cur = cur.iter().filter_map(|x| x.get(key).cloned()).collect();
			rest = &r[end..];
		} else if let Some(r) = rest.strip_prefix('[') {
			let Some(close) = find_close(r) else { return vec![] };
			let inner = &r[..close];
			rest = &r[close + 1..];
			cur = if inner == "*" {
				cur.iter().flat_map(|x| x.as_array().cloned().unwrap_or_default()).collect()
			} else if let Ok(i) = inner.parse::<usize>() {
				cur.iter().filter_map(|x| x.get(i).cloned()).collect()
			} else if let Some(key) = inner.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')) {
				cur.iter().filter_map(|x| x.get(key).cloned()).collect()
			} else if let Some(f) = inner.strip_prefix("?(@.").and_then(|f| f.strip_suffix(')')) {
				let Some((k, want)) = f.split_once("==") else { return vec![] };
				let want = want.trim().trim_matches(['"', '\'']);
				let k = k.trim();
				cur.iter()
					.flat_map(|x| x.as_array().cloned().unwrap_or_default())
					.filter(|e| e.get(k).is_some_and(|got| got.as_str().map_or_else(|| got.to_string(), str::to_owned) == want))
					.collect()
			} else {
				return vec![];
			};
		} else {
			return vec![];
		}
	}
	cur
}

fn find_close(s: &str) -> Option<usize> {
	let mut depth = 0;
	let mut quote = None;
	for (i, c) in s.char_indices() {
		match (quote, c) {
			(Some(q), _) if c == q => quote = None,
			(Some(_), _) => {},
			(None, '"' | '\'') => quote = Some(c),
			(None, '[' | '(') => depth += 1,
			(None, ')') => depth -= 1,
			(None, ']') if depth == 0 => return Some(i),
			(None, ']') => depth -= 1,
			_ => {},
		}
	}
	None
}

/// The text of jsonpath results: strings as is, others as JSON, joined by `,`.
pub fn jsonpath_text(v: &Value, path: &str) -> String {
	jsonpath(v, path)
		.iter()
		.map(|x| match x {
			Value::String(s) => s.clone(),
			Value::Null => String::new(),
			other => other.to_string(),
		})
		.collect::<Vec<_>>()
		.join(",")
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn printer_paths_cover_fields_indexes_filters_and_quoted_keys() {
		let v = json!({"spec": {"replicas": 3, "hosts": ["a", "b"]},
			"status": {"conditions": [{"type": "Ready", "status": "True"}, {"type": "Synced", "status": "False"}]},
			"metadata": {"labels": {"app.kubernetes.io/name": "x"}}});
		assert_eq!(jsonpath_text(&v, ".spec.replicas"), "3");
		assert_eq!(jsonpath_text(&v, ".spec.hosts[0]"), "a");
		assert_eq!(jsonpath_text(&v, ".spec.hosts[*]"), "a,b");
		assert_eq!(jsonpath_text(&v, r#".status.conditions[?(@.type=="Ready")].status"#), "True");
		assert_eq!(jsonpath_text(&v, ".metadata.labels['app.kubernetes.io/name']"), "x");
		assert_eq!(jsonpath_text(&v, ".missing.path"), "");
	}

	#[test]
	fn helm_records_decode_through_both_base64_layers_and_gzip() {
		let record = json!({"name": "web", "version": 3, "info": {"status": "deployed"}});
		let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
		std::io::Write::write_all(&mut gz, record.to_string().as_bytes()).unwrap();
		let inner = STANDARD.encode(gz.finish().unwrap());
		let outer = STANDARD.encode(inner);
		assert_eq!(helm_release(&outer).unwrap(), record);
	}

	#[test]
	fn secret_values_decode_text_and_name_binary() {
		assert_eq!(secret_value("aGVsbG8=").unwrap(), "hello");
		assert!(secret_value(&STANDARD.encode([0xff, 0xfe])).unwrap_err().contains("binary"));
	}
}
