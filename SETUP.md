# Setting up msgmcp

This is the runbook from a fresh checkout to a cloud agent reading your messages. A person
can follow it alone, or an agent on the Mac (such as Claude Code in this repository) can
work through it with them, doing the steps in apps and web pages too if it has browser or
computer use. [README.md](README.md) explains what each piece is and why;
this file only says what to do and how to tell it worked.

- **Part 1** installs the service on the Mac. Apps on the Mac need only this, then
  [Connect a client](README.md#apps-on-this-mac).
- **Part 2** publishes the gateway on your tailnet, for agents in cloud sandboxes.
- **Part 3** connects one cloud agent (Claude, Codex, Grok or any other) whose sandbox can run bash.

## For an agent guiding the setup

- **Find out what's already done.** Run each step's check before doing the step, and skip the
  steps that pass. Say which step you're on.
- **Steps in an app or a web page can be yours.** Some steps happen in Xcode, System
  Settings, the Tailscale admin console or the agent platform rather than a shell. If you
  have browser or computer use, offer to do them; otherwise say exactly what to click and
  wait for the user. Either way, run the step's check afterwards.
- **Some moments stay with the user.** macOS's own password or Touch ID prompts, signing
  in to an account, and anything a tool of yours refuses are theirs. Before you save a
  change that widens or narrows access (the tailnet policy in step 8, a new auth key in
  step 9), show it and wait for a yes.
- **Never handle secrets.** The gateway token and the Tailscale auth key go from where
  they're made straight into the agent platform. Don't read, print, repeat or ask for them,
  and copy the token only with `pbcopy` as shown. Entering them in the platform (step 10)
  is the user's job, even when you've done the steps around it.
- **Print no message data.** The checks here print PASS/FAIL, counts and tool names. Don't
  call tools to show what they return.
- **Each message store is the user's choice.** Ask which of iMessage/SMS, WhatsApp, Signal
  and Telegram agents may read (step 2); serve only those.
- **Sending is the user's choice.** Ask before adding `--allow-send` (step 7). Never send a
  real message during setup.
- **Stop on a failure you can't explain.** Report the command and its error. Don't work
  around a check.

In the commands, `<mac>` is the Mac's full tailnet name, such as `my-mac.tail4f2e1c.ts.net`
(step 5 finds it).

## Part 1: Install on the Mac

### 1. Prerequisites

You need Rust, Xcode's command line tools and a code-signing certificate.

```sh
cargo --version && security find-identity -v -p codesigning
```

**Done when** cargo prints a version and at least one identity is listed, such as
`"Apple Development: jane@example.com (A1B2C3D4E5)"`. Without one, make a free one in
Xcode: Settings → Accounts → add an Apple ID (the user signs in) → Manage Certificates →
"+" → Apple Development.

### 2. Build and install

```sh
python3 scripts/reinstall.py --sign "Apple Development: jane@example.com (A1B2C3D4E5)"
```

Use the identity from step 1, in full. Later runs reuse the installed app's certificate, so
`--sign` is needed only this once (or set `MSGMCP_SIGN_IDENTITY` in `.env`; see
[.env.example](.env.example)).

On a first install it stops before building and lists the message stores found on this Mac.
Ask the user which ones agents may read, then run it again with a `--source` for each, such
as:

```sh
python3 scripts/reinstall.py --sign "Apple Development: jane@example.com (A1B2C3D4E5)" --source imessage --source whatsapp
```

Later runs keep these stores; passing `--source` again replaces the set. The script builds,
installs `~/Applications/msgmcp.app` and its LaunchAgent, then reads a little from each
store. When it serves iMessage, WhatsApp or Telegram, a first install stops there with a Full
Disk Access error. That's expected: go to step 3. Telegram means Telegram for macOS, the
native app; msgmcp can't read it while its Passcode Lock is on.

### 3. Full Disk Access

Needed for iMessage, WhatsApp and Telegram; Signal's folder isn't protected. Open System Settings → Privacy & Security → Full Disk Access, click "+", press ⌘⇧G, enter
`~/Applications/msgmcp.app` and turn it on. macOS asks the user for their password or
Touch ID. Then:

```sh
python3 scripts/reinstall.py
```

**Done when** it ends with "Reinstalled; restart MCP clients to pick up tool changes." If it
still reports Full Disk Access, remove **msgmcp** from the list with "–", add it again,
and rerun. The check also reads WhatsApp Desktop, so on a Mac without WhatsApp Desktop it
fails there; tell the user rather than working around it.

When it serves Signal, the script also prints "Checking Signal", before the Full Disk
Access check, and the first time (in step 2 or here) macOS asks whether msgmcp may use
"Signal Safe Storage" from the Keychain. The user chooses **Always Allow** (macOS may ask
for their login password). It prints "Signal access succeeded" with counts, and later runs
don't ask again. If they chose Deny, rerun the script.

Apps on this Mac can connect now: see [Connect a client](README.md#apps-on-this-mac).

## Part 2: Remote access

Cloud agents reach the Mac only while it is awake and you are logged in, because the
service is a LaunchAgent in your login session.

### 4. Choose whether agents may send

Agents can always read. With `--allow-send`, they can also send iMessages, but only to
addresses saved in Contacts, and only after the user approves a draft (the agent's rule,
not the server's; see [Security](README.md#security)). Ask the user, and remember the
answer for step 7.

### 5. Tailscale on the Mac

The Mac needs Tailscale running and signed in, and its `tailscale` command line tool (see
[Tailscale's CLI docs](https://tailscale.com/kb/1080/cli) if the command is missing). In
the [admin console](https://login.tailscale.com/admin/dns), MagicDNS and HTTPS
Certificates must be on.

```sh
tailscale status --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))'
tailscale ip -4
```

**Done when** the first prints the Mac's name (this is `<mac>`) and the second its tailnet
address, which step 8 needs.

### 6. Publish the gateway on the tailnet

```sh
tailscale serve --bg 8765
tailscale serve status
```

**Done when** the status shows `https://<mac>` proxying to `http://127.0.0.1:8765`. Use
`serve`, never `funnel`: Funnel publishes to the internet.

### 7. Let the gateway accept its tailnet name

```sh
python3 scripts/reinstall.py --allow-host <mac>
```

Add `--allow-send` if the user chose it in step 4. Passing any of these options replaces
the installed set, so give every option you want each time you change one.

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://<mac>/mcp -H 'Content-Type: application/json' -d '{}'
"$HOME/Applications/msgmcp.app/Contents/MacOS/msgmcp" schema --url https://<mac>/mcp >/dev/null && echo ok
```

**Done when** the first prints `401` (the gateway refuses a request without the token) and
the second prints `ok` (with the token, it accepts the tailnet name). A 403 from the second
means `--allow-host` is missing or misspelled.

### 8. Restrict agents in the tailnet policy

Open [Access controls](https://login.tailscale.com/admin/acls) and merge this into the
policy, with the Mac's address from step 5. If you're editing it, show the user the
change and wait for a yes before saving:

```jsonc
{
  "tagOwners": {
    "tag:agent": ["autogroup:admin"],
  },
  "hosts": {
    "msgmcp-mac": "100.101.102.103",
  },
  "grants": [
    // Your own devices keep full access. This replaces a rule with "src": ["*"]:
    // "*" includes tagged devices, so it would give agents everything.
    {"src": ["autogroup:member"], "dst": ["*"], "ip": ["*"]},
    // Agents reach only HTTPS on the Mac, where tailscale serve forwards to the gateway.
    {"src": ["tag:agent"], "dst": ["msgmcp-mac"], "ip": ["tcp:443"]},
  ],
  "tests": [
    {"src": "tag:agent", "accept": ["msgmcp-mac:443"], "deny": ["msgmcp-mac:22", "msgmcp-mac:7000"]},
  ],
}
```

Merge, don't replace: keep the policy's other sections, and convert any `acls` rule whose
`src` is `"*"` the same way (`"src": ["autogroup:member"]`). Nothing else may grant
`tag:agent` access.

**Done when** the console saves the policy; it runs the `tests` on save and refuses a
policy that fails them. The real check is the agent's in step 12.

### 9. Make an auth key for agents

Open [Keys](https://login.tailscale.com/admin/settings/keys) and generate an auth key with
**Reusable** and **Ephemeral** on and the tag `tag:agent`. The console shows the key once:
if you generated it, stop there and leave it on screen for the user to copy in step 10,
without reading it out. Keys expire (90 days at most); a new one replaces it in step 10.
Without the tag, a sandbox joins as one of the user's own devices, with the user's access.

## Part 3: Connect a cloud agent

### 10. Give the agent its environment

The agent platform must offer a Linux sandbox that runs bash, curl and tar; outbound HTTPS
(to download Tailscale and join the tailnet); environment variables or secrets; and read
access to a copy of this repository, such as a GitHub connection. Ask the user which
platform it is. If you don't know how it does each of these, ask or check its docs; don't
guess.

Set these in the platform (samples in [.env.example](.env.example)). You can set up the
platform and fill in `MSGMCP_URL` and `MSGMCP_AGENT_HOSTNAME`; the user pastes in the two
secrets:

| Variable | Value |
| --- | --- |
| `MSGMCP_URL` | `https://<mac>/mcp` |
| `TS_AUTHKEY` | the key from step 9 (secret) |
| `MSGMCP_TOKEN` | the gateway token (secret); `pbcopy < "$HOME/Library/Application Support/MSGMCP/gateway.token"` copies it without printing it |
| `MSGMCP_AGENT_HOSTNAME` | optional: letters, digits and hyphens, such as `grok-agent`; names the sandbox on the tailnet and in the audit log (default `msgmcp-agent`) |

### 11. Give the agent its instruction

Put this in the agent's chat or saved instructions, with the repository it can read
(`git remote get-url origin` shows this checkout's):

> You can read my iMessage, SMS, WhatsApp, Signal and Telegram history, and send iMessages once I approve
> a draft, through msgmcp. At the start of every session, fetch the `agent/` folder of
> github.com/jane-doe/msgmcp fresh and follow `agent/README.md`. If any setup check
> fails, stop and tell me.

The agent then sets itself up from [agent/README.md](agent/README.md) and reads the tool
list from the server; nothing about the tools needs pasting.

### 12. First session

Start a session on the platform and ask the agent to set up msgmcp. **Done
when** it reports "All checks passed." On the Mac, its calls show in the audit log:

```sh
grep -c '"tag:agent"' ~/Library/Logs/msgmcp/audit.log
```

A FAIL from the agent points back to a step:

| The agent reports | Go to |
| --- | --- |
| a variable is not set, contains whitespace, or is malformed | step 10 |
| could not find a Tailscale download, or `tailscale up` fails | step 10 (outbound access) or step 9 (expired key) |
| this node is not tagged `tag:agent` | steps 8 (`tagOwners`) and 9 (the key's tag) |
| expected HTTP 401 without a token, got `000` | the Mac is asleep or offline, or steps 6 and 8 |
| tools/list is missing a tool | step 10 (`MSGMCP_TOKEN`), or step 7 (a 403 means `--allow-host`) |
| port 7000 on the Mac answered | step 8: a rule still gives `tag:agent` more than port 443 |
| list_conversations did not return data | step 3 |

## Later changes

- **Turn sending on or off:** step 7, with the full set of options.
- **A new auth key:** step 9, then replace `TS_AUTHKEY` in the platform.
- **Rotate the gateway token:** delete `~/Library/Application Support/MSGMCP/gateway.token`,
  run `launchctl kickstart -k gui/$(id -u)/local.msgmcp.service`, then replace
  `MSGMCP_TOKEN` in every agent platform.
- **After code changes:** `python3 scripts/reinstall.py` keeps the certificate, stores and
  options.
- **Serve another store:** `python3 scripts/reinstall.py --source imessage --source signal`,
  listing every store to serve.
- **Remove it all:** `python3 scripts/install_service.py --uninstall`, then
  `tailscale serve reset` (which removes everything `tailscale serve` publishes) and the
  policy rules from step 8.
