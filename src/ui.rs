use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::dap::types::{EventMessage, StoppedBody};
use crate::dap::{ConnEvent, DapClient, initialize_args};
use crate::model::{
    FrameContext, FrameHeader, SessionState, VarNode, build_frame, evaluate_watch, fetch_children,
};

/// Result of an async fetch, tagged with the epoch it belongs to so stale
/// replies can be discarded uniformly.
struct Update {
    epoch: u64,
    kind: UpdateKind,
}

enum UpdateKind {
    Frame {
        ctx: Option<FrameContext>,
    },
    Children {
        target: FetchTarget,
        children: Vec<VarNode>,
    },
    /// A watch finished evaluating: `Some` resolved, `None` out of scope.
    Watch {
        expr: String,
        node: Option<VarNode>,
    },
    Error {
        message: String,
    },
}

/// Which forest a fetched node lives in, located stably so the reply lands on
/// the right node even if the watch list changed while the fetch was in flight.
enum FetchTarget {
    /// Scope tree: positional path (scopes are stable within a stop).
    Scope { path: Vec<usize> },
    /// Watch subtree: the (stable) expression plus a path below its root node.
    Watch { expr: String, subpath: Vec<usize> },
}

/// Which forest a visible row belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tree {
    Watch,
    Scope,
}

/// A pinned watch: a durable expression plus the most recent evaluation.
struct Watch {
    expr: String,
    state: WatchState,
}

/// The evaluation state of a watch at the current stop.
enum WatchState {
    /// Evaluation in flight (or not yet started this stop).
    Pending,
    /// Resolved to a value/subtree.
    Resolved(VarNode),
    /// Did not resolve in the current frame (e.g. out of scope).
    Unavailable,
}

/// A flattened, currently-visible row (respects expansion state).
///
/// `path` locates the backing node within its `tree`: for scopes it indexes
/// `roots`; for watches `path[0]` indexes `watches` and the rest descends the
/// resolved node's children. Rows are addressed by position, not by
/// `variablesReference`, so a watched variable and its in-tree twin (which some
/// adapters report with the same reference) stay independently selectable.
struct Row {
    tree: Tree,
    path: Vec<usize>,
    depth: usize,
    name: String,
    value: String,
    ty: String,
    eval_name: String,
    expandable: bool,
    expanded: bool,
    fetched: bool,
    /// False for the non-selectable section header.
    selectable: bool,
    /// A watch root row, prefixed with a pin marker.
    watched: bool,
    /// A pending/unavailable watch placeholder: `value` holds the status text.
    placeholder: bool,
}

struct App {
    client: DapClient,
    state: SessionState,
    /// Increments on every `stopped`. Tags async work to discard stale replies.
    epoch: u64,
    stop_count: u64,
    header: Option<FrameHeader>,
    /// Current frame id, needed to `evaluate` watches. Set when a frame
    /// resolves, cleared on stop/idle.
    frame_id: Option<i64>,
    roots: Vec<VarNode>,
    /// Durable watch list: the `expr`s survive re-rooting; each `state` is
    /// ephemeral and re-evaluated every stop.
    watches: Vec<Watch>,
    selected: usize,
    status: String,
    should_quit: bool,
    list_state: ListState,
    update_tx: tokio::sync::mpsc::UnboundedSender<Update>,
}

/// Connect-time handshake + UI event loop. Owns the terminal and restores it on
/// every exit path.
pub async fn run(
    client: DapClient,
    mut events: tokio::sync::mpsc::UnboundedReceiver<ConnEvent>,
) -> Result<()> {
    client
        .request("initialize", Some(initialize_args()))
        .await
        .context("initialize handshake failed")?;

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(client.clone(), update_tx);
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    while !app.should_quit {
        terminal.draw(|f| app.render(f))?;
        tokio::select! {
            input = term_events.next() => match input {
                Some(Ok(Event::Key(key))) => app.on_key(key),
                Some(Err(_)) | None => app.should_quit = true,
                _ => {}
            },
            conn = events.recv() => match conn {
                Some(ConnEvent::Dap(ev)) => app.on_dap_event(ev),
                Some(ConnEvent::Disconnected(err)) => app.on_disconnect(err),
                None => app.on_disconnect(None),
            },
            upd = update_rx.recv() => {
                if let Some(u) = upd {
                    app.on_update(u);
                }
            }
            _ = tick.tick() => {}
        }
    }

    // Clean exit: non-terminating disconnect, then the guard restores the
    // terminal as it drops.
    client.disconnect();
    Ok(())
}

