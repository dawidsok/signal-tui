use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use notify_rust::Notification;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// dim-sum theme palette
const BG: Color = Color::Rgb(0x11, 0x11, 0x10);
const BG3: Color = Color::Rgb(0x31, 0x30, 0x2c);
const DIM: Color = Color::Rgb(0x57, 0x56, 0x51);
const FG: Color = Color::Rgb(0xce, 0xcb, 0xc1);
const CYAN: Color = Color::Rgb(0x5f, 0x9b, 0x95);
const GREEN: Color = Color::Rgb(0x87, 0x96, 0x5f);
const RED: Color = Color::Rgb(0xa8, 0x5f, 0x59);
const BLUE: Color = Color::Rgb(0x6f, 0x8f, 0xaf);
const MAX_EVENTS_PER_TICK: usize = 25;
const COMMANDS: &[(&str, &str)] = &[
    ("daemon", "run the background receiver"),
    ("status", "print background receiver status"),
    ("help", "print this help"),
    ("completions", "print shell completions: zsh, bash, fish"),
];

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
    id: String,
    from: String,
    text: String,
}

struct PendingSend {
    pending_id: String,
    contact_id: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
struct HistoryRecord {
    id: String,
    contact_id: String,
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
    unread: HashSet<String>,
    favorites: HashSet<String>,
    seen_msg_ids: HashSet<String>,
    pending_sends: HashMap<u64, PendingSend>,
    history_path: Option<PathBuf>,
    msg_seq: usize,
    input: String,
    focus: Focus,
    search: String,
    connected: bool,
    // pending 'g' for 'gg' binding
    pending_g: bool,
    // open chat contact id — decoupled from list sort index
    open_id: Option<String>,
    self_id: Option<String>,
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
                .filter(|c| c.name.to_lowercase().contains(&q) || c.id.to_lowercase().contains(&q))
                .collect()
        };
        contacts.sort_by(|a, b| {
            let fa = self.favorites.contains(&a.id);
            let fb = self.favorites.contains(&b.id);
            let sa = self.last_msg_seq.get(&a.id).copied().unwrap_or(0);
            let sb = self.last_msg_seq.get(&b.id).copied().unwrap_or(0);
            fb.cmp(&fa).then_with(|| sb.cmp(&sa))
        });
        contacts
    }
}

fn rpc(stdin: &mut (impl Write + ?Sized), id: u64, method: &str, params: Value) {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let _ = writeln!(stdin, "{}", req);
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("SIGNAL_TUI_CONFIG")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/signal-tui")))
}

fn favorites_path() -> Option<PathBuf> {
    Some(config_dir()?.join("favorites"))
}

fn data_dir() -> Option<PathBuf> {
    std::env::var_os("SIGNAL_TUI_DATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|h| PathBuf::from(h).join("signal-tui")))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/signal-tui"))
        })
}

fn history_path() -> Option<PathBuf> {
    Some(data_dir()?.join("messages.jsonl"))
}

fn unread_path() -> Option<PathBuf> {
    Some(data_dir()?.join("unread"))
}

fn status_path() -> Option<PathBuf> {
    Some(data_dir()?.join("status.json"))
}

fn receiver_lock_path() -> Option<PathBuf> {
    Some(data_dir()?.join("receiver.lock"))
}

fn read_favorites(path: &Path) -> std::io::Result<HashSet<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e),
    }
}

fn write_favorites(path: &Path, favorites: &HashSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut lines: Vec<&str> = favorites.iter().map(String::as_str).collect();
    lines.sort_unstable();
    fs::write(path, lines.join("\n"))
}

fn load_favorites() -> HashSet<String> {
    favorites_path()
        .and_then(|p| read_favorites(&p).ok())
        .unwrap_or_default()
}

fn read_set(path: &Path) -> std::io::Result<HashSet<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e),
    }
}

fn write_set(path: &Path, set: &HashSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut lines: Vec<&str> = set.iter().map(String::as_str).collect();
    lines.sort_unstable();
    fs::write(path, lines.join("\n"))
}

fn load_unread() -> HashSet<String> {
    unread_path()
        .and_then(|p| read_set(&p).ok())
        .unwrap_or_default()
}

fn save_unread(app: &mut App) {
    if app.history_path.is_none() {
        return;
    }
    if let Some(path) = unread_path()
        && let Err(e) = write_set(&path, &app.unread)
    {
        app.status = format!("unread not saved: {e}");
    }
}

fn save_favorites(app: &mut App) {
    if let Some(path) = favorites_path()
        && let Err(e) = write_favorites(&path, &app.favorites)
    {
        app.status = format!("favorites not saved: {e}");
    }
}

fn read_history(path: &Path) -> std::io::Result<Vec<HistoryRecord>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s
            .lines()
            .filter_map(|line| serde_json::from_str::<HistoryRecord>(line).ok())
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e),
    }
}

