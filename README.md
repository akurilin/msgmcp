# msgmcp

A local MCP server, written in Rust, for message history on a Mac. It reads iMessage
and SMS from `chat.db` and WhatsApp Desktop from its `ChatStorage.sqlite`, both
read-only, and sends iMessage text through the Messages app.

## Architecture

```mermaid
flowchart LR
  subgraph cloud["Cloud sandbox"]
    agent["Agent<br/>agent/msgmcp.sh, curl"]
  end
  subgraph mac["Mac"]
    serve["tailscale serve<br/>https://my-mac.tail4f2e1c.ts.net"]
    local["Local MCP clients<br/>msgmcp stdio adapter, schema, call"]
    subgraph app["msgmcp.app (LaunchAgent)"]
      gateway["msgmcp gateway<br/>127.0.0.1:8765/mcp, bearer token"]
      socket["or msgmcp socket<br/>Unix socket, this user only"]
    end
    stores[("chat.db, ChatStorage.sqlite,<br/>Contacts")]
    messages["Messages app"]
    audit[/"Audit log<br/>~/Library/Logs/msgmcp"/]
  end
  agent -- "tailnet, tag:agent:<br/>HTTPS to the Mac only" --> serve
  serve --> gateway
  local --> app
  app -- "Full Disk Access, read-only" --> stores
  app -- "Automation: send,<br/>Contacts recipients only" --> messages
  app --> audit
```

- **One privileged process.** Only `~/Applications/msgmcp.app`, run by a LaunchAgent,
  holds the macOS permissions and opens the databases. Everything else, local or remote,
  is a client of it, and no client can do more than the tools allow.
- **Two services, same tools.** The LaunchAgent runs one of them:
  - `msgmcp gateway` (the default): MCP over HTTP at `http://127.0.0.1:8765/mcp`, with a
    bearer token, for clients that can't reach a local socket.
  - `msgmcp socket`: MCP on a Unix socket that only processes running as your user can open.
- **Local clients** run the same binary with no arguments as a stdio adapter; scripts use
  `msgmcp schema` and `msgmcp call`. They use the socket if it exists, otherwise the gateway.
- **Remote access is two locks.** The tailnet policy lets devices tagged `tag:agent`
  reach only port 443 on the Mac, where `tailscale serve` forwards to the gateway; the
  gateway then requires its bearer token. Remote agents can send only to people saved in
  Contacts, after you approve a draft, and every call lands in an audit log.
- **Agents set themselves up.** A sandbox agent follows [agent/README.md](agent/README.md),
  which joins the tailnet without root and checks the whole path, including that it really
  is restricted, before it reads anything.

## Install

```sh
python3 scripts/reinstall.py --sign "Apple Development: jane@example.com (A1B2C3D4E5)"
```

The script builds the release binary, installs `~/Applications/msgmcp.app` and its
LaunchAgent, and checks that the service can read both stores. List your certificates with
`security find-identity -v -p codesigning`; a free Apple Development certificate from Xcode
works. To skip `--sign`, set `MSGMCP_SIGN_IDENTITY` in `.env` (see [.env.example](.env.example)). The first time, grant **msgmcp** Full Disk Access (to read `chat.db` and WhatsApp's
group container) under System Settings → Privacy & Security, then run the script again.
Automation → Messages (to send) is requested on the first send.

**To reinstall after changes**, run `python3 scripts/reinstall.py` with no arguments. It reuses
the installed app's certificate and service options, so macOS keeps its grants. Options:
`--service socket`, `--allow-host my-mac.tail4f2e1c.ts.net`, `--allow-send`; passing any of them
replaces the installed set. Restart MCP clients afterwards to pick up tool changes.

macOS ties privacy grants to the app's signature, so the script refuses an ad hoc signature,
which changes with every build. If the check reports missing Full Disk Access (for example
after switching certificates), remove **msgmcp** from Full Disk Access with "–", add
`~/Applications/msgmcp.app` with "+", and run the script again.
`python3 scripts/install_service.py --uninstall` removes the app and LaunchAgent.

## Connect a client

### Apps on this Mac

The adapter command is always `/Users/<you>/Applications/msgmcp.app/Contents/MacOS/msgmcp`.

- **Claude Desktop**: add the entry below to `~/Library/Application Support/Claude/claude_desktop_config.json`,
  then quit the app with ⌘Q and reopen it. Its tools then appear as `msgmcp__*`; in
  cloud sessions linked to the Mac they appear as `mcp__remote-devices__msgmcp__*`.
  ```json
  "mcpServers": { "msgmcp": { "command": "/Users/<you>/Applications/msgmcp.app/Contents/MacOS/msgmcp" } }
  ```
- **Codex**: `codex mcp add msgmcp -- "$HOME/Applications/msgmcp.app/Contents/MacOS/msgmcp"`
- **Claude Code**: `claude mcp add msgmcp -- "$HOME/Applications/msgmcp.app/Contents/MacOS/msgmcp"`

