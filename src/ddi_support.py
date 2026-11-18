"""Cancellable Personalized DDI operations for the pinned pymobiledevice3 runtime.

The upstream TSS client uses requests without a timeout inside an executor whose
context manager joins its thread on cancellation. Do not use that transport in
the bridge: a stalled Apple request must not stop IPC or USB recovery.
"""
from __future__ import annotations

import asyncio
import copy
import contextlib
import hashlib
import logging
import os
import plistlib
import sys
import tempfile
import time
from contextlib import asynccontextmanager
from pathlib import Path
from xml.parsers.expat import ExpatError

import httpx
from pymobiledevice3.exceptions import (
    NoSuchBuildIdentityError, AlreadyMountedError, MissingManifestError,
    DeveloperModeIsNotEnabledError, GetProhibitedError, NotPairedError,
    PasswordRequiredError, DeviceNotFoundError, ConnectionFailedToUsbmuxdError,
    ConnectionTerminatedError, MuxException, InvalidServiceError,
)
from pymobiledevice3.restore.tss import TSSRequest, TSS_CONTROLLER_ACTION_URL
from pymobiledevice3.services.mobile_image_mounter import PersonalizedImageMounter as UpstreamMounter

log = logging.getLogger('iphoneMirror.ddi')
# Preserve Apple's upstream signing endpoint. gs.apple.com's HTTPS listener
# does not provide a publicly trusted certificate on every route. The returned
# Apple ticket is verified by the device; never disable TLS verification.
TSS_URL = TSS_CONTROLLER_ACTION_URL
TSS_TIMEOUT_SECONDS = 40
SERVICE_TIMEOUT_SECONDS = 15
UPLOAD_TIMEOUT_SECONDS = 90
MOUNT_TIMEOUT_SECONDS = 45
CLOSE_TIMEOUT_SECONDS = 3
MAX_TSS_RESPONSE_BYTES = 4 * 1024 * 1024
TSS_ATTEMPTS = 2
TSS_RETRY_DELAY_SECONDS = 1


class BridgePrerequisiteError(RuntimeError):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


def device_failure(error):
    """Preserve actionable device errors across DDI candidate fallback."""
    for types, code in (
        ((ConnectionFailedToUsbmuxdError,), 'apple_usbmux_unavailable'),
        ((GetProhibitedError, NotPairedError), 'apple_device_not_trusted'),
        ((PasswordRequiredError,), 'apple_device_locked'),
        ((DeviceNotFoundError,), 'apple_device_not_found'),
        ((DeveloperModeIsNotEnabledError,), 'developer_mode_required'),
        ((ConnectionTerminatedError, MuxException, ConnectionError), 'apple_connection_lost'),
        ((InvalidServiceError,), 'developer_image_service_unavailable'),
    ):
        if isinstance(error, types):
            return BridgePrerequisiteError(code, f'DDI device operation failed ({type(error).__name__}).')
    return None


@asynccontextmanager
async def bounded_mounter(mounter):
    # Bound entering AND leaving the service, not just mount(). A failed open
    # may still own a connection, so always close even when __aenter__ fails.
    try:
        try:
            async with asyncio.timeout(SERVICE_TIMEOUT_SECONDS):
                await mounter.__aenter__()
        except TimeoutError as error:
            raise BridgePrerequisiteError('developer_image_service_timeout',
                                          'Opening the image-mounter service timed out.') from error
        except Exception as error:
            raise device_failure(error) or BridgePrerequisiteError(
                'developer_image_service_unavailable',
                f'Opening the image-mounter service failed ({type(error).__name__}).') from error
        yield mounter
    finally:
        # The pinned SDK clears ServiceConnection.writer/socket in a finally
        # block even if cancellation interrupts wait_closed before socket.close.
        # Retain only this mounter's connection, never the parent Lockdown one.
        service = getattr(mounter, '_service', None)
        writer = getattr(service, 'writer', None)
        sock = getattr(service, 'socket', None)
        closed = False
        try:
            async with asyncio.timeout(CLOSE_TIMEOUT_SECONDS):
                await mounter.__aexit__(None, None, None)
            closed = True
        except Exception as error:
            log.warning('DDI service cleanup failed: %s', type(error).__name__)
        finally:
            if not closed:
                if writer is not None:
                    with contextlib.suppress(Exception):
                        writer.transport.abort()
                if sock is not None:
                    with contextlib.suppress(Exception):
                        sock.close()


