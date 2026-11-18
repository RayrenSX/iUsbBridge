"""Real pinned Cryptex/RemoteXPC lifecycle with device-free transport doubles."""
import asyncio
import contextlib
import plistlib
import socket
import sys
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))
import ddi_support as ddi
import usb_touch_bridge as bridge
from pymobiledevice3.services.cryptexd import CryptexdService, DDI_CRYPTEX_IDENTIFIER
from pymobiledevice3.remote.remotexpc import RemoteXPCConnection
from pymobiledevice3.remote.remote_service_discovery import RemoteServiceDiscoveryService
from pymobiledevice3.remote.xpc_message import XpcUInt64Type


class Writer:
    def __init__(self, *, stalled=False):
        self.closed = self.aborted = 0
        self.stalled = stalled
        self.transport = SimpleNamespace(abort=self.abort)

    def close(self):
        self.closed += 1

    def abort(self):
        self.aborted += 1

    async def wait_closed(self):
        if self.stalled:
            await asyncio.Event().wait()


class CryptexLifecycleTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.connections = []
        self.writers = []
        self.requests = []
        self.transfers = []
        self.features = []
        self.installed = False
        self.stall_handshake = False
        self.stall_close = False
        self.stall_routine = None
        self.handshake_error = None
        self.install_error = None
        self.entered = asyncio.Event()
        self.rsd = SimpleNamespace(
            peer_info={'Services': {CryptexdService.SERVICE_NAME: {}}},
            service=SimpleNamespace(close=AsyncMock()),
            start_remote_service=self.new_connection,
            require_feature=lambda name, feature: self.features.append((name, feature)))
        self.enterContext(patch.object(ddi, 'CLOSE_TIMEOUT_SECONDS', .015))
        self.enterContext(patch.object(ddi, 'SERVICE_TIMEOUT_SECONDS', .025))
        self.enterContext(patch.object(ddi, 'UPLOAD_TIMEOUT_SECONDS', .025))

    def new_connection(self, name):
        self.assertEqual(name, CryptexdService.SERVICE_NAME)
        writer = Writer(stalled=self.stall_close)
        connection = RemoteXPCConnection(('synthetic', 1),
            open_connection=AsyncMock(return_value=(object(), writer)))
        self.connections.append(connection)
        self.writers.append(writer)

        async def handshake():
            if self.stall_handshake:
                self.entered.set()
                await asyncio.Event().wait()
            if self.handshake_error is not None:
                raise self.handshake_error

        async def response(request):
            routine = request['routine']
            self.requests.append(request)
            if routine == self.stall_routine or (routine == 'copy-installed' and
                                                self.stall_routine == 'verification' and self.installed):
                self.entered.set()
                await asyncio.Event().wait()
            if routine == 'copy-installed':
                entries = [{'remote-cryptex-identifier': DDI_CRYPTEX_IDENTIFIER,
                            'remote-cryptex-version': 'test-ddi'}] if self.installed else []
                return {'error': 0, 'argv': {'remote-cryptex-array': entries}}
            if routine == 'read-personalization-id':
                return {'argv': {'img4_chip_chip': 1, 'img4_chip_ecid': 2, 'img4_chip_cpro': True}}
            if routine == 'get-nonce':
                self.assertEqual(request['argv'], {'nonce-domain-handle': XpcUInt64Type(4)})
                return {'argv': {'nonce': b'\0\0' + bytes(48) + (48).to_bytes(4, 'little')}}
            raise AssertionError(routine)

        async def install_request(request, wanting_reply):
            self.assertTrue(wanting_reply)
            self.requests.append(request)
            if self.install_error:
                raise self.install_error
            self.assertEqual(request['routine'], 'install')
            for key in ('auth', 'client-version', 'nonce-persistence', 'persistence'):
                self.assertIsInstance(request['argv'][key], XpcUInt64Type)

        async def transfer(transfer_id, payload):
            if self.stall_routine == 'install':
                self.entered.set()
                await asyncio.Event().wait()
            self.transfers.append((transfer_id, payload))

        async def installed():
            self.installed = True
            return {'error': 0, 'argv': {}}

        connection._do_handshake = handshake
        connection.send_receive_request = response
        connection.send_request = install_request
        connection.send_file_transfer = transfer
        connection.receive_response = installed
        return connection

    def mock_assets(self):
        identity = {
            'Cryptex1,ChipID': '0x1', 'Cryptex1,Type': 'DDI', 'Cryptex1,SubType': 1,
            'Cryptex1,ProductClass': '0x1', 'Cryptex1,UseProductClass': True,
            'Cryptex1,NonceDomain': 4, 'Cryptex1,Version': 'test',
            'Cryptex1,PreauthorizationVersion': 'test',
            'Manifest': {'Cryptex1,GenericDmg': {'Info': {'Personalize': False}},
                         'Cryptex1,CryptexInfoPlist': {'Info': {'Personalize': True}, 'Digest': b'digest'}},
        }
        assets = SimpleNamespace(build_identity=identity, nonce_domain=4, image=b'image',
            trustcache=b'trust', info=b'info', volumehash=b'hash',
            cryptex1_properties={'Cryptex1,NonceDomain': XpcUInt64Type(4)})
        self.enterContext(patch.object(ddi, 'fetch_cryptex_bundle', AsyncMock(return_value=Path('mock'))))
        self.enterContext(patch('pymobiledevice3.services.cryptexd.load_cryptex1_assets', return_value=assets))

        async def ticket(payload, keys):
            request = plistlib.loads(payload)
            self.assertTrue(request['@Cryptex1,Ticket'])
            self.assertEqual(request['Cryptex1,NonceDomain'], 4)
            self.assertEqual(request['Cryptex1,Nonce'], bytes(48))
            self.assertEqual(len(request['Cryptex1,UDID']), 16)
            self.assertNotIn('Cryptex1,GenericDmg', request)
            return b'ticket'
        self.enterContext(patch.object(ddi, 'request_ticket', ticket))

    def assert_closed(self):
        self.rsd.service.close.assert_not_awaited()
        self.assertTrue(self.connections)
        for connection, writer in zip(self.connections, self.writers):
            self.assertTrue(writer.closed or writer.aborted)
            self.assertIsNone(connection._writer)
            self.assertIsNone(connection._reader)
        self.assertFalse([task for task in asyncio.all_tasks() if task is not asyncio.current_task()
                          and not task.done()], 'cleanup left a background task')

    async def test_real_sdk_success_preserves_payloads_nonce_and_parent(self):
        self.mock_assets()
        self.assertEqual(await ddi.prepare_cryptex_ddi(self.rsd), 'test-ddi')
        self.assertEqual([request['routine'] for request in self.requests],
            ['copy-installed', 'read-personalization-id', 'get-nonce', 'install', 'copy-installed'])
        self.assertEqual(self.transfers, list(enumerate([b'image', b'trust', b'ticket', b'info', b'hash'], 1)))
        self.assert_closed()

    async def test_real_sdk_cryptex_rejection_can_reuse_pre2_image(self):
        new_connection = self.rsd.start_remote_service
        def rejected_connection(name):
            connection = new_connection(name)
            connection.send_receive_request = AsyncMock(return_value={
                'cferr': {'cferr_domain': 'synthetic', 'cferr_code': 1,
                          'cferr_userinfo': {'NSLocalizedDescription': 'unsupported operation'}}})
            return connection
        self.rsd.start_remote_service = rejected_connection
        session = bridge.TouchSession(SimpleNamespace(emit=AsyncMock()), 120, 'synthetic')
        lockdown = SimpleNamespace(product_version='27.0.1',
                                   get_developer_mode_status=AsyncMock(return_value=True))
        async def prepare(_):
            await ddi.prepare_cryptex_ddi(self.rsd, session._emit_status)
        with patch.object(session, '_prepare_cryptex_developer_image', prepare), \
             patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=True)) as inventory, \
             patch.object(session, '_mount_personalized_ddi', AsyncMock()) as mount:
            await session._preflight_developer_environment(lockdown)
            inventory.assert_awaited_once_with(lockdown)
            mount.assert_not_awaited()
        warnings = [call.args[0] for call in session.ipc.emit.call_args_list
                    if call.args[0].get('event') == 'warning']
        self.assertEqual(warnings, [{'event': 'warning', 'code': 'developer_image_cryptex_fallback',
            'message': 'Cryptex preparation failed (developer_image_mount_failed); trying Personalized DDI.'}])
        self.assert_closed()

    async def test_connect_timeout_closes_partial_handshake(self):
        self.stall_handshake = True
        with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
            await ddi.prepare_cryptex_ddi(self.rsd)
        self.assertEqual(raised.exception.code, 'developer_image_service_timeout')
        self.assert_closed()

    async def test_connect_exception_is_preserved_even_when_close_stalls(self):
        self.handshake_error = ConnectionError('synthetic connection lost')
        self.stall_close = True
        with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
            await ddi.prepare_cryptex_ddi(self.rsd)
        self.assertEqual(raised.exception.code, 'apple_connection_lost')
        self.assert_closed()
        self.assertGreater(self.writers[0].aborted, 0)

    async def test_connect_error_survives_operation_deadline_during_cleanup(self):
        self.handshake_error = ConnectionError('synthetic connection lost')
        self.stall_close = True
        # Force the operation budget to cancel SDK cleanup before its own
        # deadline. This ordering must not change the already-known failure.
        with patch.object(ddi, 'CLOSE_TIMEOUT_SECONDS', 1), \
                patch.object(ddi, 'SERVICE_TIMEOUT_SECONDS', .01):
            with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                await ddi.prepare_cryptex_ddi(self.rsd)
        self.assertEqual(raised.exception.code, 'apple_connection_lost')
        self.assert_closed()
        self.assertGreater(self.writers[0].aborted, 0)

    async def test_inventory_timeout_with_stalled_close_returns_within_cleanup_budget(self):
        self.stall_routine = 'copy-installed'
        self.stall_close = True
        started = time.monotonic()
        with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
            await ddi.prepare_cryptex_ddi(self.rsd)
        self.assertLess(time.monotonic() - started, .3)
        self.assertEqual(raised.exception.code, 'developer_image_service_timeout')
        self.assert_closed()
        self.assertGreater(self.writers[0].aborted, 0)

    async def test_total_preflight_timeout_is_not_swallowed_by_sdk_finally(self):
        self.stall_routine = 'copy-installed'
        self.stall_close = True
        session = bridge.TouchSession(SimpleNamespace(emit=AsyncMock()), 120, 'synthetic')
        session._ddi_deadline = asyncio.get_running_loop().time() + .02
        session._prepare_developer_environment = lambda _: ddi.prepare_cryptex_ddi(self.rsd)
        started = time.monotonic()
        with patch.object(ddi, 'SERVICE_TIMEOUT_SECONDS', 1):
            with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                await session._preflight_developer_environment(object())
        self.assertEqual(raised.exception.code, 'developer_image_prepare_timeout')
        self.assertLess(time.monotonic() - started, .3)
        self.assert_closed()

    async def test_user_cancel_during_handshake_releases_without_masking_cancel(self):
        self.stall_handshake = True
        self.stall_close = True
        task = asyncio.create_task(ddi.prepare_cryptex_ddi(self.rsd))
        await self.entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assert_closed()

    async def test_real_sdk_phase_timeouts_are_routed(self):
        for routine, code in [('read-personalization-id', 'developer_image_service_timeout'),
                              ('get-nonce', 'developer_image_service_timeout'),
                              ('install', 'developer_image_upload_timeout'),
                              ('verification', 'developer_image_service_timeout')]:
            with self.subTest(routine=routine):
                self.installed = False
                self.mock_assets()
                self.stall_routine = routine
                with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                    await ddi.prepare_cryptex_ddi(self.rsd)
                self.assertEqual(raised.exception.code, code)
                self.assert_closed()

    async def test_install_connection_cancel_releases_own_writer(self):
        self.mock_assets()
        original = self.new_connection
        count = 0
        def create(name):
            nonlocal count
            count += 1
            self.stall_handshake = count == 4
            return original(name)
        self.rsd.start_remote_service = create
        task = asyncio.create_task(ddi.prepare_cryptex_ddi(self.rsd))
        await self.entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assert_closed()

    async def test_existing_image_does_not_fetch_sign_or_install(self):
        self.installed = True
        fetch = self.enterContext(patch.object(ddi, 'fetch_cryptex_bundle', AsyncMock()))
        self.assertEqual(await ddi.prepare_cryptex_ddi(self.rsd), 'test-ddi')
        fetch.assert_not_awaited()
        self.assertEqual(len(self.connections), 1)
        self.assert_closed()

    async def test_file_reader_cleanup_cannot_swallow_the_close_deadline(self):
        raw = self.new_connection(CryptexdService.SERVICE_NAME)
        await raw.connect()
        self.writers[-1].stalled = True
        started = asyncio.Event()
        async def reader():
            started.set()
            try:
                await asyncio.Event().wait()
            finally:
                await asyncio.sleep(1)
        raw._file_chunk_reader_task = asyncio.create_task(reader())
        await started.wait()
        connection = ddi._CryptexConnection(raw)
        deadline = time.monotonic()
        await connection.close()
        self.assertLess(time.monotonic() - deadline, .3)
        self.assertGreater(self.writers[-1].aborted, 0)
        self.assert_closed()

    async def test_cancel_during_close_aborts_and_propagates_cancellation(self):
        raw = self.new_connection(CryptexdService.SERVICE_NAME)
        await raw.connect()
        entered = asyncio.Event()
        async def wait_closed():
            entered.set()
            await asyncio.Event().wait()
        self.writers[-1].wait_closed = wait_closed
        connection = ddi._CryptexConnection(raw)
        task = asyncio.create_task(connection.close())
        await entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assert_closed()
        self.assertGreater(self.writers[-1].aborted, 0)

    async def test_real_socket_is_closed_after_partial_connect_cancellation(self):
        client, peer = socket.socketpair()
        writer = None
        async def open_connection(*_):
            nonlocal writer
            reader, writer = await asyncio.open_connection(sock=client)
            return reader, writer
        raw = RemoteXPCConnection(('offline', 1), open_connection=open_connection)
        entered = asyncio.Event()
        async def handshake():
            entered.set()
            await asyncio.Event().wait()
        raw._do_handshake = handshake
        owned = ddi._CryptexConnection(raw)
        try:
            task = asyncio.create_task(owned.connect())
            await entered.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
            self.assertTrue(writer.is_closing())
            self.assertIsNone(raw._writer)
            self.assertEqual(client.fileno(), -1)
        finally:
            peer.close()
            client.close()

    async def test_same_parent_can_be_reused_after_timed_out_connection(self):
        self.stall_handshake = True
        with self.assertRaises(ddi.BridgePrerequisiteError):
            await ddi.prepare_cryptex_ddi(self.rsd)
        self.stall_handshake = False
        self.installed = True
        self.assertEqual(await ddi.prepare_cryptex_ddi(self.rsd), 'test-ddi')
        self.assertEqual(len(self.connections), 2)
        self.assert_closed()

    async def test_cancel_during_real_sdk_payload_upload_does_not_verify_or_leak(self):
        self.mock_assets()
        self.stall_routine = 'install'
        task = asyncio.create_task(ddi.prepare_cryptex_ddi(self.rsd))
        await self.entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertFalse(self.installed)
        self.assertEqual([r['routine'] for r in self.requests].count('copy-installed'), 1)
        self.assert_closed()

    async def test_preparation_parent_rsd_stalled_close_is_aborted(self):
        rsd = RemoteServiceDiscoveryService(('offline', 1))
        writer = Writer(stalled=True)
        rsd.service._writer, rsd.service._reader = writer, object()
        session = bridge.TouchSession(SimpleNamespace(emit=AsyncMock()), 120)
        with patch.object(bridge, 'HID_CLEANUP_TIMEOUT_SECONDS', .015):
            await session._close_remote_service(rsd, 'cryptex_preparation_rsd')
        self.assertEqual(writer.aborted, 1)
        self.assertIsNone(rsd.service._writer)
        self.assertIsNone(rsd.service._reader)


if __name__ == '__main__':
    unittest.main()
