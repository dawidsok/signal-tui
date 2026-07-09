use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use notify_rust::Notification;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

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
    focus_input: bool,
    search: String,
    searching: bool,
    status: String,
}

impl App {
    fn filtered(&self) -> Vec<&Contact> {
        let q = self.search.to_lowercase();
        let mut contacts: Vec<&Contact> = if q.is_empty() {
            self.contacts.iter().collect()
        } else {
            self.contacts.iter().filter(|c| c.name.to_lowercase().contains(&q)).collect()
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
        .map(|o| {
            // listAccounts prints one account per line; empty → not linked
            !String::from_utf8_lossy(&o.stdout).trim().is_empty()
        })
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
        focus_input: true,
        search: String::new(),
        searching: false,
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
        while let Ok(v) = rx.try_recv() {
            handle_json(app, &v);
        }

        terminal.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(())
                }
                KeyCode::Esc if app.searching => {
                    app.search.clear();
                    app.searching = false;
                    let max = app.filtered().len().saturating_sub(1);
                    let i = app.selected.selected().unwrap_or(0).min(max);
                    app.selected.select(Some(i));
                }
                KeyCode::Backspace if app.searching => {
                    app.search.pop();
                    let max = app.filtered().len().saturating_sub(1);
                    let i = app.selected.selected().unwrap_or(0).min(max);
                    app.selected.select(Some(i));
                }
                KeyCode::Char(ch) if app.searching => {
                    app.search.push(ch);
                    app.selected.select(Some(0));
                }
                KeyCode::Char('/') if !app.focus_input && !app.searching => {
                    app.searching = true;
                    app.search.clear();
                    app.selected.select(Some(0));
                }
                KeyCode::Tab => {
                    app.focus_input = !app.focus_input;
                    app.searching = false;
                }
                KeyCode::Up if !app.focus_input => {
                    let i = app.selected.selected().unwrap_or(0);
                    app.selected.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Down if !app.focus_input => {
                    let i = app.selected.selected().unwrap_or(0);
                    let max = app.filtered().len().saturating_sub(1);
                    app.selected.select(Some((i + 1).min(max)));
                }
                KeyCode::Enter if app.focus_input && !app.input.is_empty() => {
                    let contact_id = app
                        .selected
                        .selected()
                        .and_then(|i| app.filtered().get(i).map(|c| (c.id.clone(), c.is_group)));
                    if let Some((id, is_group)) = contact_id {
                        let params = if is_group {
                            json!({"groupId": id, "message": app.input})
                        } else {
                            json!({"recipient": [id], "message": app.input})
                        };
                        rpc(child_stdin, next_id, "send", params);
                        next_id += 1;
                        app.msg_seq += 1;
                        app.last_msg_seq.insert(id.clone(), app.msg_seq);
                        app.messages.push((
                            id,
                            Msg { from: "me".into(), text: app.input.clone() },
                        ));
                        app.input.clear();
                    }
                }
                KeyCode::Backspace if app.focus_input => {
                    app.input.pop();
                }
                KeyCode::Char('q') if !app.focus_input => return Ok(()),
                KeyCode::Char(ch) if app.focus_input => app.input.push(ch),
                _ => {}
            }
        }
    }
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
            app.messages.push((cid, Msg { from: from.clone(), text: text.into() }));
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

fn draw(f: &mut ratatui::Frame, app: &mut App) {
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

    // ponytail: collect to break borrow before render_stateful_widget needs &mut app.selected
    let filtered: Vec<(String, String, bool)> = app
        .filtered()
        .into_iter()
        .map(|c| (c.id.clone(), c.name.clone(), c.is_group))
        .collect();
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|(_, name, is_group)| {
            ListItem::new(if *is_group {
                format!("# {}", name)
            } else {
                name.clone()
            })
        })
        .collect();
    let list_title = if app.searching {
        format!("/ {}_", app.search)
    } else if app.focus_input {
        "Contacts (Tab)".into()
    } else {
        "Contacts * (/ search)".into()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(list_title))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, cols[0], &mut app.selected);

    let current = app.selected.selected().and_then(|i| filtered.get(i).cloned());
    let lines: Vec<Line> = app
        .messages
        .iter()
        .filter(|(cid, _)| current.as_ref().map(|(id, _, _)| id == cid).unwrap_or(false))
        .map(|(_, m)| Line::from(format!("{}: {}", m.from, m.text)))
        .collect();
    let title = current.as_ref().map(|(_, name, _)| name.clone()).unwrap_or_default();
    let msgs = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::MAX, 0))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(msgs, right[0]);

    let input = Paragraph::new(app.input.as_str()).block(
        Block::default().borders(Borders::ALL).title(if app.focus_input {
            "Message *"
        } else {
            "Message (Tab)"
        }),
    );
    f.render_widget(input, right[1]);

    f.render_widget(
        Paragraph::new(format!(
            " {} | Tab: switch focus | Enter: send | q (list focus)/Ctrl-C: quit",
            app.status
        ))
        .style(Style::default().fg(Color::DarkGray)),
        right[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_incoming_message() {
        let mut app = App {
            contacts: vec![],
            selected: ListState::default(),
            messages: vec![],
            last_msg_seq: HashMap::new(),
            msg_seq: 0,
            input: String::new(),
            focus_input: true,
            search: String::new(),
            searching: false,
            status: String::new(),
        };
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+48123","sourceName":"Bob","dataMessage":{"message":"hi"}}}}"#,
        )
        .unwrap();
        handle_json(&mut app, &v);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].0, "+48123");
        assert_eq!(app.messages[0].1.text, "hi");
    }
}
