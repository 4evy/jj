// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeSet;
use std::io;
use std::io::IsTerminal as _;

use ansi_to_tui::IntoText as _;
use chrono::Local;
use clap_complete::ArgValueCompleter;
use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use futures::TryStreamExt as _;
use jj_lib::backend::CommitId;
use jj_lib::backend::Timestamp;
use jj_lib::commit::Commit;
use jj_lib::graph::GraphEdge;
use jj_lib::graph::TopoGroupedGraph;
use jj_lib::repo::Repo as _;
use jj_lib::time_util::parse_datetime_expression;
use ratatui::Terminal;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::ListState;
use ratatui::widgets::Paragraph;
use rustyline::error::ReadlineError;
use tracing::instrument;

use super::metaedit;
use super::metaedit::CommitEditArgs;
use super::metaedit::CommitEditOptions;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandHelper;
use crate::cli_util::format_template;
use crate::command_error::CommandError;
use crate::command_error::internal_error;
use crate::command_error::user_error;
use crate::complete;
use crate::formatter::FormatterExt as _;
use crate::graphlog::GraphStyle;
use crate::graphlog::get_graph_renderer;
use crate::templater::TemplateRenderer;
use crate::tui_util;
use crate::ui::Ui;

/// Interactively set exact timestamps on revisions
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct RedateArgs {
    /// The revisions to redate
    #[arg(value_name = "REVSETS")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_mutable))]
    revisions_pos: Vec<RevisionArg>,

    #[arg(short = 'r', hide = true, value_name = "REVSETS")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_mutable))]
    revisions_opt: Vec<RevisionArg>,

    /// Set the timestamp without prompting
    ///
    /// Accepts exact timestamps and local-time expressions such as `now`,
    /// `yesterday 5pm`, or `2026-08-28 14:30 +03:00`.
    #[arg(long, value_name = "TIMESTAMP", value_parser = parse_timestamp_arg)]
    timestamp: Option<Timestamp>,

    /// Sign the rewritten revisions
    #[arg(long)]
    sign: bool,
}

#[instrument(skip_all)]
pub(crate) async fn cmd_redate(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &RedateArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    if args.sign && !workspace_command.repo().store().signer().can_sign() {
        return Err(user_error("No signing backend configured").hinted(
            "For configuring a signing backend, see \
             https://docs.jj-vcs.dev/latest/config/#commit-signing",
        ));
    }
    let interactive = args.revisions_pos.is_empty()
        && args.revisions_opt.is_empty()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal();
    if (interactive || args.timestamp.is_none()) && !ui.can_prompt() {
        return Err(user_error("Cannot prompt for redate input")
            .hinted("Run from a terminal, set JJ_INTERACTIVE=1, or pass --timestamp"));
    }
    let target_expression = if !args.revisions_pos.is_empty() || !args.revisions_opt.is_empty() {
        workspace_command
            .parse_union_revsets(ui, &[&*args.revisions_pos, &*args.revisions_opt].concat())?
    } else if interactive {
        let revset = workspace_command.settings().get_string("revsets.redate")?;
        workspace_command.parse_revset(ui, &RevisionArg::from(revset))?
    } else {
        workspace_command.parse_revset(ui, &RevisionArg::AT)?
    }
    .resolve()?;
    workspace_command
        .check_rewritable_expr(&target_expression)
        .await?;
    let commits = {
        let revset = target_expression.evaluate(workspace_command.repo().as_ref())?;
        let graph_stream = TopoGroupedGraph::new(revset.stream_graph(), |id| id).stream();
        graph_stream
            .and_then(async |(commit_id, edges)| {
                let commit = workspace_command
                    .repo()
                    .store()
                    .get_commit_async(&commit_id)
                    .await?;
                Ok((commit, edges))
            })
            .try_collect::<Vec<_>>()
            .await?
    };
    if commits.is_empty() {
        writeln!(ui.status(), "No revisions to redate.")?;
        return Ok(());
    }
    let commit_ids: Vec<CommitId> = if interactive {
        let graph_style = GraphStyle::from_settings(workspace_command.settings())?;
        let rows = render_rows(ui, &workspace_command, &commits)?;
        let Some(indices) = select_revisions(ui, graph_style, &rows)? else {
            return Ok(());
        };
        indices
            .into_iter()
            .map(|index| rows[index].commit_id.clone())
            .collect()
    } else {
        commits
            .iter()
            .map(|(commit, _)| commit.id().clone())
            .collect()
    };
    let timestamp = if let Some(timestamp) = args.timestamp {
        timestamp
    } else {
        prompt_timestamp(ui)?
    };
    let display_timestamp = timestamp
        .to_datetime()
        .map_err(internal_error)?
        .to_rfc3339();
    let revision_label = match commit_ids.as_slice() {
        [_] => "1 revision".to_owned(),
        revisions => format!("{} revisions", revisions.len()),
    };
    if args.timestamp.is_none()
        && !ui.prompt_yes_no(
            &format!("Redate {revision_label} to {display_timestamp}?"),
            Some(true),
        )?
    {
        return Ok(());
    }
    metaedit::edit_commits(
        ui,
        &mut workspace_command,
        &CommitEditArgs::for_redate(timestamp),
        CommitEditOptions::redate(args.sign),
        commit_ids,
    )
    .await
}

