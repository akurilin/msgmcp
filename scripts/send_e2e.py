#!/usr/bin/env python3
"""Manual MCP check. Sends one real iMessage only with --send."""
import argparse
import pathlib
import smoke

# The recipient is a real phone number, so it lives in the untracked .env file.
ENV_FILE = smoke.ROOT / '.env'
ENV_KEY = 'MSGMCP_TEST_RECIPIENT'
TEXT = 'msgmcp end-to-end test.'
TIMEOUT_SECONDS = 60


def recipient():
    if ENV_FILE.is_file():
        for line in ENV_FILE.read_text().splitlines():
            key, _, value = line.partition('=')
            if key.strip() == ENV_KEY and value.strip():
                return value.strip()
    raise SystemExit(f'Set {ENV_KEY}=+<number> in {ENV_FILE}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--send', action='store_true', required=True,
                        help=f'Send "{TEXT}" to {ENV_KEY} from .env')
    parser.add_argument('--binary', type=pathlib.Path, default=smoke.BINARY)
    args = parser.parse_args()
    to = recipient()
    smoke.BINARY = args.binary.resolve()
    smoke.TIMEOUT_SECONDS = TIMEOUT_SECONDS
    client = smoke.Client()
    try:
        # Use the real MCP request path; do not retry a send on failure.
        result = client.tool('send_message', recipient=to, text=TEXT)
        assert result == {'status': 'submitted', 'service': 'iMessage', 'recipient': to}, result
        print(f'Messages accepted submission to {to}; delivery is unverified.')
    finally:
        assert client.close() == 0
