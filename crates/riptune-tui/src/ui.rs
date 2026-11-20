//! Rendering. Pure function of `App` state (apart from pane scroll offsets
//! and cover-art resize state, which are render-time concerns).

use crate::app::{App, Focus, Status};
use crate::keymap;
use crate::queue::RepeatMode;
use crate::text::{bar, format_duration, highlighted, truncate, width};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;
use ratatui_image::StatefulImage;

/// One row's pieces: left text (highlightable) and a muted right rail.
struct Row<'a> {
    marker: Span<'static>,
    lead: String,
    text: &'a str,
    detail: Option<String>,
    rail: String,
    style: Style,
}

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    if area.width < 24 || area.height < 6 {
        f.render_widget(Paragraph::new("riptune: window too small").style(Style::new().fg(app.theme.text_muted)), area);
        return;
    }

    // The player shrinks before the browser does: full deck with art on
    // tall terminals, a two-line strip on short ones.
    let deck_height = match area.height {
        h if h >= 24 => 7,
        h if h >= 14 => 4,
        _ => 2,
    };
    let [browser, deck, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(deck_height), Constraint::Length(1)]).areas(area);

    draw_browser(app, f, browser);
    draw_deck(app, f, deck);
    draw_footer(app, f, footer);

    if app.show_help {
        draw_help(app, f, area);
    }
}

fn draw_browser(app: &mut App, f: &mut Frame, area: Rect) {
    // Show as many columns as fit, always including the focused one, and
    // give the focused column the most room.
    let panes: Vec<Focus> = match area.width {
        w if w >= 96 => vec![Focus::Artists, Focus::Albums, Focus::Tracks],
        w if w >= 60 => match app.focus {
            Focus::Artists => vec![Focus::Artists, Focus::Albums],
            other => vec![other.left(), other],
        },
        _ => vec![app.focus],
    };
    let weights: Vec<Constraint> = panes
        .iter()
        .map(|&p| {
            let w = match (panes.len(), p == app.focus, p) {
                (1, ..) => 1,
                (_, true, Focus::Tracks) => 5,
                (_, true, _) => 4,
                (_, false, Focus::Tracks) => 3,
                _ => 2,
            };
            Constraint::Fill(w)
        })
        .collect();
    let rects = Layout::horizontal(weights).split(area);
    for (&pane, &rect) in panes.iter().zip(rects.iter()) {
        draw_pane(app, f, rect, pane);
    }
}