fn append_history(path: &Path, record: &HistoryRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(record).map_err(std::io::Error::other)?
    )
}

fn msg_id(contact_id: &str, from: &str, ts: i64) -> String {
    format!("{contact_id}:{from}:{ts}")
}

fn json_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|n| n.try_into().ok()))
}

#[derive(Deserialize, Serialize)]
struct DaemonStatus {
    pid: u32,
    role: String,
    state: String,
    updated_at_ms: u128,
    last_error: Option<String>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn write_status(state: &str, error: Option<String>) {
    let Some(path) = status_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let status = DaemonStatus {
        pid: std::process::id(),
        role: "receiver".into(),
        state: state.into(),
        updated_at_ms: now_ms(),
        last_error: error,
    };
    if let Ok(s) = serde_json::to_string_pretty(&status) {
        let _ = fs::write(path, s);
    }
}

fn pid_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct ReceiverLock {
    path: PathBuf,
}

impl Drop for ReceiverLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_receiver_lock() -> std::io::Result<Option<ReceiverLock>> {
    let Some(path) = receiver_lock_path() else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                return Ok(Some(ReceiverLock { path }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let live = fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .is_some_and(pid_is_alive);
                if live {
                    return Ok(None);
                }
                let _ = fs::remove_file(&path);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn parse_linked_account(stdout: &str) -> Option<String> {
    serde_json::from_str::<Value>(stdout)
        .ok()?
        .as_array()?
        .iter()
        .find_map(|a| a.get("number").and_then(|n| n.as_str()).map(str::to_string))
}

/// Returns the first linked/registered signal-cli account, if any.
fn linked_account() -> Option<String> {
    Command::new("signal-cli")
        .args(["--output=json", "listAccounts"])
        .output()
        .ok()
        .and_then(|o| parse_linked_account(&String::from_utf8_lossy(&o.stdout)))
}

/// Run signal-cli link in foreground, printing the QR code to the terminal.
/// Blocks until the user scans and the link is confirmed (process exits).
fn help_text() -> String {
    let mut s = "signal-tui [COMMAND]\n\nCommands:\n".to_string();
    for (cmd, desc) in COMMANDS {
        s.push_str(&format!("  {cmd:<12} {desc}\n"));
    }
    s.push_str("\nWith no command, opens the TUI.\n");
    s
}

fn print_help() {
    print!("{}", help_text());
}

fn completion_script(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(
            r#"#compdef signal-tui
_signal_tui() {
  local -a commands shells
  commands=(
    'daemon:run the background receiver'
    'status:print background receiver status'
    'help:print help'
    'completions:print shell completions'
  )
  shells=('zsh:Zsh completions' 'bash:Bash completions' 'fish:Fish completions')
  if (( CURRENT == 2 )); then
    _describe 'command' commands
  elif [[ ${words[2]} == completions ]]; then
    _describe 'shell' shells
  fi
}
compdef _signal_tui signal-tui
"#,
        ),
        "bash" => Some(
            r#"_signal_tui() {
  local cur prev
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  if [[ "$prev" == "completions" ]]; then
    COMPREPLY=( $(compgen -W "zsh bash fish" -- "$cur") )
  else
    COMPREPLY=( $(compgen -W "daemon status help completions" -- "$cur") )
  fi
}
complete -F _signal_tui signal-tui
"#,
        ),
        "fish" => Some(
            r#"complete -c signal-tui -f -a "daemon" -d "run the background receiver"
complete -c signal-tui -f -a "status" -d "print background receiver status"
complete -c signal-tui -f -a "help" -d "print help"
complete -c signal-tui -f -a "completions" -d "print shell completions"
complete -c signal-tui -n "__fish_seen_subcommand_from completions" -f -a "zsh bash fish"
"#,
        ),
        _ => None,
    }
}

fn print_completions(shell: &str) -> std::io::Result<()> {
    match completion_script(shell) {
        Some(script) => {
            print!("{script}");
            Ok(())
        }
        None => Err(std::io::Error::other(
            "usage: signal-tui completions [zsh|bash|fish]",
        )),
    }
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

fn ensure_account() -> std::io::Result<Option<String>> {
    let mut account = linked_account();
    if account.is_none() {
        run_link()?;
        account = linked_account();
    }
    Ok(account)
}

fn new_app(account: Option<String>, status: String) -> App {
    let mut app = App {
        contacts: vec![],
        selected: ListState::default(),
        messages: vec![],
        last_msg_seq: HashMap::new(),
        unread: load_unread(),
        favorites: load_favorites(),
        seen_msg_ids: HashSet::new(),
        pending_sends: HashMap::new(),
        history_path: history_path(),
        msg_seq: 0,
        input: String::new(),
        focus: Focus::List,
        search: String::new(),
        connected: true,
        pending_g: false,
        open_id: None,
        self_id: account.clone(),
        status,
    };
    load_history(&mut app);
    for id in app
        .messages
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>()
    {
        add_contact_once(&mut app, id.clone(), id, false);
    }
    if let Some(account) = account {
        add_contact_once(&mut app, account, "Note to Self".into(), false);
    }
    app.selected.select(Some(0));
    app
}

fn spawn_signal_jsonrpc(account: Option<&str>) -> std::io::Result<Child> {
    let mut command = Command::new("signal-cli");
    command.arg("--output=json");
    if let Some(account) = account {
        command.args(["-a", account]);
    }
    command
        .arg("jsonRpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            std::io::Error::other(format!(
                "failed to start signal-cli ({e}); install: brew install signal-cli, then link/register account"
            ))
        })
}

