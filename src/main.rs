use std::{
    collections::{HashMap, HashSet},
    env, io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use idevice::remote_pairing::{
    PeerDevice, RemotePairingClient, RemotePairingLockdownService, RpPairingFile, RpPairingSocket,
    connect_tls_psk_tunnel_native,
};
use idevice::{
    IdeviceService, RsdService,
    core_device::hid::{
        ButtonState, HidSurface, IndigoHidClient, TouchscreenContact, UniversalHidServiceClient,
        build_multitouch_report,
    },
    core_device::{
        CallInfoBlob, DisplayServiceClient, build_screen_audio_offer, build_screen_video_offer,
        build_start_audio_parameters, build_start_video_parameters,
    },
    core_device::{GENERAL_PASTEBOARD, PasteboardServiceClient},
    core_device_proxy::CoreDeviceProxy,
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    provider::IdeviceProvider,
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    tcp::handle::UdpSocketHandle,
    usbmuxd::{Connection, UsbmuxdAddr},
};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use uuid::Uuid;

mod raw_usbmux;

const SCHEMA: &str = "iphoneMirror.touch.v2";
const TOUCH_KIND: &str = "touch_batch";
const KEYBOARD_KIND: &str = "keyboard_batch";
const BUTTON_KIND: &str = "button_event";
const PASTE_TEXT_KIND: &str = "paste_text";
const COPY_SELECTION_KIND: &str = "copy_selection";
const READ_CLIPBOARD_KIND: &str = "read_clipboard";
const MAX_SLOTS: usize = 5;
const MAIN_TOUCHSCREEN: u64 = 257;
const DDI_REPOSITORY: &str = "doronz88/DeveloperDiskImage";
const DDI_DIRECTORY: &str = "PersonalizedImages/Xcode_iOS_DDI_Personalized";
const MODERN_UNIVERSAL_HID_SERVICE: &str = "com.apple.coredevice.hid.universalhidservice";
const MODERN_UNIVERSAL_HID_FEATURE: &str =
    "com.apple.coredevice.feature.remote.universalhidservice";
const LEGACY_UNIVERSAL_HID_SERVICE: &str = "com.apple.coredevice.hid.universalhid";
const LEGACY_UNIVERSAL_HID_FEATURE: &str = "com.apple.coredevice.feature.remote.universalhid";
const PASTEBOARD_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
const PASTEBOARD_CONTEXT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
// SET_REPLY confirms that PasteboardService accepted the request. Prefer an
// exact PULL readback before Command+V, but some iOS releases accept a write
// without exposing it through PULL to the same service connection.
const PASTEBOARD_WRITE_CONFIRM_DELAYS_MS: &[u64] = &[0, 80, 160, 320];
const REMOTE_PAIRING_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
// Background clipboard polling must never delay touch input, but a user-requested
// read, copy, or paste may wait behind one in-flight Pasteboard RPC.
const EXPLICIT_CLIPBOARD_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
// Universal HID reports normally complete in milliseconds. A shorter bound
// turns a dead device socket into a recoverable session rotation before the
// user perceives a multi-second input freeze.
const HID_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
const HID_HEALTH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const HID_HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
// RSD/CoreDevice can briefly delay service enumeration while the phone is
// handling a display, pasteboard, or USBMux operation. Do not tear down an
// otherwise usable session after one or two slow inventory calls. Wired
// direct-HID sessions skip this probe entirely because input send failures are
// a more reliable liveness signal for that transport.
const HID_HEALTH_CHECK_FAILURE_THRESHOLD: u8 = 6;

static RAW_CAPTURE_MUX: OnceLock<Mutex<Option<Arc<raw_usbmux::RawMux>>>> = OnceLock::new();

#[derive(Debug, Deserialize, Serialize)]
struct GithubCommit {
    sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GithubAsset {
    name: String,
    sha: String,
    size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct DdiCacheMetadata {
    commit: String,
    assets: Vec<GithubAsset>,
}

struct WirelessDdiPreflightError {
    code: &'static str,
    message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Usb,
    Network,
}

fn requested_transport() -> Transport {
    if env::args().any(|arg| arg == "--wireless") {
        Transport::Network
    } else {
        Transport::Usb
    }
}

fn requested_udid() -> Option<String> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--udid" {
            return args.next();
        }
    }
    None
}

fn requested_ddi_dir() -> Option<PathBuf> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--ddi-dir" {
            return args.next().map(PathBuf::from);
        }
    }
    env::var_os("IPHONE_MIRROR_DDI_DIR").map(PathBuf::from)
}

fn same_udid(left: &str, right: &str) -> bool {
    left.chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .flat_map(char::to_lowercase)
        .eq(right
            .chars()
            .filter(|ch| ch.is_ascii_hexdigit())
            .flat_map(char::to_lowercase))
}

fn requested_rate_hz() -> u32 {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--rate-hz" {
            return args
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(120);
        }
    }
    120
}

fn default_ddi_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("iPhoneMirror")
        .join("developer-image")
}

fn remote_pairing_file_path(device_udid: Option<&str>) -> PathBuf {
    if let Some(path) = env::var_os("IPHONE_MIRROR_RP_PAIRING_FILE") {
        return PathBuf::from(path);
    }
    let device = device_udid
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "default".into());
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("iPhoneMirror")
        .join("remote-pairing")
        .join(format!("{device}.plist"))
}

async fn remote_pairing_candidates()
-> Result<Vec<(String, PathBuf, RpPairingFile)>, Box<dyn std::error::Error>> {
    if env::var_os("IPHONE_MIRROR_RP_PAIRING_FILE").is_some() || requested_udid().is_some() {
        let requested = requested_udid();
        let path = remote_pairing_file_path(requested.as_deref());
        if let Ok(pairing) = RpPairingFile::read_from_file(&path).await {
            return Ok(vec![(requested.unwrap_or_default(), path, pairing)]);
        }
        // Cache filenames were historically written with whatever UDID
        // spelling the provider returned. Reuse an equivalent plist even when
        // the caller changes case or inserts/removes separators.
        if env::var_os("IPHONE_MIRROR_RP_PAIRING_FILE").is_none() {
            if let Some(requested_id) = requested.as_deref() {
                if let Some(directory) = path.parent() {
                    if let Ok(mut entries) = tokio::fs::read_dir(directory).await {
                        while let Some(entry) = entries.next_entry().await? {
                            let candidate = entry.path();
                            if candidate.extension().and_then(|value| value.to_str())
                                != Some("plist")
                                || !candidate
                                    .file_stem()
                                    .and_then(|value| value.to_str())
                                    .is_some_and(|stem| same_udid(stem, requested_id))
                            {
                                continue;
                            }
                            if let Ok(pairing) = RpPairingFile::read_from_file(&candidate).await {
                                return Ok(vec![(requested_id.to_owned(), candidate, pairing)]);
                            }
                        }
                    }
                }
            }
        }
        // Without a pairing record, mDNS does not reveal a trustworthy mapping
        // from the selected Apple UDID to a RemotePairing endpoint. The caller
        // must provision this credential over the already trusted USB lockdown
        // channel rather than attempting to pair the first LAN service it sees.
        return Err(format!(
            "RemotePairing pairing file is missing or invalid: {}",
            path.display()
        )
        .into());
    }

    let directory = remote_pairing_file_path(Some("placeholder"))
        .parent()
        .ok_or("RemotePairing cache has no parent directory")?
        .to_path_buf();
    let mut entries = tokio::fs::read_dir(&directory).await.map_err(|_| {
        format!(
            "RemotePairing pairing file is missing or invalid: {}",
            directory.display()
        )
    })?;
    let mut candidates = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("plist") {
            continue;
        }
        let Some(udid) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Ok(pairing) = RpPairingFile::read_from_file(&path).await {
            candidates.push((udid, path, pairing));
        }
    }
    if candidates.is_empty() {
        return Err(format!(
            "RemotePairing pairing file is missing or invalid: {}",
            directory.display()
        )
        .into());
    }
    Ok(candidates)
}

fn coredevice_error_code(message: &str) -> &'static str {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("developer mode") {
        "developer_mode_required"
    } else if normalized.contains("pairing file")
        || normalized.contains("not paired")
        || normalized.contains("pairing trust")
    {
        "apple_device_not_trusted"
    } else if normalized.contains("service not found") {
        "developer_image_required"
    } else if normalized.contains("maintouchscreen") {
        "touch_surface_unavailable"
    } else {
        "coredevice_connection_failed"
    }
}

fn device_connection_error_code(message: &str) -> &'static str {
    if message.contains("no matching") || message.contains("device not found") {
        "apple_device_not_found"
    } else {
        "apple_usbmux_unavailable"
    }
}

fn remote_pairing_error_code(message: &str) -> &'static str {
    if message.contains("pairing file is missing or invalid") {
        "wireless_remote_pairing_required"
    } else if message.contains("no RemotePairing mDNS service") {
        "wireless_device_not_discoverable"
    } else {
        "wireless_remote_pairing_failed"
    }
}

fn coredevice_error_message(message: &str) -> String {
    match coredevice_error_code(message) {
        "developer_image_required" => {
            "The CoreDevice service is unavailable after mounting the developer image.".into()
        }
        _ => message.chars().take(1024).collect(),
    }
}

fn git_blob_sha1(content: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

fn is_personalized_ddi_entry(entry: &plist::Value) -> bool {
    let Some(dict) = entry.as_dictionary() else {
        return false;
    };
    ["ImageType", "DiskImageType"]
        .iter()
        .filter_map(|key| dict.get(key).and_then(plist::Value::as_string))
        .any(|value| value.eq_ignore_ascii_case("Personalized"))
        || dict
            .get("PersonalizedImageType")
            .and_then(plist::Value::as_string)
            .is_some_and(|value| value.eq_ignore_ascii_case("DeveloperDiskImage"))
}

async fn http_json<T: serde::de::DeserializeOwned>(
    url: &str,
) -> Result<T, Box<dyn std::error::Error>> {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        idevice::http::HttpRequest::get(url)
            .header("User-Agent", "iPhoneMirror-idevice")
            .header("Accept", "application/vnd.github+json")
            .send(),
    )
    .await
    .map_err(|_| format!("GitHub request timed out for {url}"))??;
    if response.status != 200 {
        return Err(format!("GitHub returned HTTP {} for {url}", response.status).into());
    }
    Ok(serde_json::from_slice(&response.body)?)
}

async fn download_github_blob(asset: &GithubAsset) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.github.com/repos/{DDI_REPOSITORY}/git/blobs/{}",
        asset.sha
    );
    const CHUNK_SIZE: u64 = 1024 * 1024;
    const MAX_ATTEMPTS: usize = 3;
    let mut content = Vec::with_capacity(asset.size as usize);
    while (content.len() as u64) < asset.size {
        let start = content.len() as u64;
        let end = (start + CHUNK_SIZE - 1).min(asset.size - 1);
        let expected_len = (end - start + 1) as usize;
        let mut chunk = None;
        let mut last_error = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let request = idevice::http::HttpRequest::get(&url)
                .header("User-Agent", "iPhoneMirror-idevice")
                .header("Accept", "application/vnd.github.raw+json")
                .header("Range", format!("bytes={start}-{end}"))
                .send();
            match tokio::time::timeout(std::time::Duration::from_secs(180), request).await {
                Ok(Ok(response))
                    if response.status == 206 && response.body.len() == expected_len =>
                {
                    chunk = Some(response.body);
                    break;
                }
                Ok(Ok(response)) => {
                    last_error = format!(
                        "HTTP {} returned {} bytes, expected {expected_len}",
                        response.status,
                        response.body.len()
                    );
                }
                Ok(Err(error)) => last_error = error.to_string(),
                Err(_) => last_error = "request timed out".into(),
            }
            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
            }
        }
        let chunk = chunk.ok_or_else(|| {
            format!(
                "DDI chunk download failed for {} at bytes {start}-{end}: {last_error}",
                asset.name
            )
        })?;
        content.extend_from_slice(&chunk);
    }
    if content.len() as u64 != asset.size {
        return Err(format!(
            "DDI download size mismatch for {}: expected {}, got {}",
            asset.name,
            asset.size,
            content.len()
        )
        .into());
    }
    let actual_sha = git_blob_sha1(&content);
    if actual_sha != asset.sha {
        return Err(format!(
            "DDI integrity validation failed for {}: expected {}, got {}",
            asset.name, asset.sha, actual_sha
        )
        .into());
    }
    Ok(content)
}

