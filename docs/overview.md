# signal-tui — Overview

Terminal UI for Signal messaging. Wraps `signal-cli` and exposes a keyboard-driven chat interface in the terminal.

## System context

```mermaid
graph TB
    User(["👤 User"])
    TUI["signal-tui\n(Rust TUI)"]
    CLI["signal-cli\n(Java CLI)"]
    NET(["Signal Network"])
    OS["macOS\nNotification Center"]

    User -- "keyboard" --> TUI
    TUI -- "renders to terminal" --> User
    TUI -- "JSON-RPC via stdin" --> CLI
    CLI -- "JSON-RPC via stdout" --> TUI
    CLI <-- "Signal protocol / TLS" --> NET
    TUI -- "notify-rust" --> OS
    OS -- "banner / badge" --> User
```

## Runtime topology

```mermaid
graph LR
    subgraph "signal-tui process"
        MT["Main thread\n(event loop + draw)"]
        RT["Reader thread\n(stdout parser)"]
        CH["mpsc channel\nserde_json::Value"]
        RT --> CH --> MT
    end

    subgraph "Child process"
        CLI["signal-cli\n--output=json jsonRpc"]
    end

    MT -- "JSON-RPC line\nstdin pipe" --> CLI
    CLI -- "JSON-RPC line\nstdout pipe" --> RT
```

## Technology stack

| Concern | Crate / Tool | Notes |
|---|---|---|
| TUI rendering | `ratatui` + `crossterm` | Layout, widgets, raw terminal I/O |
| Signal backend | `signal-cli` (subprocess) | Protocol, crypto, account storage |
| IPC | `serde_json` + `std::process` | JSON-RPC 2.0 over child process pipes |
| Notifications | `notify-rust` | macOS `NSUserNotificationCenter` — no app bundle required |
| Theme | dim-sum palette (`Color::Rgb`) | Dark muted color scheme from `dawidsok/dim-sum-theme` |
| Distribution | Homebrew (`dawidsok/tap`) | `brew install signal-tui` |
