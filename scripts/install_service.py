#!/usr/bin/env python3
"""Install or remove the per-user MSGMCP app and login service."""
import argparse
import os
import pathlib
import plistlib
import shutil
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
IDENTIFIER = 'local.msgmcp.service'
HOME = pathlib.Path.home()
APP = HOME / 'Applications/msgmcp.app'
PLIST = HOME / 'Library/LaunchAgents' / (IDENTIFIER + '.plist')
STATE = HOME / 'Library/Application Support/MSGMCP'
LOGS = HOME / 'Library/Logs/msgmcp'
DOMAIN = f'gui/{os.getuid()}'
# The LaunchAgent runs one of these service commands; see `msgmcp --help`.
SERVICES = ('gateway', 'socket')
GATEWAY_ADDRESS = ('127.0.0.1', 8765)
SOCKET = STATE / 'run/service.sock'
TOKEN = STATE / 'gateway.token'


def run(*args, check=True):
    return subprocess.run(args, check=check)


def check_paths():
    # Reject replaced paths before writing or deleting our managed files.
    for path in (APP, APP / 'Contents', APP / 'Contents/MacOS',
                 APP / 'Contents/MacOS/msgmcp', APP / 'Contents/Info.plist',
                 PLIST, STATE, TOKEN, LOGS, LOGS / 'service.stdout.log', LOGS / 'service.stderr.log',
                 LOGS / 'audit.log'):
        if path.is_symlink():
            raise SystemExit(f'Refusing symlink: {path}')
        if path.exists() and path.stat().st_uid != os.getuid():
            raise SystemExit(f'Path belongs to another user: {path}')


def uninstall():
    # Remove only this app and launch job; retain logs and permission records.
    run('launchctl', 'bootout', DOMAIN, str(PLIST), check=False)
    PLIST.unlink(missing_ok=True)
    if APP.exists():
        shutil.rmtree(APP)
    print('MSGMCP app and login service removed. Logs retained.')


def service_command(service='gateway', allowed_hosts=(), allow_send=False):
    """The msgmcp arguments the LaunchAgent runs."""
    if (allowed_hosts or allow_send) and service != 'gateway':
        raise SystemExit('--allow-host and --allow-send apply only to the gateway')
    hosts = [arg for host in allowed_hosts for arg in ('--allow-host', host)]
    return [service] + hosts + (['--allow-send'] if allow_send else [])


def installed_command():
    """The msgmcp arguments of the installed LaunchAgent, or None."""
    if not PLIST.is_file():
        return None
    with PLIST.open('rb') as file:
        arguments = plistlib.load(file).get('ProgramArguments', [])
    return arguments[1:] if len(arguments) > 1 and arguments[1] in SERVICES else None


def install(binary, identity, command=('gateway',)):
    if not binary.is_file():
        raise SystemExit(f'Build the release binary first: {binary}')
    run('launchctl', 'bootout', DOMAIN, str(PLIST), check=False)
    executable = APP / 'Contents/MacOS/msgmcp'
    executable.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, executable)
    info = {
        'CFBundleIdentifier': IDENTIFIER,
        'CFBundleName': 'msgmcp',
        'CFBundleDisplayName': 'msgmcp',
        'CFBundleExecutable': 'msgmcp',
        'CFBundlePackageType': 'APPL',
        'CFBundleVersion': '1',
        'CFBundleShortVersionString': '0.1.0',
        'LSUIElement': True,
        'NSAppleEventsUsageDescription': 'msgmcp controls Messages to send text requested by the user.',
    }
    with (APP / 'Contents/Info.plist').open('wb') as file:
        plistlib.dump(info, file)
    # A certificate identity gives a stable designated requirement, so macOS privacy
    # grants survive rebuilds; an ad hoc ('-') signature changes with every build.
    run('codesign', '--force', '--sign', identity, '--identifier', IDENTIFIER, str(APP))
    run('/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister', str(APP))
    for directory in (STATE, LOGS):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        directory.chmod(0o700)
    PLIST.parent.mkdir(parents=True, exist_ok=True)
    job = {
        'Label': IDENTIFIER,
        'ProgramArguments': [str(executable), *command],
        'AssociatedBundleIdentifiers': [IDENTIFIER],
        'LimitLoadToSessionType': 'Aqua',
        'RunAtLoad': True,
        'KeepAlive': True,
        'Umask': 0o077,
        'StandardOutPath': str(LOGS / 'service.stdout.log'),
        'StandardErrorPath': str(LOGS / 'service.stderr.log'),
    }
    with PLIST.open('wb') as file:
        plistlib.dump(job, file)
    run('launchctl', 'bootstrap', DOMAIN, str(PLIST))
    print(f'Installed {APP} running `msgmcp {" ".join(command)}`\nMCP clients run: {executable}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--uninstall', action='store_true')
    parser.add_argument('--binary', type=pathlib.Path, default=ROOT / 'target/release/msgmcp')
    parser.add_argument('--sign', default='-', help='codesign identity (default: ad hoc)')
    parser.add_argument('--service', choices=SERVICES, default='gateway',
                        help='service the LaunchAgent runs (default: gateway)')
    parser.add_argument('--allow-host', action='append', default=[],
                        help='extra Host name the gateway accepts, such as a tailnet name; repeatable')
    parser.add_argument('--allow-send', action='store_true',
                        help='let gateway clients use send_message (Contacts recipients only)')
    args = parser.parse_args()
    check_paths()
    if args.uninstall:
        uninstall()
    else:
        install(args.binary.resolve(), args.sign,
                service_command(args.service, args.allow_host, args.allow_send))