The service describes its own tools, so they aren't documented by hand. To see them, or to
call one from a script:

```sh
msgmcp schema
msgmcp call list_conversations '{"query": "sarah"}'
```

### A cloud agent

Set up [remote access](#remote-access) first. Then give the agent:

1. **Its environment** ([.env.example](.env.example) has samples of each):
   - `MSGMCP_URL`: the gateway's tailnet address, such as `https://my-mac.tail4f2e1c.ts.net/mcp`.
     `tailscale status --self` shows the Mac's name; the tailnet's domain is under DNS in
     the Tailscale admin console.
   - `TS_AUTHKEY` (secret): a reusable, ephemeral Tailscale auth key tagged `tag:agent`.
   - `MSGMCP_TOKEN` (secret): the gateway token. This copies it without printing it:
     `pbcopy < "$HOME/Library/Application Support/MSGMCP/gateway.token"`
   - `MSGMCP_AGENT_HOSTNAME` (optional): the sandbox's name on the tailnet and in the audit
     log; `msgmcp-agent` by default.
2. **Read access to this repository**, for example through the agent platform's GitHub
   connection.
3. **This instruction**, pasted into its chat or saved instructions:

   > You can read my iMessage, SMS and WhatsApp history, and send iMessages once I approve
   > a draft, through msgmcp. At the start of every session, fetch the `agent/` folder of
   > github.com/jane-doe/msgmcp fresh and follow `agent/README.md`. If any setup check
   > fails, stop and tell me.

   Replace `github.com/jane-doe/msgmcp` with the copy of this repository the agent can read.

The agent then joins the tailnet, checks that it is restricted, and reads the tool list
itself; nothing about the tools needs to be pasted in.

## Remote access

The gateway listens on `127.0.0.1` only. To reach it from elsewhere, publish it on a private
network, for example with `tailscale serve --bg 8765`, and reinstall with
`--allow-host my-mac.tail4f2e1c.ts.net` (the name `tailscale serve` prints) so the gateway accepts the
proxied `Host` header. Requests
from web pages (an `Origin` header) are refused.

For Tailscale, the tailnet policy should give `tag:agent` only `tcp:443` on the Mac, and
sandboxes join with a reusable, ephemeral auth key tagged `tag:agent`. A device that joins
untagged counts as one of your own devices and gets your access. To connect an agent, see
[Connect a client](#a-cloud-agent).

Every request needs `Authorization: Bearer <token>`. The gateway creates a random token in
`~/Library/Application Support/MSGMCP/gateway.token` (mode 0600) on first start; delete the
file and restart the service to rotate it. The gateway is stateless, so an agent without MCP
can POST one JSON-RPC message:

```sh
curl -s https://my-mac.tail4f2e1c.ts.net/mcp -H "Authorization: Bearer $MSGMCP_TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_conversations","arguments":{"limit":5}}}'
```

**Sending.** Message text is written by whoever sent it, so treat it as untrusted input to
an agent. `send_message` stays off on the gateway unless it runs with `--allow-send`, and on
every service it sends only to phone numbers and email addresses saved in Contacts. That
stops an agent steered by a stranger's message from texting the stranger. A number saved
without a country code counts as North American (`+1`), as Messages dials it. A success
means submitted, not delivered; never retry a send automatically.

**Audit log.** Both services append one JSON line per tool call to
`~/Library/Logs/msgmcp/audit.log` (Console.app shows it under Log Reports): the time,
service, caller, tool, arguments, outcome and any error. The gateway also logs requests it
refuses. Sent message text is logged as an argument; what tools return is never logged. The
log rotates at 10 MB and keeps five old files.

The caller is the tailnet device behind the gateway (its address, name, and tags or owner,
from `tailscale whois`, cached for ten minutes), `127.0.0.1` for apps on this Mac using the
gateway directly, or for socket clients the process and the program that started it.

```sh
tail -f ~/Library/Logs/msgmcp/audit.log
```

## Security

**Kept out:** the internet. The gateway listens on `127.0.0.1`, `tailscale serve` exposes it
to your tailnet only (never use Funnel), agent devices may reach only port 443, and every
request needs the token. Databases are read-only; sending reaches Apple Contacts only, and
every call is in the [audit log](#remote-access).

**Not kept out:**

- **What people text you.** It can steer an agent into leaking what it reads, or, with
  `--allow-send`, texting your contacts. Approving drafts is the agent's rule, not the server's.
- **WhatsApp names.** Senders choose them, so a stranger can show up as "Mom".
- **Your accounts.** The agent platform, GitHub and Tailscale can each let someone in.
- **Programs running as you.** They can read the token and use the socket.

## Limits

This is a proof of concept for the current macOS `chat.db` and WhatsApp Desktop schemas,
which can change without notice.

## License

GPL-3.0-or-later. Text decoding uses [crabstep](https://github.com/ReagentX/crabstep).
`scripts/fixtures/attributed_body` is crabstep 0.7.0's `AttributedBodyTextOnly` test fixture.
