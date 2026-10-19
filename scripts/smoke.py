#!/usr/bin/env python3
"""Test the real server through MCP stdio with isolated SQLite databases.

Every end-to-end test runs twice: against the socket service and against the gateway.
"""
import argparse
import datetime
import json
import os
import pathlib
import select
import socket
import sqlite3
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
BINARY = ROOT / 'target/debug/msgmcp'
TIMEOUT_SECONDS = 10
SOCKET = pathlib.Path.home() / 'Library/Application Support/MSGMCP/run/service.sock'
GATEWAY_URL = 'http://127.0.0.1:8765/mcp'
SCAN_LIMIT = 10_000
APPLE_EPOCH = 978_307_200
NANOS_PER_SECOND = 1_000_000_000
DATE = 800_000_000 * NANOS_PER_SECOND
WA_DATE = 800_000_000
# WhatsApp stores a far-future ZLASTMESSAGEDATE for pinned chats; msgmcp must ignore it.
PINNED_DATE = WA_DATE + 14_000 * 31_556_952
PROTOCOL_VERSION = '2025-11-25'
MARCO = {'address': '+393331234567', 'jid': '393331234567@s.whatsapp.net',
         'name': 'Marco Rossi', 'ambiguous': False}
PRIYA = {'address': '+447700900123', 'jid': '222@lid', 'name': 'Priya', 'ambiguous': False}
ME = {'address': None, 'name': 'me', 'ambiguous': False}


def timestamp(date):
    seconds = APPLE_EPOCH + date // NANOS_PER_SECOND
    return datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc).isoformat()


def wa_timestamp(seconds):
    return timestamp(int(seconds * NANOS_PER_SECOND))


def varint(value):
    out = b''
    while True:
        byte, value = value & 0x7F, value >> 7
        out += bytes([byte | (0x80 if value else 0)])
        if not value:
            return out


def pb_bytes(field, data):
    return varint(field << 3 | 2) + varint(len(data)) + data


def pb_int(field, value):
    return varint(field << 3) + varint(value)


def reaction(emoji, ts, sender=None):
    entry = (pb_bytes(2, sender.encode()) if sender else b'') + pb_bytes(3, emoji.encode()) + pb_int(4, ts)
    return pb_bytes(7, pb_bytes(1, entry))


def whatsapp_fixture(directory):
    """Mirror the columns msgmcp reads from WhatsApp Desktop's Core Data stores."""
    directory.mkdir()
    db = sqlite3.connect(directory / 'ChatStorage.sqlite')
    db.executescript('''
        CREATE TABLE ZWACHATSESSION(Z_PK INTEGER PRIMARY KEY, ZCONTACTJID TEXT, ZCONTACTIDENTIFIER TEXT, ZPARTNERNAME TEXT, ZSESSIONTYPE INTEGER, ZREMOVED INTEGER, ZLASTMESSAGEDATE TIMESTAMP, ZUNREADCOUNT INTEGER, ZARCHIVED INTEGER);
        CREATE TABLE ZWAMESSAGE(Z_PK INTEGER PRIMARY KEY, ZSTANZAID TEXT, ZCHATSESSION INTEGER, ZMESSAGEDATE TIMESTAMP, ZISFROMME INTEGER, ZFROMJID TEXT, ZGROUPMEMBER INTEGER, ZTEXT TEXT, ZMESSAGETYPE INTEGER, ZMEDIAITEM INTEGER, ZMESSAGEINFO INTEGER);
        CREATE TABLE ZWAGROUPMEMBER(Z_PK INTEGER PRIMARY KEY, ZCHATSESSION INTEGER, ZMEMBERJID TEXT, ZCONTACTNAME TEXT, ZISACTIVE INTEGER);
        CREATE TABLE ZWAMEDIAITEM(Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT, ZMETADATA BLOB);
        CREATE TABLE ZWAMESSAGEINFO(Z_PK INTEGER PRIMARY KEY, ZMESSAGE INTEGER, ZRECEIPTINFO BLOB);
        CREATE TABLE ZWAPROFILEPUSHNAME(ZJID TEXT, ZPUSHNAME TEXT);
        INSERT INTO ZWACHATSESSION VALUES
            (1, '393331234567@s.whatsapp.net', '111@lid', 'Marco WA', 0, 0, ?, 2, 1),
            (2, '222@lid', NULL, NULL, 0, 0, NULL, -1, 0),
            (3, '120363@g.us', NULL, 'Trip planning', 1, 0, 800000006, 0, NULL),
            (4, '555@status', NULL, NULL, 3, 0, NULL, 4, 0),
            (5, '999@s.whatsapp.net', NULL, 'Gone', 0, 1, NULL, 7, 0);
        INSERT INTO ZWAGROUPMEMBER VALUES
            (1, 3, '393331234567@s.whatsapp.net', 'Marco WA', 1),
            (2, 3, '222@lid', NULL, 1),
            (3, 3, '333@lid', 'Unknown Member', 1),
            (4, 3, '444@lid', 'Former Member', 0);
        INSERT INTO ZWAPROFILEPUSHNAME VALUES ('222@lid', 'Priya');
    '''.replace('?', 'NULL'))
    db.execute('UPDATE ZWACHATSESSION SET ZLASTMESSAGEDATE=? WHERE Z_PK=1', (PINNED_DATE,))
    db.executemany('INSERT INTO ZWAMEDIAITEM VALUES(?,?,?)', [
        (1, None, pb_bytes(5, b'wa-1') + pb_bytes(6, b'393331234567@s.whatsapp.net')),
        (2, 'Pizza photo', None),
        (3, 'secret original text', None)])
    db.execute('INSERT INTO ZWAMESSAGEINFO VALUES(1, 2, ?)', (
        reaction('😂', 500, '222@lid') + reaction('👍', 1000) +
        reaction('❤️', 2000, '393331234567@s.whatsapp.net') + reaction('', 3000, '222@lid'),))
    db.executemany('INSERT INTO ZWAMESSAGE VALUES(?,?,?,?,?,?,?,?,?,?,?)', [
        (1, 'wa-1', 1, WA_DATE, 0, '393331234567@s.whatsapp.net', None, 'Pizza tonight?', 0, None, None),
        (2, 'wa-2', 1, WA_DATE, 1, None, None, 'Yes, pizza works', 0, 1, 1),
        (3, 'wa-3', 1, WA_DATE + 1.5, 0, '393331234567@s.whatsapp.net', None, None, 1, 2, None),
        (4, 'wa-4', 1, WA_DATE + 2, 0, '393331234567@s.whatsapp.net', None, None, 14, 3, None),
        (5, 'wa-5', 1, WA_DATE + 3, 0, '393331234567@s.whatsapp.net', None, None, 10, None, None),
        (6, 'wa-6', 3, WA_DATE + 4, 0, '120363@g.us', 2, 'Flights booked', 0, None, None),
        (7, 'wa-7', 3, WA_DATE + 5, 0, '120363@g.us', None, 'Group renamed', 6, None, None),
        (8, 'wa-8', 3, WA_DATE + 6, 0, '120363@g.us', 3, 'Who has the tickets?', 0, None, None),
        (9, 'wa-9', 4, WA_DATE + 7, 0, '555@status', None, 'Status pizza', 0, None, None),
        (10, 'wa-10', 5, WA_DATE + 8, 0, '999@s.whatsapp.net', None, 'Removed pizza', 0, None, None)])
    db.commit()
    db.close()
    with sqlite3.connect(directory / 'LID.sqlite') as lid:
        lid.executescript('''
            CREATE TABLE ZWAZACCOUNT(ZIDENTIFIER TEXT, ZPHONENUMBER TEXT);
            INSERT INTO ZWAZACCOUNT VALUES ('222@lid', '447700900123');
        ''')
    lid.close()


TYPE_CHECKS = {
    'object': lambda v: isinstance(v, dict),
    'array': lambda v: isinstance(v, list),
    'string': lambda v: isinstance(v, str),
    'boolean': lambda v: isinstance(v, bool),
    'null': lambda v: v is None,
    'integer': lambda v: isinstance(v, int) and not isinstance(v, bool),
    'number': lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
}