fn spawn_reader(child_stdout: impl std::io::Read + Send + 'static) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(child_stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    rx
}

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("daemon") => run_daemon(),
        Some("status") => print_status(),
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("completions") | Some("completion") => {
            let shell = std::env::args().nth(2).unwrap_or_else(|| "zsh".into());
            print_completions(&shell)
        }
        Some(other) => Err(std::io::Error::other(format!(
            "unknown command: {other}\n\n{}",
            help_text()
        ))),
        None => run_tui(),
    }
}

fn run_tui() -> std::io::Result<()> {
    let account = ensure_account()?;
    let receiver_lock = acquire_receiver_lock()?;
    let mut child = None;
    let mut rx = None;
    let mut child_stdin = None;
    let status = if receiver_lock.is_some() {
        let mut c = spawn_signal_jsonrpc(account.as_deref())?;
        child_stdin = c.stdin.take();
        rx = c.stdout.take().map(spawn_reader);
        child = Some(c);
        "loading contacts...".into()
    } else {
        "background receiver active".into()
    };

    if let Some(stdin) = child_stdin.as_mut() {
        rpc(stdin, 1, "listContacts", json!({}));
        rpc(stdin, 2, "listGroups", json!({}));
    }

    let mut terminal = ratatui::init();
    let mut app = new_app(account, status);
    let res = run(
        &mut terminal,
        &mut app,
        rx.as_ref(),
        child_stdin.as_mut().map(|s| s as &mut dyn Write),
    );
    ratatui::restore();
    drop(receiver_lock);
    if let Some(mut child) = child {
        let _ = child.kill();
    }
    res
}

fn print_status() -> std::io::Result<()> {
    if let Some(path) = status_path()
        && let Ok(s) = fs::read_to_string(path)
    {
        println!("{s}");
        return Ok(());
    }
    println!("no background receiver status");
    Ok(())
}

fn run_daemon() -> std::io::Result<()> {
    let account = ensure_account()?;
    let Some(_lock) = acquire_receiver_lock()? else {
        println!("receiver already running");
        return Ok(());
    };
    write_status("starting", None);
    let mut app = new_app(account.clone(), "daemon".into());
    let mut child = spawn_signal_jsonrpc(account.as_deref())?;
    let stdout = child.stdout.take().unwrap();
    write_status("connected", None);
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    handle_json(&mut app, &v);
                    write_status("connected", None);
                }
                Err(e) => write_status("parse-error", Some(e.to_string())),
            },
            Err(e) => {
                write_status("disconnected", Some(e.to_string()));
                break;
            }
        }
    }
    let _ = child.kill();
    Ok(())
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: Option<&mpsc::Receiver<Value>>,
    mut child_stdin: Option<&mut dyn Write>,
) -> std::io::Result<()> {
    let mut next_id: u64 = 100;
    let mut last_reload = Instant::now();
    loop {
        if let Some(rx) = rx {
            drain_incoming(app, rx, MAX_EVENTS_PER_TICK);
        } else if last_reload.elapsed() >= Duration::from_secs(1) {
            reload_local_state(app);
            last_reload = Instant::now();
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
                    KeyCode::Enter => confirm_search(app),
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
                        let sent = if let Some(stdin) = child_stdin.as_deref_mut() {
                            submit_input(app, stdin, next_id)
                        } else {
                            submit_input_local(app)
                        };
                        if sent {
                            next_id += 1;
                        }
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
                            open_selected_contact(app);
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
                        KeyCode::Char('f') => {
                            toggle_favorite(app);
                            false
                        }
                        _ => false,
                    };
                }
            }
        }
    }
}

fn drain_incoming(app: &mut App, rx: &mpsc::Receiver<Value>, max: usize) {
    for _ in 0..max {
        match rx.try_recv() {
            Ok(v) => handle_json(app, &v),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                app.connected = false;
                break;
            }
        }
    }
}

fn clamp_selected(app: &mut App) {
    let max = app.filtered().len().saturating_sub(1);
    let i = app.selected.selected().unwrap_or(0).min(max);
    app.selected.select(Some(i));
}

fn add_contact_once(app: &mut App, id: String, name: String, is_group: bool) {
    if !app.contacts.iter().any(|c| c.id == id) {
        app.contacts.push(Contact { id, name, is_group });
    }
}

