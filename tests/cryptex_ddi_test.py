"""Issue #52: verified Cryptex assets, Apple signing and exact-device preparation."""
import asyncio
import contextlib
import hashlib
import plistlib
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))
import httpx
import ddi_support as ddi
import usb_touch_bridge as bridge
from pymobiledevice3.services import cryptexd
from pymobiledevice3.remote import userspace_tunnel


class BundleTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        # Ordinary download tests must not depend on developer/build payloads.
        self.bundle_patch = patch.object(ddi, 'bundled_cryptex_directory', return_value=Path('missing-test-ddi'))
        self.bundle_patch.start()
        self.addCleanup(self.bundle_patch.stop)

    def fixture(self, root):
        payloads = {name: name.encode() for name in ddi.CRYPTEX_PAYLOAD_PATHS.values()}
        manifest = {'ProductBuildVersion': ddi.CRYPTEX_DDI_BUILD, 'BuildIdentities': [{
            'Info': {'Variant': 'iOS Customer Developer Disk Image Cryptex'},
            'Manifest': {key: {'Info': {'Path': name}, 'Digest': hashlib.sha384(payloads[name]).digest()}
                         for key, name in ddi.CRYPTEX_PAYLOAD_PATHS.items()},
        }]}
        payloads['BuildManifest.plist'] = plistlib.dumps(manifest)
        hashes = {name: (len(data), hashlib.sha256(data).hexdigest()) for name, data in payloads.items()}
        return payloads, hashes

    async def test_bundled_cryptex_is_verified_and_used_without_network_or_writable_cache(self):
        with TemporaryDirectory() as temp:
            root = Path(temp)
            payloads, hashes = self.fixture(root)
            for name, data in payloads.items():
                (root / name).write_bytes(data)
            with patch.object(ddi, 'bundled_cryptex_directory', return_value=root), \
                 patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
                 patch.object(ddi.httpx, 'AsyncClient') as network, \
                 patch.object(ddi.Path, 'mkdir') as mkdir:
                self.assertEqual(await ddi.fetch_cryptex_bundle(), root)
                network.assert_not_called()
                mkdir.assert_not_called()

    async def test_corrupt_bundle_falls_back_to_verified_download(self):
        with TemporaryDirectory() as temp:
            root = Path(temp) / 'bundle'
            root.mkdir()
            payloads, hashes = self.fixture(root)
            for name, data in payloads.items():
                (root / name).write_bytes(data)
            (root / 'Image.dmg').write_bytes(b'bad')
            real_client = httpx.AsyncClient
            with patch.object(ddi, 'bundled_cryptex_directory', return_value=root), \
                 patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
                 patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
                 patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                     real_client(transport=httpx.MockTransport(lambda request: httpx.Response(
                         200, content=payloads[request.url.path.rsplit('/', 1)[-1]])), **kw)):
                actual = await ddi.fetch_cryptex_bundle()
                self.assertNotEqual(actual, root)
                ddi.validate_cryptex_bundle(actual)

    async def test_download_validates_all_assets_and_reuses_verified_cache(self):
        with TemporaryDirectory() as temp:
            payloads, hashes = self.fixture(Path(temp))
            requests = []
            def handler(request):
                requests.append(str(request.url))
                return httpx.Response(200, content=payloads[request.url.path.rsplit('/', 1)[-1]])
            real_client = httpx.AsyncClient
            with patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
                 patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
                 patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                              real_client(transport=httpx.MockTransport(handler), **kw)):
                root = await ddi.fetch_cryptex_bundle()
                self.assertEqual(await ddi.fetch_cryptex_bundle(), root)
                self.assertEqual(len(requests), 5)
                self.assertTrue(all(ddi.CRYPTEX_DDI_REVISION in url for url in requests))
                # A damaged cache entry is downloaded again, not trusted by size.
                (root / 'Image.dmg').write_bytes(b'x' * len(payloads['Image.dmg']))
                await ddi.fetch_cryptex_bundle()
                self.assertEqual(len(requests), 6)
                self.assertFalse(list(root.glob('*.part')))

    async def test_bad_download_cannot_replace_cached_image(self):
        with TemporaryDirectory() as temp:
            _, hashes = self.fixture(Path(temp))
            real_client = httpx.AsyncClient
            with patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
                 patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
                 patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                              real_client(transport=httpx.MockTransport(lambda _: httpx.Response(200, content=b'bad')), **kw)):
                with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                    await ddi.fetch_cryptex_bundle()
                self.assertEqual(raised.exception.code, 'developer_image_download_failed')
            root = Path(temp) / 'iPhoneMirror' / 'DDI' / 'Cryptex1' / ddi.CRYPTEX_DDI_REVISION
            self.assertFalse(list(root.iterdir()))

    async def test_raw_connection_failure_falls_back_to_same_revision_api(self):
        with TemporaryDirectory() as temp:
            payloads, hashes = self.fixture(Path(temp))
            requests = []
            def handler(request):
                requests.append(request)
                if request.url.host == 'raw.githubusercontent.com':
                    raise httpx.ConnectError('connection reset', request=request)
                self.assertEqual(request.url.params['ref'], ddi.CRYPTEX_DDI_REVISION)
                self.assertEqual(request.headers['Accept'], 'application/vnd.github.raw+json')
                return httpx.Response(200, content=payloads[request.url.path.rsplit('/', 1)[-1]])
            real_client = httpx.AsyncClient
            with patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
                 patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
                 patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                              real_client(transport=httpx.MockTransport(handler), **kw)):
                root = await ddi.fetch_cryptex_bundle()
                ddi.validate_cryptex_bundle(root)
                self.assertEqual(len(requests), 10)
                self.assertFalse(list(root.glob('*.part')))

    async def test_invalid_raw_body_uses_verified_api_fallback(self):
        for damage in ('hash', 'truncated', 'oversized'):
            with self.subTest(damage=damage), TemporaryDirectory() as temp:
                payloads, hashes = self.fixture(Path(temp))
                requests = []
                def handler(request):
                    requests.append(request)
                    data = payloads[request.url.path.rsplit('/', 1)[-1]]
                    if request.url.host == 'raw.githubusercontent.com':
                        data = {'hash': b'x' * len(data), 'truncated': data[:-1],
                                'oversized': data + b'x'}[damage]
                    else:
                        self.assertEqual(request.url.params['ref'], ddi.CRYPTEX_DDI_REVISION)
                    return httpx.Response(200, content=data)
                real_client = httpx.AsyncClient
                with patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
                     patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
                     patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                        real_client(transport=httpx.MockTransport(handler), **kw)):
                    root = await ddi.fetch_cryptex_bundle()
                    ddi.validate_cryptex_bundle(root)
                self.assertEqual(len(requests), 10)
                self.assertFalse(list(root.glob('*.part')))

    async def test_manifest_digest_is_checked_even_when_pinned_file_hash_matches(self):
        with TemporaryDirectory() as temp:
            root = Path(temp)
            payloads, hashes = self.fixture(root)
            manifest = plistlib.loads(payloads['BuildManifest.plist'])
            manifest['BuildIdentities'][0]['Manifest']['Cryptex1,GenericDmg']['Digest'] = b'bad'
            payloads['BuildManifest.plist'] = plistlib.dumps(manifest)
            hashes['BuildManifest.plist'] = (len(payloads['BuildManifest.plist']),
                                            hashlib.sha256(payloads['BuildManifest.plist']).hexdigest())
            for name, data in payloads.items():
                (root / name).write_bytes(data)
            with patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes):
                with self.assertRaises(ddi.BridgePrerequisiteError):
                    ddi.validate_cryptex_bundle(root)

    async def test_cancelled_download_removes_partial_without_publishing(self):
        entered = asyncio.Event()
        payloads, hashes = self.fixture(Path('unused'))
        class SlowBody(httpx.AsyncByteStream):
            async def __aiter__(self):
                yield b'partial'
                entered.set()
                await asyncio.Event().wait()
        real_client = httpx.AsyncClient
        with TemporaryDirectory() as temp, patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
             patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
             patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                real_client(transport=httpx.MockTransport(lambda _: httpx.Response(200, stream=SlowBody())), **kw)):
            task = asyncio.create_task(ddi.fetch_cryptex_bundle())
            await entered.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
            root = Path(temp) / 'iPhoneMirror/DDI/Cryptex1' / ddi.CRYPTEX_DDI_REVISION
            self.assertEqual(list(root.iterdir()), [])

    async def test_total_download_budget_has_specific_timeout_and_cleans_partials(self):
        _, hashes = self.fixture(Path('unused'))
        class SlowBody(httpx.AsyncByteStream):
            async def __aiter__(self):
                yield b'partial'
                await asyncio.Event().wait()
        real_client, timeout = httpx.AsyncClient, asyncio.timeout
        with TemporaryDirectory() as temp, patch.dict(ddi.os.environ, {'LOCALAPPDATA': temp}), \
             patch.object(ddi, 'CRYPTEX_DDI_FILES', hashes), \
             patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                real_client(transport=httpx.MockTransport(lambda _: httpx.Response(200, stream=SlowBody())), **kw)), \
             patch.object(ddi.asyncio, 'timeout', side_effect=lambda seconds: timeout(.015 if seconds == 90 else seconds)):
            with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                await ddi.fetch_cryptex_bundle()
            self.assertEqual(raised.exception.code, 'developer_image_download_timeout')
            root = Path(temp) / 'iPhoneMirror/DDI/Cryptex1' / ddi.CRYPTEX_DDI_REVISION
            self.assertEqual(list(root.iterdir()), [])