def personalization_request(build_manifest, identifiers, ecid, nonce):
    """Build the same DDI ticket request as pymobiledevice3 11.3.1.

    Keep Apple's identity selection and restore rules; replace only the HTTP
    transport. Never log this payload (it contains device identifiers/nonces).
    """
    board, chip = identifiers['BoardId'], identifiers['ChipID']
    identity = next((item for item in build_manifest['BuildIdentities']
                     if int(item['ApBoardID'], 0) == board and
                     int(item['ApChipID'], 0) == chip), None)
    if identity is None:
        raise NoSuchBuildIdentityError('No DDI identity matches this device.')
    request = TSSRequest()
    payload = {key: value for key, value in identifiers.items() if key.startswith('Ap,')}
    payload.update({
        '@ApImg4Ticket': True, '@BBTicket': True, 'ApBoardID': board,
        'ApChipID': chip, 'ApECID': ecid, 'ApNonce': nonce,
        'ApProductionMode': True, 'ApSecurityDomain': 1, 'ApSecurityMode': True,
        'SepNonce': bytes(20), 'UID_MODE': False,
    })
    parameters = {'ApProductionMode': True, 'ApSecurityDomain': 1,
                  'ApSecurityMode': True, 'ApSupportsImg4': True}
    manifest = identity['Manifest']
    rules = manifest.get('LoadableTrustCache', {}).get('Info', {}).get('RestoreRequestRules', [])
    for key, entry in manifest.items():
        if entry.get('Info') is None or not entry.get('Trusted', False):
            continue
        value = copy.deepcopy(entry)
        value.pop('Info')
        if rules:
            value = request.apply_restore_request_rules(value, parameters, rules)
        if entry.get('Digest') is None:
            value['Digest'] = b''
        payload[key] = value
    request.update(payload)
    return plistlib.dumps(request._request)


async def _request_ticket_once(payload: bytes, headers, ticket_keys=('ApImg4Ticket',)) -> bytes:
    async with httpx.AsyncClient(timeout=httpx.Timeout(18, connect=8),
                                 follow_redirects=False) as client:
        async with client.stream('POST', TSS_URL, headers=headers, content=payload) as response:
            if response.status_code in (408, 429, 500, 502, 503, 504):
                raise BridgePrerequisiteError('developer_image_tss_unavailable',
                                              f'Apple signing service returned HTTP {response.status_code}.')
            response.raise_for_status()
            content = bytearray()
            async for chunk in response.aiter_bytes():
                content.extend(chunk)
                if len(content) > MAX_TSS_RESPONSE_BYTES:
                    raise ValueError('TSS response exceeds the size limit')
    envelope, separator, document = bytes(content).partition(b'REQUEST_STRING=')
    fields = dict(part.split(b'=', 1) for part in envelope.split(b'&') if b'=' in part)
    status = fields.get(b'STATUS', b'0')
    if status == b'93':
        raise BridgePrerequisiteError('developer_image_tss_unavailable',
                                      'Apple signing service reported an internal error (93).')
    if status != b'0':
        raise BridgePrerequisiteError('developer_image_tss_rejected',
                                      'Apple rejected personalization of this developer image.')
    if fields.get(b'MESSAGE') != b'SUCCESS' or not separator:
        raise ValueError('Apple did not return a successful personalization response')
    result = plistlib.loads(document)
    for key in ticket_keys:
        ticket = result.get(key)
        if isinstance(ticket, bytes) and ticket:
            return ticket
    raise ValueError('Apple returned no personalization ticket')


