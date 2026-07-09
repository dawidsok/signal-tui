# signal-tui — Architecture

## High-level: startup + steady-state data flow

```mermaid
flowchart TD
    START([binary starts]) --> LINKED{is_linked?}
    LINKED -- "no" --> LINK[run_link\nprint QR, block until scan]
    LINK --> LINKED
    LINKED -- "yes" --> SPAWN[spawn signal-cli\n--output=json jsonRpc]
    SPAWN --> RPCS[send listContacts\n+ listGroups RPCs]
    RPCS --> LOOP

    subgraph LOOP["steady-state event loop (100 ms tick)"]
        direction TB
        RECV[drain mpsc channel] --> HJ[handle_json\nupdate App state]
        HJ --> DRAW[draw\nrender to terminal]
        DRAW --> POLL{key event?}
        POLL -- "no" --> RECV
        POLL -- "yes" --> DISPATCH[dispatch by Focus\nList / Search / Input]
        DISPATCH --> MUTATE[mutate App state\noptionally call send_message]
        MUTATE --> RECV
    end

    SPAWN --> RT[reader thread\nparse stdout lines → mpsc]
    RT -.->|serde_json::Value| RECV
```

## App state

```mermaid
classDiagram
    class App {
        +Vec~Contact~ contacts
        +ListState selected
        +Vec~(String, Msg)~ messages
        +HashMap~String,usize~ last_msg_seq
        +usize msg_seq
        +String input
        +Focus focus
        +String search
        +bool connected
        +bool pending_g
        +Option~String~ open_id
        +String status
        +filtered() Vec~Contact~
    }

    class Contact {
        +String id
        +String name
        +bool is_group
    }

    class Msg {
        +String from
        +String text
    }

    class Focus {
        <<enum>>
        List
        Search
        Input
    }

    App --> Contact : contacts
    App --> Msg : messages (contact_id, Msg)
    App --> Focus : focus
```

**Key invariants:**

- `open_id` is set when the user opens a chat (`i`/`Enter` from List focus). It is **decoupled from the sorted list index** — sort re-orders don't affect the active conversation.
- `messages` is a flat `Vec` keyed by `contact_id`; filtered on render by `open_id` via `chat_messages()`.
- `last_msg_seq` is a monotonic counter per contact, incremented on every send/receive. `filtered()` sorts by this value descending so the most recently active contact is always first.

## Low-level: function call graph

```mermaid
graph TD
    main --> is_linked
    main --> run_link
    main --> rpc
    main --> run

    run --> handle_json
    run --> draw
    run --> send_message
    run --> clamp_selected

    handle_json --> rpc_parse["parse listContacts\n/ listGroups result\n→ App.contacts"]
    handle_json --> receive_parse["parse receive\nnotification\n→ App.messages"]
    handle_json --> Notification["notify_rust::Notification\n(macOS banner)"]

    send_message --> rpc
    send_message --> App_messages["App.messages.push()"]

    draw --> App_filtered["App::filtered()\nsearch + sort"]
    draw --> chat_messages["chat_messages()\nfilter by open_id"]
    draw --> ratatui_render["ratatui layout\n+ widget render"]

    App_filtered --> last_msg_seq_sort["sort by\nlast_msg_seq desc"]
```

## IPC: JSON-RPC message shapes

### Outbound (signal-tui → signal-cli)

```json
{ "jsonrpc": "2.0", "id": 1, "method": "listContacts", "params": {} }
{ "jsonrpc": "2.0", "id": 2, "method": "listGroups",   "params": {} }
{ "jsonrpc": "2.0", "id": 100, "method": "send",
  "params": { "recipient": ["+48123456789"], "message": "hello" } }
{ "jsonrpc": "2.0", "id": 101, "method": "send",
  "params": { "groupId": "abc123==", "message": "hi group" } }
```

### Inbound (signal-cli → signal-tui)

```json
// Contact list response
{ "jsonrpc": "2.0", "id": 1,
  "result": [{ "number": "+48123", "profile": { "givenName": "Alice" } }] }

// Incoming message push
{ "jsonrpc": "2.0", "method": "receive",
  "params": { "envelope": {
    "sourceNumber": "+48123", "sourceName": "Alice",
    "dataMessage": { "message": "hey",
                     "groupInfo": { "groupId": "abc==" } } } } }
```
