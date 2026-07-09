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
| / | Search contacts; type a new number and Enter to start a chat |
| i / Enter | Open selected chat |
| f | Toggle favorite for selected/open chat |
| Enter | Send message (input focus) |
| q | Quit (contacts focus) |
| Ctrl-C | Quit (any focus) |

A **Note to Self** chat is added automatically from your linked Signal account, so you can test without messaging other people.

Contact markers: `*` = unread, `★` = favorite. Favorites are stored locally in `~/.config/signal-tui/favorites`.

## Slash commands

Type these in the message input:

| Command | Action |
|---------|--------|
| `/help` | Show available commands in the status line |
| `/chat <number>` | Open/start a chat with a phone number |
| `/new <number>` | Alias for `/chat` |
| `/self` or `/me` | Open Note to Self |
| `/fav` or `/favorite` | Favorite selected/open chat |
| `/unfav` or `/unfavorite` | Remove selected/open chat from favorites |