class InstallTests(unittest.IsolatedAsyncioTestCase):
    def setup_install(self, stack, inventories):
        rsd = SimpleNamespace(peer_info={'Services': {cryptexd.CryptexdService.SERVICE_NAME: {}}})
        service = SimpleNamespace(copy_installed=AsyncMock(side_effect=inventories),
            read_personalization_identifiers=AsyncMock(return_value={'device': 'private'}),
            cryptex_nonce=AsyncMock(return_value=b'nonce'), install=AsyncMock())
        stack.enter_context(patch.object(cryptexd, 'CryptexdService', Mock(
            return_value=service, SERVICE_NAME=cryptexd.CryptexdService.SERVICE_NAME)))
        fetch = stack.enter_context(patch.object(ddi, 'fetch_cryptex_bundle', AsyncMock(return_value=Path('verified'))))
        assets = SimpleNamespace(build_identity={'identity': True}, nonce_domain=4,
            image=b'image', trustcache=b'trust', info=b'info', volumehash=b'hash', cryptex1_properties={'uint64': True})
        stack.enter_context(patch.object(cryptexd, 'load_cryptex1_assets', Mock(return_value=assets)))
        request = SimpleNamespace(_request={'request': True}, add_cryptex1_tags=Mock())
        stack.enter_context(patch.object(ddi, 'TSSRequest', Mock(return_value=request)))
        ticket = stack.enter_context(patch.object(ddi, 'request_ticket', AsyncMock(return_value=b'apple-ticket')))
        return rsd, service, fetch, assets, request, ticket

    async def test_install_uses_upstream_nonce_ticket_and_all_payloads_then_verifies(self):
        installed = SimpleNamespace(identifier=cryptexd.DDI_CRYPTEX_IDENTIFIER, version='27.1.5228.8')
        with contextlib.ExitStack() as stack:
            rsd, service, _, assets, request, ticket = self.setup_install(stack, [[], [installed]])
            status = AsyncMock()
            self.assertEqual(await ddi.prepare_cryptex_ddi(rsd, status), installed.version)
            service.cryptex_nonce.assert_awaited_once_with(4)
            request.add_cryptex1_tags.assert_called_once_with(assets.build_identity, {'device': 'private'}, b'nonce')
            ticket.assert_awaited_once_with(plistlib.dumps(request._request), ('Cryptex1,Ticket', 'ApImg4Ticket'))
            service.install.assert_awaited_once_with(b'image', b'trust', b'apple-ticket', b'info', b'hash', {'uint64': True})
            self.assertEqual(status.await_args_list[-1].args, ('verifying_developer_image',))

    async def test_existing_ddi_is_preserved_without_download_or_nonce_change(self):
        installed = SimpleNamespace(identifier=cryptexd.DDI_CRYPTEX_IDENTIFIER, version='newer-ddi')
        with contextlib.ExitStack() as stack:
            rsd, service, fetch, _, _, ticket = self.setup_install(stack, [[installed]])
            self.assertEqual(await ddi.prepare_cryptex_ddi(rsd), 'newer-ddi')
            fetch.assert_not_awaited()
            ticket.assert_not_awaited()
            service.install.assert_not_awaited()
            service.cryptex_nonce.assert_not_awaited()

    async def test_install_acknowledgment_without_inventory_is_not_success(self):
        with contextlib.ExitStack() as stack:
            rsd, _, _, _, _, _ = self.setup_install(stack, [[], [], [], []])
            with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                await ddi.prepare_cryptex_ddi(rsd)
            self.assertEqual(raised.exception.code, 'developer_image_verification_failed')

    async def test_cryptex_ticket_and_upstream_fallback(self):
        real_client = httpx.AsyncClient
        for key in ('Cryptex1,Ticket', 'ApImg4Ticket'):
            with self.subTest(key=key), patch.object(ddi.httpx, 'AsyncClient', side_effect=lambda **kw:
                    real_client(transport=httpx.MockTransport(lambda _: httpx.Response(200,
                        content=b'STATUS=0&MESSAGE=SUCCESS&REQUEST_STRING=' + plistlib.dumps({key: b'ticket'}))), **kw)):
                self.assertEqual(await ddi.request_ticket(b'request', ('Cryptex1,Ticket', 'ApImg4Ticket')), b'ticket')

    async def test_cancelled_install_remains_cancellation(self):
        with contextlib.ExitStack() as stack:
            rsd, service, _, _, _, _ = self.setup_install(stack, [[]])
            started = asyncio.Event()
            async def stall(*_):
                started.set()
                await asyncio.Event().wait()
            service.install.side_effect = stall
            task = asyncio.create_task(ddi.prepare_cryptex_ddi(rsd))
            await started.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task


