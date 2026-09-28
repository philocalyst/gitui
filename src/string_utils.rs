use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

///
pub fn trim_length_left(s: &str, width: usize) -> &str {
	let len = s.len();
	if len > width {
		for i in len - width..len {
			if s.is_char_boundary(i) {
				return &s[i..];
			}
		}
	}

	s
}

//TODO: allow customize tabsize
pub fn tabs_to_spaces(input: String) -> String {
	if input.contains('\t') {
		input.replace('\t', "  ")
	} else {
		input
	}
}

/// This function will return a str slice which start at specified offset.
/// As src is a unicode str, start offset has to be calculated with each character.
pub fn trim_offset(src: &str, mut offset: usize) -> &str {
	let mut start = 0;
	for c in UnicodeSegmentation::graphemes(src, true) {
		let w = c.width();
		if w <= offset {
			offset -= w;
			start += c.len();
		} else {
			break;
		}
	}
	&src[start..]
}

/// The longest prefix of `input` whose display width does not
/// exceed `width` (grapheme-cluster aware). Hand-rolled rather
/// than `unicode-truncate`: that crate panics on right-to-left
/// text at several widths (the audit hit it with an Arabic
/// commit subject), which here would take down the whole TUI.
#[must_use]
pub fn truncate_to_width(input: &str, width: usize) -> &str {
	let mut end = 0;
	let mut used = 0;
	for grapheme in UnicodeSegmentation::graphemes(input, true) {
		let w = grapheme.width();
		if used + w > width {
			break;
		}
		used += w;
		end += grapheme.len();
	}
	&input[..end]
}

/// Replace control characters with a printable placeholder.
/// Terminals misrender (or act on) control bytes — an ESC in a
/// commit subject would inject escape sequences into the TUI —
/// and truncating at zero-width boundaries is unreliable
/// (the audit crashed `unicode-truncate` on exactly that).
#[must_use]
pub fn sanitize_control_chars(input: &str) -> String {
	if !input.chars().any(char::is_control) {
		return String::from(input);
	}
	input
		.chars()
		.map(|c| if c.is_control() { '?' } else { c })
		.collect()
}

#[cfg(test)]
mod test {
	use pretty_assertions::assert_eq;

	use crate::string_utils::{
		sanitize_control_chars, trim_length_left, truncate_to_width,
	};

	#[test]
	fn test_trim() {
		assert_eq!(trim_length_left("👍foo", 3), "foo");
		assert_eq!(trim_length_left("👍foo", 4), "foo");
	}

	/// The regression that took down the rail: RTL text must
	/// truncate without panicking at any width.
	#[test]
	fn test_truncate_to_width() {
		assert_eq!(truncate_to_width("hello", 10), "hello");
		assert_eq!(truncate_to_width("hello", 4), "hell");
		assert_eq!(truncate_to_width("", 3), "");
		assert_eq!(truncate_to_width("abc", 0), "");
		let rtl =
			"RTL \u{5d9}\u{5d5}\u{5dd} \u{633}\u{644}\u{627} mixed";
		for width in 0..40 {
			let _ = truncate_to_width(rtl, width);
		}
		assert_eq!(truncate_to_width(rtl, 4), "RTL ");
		// grapheme clusters are never split
		assert_eq!(
			truncate_to_width("e\u{301}\u{301}f", 1),
			"e\u{301}\u{301}"
		);
	}

	#[test]
	fn test_sanitize_control_chars() {
		assert_eq!(
			sanitize_control_chars("plain"),
			String::from("plain")
		);
		assert_eq!(
			sanitize_control_chars("a\u{7}b\u{1b}[31m"),
			String::from("a?b?[31m")
		);
		assert_eq!(
			sanitize_control_chars("tab\tnewline\n"),
			String::from("tab?newline?")
		);
		// wide and RTL characters survive untouched
		assert_eq!(
			sanitize_control_chars("👍 \u{633}"),
			String::from("👍 \u{633}")
		);
	}
}
