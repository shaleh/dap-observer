//! The terminal face: the event loop, the immediate-mode rendering, and the key
//! map. This is the only layer that knows about the terminal. It owns the
//! terminal and restores it on every exit path, maps keys to model actions, draws
//! every pane as a function of the model each frame, and runs the DAP work the
//! update step asks for.

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use dap_client::dap::{ConnEvent, DapClient, initialize};
use dap_client::model::{
    SessionState, build_scope_roots, drive, evaluate, evaluate_watch, fetch_children, resolve_stop,
};

use crate::model::{DebuggerModel, HELP_LINES, InputMode, Row, RowKind, Tone, Tree};
use crate::update::{Action, Effect, Update, UpdateKind, apply_action, apply_event, apply_update};

/// Connect-time handshake and the single event loop over terminal key events and
/// DAP events. Owns the terminal and restores it on every exit path.
pub async fn run(client: DapClient, mut events: UnboundedReceiver<ConnEvent>) -> Result<()> {
    // The minimal late-join handshake. The mux then replays the current stopped
    // state.
    initialize(&client).await?;

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    let (update_tx, mut update_rx) = unbounded_channel();
    let mut model = DebuggerModel::new();
    let mut vars_state = ListState::default();
    let mut stack_state = ListState::default();
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    while !model.should_quit {
        // A draw failure means the terminal is gone, for example a dropped SSH
        // session. We are still attached to the shared mux, so we end the loop
        // and disconnect on the way out the same as any other exit.
        if terminal
            .draw(|f| render(&client, &model, &mut vars_state, &mut stack_state, f))
            .is_err()
        {
            break;
        }

        tokio::select! {
            input = term_events.next() => match input {
                Some(Ok(Event::Key(key))) => {
                    for action in keymap(key, &model) {
                        let effects = apply_action(&mut model, action);
                        run_effects(effects, &client, &update_tx);
                    }
                }
                Some(Err(_)) | None => model.should_quit = true,
                _ => {}
            },
            conn = events.recv() => match conn {
                Some(ConnEvent::Dap(event)) => {
                    let effects = apply_event(&mut model, event);
                    run_effects(effects, &client, &update_tx);
                }
                Some(ConnEvent::Disconnected(err)) => on_disconnect(&mut model, err),
                None => on_disconnect(&mut model, None),
            },
            upd = update_rx.recv() => {
                if let Some(update) = upd {
                    let effects = apply_update(&mut model, update);
                    run_effects(effects, &client, &update_tx);
                }
            }
            _ = tick.tick() => {}
        }
    }

    client.disconnect().await;
    Ok(())
}

fn on_disconnect(model: &mut DebuggerModel, err: Option<String>) {
    model.state = SessionState::Ended;
    model.status = match err {
        Some(e) => format!("disconnected: {e}"),
        None => "connection closed".to_string(),
    };
    model.should_quit = true;
}

/// Map a keypress to model actions. Printable keys edit the expression line in
/// insert mode. In normal mode single keys navigate, run control, and pin
/// watches, so no command needs typing.
fn keymap(key: KeyEvent, model: &DebuggerModel) -> Vec<Action> {
    if key.kind != KeyEventKind::Press {
        return vec![];
    }
    // Ctrl-C quits from any mode.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return vec![Action::Quit];
    }

    match model.mode {
        InputMode::Insert => match key.code {
            KeyCode::Esc => vec![Action::InputCancel],
            KeyCode::Enter => vec![Action::InputSubmit],
            KeyCode::Backspace => vec![Action::InputBackspace],
            KeyCode::Char(c) => vec![Action::InputChar(c)],
            _ => vec![],
        },
        InputMode::Normal => {
            // The help overlay swallows the next key to dismiss itself.
            if model.show_help {
                return match key.code {
                    KeyCode::Char('q') => vec![Action::Quit],
                    _ => vec![Action::ToggleHelp],
                };
            }
            match key.code {
                KeyCode::Char('q') => vec![Action::Quit],
                KeyCode::Char('i') => vec![Action::EnterInsert],
                KeyCode::Char('?') => vec![Action::ToggleHelp],
                KeyCode::Char('j') | KeyCode::Down => vec![Action::RowDown],
                KeyCode::Char('k') | KeyCode::Up => vec![Action::RowUp],
                KeyCode::Char('h') | KeyCode::Left => vec![Action::Collapse],
                KeyCode::Char('l') | KeyCode::Right => vec![Action::Expand],
                KeyCode::Enter | KeyCode::Char(' ') => vec![Action::ToggleExpand],
                KeyCode::Char('g') => vec![Action::RowFirst],
                KeyCode::Char('G') => vec![Action::RowLast],
                KeyCode::Char('K') => vec![Action::SelectCallerFrame],
                KeyCode::Char('J') => vec![Action::SelectCalleeFrame],
                KeyCode::Char('w') => vec![Action::ToggleWatch],
                KeyCode::Char('c') => vec![Action::Continue],
                KeyCode::Char('n') => vec![Action::StepOver],
                KeyCode::Char('s') => vec![Action::StepInto],
                KeyCode::Char('o') => vec![Action::StepOut],
                KeyCode::Char('p') => vec![Action::Pause],
                _ => vec![],
            }
        }
    }
}

