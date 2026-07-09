# signal-tui

Terminal UI for Signal messaging, wrapping `signal-cli` JSON-RPC.

## Requirements

- macOS (arm64 binary in the Homebrew formula)
- Signal account on a phone

## Install via Homebrew

```sh
brew tap dawidsok/tap
brew install signal-tui
```

## First run

On first launch, `signal-tui` detects no linked account and runs the Signal device-linking flow automatically:

1. A QR code is printed in your terminal
2. On your phone: **Settings → Linked Devices → Link New Device**
3. Scan the QR code
4. Once confirmed, the TUI starts

No manual `signal-cli` setup needed.

## Build from source

```sh
cargo build --release
./target/release/signal-tui
```

`signal-cli` must be on `$PATH` (`brew install signal-cli`).

## Keys

| Key | Action |
|-----|--------|
| Tab | Switch focus (contacts ↔ input) |
| Up / Down | Navigate contacts (contacts focus) |
| Enter | Send message |
| q | Quit (contacts focus) |
| Ctrl-C | Quit (any focus) |