impl App {
    fn new(client: DapClient, update_tx: tokio::sync::mpsc::UnboundedSender<Update>) -> Self {
        App {
            client,
            state: SessionState::Connecting,
            epoch: 0,
            stop_count: 0,
            header: None,
            frame_id: None,
            roots: Vec::new(),
            watches: Vec::new(),
            selected: 0,
            status: "waiting for the program to stop (breakpoint or step)…".to_string(),
            should_quit: false,
            list_state: ListState::default(),
            update_tx,
        }
    }

    fn on_dap_event(&mut self, ev: EventMessage) {
        match ev.event.as_str() {
            "stopped" => {
                // Re-root: new epoch, drop the old tree, rebuild from this stop.
                self.epoch += 1;
                self.stop_count += 1;
                self.state = SessionState::Stopped;
                self.roots.clear();
                self.header = None;
                self.frame_id = None;
                // Watches persist, but their values are re-evaluated once the
                // frame resolves (see the Frame update handler).
                for w in &mut self.watches {
                    w.state = WatchState::Pending;
                }
                self.selected = 0;
                self.status = "stopped — resolving frame…".to_string();

                let body: StoppedBody = ev
                    .body
                    .and_then(|b| serde_json::from_value(b).ok())
                    .unwrap_or_default();
                let epoch = self.epoch;
                let stop_number = self.stop_count;
                let client = self.client.clone();
                let tx = self.update_tx.clone();
                tokio::spawn(async move {
                    let kind = match build_frame(&client, body, stop_number).await {
                        Ok(ctx) => UpdateKind::Frame { ctx },
                        Err(e) => UpdateKind::Error {
                            message: e.to_string(),
                        },
                    };
                    let _ = tx.send(Update { epoch, kind });
                });
            }
            "continued" => {
                // Program resumed: the last stop's handles are now invalid,
                // so we can't fetch live data. We intentionally do NOT clear
                // the tree here, leaving the last tree on screen — just flagged
                // as stale. The next `stopped` re-roots it.
                self.state = SessionState::Running;
                self.status = "running — variables stale".to_string();
            }
            "terminated" | "exited" => {
                self.state = SessionState::Ended;
                self.status = "session ended".to_string();
            }
            _ => {}
        }
    }

    fn on_disconnect(&mut self, err: Option<String>) {
        self.state = SessionState::Ended;
        self.status = match err {
            Some(e) => format!("disconnected: {e}"),
            None => "connection closed".to_string(),
        };
        // Surface and exit cleanly rather than hang.
        self.should_quit = true;
    }

    fn on_update(&mut self, update: Update) {
        // Discard anything tagged with a superseded epoch.
        if update.epoch != self.epoch {
            return;
        }
        match update.kind {
            UpdateKind::Frame { ctx } => {
                match ctx {
                    Some(c) => {
                        self.header = Some(c.header);
                        self.frame_id = Some(c.frame_id);
                        self.roots = c.roots;
                        self.status = "stopped".to_string();
                        // Now that the frame is known, evaluate every watch
                        // against it. Collect first to avoid borrowing `self`
                        // mutably and immutably at once.
                        let exprs: Vec<String> =
                            self.watches.iter().map(|w| w.expr.clone()).collect();
                        for expr in exprs {
                            self.spawn_watch(expr, c.frame_id);
                        }
                    }
                    None => {
                        self.frame_id = None;
                        self.status = "stopped — no frames (idle)".to_string();
                    }
                }
                self.clamp_selection();
            }
            UpdateKind::Children { target, children } => {
                let node = match &target {
                    FetchTarget::Scope { path } => self.scope_node_mut(path),
                    FetchTarget::Watch { expr, subpath } => {
                        self.watch_node_mut_by_expr(expr, subpath)
                    }
                };
                if let Some(node) = node {
                    node.children = Some(children);
                }
                self.clamp_selection();
            }
            UpdateKind::Watch { expr, node } => {
                if let Some(w) = self.watches.iter_mut().find(|w| w.expr == expr) {
                    w.state = match node {
                        Some(n) => WatchState::Resolved(n),
                        None => WatchState::Unavailable,
                    };
                }
                self.clamp_selection();
            }
            UpdateKind::Error { message } => self.status = format!("error: {message}"),
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let rows = self.visible_rows();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Down | KeyCode::Char('j') => self.step_selection(&rows, true),
            KeyCode::Up | KeyCode::Char('k') => self.step_selection(&rows, false),
            KeyCode::Char('g') => self.selected = first_selectable(&rows),
            KeyCode::Char('G') => self.selected = last_selectable(&rows),
            KeyCode::Enter | KeyCode::Char(' ') => self.set_expanded(&rows, ExpandAction::Toggle),
            KeyCode::Right | KeyCode::Char('l') => self.set_expanded(&rows, ExpandAction::Expand),
            KeyCode::Left | KeyCode::Char('h') => self.set_expanded(&rows, ExpandAction::Collapse),
            KeyCode::Char('w') => self.toggle_watch(&rows),
            _ => {}
        }
    }

