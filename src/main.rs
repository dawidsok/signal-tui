use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use notify_rust::Notification;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

// dim-sum theme palette
const BG: Color = Color::Rgb(0x11, 0x11, 0x10);
const BG3: Color = Color::Rgb(0x31, 0x30, 0x2c);
const DIM: Color = Color::Rgb(0x57, 0x56, 0x51);
const FG: Color = Color::Rgb(0xce, 0xcb, 0xc1);
const CYAN: Color = Color::Rgb(0x5f, 0x9b, 0x95);
const GREEN: Color = Color::Rgb(0x87, 0x96, 0x5f);
const RED: Color = Color::Rgb(0xa8, 0x5f, 0x59);
const BLUE: Color = Color::Rgb(0x6f, 0x8f, 0xaf);

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    List,
    Search,
    Input,
}

struct Contact {
    id: String, // phone number or groupId
    name: String,
    is_group: bool,
}

struct Msg {
    from: String,
    text: String,
}

struct App {
    contacts: Vec<Contact>,
    selected: ListState,
    // ponytail: all messages in one Vec, filter on render; fine until thousands of messages
    messages: Vec<(String, Msg)>, // (contact_id, msg)
    // monotonic counter per contact: higher = more recently messaged
    last_msg_seq: HashMap<String, usize>,
    msg_seq: usize,
    input: String,
    focus: Focus,
    search: String,
    connected: bool,
    // pending 'g' for 'gg' binding
    pending_g: bool,
    // open chat contact id — decoupled from list sort index
    open_id: Option<String>,
    status: String,
}

impl App {
    fn filtered(&self) -> Vec<&Contact> {
        let q = self.search.to_lowercase();
        let mut contacts: Vec<&Contact> = if q.is_empty() {
            self.contacts.iter().collect()
        } else {
            self.contacts
                .iter()
                .filter(|c| c.name.to_lowercase().contains(&q))
                .collect()
        };
        // sort by most recently messaged; contacts never messaged go to bottom
        contacts.sort_by(|a, b| {
            let sa = self.last_msg_seq.get(&a.id).copied().unwrap_or(0);
            let sb = self.last_msg_seq.get(&b.id).copied().unwrap_or(0);
            sb.cmp(&sa)
        });
        contacts
    }
}

fn rpc(stdin: &mut impl Write, id: u64, method: &str, params: Value) {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let _ = writeln!(stdin, "{}", req);
}