async fn ensure_personalized_ddi(directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let expected = [
        ("Image.dmg", "Image.dmg"),
        ("BuildManifest.plist", "BuildManifest.plist"),
        ("Image.trustcache", "Image.dmg.trustcache"),
    ];
    if valid_cached_ddi(directory, &expected).await {
        return Ok(());
    }
    emit(Event::Status {
        code: "downloading_developer_image",
        message: "正在下载并校验 Personalized DDI",
    })?;
    let reference = env::var("IPHONE_MIRROR_DDI_GITHUB_REF").unwrap_or_else(|_| "main".into());
    let commit: GithubCommit = http_json(&format!(
        "https://api.github.com/repos/{DDI_REPOSITORY}/commits/{reference}"
    ))
    .await?;
    if commit.sha.len() != 40 || !commit.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("GitHub returned an invalid DDI revision".into());
    }
    let assets: Vec<GithubAsset> = http_json(&format!(
        "https://api.github.com/repos/{DDI_REPOSITORY}/contents/{DDI_DIRECTORY}?ref={}",
        commit.sha
    ))
    .await?;
    let parent = directory
        .parent()
        .ok_or("DDI cache directory has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let staging = parent.join(format!(".developer-image.download-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&staging).await?;
    let download_result = async {
        for (local, upstream) in expected {
            let asset = assets
                .iter()
                .find(|a| a.name == upstream)
                .ok_or_else(|| format!("DDI metadata is missing {upstream}"))?;
            if asset.size == 0 {
                return Err(format!("DDI metadata has invalid size for {upstream}").into());
            }
            let content = download_github_blob(asset).await?;
            tokio::fs::write(staging.join(local), content).await?;
        }
        let manifest = tokio::fs::read(staging.join("BuildManifest.plist")).await?;
        let _: plist::Value = plist::from_bytes(&manifest)?;
        let metadata = DdiCacheMetadata {
            commit: commit.sha.clone(),
            assets: assets.clone(),
        };
        tokio::fs::write(
            staging.join("idevice-ddi-metadata.json"),
            serde_json::to_vec_pretty(&metadata)?,
        )
        .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    if let Err(error) = download_result {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }

    let backup = parent.join(format!(".developer-image.backup-{}", Uuid::new_v4()));
    let had_existing = tokio::fs::try_exists(directory).await?;
    if had_existing {
        tokio::fs::rename(directory, &backup).await?;
    }
    if let Err(error) = tokio::fs::rename(&staging, directory).await {
        if had_existing {
            let _ = tokio::fs::rename(&backup, directory).await;
        }
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error.into());
    }
    if had_existing {
        let _ = tokio::fs::remove_dir_all(backup).await;
    }
    Ok(())
}

async fn verify_local_personalized_ddi(directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let expected = [
        ("Image.dmg", "Image.dmg"),
        ("BuildManifest.plist", "BuildManifest.plist"),
        ("Image.trustcache", "Image.dmg.trustcache"),
    ];
    if valid_cached_ddi(directory, &expected).await {
        Ok(())
    } else {
        Err(format!(
            "DDI directory is missing or failed integrity validation: {}",
            directory.display()
        )
        .into())
    }
}