    /// Move the selection to the next/previous selectable row, skipping the
    /// non-selectable section header.
    fn step_selection(&mut self, rows: &[Row], forward: bool) {
        let n = rows.len() as isize;
        let mut i = self.selected as isize;
        loop {
            i += if forward { 1 } else { -1 };
            if i < 0 || i >= n {
                return;
            }
            if rows[i as usize].selectable {
                self.selected = i as usize;
                return;
            }
        }
    }

    /// Toggle a watch with `w`: from a watched row, unpin it; from a scope
    /// variable, pin or unpin it by its `evaluateName`. No-op without one.
    fn toggle_watch(&mut self, rows: &[Row]) {
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if row.tree == Tree::Watch {
            // Unpin the selected watch (its root is the first path element).
            if let Some(&wi) = row.path.first()
                && wi < self.watches.len()
            {
                self.watches.remove(wi);
            }
            self.clamp_selection();
            return;
        }
        let expr = row.eval_name.clone();
        if expr.is_empty() {
            return;
        }
        if let Some(pos) = self.watches.iter().position(|w| w.expr == expr) {
            self.watches.remove(pos);
        } else {
            self.watches.push(Watch {
                expr: expr.clone(),
                state: WatchState::Pending,
            });
            // Pinned while stopped: evaluate immediately against the live frame.
            if self.state == SessionState::Stopped
                && let Some(fid) = self.frame_id
            {
                self.spawn_watch(expr, fid);
            }
        }
        self.clamp_selection();
    }

    /// Expand/collapse the selected node, triggering a lazy fetch on first
    /// expand. Non-expandable leaves issue no request.
    fn set_expanded(&mut self, rows: &[Row], action: ExpandAction) {
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if !row.expandable {
            return;
        }
        let tree = row.tree;
        let path = row.path.clone();
        let node = match tree {
            Tree::Scope => self.scope_node_mut(&path),
            Tree::Watch => self.watch_node_mut(&path),
        };
        let mut fetch_ref = None;
        if let Some(node) = node {
            let want = match action {
                ExpandAction::Expand => true,
                ExpandAction::Collapse => false,
                ExpandAction::Toggle => !node.expanded,
            };
            node.expanded = want;
            if want && node.children.is_none() {
                fetch_ref = Some(node.var_ref);
            }
        }
        if let Some(var_ref) = fetch_ref {
            let target = match tree {
                Tree::Scope => FetchTarget::Scope { path },
                Tree::Watch => FetchTarget::Watch {
                    expr: self.watches[path[0]].expr.clone(),
                    subpath: path[1..].to_vec(),
                },
            };
            self.spawn_fetch(target, var_ref);
        }
    }

