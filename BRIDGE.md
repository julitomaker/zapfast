# Local read-only bridge (fork addition)

This fork adds a small HTTP server inside ZapFast so tools on this computer
(a dashboard, an assistant) can read the chats to organize them. It cannot
send, delete, or mark anything read.

- Listens on `127.0.0.1:47120` only, `GET` only, and refuses other `Host` names.
- Every request needs `Authorization: Bearer <token>`.
- Port and token are in `bridge.json` in the config folder
  (`~/Library/Application Support/me.paolino.zapfast-julito/bridge.json` on macOS),
  created with mode 0600 on first run. Set `"enabled": false` to turn it off.
- Uses its own query-only SQLCipher connection, keyed from the OS keyring.
- Locked chats are never served.

## Routes

`account` defaults to `1`. Times are Unix seconds.

| Route | What it returns |
|---|---|
| `/health` | `{ ok, version }` |
| `/accounts` | Linked account ids |
| `/chats?account=` | Chats, newest activity first: id, name, kind, unread, archived, pinned, muted, message count |
| `/messages?account=&chat=&since=&before=&limit=` | With `since`, the first `limit` messages from then on; otherwise the newest `limit` before `before`. Without `chat`, across all chats. Default limit 200, max 5000 |
| `/search?account=&q=&chat=&limit=` | Messages whose text, caption or file name contains `q`, newest first |

Each message: `chat`, `chat_name`, `id`, `sender`, `sender_name`, `from_me`,
`timestamp`, `kind`, `text`, `reply_to`, `edited`.

```sh
TOKEN=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser('~/Library/Application Support/me.paolino.zapfast-julito/bridge.json')))['token'])")
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:47120/messages?since=$(( $(date +%s) - 86400 ))"
```

## Building this fork on macOS

```sh
cargo build --release --locked
packaging/macos/bundle-julito.sh
```

The fork has its own identity (`paths::APP_NAME`, bundle id
`com.julitomaker.zapfast`): its own folders, linked device, archive and
keyring entry, so it runs beside ZapFast. It asks for WhatsApp's full
history backfill (365 days) when linking. `bundle-julito.sh` takes the app
icon and message sound from the local WhatsApp installation, never from
this repository.

Each rebuild has a new ad-hoc signature, so macOS asks once more for the
keychain item: choose "Always Allow". Self-update is off in this fork; bring
upstream changes in with `git fetch upstream && git rebase upstream/main`.

## Claude Desktop (MCP)

The app binary is also a stdio MCP server: `<app>/Contents/MacOS/zapfast mcp`.
Settings > Integrations > Claude Desktop > Connect adds it to Claude
Desktop's `claude_desktop_config.json` as `whats-pro` (keeping the other
servers, with a `.bak` copy). Tools, all read-only: `list_chats`,
`read_chat`, `recent_activity`, `search_messages`. Whats Pro must be open,
since the MCP server reads through the bridge.

## Local transcription and picture text (fork addition)

- Voice messages, audio files and videos are transcribed on this computer
  with whisper.cpp (`whisper-rs`): Metal on Apple Silicon, the CPU on Intel
  Macs. The model, `ggml-large-v3-turbo-q5_0.bin` (~550 MB), downloads from
  whisper.cpp's model repository into `<data>/models/` on first use.
- The text in pictures is read with macOS's Vision framework.
- Settings > Transcription chooses Off, Chosen chats or All chats, for speech
  and for pictures; a chat's right-click menu chooses it. Older messages have a
  Transcribe / Read text button under the bubble.
- Results live in the archive's `transcripts` table; the bridge returns them
  as each message's `transcript`, search matches them, and the MCP tools show
  them as `[transcript: …]`.
- `zapfast transcribe <file>` prints a file's transcription from the shell.

### Choosing the engine