/// Returns true if signal-cli has at least one linked/registered account.
fn is_linked() -> bool {
    Command::new("signal-cli")
        .args(["listAccounts"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// Run signal-cli link in foreground, printing the QR code to the terminal.
/// Blocks until the user scans and the link is confirmed (process exits).
fn run_link() -> std::io::Result<()> {
    println!("No Signal account linked. Starting device linking...\n");
    println!("Scan the QR code below with Signal on your phone:");
    println!("  Settings → Linked Devices → Link New Device\n");

    let status = Command::new("signal-cli")
        .args(["link", "--name", "signal-tui"])
        .status()?;

    if status.success() {
        println!("\nLinked successfully. Starting signal-tui...\n");
        Ok(())
    } else {
        Err(std::io::Error::other("linking failed or was cancelled"))
    }
}

fn main() -> std::io::Result<()> {
    if !is_linked() {
        run_link()?;
    }

    let mut child: Child = Command::new("signal-cli")
        .args(["--output=json", "jsonRpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            std::io::Error::other(format!(
                "failed to start signal-cli ({e}); install: brew install signal-cli, then link/register account"
            ))
        })?;

    let mut child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(child_stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
        // thread exits → channel closes → main loop detects disconnect
    });

    rpc(&mut child_stdin, 1, "listContacts", json!({}));
    rpc(&mut child_stdin, 2, "listGroups", json!({}));

    let mut terminal = ratatui::init();
    let mut app = App {
        contacts: vec![],
        selected: ListState::default(),
        messages: vec![],
        last_msg_seq: HashMap::new(),
        msg_seq: 0,
        input: String::new(),
        focus: Focus::List,
        search: String::new(),
        connected: true,
        pending_g: false,
        open_id: None,
        status: "loading contacts...".into(),
    };
    app.selected.select(Some(0));

    let res = run(&mut terminal, &mut app, &rx, &mut child_stdin);
    ratatui::restore();
    let _ = child.kill();
    res
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<Value>,
    child_stdin: &mut impl Write,
) -> std::io::Result<()> {
    let mut next_id: u64 = 100;
    loop {
        loop {
            match rx.try_recv() {
                Ok(v) => handle_json(app, &v),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.connected = false;
                    break;
                }
            }
        }

        terminal.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.focus {
                Focus::Search => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Esc => {
                        app.search.clear();
                        app.focus = Focus::List;
                        clamp_selected(app);
                    }
                    KeyCode::Backspace => {
                        app.search.pop();
                        clamp_selected(app);
                    }
                    KeyCode::Enter => app.focus = Focus::List,
                    KeyCode::Char(ch) => {
                        app.search.push(ch);
                        app.selected.select(Some(0));
                    }
                    _ => {}
                },
                Focus::Input => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Esc => app.focus = Focus::List,
                    KeyCode::Enter if !app.input.is_empty() => {
                        send_message(app, child_stdin, next_id);
                        next_id += 1;
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(ch) => app.input.push(ch),
                    _ => {}
                },
                Focus::List => {
                    app.pending_g = match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(());
                        }
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Char('/') => {
                            app.focus = Focus::Search;
                            app.search.clear();
                            app.selected.select(Some(0));
                            false
                        }
                        KeyCode::Char('i') | KeyCode::Enter => {
                            app.open_id = app
                                .selected
                                .selected()
                                .and_then(|i| app.filtered().get(i).map(|c| c.id.clone()));
                            app.focus = Focus::Input;
                            false
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            let i = app.selected.selected().unwrap_or(0);
                            let max = app.filtered().len().saturating_sub(1);
                            app.selected.select(Some((i + 1).min(max)));
                            false
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            let i = app.selected.selected().unwrap_or(0);
                            app.selected.select(Some(i.saturating_sub(1)));
                            false
                        }
                        KeyCode::Char('g') if app.pending_g => {
                            app.selected.select(Some(0));
                            false
                        }
                        KeyCode::Char('g') => true, // wait for second g
                        KeyCode::Char('G') => {
                            let max = app.filtered().len().saturating_sub(1);
                            app.selected.select(Some(max));
                            false
                        }
                        _ => false,
                    };
                }
            }
        }
    }
}

fn clamp_selected(app: &mut App) {
    let max = app.filtered().len().saturating_sub(1);
    let i = app.selected.selected().unwrap_or(0).min(max);
    app.selected.select(Some(i));
}

fn send_message(app: &mut App, child_stdin: &mut impl Write, next_id: u64) {
    let open_id = match app.open_id.clone() {
        Some(id) => id,
        None => return,
    };
    let is_group = app
        .contacts
        .iter()
        .find(|c| c.id == open_id)
        .map(|c| c.is_group)
        .unwrap_or(false);
    let params = if is_group {
        json!({"groupId": open_id, "message": app.input})
    } else {
        json!({"recipient": [open_id], "message": app.input})
    };
    rpc(child_stdin, next_id, "send", params);
    app.msg_seq += 1;
    app.last_msg_seq.insert(open_id.clone(), app.msg_seq);
    app.messages.push((
        open_id,
        Msg {
            from: "me".into(),
            text: app.input.clone(),
        },
    ));
    app.input.clear();
}