async def request_ticket(payload: bytes, ticket_keys=('ApImg4Ticket',)) -> bytes:
    headers = {'Cache-Control': 'no-cache', 'Content-Type': 'text/xml; charset="utf-8"',
               'User-Agent': 'InetURL/1.0'}
    try:
        # Wall-clock bound also covers slow trickle responses and connection
        # setup. Async sockets cancel without joining a blocking worker thread.
        async with asyncio.timeout(TSS_TIMEOUT_SECONDS):
            for attempt in range(TSS_ATTEMPTS):
                try:
                    return await _request_ticket_once(payload, headers, ticket_keys)
                except BridgePrerequisiteError as error:
                    if error.code != 'developer_image_tss_unavailable' or attempt + 1 == TSS_ATTEMPTS:
                        raise
                except (httpx.TransportError, TimeoutError):
                    if attempt + 1 == TSS_ATTEMPTS:
                        raise
                log.info('Apple signing request retry=%d/%d', attempt + 1, TSS_ATTEMPTS - 1)
                await asyncio.sleep(TSS_RETRY_DELAY_SECONDS)
    except (TimeoutError, httpx.TimeoutException) as error:
        raise BridgePrerequisiteError('developer_image_tss_timeout',
                                      'Apple DDI personalization timed out; check access to gs.apple.com.') from error
    except (httpx.HTTPError, ValueError, TypeError, AttributeError, plistlib.InvalidFileException, ExpatError) as error:
        # Do not copy proxy URLs, response bodies, identifiers or tickets into logs.
        raise BridgePrerequisiteError('developer_image_tss_failed',
                                      f'Apple DDI personalization failed ({type(error).__name__}).') from error


# Same immutable upstream snapshot as the legacy downloader. Cryptex's generic
# identity does not require a BoardID/ChipID entry for each new iPhone model.
CRYPTEX_DDI_REVISION = '6eae353ae694bda1c421d4a3eee5459ae59c99a1'
CRYPTEX_DDI_BUILD = '27A5228h'
CRYPTEX_DDI_FILES = {
    'BuildManifest.plist': (804946, '27385d7582b03b36bb3104e22b520aee0c47d72fecb4e8ecfe12ef5d966c7012'),
    'Image.dmg': (15895040, '873097f695a8b9734e2abc54f795a8874d40ff6fd11208ecb01ef29534c7c176'),
    'Image.dmg.trustcache': (1895, 'f7f21986074eee03a215aca16ecfc78d6bf183600d8a0d2fb691f9896782e6f0'),
    'Image.dmg.cryptex_info': (430, 'edf49aef55aacc063d4d7be05b713bb545ce2993b3f62bcc15eccd75e610ee6c'),
    'Image.dmg.root_hash': (229, '3543fad2805b88119695c417e12679380b3b5a2742994bbcc839c8e2de5d7302'),
}
CRYPTEX_PAYLOAD_PATHS = {
    'Cryptex1,GenericDmg': 'Image.dmg',
    'Cryptex1,GenericTrustCache': 'Image.dmg.trustcache',
    'Cryptex1,CryptexInfoPlist': 'Image.dmg.cryptex_info',
    'Cryptex1,GenericVolume': 'Image.dmg.root_hash',
}


class _CryptexConnection:
    """Own one SDK-created RemoteXPC channel, including partial connect.

    CryptexdService opens a fresh channel per routine. Its connect precedes
    its finally block, and its close has no deadline. Interpose only the
    transport lifecycle; leave Apple's requests/file transfers in the SDK.
    """
    def __init__(self, connection):
        self._connection = connection
        self._closed = False
        self._sdk_close = connection.close
        # RemoteXPC.connect also calls self.close on handshake exceptions.
        # Bound that internal path on this owned instance, never globally.
        connection.close = self.close

    def __getattr__(self, name):
        return getattr(self._connection, name)

    async def connect(self):
        try:
            await self._connection.connect()
        except BaseException:
            await self.close()
            raise

    async def close(self):
        if self._closed:
            return
        self._closed = True
        # SDK connect() calls close from its exception handler. If the outer
        # operation deadline expires during that cleanup, retain the original
        # connection failure instead of replacing it with a service timeout.
        original_error = sys.exception()
        connection = self._connection
        # Keep the handles before SDK cleanup can clear its attributes.
        writer = getattr(connection, '_writer', None)
        reader_task = getattr(connection, '_file_chunk_reader_task', None)
        completed = False
        try:
            async with asyncio.timeout(CLOSE_TIMEOUT_SECONDS):
                if reader_task is not None:
                    # The SDK suppresses CancelledError around this await,
                    # including cancellation from our deadline. Join it here
                    # so that cancellation cannot be consumed before close.
                    connection._file_chunk_reader_task = None
                    reader_task.cancel()
                    await asyncio.gather(reader_task, return_exceptions=True)
                await self._sdk_close()
            completed = True
        except asyncio.CancelledError:
            if isinstance(original_error, Exception):
                raise original_error
            raise
        except Exception as error:
            # Never replace a request/handshake failure with cleanup noise.
            log.warning('Cryptex channel cleanup failed: %s', type(error).__name__)
        finally:
            if not completed:
                if reader_task is not None:
                    reader_task.cancel()
                if writer is not None:
                    with contextlib.suppress(Exception):
                        writer.transport.abort()
                connection._writer = connection._reader = None


