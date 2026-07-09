# signal-tui — User Flows

## 1. First-run device linking

Runs once, before the TUI starts. signal-cli prints a QR code to the terminal.

```mermaid
stateDiagram-v2
    [*] --> CheckLinked : binary starts
    CheckLinked --> Linking : no account found\n(is_linked = false)
    CheckLinked --> TUI : account exists\n(is_linked = true)

    Linking --> QRDisplayed : signal-cli link\nprints QR to stdout
    QRDisplayed --> Scanning : user opens Signal\nSettings → Linked Devices
    Scanning --> Confirmed : phone scans QR
    Confirmed --> TUI : signal-cli exits 0

    Scanning --> Failed : user cancels / timeout
    Failed --> [*] : process exits with error
```

## 2. Startup — contact loading

```mermaid
sequenceDiagram
    participant TUI as signal-tui (main)
    participant RT as Reader thread
    participant CLI as signal-cli

    TUI->>CLI: listContacts (id=1)
    TUI->>CLI: listGroups   (id=2)
    CLI-->>RT: JSON result array (contacts)
    RT-->>TUI: serde_json::Value via mpsc
    TUI->>TUI: handle_json → App.contacts populated
    CLI-->>RT: JSON result array (groups)
    RT-->>TUI: serde_json::Value via mpsc
    TUI->>TUI: handle_json → App.contacts += groups
    TUI->>TUI: draw() — contacts list renders
```

## 3. Focus state machine

```mermaid
stateDiagram-v2
    [*] --> List : app starts

    List --> Search : /
    Search --> List : Enter (confirm filter)
    Search --> List : Esc (clear + cancel)

    List --> Input : i or Enter\n(sets open_id)
    Input --> List : Esc

    List --> [*] : q or Ctrl-C
    Input --> [*] : Ctrl-C
    Search --> [*] : Ctrl-C
```

## 4. Contact navigation (List focus)

```mermaid
flowchart TD
    L[List focus] --> JK{"j / k\nor ↑ / ↓"}
    JK -- j / ↓ --> DN[selected += 1\nclamped to filtered.len-1]
    JK -- k / ↑ --> UP[selected -= 1\nclamped to 0]
    DN --> L
    UP --> L

    L --> GG{"g pressed?"}
    GG -- "first g" --> PG[pending_g = true]
    PG --> GG2{"g again?"}
    GG2 -- yes --> TOP[selected = 0\npending_g = false]
    GG2 -- other key --> PG2[pending_g = false\nhandle key normally]
    TOP --> L

    L --> BIG_G["G"] --> BOT[selected = filtered.len - 1]
    BOT --> L

    L --> SLASH["/"] --> SF[Focus::Search\nsearch.clear\nselected = 0]
```

## 5. Contact search / filter

```mermaid
sequenceDiagram
    participant U as User
    participant App

    U->>App: / (in List focus)
    App->>App: focus = Search, search = ""

    loop typing
        U->>App: char
        App->>App: search.push(ch)\nselected = 0
        App->>App: draw() — filtered() re-runs\ncontacts list narrows live
    end

    alt confirm
        U->>App: Enter
        App->>App: focus = List\n(search string kept — filter stays active)
    else cancel
        U->>App: Esc
        App->>App: search.clear()\nfocus = List\nclamp_selected()
    end
```

## 6. Sending a message

```mermaid
sequenceDiagram
    participant U as User
    participant App
    participant CLI as signal-cli

    U->>App: i or Enter (List focus)
    App->>App: open_id = filtered[selected].id\nfocus = Input

    loop typing
        U->>App: char
        App->>App: input.push(ch)
    end

    U->>App: Enter (Input focus, input non-empty)
    App->>App: send_message()
    App->>CLI: JSON-RPC send\n{recipient / groupId, message}
    App->>App: messages.push((open_id, Msg{from:"me", …}))\nlast_msg_seq[open_id] = ++msg_seq\ninput.clear()
    App->>App: draw() — message appears in chat panel
```

## 7. Receiving a message

```mermaid
sequenceDiagram
    participant NET as Signal Network
    participant CLI as signal-cli
    participant RT as Reader thread
    participant App
    participant OS as macOS Notification

    NET->>CLI: incoming message
    CLI->>RT: JSON-RPC "receive" push (stdout line)
    RT->>App: Value via mpsc channel

    App->>App: handle_json()\nmessages.push((cid, Msg{from, text}))\nlast_msg_seq[cid] = ++msg_seq

    App->>OS: Notification "Signal: {from}" body=text
    OS->>U: banner notification

    App->>App: draw()\n— contact floats to top of sorted list\n— if open_id == cid, message appears in chat
```

## 8. Contact sort order

Contacts are sorted by `last_msg_seq` (descending) on every render. The most recently active contact is always at the top.

```mermaid
flowchart LR
    S[("last_msg_seq\nHashMap")] --> F["App::filtered()\n1. apply search filter\n2. sort by seq desc"]
    F --> L[Contacts list\n(rendered order)]

    EV1[send_message] -- "seq++" --> S
    EV2[handle_json\n(receive)] -- "seq++" --> S
```