fn render_rows(
    ui: &Ui,
    workspace_command: &WorkspaceCommandHelper,
    commits: &[(Commit, Vec<GraphEdge<CommitId>>)],
) -> Result<Vec<RevisionRow>, CommandError> {
    let template = workspace_command
        .parse_commit_template(
            ui,
            &workspace_command
                .settings()
                .get_string("templates.redate")?,
        )?
        .labeled(["commit"]);
    let language = workspace_command.commit_template_language();
    let node_template: TemplateRenderer<Option<Commit>> = workspace_command
        .parse_template(
            ui,
            &language,
            &workspace_command
                .settings()
                .get_string("templates.log_node")?,
        )?
        .labeled(["redate", "commit", "node"]);
    commits
        .iter()
        .map(|(commit, edges)| {
            let mut output = vec![];
            let mut formatter = ui.new_formatter(&mut output).into_labeled("redate");
            template
                .format(commit, formatter.as_mut())
                .map_err(|error| internal_error(format!("Failed to format redate row: {error}")))?;
            drop(formatter);
            Ok(RevisionRow {
                commit_id: commit.id().clone(),
                edges: edges.clone(),
                node_symbol: format_template(ui, &Some(commit.clone()), &node_template),
                text: output,
                timestamp: commit
                    .committer()
                    .timestamp
                    .to_datetime()
                    .map_err(internal_error)?
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
            })
        })
        .collect()
}

struct RevisionRow {
    commit_id: CommitId,
    edges: Vec<GraphEdge<CommitId>>,
    node_symbol: String,
    text: Vec<u8>,
    timestamp: String,
}

const SELECTOR_HELP: &[(&str, &str)] = &[
    ("↑↓/jk", "move"),
    ("⇧↑↓", "range"),
    ("space", "toggle"),
    ("a", "all/none"),
    ("c", "confirm"),
    ("q", "cancel"),
];

const SELECTOR_HELP_COMPACT: &[(&str, &str)] = &[
    ("↑↓", "move"),
    ("⇧↑↓", "range"),
    ("space", "toggle"),
    ("c", "confirm"),
];

fn select_revisions(
    ui: &Ui,
    graph_style: GraphStyle,
    rows: &[RevisionRow],
) -> Result<Option<Vec<usize>>, CommandError> {
    tui_util::with_terminal(|terminal| run_selector(ui, terminal, graph_style, rows))
}

fn run_selector<B: ratatui::backend::Backend>(
    ui: &Ui,
    terminal: &mut Terminal<B>,
    graph_style: GraphStyle,
    rows: &[RevisionRow],
) -> Result<Option<Vec<usize>>, CommandError> {
    let mut list_state = ListState::default().with_selected(Some(0));
    let mut state = SelectorState::default();
    let help_line = tui_util::key_help_line(SELECTOR_HELP);
    let compact_help_line = tui_util::key_help_line(SELECTOR_HELP_COMPACT);

    loop {
        let rendered_rows = render_graph_rows(ui, graph_style, rows, &state)?;
        terminal
            .draw(|frame| {
                render_selector(
                    frame,
                    &rendered_rows,
                    rows,
                    &state,
                    &mut list_state,
                    &help_line,
                    &compact_help_line,
                );
            })
            .map_err(|error| internal_error(format!("Failed to draw redate TUI: {error}")))?;

        let Event::Key(key) = event::read()
            .map_err(|error| internal_error(format!("Failed to read redate TUI input: {error}")))?
        else {
            continue;
        };
        if key.is_release() {
            continue;
        }
        match state.handle_key(key.code, key.modifiers, rows.len()) {
            SelectorAction::Continue => list_state.select(Some(state.cursor)),
            SelectorAction::Submit(indices) => return Ok(Some(indices)),
            SelectorAction::Cancel => return Ok(None),
        }
    }
}