fn draw_pane(app: &mut App, f: &mut Frame, area: Rect, pane: Focus) {
    let theme = app.theme;
    let focused = app.focus == pane;
    let query = app.search.as_ref().filter(|s| s.scope == pane).map(|s| s.query.clone());

    let (name, len) = match pane {
        Focus::Artists => ("Artists", app.artists.items.len()),
        Focus::Albums => ("Albums", app.albums.items.len()),
        Focus::Tracks => ("Tracks", app.tracks.items.len()),
    };
    let mut title = vec![Span::styled(
        format!(" {name} "),
        if focused { Style::new().fg(theme.accent).add_modifier(Modifier::BOLD) } else { Style::new().fg(theme.text_muted) },
    )];
    if let Some(q) = query.as_deref().filter(|q| !q.is_empty()) {
        title.push(Span::styled(format!("/{} ", truncate(q, 20)), Style::new().fg(theme.accent)));
    }
    title.push(Span::styled(format!("{len} "), Style::new().fg(theme.text_muted)));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { theme.border_focused } else { theme.border_unfocused }))
        .title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if len == 0 {
        let hint = empty_hint(app, pane, query.as_deref());
        let y = inner.y + inner.height.saturating_sub(1) / 2;
        f.render_widget(
            Paragraph::new(hint).alignment(Alignment::Center).style(Style::new().fg(theme.text_muted)),
            Rect { y, height: 1, ..inner },
        );
        return;
    }

    let height = inner.height as usize;
    let text_width = inner.width.saturating_sub(1) as usize; // leave the scrollbar column
    let playing_id = app.now_playing.as_ref().map(|np| np.track.id.clone());
    let paused = app.now_playing.as_ref().is_some_and(|np| np.status == Status::Paused);
    let in_album = app.tracks_of.is_some();
    let hit = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

    let (range, selected, offset) = match pane {
        Focus::Artists => (app.artists.window(height), app.artists.selected, app.artists.offset),
        Focus::Albums => (app.albums.window(height), app.albums.selected, app.albums.offset),
        Focus::Tracks => (app.tracks.window(height), app.tracks.selected, app.tracks.offset),
    };

    let rows: Vec<ListItem> = range
        .clone()
        .map(|i| {
            let row = match pane {
                Focus::Artists => {
                    let a = &app.artists.items[i];
                    Row {
                        marker: Span::raw(" "),
                        lead: String::new(),
                        text: &a.name,
                        detail: None,
                        rail: if a.album_count > 0 { a.album_count.to_string() } else { String::new() },
                        style: Style::new(),
                    }
                }
                Focus::Albums => {
                    let a = &app.albums.items[i];
                    Row {
                        marker: Span::raw(" "),
                        lead: String::new(),
                        text: &a.name,
                        detail: None,
                        rail: a.year.map(|y| y.to_string()).unwrap_or_default(),
                        style: Style::new(),
                    }
                }
                Focus::Tracks => {
                    let t = &app.tracks.items[i];
                    let playing = playing_id.as_deref() == Some(t.id.as_str());
                    Row {
                        marker: if playing {
                            Span::styled(if paused { "⏸" } else { "▶" }, Style::new().fg(theme.now_playing))
                        } else {
                            Span::raw(" ")
                        },
                        // Album view: track numbers. Search/queue view: the
                        // artist, since titles alone are ambiguous there.
                        lead: match (in_album, t.track_number) {
                            (true, Some(n)) => format!("{n:>2}  "),
                            (true, None) => "    ".into(),
                            _ => String::new(),
                        },
                        text: &t.title,
                        detail: (!in_album).then(|| t.artist.clone()).flatten(),
                        rail: t.duration_secs.map(|d| format_duration(d as u64)).unwrap_or_default(),
                        style: if playing {
                            Style::new().fg(theme.now_playing).add_modifier(Modifier::BOLD)
                        } else {
                            Style::new()
                        },
                    }
                }
            };
            ListItem::new(render_row(row, text_width, query.as_deref(), hit, theme.text_muted))
        })
        .collect();

    // Focused: solid cursor band. Unfocused: a quieter marker, so you can
    // still see which artist/album the panes to the right belong to.
    let highlight = if focused {
        Style::new().bg(theme.selection_bg).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.accent)
    };
    let mut state = ListState::default().with_selected(Some(selected - range.start));
    f.render_stateful_widget(List::new(rows).highlight_style(highlight), inner, &mut state);

    if len > height {
        let mut sb = ScrollbarState::new(len.saturating_sub(height)).viewport_content_length(height).position(offset);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some(" "))
                .thumb_style(Style::new().fg(theme.border_unfocused)),
            inner,
            &mut sb,
        );
    }
}

fn render_row(row: Row, cols: usize, query: Option<&str>, hit: Style, muted: ratatui::style::Color) -> Line<'static> {
    let muted = Style::new().fg(muted);
    let rail_w = if row.rail.is_empty() { 0 } else { width(&row.rail) + 1 };
    let fixed = 2 + width(&row.lead) + rail_w;
    let budget = cols.saturating_sub(fixed);

    let mut spans = vec![row.marker, Span::raw(" ")];
    if !row.lead.is_empty() {
        spans.push(Span::styled(row.lead, muted));
    }
    // Title gets priority; the artist detail takes only what's left over.
    let title_budget = match &row.detail {
        Some(_) => budget.min(width(row.text)).max(budget * 3 / 5),
        None => budget,
    };
    let title = highlighted(row.text, title_budget.min(budget), query, row.style, hit);
    let mut used: usize = title.iter().map(|s| width(&s.content)).sum();
    spans.extend(title);
    if let Some(detail) = row.detail {
        let room = budget.saturating_sub(used + 3);
        if room >= 4 {
            let d = format!(" · {}", truncate(&detail, room));
            used += width(&d);
            spans.push(Span::styled(d, muted));
        }
    }
    if rail_w > 0 {
        spans.push(Span::raw(" ".repeat(budget.saturating_sub(used) + 1)));
        spans.push(Span::styled(row.rail, muted));
    }
    Line::from(spans)
}

