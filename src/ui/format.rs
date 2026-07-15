use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub(crate) fn relative_time(rfc3339: &str, now: time::OffsetDateTime) -> String {
    let Ok(t) =
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
    else {
        // Char-boundary-safe: byte-offset truncate panics mid-char on
        // multi-byte input, and this path exists to degrade gracefully.
        return rfc3339.chars().take(10).collect();
    };
    let secs = (now - t).whole_seconds().max(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=172_799 => "yesterday".into(),
        172_800..=604_799 => format!("{}d ago", secs / 86_400),
        _ => format!("{} {}", MONTHS[t.month() as usize - 1], t.day()),
    }
}

pub(crate) fn progress_bar(done: usize, total: usize, width: usize) -> (String, String) {
    let filled = (width * done)
        .checked_div(total)
        .map_or(0, |x| x.min(width));
    ("▰".repeat(filled), "▱".repeat(width - filled))
}

/// Truncates `s` to `max_cols` DISPLAY COLUMNS, keeping the end: `…` plus
/// the longest suffix that fits. Column-based, unlike `word_fit_rows`'s
/// char-based convention: these truncators exist to stop overflow past a
/// ratatui border, and ratatui clips by columns — a CJK char is one char
/// but two columns. Degenerate budgets are defined: 1 returns `…`, 0
/// returns the empty string.
pub(crate) fn truncate_path_start(s: &str, max_cols: usize) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let budget = max_cols - 1; // the `…` occupies one column
    let mut cols = 0;
    let mut tail: Vec<char> = Vec::new();
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if cols + w > budget {
            break;
        }
        cols += w;
        tail.push(c);
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("\u{2026}{tail}")
}

/// Truncates `s` to `max_cols` display columns keeping the START, with a
/// trailing `…` when cut. Same column-based rationale as
/// [`truncate_path_start`].
pub(crate) fn truncate_end(s: &str, max_cols: usize) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let budget = max_cols - 1;
    let mut cols = 0;
    let mut out = String::new();
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if cols + w > budget {
            break;
        }
        cols += w;
        out.push(c);
    }
    out.push('\u{2026}');
    out
}

/// Rows `text` occupies once ratatui's `Wrap { trim: false }` word-wraps it
/// at `width` columns: greedy word fill — words split on whitespace, packed
/// with single joining spaces; a word wider than a whole row char-splits
/// across rows. `ceil(chars / width)` is NOT a substitute: word breaks waste
/// end-of-line space, so a char count is only a lower bound on the rendered
/// rows (e.g. `✓ done · 3 findings · 12s — saved`, 33 chars, is 2 rows by
/// char count at width 18 but renders as 3). Char-based counting (not
/// display columns), consistent with the codebase convention. Shared by the
/// dashboard's per-panel row budget and triage's detail-pane overflow
/// markers — both need the estimate to match ratatui's actual wrap, not a
/// naive division.
pub(crate) fn word_fit_rows(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1;
    let mut col = 0; // chars already on the current row
    for word in text.split_whitespace() {
        let mut len = word.chars().count();
        if col > 0 {
            if col + 1 + len <= width {
                col += 1 + len; // joins the current row after a space
                continue;
            }
            rows += 1; // wraps to a fresh row
        }
        while len > width {
            rows += 1; // a too-wide word char-splits across full rows
            len -= width;
        }
        col = len; // the word (or its final fragment) opens the fresh row
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn relative_time_buckets() {
        let now = datetime!(2026-07-10 12:00:00 UTC);
        assert_eq!(relative_time("2026-07-10T11:59:30Z", now), "just now");
        assert_eq!(relative_time("2026-07-10T11:55:00Z", now), "5m ago");
        assert_eq!(relative_time("2026-07-10T10:00:00Z", now), "2h ago");
        assert_eq!(relative_time("2026-07-09T06:00:00Z", now), "yesterday");
        assert_eq!(relative_time("2026-07-07T12:00:00Z", now), "3d ago");
        assert_eq!(relative_time("2026-06-01T12:00:00Z", now), "Jun 1");
        assert_eq!(relative_time("garbage-in", now), "garbage-in");
        // Byte offset 10 lands mid-char (é spans bytes 9-10): truncation must
        // respect char boundaries — first 10 CHARS, no panic.
        assert_eq!(relative_time("aaaaaaaaaé-not-a-date", now), "aaaaaaaaaé");
    }

    #[test]
    fn progress_bar_fills_proportionally_and_clamps() {
        assert_eq!(progress_bar(5, 10, 10), ("▰▰▰▰▰".into(), "▱▱▱▱▱".into()));
        assert_eq!(progress_bar(0, 3, 6), ("".into(), "▱▱▱▱▱▱".into()));
        assert_eq!(progress_bar(3, 3, 6), ("▰▰▰▰▰▰".into(), "".into()));
        assert_eq!(progress_bar(7, 3, 6), ("▰▰▰▰▰▰".into(), "".into()));
        assert_eq!(progress_bar(0, 0, 4), ("".into(), "▱▱▱▱".into()));
    }

    #[test]
    fn truncate_path_start_keeps_tail_by_display_columns() {
        assert_eq!(truncate_path_start("docs/plan.md", 50), "docs/plan.md");
        assert_eq!(
            truncate_path_start("docs/specs/plan.md", 8),
            "\u{2026}plan.md"
        );
        // CJK chars are 2 COLUMNS each: budget must count columns, not chars.
        // "…計画.md" is 1 + 2 + 2 + 3 = 8 columns exactly.
        assert_eq!(
            truncate_path_start("specs/漢字計画.md", 8),
            "\u{2026}計画.md"
        );
        // degenerate budgets are defined, not panics
        assert_eq!(truncate_path_start("abc", 1), "\u{2026}");
        assert_eq!(truncate_path_start("abc", 0), "");
    }

    #[test]
    fn truncate_end_keeps_start_by_display_columns() {
        assert_eq!(truncate_end("short", 10), "short");
        assert_eq!(
            truncate_end("reviewers work blind", 10),
            "reviewers\u{2026}"
        );
        // 2-column glyphs: "漢字…" is 2 + 2 + 1 = 5 columns
        assert_eq!(truncate_end("漢字漢字", 5), "漢字\u{2026}");
        assert_eq!(truncate_end("abc", 1), "\u{2026}");
        assert_eq!(truncate_end("abc", 0), "");
    }

    #[test]
    fn word_fit_rows_matches_the_renderers_word_wrap() {
        // 33 chars at width 18 is 2 rows by char count, but ratatui
        // word-wraps to 3 (`✓ done · 3` / `findings · 12s —` / `saved`).
        assert_eq!(word_fit_rows("✓ done · 3 findings · 12s — saved", 18), 3);
        // Exactly filling the row (4 + space + 4 = 9) is still one row…
        assert_eq!(word_fit_rows("aaaa bbbb", 9), 1);
        // …one column narrower and the second word wraps.
        assert_eq!(word_fit_rows("aaaa bbbb", 8), 2);
        // A word wider than the whole row char-splits across rows.
        assert_eq!(word_fit_rows("abcdefghij", 4), 3);
        // Empty text still occupies one row.
        assert_eq!(word_fit_rows("", 18), 1);
        // Multi-char whitespace runs collapse to single joining spaces the
        // way split_whitespace sees them — no panic, deterministic count.
        assert_eq!(word_fit_rows("a  b\tc", 5), 1);
    }
}
