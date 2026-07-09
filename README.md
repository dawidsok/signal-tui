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

## Background receiver

Run the receiver without opening the TUI:

```sh
signal-tui daemon
```

In another terminal, open/close the TUI whenever you want:

```sh
signal-tui
```

If the daemon is running, the TUI reads local history/unread state and does not start a second receiver. If no daemon is running, the TUI receives messages itself like before.

Check daemon state:

```sh
signal-tui status
```

Show CLI help:

```sh
signal-tui help
```

Shell completions:

```sh
# zsh
source <(signal-tui completions zsh)

# bash
source <(signal-tui completions bash)

# fish
signal-tui completions fish > ~/.config/fish/completions/signal-tui.fish
```

## Keys

| Key | Action |
|-----|--------|
| Tab | Switch focus (contacts ↔ input) |
| Up / Down | Navigate contacts (contacts focus) |
| / | Search the current chat/contact list |
| n | Show contacts to start a new chat |
| a | Toggle archived chats view |
| i / Enter | Open selected chat |
| f | Toggle favorite for selected/open chat |
| Enter | Send message (input focus) |
| q | Quit (contacts focus) |
| Ctrl-C | Quit (any focus) |

A **Note to Self** chat is added automatically from your linked Signal account, so you can test without messaging other people.

The left pane shows open chats by default, not every contact. Press `n` to browse contacts for a new chat, or `a` to view archived chats.

Contact markers: `*` = unread, `★` = favorite.

Local files:

- Favorites: `~/.config/signal-tui/favorites`
- Chat history: `~/.local/share/signal-tui/messages.jsonl`
- Unread chats: `~/.local/share/signal-tui/unread`
- Receiver status: `~/.local/share/signal-tui/status.json`
- Receiver lock: `~/.local/share/signal-tui/receiver.lock`

History is plain text JSONL on disk; delete the file to clear local history.

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