fn render_selector(
    frame: &mut ratatui::Frame<'_>,
    rendered_rows: &[Text<'static>],
    rows: &[RevisionRow],
    state: &SelectorState,
    list_state: &mut ListState,
    help_line: &Line<'static>,
    compact_help_line: &Line<'static>,
) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let timestamp_width = if areas[0].width >= 65 { 29 } else { 0 };
    let header_areas =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(timestamp_width)])
            .split(areas[0]);
    let header_style = Style::default().add_modifier(Modifier::REVERSED);
    let header = Line::from(vec![
        Span::styled(
            " Redate revisions",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}/{} selected",
            state.selected.len(),
            rows.len()
        )),
    ]);
    frame.render_widget(Paragraph::new(header).style(header_style), header_areas[0]);
    if timestamp_width > 0 {
        frame.render_widget(
            Paragraph::new(format!("committed  {} ", rows[state.cursor].timestamp))
                .alignment(Alignment::Right)
                .style(header_style),
            header_areas[1],
        );
    }

    let items = rendered_rows.iter().cloned().map(|mut row| {
        // List requires each item to fit within the viewport
        row.lines.truncate(usize::from(areas[1].height));
        ListItem::new(row)
    });
    let list = List::new(items).scroll_padding(2);
    frame.render_stateful_widget(list, areas[1], list_state);
    highlight_cursor_line(
        frame,
        areas[1],
        rendered_rows,
        state.cursor,
        list_state.offset(),
    );

    let footer = if state.invalid {
        Line::from("Select at least one revision").style(Style::default().fg(Color::Red))
    } else if help_line.width() <= usize::from(areas[2].width) {
        help_line.clone()
    } else {
        compact_help_line.clone()
    };
    frame.render_widget(Paragraph::new(footer), areas[2]);
}

fn highlight_cursor_line(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    rendered_rows: &[Text<'static>],
    cursor: usize,
    list_offset: usize,
) {
    let Some(row) = rendered_rows.get(cursor) else {
        return;
    };
    let Some(marker_line) = row.lines.iter().position(|line| {
        line.spans
            .iter()
            .any(|span| span.content.contains("[ ]") || span.content.contains("[●]"))
    }) else {
        return;
    };
    let Some(rows_before_cursor) = rendered_rows.get(list_offset..cursor) else {
        return;
    };
    let line_offset = rows_before_cursor
        .iter()
        .map(|row| row.lines.len())
        .sum::<usize>()
        + marker_line;
    if line_offset >= usize::from(area.height) {
        return;
    }
    let cursor_area = Rect::new(area.x, area.y + line_offset as u16, area.width, 1);
    frame.buffer_mut().set_style(
        cursor_area,
        Style::default().add_modifier(Modifier::REVERSED),
    );
}

fn render_graph_rows(
    ui: &Ui,
    graph_style: GraphStyle,
    rows: &[RevisionRow],
    state: &SelectorState,
) -> Result<Vec<Text<'static>>, CommandError> {
    let mut graph = get_graph_renderer(graph_style);
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let mut message = vec![];
            {
                let mut formatter = ui.new_formatter(&mut message).into_labeled("redate");
                if state.selected.contains(&index) {
                    write!(formatter.labeled("selection selected"), "[●] ")?;
                } else {
                    write!(formatter, "[ ] ")?;
                }
            }
            message.extend_from_slice(&row.text);
            graph
                .render_row(
                    &row.commit_id,
                    &row.edges,
                    &row.node_symbol,
                    &String::from_utf8_lossy(&message),
                )
                .into_bytes()
                .into_text()
                .map_err(|error| internal_error(format!("Failed to render redate graph: {error}")))
        })
        .collect()
}