fn current_chat_id(app: &App) -> Option<String> {
    app.open_id.clone().or_else(|| {
        app.selected
            .selected()
            .and_then(|i| app.filtered().get(i).map(|c| c.id.clone()))
    })
}

fn sync_unread_from_disk(app: &mut App) {
    if app.history_path.is_some() {
        app.unread = load_unread();
    }
}

fn open_chat(app: &mut App, id: String) {
    sync_unread_from_disk(app);
    app.unread.remove(&id);
    save_unread(app);
    app.open_id = Some(id);
    app.focus = Focus::Input;
}

fn set_favorite(app: &mut App, id: String, favorite: bool) {
    if favorite {
        app.favorites.insert(id.clone());
        app.status = format!("favorited: {id}");
    } else {
        app.favorites.remove(&id);
        app.status = format!("unfavorited: {id}");
    }
    save_favorites(app);
}

fn toggle_favorite(app: &mut App) {
    if let Some(id) = current_chat_id(app) {
        set_favorite(app, id.clone(), !app.favorites.contains(&id));
    }
}

fn open_selected_contact(app: &mut App) {
    if let Some(id) = app
        .selected
        .selected()
        .and_then(|i| app.filtered().get(i).map(|c| c.id.clone()))
    {
        open_chat(app, id);
    } else {
        app.focus = Focus::Input;
    }
}

fn confirm_search(app: &mut App) {
    let q = app.search.trim().to_string();
    if app.filtered().is_empty() && !q.is_empty() {
        app.search = q.clone();
        add_contact_once(app, q.clone(), q.clone(), false);
        open_chat(app, q.clone());
        app.status = format!("new chat: {q}");
        app.selected.select(Some(0));
    } else {
        app.focus = Focus::List;
        clamp_selected(app);
    }
}

fn push_message(app: &mut App, cid: String, msg: Msg) {
    app.msg_seq += 1;
    app.last_msg_seq.insert(cid.clone(), app.msg_seq);
    app.messages.push((cid, msg));
}

fn persist_history(app: &mut App, record: &HistoryRecord) {
    if let Some(path) = &app.history_path
        && let Err(e) = append_history(path, record)
    {
        app.status = format!("history not saved: {e}");
    }
}

fn record_message(
    app: &mut App,
    cid: String,
    from: String,
    text: &str,
    id: String,
    persist: bool,
) -> bool {
    if !app.seen_msg_ids.insert(id.clone()) {
        return false;
    }
    push_message(
        app,
        cid.clone(),
        Msg {
            id: id.clone(),
            from: from.clone(),
            text: text.into(),
        },
    );
    if persist {
        persist_history(
            app,
            &HistoryRecord {
                id,
                contact_id: cid,
                from,
                text: text.into(),
            },
        );
    }
    true
}

fn record_ephemeral_message(app: &mut App, cid: String, from: String, text: &str, id: String) {
    push_message(
        app,
        cid,
        Msg {
            id,
            from,
            text: text.into(),
        },
    );
}

fn load_history(app: &mut App) {
    if let Some(path) = &app.history_path.clone()
        && let Ok(records) = read_history(path)
    {
        for r in records {
            record_message(app, r.contact_id, r.from, &r.text, r.id, false);
        }
    }
}

fn reload_local_state(app: &mut App) {
    load_history(app);
    app.unread = load_unread();
}

fn drop_matching_pending(app: &mut App, cid: &str, text: &str) {
    if let Some((req_id, pending_id)) = app
        .pending_sends
        .iter()
        .find(|(_, p)| p.contact_id == cid && p.text == text)
        .map(|(id, p)| (*id, p.pending_id.clone()))
    {
        app.pending_sends.remove(&req_id);
        app.messages.retain(|(_, m)| m.id != pending_id);
    }
}

fn confirm_send(app: &mut App, req_id: u64, ts: i64) {
    let Some(pending) = app.pending_sends.remove(&req_id) else {
        return;
    };
    let id = msg_id(&pending.contact_id, "me", ts);
    if app.seen_msg_ids.contains(&id) {
        app.messages.retain(|(_, m)| m.id != pending.pending_id);
        return;
    }
    app.seen_msg_ids.insert(id.clone());
    if let Some((_, msg)) = app
        .messages
        .iter_mut()
        .find(|(_, m)| m.id == pending.pending_id)
    {
        msg.id = id.clone();
    } else {
        push_message(
            app,
            pending.contact_id.clone(),
            Msg {
                id: id.clone(),
                from: "me".into(),
                text: pending.text.clone(),
            },
        );
    }
    persist_history(
        app,
        &HistoryRecord {
            id,
            contact_id: pending.contact_id,
            from: "me".into(),
            text: pending.text,
        },
    );
}