async fn valid_cached_ddi(directory: &Path, expected: &[(&str, &str)]) -> bool {
    let metadata = match tokio::fs::read(directory.join("idevice-ddi-metadata.json")).await {
        Ok(bytes) => match serde_json::from_slice::<DdiCacheMetadata>(&bytes) {
            Ok(metadata) => metadata,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    if metadata.commit.len() != 40 || !metadata.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    for (local, upstream) in expected {
        let Some(asset) = metadata.assets.iter().find(|asset| asset.name == *upstream) else {
            return false;
        };
        let content = match tokio::fs::read(directory.join(local)).await {
            Ok(content) => content,
            Err(_) => return false,
        };
        if content.len() as u64 != asset.size || git_blob_sha1(&content) != asset.sha {
            return false;
        }
    }
    tokio::fs::read(directory.join("BuildManifest.plist"))
        .await
        .ok()
        .and_then(|bytes| plist::from_bytes::<plist::Value>(&bytes).ok())
        .is_some()
}

#[derive(Debug, Deserialize)]
struct InputFrame {
    schema: String,
    kind: String,
    #[serde(default)]
    points: Vec<InputPoint>,
    #[serde(default)]
    usages: Vec<u64>,
    #[serde(rename = "usagePage", default)]
    usage_page: u64,
    #[serde(rename = "usageCode", default)]
    usage_code: u64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(rename = "timestampNs", default)]
    timestamp_ns: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct InputPoint {
    #[serde(rename = "pointerId")]
    pointer_id: u32,
    action: String,
    #[serde(rename = "normalizedX")]
    normalized_x: f64,
    #[serde(rename = "normalizedY")]
    normalized_y: f64,
}

enum InputMessage {
    Frame(InputFrame),
    Error(String),
}

#[derive(Debug, Serialize)]
#[serde(tag = "event")]
enum Event {
    #[serde(rename = "status")]
    Status {
        code: &'static str,
        message: &'static str,
    },
    #[serde(rename = "ready")]
    Ready {
        protocol: u8,
        capabilities: [&'static str; 2],
        udid: String,
        #[serde(rename = "rateHz")]
        rate_hz: u32,
        #[serde(rename = "gateOpen")]
        gate_open: bool,
        #[serde(rename = "authMode")]
        auth_mode: &'static str,
        transport: &'static str,
    },
    #[serde(rename = "clipboard_text")]
    ClipboardText { text: Option<String> },
    #[serde(rename = "clipboard_paste_complete")]
    ClipboardPasteComplete { seq: Option<u64> },
    #[serde(rename = "clipboard_paste_failed")]
    ClipboardPasteFailed { seq: Option<u64>, message: String },
    #[serde(rename = "warning")]
    Warning { code: String, message: String },
    #[serde(rename = "wifi_sync_enabled")]
    WifiSyncEnabled { udid: String, changed: bool },
    #[serde(rename = "error")]
    Error { code: String, message: String },
}

#[derive(Debug, PartialEq, Eq)]
enum PasteboardWriteConfirmation {
    Confirmed,
    AcceptedButUnconfirmed(String),
}

struct TerminationGuard;

impl Drop for TerminationGuard {
    fn drop(&mut self) {
        let _ = emit(Event::Status {
            code: "terminated",
            message: "idevice 设备会话已结束",
        });
    }
}

struct Session {
    pasteboard: Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
    _adapter: AdapterHandle,
    _handshake: RsdHandshake,
    hid: UniversalHidServiceClient<Box<dyn idevice::ReadWrite>>,
    indigo: Arc<tokio::sync::Mutex<IndigoHidClient<Box<dyn idevice::ReadWrite>>>>,
    _display: Option<DisplayServiceClient<Box<dyn idevice::ReadWrite>>>,
    _audio_udp: Option<UdpSocketHandle>,
    _video_udp: Option<UdpSocketHandle>,
    auth_mode: &'static str,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "iUsbBridge.exe [--usb|--wireless] [--udid UDID] [--rate-hz HZ] [--ddi-dir DIRECTORY] [--enable-wifi-sync]"
        );
        return Ok(());
    }
    if env::args().any(|arg| arg == "--enable-wifi-sync") {
        if let Err(error) = enable_wifi_sync().await {
            emit(Event::Error {
                code: "wifi_sync_enable_failed".into(),
                message: error.to_string(),
            })?;
            std::process::exit(1);
        }
        return Ok(());
    }
    let _termination = TerminationGuard;
    emit(Event::Status {
        code: "connecting_device",
        message: "正在建立 idevice 设备会话",
    })?;

    let (session, actual_transport, actual_udid) =
        if matches!(requested_transport(), Transport::Network) {
            let explicit_ddi = requested_ddi_dir();
            let ddi_dir = explicit_ddi.clone().unwrap_or_else(default_ddi_dir);
            if explicit_ddi.is_none()
                && let Err(error) = ensure_personalized_ddi(&ddi_dir).await
            {
                emit(Event::Error {
                    code: "developer_image_download_failed".into(),
                    message: error.to_string(),
                })?;
                return Ok(());
            }
            emit(Event::Status {
                code: "discovering_wireless_device",
                message: "正在发现无线 CoreDevice 服务",
            })?;
            emit(Event::Status {
                code: "initializing_touch",
                message: "正在初始化 Universal HID",
            })?;
            // A previously mounted DDI remains usable after the cable is
            // removed. Prefer that validated wireless HID session so normal
            // wireless reconnects do not require USB on every application
            // start. USB is only needed when Universal HID is absent and a
            // DDI mount or refresh is actually required.
            if let Ok((session, udid)) =
                try_connect_existing_wireless_hid(requested_udid().as_deref()).await
            {
                (Ok(session), "wireless", udid)
            } else {
                if explicit_ddi.is_some()
                    && let Err(error) = verify_local_personalized_ddi(&ddi_dir).await
                {
                    emit(Event::Error {
                        code: "developer_image_bundle_invalid".into(),
                        message: error.to_string(),
                    })?;
                    return Ok(());
                }
                if let Err(error) =
                    prepare_wireless_personalized_ddi(requested_udid().as_deref(), &ddi_dir).await
                {
                    emit(Event::Error {
                        code: error.code.into(),
                        message: error.message,
                    })?;
                    return Ok(());
                }
                match remote_pairing_transport().await {
                    Ok((adapter, handshake, udid)) => (
                        connect_session_from_transport(adapter, handshake, true).await,
                        "wireless",
                        udid,
                    ),
                    Err(remote_error) => {
                        let remote_message = remote_error.to_string();
                        let mut retry_remote_pairing = false;
                        if remote_pairing_error_code(&remote_message)
                            == "wireless_remote_pairing_required"
                        {
                            // A fresh installation has no Wi-Fi pairing plist yet. The
                            // trusted USB lockdown channel can provision it without
                            // requiring the user to manually switch modes first.
                            match device_provider(Transport::Usb).await {
                                Ok((provider, usb_udid)) => {
                                    let requested = requested_udid();
                                    if requested
                                        .as_deref()
                                        .is_none_or(|value| same_udid(value, &usb_udid))
                                    {
                                        match provision_remote_pairing(&*provider, &usb_udid).await
                                        {
                                            Ok(()) => retry_remote_pairing = true,
                                            Err(error) => {
                                                emit(Event::Warning {
                                                    code:
                                                        "wireless_remote_pairing_provision_failed"
                                                            .into(),
                                                    message: error.to_string(),
                                                })?;
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    emit(Event::Warning {
                                        code: "wireless_remote_pairing_usb_unavailable".into(),
                                        message: error.to_string(),
                                    })?;
                                }
                            }
                        }
                        if retry_remote_pairing {
                            match remote_pairing_transport().await {
                                Ok((adapter, handshake, udid)) => (
                                    connect_session_from_transport(adapter, handshake, true).await,
                                    "wireless",
                                    udid,
                                ),
                                Err(error) => {
                                    emit(Event::Error {
                                        code: remote_pairing_error_code(&error.to_string()).into(),
                                        message: error.to_string(),
                                    })?;
                                    return Ok(());
                                }
                            }
                        } else {
                            match device_provider(Transport::Network).await {
                                Ok((provider, udid)) => {
                                    let result = connect_session(&*provider).await;
                                    (result, "wireless", udid)
                                }
                                Err(_) => {
                                    emit(Event::Error {
                                        code: remote_pairing_error_code(&remote_message).into(),
                                        message: remote_message,
                                    })?;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        } else {
            let (mut provider, udid) = match device_provider(Transport::Usb).await {
                Ok(provider) => provider,
                Err(error) => {
                    let message = error.to_string();
                    emit(Event::Error {
                        code: device_connection_error_code(&message).into(),
                        message,
                    })?;
                    return Ok(());
                }
            };
            // RemotePairing is provisioned over the already trusted USB link.
            // Keep this best-effort so wired HID remains usable on iOS builds
            // that do not expose remotepairingdeviced.
            if let Err(error) = provision_remote_pairing(&*provider, &udid).await {
                emit(Event::Warning {
                    code: "wireless_remote_pairing_provision_failed".into(),
                    message: error.to_string(),
                })?;
            }
            emit(Event::Status {
                code: "checking_developer_environment",
                message: "正在检查 Developer Mode 和 Personalized DDI",
            })?;
            let explicit_ddi = requested_ddi_dir();
            let ddi_dir = explicit_ddi.clone().unwrap_or_else(default_ddi_dir);
            let ddi_result = if explicit_ddi.is_some() {
                verify_local_personalized_ddi(&ddi_dir).await
            } else {
                ensure_personalized_ddi(&ddi_dir).await
            };
            if let Err(error) = ddi_result {
                emit(Event::Error {
                    code: if explicit_ddi.is_some() {
                        "developer_image_bundle_invalid"
                    } else {
                        "developer_image_download_failed"
                    }
                    .into(),
                    message: error.to_string(),
                })?;
                return Ok(());
            }
            emit(Event::Status {
                code: "initializing_touch",
                message: "正在初始化 Universal HID",
            })?;
            // Wired mirroring can hide the device from Apple's usbmuxd and
            // force this bridge through the raw QuickTime mux. If the DDI is
            // already active, mounting it again is both unnecessary and can
            // tear down that constrained device socket. Reuse the published
            // HID service first and only enter the mount path when it is not
            // available.
            let mut existing_session = connect_hid_session(&*provider).await;
            // A failed CoreDevice socket does not mean the DDI disappeared.
            // Rebuild the USB/HID transport first; mounting an already active
            // DDI can take ten seconds and may reset the capture interface.
            if existing_session
                .as_ref()
                .err()
                .is_some_and(|error| is_recoverable_usb_transport_error(&error.to_string()))
            {
                let mut last_transport_error = existing_session
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                for (attempt, delay_ms) in [100_u64, 250, 500, 1000].into_iter().enumerate() {
                    emit(Event::Warning {
                        code: "hid_session_transport_retry".into(),
                        message: format!(
                            "Universal HID 传输短暂断开，正在快速重建 USB 会话 {}/4: {}",
                            attempt + 1,
                            last_transport_error
                        ),
                    })?;
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    match device_provider(Transport::Usb).await {
                        Ok((fresh_provider, _)) => {
                            provider = fresh_provider;
                            existing_session = connect_hid_session(&*provider).await;
                            match existing_session.as_ref() {
                                Ok(_) => break,
                                Err(error)
                                    if is_recoverable_usb_transport_error(&error.to_string()) =>
                                {
                                    last_transport_error = error.to_string();
                                }
                                Err(_) => break,
                            }
                        }
                        Err(error) => last_transport_error = error.to_string(),
                    }
                }
                if existing_session
                    .as_ref()
                    .err()
                    .is_some_and(|error| is_recoverable_usb_transport_error(&error.to_string()))
                {
                    emit(Event::Error {
                        code: device_connection_error_code(&last_transport_error).into(),
                        message: last_transport_error,
                    })?;
                    return Ok(());
                }
            }
            if existing_session.is_err() {
                emit(Event::Status {
                    code: "mounting_developer_image",
                    message: "正在挂载 Personalized DDI",
                })?;
                if let Err(first_error) = mount_local_ddi(&*provider, &ddi_dir, false).await {
                    let first_message = first_error.to_string();
                    if is_recoverable_usb_transport_error(&first_message) {
                        emit(Event::Warning {
                            code: "developer_image_transport_retry".into(),
                            message: format!(
                                "DDI 挂载连接已重新枚举，正在重新建立 USB 通道: {first_message}"
                            ),
                        })?;
                        drop(provider);
                        // Do not tear down the capture mux merely because the
                        // DDI socket was re-enumerated. device_provider() will
                        // discard it if its reader is actually dead or if it
                        // no longer answers a usbmux request. Keeping a live
                        // mux here preserves the host-owned projection channel
                        // while the control provider is rebuilt.
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        match device_provider(Transport::Usb).await {
                            Ok((fresh_provider, _)) => provider = fresh_provider,
                            Err(error) => {
                                emit(Event::Error {
                                    code: device_connection_error_code(&error.to_string()).into(),
                                    message: error.to_string(),
                                })?;
                                return Ok(());
                            }
                        }
                    } else {
                        emit_developer_image_mount_error(first_message)?;
                        return Ok(());
                    }
                    if let Err(error) = mount_local_ddi(&*provider, &ddi_dir, false).await {
                        emit_developer_image_mount_error(error.to_string())?;
                        return Ok(());
                    }
                }
            }
            // RemotePairing is only a wireless transport prerequisite. USB
            // control is authenticated by the wired lockdown/HID session and
            // must not require Apple Devices' Wi-Fi pairing state.
            (
                match existing_session {
                    Ok(session) => Ok(session),
                    Err(_) => connect_session_with_ddi_recovery(&*provider, &ddi_dir).await,
                },
                "usb",
                udid,
            )
        };
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            let message = error.to_string();
            emit(Event::Error {
                code: coredevice_error_code(&message).into(),
                message: coredevice_error_message(&message),
            })?;
            return Ok(());
        }
    };

    let surfaces = match session.hid.list_connected_services().await {
        Ok(surfaces) => surfaces,
        Err(error) => {
            emit(Event::Error {
                code: "touch_surface_unavailable".into(),
                message: error.to_string(),
            })?;
            return Ok(());
        }
    };
    if !surfaces
        .iter()
        .any(|surface| surface.service_id == MAIN_TOUCHSCREEN)
    {
        emit(Event::Error {
            code: "touch_surface_unavailable".into(),
            message: describe_surfaces(&surfaces),
        })?;
        return Ok(());
    }

    emit(Event::Ready {
        protocol: 2,
        capabilities: ["iphoneMirror.usb_touch.v2", "iphoneMirror.usb_keyboard.v1"],
        udid: actual_udid.clone(),
        rate_hz: requested_rate_hz(),
        gate_open: true,
        auth_mode: session.auth_mode,
        transport: actual_transport,
    })?;

    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        loop {
            match read_frame(&mut stdin).await {
                Ok(Some(frame)) => {
                    if input_tx.send(InputMessage::Frame(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let message = error.to_string();
                    drop(error);
                    let _ = input_tx.send(InputMessage::Error(message)).await;
                    break;
                }
            }
        }
    });
    let mut active: HashMap<u32, TouchscreenContact> = HashMap::new();
    let mut slots: HashMap<u32, u8> = HashMap::new();
    let mut pressed_keys = HashSet::new();
    let mut pressed_buttons = HashSet::new();
    let mut last_sequence = None;
    // Clipboard commands are fire-and-forget from the desktop input path.
    // Keep one non-blocking operation gate rather than queuing tasks behind a
    // stalled pasteboard service. This keeps mouse/touch frames flowing even
    // when iOS rejects or delays a clipboard transaction.
    let clipboard_operation = Arc::new(tokio::sync::Mutex::new(()));
    let mut background_tasks = Vec::new();
    // PasteboardService is available through both RSD transports. Keep the
    // Windows clipboard synchronized for wired control too; otherwise USB
    // sessions only report clipboard contents after an explicit read request.
    background_tasks.push(tokio::spawn(monitor_clipboard(
        Arc::clone(&session.pasteboard),
        Arc::clone(&clipboard_operation),
    )));
    let direct_rotation = tokio::time::sleep(std::time::Duration::from_secs(12 * 60));
    tokio::pin!(direct_rotation);
    let mut hid_health = tokio::time::interval_at(
        tokio::time::Instant::now() + HID_HEALTH_CHECK_INTERVAL,
        HID_HEALTH_CHECK_INTERVAL,
    );
    hid_health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut hid_health_failures = 0u8;
    loop {
        let frame = tokio::select! {
            message = input_rx.recv() => match message {
                Some(InputMessage::Frame(frame)) => Some(frame),
                Some(InputMessage::Error(error)) => {
                    emit(Event::Error { code: "bad_frame".into(), message: error })?;
                    break;
                }
                None => None,
            },
            _ = &mut direct_rotation, if actual_transport == "usb" && session.auth_mode == "direct" && active.is_empty() && pressed_keys.is_empty() && pressed_buttons.is_empty() => {
                emit(Event::Status {
                    code: "direct_hid_rotating",
                    message: "正在无感更新 Universal HID 会话",
                })?;
                for task in background_tasks.drain(..) {
                    task.abort();
                    let _ = task.await;
                }
                let old_session = session;
                match reconnect_direct_session(
                    old_session,
                    actual_transport,
                    &actual_udid,
                    false,
                ).await {
                    Ok(new_session) => {
                        session = new_session;
                        active.clear();
                        slots.clear();
                        background_tasks.push(tokio::spawn(monitor_clipboard(
                            Arc::clone(&session.pasteboard),
                            Arc::clone(&clipboard_operation),
                        )));
                        direct_rotation.as_mut().reset(
                            tokio::time::Instant::now() + std::time::Duration::from_secs(12 * 60),
                        );
                        emit(Event::Status {
                            code: "direct_hid_rotated",
                            message: "Universal HID 会话已无感更新",
                        })?;
                        continue;
                    }
                    Err(error) => {
                        emit(Event::Error {
                            code: "direct_hid_rotation_failed".into(),
                            message: error.to_string(),
                        })?;
                        return Ok(());
                    }
                }
            }
            _ = hid_health.tick(), if (actual_transport != "usb" || session.auth_mode != "direct")
                && active.is_empty() && pressed_keys.is_empty() && pressed_buttons.is_empty() => {
                match tokio::time::timeout(
                    HID_HEALTH_CHECK_TIMEOUT,
                    session.hid.list_connected_services(),
                ).await {
                    Ok(Ok(surfaces)) if surfaces.iter().any(|surface| surface.service_id == MAIN_TOUCHSCREEN) => {
                        hid_health_failures = 0;
                        continue;
                    }
                    Ok(Ok(surfaces)) => {
                        hid_health_failures = hid_health_failures.saturating_add(1);
                        if hid_health_failures < HID_HEALTH_CHECK_FAILURE_THRESHOLD { continue; }
                        emit(Event::Error {
                            code: "touch_surface_unavailable".into(),
                            message: describe_surfaces(&surfaces),
                        })?;
                    }
                    Ok(Err(error)) => {
                        hid_health_failures = hid_health_failures.saturating_add(1);
                        if hid_health_failures < HID_HEALTH_CHECK_FAILURE_THRESHOLD { continue; }
                        emit(Event::Error {
                            code: "hid_health_check_failed".into(),
                            message: error.to_string(),
                        })?;
                    }
                    Err(_) => {
                        hid_health_failures = hid_health_failures.saturating_add(1);
                        if hid_health_failures < HID_HEALTH_CHECK_FAILURE_THRESHOLD { continue; }
                        emit(Event::Error {
                            code: "hid_health_check_timeout".into(),
                            message: "Universal HID health check timed out".into(),
                        })?;
                    }
                }
                break;
            }
        };
        let Some(frame) = frame else { break };
        if frame.schema != SCHEMA
            || !matches!(
                frame.kind.as_str(),
                TOUCH_KIND
                    | KEYBOARD_KIND
                    | BUTTON_KIND
                    | PASTE_TEXT_KIND
                    | COPY_SELECTION_KIND
                    | READ_CLIPBOARD_KIND
            )
        {
            emit(Event::Error {
                code: "invalid_message".into(),
                message: "unsupported schema or message kind".into(),
            })?;
            break;
        }
        if let Some(sequence) = frame.seq {
            if last_sequence.is_some_and(|previous| sequence <= previous) {
                emit(Event::Warning {
                    code: "invalid_sequence_resynced".into(),
                    message: format!(
                        "message sequence did not increase strictly monotonically (seq={sequence} <= last={last_sequence:?}); resyncing"
                    ),
                })?;
            }
            last_sequence = Some(sequence);
        }
        if frame.kind == KEYBOARD_KIND {
            if frame.usages.len() > 30 || frame.usages.iter().any(|usage| *usage > 239) {
                emit(Event::Error {
                    code: "invalid_keyboard_batch".into(),
                    message: "keyboard usage list is invalid".into(),
                })?;
                break;
            }
            let next_keys: HashSet<u64> = frame.usages.into_iter().collect();
            let mut indigo = session.indigo.lock().await;
            if let Err(error) = set_keyboard_state(&mut indigo, &pressed_keys, &next_keys).await {
                let message = error.to_string();
                drop(indigo);
                if is_recoverable_usb_transport_error(&message) {
                    // Do not replay a partially applied keyboard transition. A
                    // reconnect starts from a clean HID state and avoids turning
                    // one transport blip into duplicated key presses.
                    emit(Event::Warning {
                        code: "hid_transport_recovering".into(),
                        message: format!("键盘通道暂时不可用，正在恢复: {message}"),
                    })?;
                    pressed_keys.clear();
                    pressed_buttons.clear();
                    active.clear();
                    slots.clear();
                    match recover_hid_session(
                        session,
                        &mut background_tasks,
                        &clipboard_operation,
                        actual_transport,
                        &actual_udid,
                    )
                    .await
                    {
                        Ok(new_session) => {
                            let (discarded, pending_input_error) =
                                discard_stale_input_frames(&mut input_rx).await;
                            if let Some(error) = pending_input_error {
                                emit(Event::Error {
                                    code: "bad_frame".into(),
                                    message: error,
                                })?;
                                return Ok(());
                            }
                            session = new_session;
                            direct_rotation.as_mut().reset(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_secs(12 * 60),
                            );
                            emit(Event::Warning {
                                code: "hid_transport_recovered".into(),
                                message: recovered_transport_message("键盘", &message, discarded),
                            })?;
                            continue;
                        }
                        Err(recovery_error) => {
                            emit(Event::Error {
                                code: "send_failed".into(),
                                message: format!(
                                    "keyboard send failed: {message}; recovery failed: {recovery_error}"
                                ),
                            })?;
                            return Ok(());
                        }
                    }
                }
                emit(Event::Error {
                    code: "send_failed".into(),
                    message: format!("keyboard send failed: {message}"),
                })?;
                break;
            }
            drop(indigo);
            pressed_keys = next_keys;
            continue;
        }
        if frame.kind == BUTTON_KIND {
            let state = match frame.state.as_str() {
                "down" => ButtonState::Down,
                "up" => ButtonState::Up,
                "canceled" => ButtonState::Canceled,
                _ => {
                    emit(Event::Error {
                        code: "invalid_button_state".into(),
                        message: "button state must be down, up, or canceled".into(),
                    })?;
                    break;
                }
            };
            if frame.usage_page > u16::MAX as u64 || frame.usage_code > u16::MAX as u64 {
                emit(Event::Error {
                    code: "invalid_button_usage".into(),
                    message: "button usage page and code must fit in 16 bits".into(),
                })?;
                break;
            }
            let mut indigo = session.indigo.lock().await;
            let send = indigo.send_button(frame.usage_page, frame.usage_code, state);
            if let Err(error) = tokio::time::timeout(HID_OPERATION_TIMEOUT, send)
                .await
                .map_err(|_| "button operation timed out".to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
            {
                let message = error.to_string();
                drop(indigo);
                if is_recoverable_usb_transport_error(&message) {
                    emit(Event::Warning {
                        code: "hid_transport_recovering".into(),
                        message: format!("按钮通道暂时不可用，正在恢复: {message}"),
                    })?;
                    pressed_keys.clear();
                    pressed_buttons.clear();
                    active.clear();
                    slots.clear();
                    match recover_hid_session(
                        session,
                        &mut background_tasks,
                        &clipboard_operation,
                        actual_transport,
                        &actual_udid,
                    )
                    .await
                    {
                        Ok(new_session) => {
                            let (discarded, pending_input_error) =
                                discard_stale_input_frames(&mut input_rx).await;
                            if let Some(error) = pending_input_error {
                                emit(Event::Error {
                                    code: "bad_frame".into(),
                                    message: error,
                                })?;
                                return Ok(());
                            }
                            session = new_session;
                            direct_rotation.as_mut().reset(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_secs(12 * 60),
                            );
                            emit(Event::Warning {
                                code: "hid_transport_recovered".into(),
                                message: recovered_transport_message("按钮", &message, discarded),
                            })?;
                            continue;
                        }
                        Err(recovery_error) => {
                            emit(Event::Error {
                                code: "send_failed".into(),
                                message: format!(
                                    "button send failed: {message}; recovery failed: {recovery_error}"
                                ),
                            })?;
                            return Ok(());
                        }
                    }
                }
                emit(Event::Error {
                    code: "send_failed".into(),
                    message: format!("button send failed: {message}"),
                })?;
                break;
            }
            let button = (frame.usage_page, frame.usage_code);
            match state {
                ButtonState::Down => {
                    pressed_buttons.insert(button);
                }
                ButtonState::Up | ButtonState::Canceled => {
                    pressed_buttons.remove(&button);
                }
            }
            continue;
        }
        if frame.kind == PASTE_TEXT_KIND {
            // Keep the synthetic Command+V chord ordered before the next
            // keyboard or touch frame. A background task let Enter/mouse input
            // overtake the paste and could leave the device in a held state.
            let Ok(_operation) =
                tokio::time::timeout(EXPLICIT_CLIPBOARD_QUEUE_TIMEOUT, clipboard_operation.lock())
                    .await
            else {
                emit(Event::ClipboardPasteFailed {
                    seq: frame.seq,
                    message: "a clipboard operation is already in progress".into(),
                })?;
                continue;
            };
            let keys = pressed_keys.clone();
            match paste_text(&session.indigo, &session.pasteboard, &keys, &frame.text).await {
                Ok(PasteboardWriteConfirmation::Confirmed) => {
                    emit(Event::ClipboardPasteComplete { seq: frame.seq })?;
                }
                Ok(PasteboardWriteConfirmation::AcceptedButUnconfirmed(message)) => {
                    emit(Event::Warning {
                        code: "clipboard_paste_unconfirmed".into(),
                        message,
                    })?;
                    emit(Event::ClipboardPasteComplete { seq: frame.seq })?;
                }
                Err(error) => {
                    emit(Event::ClipboardPasteFailed {
                        seq: frame.seq,
                        message: error.to_string(),
                    })?;
                }
            }
            continue;
        }
        if frame.kind == COPY_SELECTION_KIND {
            background_tasks.retain(|task| !task.is_finished());
            let indigo = Arc::clone(&session.indigo);
            let pasteboard = Arc::clone(&session.pasteboard);
            let operation = Arc::clone(&clipboard_operation);
            let keys = pressed_keys.clone();
            background_tasks.push(tokio::spawn(async move {
                let Ok(_operation) =
                    tokio::time::timeout(EXPLICIT_CLIPBOARD_QUEUE_TIMEOUT, operation.lock()).await
                else {
                    let _ = emit(Event::Warning {
                        code: "copy_selection_busy".into(),
                        message: "a clipboard operation is already in progress".into(),
                    });
                    return;
                };
                if let Err(error) = copy_selection(&indigo, &pasteboard, &keys).await {
                    let _ = emit(Event::Warning {
                        code: "copy_selection_failed".into(),
                        message: error.to_string(),
                    });
                }
            }));
            continue;
        }
        if frame.kind == READ_CLIPBOARD_KIND {
            background_tasks.retain(|task| !task.is_finished());
            let pasteboard = Arc::clone(&session.pasteboard);
            let operation = Arc::clone(&clipboard_operation);
            background_tasks.push(tokio::spawn(async move {
                let Ok(_operation) =
                    tokio::time::timeout(EXPLICIT_CLIPBOARD_QUEUE_TIMEOUT, operation.lock()).await
                else {
                    let _ = emit(Event::Warning {
                        code: "clipboard_read_busy".into(),
                        message: "a clipboard operation is already in progress".into(),
                    });
                    return;
                };
                emit_clipboard_snapshot(&pasteboard).await;
            }));
            continue;
        }
        if frame.points.is_empty() || frame.points.len() > MAX_SLOTS {
            emit(Event::Error {
                code: "invalid_touch_batch".into(),
                message: format!("touch batch must contain 1 to {MAX_SLOTS} points"),
            })?;
            break;
        }

        let points = frame.points;
        let timestamp = frame.timestamp_ns.map(|value| value & ((1u64 << 48) - 1));
        let mut seen_pointer_ids = HashSet::new();
        if points.iter().any(|point| {
            !seen_pointer_ids.insert(point.pointer_id)
                || !matches!(point.action.as_str(), "down" | "move" | "up")
                || !point.normalized_x.is_finite()
                || !point.normalized_y.is_finite()
                || !(0.0..=1.0).contains(&point.normalized_x)
                || !(0.0..=1.0).contains(&point.normalized_y)
        }) {
            emit(Event::Error {
                code: "invalid_touch_point".into(),
                message:
                    "touch point has a duplicate pointer id, invalid action, or invalid coordinate"
                        .into(),
            })?;
            break;
        }
        let mut releases = Vec::new();
        // Free released slots before assigning new contacts so a full five-
        // finger batch can atomically replace one contact with another.
        for point in points.iter().filter(|point| point.action == "up") {
            if let Some(identity) = slots.remove(&point.pointer_id)
                && active.remove(&point.pointer_id).is_some()
            {
                releases.push(TouchscreenContact {
                    identity,
                    touching: false,
                    x: (point.normalized_x * 65535.0).round() as u16,
                    y: (point.normalized_y * 65535.0).round() as u16,
                });
            }
        }
        for point in points {
            if point.action == "up" {
                continue;
            }
            let identity = match point.action.as_str() {
                "down" => allocate_slot(&mut slots, point.pointer_id),
                "move" => slots
                    .get(&point.pointer_id)
                    .copied()
                    .or_else(|| allocate_slot(&mut slots, point.pointer_id)),
                _ => None,
            };
            let Some(identity) = identity else { continue };
            let contact = TouchscreenContact {
                identity,
                touching: true,
                x: (point.normalized_x * 65535.0).round() as u16,
                y: (point.normalized_y * 65535.0).round() as u16,
            };
            active.insert(point.pointer_id, contact);
        }
        let mut contacts: Vec<_> = active.values().copied().collect();
        contacts.extend(releases);
        if contacts.is_empty() {
            continue;
        }
        let report = build_multitouch_report(&contacts, timestamp)?;
        let send = session.hid.send_report(MAIN_TOUCHSCREEN, report);
        if let Err(error) = tokio::time::timeout(HID_OPERATION_TIMEOUT, send)
            .await
            .map_err(|_| "touch operation timed out".to_string())
            .and_then(|result| result.map_err(|error| error.to_string()))
        {
            let message = error.to_string();
            if is_recoverable_usb_transport_error(&message) {
                // The failed report may have been partially accepted. Clear
                // host-side contacts and rebuild the session instead of
                // replaying it, which is safer for both taps and drags.
                emit(Event::Warning {
                    code: "hid_transport_recovering".into(),
                    message: format!("触控通道暂时不可用，正在恢复: {message}"),
                })?;
                active.clear();
                slots.clear();
                pressed_keys.clear();
                pressed_buttons.clear();
                match recover_hid_session(
                    session,
                    &mut background_tasks,
                    &clipboard_operation,
                    actual_transport,
                    &actual_udid,
                )
                .await
                {
                    Ok(new_session) => {
                        let (discarded, pending_input_error) =
                            discard_stale_input_frames(&mut input_rx).await;
                        if let Some(error) = pending_input_error {
                            emit(Event::Error {
                                code: "bad_frame".into(),
                                message: error,
                            })?;
                            return Ok(());
                        }
                        session = new_session;
                        direct_rotation.as_mut().reset(
                            tokio::time::Instant::now() + std::time::Duration::from_secs(12 * 60),
                        );
                        emit(Event::Warning {
                            code: "hid_transport_recovered".into(),
                            message: recovered_transport_message("触控", &message, discarded),
                        })?;
                        continue;
                    }
                    Err(recovery_error) => {
                        emit(Event::Error {
                            code: "send_failed".into(),
                            message: format!(
                                "touch send failed: {message}; recovery failed: {recovery_error}"
                            ),
                        })?;
                        return Ok(());
                    }
                }
            }
            emit(Event::Error {
                code: "send_failed".into(),
                message: format!("touch send failed: {message}"),
            })?;
            break;
        }
    }
    for task in background_tasks {
        task.abort();
        let _ = task.await;
    }
    let _ = release_all(&mut session).await;
    let mut indigo = session.indigo.lock().await;
    // A background paste task may be cancelled between Command/V transitions.
    // Release the synthetic chord explicitly before releasing host-held keys.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let _ = indigo.send_keyboard(0x19, ButtonState::Up).await;
        let _ = indigo.send_keyboard(0xE3, ButtonState::Up).await;
        for usage in pressed_keys {
            let _ = indigo.send_keyboard(usage, ButtonState::Up).await;
        }
    })
    .await;
    drop(indigo);
    // Do not call DisplayService stop_media_stream here. The API currently
    // exposes only stopAll=true, which would terminate the host capture
    // session's projection stream when the control bridge exits.
    Ok(())
}

fn clear_raw_capture_mux() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(holder) = RAW_CAPTURE_MUX.get() {
        *holder
            .lock()
            .map_err(|_| io::Error::other("raw usbmux state poisoned"))? = None;
    }
    Ok(())
}

fn is_recoverable_usb_transport_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("socket io")
        || normalized.contains("connection reset")
        || normalized.contains("connection lost")
        || normalized.contains("connection aborted")
        || normalized.contains("broken pipe")
        || normalized.contains("unexpected eof")
        || normalized.contains("timed out")
        || normalized.contains("timeout")
}

async fn prepare_wireless_personalized_ddi(
    expected_udid: Option<&str>,
    ddi_dir: &Path,
) -> Result<(), WirelessDdiPreflightError> {
    emit(Event::Status {
        code: "checking_developer_environment",
        message: "正在通过 USB 检查 Developer Mode 和 Personalized DDI",
    })
    .map_err(|error| WirelessDdiPreflightError {
        code: "developer_image_mount_failed",
        message: error.to_string(),
    })?;

    let mut last_failure: Option<WirelessDdiPreflightError> = None;
    for (attempt, delay_ms) in [0_u64, 150, 400, 900].into_iter().enumerate() {
        if delay_ms > 0 {
            emit(Event::Warning {
                code: "wireless_ddi_usb_retry".into(),
                message: format!("无线控制正在重新建立 USB DDI 检查通道 {}/4", attempt + 1),
            })
            .map_err(|error| WirelessDdiPreflightError {
                code: "developer_image_mount_failed",
                message: error.to_string(),
            })?;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        let (provider, usb_udid) = match device_provider(Transport::Usb).await {
            Ok(provider) => provider,
            Err(error) => {
                let message = error.to_string();
                let code = if attempt == 3 || !is_recoverable_usb_transport_error(&message) {
                    "wireless_ddi_usb_required"
                } else {
                    last_failure = Some(WirelessDdiPreflightError {
                        code: "wireless_ddi_usb_required",
                        message,
                    });
                    continue;
                };
                return Err(WirelessDdiPreflightError { code, message });
            }
        };
        if expected_udid.is_some_and(|expected| !same_udid(expected, &usb_udid)) {
            return Err(WirelessDdiPreflightError {
                code: "wireless_ddi_usb_required",
                message: format!(
                    "USB DDI 检查发现的设备与无线控制目标不一致: expected={}, actual={}",
                    expected_udid.unwrap_or_default(),
                    usb_udid
                ),
            });
        }

        // Provisioning is intentionally best effort: DDI readiness is the
        // prerequisite here, while network discovery below will still report
        // a precise RemotePairing error if the device declines this service.
        if let Err(error) = provision_remote_pairing(&*provider, &usb_udid).await {
            emit(Event::Warning {
                code: "wireless_remote_pairing_provision_failed".into(),
                message: error.to_string(),
            })
            .map_err(|emit_error| WirelessDdiPreflightError {
                code: "developer_image_mount_failed",
                message: emit_error.to_string(),
            })?;
        }

        match connect_session_with_ddi_recovery(&*provider, ddi_dir).await {
            Ok(session) => {
                drop(session);
                return Ok(());
            }
            Err(error) => {
                let message = error.to_string();
                if is_recoverable_usb_transport_error(&message) && attempt < 3 {
                    last_failure = Some(WirelessDdiPreflightError {
                        code: "wireless_ddi_usb_required",
                        message,
                    });
                    continue;
                }
                return Err(WirelessDdiPreflightError {
                    code: if message.to_ascii_lowercase().contains("developer mode") {
                        "developer_mode_required"
                    } else if is_recoverable_usb_transport_error(&message) {
                        "wireless_ddi_usb_required"
                    } else {
                        "developer_image_mount_failed"
                    },
                    message,
                });
            }
        }
    }

    Err(last_failure.unwrap_or(WirelessDdiPreflightError {
        code: "wireless_ddi_usb_required",
        message: "无线控制的 USB DDI 检查未能建立设备会话".into(),
    }))
}

fn emit_developer_image_mount_error(message: String) -> Result<(), Box<dyn std::error::Error>> {
    emit(Event::Error {
        code: if message.to_ascii_lowercase().contains("developer mode") {
            "developer_mode_required"
        } else {
            "developer_image_mount_failed"
        }
        .into(),
        message,
    })
}

async fn device_provider(
    transport: Transport,
) -> Result<(Box<dyn IdeviceProvider>, String), Box<dyn std::error::Error>> {
    let udid = requested_udid();
    // When QuickTime is actively mirroring, claim its hidden usbmux
    // interface first and give the control session its own localhost port.
    // The normal Apple usbmux record can still be visible, but it cannot
    // reliably service a second CoreDevice client in that configuration.
    if transport == Transport::Usb
        && let Some(serial) = udid.as_deref()
    {
        let mut start_raw_mux = true;
        // A direct-HID session rotation keeps the capture mux alive so the
        // projection can continue using QuickTime's hidden USB interface.
        // Reuse that listener instead of claiming the same USB interface a
        // second time from the same bridge process.
        if let Some(existing_mux) = RAW_CAPTURE_MUX
            .get()
            .and_then(|holder| holder.lock().ok().and_then(|value| value.clone()))
        {
            if !existing_mux.is_alive() {
                // A reader-side USB error invalidates the listener even though
                // the global Arc is still present. Drop it before attempting a
                // fresh claim so reconnect does not reuse a dead mux.
                clear_raw_capture_mux()?;
            } else {
                let address = UsbmuxdAddr::TcpSocket(existing_mux.address());
                let mut mux_client = match address.connect(1).await {
                    Ok(client) => Some(client),
                    Err(_) => None,
                };
                if let Some(client) = mux_client.as_mut() {
                    if let Ok(devices) = client.get_devices().await {
                        // A responsive listener already owns QuickTime's
                        // hidden interface. Do not start a second RawMux
                        // against that same interface while the device is
                        // briefly absent from ListDevices after re-enumeration.
                        start_raw_mux = false;
                        if let Some(device) = devices.into_iter().find(|device| {
                            udid.as_deref()
                                .is_none_or(|requested| same_udid(&device.udid, requested))
                                && device.connection_type == Connection::Usb
                        }) {
                            let actual_udid = device.udid.clone();
                            return Ok((
                                Box::new(device.to_provider(address, "iPhoneMirror-idevice")),
                                actual_udid,
                            ));
                        }
                        // During DDI/QuickTime re-enumeration the listener can
                        // remain healthy while ListDevices is briefly empty.
                        // Keep the mux for the projection and continue through
                        // the normal Apple usbmux endpoints below instead of
                        // turning that short window into a terminal NotFound.
                        let _ = emit(Event::Warning {
                            code: "capture_mux_device_pending".into(),
                            message: format!(
                                "QuickTime USBMux 暂未返回目标设备 {serial}，继续等待其它 USB 通道"
                            ),
                        });
                    }
                }
                if start_raw_mux {
                    // The listener did not answer its own usbmux request, so
                    // its USB reader is no longer a usable owner.
                    clear_raw_capture_mux()?;
                }
            }
        }
        if start_raw_mux {
            match raw_usbmux::RawMux::start(serial) {
                Err(error) => {
                    let _ = emit(Event::Warning {
                        code: "capture_mux_start_failed".into(),
                        message: format!("QuickTime USBMux 启动失败: {error}"),
                    });
                }
                Ok(None) => {
                    let _ = emit(Event::Status {
                        code: "capture_mux_not_found",
                        message: "未找到 QuickTime 隐藏 USBMux 接口".into(),
                    });
                }
                Ok(Some(mux)) => {
                    let address = UsbmuxdAddr::TcpSocket(mux.address());
                    let holder = RAW_CAPTURE_MUX.get_or_init(|| Mutex::new(None));
                    *holder
                        .lock()
                        .map_err(|_| io::Error::other("raw usbmux state poisoned"))? = Some(mux);
                    emit(Event::Status {
                        code: "capture_mux_ready",
                        message: "已启动 QuickTime USB 本地 usbmux 通道",
                    })?;
                    let mut mux_client = address.connect(1).await?;
                    let devices = mux_client.get_devices().await?;
                    if let Some(device) = devices.into_iter().find(|device| {
                        udid.as_deref()
                            .is_none_or(|requested| same_udid(&device.udid, requested))
                            && device.connection_type == Connection::Usb
                    }) {
                        let actual_udid = device.udid.clone();
                        return Ok((
                            Box::new(device.to_provider(address, "iPhoneMirror-idevice")),
                            actual_udid,
                        ));
                    }
                    let _ = emit(Event::Warning {
                        code: "capture_mux_device_not_found".into(),
                        message: format!("QuickTime USBMux 未返回目标设备 {serial}"),
                    });
                }
            }
        }
    }
    let addresses = [
        UsbmuxdAddr::default(),
        UsbmuxdAddr::TcpSocket(std::net::SocketAddr::from(([127, 0, 0, 1], 37015))),
    ];
    let mut reachable = false;
    let mut last_error = None;
    for address in addresses {
        let mut mux = match address.connect(1).await {
            Ok(mux) => mux,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        reachable = true;
        let devices = match mux.get_devices().await {
            Ok(devices) => devices,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        if let Some(device) = devices.into_iter().find(|device| {
            udid.as_deref()
                .is_none_or(|requested| same_udid(&device.udid, requested))
                && match transport {
                    Transport::Usb => device.connection_type == Connection::Usb,
                    Transport::Network => matches!(device.connection_type, Connection::Network(_)),
                }
        }) {
            let udid = device.udid.clone();
            return Ok((
                Box::new(device.to_provider(address, "iPhoneMirror-idevice")),
                udid,
            ));
        }
    }
    if reachable {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no matching {transport:?} iOS device found"),
        )
        .into())
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "no Apple or capture usbmuxd endpoint is available{}",
                last_error
                    .map(|value| format!(": {value}"))
                    .unwrap_or_default()
            ),
        )
        .into())
    }
}

async fn enable_wifi_sync() -> Result<(), Box<dyn std::error::Error>> {
    let udid = requested_udid().ok_or("--enable-wifi-sync requires --udid")?;
    emit(Event::Status {
        code: "enabling_wifi_sync",
        message: "正在通过 USB 启用 Apple Wi-Fi 同步",
    })?;
    let (provider, _) = device_provider(Transport::Usb).await?;
    let mut lockdown = LockdownClient::connect(&*provider).await?;
    let pairing = provider.get_pairing_file().await?;
    lockdown.start_session(&pairing).await?;
    const DOMAIN: &str = "com.apple.mobile.wireless_lockdown";
    const KEY: &str = "EnableWifiConnections";
    let was_enabled = lockdown
        .get_value(Some(KEY), Some(DOMAIN))
        .await
        .ok()
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    if !was_enabled {
        lockdown
            .set_value(KEY, plist::Value::Boolean(true), Some(DOMAIN))
            .await?;
    }
    let enabled = lockdown
        .get_value(Some(KEY), Some(DOMAIN))
        .await?
        .as_boolean()
        .unwrap_or(false);
    if !enabled {
        return Err("device rejected EnableWifiConnections=true".into());
    }
    emit(Event::WifiSyncEnabled {
        udid,
        changed: !was_enabled,
    })?;
    Ok(())
}

async fn remote_pairing_transport()
-> Result<(AdapterHandle, RsdHandshake, String), Box<dyn std::error::Error>> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse("_remotepairing._tcp.local.")?;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(15));
    tokio::pin!(deadline);
    let mut candidates = remote_pairing_candidates().await?;
    loop {
        tokio::select! {
            _ = &mut deadline => return Err("no RemotePairing mDNS service found within 15 seconds".into()),
            event = receiver.recv_async() => {
                let ServiceEvent::ServiceResolved(service) = event? else { continue; };
                let advertised_id = service.get_property("identifier").map(|p| p.val_str().to_owned());
                let Some(index) = candidates.iter().position(|(_, _, pairing)| {
                    if advertised_id.as_deref() != Some(pairing.identifier()) { return false; }
                    match (
                        pairing.alt_irk(),
                        service.get_property("authTag").map(|p| p.val_str()),
                        advertised_id.as_deref(),
                    ) {
                        (Some(alt_irk), Some(auth_tag), Some(identifier)) =>
                            PeerDevice::validate_auth_tag(alt_irk, identifier, auth_tag),
                        _ => candidates.len() == 1,
                    }
                }) else { continue; };
                let (actual_udid, pairing_path, mut pairing) = candidates.swap_remove(index);
                let endpoint = service.get_addresses().iter().find_map(|ip| match ip {
                    ScopedIp::V4(v4) => Some(std::net::SocketAddr::new(
                        std::net::IpAddr::V4(*v4.addr()), service.get_port())),
                    ScopedIp::V6(v6) => Some(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                        *v6.addr(), service.get_port(), 0, v6.scope_id().index))),
                    _ => None,
                }).ok_or("RemotePairing service has no usable address")?;
                let stream = TcpStream::connect(endpoint).await?;
                let mut rpc = RemotePairingClient::new(RpPairingSocket::new(stream), "iPhoneMirror-idevice");
                rpc.connect(&mut pairing, || async { "000000".to_string() }).await?;
                if let Some(parent) = pairing_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&pairing_path, pairing.to_bytes()).await?;
                let tunnel_port = rpc.create_tcp_listener().await?;
                let tunnel_endpoint = match endpoint {
                    std::net::SocketAddr::V4(v4) => std::net::SocketAddr::V4(std::net::SocketAddrV4::new(*v4.ip(), tunnel_port)),
                    std::net::SocketAddr::V6(v6) => std::net::SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), tunnel_port, v6.flowinfo(), v6.scope_id())),
                };
                let tunnel = connect_tls_psk_tunnel_native(
                    TcpStream::connect(tunnel_endpoint).await?,
                    rpc.encryption_key(),
                ).await?;
                let client_ip: std::net::IpAddr = tunnel.info.client_address.parse()?;
                let server_ip: std::net::IpAddr = tunnel.info.server_address.parse()?;
                let mtu = tunnel.info.mtu as usize;
                let rsd_port = tunnel.info.server_rsd_port;
                let mut adapter = idevice::tcp::adapter::Adapter::new(Box::new(tunnel.into_inner()), client_ip, server_ip);
                adapter.set_mss(mtu.saturating_sub(60));
                let mut adapter = adapter.to_async_handle();
                let handshake = RsdHandshake::new(adapter.connect(rsd_port).await?).await?;
                let _ = daemon.shutdown();
                return Ok((adapter, handshake, actual_udid));
            }
        }
    }
}