#[derive(Default)]
struct SelectorState {
    cursor: usize,
    selected: BTreeSet<usize>,
    range: Option<SelectionRange>,
    invalid: bool,
}

struct SelectionRange {
    anchor: usize,
    selection_before_range: BTreeSet<usize>,
}

#[derive(Debug, PartialEq, Eq)]
enum SelectorAction {
    Continue,
    Submit(Vec<usize>),
    Cancel,
}

impl SelectorState {
    fn move_cursor(&mut self, cursor: usize) {
        self.cursor = cursor;
        self.range = None;
    }

    fn extend_range(&mut self, cursor: usize) {
        if cursor == self.cursor {
            return;
        }
        let range = self.range.get_or_insert_with(|| SelectionRange {
            anchor: self.cursor,
            selection_before_range: self.selected.clone(),
        });
        self.cursor = cursor;
        self.selected.clone_from(&range.selection_before_range);
        let start = range.anchor.min(cursor);
        let end = range.anchor.max(cursor);
        self.selected.extend(start..=end);
        self.invalid = false;
    }

    fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        row_count: usize,
    ) -> SelectorAction {
        match (code, modifiers) {
            (KeyCode::Char('q') | KeyCode::Esc, KeyModifiers::NONE)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => SelectorAction::Cancel,
            (KeyCode::Char('c'), KeyModifiers::NONE) if !self.selected.is_empty() => {
                SelectorAction::Submit(self.selected.iter().copied().collect())
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                self.invalid = true;
                SelectorAction::Continue
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_cursor((self.cursor + 1).min(row_count - 1));
                SelectorAction::Continue
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_cursor(self.cursor.saturating_sub(1));
                SelectorAction::Continue
            }
            (KeyCode::Down | KeyCode::Char('J' | 'j'), KeyModifiers::SHIFT) => {
                self.extend_range((self.cursor + 1).min(row_count - 1));
                SelectorAction::Continue
            }
            (KeyCode::Up | KeyCode::Char('K' | 'k'), KeyModifiers::SHIFT) => {
                self.extend_range(self.cursor.saturating_sub(1));
                SelectorAction::Continue
            }
            (KeyCode::Home, KeyModifiers::NONE) => {
                self.move_cursor(0);
                SelectorAction::Continue
            }
            (KeyCode::End, KeyModifiers::NONE) => {
                self.move_cursor(row_count - 1);
                SelectorAction::Continue
            }
            (KeyCode::Home, KeyModifiers::SHIFT) => {
                self.extend_range(0);
                SelectorAction::Continue
            }
            (KeyCode::End, KeyModifiers::SHIFT) => {
                self.extend_range(row_count - 1);
                SelectorAction::Continue
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                self.range = None;
                if !self.selected.insert(self.cursor) {
                    self.selected.remove(&self.cursor);
                }
                self.invalid = false;
                SelectorAction::Continue
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                self.range = None;
                if self.selected.len() == row_count {
                    self.selected.clear();
                } else {
                    self.selected.extend(0..row_count);
                }
                self.invalid = false;
                SelectorAction::Continue
            }
            _ => SelectorAction::Continue,
        }
    }
}

fn prompt_timestamp(ui: &Ui) -> Result<Timestamp, CommandError> {
    let now = Local::now();
    writeln!(
        ui.hint_default(),
        "Enter a local date and time, such as `yesterday 5pm`, or an exact timestamp."
    )?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(
            ui.prompt_choice_with("Timestamp [now]", Some("now"), |input| {
                parse_datetime_expression(input, now)
            })?,
        );
    }

    let mut editor = rustyline::DefaultEditor::new().map_err(internal_error)?;
    loop {
        let input = editor
            .readline("Timestamp [now]: ")
            .map_err(|error| match error {
                ReadlineError::Eof | ReadlineError::Interrupted => {
                    user_error("Timestamp input canceled")
                }
                ReadlineError::Io(error) => error.into(),
                error => internal_error(error),
            })?;
        let input = input.trim();
        let input = if input.is_empty() { "now" } else { input };
        match parse_datetime_expression(input, now) {
            Ok(timestamp) => return Ok(timestamp),
            Err(error) => writeln!(ui.warning_no_heading(), "{error}")?,
        }
    }
}