fn handle_command(app: &mut App, command: &str) {
    let mut parts = command.split_whitespace();
    match parts.next().unwrap_or("") {
        "/help" => app.status = "commands: /chat <number>, /self, /fav, /unfav".into(),
        "/chat" | "/new" => match parts.next() {
            Some(id) => {
                add_contact_once(app, id.into(), id.into(), false);
                open_chat(app, id.into());
                app.search.clear();
                app.status = format!("chat: {id}");
            }
            None => app.status = "usage: /chat <number>".into(),
        },
        "/self" | "/me" => match app.self_id.clone() {
            Some(id) => {
                add_contact_once(app, id.clone(), "Note to Self".into(), false);
                open_chat(app, id);
                app.search.clear();
                app.status = "chat: Note to Self".into();
            }
            None => app.status = "no linked account for /self".into(),
        },
        "/fav" | "/favorite" => match current_chat_id(app) {
            Some(id) => set_favorite(app, id, true),
            None => app.status = "no chat to favorite".into(),
        },
        "/unfav" | "/unfavorite" => match current_chat_id(app) {
            Some(id) => set_favorite(app, id, false),
            None => app.status = "no chat to unfavorite".into(),
        },
        other => app.status = format!("unknown command: {other}"),
    }
}

fn submit_input(app: &mut App, child_stdin: &mut (impl Write + ?Sized), next_id: u64) -> bool {
    if app.input.trim_start().starts_with('/') {
        let command = std::mem::take(&mut app.input);
        handle_command(app, command.trim());
        false
    } else {
        send_message(app, child_stdin, next_id);
        true
    }
}

fn submit_input_local(app: &mut App) -> bool {
    if app.input.trim_start().starts_with('/') {
        let command = std::mem::take(&mut app.input);
        handle_command(app, command.trim());
        false
    } else {
        send_message_oneshot(app);
        true
    }
}

fn open_id_or_selected(app: &mut App) -> Option<String> {
    if app.open_id.is_none() {
        app.open_id = app
            .selected
            .selected()
            .and_then(|i| app.filtered().get(i).map(|c| c.id.clone()));
    }
    app.open_id.clone()
}

fn send_message_oneshot(app: &mut App) {
    let Some(open_id) = open_id_or_selected(app) else {
        return;
    };
    let text = std::mem::take(&mut app.input);
    let is_group = app
        .contacts
        .iter()
        .find(|c| c.id == open_id)
        .map(|c| c.is_group)
        .unwrap_or(false);
    let mut command = Command::new("signal-cli");
    command.arg("--output=json");
    if let Some(account) = &app.self_id {
        command.args(["-a", account]);
    }
    command.arg("send");
    if app.self_id.as_deref() == Some(open_id.as_str()) {
        command.arg("--note-to-self");
    } else if is_group {
        command.args(["--group-id", &open_id]);
    } else {
        command.arg(&open_id);
    }
    command.args(["--message", &text]);
    match command.output() {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let ts = serde_json::from_str::<Value>(&stdout)
                .ok()
                .and_then(|v| find_timestamp(&v));
            if let Some(ts) = ts {
                let id = msg_id(&open_id, "me", ts);
                record_message(app, open_id, "me".into(), &text, id, true);
            } else {
                let id = format!("local:{}", app.msg_seq + 1);
                record_ephemeral_message(app, open_id, "me".into(), &text, id);
            }
        }
        Ok(o) => {
            app.input = text;
            app.status = format!("send failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        Err(e) => {
            app.input = text;
            app.status = format!("send failed: {e}");
        }
    }
}

fn find_timestamp(v: &Value) -> Option<i64> {
    json_i64(&v["timestamp"])
        .or_else(|| json_i64(&v["result"]["timestamp"]))
        .or_else(|| v.as_array()?.iter().find_map(find_timestamp))
}

fn send_message(app: &mut App, child_stdin: &mut (impl Write + ?Sized), next_id: u64) {
    let open_id = match open_id_or_selected(app) {
        Some(id) => id,
        None => return,
    };
    app.unread.remove(&open_id);
    let is_group = app
        .contacts
        .iter()
        .find(|c| c.id == open_id)
        .map(|c| c.is_group)
        .unwrap_or(false);
    let params = if app.self_id.as_deref() == Some(open_id.as_str()) {
        json!({"noteToSelf": true, "message": app.input})
    } else if is_group {
        json!({"groupId": open_id, "message": app.input})
    } else {
        json!({"recipient": [open_id], "message": app.input})
    };
    rpc(child_stdin, next_id, "send", params);
    let text = std::mem::take(&mut app.input);
    let pending_id = format!("pending:{next_id}");
    record_ephemeral_message(app, open_id.clone(), "me".into(), &text, pending_id.clone());
    app.pending_sends.insert(
        next_id,
        PendingSend {
            pending_id,
            contact_id: open_id,
            text,
        },
    );
}