Settings > Transcription > Engine picks who reads voice and videos (this
computer with Whisper, the OpenAI, Groq or Gemini API, or the Gemini CLI)
and pictures (this computer with Vision for text only, or Claude Code,
Codex or Gemini CLI, or the Anthropic, OpenAI, Gemini or OpenRouter API,
which also describe the picture). Models are editable; API keys live in the
OS keyring under `com.julitomaker.whatspro.ai`, shared with Projects.

## Running on a Linux server (fork addition)

`zapfast serve` runs Whats Pro with no window, as a linked device of its
own, so Claude Code on a server (Ubuntu 22.04, say) can use the `zapfast
mcp` tools, reading and sending, while the desktop is off. It keeps the
link, the archive, the bridge and the project analysis running; it shows
no notifications, plays no sounds and never changes the settings file.

### Building

Rust comes from rustup (the version in `rust-toolchain.toml`); then:

```sh
sudo apt install build-essential cmake perl pkg-config clang libclang-dev \
  libasound2-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev poppler-utils
cargo build --release --locked
sudo install -m 755 target/release/zapfast /usr/local/bin/zapfast
```

A small server may run out of memory linking; add swap or build with
`CARGO_BUILD_JOBS=2`. `poppler-utils` is optional: its `pdftotext` lets the
MCP read the text of PDFs, which macOS reads with PDFKit.

### Keys without a keyring

A server has no OS keyring, so `--secrets-dir <dir>` keeps the keys as files
in a folder only you can read (0700, each file 0600):

- `archive-<digest>.key`: the archive's 32-byte SQLCipher key, made on the
  first start. **Back it up**: without it the archive cannot be read.
  `WHATSPRO_ARCHIVE_KEY=<64 hex digits>` gives the key instead of the file.
- `ai-<provider>`: an AI provider's API key (`ai-OpenAi`, `ai-Anthropic`,
  `ai-Gemini`, `ai-OpenRouter`, `ai-Groq`), as plain text:
  `printf '%s' "$KEY" > ~/.whatspro-secrets/ai-OpenAi`.

The flag sets `WHATSPRO_SECRETS_DIR`, which the processes Whats Pro starts
inherit. Without it, and with no D-Bus session to reach a keyring, Whats Pro
stops and names the flag; it never opens a throwaway archive instead.

### Linking

Link once from an interactive terminal (ssh, tmux):

```sh
zapfast serve --secrets-dir ~/.whatspro-secrets
```

It prints a QR code to scan with WhatsApp on the phone (Linked devices >
Link a device). Or link with a code: `--pair-phone +5491122334455` prints an
eight-character code to enter under Link with phone number instead. With
`--qr-png <path>` the QR code is also saved as a PNG (0600), deleted once
linked. The log (`~/.local/state/zapfast-julito/zapfast.log`) records only
the link's state, never the code. Ctrl-C or SIGTERM stops it cleanly.

### Keeping it running

As a systemd user service, after `loginctl enable-linger $USER`, in
`~/.config/systemd/user/whatspro.service`:

```ini
[Unit]
Description=Whats Pro (headless)
After=network-online.target

[Service]
ExecStart=/usr/local/bin/zapfast serve --secrets-dir %h/.whatspro-secrets
Restart=on-failure

[Install]
WantedBy=default.target
```

`systemctl --user enable --now whatspro`. Only one copy runs per user.

### Files

- `~/.config/zapfast-julito/`: `settings.json`, `bridge.json`, `mcp.json`,
  `projects.json`.
- `~/.local/state/zapfast-julito/`: `accounts/<id>/` with `session.db` and
  `archive.db`, and `zapfast.log`.
- `~/.cache/zapfast-julito/`: media and avatars.

### Claude Code

Sending is off by default. To let Claude Code send, not only draft, set
`send` in `~/.config/zapfast-julito/mcp.json` (fields left out keep their
defaults):

```json
{ "send": true }
```

Then add the MCP server for every project, with the same secrets folder:

```sh
claude mcp add --scope user -e WHATSPRO_SECRETS_DIR=$HOME/.whatspro-secrets \
  whats-pro -- /usr/local/bin/zapfast mcp
```