fn handle_json(app: &mut App, v: &Value) {
    // contact/group list responses
    if let Some(result) = v.get("result").and_then(|r| r.as_array()) {
        for item in result {
            if item.get("members").is_some() {
                if let Some(gid) = item.get("id").and_then(|x| x.as_str()) {
                    app.contacts.push(Contact {
                        id: gid.into(),
                        name: item
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or(gid)
                            .into(),
                        is_group: true,
                    });
                }
                continue;
            }
            if let Some(number) = item.get("number").and_then(|x| x.as_str()) {
                let name = item
                    .get("profile")
                    .and_then(|p| p.get("givenName"))
                    .and_then(|x| x.as_str())
                    .or_else(|| item.get("name").and_then(|x| x.as_str()))
                    .filter(|s| !s.is_empty())
                    .unwrap_or(number);
                app.contacts.push(Contact {
                    id: number.into(),
                    name: name.into(),
                    is_group: false,
                });
            }
        }
        app.status = format!("{} contacts", app.contacts.len());
    }
    // incoming message notification
    if v.get("method").and_then(|m| m.as_str()) == Some("receive") {
        let env = &v["params"]["envelope"];
        let from = env["sourceName"]
            .as_str()
            .or(env["sourceNumber"].as_str())
            .unwrap_or("?")
            .to_string();
        let dm = &env["dataMessage"];
        if let Some(text) = dm["message"].as_str() {
            let cid = dm["groupInfo"]["groupId"]
                .as_str()
                .or(env["sourceNumber"].as_str())
                .unwrap_or("?")
                .to_string();
            app.msg_seq += 1;
            let seq = app.msg_seq;
            app.last_msg_seq.insert(cid.clone(), seq);
            app.messages.push((
                cid.clone(),
                Msg {
                    from: from.clone(),
                    text: text.into(),
                },
            ));
            let _ = Notification::new()
                .summary(&format!("Signal: {}", from))
                .body(text)
                .appname("signal-tui")
                .show();
        }
    }
    if let Some(err) = v.get("error") {
        app.status = format!("error: {}", err["message"].as_str().unwrap_or("rpc error"));
    }
}