def validate(value, schema, defs, path='$'):
    """Check a value against the JSON Schema subset schemars emits. Objects are closed:
    a field the schema does not declare fails, so results cannot carry undocumented fields."""
    if '$ref' in schema:
        return validate(value, defs[schema['$ref'].rsplit('/', 1)[1]], defs, path)
    for key in ('anyOf', 'oneOf'):
        if key in schema:
            matches = 0
            for branch in schema[key]:
                try:
                    validate(value, branch, defs, path)
                    matches += 1
                except AssertionError:
                    pass
            if matches == 0 or (key == 'oneOf' and matches > 1):
                raise AssertionError(f'{path}: {value!r} matches {matches} {key} branches')
            return
    types = schema.get('type')
    types = [types] if isinstance(types, str) else types
    if types is not None and not any(TYPE_CHECKS[t](value) for t in types):
        raise AssertionError(f'{path}: {value!r} is not {types}')
    if 'const' in schema and value != schema['const']:
        raise AssertionError(f'{path}: {value!r} != {schema["const"]!r}')
    if 'enum' in schema and value not in schema['enum']:
        raise AssertionError(f'{path}: {value!r} not in {schema["enum"]}')
    if isinstance(value, dict) and 'properties' in schema:
        missing = set(schema.get('required', [])) - value.keys()
        extra = value.keys() - schema['properties'].keys()
        if missing or extra:
            raise AssertionError(f'{path}: missing {sorted(missing)}, undeclared {sorted(extra)}')
        for key, item in value.items():
            validate(item, schema['properties'][key], defs, f'{path}.{key}')
    if isinstance(value, list) and 'items' in schema:
        for index, item in enumerate(value):
            validate(item, schema['items'], defs, f'{path}[{index}]')


class Client:
    def __init__(self, *args):
        self.stderr = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [str(BINARY), *map(str, args)], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=self.stderr)
        self.request_id = 0
        self.buffer = b''
        try:
            self.initialized = self.request('initialize', {
                'protocolVersion': PROTOCOL_VERSION, 'capabilities': {},
                'clientInfo': {'name': 'e2e', 'version': '1'}})
            self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
            self.schemas = {t['name']: t['outputSchema']
                            for t in self.request('tools/list', {})['tools']}
        except BaseException:
            self.close()
            raise

    def send(self, value):
        self.process.stdin.write((json.dumps(value) + '\n').encode())
        self.process.stdin.flush()

    def request(self, method, params):
        self.request_id += 1
        self.send({'jsonrpc': '2.0', 'id': self.request_id,
                   'method': method, 'params': params})
        deadline = time.monotonic() + TIMEOUT_SECONDS
        # Read raw bytes so Python buffering cannot hide a response from select.
        while True:
            if b'\n' not in self.buffer:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select(
                        [self.process.stdout], [], [], remaining)[0]:
                    raise AssertionError('MCP response timeout')
                chunk = os.read(self.process.stdout.fileno(), 65536)
                if not chunk:
                    self.stderr.seek(0)
                    raise AssertionError('MCP exited: ' + self.stderr.read().decode())
                self.buffer += chunk
                continue
            line, self.buffer = self.buffer.split(b'\n', 1)
            response = json.loads(line)
            if response.get('id') == self.request_id:
                if 'error' in response:
                    raise AssertionError(response)
                return response['result']
            if time.monotonic() >= deadline:
                raise AssertionError('MCP response timeout')

    def tool(self, name, **arguments):
        result = self.request('tools/call', {'name': name, 'arguments': arguments})
        if result.get('isError'):
            raise AssertionError(result)
        value = result['structuredContent']
        if json.loads(result['content'][0]['text']) != value:
            raise AssertionError('Text content differs from structuredContent')
        schema = self.schemas[name]
        validate(value, schema, schema.get('$defs', {}))
        return value

    def pages(self, name, field, **arguments):
        items, cursors = [], set()
        # Bound continuation so a repeated cursor fails instead of hanging.
        for _ in range(20):
            page = self.tool(name, **arguments)
            items.extend(page[field])
            if page['complete']:
                if page['next_cursor'] is not None:
                    raise AssertionError('Complete page has a cursor')
                return items
            cursor = page['next_cursor']
            if cursor is None or cursor in cursors:
                raise AssertionError('Missing or repeated cursor')
            cursors.add(cursor)
            arguments['cursor'] = cursor
        raise AssertionError('Too many pages')

    def close(self):
        # Always reap the process, including after an assertion or timeout.
        try:
            if not self.process.stdin.closed:
                self.process.stdin.close()
            try:
                return self.process.wait(timeout=TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=TIMEOUT_SECONDS)
                raise AssertionError('Server did not exit after stdin closed')
        finally:
            self.process.stdout.close()
            self.stderr.close()


def im(message, chat):
    return (f'imessage:{message}', f'imessage:{chat}')


def wa(message, chat):
    return (f'whatsapp:{message}', f'whatsapp:{chat}')