async fn provision_remote_pairing(
    provider: &dyn IdeviceProvider,
    udid: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = remote_pairing_file_path(Some(udid));
    emit(Event::Status {
        code: "provisioning_remote_pairing",
        message: "正在通过 USB 配置无线 RemotePairing",
    })?;
    let parent = path.parent().ok_or("RemotePairing cache has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let service = tokio::time::timeout(
        REMOTE_PAIRING_OPERATION_TIMEOUT,
        RemotePairingLockdownService::connect(provider),
    )
    .await
    .map_err(|_| "RemotePairing service connection timed out")??;
    let mut client = service.into_client("iPhoneMirror-idevice")?;
    // A readable plist can still be stale after the device is reset or
    // re-paired. Reuse it only as the starting point, then require a real
    // device-side connect and persist the refreshed pairing material.
    let mut pairing = match RpPairingFile::read_from_file(&path).await {
        Ok(pairing) => pairing,
        Err(_) => RpPairingFile::generate("iPhoneMirror-idevice"),
    };
    tokio::time::timeout(
        REMOTE_PAIRING_OPERATION_TIMEOUT,
        client.connect(&mut pairing, async || "000000".to_string()),
    )
    .await
    .map_err(|_| "RemotePairing pairing verification timed out")??;
    pairing.write_to_file(&path).await?;
    Ok(())
}