    fn spawn_fetch(&self, target: FetchTarget, var_ref: i64) {
        let client = self.client.clone();
        let tx = self.update_tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let children = fetch_children(&client, var_ref).await;
            let _ = tx.send(Update {
                epoch,
                kind: UpdateKind::Children { target, children },
            });
        });
    }

    fn spawn_watch(&self, expr: String, frame_id: i64) {
        let client = self.client.clone();
        let tx = self.update_tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let node = evaluate_watch(&client, &expr, frame_id).await.ok();
            let _ = tx.send(Update {
                epoch,
                kind: UpdateKind::Watch { expr, node },
            });
        });
    }

    /// Locate a scope node by positional path (path[0] indexes `roots`).
    fn scope_node_mut(&mut self, path: &[usize]) -> Option<&mut VarNode> {
        let (&first, rest) = path.split_first()?;
        descend(self.roots.get_mut(first)?, rest)
    }

    /// Locate a watch node by positional path (path[0] indexes `watches`).
    fn watch_node_mut(&mut self, path: &[usize]) -> Option<&mut VarNode> {
        let (&wi, rest) = path.split_first()?;
        let WatchState::Resolved(root) = &mut self.watches.get_mut(wi)?.state else {
            return None;
        };
        descend(root, rest)
    }

    /// Locate a node within a watch's subtree by the watch's (stable)
    /// expression — used when an async children reply lands, so a concurrent
    /// watch-list edit can't misdirect it.
    fn watch_node_mut_by_expr(&mut self, expr: &str, subpath: &[usize]) -> Option<&mut VarNode> {
        let w = self.watches.iter_mut().find(|w| w.expr == expr)?;
        let WatchState::Resolved(root) = &mut w.state else {
            return None;
        };
        descend(root, subpath)
    }

    fn clamp_selection(&mut self) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected >= rows.len() {
            self.selected = rows.len() - 1;
        }
        if !rows[self.selected].selectable {
            // Prefer the next selectable row, else fall back to the previous.
            self.selected = match rows[self.selected..].iter().position(|r| r.selectable) {
                Some(off) => self.selected + off,
                None => rows[..self.selected]
                    .iter()
                    .rposition(|r| r.selectable)
                    .unwrap_or(0),
            };
        }
    }

    /// Flatten the watch section and scope tree into the rows currently visible
    /// (collapsed subtrees omitted). Watches come first under a header, then the
    /// scopes; every node row carries its `(tree, path)` address.
    fn visible_rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        if !self.watches.is_empty() {
            out.push(Row::header("watched"));
            for (wi, w) in self.watches.iter().enumerate() {
                match &w.state {
                    WatchState::Resolved(node) => {
                        out.push(Row::node(Tree::Watch, vec![wi], 0, node));
                        if node.expanded
                            && let Some(children) = &node.children
                        {
                            walk_nodes(Tree::Watch, &[wi], children, 1, &mut out);
                        }
                    }
                    WatchState::Pending => out.push(Row::watch_placeholder(wi, &w.expr, "…")),
                    WatchState::Unavailable => {
                        out.push(Row::watch_placeholder(wi, &w.expr, "(unavailable)"))
                    }
                }
            }
        }
        walk_nodes(Tree::Scope, &[], &self.roots, 0, &mut out);
        out
    }

    fn render(&mut self, f: &mut ratatui::Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

        self.render_header(f, chunks[0]);
        self.render_tree(f, chunks[1]);
        self.render_status(f, chunks[2]);
    }

    fn render_header(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let (label, color) = match self.state {
            SessionState::Connecting => ("● connecting…", Color::Yellow),
            SessionState::Running => ("▶ RUNNING — variables not live", Color::Yellow),
            SessionState::Stopped => ("● STOPPED — live", Color::Green),
            SessionState::Ended => ("■ SESSION ENDED", Color::Red),
        };

        let frame_line = match &self.header {
            Some(h) => {
                let loc = match &h.source {
                    Some(src) => format!("{src}:{}", h.line),
                    None => format!("line {}", h.line),
                };
                Line::from(format!(
                    "{}  @ {}   (stop #{}, {})",
                    h.name, loc, h.stop_number, h.reason
                ))
            }
            None => Line::from(Span::styled(
                "no frame",
                Style::default().add_modifier(Modifier::DIM),
            )),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" dap-observer ");
        let body = vec![
            Line::from(Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
            frame_line,
        ];
        f.render_widget(Paragraph::new(body).block(block), area);
    }

    fn render_tree(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let rows = self.visible_rows();
        let live = self.state == SessionState::Stopped;
        let items: Vec<ListItem> = rows.iter().map(|row| row_list_item(row, live)).collect();

        let title = match self.state {
            SessionState::Running => " variables (stale — running) ",
            SessionState::Ended => " variables (session ended) ",
            _ => " variables ",
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("› ");

        self.list_state.select(if rows.is_empty() {
            None
        } else {
            Some(self.selected)
        });
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_status(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let hint = "j/k move · l/h or ⏎ expand · w pin/unpin · g/G top/bottom · q quit";
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::Black).bg(Color::Gray),
            ),
            Span::raw("  "),
            Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
}

/// Descend a node by a path of child indices.
fn descend<'a>(mut node: &'a mut VarNode, path: &[usize]) -> Option<&'a mut VarNode> {
    for &i in path {
        node = node.children.as_mut()?.get_mut(i)?;
    }
    Some(node)
}