class _CryptexRsd:
    """Borrow the selected RSD, own only its Cryptex child connections."""
    def __init__(self, rsd):
        self._rsd = rsd
        self._connections = []

    def __getattr__(self, name):
        return getattr(self._rsd, name)

    def start_remote_service(self, name):
        connection = _CryptexConnection(self._rsd.start_remote_service(name))
        self._connections.append(connection)
        return connection

    async def close(self):
        cancelled = None
        for connection in self._connections:
            try:
                await connection.close()
            except asyncio.CancelledError as error:
                # Finish the other owned channels, then deliver cancellation.
                cancelled = error
        self._connections.clear()
        if cancelled is not None:
            raise cancelled


async def _cryptex_operation(operation, timeout, error_code):
    try:
        async with asyncio.timeout(timeout):
            return await operation
    except TimeoutError as error:
        raise BridgePrerequisiteError(error_code, 'Cryptex DDI device operation timed out.') from error


def validate_cryptex_bundle(root: Path) -> None:
    try:
        for name, (size, digest) in CRYPTEX_DDI_FILES.items():
            data = (root / name).read_bytes()
            if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
                raise ValueError('Cryptex DDI size/hash mismatch')
        manifest = plistlib.loads((root / 'BuildManifest.plist').read_bytes())
        if manifest.get('ProductBuildVersion') != CRYPTEX_DDI_BUILD:
            raise ValueError('Cryptex DDI build mismatch')
        identity = next(item for item in manifest['BuildIdentities']
                        if item.get('Info', {}).get('Variant', '').endswith('Developer Disk Image Cryptex'))
        for key, name in CRYPTEX_PAYLOAD_PATHS.items():
            entry = identity['Manifest'][key]
            if entry['Info']['Path'] != name or \
                    hashlib.sha384((root / name).read_bytes()).digest() != entry['Digest']:
                raise ValueError('Cryptex payload path/digest mismatch')
    except (OSError, KeyError, ValueError, TypeError, AttributeError, StopIteration,
            plistlib.InvalidFileException, ExpatError) as error:
        raise BridgePrerequisiteError('developer_image_bundle_invalid',
                                      f'Cryptex DDI validation failed ({type(error).__name__}).') from error


def bundled_cryptex_directory() -> Path:
    base = Path(sys.executable).resolve().parent if getattr(sys, 'frozen', False) else Path(__file__).resolve().parent
    return base / 'ddi' / 'Xcode_iOS_DDI_Cryptex'