fn empty_hint(app: &App, pane: Focus, query: Option<&str>) -> String {
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        return format!("no {} match \"{}\"", pane.label(), truncate(q, 24));
    }
    match pane {
        Focus::Artists if app.sync.is_some() => "indexing your library…".into(),
        Focus::Artists => "library is empty — check `riptune ping`".into(),
        Focus::Albums if app.artists.items.is_empty() => String::new(),
        Focus::Albums => "no albums".into(),
        Focus::Tracks if app.albums.items.is_empty() => String::new(),
        Focus::Tracks => "no tracks".into(),
    }
}

fn draw_deck(app: &mut App, f: &mut Frame, area: Rect) {
    let theme = app.theme;
    let muted = Style::new().fg(theme.text_muted);
    let bordered = area.height >= 4;
    let inner = if bordered {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme.border_unfocused));
        let inner = block.inner(area);
        f.render_widget(block, area);
        inner
    } else {
        area
    };

    // Cover art, roughly square (terminal cells are ~2:1).
    let cover_id = app.now_playing.as_ref().and_then(|np| np.track.cover_art.clone());
    let art = (inner.height >= 5).then(|| cover_id.as_deref().and_then(|id| app.covers.get_mut(id))).flatten();
    let text_area = if let Some(protocol) = art {
        let [art_area, _, rest] =
            Layout::horizontal([Constraint::Length(inner.height * 2 + 1), Constraint::Length(2), Constraint::Min(10)]).areas(inner);
        f.render_stateful_widget(StatefulImage::new(None), art_area, protocol);
        rest
    } else {
        inner.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 })
    };
    let w = text_area.width as usize;

    // Mode badges, right-aligned on the title line.
    let on = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let off = Style::new().fg(theme.border_unfocused);
    let mut badges = vec![Span::styled("shuffle", if app.shuffle { on } else { off }), Span::raw("  ")];
    badges.push(match app.repeat {
        RepeatMode::Off => Span::styled("repeat", off),
        RepeatMode::All => Span::styled("repeat all", on),
        RepeatMode::One => Span::styled("repeat one", on),
    });
    if let Some((pos, len)) = app.queue.as_ref().map(|q| q.progress()) {
        badges.insert(0, Span::styled(format!("{pos}/{len}   "), muted));
    }
    let badges_w: usize = badges.iter().map(|s| width(&s.content)).sum();

    let Some(np) = &app.now_playing else {
        let idle = if app.queue.is_some() { "stopped — space to resume the queue" } else { "nothing playing — pick a track and press enter" };
        let text = truncate(idle, w.saturating_sub(badges_w + 2));
        let pad = w.saturating_sub(width(&text) + badges_w);
        let mut spans = vec![Span::styled(text, muted), Span::raw(" ".repeat(pad))];
        spans.extend(badges);
        let mut lines = vec![Line::from(spans)];
        if text_area.height >= 5 {
            lines.insert(0, Line::raw(""));
        }
        f.render_widget(Paragraph::new(lines), text_area);
        return;
    };

    let (icon, icon_style) = match np.status {
        Status::Loading => ("◌", muted),
        Status::Playing => ("▶", Style::new().fg(theme.now_playing)),
        Status::Paused => ("⏸", Style::new().fg(theme.accent)),
    };
    let title_room = w.saturating_sub(badges_w + 4);
    let title = truncate(&np.track.title, title_room);
    let pad = w.saturating_sub(2 + width(&title) + badges_w);
    let mut title_line = vec![
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(title, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
    ];
    title_line.extend(badges);

    let artist = np.track.artist.as_deref().unwrap_or("unknown artist");
    let subtitle = match np.track.album.as_deref() {
        Some(album) if !album.is_empty() => format!("{artist} — {album}"),
        _ => artist.to_string(),
    };

    let elapsed = np.position.as_secs();
    let total = np.track.duration_secs.map(u64::from);
    let left = format_duration(elapsed);
    let right = match np.status {
        Status::Loading => "loading…".to_string(),
        _ => total.map(format_duration).unwrap_or_else(|| "--:--".into()),
    };
    let ratio = total.filter(|&t| t > 0).map(|t| elapsed as f64 / t as f64).unwrap_or(0.0);
    let bar_w = w.saturating_sub(width(&left) + width(&right) + 2);
    let (filled, rest) = bar(ratio, bar_w);
    let progress = Line::from(vec![
        Span::styled(left, muted),
        Span::raw(" "),
        Span::styled(filled, Style::new().fg(theme.progress)),
        Span::styled(rest, Style::new().fg(theme.border_unfocused)),
        Span::raw(" "),
        Span::styled(right, muted),
    ]);

    let vol_pct = (app.volume * 100.0).round() as u16;
    let (vfill, vrest) = bar(app.volume as f64, 10);
    let volume = Line::from(vec![
        Span::styled("vol ", muted),
        Span::styled(vfill, Style::new().fg(theme.volume)),
        Span::styled(vrest, Style::new().fg(theme.border_unfocused)),
        Span::styled(format!(" {vol_pct}%"), muted),
    ]);

    let lines = match text_area.height {
        0..=2 => vec![Line::from(title_line), progress],
        3..=4 => vec![Line::from(title_line), Line::styled(truncate(&subtitle, w), muted), progress],
        _ => vec![Line::from(title_line), Line::styled(truncate(&subtitle, w), muted), Line::raw(""), progress, volume],
    };
    f.render_widget(Paragraph::new(lines), text_area);
}

