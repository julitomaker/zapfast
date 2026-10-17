# Local read-only bridge (fork addition)

This fork adds a small HTTP server inside ZapFast so tools on this computer
(a dashboard, an assistant) can read the chats to organize them. It cannot
send, delete, or mark anything read.

- Listens on `127.0.0.1:47120` only, `GET` only, and refuses other `Host` names.
- Every request needs `Authorization: Bearer <token>`.
- Port and token are in `bridge.json` in the config folder
  (`~/Library/Application Support/me.paolino.zapfast/bridge.json` on macOS),
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
TOKEN=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser('~/Library/Application Support/me.paolino.zapfast/bridge.json')))['token'])")
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:47120/messages?since=$(( $(date +%s) - 86400 ))"
```

## Building this fork on macOS

```sh
cargo build --release --locked
packaging/macos/bundle.sh target/release/zapfast ~/Applications/"ZapFast Julito.app" 0.19.0
```

Each rebuild has a new ad-hoc signature, so macOS asks once more for the
keychain item: choose "Always Allow". Self-update is off in this fork; bring
upstream changes in with `git fetch upstream && git rebase upstream/main`.
