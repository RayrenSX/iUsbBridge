"""Read SpringBoard direction over the existing capture usbmux interface.

This path does not mount a DDI, enable developer mode, pair, or send input.
The native parent owns its lifetime by keeping stdin open.
"""
import asyncio
import contextlib
import os
import sys

from pymobiledevice3.services.springboard import SpringBoardServicesService


def capture_mux_environment_key(udid):
    identity = udid.replace('-', '').upper()
    if not identity or any(c not in '0123456789ABCDEF' for c in identity):
        raise ValueError('invalid Apple device identity')
    return 'IPHONE_MIRROR_CAPTURE_MUX_' + identity


def capture_mux_address(value):
    if not isinstance(value, str) or not value.startswith('127.0.0.1:'):
        return None
    port = value[len('127.0.0.1:'):]
    if not port.isascii() or not port.isdigit() or not 1 <= int(port) <= 65535:
        return None
    return value


async def observe_orientation(bridge, udid):
    ipc = bridge.BridgeChannel()
    session = bridge.TouchSession(ipc, 120, udid=udid)
    lockdown = None

    async def observe():
        nonlocal lockdown
        await session._start_capture_mux(attempts=1)
        address = capture_mux_address(os.environ.get('USBMUXD_SOCKET_ADDRESS'))
        if not address:
            raise ConnectionError('capture usbmux is unavailable')
        lockdown = await asyncio.wait_for(bridge.create_using_usbmux(
            serial=udid, connection_type='USB', autopair=False), 5)
        if not bridge._udid_matches(lockdown.udid, udid):
            raise ConnectionError('orientation device identity mismatch')
        try:
            async with SpringBoardServicesService(lockdown) as springboard:
                print('IM_MUX ' + address.rsplit(':', 1)[1], flush=True)
                while True:
                    orientation = int(await asyncio.wait_for(
                        springboard.get_interface_orientation(), 1))
                    if orientation not in (1, 2, 3, 4):
                        raise ValueError('unknown SpringBoard interface orientation')
                    print(f'IM_ORIENTATION {orientation}', flush=True)
                    await asyncio.sleep(.1)
        except Exception:
            # Direction failure triggers ordinary B fallback in the parent.
            # This process may still own the broker used by reverse control;
            # keep that USB interface alive until the parent closes stdin.
            print('IM_ORIENTATION_UNAVAILABLE', flush=True)
            await asyncio.Future()

    observation = asyncio.create_task(observe())
    disconnected = asyncio.create_task(ipc.wait_for_disconnect())
    try:
        done, _ = await asyncio.wait((observation, disconnected), return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            await task
    finally:
        for task in (observation, disconnected):
            task.cancel()
        await asyncio.gather(observation, disconnected, return_exceptions=True)
        if lockdown is not None:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(lockdown.close(), 2)
        session._close_capture_mux()
        await ipc.close()