class PreparationTests(unittest.IsolatedAsyncioTestCase):
    def session(self, transport='usb'):
        return bridge.TouchSession(SimpleNamespace(emit=AsyncMock()), 120, 'test-device', transport)

    def lockdown(self, version='27.0.1'):
        return SimpleNamespace(udid='test-device', product_version=version,
                               get_developer_mode_status=AsyncMock(return_value=True), close=AsyncMock())

    async def test_new_ios_uses_cryptex_and_old_ios_preserves_legacy(self):
        for version, cryptex in [('27.0.1', True), ('28.0', False), ('26.0.1', False)]:
            session = self.session()
            with self.subTest(version=version), \
                 patch.object(session, '_prepare_cryptex_developer_image', AsyncMock()) as prepare, \
                 patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=True)) as legacy:
                await session._preflight_developer_environment(self.lockdown(version))
                self.assertEqual(prepare.await_count, int(cryptex))
                self.assertEqual(legacy.await_count, int(not cryptex))
                self.assertEqual(session._ddi_was_mounted, not cryptex)

    async def test_wireless_usb_preparation_does_not_use_legacy_image(self):
        session = self.session('wireless')
        lockdown = self.lockdown()
        with patch.object(session, '_create_lockdown_with_retry', AsyncMock(return_value=lockdown)) as create, \
             patch.object(session, '_prepare_cryptex_developer_image', AsyncMock()) as prepare, \
             patch.object(session, '_refresh_personalized_ddi', AsyncMock()) as refresh:
            await session._prepare_ddi_for_remote_pairing()
            create.assert_awaited_once_with('USB')
            prepare.assert_awaited_once_with(lockdown)
            refresh.assert_not_awaited()
            lockdown.close.assert_awaited_once()

    async def test_prepared_wireless_phone_checks_hid_without_legacy_mount(self):
        session = self.session('wireless')
        with patch.object(session, '_prepare_cryptex_developer_image', AsyncMock()) as prepare, \
             patch.object(session, '_is_personalized_ddi_mounted', AsyncMock()) as legacy:
            await session._preflight_developer_environment(self.lockdown())
            prepare.assert_not_awaited()
            legacy.assert_not_awaited()

    async def test_cryptex_rejection_preserves_pre2_personalized_preparation(self):
        for mounted in (False, True):
            session = self.session()
            lockdown = self.lockdown()
            rejected = ddi.BridgePrerequisiteError('developer_image_mount_failed', 'Cryptex rejected')
            with self.subTest(mounted=mounted), \
                 patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=rejected)), \
                 patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=mounted)) as inventory, \
                 patch.object(session, '_mount_personalized_ddi', AsyncMock()) as mount:
                await session._preflight_developer_environment(lockdown)
                inventory.assert_awaited_once_with(lockdown)
                self.assertEqual(mount.await_count, int(not mounted))
                self.assertTrue(session._ddi_uses_cryptex)

    async def test_successful_personalized_fallback_is_kept_across_hid_retries(self):
        for mounted in (False, True):
            session = self.session()
            lockdown = self.lockdown()
            with self.subTest(mounted=mounted), \
                 patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=
                    ddi.BridgePrerequisiteError('developer_image_mount_failed', 'rejected'))) as cryptex, \
                 patch.object(session, '_is_personalized_ddi_mounted',
                              AsyncMock(side_effect=[mounted, True, True])) as inventory, \
                 patch.object(session, '_mount_personalized_ddi', AsyncMock()) as mount:
                await session._preflight_developer_environment(lockdown)
                await session._preflight_developer_environment(lockdown)
                session._recovering = True
                await session._preflight_developer_environment(lockdown)
                cryptex.assert_awaited_once()
                self.assertEqual(inventory.await_count, 3)
                self.assertEqual(mount.await_count, int(not mounted))

    async def test_failed_or_cancelled_personalized_fallback_does_not_commit_route(self):
        for failure in (asyncio.CancelledError(),
                        ddi.BridgePrerequisiteError('apple_connection_lost', 'disconnected')):
            session = self.session()
            lockdown = self.lockdown()
            with self.subTest(failure=type(failure).__name__), \
                 patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=[
                    ddi.BridgePrerequisiteError('developer_image_mount_failed', 'rejected'), None])) as cryptex, \
                 patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=False)), \
                 patch.object(session, '_mount_personalized_ddi', AsyncMock(side_effect=failure)):
                with self.assertRaises(type(failure)):
                    await session._preflight_developer_environment(lockdown)
                await session._preflight_developer_environment(lockdown)
                self.assertEqual(cryptex.await_count, 2)

    async def test_cryptex_fallback_is_limited_to_image_and_service_compatibility(self):
        fallback_codes = {'developer_image_mount_failed', 'developer_image_bundle_invalid',
                          'developer_image_download_incompatible', 'developer_image_tss_rejected',
                          'developer_image_service_unavailable'}
        terminal_codes = {'apple_device_locked', 'apple_device_not_trusted', 'apple_connection_lost',
                          'device_identity_mismatch', 'developer_image_tss_timeout',
                          'developer_image_tss_unavailable', 'developer_image_service_timeout',
                          'developer_image_upload_timeout', 'developer_image_verification_failed',
                          'developer_image_download_failed'}
        for code in fallback_codes | terminal_codes:
            session = self.session()
            error = ddi.BridgePrerequisiteError(code, 'synthetic failure')
            with self.subTest(code=code), \
                 patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=error)), \
                 patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=True)) as legacy:
                if code in fallback_codes:
                    await session._preflight_developer_environment(self.lockdown())
                    legacy.assert_awaited_once()
                else:
                    with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                        await session._preflight_developer_environment(self.lockdown())
                    self.assertIs(raised.exception, error)
                    legacy.assert_not_awaited()

    async def test_cancelled_cryptex_does_not_start_legacy_fallback(self):
        session = self.session()
        with patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=asyncio.CancelledError)), \
             patch.object(session, '_is_personalized_ddi_mounted', AsyncMock()) as legacy:
            with self.assertRaises(asyncio.CancelledError):
                await session._preflight_developer_environment(self.lockdown())
            legacy.assert_not_awaited()

    async def test_real_preparation_tunnel_failures_do_not_try_another_image(self):
        for error, code in [(TimeoutError(), 'developer_image_service_timeout'),
                            (bridge.ConnectionFailedError(), 'apple_connection_lost')]:
            session = self.session()
            with self.subTest(error=type(error).__name__), \
                 patch.object(bridge.CoreDeviceTunnelProxy, 'create', AsyncMock(side_effect=error)), \
                 patch.object(session, '_is_personalized_ddi_mounted', AsyncMock()) as legacy:
                with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                    await session._preflight_developer_environment(self.lockdown())
                self.assertEqual(raised.exception.code, code)
                self.assertIs(raised.exception.__cause__, error)
                legacy.assert_not_awaited()

    async def test_legacy_fallback_shares_original_preparation_deadline(self):
        session = self.session()
        session._ddi_deadline = asyncio.get_running_loop().time() + .02
        deadline = session._ddi_deadline
        async def stalled(_):
            await asyncio.Event().wait()
        with patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=
                ddi.BridgePrerequisiteError('developer_image_mount_failed', 'Cryptex rejected'))), \
             patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=False)), \
             patch.object(session, '_mount_personalized_ddi', stalled):
            with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                await asyncio.wait_for(session._preflight_developer_environment(self.lockdown()), .5)
        self.assertEqual(raised.exception.code, 'developer_image_prepare_timeout')
        self.assertEqual(session._ddi_deadline, deadline)

    async def test_wireless_usb_fallback_never_unmounts_existing_image(self):
        session = self.session('wireless')
        lockdown = self.lockdown()
        with patch.object(session, '_create_lockdown_with_retry', AsyncMock(return_value=lockdown)), \
             patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=
                ddi.BridgePrerequisiteError('developer_image_mount_failed', 'Cryptex rejected'))), \
             patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=True)), \
             patch.object(session, '_refresh_personalized_ddi', AsyncMock()) as refresh:
            await session._prepare_ddi_for_remote_pairing()
            refresh.assert_not_awaited()
            lockdown.close.assert_awaited_once()

    async def test_fallback_starts_only_after_preparation_tunnel_closes(self):
        session = self.session()
        lockdown = self.lockdown()
        service = SimpleNamespace(close=AsyncMock())
        tun = SimpleNamespace(set_peer=Mock())
        result = SimpleNamespace(client=SimpleNamespace(tun=tun), address='::1', port=1234)
        rsd = SimpleNamespace(udid=lockdown.udid, __aenter__=AsyncMock(), close=AsyncMock())
        order = []
        @contextlib.asynccontextmanager
        async def tunnel(_):
            try:
                yield result
            finally:
                self.assertEqual(rsd.close.await_count, 1)
                order.append('tunnel_closed')
        @contextlib.asynccontextmanager
        async def dial(*_):
            yield SimpleNamespace(dial=AsyncMock())
        async def close_remote(remote, stage):
            await remote.close()
        async def inventory(_):
            self.assertEqual(order, ['tunnel_closed'])
            service.close.assert_awaited_once()
            return True
        with patch.object(bridge.CoreDeviceTunnelProxy, 'create', AsyncMock(return_value=service)), \
             patch.object(session, '_bounded_tunnel', tunnel), \
             patch.object(userspace_tunnel, 'UserspaceDialPlane', dial), \
             patch.object(bridge, 'RemoteServiceDiscoveryService', Mock(return_value=rsd)), \
             patch.object(session, '_close_remote_service', close_remote), \
             patch.object(bridge, 'prepare_cryptex_ddi', AsyncMock(side_effect=
                ddi.BridgePrerequisiteError('developer_image_mount_failed', 'Cryptex rejected'))), \
             patch.object(session, '_is_personalized_ddi_mounted', inventory):
            await session._preflight_developer_environment(lockdown)
        lockdown.close.assert_not_awaited()

    async def test_failed_legacy_fallback_retains_both_failure_codes(self):
        session = self.session()
        rejected = ddi.BridgePrerequisiteError('developer_image_mount_failed', 'Cryptex rejected')
        incompatible = ddi.BridgePrerequisiteError('developer_image_download_incompatible', 'legacy incompatible')
        with patch.object(session, '_prepare_cryptex_developer_image', AsyncMock(side_effect=rejected)), \
             patch.object(session, '_is_personalized_ddi_mounted', AsyncMock(return_value=False)), \
             patch.object(session, '_mount_personalized_ddi', AsyncMock(side_effect=incompatible)):
            with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                await session._preflight_developer_environment(self.lockdown())
            self.assertIs(raised.exception, incompatible)
        warnings = [call.args[0] for call in session.ipc.emit.call_args_list
                    if call.args[0].get('event') == 'warning']
        self.assertTrue(any('developer_image_mount_failed' in item.get('message', '') for item in warnings))

    async def test_cryptex_fallback_never_unmounts_existing_image_during_hid_recovery(self):
        session = self.session()
        session._ddi_uses_cryptex = session._ddi_was_mounted = True
        with patch.object(session, '_connect_with_lockdown', AsyncMock(side_effect=[
                ddi.BridgePrerequisiteError('touch_surface_unavailable', 'missing'), None])), \
             patch.object(session, '_refresh_personalized_ddi', AsyncMock()) as refresh:
            await session._connect_with_ddi_recovery(self.lockdown())
            refresh.assert_not_awaited()

    async def test_missing_wireless_hid_prepares_usb_then_retries_wireless(self):
        session = self.session('wireless')
        session._ddi_uses_cryptex = True
        with patch.object(session, '_connect_with_lockdown', AsyncMock(side_effect=[
                ddi.BridgePrerequisiteError('touch_surface_unavailable', 'missing'), None])) as connect, \
             patch.object(session, '_prepare_ddi_for_remote_pairing', AsyncMock()) as prepare:
            await session._connect_with_ddi_recovery(self.lockdown())
            prepare.assert_awaited_once()
            self.assertEqual(connect.await_count, 2)
            self.assertEqual(session.transport_mode, 'wireless')

    async def test_preparation_closes_rsd_tunnel_and_service_before_hid(self):
        for wrong_rsd in (False, True):
            session = self.session()
            lockdown = self.lockdown()
            service = SimpleNamespace(close=AsyncMock())
            tun = SimpleNamespace(set_peer=Mock())
            result = SimpleNamespace(client=SimpleNamespace(tun=tun), address='::1', port=1234)
            rsd = SimpleNamespace(udid='other-device' if wrong_rsd else 'test-device',
                                  __aenter__=AsyncMock(), close=AsyncMock())
            order = []
            @contextlib.asynccontextmanager
            async def tunnel(_):
                try:
                    yield result
                finally:
                    self.assertEqual(rsd.close.await_count, 1)
                    order.append('tunnel_closed')
            @contextlib.asynccontextmanager
            async def dial(*_):
                yield SimpleNamespace(dial=AsyncMock())
            async def close_remote(service, stage):
                await service.close()
            with self.subTest(wrong_rsd=wrong_rsd), \
                 patch.object(bridge.CoreDeviceTunnelProxy, 'create', AsyncMock(return_value=service)), \
                 patch.object(session, '_bounded_tunnel', tunnel), \
                 patch.object(userspace_tunnel, 'UserspaceDialPlane', dial), \
                 patch.object(bridge, 'RemoteServiceDiscoveryService', Mock(return_value=rsd)), \
                 patch.object(session, '_close_remote_service',
                              side_effect=close_remote), \
                 patch.object(bridge, 'prepare_cryptex_ddi', AsyncMock()) as prepare:
                if wrong_rsd:
                    with self.assertRaises(ddi.BridgePrerequisiteError) as raised:
                        await session._prepare_cryptex_developer_image(lockdown)
                    self.assertEqual(raised.exception.code, 'device_identity_mismatch')
                    prepare.assert_not_awaited()
                else:
                    await session._prepare_cryptex_developer_image(lockdown)
                    prepare.assert_awaited_once_with(rsd, session._emit_status)
                service.close.assert_awaited_once()
                lockdown.close.assert_not_awaited()
                self.assertEqual(order, ['tunnel_closed'])


if __name__ == '__main__':
    unittest.main()