/// Flatten a node forest into visible rows, tagging each with its `(tree, path)`.
fn walk_nodes(tree: Tree, prefix: &[usize], nodes: &[VarNode], depth: usize, out: &mut Vec<Row>) {
    for (i, n) in nodes.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(i);
        out.push(Row::node(tree, path.clone(), depth, n));
        if n.expanded
            && let Some(children) = &n.children
        {
            walk_nodes(tree, &path, children, depth + 1, out);
        }
    }
}

fn first_selectable(rows: &[Row]) -> usize {
    rows.iter().position(|r| r.selectable).unwrap_or(0)
}

fn last_selectable(rows: &[Row]) -> usize {
    rows.iter().rposition(|r| r.selectable).unwrap_or(0)
}

impl Row {
    /// A row backed by a real tree node.
    fn node(tree: Tree, path: Vec<usize>, depth: usize, n: &VarNode) -> Row {
        Row {
            tree,
            path,
            depth,
            name: n.name.clone(),
            value: n.value.clone(),
            ty: n.ty.clone(),
            eval_name: n.eval_name.clone(),
            expandable: n.expandable(),
            expanded: n.expanded,
            fetched: n.children.is_some(),
            selectable: true,
            watched: tree == Tree::Watch && depth == 0,
            placeholder: false,
        }
    }

    /// The non-selectable section divider.
    fn header(label: &str) -> Row {
        Row {
            tree: Tree::Scope,
            path: Vec::new(),
            depth: 0,
            name: label.to_string(),
            value: String::new(),
            ty: String::new(),
            eval_name: String::new(),
            expandable: false,
            expanded: false,
            fetched: false,
            selectable: false,
            watched: false,
            placeholder: false,
        }
    }

    /// A watch row that has no value yet (pending) or did not resolve.
    fn watch_placeholder(wi: usize, expr: &str, status: &str) -> Row {
        Row {
            tree: Tree::Watch,
            path: vec![wi],
            depth: 0,
            name: expr.to_string(),
            value: status.to_string(),
            ty: String::new(),
            eval_name: expr.to_string(),
            expandable: false,
            expanded: false,
            fetched: false,
            selectable: true,
            watched: true,
            placeholder: true,
        }
    }
}

/// Render one flattened row as a `ListItem`: the section header, a pending/
/// unavailable watch placeholder, or a normal node (indent, optional pin and
/// expand markers, name, and `: type` / `= value` / pending-fetch `…`).
/// `live` dims node rows when the session isn't stopped (variables are stale).
fn row_list_item(row: &Row, live: bool) -> ListItem<'static> {
    // Non-selectable section header, rendered as a dim divider label.
    if !row.selectable {
        return ListItem::new(Line::from(Span::styled(
            format!("── {} ──", row.name),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )));
    }

    let indent = "  ".repeat(row.depth);

    // Pending/unavailable watch: pin + expression + dim status text.
    if row.placeholder {
        return ListItem::new(Line::from(vec![
            Span::raw(format!("{indent}★ ")),
            Span::styled(
                row.name.clone(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("  {}", row.value),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }

    // Prefix: indent, a pin marker for a watch root, then the expand glyph.
    let mut prefix = indent;
    if row.watched {
        prefix.push_str("★ ");
    }
    prefix.push_str(if row.expandable {
        if row.expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    });

    let mut name_style = Style::default().fg(Color::Cyan);
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

enum ExpandAction {
    Expand,
    Collapse,
    Toggle,
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
