//! Human formatting: ages, durations and natural sort order.

use std::cmp::Ordering;

/// A duration in seconds as Kubernetes tools print ages: `42s`, `7m`, `3h12m`,
/// `5d4h`, `120d`.
pub fn age(secs: i64) -> String {
	let s = secs.max(0);
	match s {
		0..60 => format!("{s}s"),
		60..3600 => format!("{}m", s / 60),
		3600..86_400 => {
			let (h, m) = (s / 3600, (s % 3600) / 60);
			if m == 0 || h >= 10 { format!("{h}h") } else { format!("{h}h{m}m") }
		},
		_ => {
			let (d, h) = (s / 86_400, (s % 86_400) / 3600);
			if h == 0 || d >= 10 { format!("{d}d") } else { format!("{d}d{h}h") }
		},
	}
}

/// Seconds from `then` to `now`, both Unix seconds.
pub fn since(now: i64, then: i64) -> i64 {
	now - then
}

/// Compares strings so digit runs order by value: `worker-2` before
/// `worker-10`. Letter case is ignored, then used as a tiebreaker.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
	let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
	loop {
		match (x.first(), y.first()) {
			(None, None) => return a.cmp(b),
			(None, Some(_)) => return Ordering::Less,
			(Some(_), None) => return Ordering::Greater,
			(Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
				let xn = x.iter().take_while(|b| b.is_ascii_digit()).count();
				let yn = y.iter().take_while(|b| b.is_ascii_digit()).count();
				let (xd, yd) = (trim_zeros(&x[..xn]), trim_zeros(&y[..yn]));
				let ord = xd.len().cmp(&yd.len()).then_with(|| xd.cmp(yd));
				if ord != Ordering::Equal {
					return ord;
				}
				x = &x[xn..];
				y = &y[yn..];
			},
			(Some(c), Some(d)) => {
				let ord = c.to_ascii_lowercase().cmp(&d.to_ascii_lowercase());
				if ord != Ordering::Equal {
					return ord;
				}
				x = &x[1..];
				y = &y[1..];
			},
		}
	}
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
	let n = digits.iter().take_while(|&&b| b == b'0').count();
	&digits[n.min(digits.len().saturating_sub(1))..]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ages_pick_the_two_largest_units_only_while_small() {
		assert_eq!(age(-5), "0s");
		assert_eq!(age(59), "59s");
		assert_eq!(age(60), "1m");
		assert_eq!(age(3599), "59m");
		assert_eq!(age(3600), "1h");
		assert_eq!(age(3600 + 12 * 60), "1h12m");
		assert_eq!(age(11 * 3600 + 60), "11h");
		assert_eq!(age(86_400 * 2 + 3600 * 3), "2d3h");
		assert_eq!(age(86_400 * 120 + 3600), "120d");
	}

	#[test]
	fn natural_order_compares_digit_runs_by_value() {
		let mut v = vec!["worker-10", "worker-2", "Worker-1", "worker-02a", "api"];
		v.sort_by(|a, b| natural_cmp(a, b));
		assert_eq!(v, ["api", "Worker-1", "worker-2", "worker-02a", "worker-10"]);
		assert_eq!(natural_cmp("a0", "a00"), Ordering::Less);
	}
}
