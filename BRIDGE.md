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
