# msgmcp.sh: reach a msgmcp gateway over Tailscale from a Linux sandbox. See agent/README.md.
# Use: source agent/msgmcp.sh && msgmcp_connect && msgmcp_test
# Needs in the environment (see .env.example):
#   TS_AUTHKEY    Tailscale auth key tagged tag:agent. Secret: never print it.
#   MSGMCP_TOKEN  gateway bearer token. Secret: never print it.
#   MSGMCP_URL    the gateway's tailnet address, such as https://my-mac.tail4f2e1c.ts.net/mcp
#   MSGMCP_AGENT_HOSTNAME  optional: this sandbox's name on the tailnet (default msgmcp-agent)
TS_DIR="$HOME/.tailscale"
TS_SOCK="$TS_DIR/tailscaled.sock"
TS_PROXY=socks5h://localhost:1055

ts() { "$TS_DIR/tailscale" --socket="$TS_SOCK" "$@"; }

# Check the variables above. Prints each problem to stderr and returns 1 if there are any.
# Shows MSGMCP_URL's value when it is wrong, never a secret's.
msgmcp_check_env() {
  local problems=0 example=https://my-mac.tail4f2e1c.ts.net/mcp
  problem() { echo "$*" >&2; problems=1; }
  # A value pasted with a trailing newline or space passes a glance but breaks every request.
  case "$MSGMCP_URL" in
    '') problem "MSGMCP_URL is not set; set it to the gateway's tailnet address, such as $example" ;;
    *[[:space:]]*) problem "MSGMCP_URL contains whitespace (a stray space or newline from pasting?)" ;;
    https://?*/mcp) ;;
    *) problem "MSGMCP_URL is \"$MSGMCP_URL\"; it must be https://<mac>.<tailnet>.ts.net/mcp, such as $example" ;;
  esac
  case "$TS_AUTHKEY" in
    '') problem "TS_AUTHKEY is not set; set it to a reusable, ephemeral Tailscale auth key tagged tag:agent" ;;
    *[[:space:]]*) problem "TS_AUTHKEY contains whitespace (a stray space or newline from pasting?)" ;;
    tskey-?*) ;;
    *) problem "TS_AUTHKEY does not start with tskey-, so it is not a Tailscale auth key" ;;
  esac
  case "$MSGMCP_TOKEN" in
    '') problem "MSGMCP_TOKEN is not set; set it to the gateway token from the Mac's gateway.token file" ;;
    *[[:space:]]*) problem "MSGMCP_TOKEN contains whitespace (a stray space or newline from pasting, or a \"Bearer \" prefix?)" ;;
  esac
  case "$MSGMCP_AGENT_HOSTNAME" in
    *[!A-Za-z0-9-]*) problem "MSGMCP_AGENT_HOSTNAME is \"$MSGMCP_AGENT_HOSTNAME\"; use only letters, digits and hyphens, or leave it unset" ;;
  esac
  return $problems
}

# Join the tailnet; safe to rerun. Installs Tailscale into ~/.tailscale and needs no root.
msgmcp_connect() {
  msgmcp_check_env || return 1
  mkdir -p "$TS_DIR"
  if [ ! -x "$TS_DIR/tailscaled" ]; then
    local arch tarball
    case "$(uname -m)" in
      x86_64) arch=amd64 ;;
      aarch64|arm64) arch=arm64 ;;
      *) echo "unsupported CPU: $(uname -m)" >&2; return 1 ;;
    esac
    tarball=$(curl -fsSL 'https://pkgs.tailscale.com/stable/?mode=json' | grep -o "tailscale_[0-9.]*_${arch}\.tgz" | head -1)
    [ -n "$tarball" ] || { echo "could not find a Tailscale download" >&2; return 1; }
    curl -fsSL "https://pkgs.tailscale.com/stable/$tarball" | tar -xz -C "$TS_DIR" --strip-components=1 || return 1
  fi
  if ! ts status >/dev/null 2>&1; then
    if ! ps -eo args 2>/dev/null | grep -q "[t]ailscaled --tun"; then
      nohup "$TS_DIR/tailscaled" --tun=userspace-networking --state=mem: --socket="$TS_SOCK" \
        --socks5-server=localhost:1055 >"$TS_DIR/tailscaled.log" 2>&1 &
      for _ in $(seq 40); do [ -S "$TS_SOCK" ] && break; sleep 0.25; done
    fi
    # Ask for tag:agent explicitly, so the node is restricted even if the key isn't tagged.
    ts up --auth-key="$TS_AUTHKEY" --hostname="${MSGMCP_AGENT_HOSTNAME:-msgmcp-agent}" --advertise-tags=tag:agent \
      --accept-dns=false --timeout=60s || return 1
  fi
  echo "On the tailnet as $(ts ip -4)"
}

