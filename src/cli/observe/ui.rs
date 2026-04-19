//! ratatui rendering for the observe TUI.

use chrono::Local;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::super::event_fmt::{format_event, EventColor};
use super::super::runs_data::{format_duration, RunStatus, RunSummary};
use super::app::{App, ForkPrompt, ForkState, Focus};
use crate::events::Event;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(32),
            Constraint::Percentage(40),
        ])
        .split(outer[0]);

    draw_runs(frame, app, cols[0]);
    draw_graph(frame, app, cols[1]);
    draw_events(frame, app, cols[2]);
    draw_help(frame, app, outer[1]);

    if let Some(prompt) = app.fork_prompt.clone() {
        draw_fork_popup(frame, &prompt, frame.area());
    }
}

fn focus_border(focus: Focus, this: Focus) -> Style {
    if focus == this {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_runs(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .runs
        .iter()
        .map(run_list_item)
        .collect();

    let title = if app.follow_tail {
        format!("Runs ({}) · follow", app.runs.len())
    } else {
        format!("Runs ({})", app.runs.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(focus_border(app.focus, Focus::Runs)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 55, 90))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("» ");

    let mut state = ListState::default();
    if !app.runs.is_empty() {
        state.select(Some(app.run_cursor));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn run_list_item(run: &RunSummary) -> ListItem<'static> {
    let status_color = match run.status {
        RunStatus::Running => Color::Yellow,
        RunStatus::Ok => Color::Green,
        RunStatus::Failed => Color::Red,
    };
    let started = run.started_at.with_timezone(&Local).format("%m-%d %H:%M");
    let dur = run
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "—".into());

    let header = Line::from(vec![
        Span::styled(
            format!("{} ", run.status.glyph()),
            Style::default().fg(status_color),
        ),
        Span::styled(
            run.pipeline.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{started} · {dur}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let preview = Line::from(Span::styled(
        format!("  {}", truncate(&run.prompt_preview, 80)),
        Style::default().fg(Color::DarkGray),
    ));

    let mut lines = vec![header, preview];
    if let Some(fork) = &run.fork {
        let parent_short = shorten_id(&fork.parent_stream_id, 24);
        lines.push(Line::from(Span::styled(
            format!("  ↩ from {parent_short}:{}", fork.fork_at_step),
            Style::default().fg(Color::Magenta),
        )));
    }

    ListItem::new(lines)
}

fn shorten_id(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(10).collect();
    let tail: String = s.chars().skip(len - 12).collect();
    format!("{head}…{tail}")
}

fn draw_graph(frame: &mut Frame, app: &App, area: Rect) {
    let title = match app.runs.get(app.run_cursor) {
        Some(r) => format!("Pipeline: {}", r.pipeline),
        None => "Pipeline".into(),
    };

    let mut lines: Vec<Line> = Vec::new();
    if let Some(warning) = &app.graph_warning {
        lines.push(Line::from(Span::styled(
            warning.clone(),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(""));
    }

    if let Some(graph) = &app.graph {
        let selected = if app.focus == Focus::Graph {
            Some(app.graph_cursor)
        } else {
            None
        };
        let content_width = area.width.saturating_sub(2) as usize;
        for line in graph.render_lines(selected, content_width) {
            lines.push(line);
        }
        lines.push(Line::from(""));
        if let Some(node) = graph.node_at(app.graph_cursor) {
            lines.push(Line::from(vec![
                Span::styled("node: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    node.id.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
            if !node.agent.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("agent: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(node.agent.clone()),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("status: ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{} {:?}", node.status.glyph(), node.status)),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "no pipeline selected",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(focus_border(app.focus, Focus::Graph)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn draw_events(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = format!("Events ({})", app.events.len());

    let items: Vec<ListItem> = app
        .events
        .iter()
        .enumerate()
        .map(|(idx, event)| {
            let formatted = format_event(event);
            let pad = "  ".repeat(formatted.indent as usize);
            let color = color_to_ratatui(formatted.color);
            let ts = event.timestamp.with_timezone(&Local).format("%H:%M:%S");
            let header = Line::from(vec![
                Span::styled(
                    format!("{pad}{} ", formatted.icon),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{ts} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    formatted.label.clone(),
                    Style::default().add_modifier(Modifier::BOLD).fg(color),
                ),
            ]);

            let mut lines = vec![header];

            let body = if app.expanded_events.contains(&idx) {
                formatted.full_detail
            } else {
                formatted.short_detail
            };
            for line in body.lines() {
                if line.is_empty() {
                    continue;
                }
                lines.push(Line::from(Span::styled(
                    format!("{pad}  {line}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }

            ListItem::new(lines)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(focus_border(app.focus, Focus::Events)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 55, 90))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("» ");

    let mut state = ListState::default();
    if !app.events.is_empty() {
        state.select(Some(app.event_cursor));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let follow = if app.follow_tail { "on" } else { "off" };
    let text = format!(
        "q quit  Tab cycle  ↑↓ nav  Enter expand/focus  f follow={follow}  r refresh  R fork-resume"
    );
    let paragraph =
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

fn draw_fork_popup(frame: &mut Frame, prompt: &ForkPrompt, screen: Rect) {
    // Running state gets a taller popup to fit the live log.
    let (pct_x, pct_y) = match &prompt.state {
        ForkState::Running => (80, 65),
        _ => (70, 40),
    };
    let area = centered_rect(pct_x, pct_y, screen);
    frame.render_widget(Clear, area);

    let (title, body, hint, color) = match &prompt.state {
        ForkState::Confirm => (
            "Fork-resume",
            vec![
                Line::from(vec![
                    Span::raw("Fork "),
                    Span::styled(
                        truncate(&prompt.parent_stream_id, 50).to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("from step "),
                    Span::styled(
                        prompt.fork_at_step.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("?"),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Steps upstream of the fork are frozen (events copied);",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "this step + DAG-descendants re-run against current configs.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "No approval handler is attached — steps tagged",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "RequiresHumanApproval will be denied (fail-closed).",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            "[Enter] confirm   [Esc] cancel",
            Color::Cyan,
        ),
        ForkState::Running => {
            let mut lines = vec![
                Line::from(format!("Forking from {}", prompt.fork_at_step)),
                Line::from(Span::styled(
                    "Re-executing downstream steps… this may take a while.",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            if prompt.tail_events.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "(waiting for new stream to appear…)",
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )));
            } else {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "— live log —",
                    Style::default().fg(Color::DarkGray),
                )));
                // Last ~14 events, oldest first.
                let tail: Vec<&Event> = prompt
                    .tail_events
                    .iter()
                    .rev()
                    .take(14)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                for ev in tail {
                    lines.push(fork_log_line(ev));
                }
            }
            (
                "Fork-resume — running",
                lines,
                "(working — Esc to dismiss popup; fork keeps running)",
                Color::Yellow,
            )
        }
        ForkState::Done {
            new_stream_id,
            message,
        } => (
            "Fork-resume — done",
            vec![
                Line::from(Span::styled(
                    message.clone(),
                    Style::default().fg(Color::Green),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    truncate(new_stream_id, 60).to_string(),
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            "[Enter] open new run   [Esc] dismiss",
            Color::Green,
        ),
        ForkState::Failed(err) => (
            "Fork-resume — failed",
            vec![Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            ))],
            "[Enter/Esc] dismiss",
            Color::Red,
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .title(title);

    let mut lines = body;
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    )));

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn fork_log_line(event: &Event) -> Line<'static> {
    let formatted = format_event(event);
    let color = color_to_ratatui(formatted.color);
    let ts = event.timestamp.with_timezone(&Local).format("%H:%M:%S");
    let detail = truncate(&formatted.short_detail, 80).to_string();
    Line::from(vec![
        Span::styled(format!("{} ", formatted.icon), Style::default().fg(color)),
        Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
        Span::styled(
            formatted.label.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
    ])
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn color_to_ratatui(c: EventColor) -> Color {
    match c {
        EventColor::Default => Color::Reset,
        EventColor::Success => Color::Green,
        EventColor::Failure => Color::Red,
        EventColor::Warning => Color::Yellow,
        EventColor::Info => Color::Cyan,
        EventColor::Highlight => Color::Magenta,
        EventColor::Dim => Color::DarkGray,
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}