async fn try_connect_existing_wireless_hid(
    expected_udid: Option<&str>,
) -> Result<(Session, String), Box<dyn std::error::Error>> {
    match remote_pairing_transport().await {
        Ok((adapter, handshake, udid)) => {
            if expected_udid.is_some_and(|expected| !same_udid(expected, &udid)) {
                return Err(format!(
                    "wireless device changed: expected={}, actual={udid}",
                    expected_udid.unwrap_or_default()
                )
                .into());
            }
            let session = connect_session_from_transport(adapter, handshake, true).await?;
            Ok((require_main_touchscreen(session).await?, udid))
        }
        Err(remote_error) => match device_provider(Transport::Network).await {
            Ok((provider, udid)) => {
                if expected_udid.is_some_and(|expected| !same_udid(expected, &udid)) {
                    return Err(format!(
                        "wireless device changed: expected={}, actual={udid}",
                        expected_udid.unwrap_or_default()
                    )
                    .into());
                }
                let session = connect_session(&*provider).await?;
                Ok((require_main_touchscreen(session).await?, udid))
            }
            Err(_) => Err(remote_error),
        },
    }
}

async fn require_main_touchscreen(
    mut session: Session,
) -> Result<Session, Box<dyn std::error::Error>> {
    let surfaces = session.hid.list_connected_services().await?;
    if surfaces
        .iter()
        .any(|surface| surface.service_id == MAIN_TOUCHSCREEN)
    {
        Ok(session)
    } else {
        Err(describe_surfaces(&surfaces).into())
    }
}