fn handle_json(app: &mut App, v: &Value) {
    if let Some(req_id) = v.get("id").and_then(|id| id.as_u64())
        && let Some(ts) = v.pointer("/result/timestamp").and_then(json_i64)
    {
        confirm_send(app, req_id, ts);
    }

    // contact/group list responses
    if let Some(result) = v.get("result").and_then(|r| r.as_array()) {
        for item in result {
            if item.get("members").is_some() {
                if let Some(gid) = item.get("id").and_then(|x| x.as_str()) {
                    let name = item
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or(gid)
                        .into();
                    add_contact_once(app, gid.into(), name, true);
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
                add_contact_once(app, number.into(), name.into(), false);
            }
        }
        app.status = format!("{} contacts", app.contacts.len());
    }
    // incoming message notification
    if v.get("method").and_then(|m| m.as_str()) == Some("receive") {
        let env = v
            .pointer("/params/envelope")
            .or_else(|| v.pointer("/params/result/envelope"))
            .unwrap_or(&Value::Null);
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
            let from_id = env["sourceNumber"].as_str().unwrap_or(&from);
            let id = json_i64(&dm["timestamp"])
                .or_else(|| json_i64(&env["timestamp"]))
                .map(|ts| msg_id(&cid, from_id, ts));
            let is_new = if let Some(id) = id {
                record_message(app, cid.clone(), from.clone(), text, id, true)
            } else {
                let id = format!("local:{}", app.msg_seq + 1);
                record_ephemeral_message(app, cid.clone(), from.clone(), text, id);
                true
            };
            if is_new {
                if app.open_id.as_deref() != Some(cid.as_str()) {
                    sync_unread_from_disk(app);
                    app.unread.insert(cid.clone());
                    save_unread(app);
                }
                let _ = Notification::new()
                    .summary(&format!("Signal: {}", from))
                    .body(text)
                    .appname("signal-tui")
                    .show();
            }
        }
        let sm = &env["syncMessage"]["sentMessage"];
        if let Some(text) = sm["message"].as_str() {
            let cid = sm["groupInfo"]["groupId"]
                .as_str()
                .or(sm["destinationNumber"].as_str())
                .or(sm["destination"].as_str())
                .or(app.self_id.as_deref())
                .unwrap_or("?")
                .to_string();
            if let Some(ts) = json_i64(&sm["timestamp"]) {
                drop_matching_pending(app, &cid, text);
                let id = msg_id(&cid, "me", ts);
                record_message(app, cid, "me".into(), text, id, true);
            } else {
                let id = format!("local:{}", app.msg_seq + 1);
                record_ephemeral_message(app, cid, "me".into(), text, id);
            }
        }
    }
    if let Some(err) = v.get("error") {
        app.status = format!("error: {}", err["message"].as_str().unwrap_or("rpc error"));
    }
}

