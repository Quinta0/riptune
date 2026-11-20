//! Width-aware string helpers. Everything here measures terminal columns
//! (via unicode-width), not chars, so CJK titles and emoji line up.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn width(s: &str) -> usize {
    s.width()
}

pub fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Truncates to at most `max` columns, ending in an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Truncates `text` to `budget` columns, then splits out the first
/// case-insensitive occurrence of `query` as its own span so the reason a
/// row matched a search is visible at a glance.
pub fn highlighted(text: &str, budget: usize, query: Option<&str>, base: Style, hit: Style) -> Vec<Span<'static>> {
    let text = truncate(text, budget);
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return vec![Span::styled(text, base)];
    };
    let Some((start, end)) = find_ignore_case(&text, query) else {
        return vec![Span::styled(text, base)];
    };
    let mut spans = Vec::with_capacity(3);
    if start > 0 {
        spans.push(Span::styled(text[..start].to_string(), base));
    }
    spans.push(Span::styled(text[start..end].to_string(), hit));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_string(), base));
    }
    spans
}

/// Byte range of the first case-insensitive match of `needle` in `hay`.
fn find_ignore_case(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let starts: Vec<usize> = hay.char_indices().map(|(i, _)| i).collect();
    for &start in &starts {
        let mut matched = 0;
        for (i, c) in hay[start..].char_indices() {
            let lower: Vec<char> = c.to_lowercase().collect();
            if needle.get(matched..matched + lower.len()) != Some(&lower[..]) {
                break;
            }
            matched += lower.len();
            if matched == needle.len() {
                return Some((start, start + i + c.len_utf8()));
            }
        }
    }
    None
}

/// A smooth horizontal bar using eighth-block glyphs, so progress visibly
/// moves every second even on narrow bars.
pub fn bar(ratio: f64, width: usize) -> (String, String) {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (ratio.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let mut filled = "█".repeat(full);
    let mut used = full;
    if full < width && !eighths.is_multiple_of(8) {
        filled.push(PARTIAL[eighths % 8]);
        used += 1;
    }
    (filled, "─".repeat(width.saturating_sub(used)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語の歌", 5), "日本…");
        assert!(width(&truncate("日本語の歌", 5)) <= 5);
    }

    #[test]
    fn highlight_finds_case_insensitive_match() {
        let spans = highlighted("Hello World", 20, Some("wor"), Style::default(), Style::default());
        let parts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(parts, ["Hello ", "Wor", "ld"]);
    }

    #[test]
    fn highlight_without_match_is_one_span() {
        assert_eq!(highlighted("abc", 20, Some("z"), Style::default(), Style::default()).len(), 1);
    }

    #[test]
    fn bar_always_fills_exact_width() {
        for r in [0.0, 0.13, 0.5, 0.999, 1.0] {
            let (a, b) = bar(r, 17);
            assert_eq!(width(&a) + width(&b), 17, "ratio {r}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3725), "1:02:05");
    }
}