async fn release_all(session: &mut Session) -> Result<(), Box<dyn std::error::Error>> {
    let releases: Vec<_> = (0..MAX_SLOTS as u8)
        .map(|identity| TouchscreenContact {
            identity,
            touching: false,
            x: 0,
            y: 0,
        })
        .collect();
    let report = build_multitouch_report(&releases, None)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        session.hid.send_report(MAIN_TOUCHSCREEN, report),
    )
    .await
    .map_err(|_| "touch release timed out")??;
    Ok(())
}

async fn set_keyboard_state(
    indigo: &mut IndigoHidClient<Box<dyn idevice::ReadWrite>>,
    from: &HashSet<u64>,
    to: &HashSet<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for usage in to.difference(from) {
        tokio::time::timeout(
            HID_OPERATION_TIMEOUT,
            indigo.send_keyboard(*usage, ButtonState::Down),
        )
        .await
        .map_err(|_| "keyboard operation timed out")??;
    }
    for usage in from.difference(to) {
        tokio::time::timeout(
            HID_OPERATION_TIMEOUT,
            indigo.send_keyboard(*usage, ButtonState::Up),
        )
        .await
        .map_err(|_| "keyboard operation timed out")??;
    }
    Ok(())
}

async fn paste_text(
    indigo: &Arc<tokio::sync::Mutex<IndigoHidClient<Box<dyn idevice::ReadWrite>>>>,
    pasteboard: &Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
    pressed_keys: &HashSet<u64>,
    text: &str,
) -> Result<PasteboardWriteConfirmation, Box<dyn std::error::Error + Send + Sync>> {
    let mut pasteboard = lock_pasteboard(pasteboard).await?;
    let confirmation = write_pasteboard_text_with_confirmation(&mut pasteboard, text).await?;
    drop(pasteboard);
    let command = HashSet::from([0xE3]);
    let command_v = HashSet::from([0xE3, 0x19]);
    let mut indigo = indigo.lock().await;
    let result = async {
        set_keyboard_state(&mut indigo, pressed_keys, &command).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        set_keyboard_state(&mut indigo, &command, &command_v).await?;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        set_keyboard_state(&mut indigo, &command_v, &command).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        set_keyboard_state(&mut indigo, &command, pressed_keys).await
    }
    .await;
    if result.is_err() {
        // Best effort: never leave the synthetic Command+V chord held after a
        // transient pasteboard or HID failure.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            indigo.send_keyboard(0x19, ButtonState::Up),
        )
        .await;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            indigo.send_keyboard(0xE3, ButtonState::Up),
        )
        .await;
        for usage in pressed_keys {
            let _ = indigo.send_keyboard(*usage, ButtonState::Down).await;
        }
    }
    result?;
    Ok(confirmation)
}

async fn write_pasteboard_text_with_confirmation(
    pasteboard: &mut PasteboardServiceClient<Box<dyn idevice::ReadWrite>>,
    text: &str,
) -> Result<PasteboardWriteConfirmation, Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(
        PASTEBOARD_OPERATION_TIMEOUT,
        pasteboard.set_text(text, GENERAL_PASTEBOARD),
    )
    .await
    .map_err(|_| "pasteboard operation timed out")??;

    match confirm_pasteboard_text(pasteboard, text).await? {
        true => Ok(PasteboardWriteConfirmation::Confirmed),
        false => Ok(PasteboardWriteConfirmation::AcceptedButUnconfirmed(
            "pasteboard SET was accepted but exact PULL confirmation was unavailable; pasted once with protected key release".into(),
        )),
    }
}