/// Messages visible in the chat panel — uses open_id, falls back to highlighted contact.
fn chat_messages<'a>(app: &'a App, filtered: &[(String, String, bool)]) -> Vec<&'a Msg> {
    let id: Option<&str> = app.open_id.as_deref().or_else(|| {
        app.selected
            .selected()
            .and_then(|i| filtered.get(i).map(|(id, _, _)| id.as_str()))
    });
    match id {
        None => vec![],
        Some(id) => app
            .messages
            .iter()
            .filter(|(cid, _)| cid.as_str() == id)
            .map(|(_, m)| m)
            .collect(),
    }
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
        .map(|(id, name, is_group)| {
            let label = if *is_group {
                format!("# {}", name)
            } else {
                name.clone()
            };
            let markers = format!(
                "{}{}",
                if app.unread.contains(id) { "*" } else { "" },
                if app.favorites.contains(id) {
                    "★"
                } else {
                    ""
                }
            );
            let label = if markers.is_empty() {
                label
            } else {
                format!("{markers} {label}")
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
        .and_then(|id| {
            filtered
                .iter()
                .find(|(fid, _, _)| fid == id)
                .map(|(_, n, _)| n.clone())
        })
        .unwrap_or_default();
    let msgs_buf: Vec<&Msg> = chat_messages(app, &filtered);
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
    let msg_scroll = lines
        .len()
        .saturating_sub(right[0].height.saturating_sub(2) as usize)
        .min(u16::MAX as usize) as u16;
    let msgs = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((msg_scroll, 0))
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
        Focus::List => "j/k navigate · / search · f favorite · i/Enter compose · q quit",
        Focus::Search => "type to filter/new chat · Enter confirm · Esc cancel",
        Focus::Input => "Enter send · /help commands · Esc back",
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
            unread: HashSet::new(),
            favorites: HashSet::new(),
            seen_msg_ids: HashSet::new(),
            pending_sends: HashMap::new(),
            history_path: None,
            msg_seq: 0,
            input: String::new(),
            focus: Focus::List,
            search: String::new(),
            connected: true,
            pending_g: false,
            open_id: None,
            self_id: None,
            status: String::new(),
        };
        app.selected.select(Some(0));
        app
    }

    fn add_contact(app: &mut App, id: &str, name: &str) {
        app.contacts.push(Contact {
            id: id.into(),
            name: name.into(),
            is_group: false,
        });
    }

    fn add_group(app: &mut App, id: &str, name: &str) {
        app.contacts.push(Contact {
            id: id.into(),
            name: name.into(),
            is_group: true,
        });
    }

    #[test]
    fn parse_linked_account_reads_json_number() {
        assert_eq!(
            parse_linked_account(r#"[{"number":"+123"}]"#).as_deref(),
            Some("+123")
        );
    }

    #[test]
    fn cli_help_lists_commands() {
        let help = help_text();
        assert!(help.contains("daemon"));
        assert!(help.contains("status"));
        assert!(help.contains("completions"));
    }

    #[test]
    fn completion_scripts_include_commands() {
        assert!(completion_script("zsh").unwrap().contains("daemon"));
        assert!(completion_script("bash").unwrap().contains("complete -F"));
        assert!(
            completion_script("fish")
                .unwrap()
                .contains("complete -c signal-tui")
        );
        assert!(completion_script("nope").is_none());
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "signal-tui-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn favorites_file_roundtrip() {
        let path = temp_path("favorites");
        let favorites = HashSet::from(["+2".to_string(), "+1".to_string()]);
        write_favorites(&path, &favorites).unwrap();
        assert_eq!(read_favorites(&path).unwrap(), favorites);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn history_file_roundtrip() {
        let path = temp_path("history");
        let record = HistoryRecord {
            id: msg_id("+1", "me", 123),
            contact_id: "+1".into(),
            from: "me".into(),
            text: "hi".into(),
        };
        append_history(&path, &record).unwrap();
        let records = read_history(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, record.id);
        assert_eq!(records[0].text, "hi");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unread_file_roundtrip() {
        let path = temp_path("unread");
        let unread = HashSet::from(["+2".to_string(), "grp".to_string()]);
        write_set(&path, &unread).unwrap();
        assert_eq!(read_set(&path).unwrap(), unread);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn finds_timestamp_in_send_output() {
        assert_eq!(find_timestamp(&json!({"timestamp": 123})), Some(123));
        assert_eq!(
            find_timestamp(&json!({"result": {"timestamp": 456}})),
            Some(456)
        );
    }

    #[test]
    fn current_pid_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    /// Simulate pressing i/Enter on the currently highlighted contact.
    fn open_selected(app: &mut App) {
        open_selected_contact(app);
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
    fn send_message_noop_when_no_contacts_at_all() {
        let mut app = make_app();
        // no contacts loaded, open_id None — nothing to fall back to
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        assert!(
            app.messages.is_empty(),
            "send must not fire with no contacts"
        );
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
    fn send_response_persists_pending_message() {
        let path = temp_path("send-history");
        let mut app = make_app();
        app.history_path = Some(path.clone());
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 42);
        handle_json(
            &mut app,
            &json!({"jsonrpc":"2.0","id":42,"result":{"timestamp":123}}),
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].1.id, msg_id("+1", "me", 123));
        assert_eq!(read_history(&path).unwrap().len(), 1);
        let _ = fs::remove_file(path);
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
        assert!(
            v["params"]["recipient"].is_null(),
            "group send must not include recipient"
        );
    }

    #[test]
    fn send_message_self_uses_note_to_self_rpc() {
        let mut app = make_app();
        app.self_id = Some("+1".into());
        add_contact(&mut app, "+1", "Note to Self");
        open_selected(&mut app);
        app.input = "test".into();
        let mut sink = vec![];
        send_message(&mut app, &mut sink, 1);
        let v: Value = serde_json::from_str(String::from_utf8(sink).unwrap().trim()).unwrap();
        assert_eq!(v["params"]["noteToSelf"], true);
        assert!(v["params"]["recipient"].is_null());
    }

    #[test]
    fn slash_chat_opens_new_chat_without_sending() {
        let mut app = make_app();
        app.input = "/chat +2".into();
        let mut sink = vec![];
        assert!(!submit_input(&mut app, &mut sink, 1));
        assert!(sink.is_empty());
        assert_eq!(app.open_id.as_deref(), Some("+2"));
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_self_opens_note_to_self() {
        let mut app = make_app();
        app.self_id = Some("+1".into());
        app.input = "/self".into();
        assert!(!submit_input(&mut app, &mut vec![], 1));
        assert_eq!(app.open_id.as_deref(), Some("+1"));
        assert!(app.contacts.iter().any(|c| c.name == "Note to Self"));
    }

    // ── chat_messages (visibility) ────────────────────────────────────────────

    fn visible<'a>(app: &'a App) -> Vec<&'a Msg> {
        let filtered: Vec<(String, String, bool)> = app
            .filtered()
            .into_iter()
            .map(|c| (c.id.clone(), c.name.clone(), c.is_group))
            .collect();
        chat_messages(app, &filtered)
    }

    fn screen_text(app: &mut App) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn draw_renders_sent_message_text() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        assert!(screen_text(&mut app).contains("hello"));
    }

    #[test]
    fn draw_marks_unread_contacts() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        app.unread.insert("+1".into());
        assert!(screen_text(&mut app).contains("* Alice"));
    }

    #[test]
    fn draw_marks_favorite_contacts() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        app.favorites.insert("+1".into());
        assert!(screen_text(&mut app).contains("★ Alice"));
    }

    #[test]
    fn favorites_sort_first() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        add_contact(&mut app, "+2", "Bob");
        app.favorites.insert("+2".into());
        assert_eq!(app.filtered()[0].id, "+2");
    }

    #[test]
    fn confirm_search_starts_chat_when_no_contact_matches() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        app.focus = Focus::Search;
        app.search = "+2".into();
        confirm_search(&mut app);
        assert_eq!(app.open_id.as_deref(), Some("+2"));
        assert!(app.focus == Focus::Input);
        assert!(app.contacts.iter().any(|c| c.id == "+2"));
    }

    #[test]
    fn chat_messages_empty_without_open_id() {
        let app = make_app();
        assert!(visible(&app).is_empty());
    }

    #[test]
    fn chat_messages_shows_sent_message() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 1);
        let v = visible(&app);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "hello");
        assert_eq!(v[0].from, "me");
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

        let v = visible(&app);
        assert_eq!(v.len(), 1, "only bob's message should show");
        assert_eq!(v[0].text, "to bob");
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

        // simulate incoming from Bob → Bob bumps to seq=2, moves to index 0
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+2","sourceName":"Bob","dataMessage":{"message":"hey"}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        // now filtered = [Bob(2), Alice(1)]

        // open_id is still "+1" (Alice); messages must still show Alice's message
        assert_eq!(app.open_id.as_deref(), Some("+1"));
        let v = visible(&app);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "hi alice");
    }

    #[test]
    fn send_without_explicit_open_uses_highlighted_contact() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        // never called open_selected — open_id stays None
        app.focus = Focus::Input;
        app.input = "hi".into();
        send_message(&mut app, &mut vec![], 1);
        // open_id should have been set from highlighted contact
        assert_eq!(app.open_id.as_deref(), Some("+1"));
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].1.text, "hi");
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
    fn drain_incoming_limits_work_per_tick() {
        let mut app = make_app();
        let (tx, rx) = mpsc::channel();
        for i in 0..3 {
            tx.send(json!({"error": {"message": format!("e{i}")}}))
                .unwrap();
        }
        drain_incoming(&mut app, &rx, 2);
        assert_eq!(app.status, "error: e1");
        assert!(rx.try_recv().is_ok(), "one event should wait for next tick");
    }

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
    fn incoming_message_marks_unread_until_opened() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        add_contact(&mut app, "+2", "Bob");
        open_selected(&mut app);
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+2","sourceName":"Bob","dataMessage":{"message":"hi"}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        assert!(app.unread.contains("+2"));
        open_chat(&mut app, "+2".into());
        assert!(!app.unread.contains("+2"));
    }

    #[test]
    fn incoming_with_same_timestamp_is_deduped() {
        let mut app = make_app();
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"sourceNumber":"+2","sourceName":"Bob","dataMessage":{"timestamp":123,"message":"hi"}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        handle_json(&mut app, &v);
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn sync_after_send_response_is_deduped() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 42);
        handle_json(
            &mut app,
            &json!({"jsonrpc":"2.0","id":42,"result":{"timestamp":123}}),
        );
        let sync: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"syncMessage":{"sentMessage":{"destinationNumber":"+1","timestamp":123,"message":"hello"}}}}}"#,
        ).unwrap();
        handle_json(&mut app, &sync);
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn sync_before_send_response_is_deduped() {
        let mut app = make_app();
        add_contact(&mut app, "+1", "Alice");
        open_selected(&mut app);
        app.input = "hello".into();
        send_message(&mut app, &mut vec![], 42);
        let sync: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"syncMessage":{"sentMessage":{"destinationNumber":"+1","timestamp":123,"message":"hello"}}}}}"#,
        ).unwrap();
        handle_json(&mut app, &sync);
        handle_json(
            &mut app,
            &json!({"jsonrpc":"2.0","id":42,"result":{"timestamp":123}}),
        );
        assert_eq!(app.messages.len(), 1);
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
    fn parses_sync_sent_message() {
        let mut app = make_app();
        app.self_id = Some("+1".into());
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"syncMessage":{"sentMessage":{"destinationNumber":"+1","message":"note"}}}}}"#,
        ).unwrap();
        handle_json(&mut app, &v);
        assert_eq!(app.messages[0].0, "+1");
        assert_eq!(app.messages[0].1.from, "me");
        assert_eq!(app.messages[0].1.text, "note");
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
