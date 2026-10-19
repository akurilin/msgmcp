#!/usr/bin/env python3
"""Build and (re)install msgmcp without losing macOS privacy grants.

macOS ties Full Disk Access to the app's signature, so this always signs with the same
certificate and refuses an ad hoc signature. The identity comes from --sign, else
MSGMCP_SIGN_IDENTITY in .env, else the certificate the installed app is already signed
with. After installing it checks the signature and that the service can read both stores.
The LaunchAgent keeps running the installed service command (the gateway on a first install)
unless --service, --allow-host or --allow-send is given, which replaces it.
"""
import argparse
import socket
import subprocess
import time

import install_service
import smoke

ENV_FILE = install_service.ROOT / '.env'
ENV_KEY = 'MSGMCP_SIGN_IDENTITY'
STARTUP_SECONDS = 10


def env_identity():
    if ENV_FILE.is_file():
        for line in ENV_FILE.read_text().splitlines():
            key, _, value = line.partition('=')
            if key.strip() == ENV_KEY and value.strip():
                return value.strip().strip('"\'')
    return None


def installed_identity():
    """The leaf certificate name of the installed app, or None if unsigned or ad hoc."""
    if not install_service.APP.exists():
        return None
    out = subprocess.run(['codesign', '-dvv', str(install_service.APP)],
                         capture_output=True, text=True).stderr
    return next((line.split('=', 1)[1] for line in out.splitlines()
                 if line.startswith('Authority=')), None)


def available_identities():
    out = subprocess.run(['security', 'find-identity', '-v', '-p', 'codesigning'],
                         capture_output=True, text=True, check=True).stdout
    return [line.split('"')[1] for line in out.splitlines() if line.count('"') >= 2]


def choose_identity(requested):
    identity = requested or env_identity() or installed_identity()
    available = available_identities()
    if not identity:
        raise SystemExit(
            'No signing identity. Pass --sign once (later runs reuse the installed app\'s), '
            f'or set {ENV_KEY} in {ENV_FILE}. Available:\n  ' + '\n  '.join(available or ['(none)']))
    if identity not in available:
        raise SystemExit(f'Signing identity not in your keychain: {identity}\nAvailable:\n  '
                         + '\n  '.join(available or ['(none)']))
    return identity


def wait_for_service(service):
    deadline = time.monotonic() + STARTUP_SECONDS
    while True:
        try:
            if service == 'socket':
                with socket.socket(socket.AF_UNIX) as client:
                    client.connect(str(install_service.SOCKET))
            else:
                socket.create_connection(install_service.GATEWAY_ADDRESS, timeout=1).close()
            return
        except OSError:
            if time.monotonic() >= deadline:
                raise SystemExit(f'Service did not start; see {install_service.LOGS}/service.stderr.log')
            time.sleep(0.2)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--sign', help='codesign identity; needed only for the first install')
    parser.add_argument('--service', choices=install_service.SERVICES,
                        help='service the LaunchAgent runs (default: the installed one, else gateway)')
    parser.add_argument('--allow-host', action='append', default=[],
                        help='extra Host name the gateway accepts, such as a tailnet name; repeatable')
    parser.add_argument('--allow-send', action='store_true',
                        help='let gateway clients use send_message (Contacts recipients only)')
    args = parser.parse_args()
    identity = choose_identity(args.sign)
    print(f'Signing with: {identity}')
    subprocess.run(['cargo', 'build', '--release', '--locked'], cwd=install_service.ROOT, check=True)
    install_service.check_paths()
    if args.service or args.allow_host or args.allow_send:
        command = install_service.service_command(args.service or 'gateway', args.allow_host,
                                                  args.allow_send)
    else:
        command = install_service.installed_command() or install_service.service_command()
    install_service.install(install_service.ROOT / 'target/release/msgmcp', identity, command)
    if installed_identity() is None:
        raise SystemExit(f'{install_service.APP} is not certificate-signed')
    wait_for_service(command[0])
    smoke.BINARY = install_service.APP / 'Contents/MacOS/msgmcp'
    if smoke.SIGNAL_DATABASE.exists():
        # The service never shows Keychain dialogs, so ask here, where the user is. After
        # Always Allow this prints counts without asking.
        print('Checking Signal; if macOS asks about "Signal Safe Storage", choose Always Allow.')
        if subprocess.run([str(smoke.BINARY), '--check-signal']).returncode != 0:
            raise SystemExit('msgmcp cannot read Signal. Run this again and choose Always Allow, '
                             'or see SETUP.md, step 3.')
    try:
        smoke.live(command[0])
    except AssertionError as error:
        if 'Full Disk Access' not in str(error):
            raise
        raise SystemExit(
            'The service cannot read the message stores. In System Settings → Privacy & Security '
            '→ Full Disk Access, remove msgmcp with "–", add ~/Applications/msgmcp.app with "+", '
            'and run this again. This is needed once after switching signing identities.')
    print('Reinstalled; restart MCP clients to pick up tool changes.')