async fn confirm_pasteboard_text(
    pasteboard: &mut PasteboardServiceClient<Box<dyn idevice::ReadWrite>>,
    expected: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    for delay_ms in PASTEBOARD_WRITE_CONFIRM_DELAYS_MS {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        let snapshot = tokio::time::timeout(
            PASTEBOARD_OPERATION_TIMEOUT,
            pasteboard.get(GENERAL_PASTEBOARD),
        )
        .await
        .map_err(|_| "pasteboard confirmation timed out")??;
        if clipboard_text_matches(expected, snapshot.text().as_deref()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn clipboard_text_matches(expected: &str, observed: Option<&str>) -> bool {
    observed == Some(expected)
}

async fn copy_selection(
    indigo: &Arc<tokio::sync::Mutex<IndigoHidClient<Box<dyn idevice::ReadWrite>>>>,
    pasteboard: &Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
    pressed_keys: &HashSet<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let command = HashSet::from([0xE3]);
    let command_c = HashSet::from([0xE3, 0x06]);
    let mut indigo = indigo.lock().await;
    let result = async {
        // Indigo handles keys as individual HID transitions. The ordering is
        // significant: a set transition may otherwise press C before Command.
        set_keyboard_state(&mut indigo, pressed_keys, &command).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        set_keyboard_state(&mut indigo, &command, &command_c).await?;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        set_keyboard_state(&mut indigo, &command_c, &command).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        set_keyboard_state(&mut indigo, &command, pressed_keys).await
    }
    .await;
    drop(indigo);
    result?;

    // Pasteboard updates following a selection command can arrive after the
    // HID key releases. Publish several short snapshots so Windows receives
    // the final value even when iOS delays the pasteboard transaction.
    // A poll that began just before Command+C can use the service for one
    // bounded request. Keep retrying past that window so an explicit copy
    // always gets a chance to publish its result to Windows.
    for delay in [120_u64, 240, 480, 800, 1200] {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        emit_clipboard_snapshot(pasteboard).await;
    }
    Ok(())
}

async fn lock_pasteboard<'a>(
    pasteboard: &'a Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
) -> Result<
    tokio::sync::MutexGuard<'a, PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>,
    Box<dyn std::error::Error + Send + Sync>,
> {
    tokio::time::timeout(PASTEBOARD_CONTEXT_TIMEOUT, pasteboard.lock())
        .await
        .map_err(|_| "pasteboard service is busy".into())
}

async fn read_clipboard_text(
    pasteboard: &Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut pasteboard = lock_pasteboard(pasteboard).await?;
    let snapshot = tokio::time::timeout(
        PASTEBOARD_OPERATION_TIMEOUT,
        pasteboard.get(GENERAL_PASTEBOARD),
    )
    .await
    .map_err(|_| "pasteboard operation timed out")??;
    Ok(snapshot.text().unwrap_or_default())
}

async fn emit_clipboard_snapshot(
    pasteboard: &Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
) {
    match read_clipboard_text(pasteboard).await {
        Ok(text) => {
            let _ = emit(Event::ClipboardText { text: Some(text) });
        }
        Err(error) => {
            let _ = emit(Event::Warning {
                code: "clipboard_read_failed".into(),
                message: error.to_string(),
            });
        }
    }
}

async fn monitor_clipboard(
    pasteboard: Arc<tokio::sync::Mutex<PasteboardServiceClient<Box<dyn idevice::ReadWrite>>>>,
    operation: Arc<tokio::sync::Mutex<()>>,
) {
    let mut last_text = None;
    loop {
        // Explicit paste/copy actions need an uncontended pasteboard request.
        // Polling is best-effort, so skip this interval instead of putting the
        // monitor ahead of the operation that the user just requested.
        if let Ok(_operation) = operation.try_lock()
            && let Ok(text) = read_clipboard_text(&pasteboard).await
            && last_text.as_ref() != Some(&text)
        {
            last_text = Some(text.clone());
            let _ = emit(Event::ClipboardText { text: Some(text) });
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
}

async fn mount_local_ddi(
    provider: &dyn IdeviceProvider,
    directory: &Path,
    force_remount: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !directory.is_dir() {
        return Err(format!("DDI directory does not exist: {}", directory.display()).into());
    }
    let image = tokio::fs::read(directory.join("Image.dmg")).await?;
    let manifest = tokio::fs::read(directory.join("BuildManifest.plist")).await?;
    let trust_path = directory.join("Image.trustcache");
    let trust_path = if trust_path.is_file() {
        trust_path
    } else {
        directory.join("Image.dmg.trustcache")
    };
    let trust_cache = tokio::fs::read(trust_path).await?;

    let mut lockdown = LockdownClient::connect(provider).await?;
    let pairing = provider.get_pairing_file().await?;
    lockdown.start_session(&pairing).await?;
    let mut mounter = ImageMounter::connect(provider).await?;
    if !mounter.query_developer_mode_status().await? {
        return Err("Developer Mode is disabled".into());
    }
    let unique_chip_id = lockdown
        .get_value(Some("UniqueChipID"), None)
        .await?
        .as_unsigned_integer()
        .ok_or("UniqueChipID is missing or not an unsigned integer")?;
    let mounted = mounter.copy_devices().await?;
    let mut already_mounted = mounted.iter().any(is_personalized_ddi_entry);
    if force_remount && already_mounted {
        let _ = mounter.unmount_image("/System/Developer").await;
        already_mounted = false;
    }
    if !already_mounted {
        mounter
            .mount_personalized(
                provider,
                image,
                trust_cache,
                &manifest,
                None,
                unique_chip_id,
            )
            .await?;
    }
    Ok(())
}

async fn connect_session(
    provider: &dyn IdeviceProvider,
) -> Result<Session, Box<dyn std::error::Error>> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy.create_software_tunnel()?.to_async_handle();
    let mut adapter = adapter;
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    connect_session_from_transport(adapter, handshake, true).await
}

async fn connect_hid_session(
    provider: &dyn IdeviceProvider,
) -> Result<Session, Box<dyn std::error::Error>> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy.create_software_tunnel()?.to_async_handle();
    let mut adapter = adapter;
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    // Wired control must not create a second device media stream beside the
    // host-owned QuickTime projection. Universal HID is usable without the
    // auxiliary media gate once the Personalized DDI is present.
    let mut session = connect_session_from_transport(adapter, handshake, false).await?;
    let surfaces = session.hid.list_connected_services().await?;
    if surfaces
        .iter()
        .any(|surface| surface.service_id == MAIN_TOUCHSCREEN)
    {
        Ok(session)
    } else {
        Err(describe_surfaces(&surfaces).into())
    }
}

async fn reconnect_direct_session(
    mut old_session: Session,
    transport: &str,
    expected_udid: &str,
    after_transport_failure: bool,
) -> Result<Session, Box<dyn std::error::Error>> {
    if !after_transport_failure {
        let _ = release_all(&mut old_session).await;
    }
    // In direct mode the display client is only a best-effort probe. The
    // actual wired projection is owned by the host capture session, so
    // stopping this client must not send a stop request to QuickTime.
    // DisplayService stop_media_stream is stopAll=true and can terminate the
    // host-owned projection. Dropping the RSD client is the only safe cleanup
    // available until the API supports a session-scoped stop.
    drop(old_session);
    tokio::time::sleep(std::time::Duration::from_millis(
        if after_transport_failure { 50 } else { 250 },
    ))
    .await;
    let mut last_error: Option<Box<dyn std::error::Error>> = None;
    // USBMux and CoreDevice services can take several seconds to reappear
    // after a DDI-driven interface reset. Keep this bounded so a real unplug
    // still surfaces promptly, while avoiding a false terminal disconnect on
    // the first empty enumeration.
    let retry_delays: &[u64] = if after_transport_failure {
        &[0, 100, 250, 500, 1000]
    } else {
        &[0, 250, 500, 1000, 2000]
    };
    for delay_ms in retry_delays {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        let result = if transport == "usb" {
            match device_provider(Transport::Usb).await {
                Ok((provider, udid)) if udid.eq_ignore_ascii_case(expected_udid) => {
                    connect_hid_session(&*provider).await
                }
                Ok((_, udid)) => Err(format!(
                    "USB device changed during direct HID rotation: expected {expected_udid}, got {udid}"
                ).into()),
                Err(error) => Err(error),
            }
        } else {
            match remote_pairing_transport().await {
                Ok((adapter, handshake, udid)) => {
                    if !udid.eq_ignore_ascii_case(expected_udid) {
                        Err(format!(
                            "wireless device changed during direct HID rotation: expected {expected_udid}, got {udid}"
                        ).into())
                    } else {
                        connect_session_from_transport(adapter, handshake, true).await
                    }
                }
                Err(remote_error) => match device_provider(Transport::Network).await {
                    Ok((provider, udid)) if udid.eq_ignore_ascii_case(expected_udid) => {
                        connect_session(&*provider).await
                    }
                    Ok((_, udid)) => Err(format!(
                        "wireless device changed during direct HID rotation: expected {expected_udid}, got {udid}"
                    ).into()),
                    Err(_) => Err(remote_error),
                },
            }
        };
        match result {
            Ok(mut session) => {
                let surfaces = session.hid.list_connected_services().await?;
                if surfaces
                    .iter()
                    .any(|surface| surface.service_id == MAIN_TOUCHSCREEN)
                {
                    return Ok(session);
                }
                last_error = Some(describe_surfaces(&surfaces).into());
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "direct HID session rotation failed".into()))
}

async fn recover_hid_session(
    old_session: Session,
    background_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    clipboard_operation: &Arc<tokio::sync::Mutex<()>>,
    transport: &str,
    expected_udid: &str,
) -> Result<Session, Box<dyn std::error::Error>> {
    for task in background_tasks.drain(..) {
        task.abort();
        let _ = task.await;
    }
    let new_session = reconnect_direct_session(old_session, transport, expected_udid, true).await?;
    background_tasks.push(tokio::spawn(monitor_clipboard(
        Arc::clone(&new_session.pasteboard),
        Arc::clone(clipboard_operation),
    )));
    Ok(new_session)
}

// The desktop closes its input gate as soon as it receives the recovering
// event, but frames already written to stdin can still be in this bounded
// queue. They describe the pre-failure HID state and must never be replayed
// into the fresh session, where they could turn into an unexpected tap or a
// resumed drag.
async fn discard_stale_input_frames(
    input_rx: &mut tokio::sync::mpsc::Receiver<InputMessage>,
) -> (usize, Option<String>) {
    let mut discarded = 0;
    // The reader can be in the middle of moving an already-written pipe frame
    // into the channel when recovery succeeds. Wait for a few empty polls so
    // that frame is discarded before announcing the fresh session to desktop.
    // This adds at most six milliseconds to a successful recovery.
    let mut quiet_polls = 0;
    while quiet_polls < 3 {
        let mut received_frame = false;
        while let Ok(message) = input_rx.try_recv() {
            match message {
                InputMessage::Frame(_) => {
                    discarded += 1;
                    received_frame = true;
                }
                InputMessage::Error(error) => return (discarded, Some(error)),
            }
        }
        if received_frame {
            quiet_polls = 0;
            tokio::task::yield_now().await;
        } else {
            quiet_polls += 1;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    (discarded, None)
}

fn recovered_transport_message(channel: &str, error: &str, discarded: usize) -> String {
    if discarded == 0 {
        format!("{channel}通道短暂断开后已恢复: {error}")
    } else {
        format!("{channel}通道短暂断开后已恢复，已丢弃恢复期间堆积的 {discarded} 条旧输入: {error}")
    }
}

async fn connect_session_with_ddi_recovery(
    provider: &dyn IdeviceProvider,
    ddi_dir: &Path,
) -> Result<Session, Box<dyn std::error::Error>> {
    let first = connect_hid_session(provider).await;
    match first {
        Ok(mut session) => match session.hid.list_connected_services().await {
            Ok(surfaces)
                if surfaces
                    .iter()
                    .any(|surface| surface.service_id == MAIN_TOUCHSCREEN) =>
            {
                return Ok(session);
            }
            Ok(surfaces) => {
                drop(session);
                let _ = surfaces;
            }
            Err(_) => drop(session),
        },
        Err(error) if !is_ddi_registration_error(&error.to_string()) => return Err(error),
        Err(_) => {}
    }

    emit(Event::Status {
        code: "remounting_developer_image",
        message: "正在刷新 Personalized DDI 服务",
    })?;
    mount_local_ddi(provider, ddi_dir, true).await?;
    let mut last_error = "mainTouchscreen service 257 is unavailable after DDI remount".into();
    for attempt in 0..3 {
        if attempt > 0 {
            emit(Event::Status {
                code: "waiting_for_hid_service",
                message: "正在等待 Personalized DDI 发布 Universal HID",
            })?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match connect_hid_session(provider).await {
            Ok(mut session) => match session.hid.list_connected_services().await {
                Ok(surfaces)
                    if surfaces
                        .iter()
                        .any(|surface| surface.service_id == MAIN_TOUCHSCREEN) =>
                {
                    return Ok(session);
                }
                Ok(surfaces) => last_error = describe_surfaces(&surfaces),
                Err(error) => last_error = error.to_string(),
            },
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error.into())
}

fn is_ddi_registration_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("service not found")
        || normalized.contains("no such service")
        || normalized.contains("universalhid")
        || normalized.contains("maintouchscreen")
}

async fn connect_session_from_transport(
    mut adapter: AdapterHandle,
    mut handshake: RsdHandshake,
    enable_media_gate: bool,
) -> Result<Session, Box<dyn std::error::Error>> {
    let (display, audio_udp, video_udp, auth_mode) = if !enable_media_gate {
        let display = DisplayServiceClient::connect_rsd(&mut adapter, &mut handshake)
            .await
            .ok();
        (display, None, None, "direct")
    } else {
        match start_media_gate(&mut adapter, &mut handshake).await {
            Ok((display, audio_udp, video_udp)) => (
                Some(display),
                Some(audio_udp),
                Some(video_udp),
                "mediastream",
            ),
            Err(error) if is_optional_media_gate_error(&error.to_string()) => {
                // iOS 18.x can reject or omit DisplayService while still exposing
                // Universal HID. Only a real HID surface determines readiness.
                let display = DisplayServiceClient::connect_rsd(&mut adapter, &mut handshake)
                    .await
                    .ok();
                (display, None, None, "direct")
            }
            Err(error) => return Err(error),
        }
    };
    // The bridge does not consume media packets, but the device will continue
    // sending them after start_media_gate. Drain both sockets so an auxiliary
    // control session cannot build an unbounded receive backlog beside the
    // host-owned projection stream. The adapter teardown closes these tasks.
    if let Some(audio_udp) = audio_udp {
        tokio::spawn(async move { drain_udp_socket(audio_udp).await });
    }
    if let Some(video_udp) = video_udp {
        tokio::spawn(async move { drain_udp_socket(video_udp).await });
    }
    let hid = connect_universal_hid(&mut adapter, &mut handshake).await?;
    let indigo = IndigoHidClient::connect_rsd(&mut adapter, &mut handshake).await?;
    let pasteboard = PasteboardServiceClient::connect_rsd(&mut adapter, &mut handshake).await?;
    Ok(Session {
        _adapter: adapter,
        _handshake: handshake,
        hid,
        indigo: Arc::new(tokio::sync::Mutex::new(indigo)),
        pasteboard: Arc::new(tokio::sync::Mutex::new(pasteboard)),
        _display: display,
        _audio_udp: None,
        _video_udp: None,
        auth_mode,
    })
}

async fn drain_udp_socket(socket: UdpSocketHandle) {
    loop {
        if socket.recv().await.is_err() {
            break;
        }
    }
}

async fn connect_universal_hid(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
) -> Result<UniversalHidServiceClient<Box<dyn idevice::ReadWrite>>, Box<dyn std::error::Error>> {
    let modern = (MODERN_UNIVERSAL_HID_SERVICE, MODERN_UNIVERSAL_HID_FEATURE);
    let legacy = (LEGACY_UNIVERSAL_HID_SERVICE, LEGACY_UNIVERSAL_HID_FEATURE);
    let candidates = if handshake
        .services
        .contains_key(MODERN_UNIVERSAL_HID_SERVICE)
    {
        [modern, legacy]
    } else {
        [legacy, modern]
    };
    let mut last_error = None;
    for (service, feature) in candidates {
        match UniversalHidServiceClient::connect_rsd_named(adapter, handshake, service, feature)
            .await
        {
            Ok(client) => return Ok(client),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .map(|error| error.into())
        .unwrap_or_else(|| "no compatible Universal HID service was available".into()))
}

fn is_optional_media_gate_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    message.contains("9021")
        || normalized.contains("requires ios")
        || normalized.contains("service not found")
        || normalized.contains("no such service")
}

async fn start_media_gate(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
) -> Result<
    (
        DisplayServiceClient<Box<dyn idevice::ReadWrite>>,
        UdpSocketHandle,
        UdpSocketHandle,
    ),
    Box<dyn std::error::Error>,
> {
    let mut display = DisplayServiceClient::connect_rsd(adapter, handshake).await?;
    let audio_udp = adapter.bind_udp(0).await?;
    let video_udp = adapter.bind_udp(0).await?;
    let receiver_ip = adapter.host_ip().to_string();
    let sender_ip = adapter.peer_ip().to_string();
    let call_info = CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "Mac17,7".into(),
        framework_version: "2205.3.1".into(),
        os_version: "25F71".into(),
        device_name: None,
        audio_device_uid: None,
    };
    let session_id = uuid::Uuid::new_v4();
    let audio_offer =
        build_screen_audio_offer(&uuid::Uuid::new_v4().to_string().to_uppercase(), &call_info)?;
    display
        .start_media_stream(build_start_audio_parameters(
            &receiver_ip,
            audio_udp.local_port(),
            &sender_ip,
            50000,
            audio_offer,
            140,
            session_id,
        ))
        .await?;
    let video_offer = build_screen_video_offer(
        &uuid::Uuid::new_v4().to_string().to_uppercase(),
        &call_info,
        uuid::Uuid::new_v4().as_u128() as u32,
    )?;
    display
        .start_media_stream(build_start_video_parameters(
            &receiver_ip,
            video_udp.local_port(),
            &sender_ip,
            50001,
            video_offer,
            140,
            1,
            session_id,
        ))
        .await?;
    Ok((display, audio_udp, video_udp))
}

async fn read_frame(
    stdin: &mut tokio::io::Stdin,
) -> Result<Option<InputFrame>, Box<dyn std::error::Error + Send + Sync>> {
    let mut length = [0u8; 4];
    match stdin.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let size = u32::from_le_bytes(length) as usize;
    if size == 0 || size > 4 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid frame length").into());
    }
    let mut payload = vec![0u8; size];
    stdin.read_exact(&mut payload).await?;
    Ok(Some(serde_json::from_slice(&payload)?))
}

fn emit(event: Event) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &event)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn describe_surfaces(surfaces: &[HidSurface]) -> String {
    let ids: Vec<String> = surfaces
        .iter()
        .map(|surface| surface.service_id.to_string())
        .collect();
    format!(
        "idevice did not publish mainTouchscreen service 257; surfaces={}",
        ids.join(",")
    )
}

fn allocate_slot(slots: &mut HashMap<u32, u8>, pointer_id: u32) -> Option<u8> {
    if let Some(identity) = slots.get(&pointer_id) {
        return Some(*identity);
    }
    let identity =
        (0..MAX_SLOTS as u8).find(|candidate| !slots.values().any(|value| value == candidate))?;
    slots.insert(pointer_id, identity);
    Some(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_stable_and_reused_only_after_release() {
        let mut slots = HashMap::new();
        assert_eq!(allocate_slot(&mut slots, 100), Some(0));
        assert_eq!(allocate_slot(&mut slots, 200), Some(1));
        assert_eq!(allocate_slot(&mut slots, 100), Some(0));
        slots.remove(&100);
        assert_eq!(allocate_slot(&mut slots, 300), Some(0));
    }

    #[test]
    fn orphan_move_allocates_slot_when_missing() {
        let mut slots = HashMap::new();
        let id = slots.get(&100).copied().or_else(|| allocate_slot(&mut slots, 100));
        assert_eq!(id, Some(0));
        assert_eq!(slots.get(&100), Some(&0));
    }

    #[test]
    fn sixth_contact_is_rejected() {
        let mut slots = HashMap::new();
        for pointer in 0..MAX_SLOTS as u32 {
            assert!(allocate_slot(&mut slots, pointer).is_some());
        }
        assert_eq!(allocate_slot(&mut slots, 99), None);
    }

    #[tokio::test]
    async fn recovery_discards_frames_buffered_before_the_new_hid_session() {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(4);
        let frame: InputFrame = serde_json::from_str(
            r#"{"schema":"iphoneMirror.touch.v2","kind":"touch_batch","seq":9,"points":[]}"#,
        )
        .unwrap();
        input_tx.try_send(InputMessage::Frame(frame)).unwrap();
        input_tx
            .try_send(InputMessage::Error("stdin closed during recovery".into()))
            .unwrap();

        assert_eq!(
            discard_stale_input_frames(&mut input_rx).await,
            (1, Some("stdin closed during recovery".into()))
        );
        assert!(input_rx.try_recv().is_err());
    }

    #[test]
    fn recovery_message_reports_discarded_frames() {
        assert_eq!(
            recovered_transport_message("触控", "socket io failed", 2),
            "触控通道短暂断开后已恢复，已丢弃恢复期间堆积的 2 条旧输入: socket io failed"
        );
    }

    #[test]
    fn pasteboard_confirmation_requires_the_exact_requested_text() {
        assert!(clipboard_text_matches("112233", Some("112233")));
        assert!(!clipboard_text_matches("112233", Some("")));
        assert!(!clipboard_text_matches("112233", Some("111222333")));
        assert!(!clipboard_text_matches("112233", None));
    }

    #[test]
    fn touch_frame_accepts_and_truncates_host_timestamp() {
        let frame: InputFrame = serde_json::from_str(
            r#"{"schema":"iphoneMirror.touch.v2","kind":"touch_batch","timestampNs":281474976710657,"points":[]}"#,
        )
        .unwrap();
        assert_eq!(
            frame.timestamp_ns.map(|value| value & ((1u64 << 48) - 1)),
            Some(1)
        );
    }

    #[test]
    fn git_blob_sha1_matches_git_known_vector() {
        assert_eq!(
            git_blob_sha1(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn coredevice_errors_have_stable_codes() {
        assert_eq!(
            coredevice_error_code("CoreDeviceError code 9021"),
            "coredevice_connection_failed"
        );
        assert_eq!(
            coredevice_error_code("service not found"),
            "developer_image_required"
        );
        assert_eq!(
            coredevice_error_code("device does not have pairing file"),
            "apple_device_not_trusted"
        );
        assert_eq!(
            coredevice_error_code("Developer mode is not enabled"),
            "developer_mode_required"
        );
        assert_eq!(
            device_connection_error_code("no matching Usb iOS device found"),
            "apple_device_not_found"
        );
        assert_eq!(
            device_connection_error_code("connection refused"),
            "apple_usbmux_unavailable"
        );
        assert_eq!(
            remote_pairing_error_code("RemotePairing pairing file is missing or invalid"),
            "wireless_remote_pairing_required"
        );
        assert_eq!(
            remote_pairing_error_code("no RemotePairing mDNS service found within 15 seconds"),
            "wireless_device_not_discoverable"
        );
        assert_eq!(
            coredevice_error_code("connection reset"),
            "coredevice_connection_failed"
        );
        assert_eq!(
            coredevice_error_message("CoreDeviceError code 9021: serialized plist bytes"),
            "CoreDeviceError code 9021: serialized plist bytes"
        );
        assert!(is_optional_media_gate_error("CoreDeviceError code 9021"));
        assert!(is_optional_media_gate_error(
            "Remote control requires iOS 27.0 or later"
        ));
        assert!(is_optional_media_gate_error("service not found"));
        assert!(is_ddi_registration_error("service not found"));
        assert!(is_ddi_registration_error(
            "mainTouchscreen service 257 is unavailable"
        ));
        assert!(!is_ddi_registration_error("connection reset"));
    }

    #[test]
    fn udid_matching_ignores_case_and_separators() {
        assert!(same_udid(
            "00008110-001234567890801E",
            "00008110001234567890801e"
        ));
        assert!(!same_udid(
            "00008110-001234567890801E",
            "00008110-001234567890801F"
        ));
    }

    #[test]
    fn transient_usb_socket_failures_are_retried_before_reporting_a_bad_ddi() {
        assert!(is_recoverable_usb_transport_error(
            "device socket io failed"
        ));
        assert!(is_recoverable_usb_transport_error(
            "ConnectionResetError: Connection lost"
        ));
        assert!(is_recoverable_usb_transport_error(
            "touch operation timed out"
        ));
        assert!(!is_recoverable_usb_transport_error(
            "Personalized image signature is invalid"
        ));
    }

    #[test]
    fn recognizes_real_personalized_ddi_copy_devices_entry() {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "DiskImageType".into(),
            plist::Value::String("Personalized".into()),
        );
        dict.insert(
            "PersonalizedImageType".into(),
            plist::Value::String("DeveloperDiskImage".into()),
        );
        dict.insert("IsMounted".into(), plist::Value::Boolean(true));
        let entry = plist::Value::Dictionary(dict);
        assert!(is_personalized_ddi_entry(&entry));
        assert!(!is_personalized_ddi_entry(&plist::Value::Dictionary(
            Default::default()
        )));
    }

    #[tokio::test]
    async fn cached_ddi_requires_matching_metadata_and_content() {
        let directory = env::temp_dir().join(format!("idevice-ddi-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let expected = [
            ("Image.dmg", "Image.dmg"),
            ("BuildManifest.plist", "BuildManifest.plist"),
            ("Image.trustcache", "Image.dmg.trustcache"),
        ];
        let contents = [
            ("Image.dmg", b"image".as_slice()),
            ("BuildManifest.plist", b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><string>manifest</string></plist>"),
            ("Image.trustcache", b"trust".as_slice()),
        ];
        let mut assets = Vec::new();
        for (name, content) in contents {
            tokio::fs::write(directory.join(name), content)
                .await
                .unwrap();
            assets.push(GithubAsset {
                name: if name == "Image.trustcache" {
                    "Image.dmg.trustcache".into()
                } else {
                    name.into()
                },
                sha: git_blob_sha1(content),
                size: content.len() as u64,
            });
        }
        let metadata = DdiCacheMetadata {
            commit: "a".repeat(40),
            assets,
        };
        tokio::fs::write(
            directory.join("idevice-ddi-metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .await
        .unwrap();

        assert!(valid_cached_ddi(&directory, &expected).await);
        tokio::fs::write(directory.join("Image.dmg"), b"tampered")
            .await
            .unwrap();
        assert!(!valid_cached_ddi(&directory, &expected).await);
        let _ = tokio::fs::remove_dir_all(&directory).await;
    }
}