/// Spawn the DAP work an interpretation asked for. Each reply is tagged with the
/// epoch the effect was issued under, so the update step can drop a reply for a
/// superseded frame view. A drive that succeeds reports nothing, since its stop
/// or resume arrives later as an event.
fn run_effects(effects: Vec<Effect>, client: &DapClient, update_tx: &UnboundedSender<Update>) {
    for effect in effects {
        let client = client.clone();
        let tx = update_tx.clone();
        match effect {
            Effect::ResolveStop { thread_hint, epoch } => {
                tokio::spawn(async move {
                    let kind = match resolve_stop(&client, thread_hint).await {
                        Ok(resolution) => UpdateKind::Stopped {
                            thread_id: resolution.thread_id,
                            stack: resolution.stack,
                            frame_id: resolution.frame_id,
                            roots: resolution.roots,
                        },
                        Err(e) => UpdateKind::Failed {
                            message: e.to_string(),
                        },
                    };
                    let _ = tx.send(Update { epoch, kind });
                });
            }
            Effect::FetchScopes { frame_id, epoch } => {
                tokio::spawn(async move {
                    let kind = match build_scope_roots(&client, frame_id).await {
                        Ok(roots) => UpdateKind::Scopes { roots },
                        Err(e) => UpdateKind::Failed {
                            message: e.to_string(),
                        },
                    };
                    let _ = tx.send(Update { epoch, kind });
                });
            }
            Effect::FetchChildren {
                target,
                var_ref,
                epoch,
            } => {
                tokio::spawn(async move {
                    let children = fetch_children(&client, var_ref).await;
                    let _ = tx.send(Update {
                        epoch,
                        kind: UpdateKind::Children { target, children },
                    });
                });
            }
            Effect::Evaluate {
                expression,
                frame_id,
                epoch,
            } => {
                tokio::spawn(async move {
                    let result = evaluate(&client, &expression, frame_id)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Update {
                        epoch,
                        kind: UpdateKind::Evaluated { result },
                    });
                });
            }
            Effect::EvalWatch {
                expression,
                frame_id,
                epoch,
            } => {
                tokio::spawn(async move {
                    let node = evaluate_watch(&client, &expression, frame_id).await.ok();
                    let _ = tx.send(Update {
                        epoch,
                        kind: UpdateKind::Watch { expression, node },
                    });
                });
            }
            Effect::Drive {
                command,
                thread_id,
                epoch,
            } => {
                tokio::spawn(async move {
                    if let Err(e) = drive(&client, command, thread_id).await {
                        let _ = tx.send(Update {
                            epoch,
                            kind: UpdateKind::DriveFailed {
                                message: e.to_string(),
                            },
                        });
                    }
                });
            }
        }
    }
}

fn render(
    client: &DapClient,
    model: &DebuggerModel,
    vars_state: &mut ListState,
    stack_state: &mut ListState,
    f: &mut ratatui::Frame,
) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    render_header(client, model, f, chunks[0]);

    let body =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Min(1)]).split(chunks[1]);
    render_stack(model, stack_state, f, body[0]);
    render_variables(model, vars_state, f, body[1]);

    render_input(model, f, chunks[2]);
    render_status(model, f, chunks[3]);

    if model.show_help {
        render_help(f, f.area());
    }
}

