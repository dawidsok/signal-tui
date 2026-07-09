# signal-tui

Minimal Signal TUI wrapping `signal-cli` JSON-RPC.

## Setup

```sh
brew install signal-cli
signal-cli link -n "signal-tui"   # scan QR with phone (Settings > Linked devices)
cargo run
```

## Keys

- Tab — switch focus (contacts / input)
- Up/Down — pick contact (list focus)
- Enter — send message
- q (list focus) or Ctrl-C — quit