class EndToEnd(unittest.TestCase):
    """Runs against the socket service; GatewayEndToEnd repeats it against the gateway."""
    TRANSPORT = 'socket'

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = pathlib.Path(directory.name)
        self.path = self.root / 'chat.db'
        self.db = sqlite3.connect(self.path)
        self.addCleanup(self.db.close)
        self.db.executescript('''
            PRAGMA journal_mode=WAL;
            CREATE TABLE chat(chat_identifier TEXT, display_name TEXT, is_archived INTEGER DEFAULT 0, is_filtered INTEGER DEFAULT 0);
            CREATE TABLE handle(id TEXT);
            CREATE TABLE chat_handle_join(chat_id INTEGER, handle_id INTEGER);
            CREATE TABLE chat_message_join(chat_id INTEGER, message_id INTEGER);
            CREATE TABLE message(guid TEXT,date INTEGER,is_from_me INTEGER,handle_id INTEGER,text TEXT,attributedBody BLOB,associated_message_type INTEGER,thread_originator_guid TEXT,is_read INTEGER DEFAULT 1,item_type INTEGER DEFAULT 0);
            INSERT INTO chat(chat_identifier,display_name,is_archived,is_filtered) VALUES ('group-a','Dinner',0,2),('group-b','Other',1,0);
            INSERT INTO handle VALUES ('+15551234567'),('sarah@example.com');
            INSERT INTO chat_handle_join VALUES (1,1),(1,2),(2,2);
        ''')
        fixture = (ROOT / 'scripts/fixtures/attributed_body').read_bytes()
        self.db.executemany('INSERT INTO message(guid,date,is_from_me,handle_id,text,attributedBody,associated_message_type,thread_originator_guid) VALUES(?,?,?,?,?,?,?,?)', [
            ('a', DATE, 0, 2, 'Dinner at six', None, 0, None),
            ('b', DATE, 1, 2, 'Dinner works', None, 0, 'a'),
            ('c', DATE + NANOS_PER_SECOND, 0, 1, None, fixture, 0, None),
            ('d', DATE + 2 * NANOS_PER_SECOND, 0, 1, None, b'bad', 0, None),
            ('e', DATE + 3 * NANOS_PER_SECOND, 0, 1, 'Reaction', None, 2001, 'a')])
        # Unread: c, and the reaction e, which must not count.
        self.db.execute("UPDATE message SET is_read=0 WHERE guid IN ('c','e')")
        self.db.executemany('INSERT INTO chat_message_join VALUES(?,?)',
                            [(1, i) for i in range(1, 6)] + [(2, 1)])
        self.db.commit()
        contacts = self.root / 'contacts' / 'Sources' / 'test'
        contacts.mkdir(parents=True)
        with sqlite3.connect(contacts / 'AddressBook-v22.abcddb') as cn:
            cn.executescript('''
                CREATE TABLE ZABCDRECORD(Z_PK INTEGER,ZFIRSTNAME TEXT,ZLASTNAME TEXT,ZNICKNAME TEXT,ZORGANIZATION TEXT);
                CREATE TABLE ZABCDPHONENUMBER(ZFULLNUMBER TEXT,ZOWNER INTEGER);
                CREATE TABLE ZABCDEMAILADDRESS(ZADDRESS TEXT,ZOWNER INTEGER);
                INSERT INTO ZABCDRECORD VALUES(1,'Sarah','Test',NULL,NULL);
                INSERT INTO ZABCDEMAILADDRESS VALUES('sarah@example.com',1);
                INSERT INTO ZABCDPHONENUMBER VALUES('(555) 010-2030',1);
                INSERT INTO ZABCDRECORD VALUES(3,'Marco','Rossi',NULL,NULL);
                INSERT INTO ZABCDPHONENUMBER VALUES('+39 333 123 4567',3);
            ''')
        cn.close()
        self.whatsapp = self.root / 'whatsapp'
        whatsapp_fixture(self.whatsapp)
        self.client = self.start_client(self.root / 'contacts')

    def stores(self, contacts, whatsapp=None):
        self.audit_log = self.root / 'logs' / 'audit.log'
        return ['--database', str(self.path), '--contacts', str(contacts),
                '--whatsapp', str(whatsapp or self.whatsapp), '--audit-log', str(self.audit_log)]

    def audit(self):
        return [json.loads(line) for line in self.audit_log.read_text().splitlines()]

    def start_service(self, *args):
        """Start a service; returns it and a function reading its log so far."""
        log = tempfile.TemporaryFile()
        self.addCleanup(log.close)
        process = subprocess.Popen([str(BINARY), *map(str, args)], stdout=log, stderr=log)
        def read():
            log.seek(0)
            return log.read().decode()
        return process, read

    def wait_for(self, process, read, ready):
        deadline = time.monotonic() + TIMEOUT_SECONDS
        while not ready():
            if process.poll() is not None or time.monotonic() >= deadline:
                self.fail('Service startup failed: ' + read())
            time.sleep(0.01)

    def start_client(self, contacts, whatsapp=None):
        run = self.root / ('run-' + str(len(list(self.root.glob('run-*')))))
        socket = run / 'service.sock'
        process, read = self.start_service('socket', '--socket', socket,
                                           *self.stores(contacts, whatsapp))
        def stop():
            process.terminate()
            process.wait(timeout=TIMEOUT_SECONDS)
            self.assertFalse(socket.exists())
        self.addCleanup(stop)
        # The service binds, then restricts the socket (its directory is already private).
        self.wait_for(process, read, lambda: socket.exists() and socket.stat().st_mode & 0o777 == 0o600)
        self.assertEqual(socket.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual(socket.stat().st_mode & 0o777, 0o600)
        self.socket = socket
        self.endpoint = ['--socket', socket]
        self.service_process = process
        client = Client(*self.endpoint)
        self.addCleanup(client.close)
        return client

    def probe(self):
        with socket.socket(socket.AF_UNIX) as probe:
            probe.connect(str(self.socket))

    def ids(self, messages):
        return [(m['message_id'], m['conversation_id']) for m in messages]

    def test_whatsapp_conversations_and_identities(self):
        chats = self.client.pages('list_conversations', 'conversations', source='whatsapp', limit=1)
        self.assertEqual([(c['conversation_id'], c['chat_type'], c['name']) for c in chats], [
            ('whatsapp:3', 'group', 'Trip planning'), ('whatsapp:1', 'direct', 'Marco WA'),
            ('whatsapp:2', 'direct', None)])
        group, marco, priya = chats
        self.assertEqual(marco['participants'], [MARCO])
        self.assertEqual(priya['participants'], [PRIYA])
        self.assertEqual(group['participant_count'], 3)
        self.assertEqual(group['participants'][2], {
            'address': '333@lid', 'jid': '333@lid', 'name': 'Unknown Member', 'ambiguous': False})
        # Newest real message: the group event (+5) and security notice (+3) don't count,
        # and the pinned chat's far-future ZLASTMESSAGEDATE is ignored.
        self.assertEqual(group['latest_message_at'], wa_timestamp(WA_DATE + 6))
        self.assertEqual(marco['latest_message_at'], wa_timestamp(WA_DATE + 2))
        self.assertIsNone(priya['latest_message_at'])
        found = self.client.tool('list_conversations', query='priya')['conversations']
        self.assertEqual([c['conversation_id'] for c in found], ['whatsapp:3', 'whatsapp:2'])

    def test_whatsapp_read_kinds_replies_reactions_and_context(self):
        messages = self.client.pages('read_conversation', 'messages', conversation_id='whatsapp:1', limit=1)
        self.assertEqual(self.ids(messages), [wa(1, 1), wa(2, 1), wa(3, 1), wa(4, 1)])
        self.assertEqual([m['kind'] for m in messages], ['text', 'text', 'image', 'deleted'])
        self.assertEqual(messages[0]['sender'], MARCO)
        self.assertEqual(messages[0]['sent_at'], wa_timestamp(WA_DATE))
        self.assertEqual(messages[0]['guid'], 'wa-1')
        self.assertEqual((messages[1]['sender'], messages[1]['reply_to_guid']), (ME, 'wa-1'))
        self.assertEqual(messages[1]['reactions'], [
            {'emoji': '👍', 'is_from_me': True, 'sender': ME},
            {'emoji': '❤️', 'is_from_me': False, 'sender': MARCO}])
        self.assertNotIn('reactions', messages[0])
        self.assertEqual((messages[2]['text'], messages[2]['text_status']), ('Pizza photo', 'ok'))
        self.assertEqual((messages[3]['text'], messages[3]['text_status']), (None, 'no_text'))
        group = self.client.tool('read_conversation', conversation_id='whatsapp:3')['messages']
        self.assertEqual(self.ids(group), [wa(6, 3), wa(8, 3)])
        self.assertEqual(group[0]['sender'], PRIYA)
        self.assertEqual(group[1]['sender']['name'], 'Unknown Member')
        context = self.client.tool('get_message_context', conversation_id='whatsapp:1',
                                   message_id='whatsapp:2', before=1, after=10)
        self.assertEqual(self.ids(context['messages']), self.ids(messages))

    def test_search_across_sources(self):
        self.db.execute("INSERT INTO message(guid,date,is_from_me,handle_id,text,attributedBody,associated_message_type,thread_originator_guid) VALUES('p',?,0,2,'Pizza place?',NULL,0,NULL)",
                        (DATE + 4 * NANOS_PER_SECOND,))
        self.db.execute('INSERT INTO chat_message_join VALUES(2,6)')
        self.db.commit()
        everything = self.client.pages('search_messages', 'messages', text='PIZZA', limit=1)
        self.assertEqual(self.ids(everything), [im(6, 2), wa(1, 1), wa(2, 1), wa(3, 1)])
        self.assertEqual([m['source'] for m in everything], ['imessage'] + ['whatsapp'] * 3)
        only = self.client.tool('search_messages', text='pizza', source='whatsapp')['messages']
        self.assertEqual(self.ids(only), [wa(1, 1), wa(2, 1), wa(3, 1)])
        for sender in ('Marco', '+393331234567', '393331234567', '393331234567@s.whatsapp.net'):
            with self.subTest(sender=sender):
                page = self.client.tool('search_messages', text='pizza', sender=sender)
                self.assertEqual(self.ids(page['messages']), [wa(1, 1), wa(3, 1)])
        mine = self.client.tool('search_messages', text='pizza', sender='me')['messages']
        self.assertEqual(self.ids(mine), [wa(2, 1)])
        later = self.client.tool('search_messages', text='pizza', source='whatsapp',
                                 after=wa_timestamp(WA_DATE + 1))['messages']
        self.assertEqual(self.ids(later), [wa(3, 1)])
        self.assertEqual(self.client.tool('search_messages', text='secret')['messages'], [])

    def test_missing_whatsapp_store_is_reported(self):
        client = self.start_client(self.root / 'contacts', self.root / 'no-whatsapp')
        chats = client.tool('list_conversations')
        self.assertEqual([c['source'] for c in chats['conversations']], ['imessage', 'imessage'])
        self.assertEqual(chats['unavailable_sources'][0]['source'], 'whatsapp')
        self.assertNotIn('unavailable_sources', self.client.tool('list_conversations'))
        result = client.request('tools/call', {'name': 'read_conversation',
                                               'arguments': {'conversation_id': 'whatsapp:1'}})
        self.assertTrue(result['isError'])
        self.assertIn('WhatsApp database not found', result['content'][0]['text'])

    def test_independent_clients_and_missing_service(self):
        # A client that disconnects before the service handles it must not stop the service.
        for _ in range(3):
            self.probe()
        time.sleep(0.1)
        self.assertIsNone(self.service_process.poll())
        other = Client(*self.endpoint)
        try:
            self.assertTrue(other.tool('list_conversations')['conversations'])
        finally:
            self.assertEqual(other.close(), 0)
        self.assertIsNone(self.service_process.poll())
        self.assertTrue(self.client.tool('list_conversations')['conversations'])
        initialize = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
            'protocolVersion': PROTOCOL_VERSION, 'capabilities': {},
            'clientInfo': {'name': 'e2e', 'version': '1'}}}) + '\n'
        missing = subprocess.run([str(BINARY), *map(str, self.missing_endpoint())],
                                 input=initialize.encode(), capture_output=True, timeout=TIMEOUT_SECONDS)
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn(b'Cannot connect to MSGMCP', missing.stderr)

    def missing_endpoint(self):
        return ['--socket', self.root / 'missing.sock']

    def test_tool_discovery_and_conversation_pages(self):
        self.assertEqual(self.client.initialized['protocolVersion'], PROTOCOL_VERSION)
        self.assertEqual(self.client.initialized['capabilities'], {'tools': {}})
        tools = self.client.request('tools/list', {})['tools']
        self.assertEqual({t['name'] for t in tools}, {
            'list_conversations', 'search_messages', 'read_conversation',
            'get_message_context', 'send_message'})
        self.assertTrue(all(t['annotations']['readOnlyHint'] for t in tools if t['name'] != 'send_message'))
        send = next(t for t in tools if t['name'] == 'send_message')
        self.assertFalse(send['annotations']['readOnlyHint'])
        self.assertFalse(send['annotations']['idempotentHint'])
        self.assertIn('iMessage, SMS and WhatsApp', self.client.initialized['instructions'])
        self.assertTrue(all(t['inputSchema']['type'] == 'object' for t in tools))
        self.assertTrue(all(t['outputSchema']['type'] == 'object' for t in tools))
        self.assertEqual(self.client.initialized['serverInfo']['name'], 'msgmcp')
        chats = self.client.pages('list_conversations', 'conversations', query='SARAH', limit=1)
        self.assertEqual([(c['conversation_id'], c['name']) for c in chats],
                         [('imessage:1', 'Dinner'), ('imessage:2', 'Other')])
        self.assertEqual(chats[0]['participant_count'], 2)
        self.assertEqual(chats[0]['participants'], [
            {'address': '+15551234567', 'name': None, 'ambiguous': False},
            {'address': 'sarah@example.com', 'name': 'Sarah Test', 'ambiguous': False}])
        self.assertEqual(self.client.tool('list_conversations', query='absent')['conversations'], [])

    def test_recent_activity_and_newest_first(self):
        # Merged newest first; equal times fall back to source then ID; no messages last.
        merged = ['whatsapp:3', 'imessage:1', 'whatsapp:1', 'imessage:2', 'whatsapp:2']
        for limit in (1, 2, 50):
            with self.subTest(limit=limit):
                chats = self.client.pages('list_conversations', 'conversations', limit=limit)
                self.assertEqual([c['conversation_id'] for c in chats], merged)
        # The reaction at +3s doesn't count as activity.
        self.assertEqual(self.client.tool('list_conversations', query='Dinner')['conversations'][0]
                         ['latest_message_at'], timestamp(DATE + 2 * NANOS_PER_SECOND))
        recent = self.client.pages('list_conversations', 'conversations', limit=1,
                                   active_after=timestamp(DATE + 2 * NANOS_PER_SECOND))
        self.assertEqual([c['conversation_id'] for c in recent], merged[:3])
        self.assertEqual(self.client.tool('list_conversations', active_after=timestamp(
            DATE + 7 * NANOS_PER_SECOND))['conversations'], [])
        for chat in ('imessage:1', 'whatsapp:1'):
            with self.subTest(chat=chat):
                oldest = self.client.pages('read_conversation', 'messages', conversation_id=chat)
                newest = self.client.pages('read_conversation', 'messages', conversation_id=chat,
                                           order='newest_first', limit=1)
                self.assertEqual(self.ids(newest), self.ids(oldest)[::-1])
                latest = self.client.tool('read_conversation', conversation_id=chat,
                                          order='newest_first', limit=2)['messages']
                self.assertEqual(self.ids(latest), self.ids(oldest)[:-3:-1])
        bounded = self.client.tool('read_conversation', conversation_id='imessage:1', order='newest_first',
                                   after=timestamp(DATE + NANOS_PER_SECOND))['messages']
        self.assertEqual(self.ids(bounded), [im(4, 1), im(3, 1)])
        searched = self.client.pages('search_messages', 'messages', text='pizza', order='newest_first', limit=1)
        self.assertEqual(self.ids(searched), [wa(3, 1), wa(2, 1), wa(1, 1)])

    def cli(self, *args):
        return subprocess.run([str(BINARY), *args, *map(str, self.endpoint)],
                              capture_output=True, text=True, timeout=TIMEOUT_SECONDS)

    def test_unread_counts_and_filter(self):
        chats = {c['conversation_id']: c for c in self.client.pages('list_conversations', 'conversations')}
        # iMessage: c is unread; the unread reaction e and my own message b don't count.
        self.assertEqual(chats['imessage:1']['unread_count'], 1)
        self.assertEqual(chats['imessage:2']['unread_count'], 0)
        self.assertEqual(chats['whatsapp:1']['unread_count'], 2)
        self.assertEqual(chats['whatsapp:3']['unread_count'], 0)
        # WhatsApp stores a chat marked unread by hand as -1.
        self.assertEqual(chats['whatsapp:2']['unread_count'], 1)
        self.assertTrue(chats['whatsapp:2']['marked_unread'])
        self.assertTrue(all('marked_unread' not in c for i, c in chats.items() if i != 'whatsapp:2'))
        # imessage:1 is filtered; imessage:2 and whatsapp:1 are archived.
        flags = {i: [f for f in ('archived', 'filtered') if c.get(f)] for i, c in chats.items()}
        self.assertEqual(flags, {'imessage:1': ['filtered'], 'imessage:2': ['archived'],
                                 'whatsapp:1': ['archived'], 'whatsapp:2': [], 'whatsapp:3': []})
        for options, expected in [
                ({}, ['whatsapp:2']),
                ({'include_filtered': True}, ['imessage:1', 'whatsapp:2']),
                ({'include_archived': True}, ['whatsapp:1', 'whatsapp:2']),
                ({'include_archived': True, 'include_filtered': True},
                 ['imessage:1', 'whatsapp:1', 'whatsapp:2'])]:
            for limit in (1, 50):
                with self.subTest(options=options, limit=limit):
                    unread = self.client.pages('list_conversations', 'conversations',
                                               unread_only=True, limit=limit, **options)
                    self.assertEqual([c['conversation_id'] for c in unread], expected)
        self.assertEqual([c['conversation_id'] for c in self.client.tool(
            'list_conversations', unread_only=True, include_archived=True, source='whatsapp',
            query='marco')['conversations']], ['whatsapp:1'])
        result = self.client.request('tools/call', {'name': 'list_conversations',
                                                    'arguments': {'include_archived': True}})
        self.assertTrue(result['isError'])
        self.assertIn('require unread_only', result['content'][0]['text'])
        # is_read appears on incoming iMessages only.
        read = {m['message_id']: m.get('is_read', 'absent') for m in self.client.pages(
            'read_conversation', 'messages', conversation_id='imessage:1')}
        self.assertEqual(read, {'imessage:1': True, 'imessage:2': 'absent', 'imessage:3': False,
                                'imessage:4': True})
        self.assertTrue(all('is_read' not in m for m in self.client.pages(
            'read_conversation', 'messages', conversation_id='whatsapp:1')))

    def test_imessage_events_are_hidden(self):
        # Unread events with no text: one between c and d, one newer than every message.
        columns = 'guid,date,is_from_me,handle_id,associated_message_type,is_read,item_type'
        self.db.executemany(f'INSERT INTO message({columns}) VALUES(?,?,0,1,0,0,?)', [
            ('event-mid', DATE + NANOS_PER_SECOND * 3 // 2, 1),
            ('event-new', DATE + 10 * NANOS_PER_SECOND, 5)])
        self.db.executemany('INSERT INTO chat_message_join VALUES(1,?)', [(6,), (7,)])
        self.db.commit()
        everything = [im(1, 1), im(2, 1), im(3, 1), im(4, 1)]
        self.assertEqual(self.ids(self.client.pages('read_conversation', 'messages',
                                                    conversation_id='imessage:1')), everything)
        self.assertEqual(self.ids(self.client.tool('read_conversation', conversation_id='imessage:1',
                                                   order='newest_first', limit=1)['messages']), [im(4, 1)])
        around = self.client.tool('get_message_context', conversation_id='imessage:1',
                                  message_id='imessage:3', before=1, after=1)['messages']
        self.assertEqual(self.ids(around), [im(2, 1), im(3, 1), im(4, 1)])
        result = self.client.request('tools/call', {'name': 'get_message_context', 'arguments': {
            'conversation_id': 'imessage:1', 'message_id': 'imessage:6'}})
        self.assertTrue(result['isError'])
        self.assertIn('Message not found', result['content'][0]['text'])
        chat = next(c for c in self.client.tool('list_conversations', source='imessage')['conversations']
                    if c['conversation_id'] == 'imessage:1')
        self.assertEqual(chat['latest_message_at'], timestamp(DATE + 2 * NANOS_PER_SECOND))
        self.assertEqual(chat['unread_count'], 1)
        recent = self.client.tool('list_conversations', active_after=timestamp(DATE + 5 * NANOS_PER_SECOND))
        self.assertNotIn('imessage:1', [c['conversation_id'] for c in recent['conversations']])

    def test_send_only_to_contacts(self):
        def error(recipient, text):
            result = self.client.request('tools/call', {'name': 'send_message', 'arguments': {
                'recipient': recipient, 'text': text}})
            self.assertTrue(result['isError'], result)
            return result['content'][0]['text']
        # Strangers are refused before anything reaches Messages.
        for stranger in ('+15550000000', 'someone@example.com', '+445550102030'):
            with self.subTest(recipient=stranger):
                message = error(stranger, 'hello')
                self.assertIn('not in Contacts', message)
                self.assertIn('Nothing was sent', message)
        # Contacts pass the check and stop at the empty text, so nothing is ever sent:
        # an email, a number saved with its country code, and one saved without (+1).
        for contact in ('sarah@example.com', '+393331234567', '+15550102030'):
            with self.subTest(recipient=contact):
                self.assertIn('text must not be empty', error(contact, ' '))
        # Any format of a saved number passes; Sarah is saved as (555) 010-2030, Marco as
        # +39 333 123 4567.
        for contact in ('+1 (555) 010-2030', '(555) 010-2030', '555-010-2030', '5550102030',
                        '15550102030', ' SARAH@example.com ', '+39 333 123 4567'):
            with self.subTest(recipient=contact):
                self.assertIn('text must not be empty', error(contact, ' '))
        # A national number without its country code is read as North American, not Italian.
        self.assertIn('not in Contacts', error('333 123 4567', ' '))
        # Short codes and names are not recipients.
        for recipient in ('262966', 'Sarah Test'):
            with self.subTest(recipient=recipient):
                message = error(recipient, ' ')
                self.assertIn('not a phone number with a country code', message)
                self.assertIn('Nothing was sent', message)

    def test_audit_log(self):
        self.client.tool('search_messages', text='dinner', limit=1)
        self.client.request('tools/call', {'name': 'read_conversation',
                                           'arguments': {'conversation_id': 'imessage:999'}})
        calls = [e for e in self.audit() if e['event'] == 'tool_call']
        search, failed = calls[-2], calls[-1]
        self.assertEqual((search['tool'], search['arguments']['text'], search['outcome']),
                         ('search_messages', 'dinner', 'ok'))
        self.assertEqual((failed['tool'], failed['outcome']), ('read_conversation', 'error'))
        self.assertIn('Unknown conversation ID', failed['error'])
        self.assertTrue(all(e['transport'] == self.TRANSPORT and 'time' in e for e in calls))
        caller = search['caller']
        if self.TRANSPORT == 'socket':
            # The socket peer is the stdio adapter; its parent is the client (here, Python).
            self.assertIsInstance(caller['pid'], int)
            self.assertEqual(caller['program'], str(BINARY))
            self.assertIn('ython', pathlib.Path(caller['parent_program']).name)
        else:
            self.assertEqual(caller, {'ip': '127.0.0.1'})
        # What was asked is logged; what was returned never is.
        text = self.audit_log.read_text()
        self.assertNotIn('Dinner at six', text)
        self.assertNotIn('Sarah Test', text)
        self.assertEqual(self.audit_log.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.audit_log.parent.stat().st_mode & 0o777, 0o700)

    def test_cli_schema_and_call(self):
        described = self.cli('schema')
        self.assertEqual(described.returncode, 0, described.stderr)
        schema = json.loads(described.stdout)
        self.assertEqual(schema['tools'], self.client.request('tools/list', {})['tools'])
        self.assertEqual(schema['server']['instructions'], self.client.initialized['instructions'])
        self.assertEqual(schema['transport'][self.endpoint[0][2:]], str(self.endpoint[1]))
        arguments = {'conversation_id': 'imessage:1', 'limit': 2}
        called = self.cli('call', 'read_conversation', json.dumps(arguments))
        self.assertEqual(called.returncode, 0, called.stderr)
        self.assertEqual(json.loads(called.stdout), self.client.tool('read_conversation', **arguments))
        self.assertEqual(json.loads(self.cli('call', 'list_conversations').stdout),
                         self.client.tool('list_conversations'))
        for args, expected in [
                (('call', 'read_conversation', '{"conversation_id": "imessage:999"}'), 'Unknown conversation ID'),
                (('call', 'read_conversation', '[]'), 'must be a JSON object'),
                (('call', 'no_such_tool'), 'tool not found'),
                (('call',), 'needs a tool name')]:
            with self.subTest(args=args):
                failed = self.cli(*args)
                self.assertEqual(failed.returncode, 1)
                self.assertEqual(failed.stdout, '')
                self.assertIn(expected, failed.stderr)

    def test_search_filters_and_equal_timestamp_pagination(self):
        messages = self.client.pages('search_messages', 'messages', text='DINNER', limit=1)
        self.assertEqual(self.ids(messages), [im(1, 1), im(1, 2), im(2, 1)])
        self.assertEqual(self.ids(self.client.tool('search_messages', text='dinner',
                         sender='Sarah')['messages']), [im(1, 1), im(1, 2)])
        own = self.client.tool('search_messages', text='dinner', sender='me')['messages']
        self.assertEqual(self.ids(own), [im(2, 1)])
        self.assertEqual(own[0]['sender'], {'address': None, 'name': 'me', 'ambiguous': False})
        self.assertEqual(self.ids(self.client.tool('search_messages', text='dinner',
                         conversation_id='imessage:2', sender='sarah@example.com', after=timestamp(DATE),
                         before=timestamp(DATE + NANOS_PER_SECOND))['messages']), [im(1, 2)])
        self.assertEqual(self.client.tool('search_messages', text='dinner',
                         before=timestamp(DATE))['messages'], [])
        self.assertEqual(self.client.tool('search_messages', text='%')['messages'], [])

    def test_read_decoding_replies_and_context(self):
        messages = self.client.pages('read_conversation', 'messages', conversation_id='imessage:1', limit=1)
        self.assertEqual(self.ids(messages), [im(1, 1), im(2, 1), im(3, 1), im(4, 1)])
        self.assertEqual(messages[1]['reply_to_guid'], 'a')
        self.assertEqual(messages[0]['sent_at'], timestamp(DATE))
        self.assertEqual(messages[2]['text'], 'Noter test')
        self.assertEqual(messages[2]['text_status'], 'ok')
        self.assertEqual((messages[3]['text'], messages[3]['text_status']), (None, 'decode_failed'))
        decoded = self.client.tool('search_messages', text='Noter test')
        self.assertEqual(self.ids(decoded['messages']), [im(3, 1)])
        self.assertEqual(decoded['decode_failures'], 1)
        context = self.client.tool('get_message_context', conversation_id='imessage:1',
                                   message_id='imessage:2', before=1, after=10)
        self.assertEqual(self.ids(context['messages']), self.ids(messages))
        first = self.client.tool('get_message_context', conversation_id='imessage:1',
                                 message_id='imessage:1', before=10, after=0)
        self.assertEqual(self.ids(first['messages']), [im(1, 1)])

    def test_missing_and_ambiguous_contacts_keep_addresses(self):
        fallback = self.start_client(self.root / 'missing-contacts')
        page = fallback.tool('read_conversation', conversation_id='imessage:2')
        self.assertEqual(page['contacts_databases_read'], 0)
        self.assertEqual(page['messages'][0]['sender'],
                         {'address': 'sarah@example.com', 'name': None, 'ambiguous': False})
        path = self.root / 'contacts/Sources/test/AddressBook-v22.abcddb'
        with sqlite3.connect(path) as cn:
            cn.executescript("INSERT INTO ZABCDRECORD VALUES(2,'Other','Person',NULL,NULL); INSERT INTO ZABCDEMAILADDRESS VALUES('sarah@example.com',2);")
        cn.close()
        sender = self.client.tool('read_conversation', conversation_id='imessage:2')['messages'][0]['sender']
        self.assertEqual(sender, {'address': 'sarah@example.com', 'name': None, 'ambiguous': True})

    def test_number_saved_without_country_code_has_a_name(self):
        # Messages stores +15551234567; Contacts has it as (555) 123-4567.
        path = self.root / 'contacts/Sources/test/AddressBook-v22.abcddb'
        with sqlite3.connect(path) as cn:
            cn.executescript("INSERT INTO ZABCDRECORD VALUES(4,'Pat','Local',NULL,NULL); INSERT INTO ZABCDPHONENUMBER VALUES('(555) 123-4567',4);")
        cn.close()
        chats = self.client.tool('list_conversations', query='pat local')['conversations']
        self.assertEqual([c['conversation_id'] for c in chats], ['imessage:1'])
        self.assertIn({'address': '+15551234567', 'name': 'Pat Local', 'ambiguous': False},
                      chats[0]['participants'])
        messages = self.client.tool('read_conversation', conversation_id='imessage:1')['messages']
        self.assertIn('Pat Local', [m['sender']['name'] for m in messages])

    # One number in the formats people and stores write it. Every tool must treat them alike.
    NUMBER = '+15551234567'
    FORMATS = ['+15551234567', '+1 (555) 123-4567', '+1-555-123-4567', '1 555 123 4567',
               '15551234567', '(555) 123-4567', '555-123-4567', '555.123.4567', '5551234567',
               '\u202a+1 (555) 123-4567\u202c', '+1\u00a0555\u00a0123\u00a04567']

    def test_phone_formats_find_conversations(self):
        for query in self.FORMATS + ['555-1234', '(555) 123']:
            with self.subTest(query=query):
                chats = self.client.tool('list_conversations', query=query)['conversations']
                self.assertEqual([c['conversation_id'] for c in chats], ['imessage:1'])
        # Marco's WhatsApp number, with and without spacing.
        for query in ('+39 333 123 4567', '+393331234567', '333 123 4567'):
            with self.subTest(query=query):
                chats = self.client.tool('list_conversations', query=query, source='whatsapp')
                self.assertIn('+393331234567', [p['address'] for c in chats['conversations']
                                                 for p in c['participants']])
        for query in ('+15551234568', '+44 555 123 4567', '+1 555 123 45678'):
            with self.subTest(query=query):
                self.assertEqual(self.client.tool('list_conversations', query=query)['conversations'], [])

    def test_phone_formats_filter_senders(self):
        expected = self.client.tool('read_conversation', conversation_id='imessage:1',
                                    sender=self.NUMBER)['messages']
        self.assertTrue(expected)
        self.assertTrue(all(m['sender']['address'] == self.NUMBER for m in expected))
        for sender in self.FORMATS:
            with self.subTest(sender=sender):
                found = self.client.tool('read_conversation', conversation_id='imessage:1',
                                         sender=sender)['messages']
                self.assertEqual(found, expected)
        # A partial number is not an address, and another country's number is another person.
        for sender in ('123-4567', '+44 555 123 4567'):
            with self.subTest(sender=sender):
                self.assertEqual(self.client.tool('read_conversation', conversation_id='imessage:1',
                                                  sender=sender)['messages'], [])

    def test_phone_formats_saved_in_contacts_give_a_name(self):
        path = self.root / 'contacts/Sources/test/AddressBook-v22.abcddb'
        for saved in self.FORMATS:
            with self.subTest(saved=saved):
                with sqlite3.connect(path) as cn:
                    cn.execute('DELETE FROM ZABCDPHONENUMBER WHERE ZOWNER=4')
                    cn.execute("INSERT OR REPLACE INTO ZABCDRECORD VALUES(4,'Pat','Local',NULL,NULL)")
                    cn.execute('INSERT INTO ZABCDPHONENUMBER VALUES(?,4)', (saved,))
                cn.close()
                chats = self.client.tool('list_conversations', query='pat local')['conversations']
                self.assertEqual([c['conversation_id'] for c in chats], ['imessage:1'])
                self.assertIn({'address': self.NUMBER, 'name': 'Pat Local', 'ambiguous': False},
                              chats[0]['participants'])
        # The same number saved for two people in different formats is ambiguous.
        with sqlite3.connect(path) as cn:
            cn.executescript("INSERT INTO ZABCDRECORD VALUES(5,'Sam','Other',NULL,NULL); INSERT INTO ZABCDPHONENUMBER VALUES('555-123-4567',5);")
        cn.close()
        participants = self.client.tool('list_conversations', query=self.NUMBER)['conversations'][0]['participants']
        self.assertIn({'address': self.NUMBER, 'name': None, 'ambiguous': True}, participants)
        # A number saved without + is not given a country other than North America.
        with sqlite3.connect(path) as cn:
            cn.executescript("DELETE FROM ZABCDPHONENUMBER WHERE ZOWNER IN (4,5); INSERT INTO ZABCDPHONENUMBER VALUES('445551234567',4);")
        cn.close()
        self.assertEqual(self.client.tool('list_conversations', query='pat local')['conversations'], [])

    def test_tool_errors_allow_later_requests(self):
        total = len(self.client.tool('list_conversations')['conversations'])
        self.assertEqual(total, 5)
        cases = [
            ('send_message', {'recipient': '+15555550123', 'text': ' '}, 'not in Contacts'),
            ('send_message', {'recipient': 'Kaley', 'text': 'test'}, 'contact names are not supported'),
            ('read_conversation', {'conversation_id': 'imessage:999'}, 'Unknown conversation ID'),
            ('read_conversation', {'conversation_id': 'whatsapp:4'}, 'Unknown conversation ID'),
            ('read_conversation', {'conversation_id': '1'}, 'Invalid ID'),
            ('read_conversation', {'conversation_id': 'imessage:1', 'source': 'whatsapp'}, 'does not match'),
            ('read_conversation', {'conversation_id': 'imessage:1', 'after': 'invalid'}, 'RFC 3339'),
            ('read_conversation', {'conversation_id': 'imessage:1', 'limit': 0}, 'limit'),
            ('read_conversation', {'conversation_id': 'imessage:1', 'cursor': 'bad'}, 'Invalid cursor'),
            ('read_conversation', {'conversation_id': 'imessage:1', 'order': 'sideways'}, 'unknown variant'),
            ('list_conversations', {'cursor': 'bad'}, 'Invalid cursor'),
            ('list_conversations', {'cursor': '1:telegram:1'}, 'Invalid cursor'),
            ('list_conversations', {'active_after': 'yesterday'}, 'RFC 3339'),
            ('read_conversation', {'conversation_id': 'whatsapp:1', 'cursor': 'whatsapp:x:1'}, 'Invalid cursor'),
            ('search_messages', {'text': 'x', 'source': 'imessage', 'cursor': 'whatsapp:'}, 'Cursor does not match'),
            ('search_messages', {'text': ' '}, 'text is required'),
            ('search_messages', {'text': 'dinner', 'after': timestamp(DATE), 'before': timestamp(DATE)}, 'earlier'),
            ('get_message_context', {'conversation_id': 'imessage:2', 'message_id': 'imessage:2'}, 'Message not found'),
            ('get_message_context', {'conversation_id': 'whatsapp:1', 'message_id': 'whatsapp:6'}, 'Message not found'),
            ('get_message_context', {'conversation_id': 'imessage:1', 'message_id': 'whatsapp:1'}, 'same source')]
        for name, arguments, expected in cases:
            with self.subTest(tool=name, arguments=arguments):
                result = self.client.request('tools/call', {'name': name, 'arguments': arguments})
                self.assertTrue(result['isError'])
                self.assertIn(expected, result['content'][0]['text'])
                self.assertEqual(len(self.client.tool('list_conversations')['conversations']), total)

    def test_scan_limit_empty_page_continues_to_match(self):
        self.db.executemany('INSERT INTO message(guid,date,is_from_me,handle_id,text,attributedBody,associated_message_type,thread_originator_guid) VALUES(?,?,?,?,?,?,?,?)', [
            (f'scan-{i}', DATE + 4 * NANOS_PER_SECOND + i, 0, 2,
             'needle' if i == SCAN_LIMIT else 'skip', None, 0, None)
            for i in range(SCAN_LIMIT + 1)])
        self.db.executemany('INSERT INTO chat_message_join VALUES(1,?)',
                            [(i,) for i in range(6, SCAN_LIMIT + 7)])
        self.db.commit()
        args = {'text': 'needle', 'conversation_id': 'imessage:1',
                'after': timestamp(DATE + 4 * NANOS_PER_SECOND)}
        first = self.client.tool('search_messages', **args)
        self.assertEqual(first['messages'], [])
        self.assertEqual(first['scanned_rows'], SCAN_LIMIT)
        self.assertFalse(first['complete'])
        self.assertIsNotNone(first['next_cursor'])
        second = self.client.tool('search_messages', **args, cursor=first['next_cursor'])
        self.assertEqual(self.ids(second['messages']), [im(SCAN_LIMIT + 6, 1)])
        self.assertTrue(second['complete'])
        self.assertIsNone(second['next_cursor'])

    def test_current_wal_read_only_and_clean_shutdown(self):
        # Keep the writer open to ensure the new row remains in the WAL.
        self.db.execute("INSERT INTO message(guid,date,is_from_me,handle_id,text,attributedBody,associated_message_type,thread_originator_guid) VALUES('new',?,0,2,'Fresh WAL message',NULL,0,NULL)",
                        (DATE + 4 * NANOS_PER_SECOND,))
        self.db.execute('INSERT INTO chat_message_join VALUES(1,6)')
        self.db.commit()
        self.assertTrue(pathlib.Path(str(self.path) + '-wal').exists())
        before = list(self.db.iterdump())
        wa_path = self.whatsapp / 'ChatStorage.sqlite'
        wa_before = wa_path.read_bytes()
        self.assertEqual(self.ids(self.client.tool('search_messages', text='Fresh WAL')['messages']), [im(6, 1)])
        self.client.tool('list_conversations')
        self.client.tool('read_conversation', conversation_id='imessage:1')
        self.client.tool('get_message_context', conversation_id='imessage:1', message_id='imessage:6')
        self.client.tool('read_conversation', conversation_id='whatsapp:1')
        self.client.tool('get_message_context', conversation_id='whatsapp:1', message_id='whatsapp:2')
        self.assertEqual(list(self.db.iterdump()), before)
        self.assertEqual(wa_path.read_bytes(), wa_before)
        self.assertEqual(self.client.close(), 0)


    def test_default_endpoint_is_the_socket(self):
        if self.TRANSPORT != 'socket':
            self.skipTest('the default gateway address is the installed one')
        # A socket path must fit in 104 bytes, so this HOME can't live under $TMPDIR.
        directory = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(directory.cleanup)
        home = pathlib.Path(directory.name)
        socket = home / 'Library/Application Support/MSGMCP/run/service.sock'
        process, read = self.start_service('socket', '--socket', socket, *self.stores(self.root / 'contacts'))
        self.addCleanup(lambda: (process.terminate(), process.wait(timeout=TIMEOUT_SECONDS)))
        self.wait_for(process, read, socket.exists)
        env = {**os.environ, 'HOME': str(home)}
        called = subprocess.run([str(BINARY), 'call', 'list_conversations'], env=env,
                                capture_output=True, text=True, timeout=TIMEOUT_SECONDS)
        self.assertEqual(called.returncode, 0, called.stderr)
        self.assertEqual(json.loads(called.stdout), self.client.tool('list_conversations'))


class GatewayEndToEnd(EndToEnd):
    TRANSPORT = 'gateway'

    def start_gateway(self, contacts, whatsapp=None, *options):
        self.token_file = self.root / 'gateway' / 'token'
        process, read = self.start_service(
            'gateway', '--listen', '127.0.0.1:0', '--token-file', self.token_file,
            *options, *self.stores(contacts, whatsapp))
        def stop():
            process.terminate()
            self.assertEqual(process.wait(timeout=TIMEOUT_SECONDS), 0, read())
        self.addCleanup(stop)
        self.wait_for(process, read, lambda: 'listening on' in read())
        url = read().split('listening on ', 1)[1].split()[0]
        return process, url

    def start_client(self, contacts, whatsapp=None):
        # The socket suite exercises send_message's schema, so offer it here too.
        process, self.url = self.start_gateway(contacts, whatsapp, '--allow-send')
        self.assertEqual(self.token_file.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.token_file.parent.stat().st_mode & 0o777, 0o700)
        self.token = self.token_file.read_text().strip()
        self.endpoint = ['--url', self.url, '--token-file', self.token_file]
        self.service_process = process
        client = Client(*self.endpoint)
        self.addCleanup(client.close)
        return client

    def probe(self):
        with socket.create_connection(urllib.parse.urlsplit(self.url)[1:2][0].rsplit(':', 1)):
            pass

    def missing_endpoint(self):
        with socket.socket() as unused:
            unused.bind(('127.0.0.1', 0))
            port = unused.getsockname()[1]
        return ['--url', f'http://127.0.0.1:{port}/mcp', '--token-file', self.token_file]

    def post(self, body, token=None, **headers):
        """POST one JSON-RPC message the way a non-MCP agent with curl would."""
        headers = {'Content-Type': 'application/json',
                   'Accept': 'application/json, text/event-stream', **headers}
        if token is not None:
            headers['Authorization'] = f'Bearer {token}'
        request = urllib.request.Request(self.url, json.dumps(body).encode(), headers)
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, None

    def test_bearer_token_origin_and_plain_post(self):
        listing = {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}
        self.assertEqual(self.post(listing)[0], 401)
        self.assertEqual(self.post(listing, 'x' * len(self.token))[0], 401)
        self.assertEqual(self.post(listing, self.token, Origin='https://example.com')[0], 403)
        self.assertEqual(self.post(listing, self.token, Host='elsewhere.example')[0], 403)
        rejected = [(e['reason'], e['status']) for e in self.audit() if e['event'] == 'rejected']
        self.assertEqual(rejected, [('missing token', 401), ('wrong token', 401),
                                    ('host or origin not allowed', 403),
                                    ('host or origin not allowed', 403)])
        self.assertNotIn(self.token, self.audit_log.read_text())
        # A single POST of tools/call works without initialize or a session.
        arguments = {'conversation_id': 'imessage:1', 'limit': 2}
        status, body = self.post({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call',
                                  'params': {'name': 'read_conversation', 'arguments': arguments}},
                                 self.token)
        self.assertEqual(status, 200)
        self.assertEqual(body['result']['structuredContent'],
                         self.client.tool('read_conversation', **arguments))
        env = {**os.environ, 'MSGMCP_TOKEN': 'wrong'}
        refused = subprocess.run([str(BINARY), 'call', 'list_conversations', '--url', self.url],
                                 env=env, capture_output=True, text=True, timeout=TIMEOUT_SECONDS)
        self.assertEqual(refused.returncode, 1)
        self.assertIn('Cannot connect to MSGMCP', refused.stderr)

    def test_caller_from_forwarded_for(self):
        # Behind tailscale serve the tailnet address is the last X-Forwarded-For entry;
        # anything before it was sent by the client.
        forwarded = {'X-Forwarded-For': '100.64.0.9, 100.101.102.103'}
        status, _ = self.post({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {
            'name': 'list_conversations', 'arguments': {'limit': 1}}}, self.token, **forwarded)
        self.assertEqual(status, 200)
        self.assertEqual(self.post({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'}, **forwarded)[0], 401)
        call = [e for e in self.audit() if e['event'] == 'tool_call'][-1]
        rejected = [e for e in self.audit() if e['event'] == 'rejected'][-1]
        self.assertEqual(call['caller']['ip'], '100.101.102.103')
        self.assertEqual(rejected['caller']['ip'], '100.101.102.103')

    def test_send_message_requires_allow_send(self):
        _, url = self.start_gateway(self.root / 'contacts', None)
        client = Client('--url', url, '--token-file', self.token_file)
        self.addCleanup(client.close)
        self.assertNotIn('send_message', {t['name'] for t in client.request('tools/list', {})['tools']})
        self.assertNotIn('Send messages', client.initialized['instructions'])
        with self.assertRaisesRegex(AssertionError, 'tool not found'):
            client.request('tools/call', {'name': 'send_message',
                                          'arguments': {'recipient': '+15550000000', 'text': 'x'}})

    def test_unsafe_token_file_is_refused(self):
        self.token_file.chmod(0o644)
        process, read = self.start_service(
            'gateway', '--listen', '127.0.0.1:0', '--token-file', self.token_file,
            *self.stores(self.root / 'contacts'))
        self.assertNotEqual(process.wait(timeout=TIMEOUT_SECONDS), 0)
        self.assertIn('mode 0600', read())


class AgentScript(unittest.TestCase):
    """agent/msgmcp.sh, as far as it runs without a tailnet."""

    # Secrets end in a marker the output must never contain.
    VALID = {'MSGMCP_URL': 'https://my-mac.tail4f2e1c.ts.net/mcp',
             'TS_AUTHKEY': 'tskey-auth-kQ7pXw2CNTRL-SECRETMARKER',
             'MSGMCP_TOKEN': '3f9a1c7e5b2d4f60SECRETMARKER'}

    def run_script(self, command, **env):
        env = {'PATH': os.environ['PATH'], 'HOME': os.environ['HOME'], **env}
        return subprocess.run(['bash', '-c', f'source agent/msgmcp.sh && {command}'], cwd=ROOT,
                              env=env, capture_output=True, text=True, timeout=TIMEOUT_SECONDS)

    def test_valid_environment_passes_the_check(self):
        for hostname in ({}, {'MSGMCP_AGENT_HOSTNAME': 'grok-agent-2'}):
            result = self.run_script('msgmcp_check_env', **self.VALID, **hostname)
            self.assertEqual((result.returncode, result.stderr), (0, ''))

    def test_bad_environment_fails_every_entry_point_with_its_reason(self):
        cases = [
            ({'MSGMCP_URL': None}, 'MSGMCP_URL is not set'),
            ({'MSGMCP_URL': 'https://my-mac.tail4f2e1c.ts.net/mcp\n'}, 'MSGMCP_URL contains whitespace'),
            ({'MSGMCP_URL': 'http://my-mac.tail4f2e1c.ts.net/mcp'},
             'MSGMCP_URL is "http://my-mac.tail4f2e1c.ts.net/mcp"; it must be https://'),
            ({'MSGMCP_URL': 'https://my-mac.tail4f2e1c.ts.net'},
             'MSGMCP_URL is "https://my-mac.tail4f2e1c.ts.net"; it must be https://'),
            ({'TS_AUTHKEY': None}, 'TS_AUTHKEY is not set'),
            ({'TS_AUTHKEY': 'tskey-auth-SECRETMARKER '}, 'TS_AUTHKEY contains whitespace'),
            ({'TS_AUTHKEY': 'SECRETMARKER'}, 'TS_AUTHKEY does not start with tskey-'),
            ({'MSGMCP_TOKEN': None}, 'MSGMCP_TOKEN is not set'),
            ({'MSGMCP_TOKEN': 'Bearer SECRETMARKER'}, 'MSGMCP_TOKEN contains whitespace'),
            ({'MSGMCP_AGENT_HOSTNAME': 'grok bot'}, 'MSGMCP_AGENT_HOSTNAME is "grok bot"'),
        ]
        for change, reason in cases:
            env = {k: v for k, v in {**self.VALID, **change}.items() if v is not None}
            for command in ('msgmcp_connect', 'msgmcp_test', 'msgmcp list_conversations'):
                with self.subTest(change=change, command=command):
                    result = self.run_script(command, **env)
                    self.assertEqual(result.returncode, 1)
                    output = result.stdout + result.stderr
                    self.assertIn(('FAIL ' if command == 'msgmcp_test' else '') + reason, output)
                    self.assertNotIn('SECRETMARKER', output)
                    # It stops before installing Tailscale or making a request.
                    self.assertNotIn('curl', output)

    def test_every_problem_is_reported_at_once(self):
        result = self.run_script('msgmcp_test')
        self.assertEqual(result.returncode, 1)
        for name in ('MSGMCP_URL', 'TS_AUTHKEY', 'MSGMCP_TOKEN'):
            self.assertIn(f'FAIL {name} is not set', result.stdout)


def live(service='socket'):
    """Read a little from the installed service; `service` is what the LaunchAgent runs."""
    client = Client(*(['--socket', SOCKET] if service == 'socket' else ['--url', GATEWAY_URL]))
    try:
        for source in ('imessage', 'whatsapp'):
            chats = client.tool('list_conversations', source=source, limit=2)
            assert chats['conversations'], f'no {source} conversations'
            chat = chats['conversations'][0]['conversation_id']
            messages = client.tool('read_conversation', conversation_id=chat, limit=2)
            if messages['messages']:
                client.tool('get_message_context', conversation_id=chat,
                            message_id=messages['messages'][0]['message_id'], before=1, after=1)
            print(f'{source}: ok')
        client.tool('search_messages', text='__msgmcp_nonmatching_smoke_query__', limit=1)
    finally:
        assert client.close() == 0
    print('Live MCP check passed')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=pathlib.Path, default=BINARY)
    parser.add_argument('--live', choices=('gateway', 'socket'), nargs='?', const='gateway',
                        help='Also read this Mac\'s Messages and WhatsApp databases through the '
                             'installed service (default: gateway)')
    args = parser.parse_args()
    BINARY = args.binary.resolve()
    suite = unittest.TestSuite(unittest.defaultTestLoader.loadTestsFromTestCase(case)
                               for case in (EndToEnd, GatewayEndToEnd, AgentScript))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        raise SystemExit(1)
    if args.live:
        live(args.live)