fn draw_footer(app: &App, f: &mut Frame, area: Rect) {
    let theme = app.theme;
    let muted = Style::new().fg(theme.text_muted);
    let key = Style::new().fg(theme.accent);
    let w = area.width as usize;

    let right = app.sync.map(|(done, total)| match total {
        0 => " ⟳ syncing… ".to_string(),
        t => format!(" ⟳ syncing {done}/{t} "),
    });
    let right_w = right.as_deref().map(width).unwrap_or(0);
    let room = w.saturating_sub(right_w);

    let left: Vec<Span> = if let Some(s) = app.search.as_ref().filter(|s| s.editing) {
        let hint = format!("  in {} · tab: switch · enter: keep · esc: cancel", s.scope.label());
        let prompt_room = room.saturating_sub(width(&hint) + 3).max(8);
        vec![
            Span::styled(" /", key),
            Span::raw(truncate(&s.query, prompt_room)),
            Span::styled("█", key),
            Span::styled(truncate(&hint, room.saturating_sub(s.query.len() + 3)), muted),
        ]
    } else if let Some(m) = &app.message {
        let style = if m.is_error { Style::new().fg(theme.error) } else { Style::new().fg(theme.now_playing) };
        vec![Span::styled(format!(" {}", truncate(&m.text, room.saturating_sub(2))), style)]
    } else {
        let mut spans = Vec::new();
        let mut used = 0;
        for (k, action) in keymap::FOOTER {
            let w = width(k) + width(action) + 4;
            if used + w > room {
                break;
            }
            used += w;
            spans.push(Span::styled(format!(" {k}"), key));
            spans.push(Span::styled(format!(" {action}  "), muted));
        }
        spans
    };

    let used: usize = left.iter().map(|s| width(&s.content)).sum();
    let mut spans = left;
    if let Some(r) = right {
        spans.push(Span::raw(" ".repeat(w.saturating_sub(used + right_w))));
        spans.push(Span::styled(r, muted));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(app: &App, f: &mut Frame, area: Rect) {
    let theme = app.theme;
    let key_w = keymap::HELP.iter().flat_map(|s| s.bindings).map(|b| width(b.keys)).max().unwrap_or(0);
    let mut lines = Vec::new();
    for section in keymap::HELP {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(section.title, Style::new().fg(theme.help_border).add_modifier(Modifier::BOLD)));
        for b in section.bindings {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<key_w$}  ", b.keys), Style::new().fg(theme.accent)),
                Span::raw(b.action),
            ]));
        }
    }
    let content_w = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let popup_w = (content_w + 4).min(area.width);
    let popup_h = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - popup_w) / 2,
        y: area.y + (area.height - popup_h) / 2,
        width: popup_w,
        height: popup_h,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.help_border))
        .title(" keys ")
        .title_bottom(Line::from(" any key to close ").right_aligned())
        .padding(ratatui::widgets::Padding::horizontal(1));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}
