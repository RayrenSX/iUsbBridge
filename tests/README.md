# Python bridge regression tests

These tests cover the Python compatibility backend in `src/`, including HID
reports, IPC, clipboard operations, USBMux recovery, DDI/Cryptex lifecycle,
wireless discovery, Device Hub actions, and document file operations. They were
synchronized from the implementation maintained in iPhoneMirror.

Install the root `requirements.txt` in a Python 3.13 environment, then run from
the repository root:

```powershell
python -m unittest discover -s tests -p "*_test.py"
```

The suite uses mocked device services and does not require a connected phone.
Some download tests use local HTTP servers. Optional decoder integration tests
skip when `dist/CoreDevice/iPhoneMirror.MediaDecoder.exe` is absent; the decoder
is an external iPhoneMirror component and is not built by the Python bridge.

Application UI, C# host, release payload, SDK, and iPhoneMirror source-staging
tests are outside this repository's scope. The separately preserved Rust branch
uses Cargo tests.