fn render_header(client: &DapClient, model: &DebuggerModel, f: &mut ratatui::Frame, area: Rect) {
    let (label, color) = match model.state {
        SessionState::Connecting => ("● connecting…", Color::Yellow),
        SessionState::Running => ("▶ RUNNING — variables not live", Color::Yellow),
        SessionState::Stopped => ("● STOPPED — live", Color::Green),
        SessionState::Ended => ("■ SESSION ENDED", Color::Red),
    };

    let frame_line = match model.selected_stack_frame() {
        Some(frame) => {
            let location = match frame
                .source
                .as_ref()
                .and_then(|s| s.name.clone().or(s.path.clone()))
            {
                Some(source) => format!("{source}:{}", frame.line),
                None => format!("line {}", frame.line),
            };
            Line::from(format!(
                "#{} {}  @ {}   (stop #{}, {})",
                model.selected_frame, frame.name, location, model.stop_count, model.stop_reason
            ))
        }
        None => Line::from(Span::styled(
            "no frame",
            Style::default().add_modifier(Modifier::DIM),
        )),
    };

    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} @ {} ",
        env!("CARGO_BIN_NAME"),
        client.address()
    ));
    let body = vec![
        Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        frame_line,
    ];
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn render_stack(
    model: &DebuggerModel,
    stack_state: &mut ListState,
    f: &mut ratatui::Frame,
    area: Rect,
) {
    let items: Vec<ListItem> = model
        .stack
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let marker = if index == model.selected_frame {
                "▸ "
            } else {
                "  "
            };
            ListItem::new(Line::from(format!(
                "{marker}#{index} {} @ {}",
                frame.name, frame.line
            )))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" stack "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    stack_state.select(if model.stack.is_empty() {
        None
    } else {
        Some(model.selected_frame)
    });
    f.render_stateful_widget(list, area, stack_state);
}

fn render_variables(
    model: &DebuggerModel,
    vars_state: &mut ListState,
    f: &mut ratatui::Frame,
    area: Rect,
) {
    let live = model.state == SessionState::Stopped;
    let items: Vec<ListItem> = model
        .rows
        .iter()
        .map(|row| row_list_item(row, live))
        .collect();

    let title = match model.state {
        SessionState::Running => " variables (stale — running) ",
        SessionState::Ended => " variables (session ended) ",
        _ => " variables ",
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");

    vars_state.select(if model.rows.is_empty() {
        None
    } else {
        Some(model.selected_row)
    });
    f.render_stateful_widget(list, area, vars_state);
}

fn render_input(model: &DebuggerModel, f: &mut ratatui::Frame, area: Rect) {
    let line = match model.mode {
        InputMode::Insert => Line::from(vec![
            Span::styled("(dap) ", Style::default().fg(Color::Green)),
            Span::raw(model.input.clone()),
            Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]),
        InputMode::Normal => Line::from(Span::styled(
            "(dap) press i to evaluate an expression",
            Style::default().add_modifier(Modifier::DIM),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn render_status(model: &DebuggerModel, f: &mut ratatui::Frame, area: Rect) {
    let hint =
        "j/k move · h/l expand · K/J frame · w watch · c/n/s/o run · p pause · ? help · q quit";
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", model.status),
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_help(f: &mut ratatui::Frame, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = (HELP_LINES.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = HELP_LINES.iter().map(|l| Line::from(*l)).collect();
    let block = Block::default().borders(Borders::ALL).title(" keys ");
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Render one flattened row: a section divider, a transcript message, a pending
/// watch placeholder, or a real node. `live` dims node rows when the session is
/// not stopped, marking the variables stale.
fn row_list_item(row: &Row, live: bool) -> ListItem<'static> {
    match row.kind {
        RowKind::Header => ListItem::new(Line::from(Span::styled(
            format!("── {} ──", row.name),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))),
        RowKind::Message => {
            let style = match row.tone {
                Tone::Error => Style::default().fg(Color::Red),
                _ => Style::default().add_modifier(Modifier::DIM),
            };
            ListItem::new(Line::from(Span::styled(row.name.clone(), style)))
        }
        RowKind::Placeholder => {
            let indent = "  ".repeat(row.depth);
            ListItem::new(Line::from(vec![
                Span::raw(format!("{indent}★ ")),
                Span::styled(
                    row.name.clone(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  {}", row.value),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]))
        }
        RowKind::Node => {
            let mut prefix = "  ".repeat(row.depth);
            if row.watched {
                prefix.push_str("★ ");
            }
            prefix.push_str(if row.expandable {
                if row.expanded { "▾ " } else { "▸ " }
            } else {
                "  "
            });

            let mut name_style = name_color(row.tree);
            if !live {
                name_style = name_style.add_modifier(Modifier::DIM);
            }
            let mut spans = vec![
                Span::raw(prefix),
                Span::styled(row.name.clone(), name_style),
            ];
            if !row.ty.is_empty() {
                spans.push(Span::styled(
                    format!(" : {}", row.ty),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !row.value.is_empty() {
                spans.push(Span::raw(format!(" = {}", row.value)));
            }
            if row.expandable && row.expanded && !row.fetched {
                spans.push(Span::styled(
                    " …",
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            ListItem::new(Line::from(spans))
        }
    }
}

fn name_color(tree: Tree) -> Style {
    match tree {
        Tree::Result => Style::default().fg(Color::Magenta),
        _ => Style::default().fg(Color::Cyan),
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

/// Best-effort terminal restore. Safe to call more than once.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// RAII guard on the terminal.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Restore the terminal before a panic prints, so a crash never leaves it wedged.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}
