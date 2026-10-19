# Working on msgmcp

Read [README.md](README.md) for what this is and how it fits together. This file holds
what the code doesn't tell you. [agent/README.md](agent/README.md) is a different thing:
instructions for agents that *use* msgmcp from a cloud sandbox. When the user asks to set
up msgmcp, give it remote access, or connect a cloud agent, work through
[SETUP.md](SETUP.md) with them; it is also the one place setup steps are written down.

## Commands

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build && cargo test
python3 scripts/smoke.py                         # end-to-end, against temporary databases
python3 scripts/smoke.py --binary target/release/msgmcp
python3 scripts/smoke.py --live                  # through the installed service; prints no message contents
python3 scripts/reinstall.py                     # rebuild and reinstall the LaunchAgent
```

The smoke tests run every end-to-end test against both services (socket and gateway).
Run them before every commit, and add a test for every behavior change.

## Rules

- **Reinstall after every commit to main, without asking.** The service runs only on the
  user's machine, so reinstalling is always safe, and it keeps the installed service (and
  what connected clients see) in step with `main`.
- **Reinstall only with `scripts/reinstall.py`.** It signs with the installed certificate.
  An ad hoc signature (`install_service.py` with no `--sign`) makes macOS drop the app's
  Full Disk Access, and the user has to grant it again by hand.
- **The schemas are the API docs.** Tool descriptions, input doc comments in `main.rs`, and
  result doc comments in `types.rs` become what clients see in `tools/list`. Edit them as
  documentation.
- **Keep API changes additive.** Agents in long-lived sandboxes may hold an old schema: add
  optional fields and arguments, don't rename or remove. Enumerated result fields are
  strings in the schema (`#[schemars(with = "String")]`), so a new value doesn't fail a
  client that checks results against an older one.
- **A new store is a module implementing `Backend` and an entry in `STORES`**
  (`backend.rs`), plus its fixture in `smoke.py` and its path in `install_service.py`.
- **Never send a real message in tests.** Send tests stop at validation (an unknown
  recipient, or empty text to a contact). `scripts/send_e2e.py` is the only real send and
  is run by hand, with the user's go-ahead.
- **Never log what tools return**, only what was asked (the audit log) and errors.
- **Addresses are compared only through `address_key` in `common.rs`**, and phone numbers
  are reported through `display_address` (E.164). Agents get numbers in every format, from
  users and from Contacts; the phone-format tests in `smoke.py` pin what must match.
- **iMessage queries go through `REAL_MESSAGE`** in `imessage.rs`, which drops reactions and
  events (group changes and the like). WhatsApp and Signal have the equivalent
  `MESSAGE_FILTER`; Telegram has `NAMESPACES` and the `tg_real` SQL function, which drop
  scheduled and service messages.
- **Tests pass every store explicitly.** A service reads only the stores enabled with
  `--source`, but an enabled store without its path option reads this Mac's real one.
  `stores()` in `smoke.py` passes a fixture for each, and
  `test_harness_passes_every_store_option` fails if a store is added to `backend.rs`
  without one there. Signal's and Telegram's fixtures are
  encrypted by `examples/sqlcipher_encrypt.rs`, and Telegram's key file is written by
  `examples/telegram_tempkey.rs`, which `smoke.py` builds, since Python's sqlite3 can't
  write SQLCipher. The Telegram fixture encodes Postbox values in Python, independently of
  the Rust decoder.
- **Never set SQLCipher `cipher_default_*` pragmas in the service.** They apply to every
  later connection in the process, so a Telegram setting would break Signal.
- **The services never show Keychain dialogs** (`disable_keychain_prompts`), so a remote
  call can't hang on one. `msgmcp --check-signal`, run by `reinstall.py`, is where the user
  allows the app to read "Signal Safe Storage".
- **Message data is private.** Diagnostics against the user's real databases print counts,
  never message text, names or numbers. `chat.db` needs Full Disk Access, which the app
  has but a development shell usually doesn't; test through the installed service.

## Running a service by hand

```sh
msgmcp socket --socket /tmp/x/service.sock --source imessage --database /path/chat.db --source signal --signal /path/signal-dir --contacts /path/AddressBook --audit-log /tmp/x/audit.log
msgmcp gateway --listen 127.0.0.1:0 --token-file /tmp/x/token --source imessage --database /path/chat.db ...   # prints its URL
msgmcp --socket /tmp/x/service.sock                                    # stdio adapter
msgmcp --url http://127.0.0.1:<port>/mcp --token-file /tmp/x/token      # stdio adapter
msgmcp --check-automation                                              # read-only AppleScript check
msgmcp --check-signal                                                  # Keychain grant for Signal; prints counts
```

`scripts/send_e2e.py --send` sends one real iMessage to `MSGMCP_TEST_RECIPIENT` in `.env`
(untracked; [.env.example](.env.example) lists every variable), which must be saved in Contacts; it needs the socket service or a gateway with
`--allow-send`.