# POST one JSON-RPC message to the gateway and print the JSON response.
msgmcp_rpc() {
  msgmcp_check_env || return 1
  curl -sS --max-time 60 --proxy "$TS_PROXY" "$MSGMCP_URL" \
    -H "Authorization: Bearer $MSGMCP_TOKEN" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -d "$1"
}

# POST without the token and print the HTTP status, which should be 401. The header tells
# the gateway this is a check, so its audit log shows a self-test rather than a stranger.
msgmcp_probe() {
  curl -s -o /dev/null -w '%{http_code}' --max-time 30 --proxy "$TS_PROXY" "$MSGMCP_URL" \
    -H 'X-Msgmcp-Probe: auth-check' -H 'Content-Type: application/json' -d '{}'
}

# Call one tool: msgmcp TOOL '{"arg": "value"}'. The answer is in .result.structuredContent;
# .result.isError true means the tool failed and .result.content[0].text says why.
msgmcp() {
  local args=${2:-'{}'}
  msgmcp_rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$args}}"
}

# Check the whole path. Prints PASS/FAIL lines and no message contents.
msgmcp_test() {
  local failed=0 out code host problem
  pass() { echo "PASS $*"; }
  fail() { echo "FAIL $*"; failed=1; }
  if ! out=$(msgmcp_check_env 2>&1); then
    while IFS= read -r problem; do fail "$problem"; done <<<"$out"
    return 1
  fi
  host=${MSGMCP_URL#https://}
  host=${host%%/*}
  if ts status >/dev/null 2>&1; then pass "on the tailnet as $(ts ip -4)"; else fail "not on the tailnet; run msgmcp_connect"; return 1; fi
  # Untagged, this node would count as one of the user's own devices, with full access.
  if ts status --json --peers=false | grep -q '"tag:agent"'; then
    pass "this node is tagged tag:agent"
  else
    fail "this node is not tagged tag:agent, so the tailnet policy does not restrict it"
  fi
  code=$(msgmcp_probe)
  if [ "$code" = 401 ]; then pass "gateway reachable; it refuses requests without the token"; else fail "expected HTTP 401 without a token, got $code"; fi
  out=$(msgmcp_rpc '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
  for tool in list_conversations search_messages read_conversation get_message_context; do
    grep -q "\"$tool\"" <<<"$out" || fail "tools/list is missing $tool (bad MSGMCP_TOKEN?)"
  done
  if grep -q '"send_message"' <<<"$out"; then pass "tools/list shows the read tools and send_message"; else pass "tools/list shows the read tools (sending is off)"; fi
  out=$(msgmcp list_conversations '{"limit":1}')
  if grep -q '"conversation_id"' <<<"$out" && ! grep -q '"isError":true' <<<"$out"; then
    pass "list_conversations returned a conversation"
  else
    fail "list_conversations did not return data"
  fi
  # The Mac's AirPlay receiver answers HTTP on port 7000. The policy allows this node only
  # port 443, so that request must get no HTTP response at all (code 000).
  local mac
  # Look up the short name: a sandbox's own DNS may answer for the full *.ts.net name.
  mac=$(ts ip -4 "${host%%.*}" 2>/dev/null)
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 --proxy "$TS_PROXY" "http://$mac:7000/")
  if [ -z "$mac" ]; then
    fail "could not find $host on the tailnet"
  elif [ "$code" = 000 ]; then
    pass "other ports on the Mac are blocked"
  else
    fail "port 7000 on the Mac answered HTTP $code; the tailnet policy is not restricting this node"
  fi
  [ "$failed" = 0 ] && echo "All checks passed."
  return $failed
}