/// Messages visible in the chat panel for the current open_id.
fn chat_messages(app: &App) -> Vec<&Msg> {
    let id = match app.open_id.as_ref() {
        Some(id) => id,
        None => return vec![],
    };
    app.messages
        .iter()
        .filter(|(cid, _)| cid == id)
        .map(|(_, m)| m)
        .collect()
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(CYAN)
    } else {
        Style::default().fg(BG3)
    }
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    // fill background
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(1)])
        .split(f.area());
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(cols[1]);

    // --- contacts list ---
    // ponytail: collect to break borrow before render_stateful_widget needs &mut app.selected
    let filtered: Vec<(String, String, bool)> = app
        .filtered()
        .into_iter()
        .map(|c| (c.id.clone(), c.name.clone(), c.is_group))
        .collect();

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|(_, name, is_group)| {
            let label = if *is_group {
                format!("# {}", name)
            } else {
                name.clone()
            };
            ListItem::new(label).style(Style::default().fg(FG))
        })
        .collect();

    let list_title = if app.focus == Focus::Search {
        format!("/ {}_", app.search)
    } else {
        "Contacts".into()
    };
    let list_active = matches!(app.focus, Focus::List | Focus::Search);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style(list_active))
                .title(Span::styled(
                    list_title,
                    Style::default().fg(if list_active { CYAN } else { DIM }),
                ))
                .style(Style::default().bg(BG)),
        )
        .highlight_style(Style::default().bg(BG3).fg(FG).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, cols[0], &mut app.selected);

    // --- messages ---
    let chat_id: Option<&str> = app.open_id.as_deref().or_else(|| {
        app.selected
            .selected()
            .and_then(|i| filtered.get(i).map(|(id, _, _)| id.as_str()))
    });
    let chat_title = chat_id
        .and_then(|id| filtered.iter().find(|(fid, _, _)| fid == id).map(|(_, n, _)| n.clone()))
        .unwrap_or_default();
    let msgs_buf: Vec<&Msg> = chat_messages(app);
    let lines: Vec<Line> = msgs_buf
        .iter()
        .map(|m| {
            let color = if m.from == "me" { BLUE } else { GREEN };
            Line::from(vec![
                Span::styled(
                    format!("{}: ", m.from),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(m.text.clone(), Style::default().fg(FG)),
            ])
        })
        .collect();
    let msgs = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::MAX, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style(false))
                .title(Span::styled(chat_title, Style::default().fg(DIM)))
                .style(Style::default().bg(BG)),
        );
    f.render_widget(msgs, right[0]);

    // --- input ---
    let input_active = app.focus == Focus::Input;
    let input = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(FG))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style(input_active))
                .title(Span::styled(
                    "Message",
                    Style::default().fg(if input_active { CYAN } else { DIM }),
                ))
                .style(Style::default().bg(BG)),
        );
    f.render_widget(input, right[1]);

    // --- statusline ---
    let (conn_sym, conn_color) = if app.connected {
        ("● connected", GREEN)
    } else {
        ("✗ disconnected", RED)
    };
    let mode_hint = match app.focus {
        Focus::List => "j/k navigate · / search · i/Enter compose · q quit",
        Focus::Search => "type to filter · Enter confirm · Esc cancel",
        Focus::Input => "Enter send · Esc back",
    };
    let status_line = Line::from(vec![
        Span::styled(format!(" {} ", conn_sym), Style::default().fg(conn_color)),
        Span::styled("│ ", Style::default().fg(BG3)),
        Span::styled(format!("{} ", app.status), Style::default().fg(DIM)),
        Span::styled("│ ", Style::default().fg(BG3)),
        Span::styled(mode_hint, Style::default().fg(DIM)),
    ]);
    f.render_widget(
        Paragraph::new(status_line).style(Style::default().bg(BG)),
        right[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> App {
        let mut app = App {
            contacts: vec![],
            selected: ListState::default(),
            messages: vec![],
            last_msg_seq: HashMap::new(),
            msg_seq: 0,
            input: String::new(),
            focus: Focus::List,
            search: String::new(),
            connected: true,
            pending_g: false,
            open_id: None,
            status: String::new(),
        };
        app.selected.select(Some(0));
        app
    }

    fn add_contact(app: &mut App, id: &str, name: &str) {
        app.contacts.push(Contact { id: id.into(), name: name.into(), is_group: false });
    }

    fn add_group(app: &mut App, id: &str, name: &str) {
        app.contacts.push(Contact { id: id.into(), name: name.into(), is_group: true });
    }

    /// Simulate pressing i/Enter on the currently highlighted contact.
    fn open_selected(app: &mut App) {
        app.open_id = app
            .selected
            .selected()
            .and_then(|i| app.filtered().get(i).map(|c| c.id.clone()));
        app.focus = Focus::Input;
    }

    // ── send_message ──────────────────────────────────────────────────────────

    #[test]
    fn send_message_pushes_to_messages() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn send_message_uses_open_id_as_key() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        assert_eq!(app.messages[0].0, "+1");
    }

    #[test]
    fn send_message_clears_input() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        assert!(app.input.is_empty());
    }

    #[test]
    fn send_message_noop_without_open_id() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        // open_id stays None — never opened a chat
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        assert!(app.messages.is_empty(), "send must not fire without open_id");
    }

    #[test]
    fn send_message_writes_rpc_to_sink() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        let mut sink = vec![];
        send_message(&mut app, &mut sink, 42);
        let out = String::from_utf8(sink).unwrap();
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["method"], "send");
        assert_eq!(v["params"]["recipient"][0], "+1");
        assert_eq!(v["params"]["message"], "hello");
    }

    #[test]
    fn send_message_group_uses_group_rpc() {
        let mut app = make_app();
        add_group(&mut app, "grp1", "Team");
        open_selected(&mut app);
        app.input = "hi".into();
        let mut sink = vec![];
        send_message(&mut app, &mut sink, 1);
        let v: Value = serde_json::from_str(String::from_utf8(sink).unwrap().trim()).unwrap();
        assert_eq!(v["params"]["groupId"], "grp1");
        assert!(v["params"]["recipient"].is_null(), "group send must not include recipient");
    }

    // ── chat_messages (visibility) ────────────────────────────────────────────

    #[test]
    fn chat_messages_empty_without_open_id() {
        let app = make_app();
        assert!(chat_messages(&app).is_empty());
    }

    #[test]
    fn chat_messages_shows_sent_message() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        let visible = chat_messages(&app);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].text, "hello");
        assert_eq!(visible[0].from, "me");
    }

    #[test]
    fn chat_messages_shows_only_current_contact() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        add_contact(&mut app, "+2", "Bob");

        // send to Alice (index 0)
        open_selected(&mut app);
        app.input = "to alice".into();
        send_message(&mut app, &mut vec![], 1);

        // switch to Bob (index 1) and send
        app.selected.select(Some(1));
        open_selected(&mut app);
        app.input = "to bob".into();
        send_message(&mut app, &mut vec![], 2);

        let visible = chat_messages(&app);
        assert_eq!(visible.len(), 1, "only bob's message should show");
        assert_eq!(visible[0].text, "to bob");
    }

    #[test]
    fn chat_messages_stable_after_sort_reorder() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        add_contact(&mut app, "+2", "Bob");

        // open Alice (index 0), send
        open_selected(&mut app);
        app.input = "hi alice".into();
        send_message(&mut app, &mut vec![], 1);
        // Alice is now seq=1, Bob seq=0 → Alice at top (index 0) still

        // simulate incoming from Bob → Bob bumps to seq=2, moves to index 0
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+2","sourceName":"Bob","dataMessage":{"message":"hey"}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        // now filtered = [Bob(2), Alice(1)] — Bob at index 0, Alice at 1

        // open_id is still "+1" (Alice); chat_messages must still show Alice's message
        assert_eq!(app.open_id.as_deref(), Some("+1"));
        let visible = chat_messages(&app);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].text, "hi alice");
    }

    // ── open_id selection ────────────────────────────────────────────────────

    #[test]
    fn open_selected_sets_open_id() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        assert_eq!(app.open_id.as_deref(), Some("+1"));
    }

    #[test]
    fn open_selected_none_when_no_contacts() {
        let mut app = make_app();
        open_selected(&mut app);
        assert!(app.open_id.is_none());
    }

    #[test]
    fn open_selected_picks_correct_index() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        add_contact(&mut app, "+2", "Bob");
        app.selected.select(Some(1));
        open_selected(&mut app);
        assert_eq!(app.open_id.as_deref(), Some("+2"));
    }

    // ── incoming message parsing ──────────────────────────────────────────────

    #[test]
    fn parses_incoming_dm() {
        let mut app = make_app();
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+48123","sourceName":"Bob","dataMessage":{"message":"hi"}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].0, "+48123");
        assert_eq!(app.messages[0].1.text, "hi");
        assert_eq!(app.messages[0].1.from, "Bob");
    }

    #[test]
    fn parses_incoming_group_message() {
        let mut app = make_app();
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+1","sourceName":"Alice","dataMessage":{"message":"hey","groupInfo":{"groupId":"grp1"}}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        assert_eq!(app.messages[0].0, "grp1");
    }

    #[test]
    fn incoming_message_bumps_seq() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+1","sourceName":"Alice","dataMessage":{"message":"hi"}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        assert_eq!(*app.last_msg_seq.get("+1").unwrap(), 1);
    }

    // ── disconnection ─────────────────────────────────────────────────────────

    #[test]
    fn disconnected_flag() {
        let mut app = make_app();
        app.connected = false;
        assert!(!app.connected);
    }
}