fn parse_timestamp_arg(input: &str) -> Result<Timestamp, String> {
    parse_datetime_expression(input, Local::now()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selector_navigation_and_submit() {
        let mut state = SelectorState::default();
        assert_eq!(
            state.handle_key(KeyCode::Down, KeyModifiers::NONE, 3),
            SelectorAction::Continue
        );
        assert_eq!(state.cursor, 1);
        state.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 3);
        state.handle_key(KeyCode::Down, KeyModifiers::NONE, 3);
        state.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 3);
        assert_eq!(
            state.handle_key(KeyCode::Enter, KeyModifiers::NONE, 3),
            SelectorAction::Continue
        );
        assert_eq!(
            state.handle_key(KeyCode::Char('c'), KeyModifiers::NONE, 3),
            SelectorAction::Submit(vec![1, 2])
        );
    }

    #[test]
    fn test_selector_bulk_and_boundary_navigation() {
        let mut state = SelectorState::default();
        state.handle_key(KeyCode::End, KeyModifiers::NONE, 3);
        assert_eq!(state.cursor, 2);
        state.handle_key(KeyCode::Home, KeyModifiers::NONE, 3);
        assert_eq!(state.cursor, 0);

        state.handle_key(KeyCode::Char('a'), KeyModifiers::NONE, 3);
        assert_eq!(state.selected, BTreeSet::from([0, 1, 2]));
        state.handle_key(KeyCode::Char('a'), KeyModifiers::NONE, 3);
        assert!(state.selected.is_empty());

        state.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 3);
        assert_eq!(
            state.handle_key(KeyCode::Char('c'), KeyModifiers::NONE, 3),
            SelectorAction::Submit(vec![0])
        );
    }

    #[test]
    fn test_selector_extends_and_shrinks_ranges() {
        let mut state = SelectorState::default();
        state.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 5);
        state.handle_key(KeyCode::Down, KeyModifiers::NONE, 5);
        state.handle_key(KeyCode::Down, KeyModifiers::NONE, 5);

        state.handle_key(KeyCode::Down, KeyModifiers::SHIFT, 5);
        assert_eq!(state.cursor, 3);
        assert_eq!(state.selected, BTreeSet::from([0, 2, 3]));
        state.handle_key(KeyCode::Char('J'), KeyModifiers::SHIFT, 5);
        assert_eq!(state.selected, BTreeSet::from([0, 2, 3, 4]));
        state.handle_key(KeyCode::Up, KeyModifiers::SHIFT, 5);
        assert_eq!(state.selected, BTreeSet::from([0, 2, 3]));
        state.handle_key(KeyCode::Char('K'), KeyModifiers::SHIFT, 5);
        assert_eq!(state.selected, BTreeSet::from([0, 2]));
        state.handle_key(KeyCode::Home, KeyModifiers::SHIFT, 5);
        assert_eq!(state.selected, BTreeSet::from([0, 1, 2]));

        state.handle_key(KeyCode::End, KeyModifiers::NONE, 5);
        state.handle_key(KeyCode::Up, KeyModifiers::SHIFT, 5);
        assert_eq!(state.selected, BTreeSet::from([0, 1, 2, 3, 4]));
    }

    #[test]
    fn test_selector_requires_selection() {
        let mut state = SelectorState::default();
        assert_eq!(
            state.handle_key(KeyCode::Char('c'), KeyModifiers::NONE, 1),
            SelectorAction::Continue
        );
        assert!(state.invalid);
        state.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 1);
        assert!(!state.invalid);
    }

    #[test]
    fn test_render_selector_keeps_tall_revision_visible() -> Result<(), CommandError> {
        let rows = [RevisionRow {
            commit_id: CommitId::from_hex("11"),
            edges: vec![],
            node_symbol: "o".to_owned(),
            text: "long description\n".repeat(20).into_bytes(),
            timestamp: "2004-01-01 00:00".to_owned(),
        }];
        let state = SelectorState::default();
        let rendered = render_graph_rows(&Ui::null(), GraphStyle::Ascii, &rows, &state)?;
        for height in 3..=6 {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(80, height)).unwrap();
            let mut list_state = ListState::default().with_selected(Some(0));
            terminal
                .draw(|frame| {
                    render_selector(
                        frame,
                        &rendered,
                        &rows,
                        &state,
                        &mut list_state,
                        &tui_util::key_help_line(SELECTOR_HELP),
                        &tui_util::key_help_line(SELECTOR_HELP_COMPACT),
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let marker: String = (3..6).map(|x| buffer[(x, 1)].symbol()).collect();
            assert_eq!(
                marker, "[ ]",
                "missing revision at terminal height {height}"
            );
            assert!(buffer[(0, 1)].modifier.contains(Modifier::REVERSED));
        }
        Ok(())
    }

    #[test]
    fn test_render_revision_graph() -> Result<(), CommandError> {
        let root_id = CommitId::from_hex("00");
        let left_id = CommitId::from_hex("11");
        let right_id = CommitId::from_hex("22");
        let merge_id = CommitId::from_hex("33");
        let rows = vec![
            RevisionRow {
                commit_id: merge_id,
                edges: vec![
                    GraphEdge::direct(left_id.clone()),
                    GraphEdge::direct(right_id.clone()),
                ],
                node_symbol: "@".to_owned(),
                text: b"merge".to_vec(),
                timestamp: "2004-01-01 00:00".to_owned(),
            },
            RevisionRow {
                commit_id: right_id,
                edges: vec![GraphEdge::direct(root_id.clone())],
                node_symbol: "o".to_owned(),
                text: b"right".to_vec(),
                timestamp: "2003-01-01 00:00".to_owned(),
            },
            RevisionRow {
                commit_id: left_id,
                edges: vec![GraphEdge::direct(root_id.clone())],
                node_symbol: "o".to_owned(),
                text: b"left".to_vec(),
                timestamp: "2002-01-01 00:00".to_owned(),
            },
            RevisionRow {
                commit_id: root_id,
                edges: vec![],
                node_symbol: "o".to_owned(),
                text: b"root".to_vec(),
                timestamp: "2001-01-01 00:00".to_owned(),
            },
        ];
        let mut state = SelectorState::default();
        state.selected.insert(1);
        let rendered = render_graph_rows(&Ui::null(), GraphStyle::Ascii, &rows, &state)?;
        let mut output = String::new();
        for text in &rendered {
            for line in &text.lines {
                for span in &line.spans {
                    output.push_str(&span.content);
                }
                output.push('\n');
            }
        }
        insta::assert_snapshot!(output, @r"
        @    [ ] merge
        |\
        | o  [●] right
        o |  [ ] left
        |/
        o  [ ] root
        ");

        let render_selector_snapshot = |width, height| {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut list_state = ListState::default().with_selected(Some(0));
            terminal
                .draw(|frame| {
                    let help_line = tui_util::key_help_line(SELECTOR_HELP);
                    let compact_help_line = tui_util::key_help_line(SELECTOR_HELP_COMPACT);
                    render_selector(
                        frame,
                        &rendered,
                        &rows,
                        &state,
                        &mut list_state,
                        &help_line,
                        &compact_help_line,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!((0..width).all(|x| buffer[(x, 1)].modifier.contains(Modifier::REVERSED)));
            assert!(!buffer[(0, 2)].modifier.contains(Modifier::REVERSED));
            assert!(!buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
            let mut output = String::new();
            for y in 0..buffer.area.height {
                let mut line = String::new();
                for x in 0..buffer.area.width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                output.push_str(line.trim_end());
                output.push('\n');
            }
            output
        };
        let output = render_selector_snapshot(80, 9);
        insta::assert_snapshot!(output, @r"
         Redate revisions  1/4 selected                     committed  2004-01-01 00:00
        @    [ ] merge
        |\
        | o  [●] right
        o |  [ ] left
        |/
        o  [ ] root

        ↑↓/jk move • ⇧↑↓ range • space toggle • a all/none • c confirm • q cancel
        ");

        let output = render_selector_snapshot(50, 9);
        insta::assert_snapshot!(output, @r"
         Redate revisions  1/4 selected
        @    [ ] merge
        |\
        | o  [●] right
        o |  [ ] left
        |/
        o  [ ] root

        ↑↓ move • ⇧↑↓ range • space toggle • c confirm
        ");
        Ok(())
    }
}