async def fetch_cryptex_bundle() -> Path:
    bundled = bundled_cryptex_directory()
    if bundled.is_dir():
        try:
            validate_cryptex_bundle(bundled)
            return bundled
        except BridgePrerequisiteError:
            log.warning('Bundled Cryptex DDI failed validation; using verified cache/download.')
    root = Path(os.environ.get('LOCALAPPDATA') or tempfile.gettempdir()) / \
        'iPhoneMirror' / 'DDI' / 'Cryptex1' / CRYPTEX_DDI_REVISION
    try:
        root.mkdir(parents=True, exist_ok=True)
        async with asyncio.timeout(90), httpx.AsyncClient(timeout=httpx.Timeout(20, connect=8),
                                                        follow_redirects=False) as client:
            for name, (size, expected_hash) in CRYPTEX_DDI_FILES.items():
                target = root / name
                if target.is_file() and target.stat().st_size == size and \
                        hashlib.sha256(target.read_bytes()).hexdigest() == expected_hash:
                    continue
                urls = (
                    f'https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/{CRYPTEX_DDI_REVISION}/'
                    f'PersonalizedImages/Xcode_iOS_DDI_Cryptex/{name}',
                    'https://api.github.com/repos/doronz88/DeveloperDiskImage/contents/'
                    f'PersonalizedImages/Xcode_iOS_DDI_Cryptex/{name}?ref={CRYPTEX_DDI_REVISION}',
                )
                for index, url in enumerate(urls):
                    partial = None
                    try:
                        with tempfile.NamedTemporaryFile(dir=root, prefix=name + '.', suffix='.part', delete=False) as output:
                            partial = Path(output.name)
                            digest, received = hashlib.sha256(), 0
                            async with client.stream('GET', url, headers={
                                    'Accept': 'application/vnd.github.raw+json', 'Accept-Encoding': 'identity',
                                    'User-Agent': 'iUsbBridge/1.0'}) as response:
                                response.raise_for_status()
                                async for chunk in response.aiter_bytes():
                                    received += len(chunk)
                                    if received > size:
                                        raise ValueError('Cryptex download exceeds pinned size')
                                    output.write(chunk)
                                    digest.update(chunk)
                            if received != size or digest.hexdigest() != expected_hash:
                                raise ValueError('Cryptex download size/hash mismatch')
                        partial.replace(target)
                        break
                    except (httpx.HTTPError, ValueError):
                        if index + 1 == len(urls):
                            raise
                        log.info('Cryptex DDI raw download unavailable or invalid; trying the pinned GitHub API asset.')
                    finally:
                        if partial is not None:
                            partial.unlink(missing_ok=True)
    except TimeoutError as error:
        raise BridgePrerequisiteError('developer_image_download_timeout',
                                      'Cryptex DDI download exceeded its total time budget.') from error
    except (httpx.HTTPError, OSError, ValueError) as error:
        raise BridgePrerequisiteError('developer_image_download_failed',
                                      f'Cryptex DDI download failed ({type(error).__name__}).') from error
    validate_cryptex_bundle(root)
    return root


async def prepare_cryptex_ddi(rsd, report_status=None) -> str:
    """Use upstream Cryptex1 assets/nonce/TSS tags with bounded async signing.

    The device verifies Apple's ticket. Never roll a nonce or uninstall an
    existing DDI. HID readiness is checked on a fresh RSD by the caller.
    """
    from pymobiledevice3.services.cryptexd import CryptexdService, DDI_CRYPTEX_IDENTIFIER, load_cryptex1_assets
    if CryptexdService.SERVICE_NAME not in (rsd.peer_info or {}).get('Services', {}):
        raise BridgePrerequisiteError('developer_image_service_unavailable',
                                      'The device did not advertise the Cryptex DDI service.')
    owner = _CryptexRsd(rsd)
    service = CryptexdService(owner)

    async def phase(code):
        if report_status is not None:
            await report_status(code)

    async def inventory():
        installed = await _cryptex_operation(service.copy_installed(),
            SERVICE_TIMEOUT_SECONDS, 'developer_image_service_timeout')
        return [item for item in installed if item.identifier == DDI_CRYPTEX_IDENTIFIER]

    try:
        existing = await inventory()
        if existing:
            return existing[0].version
        await phase('downloading_developer_image')
        root = await fetch_cryptex_bundle()
        assets = load_cryptex1_assets(root)
        await phase('personalizing_developer_image')
        request = TSSRequest()
        identifiers = await _cryptex_operation(service.read_personalization_identifiers(),
            SERVICE_TIMEOUT_SECONDS, 'developer_image_service_timeout')
        nonce = await _cryptex_operation(service.cryptex_nonce(assets.nonce_domain),
            SERVICE_TIMEOUT_SECONDS, 'developer_image_service_timeout')
        request.add_cryptex1_tags(assets.build_identity, identifiers, nonce)
        ticket = await request_ticket(plistlib.dumps(request._request), ('Cryptex1,Ticket', 'ApImg4Ticket'))
        await phase('uploading_developer_image')
        await _cryptex_operation(service.install(assets.image, assets.trustcache, ticket,
            assets.info, assets.volumehash, assets.cryptex1_properties),
            UPLOAD_TIMEOUT_SECONDS, 'developer_image_upload_timeout')
        await phase('verifying_developer_image')
        for attempt in range(3):
            installed = await inventory()
            if installed:
                log.info('Cryptex DDI prepared version=%s', installed[0].version)
                return installed[0].version
            if attempt < 2:
                await asyncio.sleep(.3)
        raise BridgePrerequisiteError('developer_image_verification_failed',
                                      'Cryptex install returned without a published DDI inventory entry.')
    except BridgePrerequisiteError:
        raise
    except Exception as error:
        raise device_failure(error) or BridgePrerequisiteError(
            'developer_image_mount_failed', f'Cryptex DDI preparation failed ({type(error).__name__}).') from error
    finally:
        await owner.close()


