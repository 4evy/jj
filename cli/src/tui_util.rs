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

use std::io;

use crossterm::ExecutableCommand as _;
use crossterm::cursor::Show;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::cleanup_guard::CleanupGuard;
use crate::command_error::CommandError;

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

/// Formats the common key-and-description footer used by terminal UIs.
pub(crate) fn key_help_line(items: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = vec![];
    for (index, (key, description)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" • "));
        }
        spans.push(Span::styled(*key, Style::default().fg(Color::Magenta)));
        spans.push(Span::raw(format!(" {description}")));
    }
    Line::from(spans)
}

/// Runs a terminal UI in raw mode on the alternate screen.
///
/// Terminal state is restored even if drawing or event handling fails.
pub(crate) fn with_terminal<T>(
    run: impl FnOnce(&mut TuiTerminal) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    io::stdout().execute(EnterAlternateScreen)?;
    let _terminal_guard = CleanupGuard::new(|| {
        drop(disable_raw_mode());
        drop(io::stdout().execute(LeaveAlternateScreen));
        drop(io::stdout().execute(Show));
    });
    enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    run(&mut terminal)
}
