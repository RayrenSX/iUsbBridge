"""Readiness and nested wireless recovery boundaries for both control modes."""
import asyncio
import contextlib
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))
import usb_touch_bridge as bridge
from usb_recovery_test import Hid, Ipc


class ControlFlowAuditTests(unittest.IsolatedAsyncioTestCase):
    async def test_discovery_to_real_input_dispatch_and_cleanup(self):
        for route in ('usb', 'network', 'remote-pairing'):
            with self.subTest(route=route), contextlib.ExitStack() as stack:
                ipc = Ipc()
                session = bridge.TouchSession(ipc, 120, 'test-device',
                                              'usb' if route == 'usb' else 'wireless')
                lockdown = SimpleNamespace(udid='test-device', product_version='26.0',
                    get_developer_mode_status=AsyncMock(return_value=True), close=AsyncMock())
                endpoint = SimpleNamespace(close=AsyncMock())
                tun = SimpleNamespace(set_peer=Mock(), close=AsyncMock())
                result = SimpleNamespace(client=SimpleNamespace(tun=tun), address='phone', port=1)
                hids, discoveries, planes = [], [], []
                class Dial:
                    dial = AsyncMock()
                    def __init__(self, *_):
                        self.closed = False
                        planes.append(self)
                    async def __aenter__(self): return self
                    async def __aexit__(self, *_): self.closed = True
                class Rsd:
                    udid = 'test-device'
                    peer_info = {'Services': {bridge.LEGACY_UNIVERSAL_HID_SERVICE: {}}}
                    def __init__(self, *_args, **_kwargs):
                        self.closed = False
                        self.service = SimpleNamespace(_writer=None)
                        discoveries.append(self)
                    async def __aenter__(self): return self
                    async def __aexit__(self, *_): self.closed = True
                class LegacyHid(Hid):
                    SERVICE_NAME = bridge.LEGACY_UNIVERSAL_HID_SERVICE
                    def __init__(self, rsd):
                        super().__init__(rsd)
                        self.closed = False
                        hids.append(self)
                    async def __aexit__(self, *_): self.closed = True
                @contextlib.asynccontextmanager
                async def tunnel(*_args, **_kwargs):
                    yield result
                @contextlib.asynccontextmanager
                async def rejected_media(_):
                    raise RuntimeError('startmediastream rejected 9021')
                    yield
                async def messages():
                    self.assertTrue(session._session_ready.is_set())
                    self.assertEqual(ipc.events[-1]['event'], 'ready')
                    for seq, usages in ((1, [4]), (2, [])):
                        yield {'schema': bridge.MESSAGE_SCHEMA, 'kind': bridge.KEYBOARD_MESSAGE_KIND,
                               'generation': session._generation, 'seq': seq, 'usages': usages}
                    for seq, action in ((3, 'down'), (4, 'up')):
                        yield {'schema': bridge.MESSAGE_SCHEMA, 'kind': bridge.MESSAGE_KIND,
                               'generation': session._generation, 'seq': seq,
                               'points': [{'pointerId': 1, 'action': action,
                                           'normalizedX': .25, 'normalizedY': .5}]}
                ipc.read_messages = messages
                stack.enter_context(patch.object(session, '_start_capture_mux', AsyncMock()))
                create = stack.enter_context(patch.object(session, '_create_initial_lockdown',
                    AsyncMock(side_effect=bridge.DeviceNotFoundError('test-device')) if route == 'remote-pairing'
                    else AsyncMock(return_value=lockdown)))
                stack.enter_context(patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=True)))
                stack.enter_context(patch.object(session, '_provision_remote_pairing', AsyncMock(return_value=True)))
                stack.enter_context(patch.object(bridge.CoreDeviceTunnelProxy, 'create', AsyncMock(return_value=endpoint)))
                stack.enter_context(patch.object(bridge, 'start_tunnel', tunnel))
                stack.enter_context(patch('pymobiledevice3.remote.userspace_tunnel.UserspaceDialPlane', Dial))
                stack.enter_context(patch.object(bridge, 'RemoteServiceDiscoveryService', Rsd))
                stack.enter_context(patch.object(bridge, 'LegacyUniversalHIDServiceService', LegacyHid))
                stack.enter_context(patch.object(bridge, 'touch_session', rejected_media))
                stack.enter_context(patch.object(bridge, 'iter_remote_paired_identifiers', return_value=['test-device']))
                stack.enter_context(patch.object(bridge, 'get_remote_pairing_tunnel_services', AsyncMock(return_value=[endpoint])))
                stack.enter_context(patch.object(session, '_poll_device_pasteboard', AsyncMock()))
                await asyncio.wait_for(session.connect(), 2)
                create.assert_awaited_once_with('USB' if route == 'usb' else 'Network')
                ready = [event for event in ipc.events if event.get('event') == 'ready']
                self.assertEqual(len(ready), 1)
                self.assertEqual((ready[0]['transport'], ready[0]['authMode']),
                                 ('usb' if route == 'usb' else 'wireless', 'direct'))
                self.assertIn((512, [4]), hids[0].keys)
                self.assertEqual(len(hids[0].reports), bridge.MAX_SLOTS * 2 + 2)
                self.assertTrue(all(item.closed for item in hids + discoveries + planes))
                self.assertFalse(session.gate_open)
                self.assertIsNone(session.hid)
                endpoint.close.assert_awaited_once()
                self.assertEqual(lockdown.close.await_count, 0 if route == 'remote-pairing' else 1)

    async def test_ready_requires_both_gate_and_hid_for_both_transports(self):
        for mode in ('usb', 'wireless'):
            for gate, hid in ((False, Hid()), (True, None), (False, None)):
                session = bridge.TouchSession(Ipc(), 120, 'test-device', mode)
                session.gate_open, session.hid = gate, hid
                with self.subTest(mode=mode, gate=gate, hid=hid is not None), \
                     patch.object(session, '_probe_hid_sender', AsyncMock()) as probe:
                    with self.assertRaises(bridge.BridgePrerequisiteError):
                        await session._emit_ready()
                    probe.assert_not_awaited()
                    self.assertEqual(session._generation, 0)
                    self.assertFalse(session._session_ready.is_set())
                    self.assertFalse(any(event.get('event') == 'ready' for event in session.ipc.events))

    async def test_nested_wireless_routes_share_one_usb_preparation(self):
        session = bridge.TouchSession(Ipc(), 120, 'test-device', 'wireless')
        network = SimpleNamespace(udid='test-device')
        usb = SimpleNamespace(udid='test-device', close=AsyncMock())
        endpoints = []
        def discover(**_):
            service = SimpleNamespace(close=AsyncMock())
            endpoints.append(service)
            return [service]
        @contextlib.asynccontextmanager
        async def tunnel(_):
            yield object()
        async def missing(_):
            raise bridge.BridgePrerequisiteError('touch_surface_unavailable', 'missing HID')
        with patch.object(bridge, 'iter_remote_paired_identifiers', return_value=['test-device']), \
             patch.object(bridge, 'get_remote_pairing_tunnel_services', AsyncMock(side_effect=discover)), \
             patch.object(session, '_bounded_tunnel', tunnel), \
             patch.object(session, '_run_observed_tunnel', missing), \
             patch.object(session, '_preflight_developer_environment', AsyncMock()), \
             patch.object(session, '_create_lockdown_with_retry', AsyncMock(return_value=usb)) as create, \
             patch.object(session, '_run_tunnel_attempt', lambda _: session._connect_via_remote_pairing()), \
             patch.object(bridge, 'HID_SERVICE_REGISTRATION_RETRY_SECONDS', 0):
            with self.assertRaises(bridge.BridgePrerequisiteError) as raised:
                await session._connect_with_ddi_recovery(network)
            self.assertEqual(raised.exception.code, 'touch_surface_unavailable')
            create.assert_awaited_once_with('USB')
            usb.close.assert_awaited_once()
            for endpoint in endpoints:
                endpoint.close.assert_awaited_once()

    async def test_usb_preparation_cancellation_does_not_record_success(self):
        session = bridge.TouchSession(Ipc(), 120, 'test-device', 'wireless')
        usb = SimpleNamespace(udid='test-device', close=AsyncMock())
        with patch.object(session, '_create_lockdown_with_retry', AsyncMock(return_value=usb)), \
             patch.object(session, '_preflight_developer_environment', AsyncMock(side_effect=asyncio.CancelledError)):
            with self.assertRaises(asyncio.CancelledError):
                await session._prepare_ddi_for_remote_pairing()
            self.assertFalse(getattr(session, '_wireless_ddi_prepared', False))
            usb.close.assert_awaited_once()

    async def test_paste_cannot_send_a_chord_after_recovery_changes_generation(self):
        for point in ('clipboard', 'modifier'):
            session = bridge.TouchSession(Ipc(), 120, 'test-device')
            session.rsd = object()
            session._generation = 1
            sent = []
            async def write(_):
                if point == 'clipboard': session._generation = 2
            async def send(usages, *_):
                sent.append((session._generation, usages))
                if point == 'modifier': session._generation = 2
            with self.subTest(point=point), \
                 patch.object(session, '_write_device_pasteboard', write), \
                 patch.object(session, '_send_keyboard_report', send), \
                 patch.object(bridge.asyncio, 'sleep', AsyncMock()):
                with self.assertRaises(RuntimeError):
                    await session._apply_paste_text('old paste')
                self.assertFalse(any(generation == 2 for generation, _ in sent))


if __name__ == '__main__':
    unittest.main()