class PersonalizedImageMounter(UpstreamMounter):
    report_status = None

    async def _phase(self, code, timeout, operation):
        started = time.monotonic()
        try:
            if self.report_status is not None:
                await self.report_status(code)
            async with asyncio.timeout(timeout):
                return await operation()
        except TimeoutError as error:
            error_code = {
                'checking_developer_image_ticket': 'developer_image_service_timeout',
                'personalizing_developer_image': 'developer_image_tss_timeout',
                'uploading_developer_image': 'developer_image_upload_timeout',
                'activating_developer_image': 'developer_image_mount_timeout',
            }[code]
            raise BridgePrerequisiteError(error_code,
                                          f'DDI phase {code} timed out after {timeout} seconds.') from error
        except (BridgePrerequisiteError, AlreadyMountedError, MissingManifestError):
            raise
        except Exception as error:
            known = device_failure(error)
            if known is not None:
                raise known from error
            error_code = {
                'checking_developer_image_ticket': 'developer_image_service_unavailable',
                'personalizing_developer_image': 'developer_image_tss_failed',
                'uploading_developer_image': 'developer_image_upload_failed',
                'activating_developer_image': 'developer_image_mount_failed',
            }[code]
            raise BridgePrerequisiteError(error_code,
                                          f'DDI phase {code} failed ({type(error).__name__}).') from error
        finally:
            log.info('DDI phase=%s elapsed_ms=%d', code, (time.monotonic() - started) * 1000)

    async def query_personalization_manifest(self, image_type, signature):
        return await self._phase('checking_developer_image_ticket', SERVICE_TIMEOUT_SECONDS,
                                lambda: super(PersonalizedImageMounter, self).query_personalization_manifest(
                                    image_type, signature))

    async def get_manifest_from_tss(self, build_manifest):
        async def personalize():
            try:
                async with asyncio.timeout(SERVICE_TIMEOUT_SECONDS):
                    identifiers = await self.query_personalization_identifiers()
                    nonce = await self.query_nonce('DeveloperDiskImage')
            except TimeoutError as error:
                raise BridgePrerequisiteError('developer_image_service_timeout',
                                              'Reading device personalization identifiers timed out.') from error
            try:
                payload = personalization_request(build_manifest, identifiers, self.lockdown.ecid, nonce)
            except (NoSuchBuildIdentityError, KeyError, ValueError, TypeError, AttributeError) as error:
                raise BridgePrerequisiteError('developer_image_download_incompatible',
                                              'The DDI build manifest does not support this device.') from error
            return await request_ticket(payload)
        return await self._phase('personalizing_developer_image', TSS_TIMEOUT_SECONDS + SERVICE_TIMEOUT_SECONDS,
                                personalize)

    async def upload_image(self, image_type, image, signature):
        return await self._phase('uploading_developer_image', UPLOAD_TIMEOUT_SECONDS,
                                lambda: super(PersonalizedImageMounter, self).upload_image(image_type, image, signature))

    async def mount_image(self, image_type, signature, extras=None):
        return await self._phase('activating_developer_image', MOUNT_TIMEOUT_SECONDS,
                                lambda: super(PersonalizedImageMounter, self).mount_image(image_type, signature, extras))
