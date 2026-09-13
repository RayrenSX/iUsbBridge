//! Raw USB usbmux transport used when QuickTime owns the normal Apple mux.
//!
//! The implementation deliberately keeps the host-side usbmuxd surface small:
//! plist requests are served on localhost and each Connect socket is bridged to
//! one TCP-over-USB mux connection.  This is the same wire contract used by
//! Apple's usbmuxd, so `idevice` can use it without a special provider.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use nusb::{
    MaybeFuture, list_devices,
    transfer::{Bulk, ControlOut, ControlType, In, Out, Recipient},
};
use plist::{Dictionary, Value};

const APPLE_VID: u16 = 0x05ac;
const USBMUX_SUBCLASS: u8 = 0xfe;
const PROTO_VERSION: u32 = 0;
const PROTO_TCP: u32 = 6;
const MUX_MAGIC: u32 = 0xfeedface;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;
const USB_MTU: usize = 3 * 16384;
const DEV_MRU: usize = 65536;

fn serial_matches(left: &str, right: &str) -> bool {
    left.chars()
        .filter(|character| *character != '-')
        .map(|character| character.to_ascii_uppercase())
        .eq(right
            .chars()
            .filter(|character| *character != '-')
            .map(|character| character.to_ascii_uppercase()))
}

pub struct RawMux {
    stop: Arc<Mutex<bool>>,
    alive: Arc<AtomicBool>,
    join: Mutex<Vec<thread::JoinHandle<()>>>,
    client_join: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    listener: TcpListener,
}

struct MuxState {
    writer: Mutex<Box<dyn PacketWriter>>,
    version: Mutex<u32>,
    tx_seq: Mutex<u16>,
    connections: Mutex<HashMap<u16, Arc<MuxConnection>>>,
    next_sport: Mutex<u16>,
    serial: String,
}

struct MuxConnection {
    state: Mutex<ConnectionState>,
    cv: Condvar,
    mux: Arc<MuxState>,
    sport: u16,
    dport: u16,
}

struct ConnectionState {
    connected: bool,
    closed: bool,
    refused: bool,
    tx_seq: u32,
    tx_ack: u32,
    rx_ack: u32,
    rx_win: u32,
    inbox: Vec<u8>,
    pending: HashMap<u32, Vec<u8>>,
}

struct RawIo {
    reader: Box<dyn PacketReader>,
    writer: Box<dyn PacketWriter>,
}

trait PacketReader: Send {
    fn read_packet(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
}

trait PacketWriter: Send {
    fn write_packet(&mut self, packet: &[u8]) -> io::Result<()>;
}

struct NusbPacketWriter(nusb::io::EndpointWrite<Bulk>);

struct NusbPacketReader(nusb::io::EndpointRead<Bulk>);

impl PacketReader for NusbPacketReader {
    fn read_packet(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl PacketWriter for NusbPacketWriter {
    fn write_packet(&mut self, packet: &[u8]) -> io::Result<()> {
        self.0.write_all(packet)?;
        // usbmux frames are packet-delimited. This emits a ZLP when required,
        // matching the libusb0 behavior used by the original bridge.
        self.0.flush_end()
    }
}

#[cfg(windows)]
mod legacy {
    use super::{PacketReader, PacketWriter};
    use std::{
        ffi::c_void,
        io,
        sync::{Arc, Mutex},
    };
    unsafe extern "C" {
        fn im_libusb0_open(serial: *const i8, input: *mut i32, output: *mut i32) -> *mut c_void;
        fn im_libusb0_read(
            handle: *mut c_void,
            ep: i32,
            buffer: *mut i8,
            len: i32,
            timeout: i32,
        ) -> i32;
        fn im_libusb0_write(
            handle: *mut c_void,
            ep: i32,
            buffer: *mut i8,
            len: i32,
            timeout: i32,
        ) -> i32;
        fn im_libusb0_close(handle: *mut c_void);
    }
    pub struct LegacyReader {
        handle: Arc<Mutex<*mut c_void>>,
        ep: i32,
    }
    pub struct LegacyWriter {
        handle: Arc<Mutex<*mut c_void>>,
        ep: i32,
    }
    unsafe impl Send for LegacyReader {}
    unsafe impl Send for LegacyWriter {}
    impl PacketReader for LegacyReader {
        fn read_packet(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let h = *self.handle.lock().unwrap();
            let n = unsafe {
                im_libusb0_read(h, self.ep, b.as_mut_ptr() as *mut i8, b.len() as i32, 500)
            };
            if n >= 0 {
                Ok(n as usize)
            } else if matches!(n, -7 | -110 | -116) {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("libusb0 bulk read timeout {n}"),
                ))
            } else {
                Err(io::Error::other(format!("libusb0 bulk read {n}")))
            }
        }
    }
    impl PacketWriter for LegacyWriter {
        fn write_packet(&mut self, b: &[u8]) -> io::Result<()> {
            let h = *self.handle.lock().unwrap();
            let n = unsafe {
                im_libusb0_write(h, self.ep, b.as_ptr() as *mut i8, b.len() as i32, 2000)
            };
            if n == b.len() as i32 {
                Ok(())
            } else {
                Err(io::Error::other(format!("libusb0 bulk write {n}")))
            }
        }
    }
    pub fn open(serial: &str) -> Option<(Box<dyn PacketReader>, Box<dyn PacketWriter>)> {
        let c = std::ffi::CString::new(serial).ok()?;
        let mut i = 0;
        let mut o = 0;
        let h = unsafe { im_libusb0_open(c.as_ptr(), &mut i, &mut o) };
        if h.is_null() {
            return None;
        };
        let shared = Arc::new(Mutex::new(h));
        Some((
            Box::new(LegacyReader {
                handle: shared.clone(),
                ep: i,
            }),
            Box::new(LegacyWriter {
                handle: shared,
                ep: o,
            }),
        ))
    }
    impl Drop for LegacyWriter {
        fn drop(&mut self) {
            if Arc::strong_count(&self.handle) == 1 {
                let h = *self.handle.lock().unwrap();
                unsafe { im_libusb0_close(h) };
            }
        }
    }
}

impl RawMux {
    /// Claim the active Apple USB interface whose subclass is 0xFE and expose
    /// it on a free localhost port. Returns None when QuickTime is not active.
    pub fn start(
        udid: &str,
    ) -> Result<Option<Arc<Self>>, Box<dyn std::error::Error + Send + Sync>> {
        let info = list_devices().wait()?.find(|d| {
            d.vendor_id() == APPLE_VID
                && d.serial_number().is_some_and(|s| serial_matches(&s, udid))
        });
        let Some(info) = info else {
            #[cfg(windows)]
            if let Some((reader, writer)) = legacy::open(udid) {
                return Self::start_with_io(udid, reader, writer).map(Some);
            }
            return Ok(None);
        };
        let device = match info.open().wait() {
            Ok(device) => device,
            Err(error) => {
                #[cfg(windows)]
                if let Some((reader, writer)) = legacy::open(udid) {
                    return Self::start_with_io(udid, reader, writer).map(Some);
                }
                return Err(error.into());
            }
        };
        let config = device.active_configuration()?;
        let mut endpoints = None;
        for group in config.interfaces() {
            for intf in group.alt_settings() {
                if intf.class() != 0xff || intf.subclass() != USBMUX_SUBCLASS {
                    continue;
                }
                let mut input = None;
                let mut output = None;
                for ep in intf.endpoints() {
                    if ep.transfer_type() != nusb::descriptors::TransferType::Bulk {
                        continue;
                    }
                    if ep.address() & 0x80 != 0 {
                        input = Some(ep.address());
                    } else {
                        output = Some(ep.address());
                    }
                }
                if let (Some(input), Some(output)) = (input, output) {
                    endpoints = Some((intf.interface_number(), input, output));
                    break;
                }
            }
            if endpoints.is_some() {
                break;
            }
        }
        let Some((interface_number, in_ep, out_ep)) = endpoints else {
            #[cfg(windows)]
            if let Some((reader, writer)) = legacy::open(udid) {
                return Self::start_with_io(udid, reader, writer).map(Some);
            }
            return Err("Apple usbmux interface endpoints not found".into());
        };
        let interface = device.claim_interface(interface_number).wait()?;
        // QuickTime can leave either bulk endpoint halted when it hands the
        // hidden usbmux interface back to us.  The reference libusb backend
        // clears both halts before sending the VERSION packet; without this,
        // Windows WinUSB accepts the claim but silently drops all traffic.
        for endpoint in [in_ep, out_ep] {
            let _ = interface
                .control_out(
                    ControlOut {
                        control_type: ControlType::Standard,
                        recipient: Recipient::Endpoint,
                        request: 0x01, // CLEAR_FEATURE
                        value: 0,
                        index: endpoint as u16,
                        data: &[],
                    },
                    Duration::from_millis(1000),
                )
                .wait();
        }
        let reader = interface
            .endpoint::<Bulk, In>(in_ep)?
            .reader(DEV_MRU)
            .with_read_timeout(Duration::from_millis(500));
        let writer = interface.endpoint::<Bulk, Out>(out_ep)?.writer(USB_MTU);
        let io = RawIo {
            reader: Box::new(NusbPacketReader(reader)),
            writer: Box::new(NusbPacketWriter(writer)),
        };
        Self::start_with_io(udid, io.reader, io.writer).map(Some)
    }

    fn start_with_io(
        udid: &str,
        reader: Box<dyn PacketReader>,
        writer: Box<dyn PacketWriter>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(false)?;
        let state = Arc::new(MuxState {
            writer: Mutex::new(writer),
            version: Mutex::new(0),
            tx_seq: Mutex::new(0),
            connections: Mutex::new(HashMap::new()),
            next_sport: Mutex::new(1),
            serial: udid.to_owned(),
        });
        let stop = Arc::new(Mutex::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let client_join = Arc::new(Mutex::new(Vec::new()));
        let mut joins = Vec::new();
        let reader_state = Arc::clone(&state);
        let reader_stop = Arc::clone(&stop);
        let reader_alive = Arc::clone(&alive);
        joins.push(
            thread::Builder::new()
                .name("raw-usbmux-reader".into())
                .spawn(move || {
                    let mut reader = reader;
                    let mut buf = vec![0u8; DEV_MRU];
                    let mut pending = Vec::new();
                    while !*reader_stop.lock().unwrap() {
                        match reader.read_packet(&mut buf) {
                            Ok(0) => continue,
                            Ok(n) => {
                                pending.extend_from_slice(&buf[..n]);
                                feed_packets(&reader_state, &mut pending);
                            }
                            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
                            Err(_) => {
                                reader_alive.store(false, Ordering::Release);
                                break;
                            }
                        }
                    }
                    for conn in reader_state.connections.lock().unwrap().values() {
                        let mut s = conn.state.lock().unwrap();
                        s.closed = true;
                        conn.cv.notify_all();
                    }
                })?,
        );
        let accept_state = Arc::clone(&state);
        let accept_stop = Arc::clone(&stop);
        let accept_listener = listener.try_clone()?;
        let accept_clients = Arc::clone(&client_join);
        joins.push(
            thread::Builder::new()
                .name("raw-usbmux-server".into())
                .spawn(move || {
                    for stream in accept_listener.incoming() {
                        if *accept_stop.lock().unwrap() {
                            break;
                        }
                        if let Ok(stream) = stream {
                            reap_finished_client_threads(&accept_clients);
                            let st = Arc::clone(&accept_state);
                            let sp = Arc::clone(&accept_stop);
                            let client = thread::spawn(move || {
                                let _ = handle_client(stream, st, sp);
                            });
                            if let Ok(mut clients) = accept_clients.lock() {
                                clients.push(client);
                            }
                        }
                    }
                })?,
        );
        // The VERSION packet is sent before accepting clients, matching the
        // device-side mux state machine used by the original bridge.
        if let Err(error) = send_packet(&state, PROTO_VERSION, &version_payload(2, 0)) {
            stop_raw_mux(&stop, &listener, joins);
            return Err(error.into());
        }
        if !wait_for_version(&state, Duration::from_secs(5)) {
            stop_raw_mux(&stop, &listener, joins);
            return Err("device did not answer raw USB usbmux VERSION packet".into());
        }
        Ok(Arc::new(Self {
            stop,
            alive,
            join: Mutex::new(joins),
            client_join,
            listener,
        }))
    }

    pub fn address(&self) -> std::net::SocketAddr {
        self.listener
            .local_addr()
            .expect("raw usbmux listener address")
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !*self.stop.lock().unwrap()
    }
}

impl Drop for RawMux {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        *self.stop.lock().unwrap() = true;
        // Wake accept() without relying on platform-specific cancellation.
        let _ = TcpStream::connect(self.listener.local_addr().unwrap());
        for h in self.join.lock().unwrap().drain(..) {
            let _ = h.join();
        }
        // Client bridge threads hold the mux state (and therefore the USB
        // writer) through their Arc. Join them before allowing a new RawMux to
        // claim the same QuickTime interface.
        for h in self.client_join.lock().unwrap().drain(..) {
            let _ = h.join();
        }
    }
}

fn reap_finished_client_threads(handles: &Arc<Mutex<Vec<thread::JoinHandle<()>>>>) {
    let completed = {
        let Ok(mut clients) = handles.lock() else {
            return;
        };
        let mut active = Vec::with_capacity(clients.len());
        let mut completed = Vec::new();
        for handle in clients.drain(..) {
            if handle.is_finished() {
                completed.push(handle);
            } else {
                active.push(handle);
            }
        }
        *clients = active;
        completed
    };
    for handle in completed {
        let _ = handle.join();
    }
}

fn version_payload(major: u32, minor: u32) -> Vec<u8> {
    [major.to_be_bytes(), minor.to_be_bytes(), 0u32.to_be_bytes()].concat()
}

fn send_packet(state: &MuxState, proto: u32, body: &[u8]) -> io::Result<()> {
    let version = *state.version.lock().unwrap();
    let mut packet = Vec::with_capacity(body.len() + if version >= 2 { 16 } else { 8 });
    packet.extend_from_slice(&proto.to_be_bytes());
    let header = if version >= 2 { 16 } else { 8 };
    packet.extend_from_slice(&((header + body.len()) as u32).to_be_bytes());
    if version >= 2 {
        packet.extend_from_slice(&MUX_MAGIC.to_be_bytes());
        let mut seq = state.tx_seq.lock().unwrap();
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&0xffffu16.to_be_bytes());
        *seq = seq.wrapping_add(1);
    }
    packet.extend_from_slice(body);
    state.writer.lock().unwrap().write_packet(&packet)
}

fn feed_packets(state: &Arc<MuxState>, pending: &mut Vec<u8>) {
    loop {
        if pending.len() < 8 {
            return;
        }
        let len = u32::from_be_bytes(pending[4..8].try_into().unwrap()) as usize;
        if !(8..=DEV_MRU).contains(&len) {
            pending.clear();
            return;
        }
        if pending.len() < len {
            return;
        }
        let packet: Vec<_> = pending.drain(..len).collect();
        let proto = u32::from_be_bytes(packet[..4].try_into().unwrap());
        let version = *state.version.lock().unwrap();
        let body = if version >= 2 {
            if packet.len() < 16 {
                continue;
            } else {
                &packet[16..]
            }
        } else {
            &packet[8..]
        };
        match proto {
            PROTO_VERSION if body.len() >= 12 => {
                let major = u32::from_be_bytes(body[..4].try_into().unwrap());
                *state.version.lock().unwrap() = major;
                if major >= 2 {
                    let _ = send_packet(state, 2, &[7]);
                }
            }
            PROTO_TCP => feed_tcp(state, body),
            _ => {}
        }
    }
}

fn feed_tcp(state: &Arc<MuxState>, body: &[u8]) {
    if body.len() < 20 {
        return;
    }
    let _sport = u16::from_be_bytes(body[..2].try_into().unwrap());
    let dport = u16::from_be_bytes(body[2..4].try_into().unwrap());
    let seq = u32::from_be_bytes(body[4..8].try_into().unwrap());
    let ack = u32::from_be_bytes(body[8..12].try_into().unwrap());
    let flags = body[13];
    let win = u16::from_be_bytes(body[14..16].try_into().unwrap()) as u32 * 256;
    let offset = ((body[12] >> 4) as usize) * 4;
    let payload = if offset <= body.len() {
        &body[offset..]
    } else {
        &[]
    };
    let conn = state.connections.lock().unwrap().get(&dport).cloned();
    let Some(conn) = conn else { return };
    let mut cs = conn.state.lock().unwrap();
    let mut send_ack = false;
    cs.rx_ack = ack;
    cs.rx_win = win;
    if !cs.connected {
        if flags == (SYN | ACK) {
            cs.connected = true;
            cs.tx_seq = cs.tx_seq.wrapping_add(1);
            cs.tx_ack = seq.wrapping_add(1);
        } else {
            cs.refused = flags & RST != 0;
            cs.closed = true;
        }
    } else if flags & RST != 0 {
        cs.closed = true;
    } else if !payload.is_empty() {
        let expected = cs.tx_ack;
        if seq == expected {
            cs.inbox.extend_from_slice(payload);
            cs.tx_ack = seq.wrapping_add(payload.len() as u32);
            loop {
                let next_ack = cs.tx_ack;
                let Some(p) = cs.pending.remove(&next_ack) else {
                    break;
                };
                cs.inbox.extend_from_slice(&p);
                cs.tx_ack = cs.tx_ack.wrapping_add(p.len() as u32);
            }
        } else if seq.wrapping_sub(expected) < 0x8000_0000 {
            cs.pending.entry(seq).or_insert_with(|| payload.to_vec());
        }
        send_ack = true;
    }
    conn.cv.notify_all();
    drop(cs);
    if send_ack || flags == (SYN | ACK) {
        let _ = send_tcp(&conn, ACK, &[]);
    }
}

fn send_tcp(conn: &MuxConnection, flags: u8, data: &[u8]) -> io::Result<()> {
    let mut cs = conn.state.lock().unwrap();
    let sequence = cs.tx_seq;
    if !data.is_empty() {
        cs.tx_seq = cs.tx_seq.wrapping_add(data.len() as u32);
    }
    let mut body = Vec::with_capacity(20 + data.len());
    body.extend_from_slice(&conn.sport.to_be_bytes());
    body.extend_from_slice(&conn.dport.to_be_bytes());
    body.extend_from_slice(&sequence.to_be_bytes());
    body.extend_from_slice(&cs.tx_ack.to_be_bytes());
    body.push(0x50);
    body.push(flags);
    body.extend_from_slice(&((131072u32 / 256) as u16).to_be_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(data);
    drop(cs);
    send_packet(&conn.mux, PROTO_TCP, &body)
}

fn stop_raw_mux(
    stop: &Arc<Mutex<bool>>,
    listener: &TcpListener,
    joins: Vec<thread::JoinHandle<()>>,
) {
    *stop.lock().unwrap() = true;
    let _ = TcpStream::connect(listener.local_addr().unwrap());
    for join in joins {
        let _ = join.join();
    }
}

fn wait_for_version(state: &MuxState, timeout: Duration) -> bool {
    let start = Instant::now();
    while *state.version.lock().unwrap() == 0 && start.elapsed() < timeout {
        thread::sleep(Duration::from_millis(10));
    }
    *state.version.lock().unwrap() != 0
}

fn handle_client(
    mut stream: TcpStream,
    state: Arc<MuxState>,
    stop: Arc<Mutex<bool>>,
) -> io::Result<()> {
    // Make idle clients observable during RawMux shutdown. Apple clients send
    // small plist frames on localhost, so a bounded read timeout does not
    // change normal request latency but prevents Drop from waiting forever.
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    loop {
        if *stop.lock().unwrap() {
            return Ok(());
        }
        let (tag, req) = match read_plist_frame(&mut stream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let ty = req
            .get("MessageType")
            .and_then(Value::as_string)
            .unwrap_or("");
        match ty {
            "ReadBUID" => write_frame(
                &mut stream,
                tag,
                dict(&[("BUID", Value::String("30142955-444094379208051516".into()))]),
            )?,
            "ListDevices" => write_frame(
                &mut stream,
                tag,
                dict(&[(
                    "DeviceList",
                    Value::Array(vec![device_entry(&state.serial, 1)]),
                )]),
            )?,
            "Listen" => {
                write_frame(&mut stream, tag, result(0))?;
                write_frame(&mut stream, tag, attached_entry(&state.serial, 1))?;
            }
            "ReadPairRecord" => {
                let requested = req
                    .get("PairRecordID")
                    .and_then(Value::as_string)
                    .unwrap_or(&state.serial);
                match read_pair_record(requested, &state.serial) {
                    Some(bytes) => write_frame(
                        &mut stream,
                        tag,
                        dict(&[("PairRecordData", Value::Data(bytes))]),
                    )?,
                    None => write_frame(&mut stream, tag, result(2))?,
                }
            }
            "SavePairRecord" | "DeletePairRecord" => write_frame(
                &mut stream,
                tag,
                dict(&[
                    ("MessageType", Value::String("Result".into())),
                    ("Number", Value::Integer(0.into())),
                ]),
            )?,
            "Connect" => {
                let raw_id = req
                    .get("DeviceID")
                    .and_then(Value::as_signed_integer)
                    .unwrap_or(-1);
                let port_raw = req
                    .get("PortNumber")
                    .and_then(Value::as_signed_integer)
                    .unwrap_or(0) as u16;
                if raw_id != 1 {
                    write_frame(&mut stream, tag, result(2))?;
                    return Ok(());
                }
                let port = port_raw.rotate_right(8);
                let conn = open_connection(&state, port)?;
                write_frame(&mut stream, tag, result(0))?;
                return bridge_stream(&mut stream, conn, stop);
            }
            _ => write_frame(&mut stream, tag, result(1))?,
        }
    }
}

fn open_connection(state: &Arc<MuxState>, dport: u16) -> io::Result<Arc<MuxConnection>> {
    let mut next = state.next_sport.lock().unwrap();
    let sport = *next;
    *next = next.wrapping_add(1).max(1);
    drop(next);
    let conn = Arc::new(MuxConnection {
        state: Mutex::new(ConnectionState {
            connected: false,
            closed: false,
            refused: false,
            tx_seq: 0,
            tx_ack: 0,
            rx_ack: 0,
            rx_win: 0,
            inbox: Vec::new(),
            pending: HashMap::new(),
        }),
        cv: Condvar::new(),
        mux: Arc::clone(state),
        sport,
        dport,
    });
    state
        .connections
        .lock()
        .unwrap()
        .insert(sport, Arc::clone(&conn));
    send_tcp(&conn, SYN, &[])?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cs = conn.state.lock().unwrap();
    while !cs.connected && !cs.closed && Instant::now() < deadline {
        let (next, _) = conn
            .cv
            .wait_timeout(cs, Duration::from_millis(100))
            .unwrap();
        cs = next;
    }
    let connected = cs.connected;
    drop(cs);
    if connected {
        Ok(conn)
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "device usbmux connection refused",
        ))
    }
}

fn bridge_stream(
    stream: &mut TcpStream,
    conn: Arc<MuxConnection>,
    stop: Arc<Mutex<bool>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut buf = vec![0u8; 65536];
    loop {
        if *stop.lock().unwrap() {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut cs = conn.state.lock().unwrap();
                while cs.rx_win == 0 && !cs.closed {
                    let (next, _) = conn.cv.wait_timeout(cs, Duration::from_secs(3)).unwrap();
                    cs = next;
                }
                if cs.closed {
                    break;
                }
                drop(cs);
                send_tcp(&conn, ACK, &buf[..n])?;
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
        let mut cs = conn.state.lock().unwrap();
        if !cs.inbox.is_empty() {
            let out = std::mem::take(&mut cs.inbox);
            drop(cs);
            stream.write_all(&out)?;
        } else if cs.closed {
            break;
        }
    }
    let _ = send_tcp(&conn, RST, &[]);
    conn.mux.connections.lock().unwrap().remove(&conn.sport);
    Ok(())
}

fn result(number: i64) -> Dictionary {
    dict(&[
        ("MessageType", Value::String("Result".into())),
        ("Number", Value::Integer(number.into())),
    ])
}
fn device_entry(serial: &str, id: i64) -> Value {
    let properties = dict(&[
        ("DeviceID", Value::Integer(id.into())),
        ("SerialNumber", Value::String(serial.into())),
        ("ConnectionType", Value::String("USB".into())),
        ("ProductID", Value::Integer(0x12a8.into())),
    ]);
    Value::Dictionary(dict(&[
        ("DeviceID", Value::Integer(id.into())),
        ("MessageType", Value::String("Attached".into())),
        ("Properties", Value::Dictionary(properties)),
    ]))
}
fn attached_entry(serial: &str, id: i64) -> Dictionary {
    let Value::Dictionary(entry) = device_entry(serial, id) else {
        unreachable!()
    };
    entry
}
fn dict(items: &[(&str, Value)]) -> Dictionary {
    items
        .iter()
        .map(|(k, v)| (String::from(*k), v.clone()))
        .collect()
}
fn pair_record_dir() -> PathBuf {
    std::env::var_os("ALLUSERSPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Apple")
        .join("Lockdown")
}
fn read_pair_record(requested: &str, serial: &str) -> Option<Vec<u8>> {
    let ids = [
        requested.to_owned(),
        requested.replace('-', ""),
        serial.to_owned(),
        serial.replace('-', ""),
    ];
    for id in &ids {
        let path = pair_record_dir().join(format!("{id}.plist"));
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
    }
    None
}

fn read_plist_frame(stream: &mut TcpStream) -> io::Result<(u32, Dictionary)> {
    let mut h = [0u8; 16];
    stream.read_exact(&mut h)?;
    let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
    if !(16..=1 << 20).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad usbmux frame",
        ));
    }
    let mut body = vec![0u8; len - 16];
    stream.read_exact(&mut body)?;
    let value = Value::from_reader_xml(body.as_slice()).map_err(io::Error::other)?;
    value
        .into_dictionary()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "usbmux plist is not a dictionary",
            )
        })
        .map(|d| (u32::from_le_bytes(h[12..16].try_into().unwrap()), d))
}
fn write_frame(stream: &mut TcpStream, tag: u32, dict: Dictionary) -> io::Result<()> {
    let mut body = Vec::new();
    Value::Dictionary(dict)
        .to_writer_xml(&mut body)
        .map_err(io::Error::other)?;
    let len = (16 + body.len()) as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&1u32.to_le_bytes())?;
    stream.write_all(&8u32.to_le_bytes())?;
    stream.write_all(&tag.to_le_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_usb_serial_matching_ignores_udid_separators_and_case() {
        assert!(serial_matches(
            "0000810100044D600A22001E",
            "00008101-00044d600a22001e"
        ));
        assert!(!serial_matches(
            "0000810100044D600A22001E",
            "0000810100044D600A22001F"
        ));
    }

    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PacketWriter for CaptureWriter {
        fn write_packet(&mut self, packet: &[u8]) -> io::Result<()> {
            self.write_all(packet)
        }
    }

    fn test_connection() -> (Arc<MuxState>, Arc<MuxConnection>, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(MuxState {
            writer: Mutex::new(Box::new(CaptureWriter(Arc::clone(&output)))),
            version: Mutex::new(2),
            tx_seq: Mutex::new(0),
            connections: Mutex::new(HashMap::new()),
            next_sport: Mutex::new(2),
            serial: "test".into(),
        });
        let connection = Arc::new(MuxConnection {
            state: Mutex::new(ConnectionState {
                connected: false,
                closed: false,
                refused: false,
                tx_seq: 0,
                tx_ack: 0,
                rx_ack: 0,
                rx_win: 0,
                inbox: Vec::new(),
                pending: HashMap::new(),
            }),
            cv: Condvar::new(),
            mux: Arc::clone(&state),
            sport: 1,
            dport: 62078,
        });
        state
            .connections
            .lock()
            .unwrap()
            .insert(1, Arc::clone(&connection));
        (state, connection, output)
    }

    fn tcp_body(sport: u16, dport: u16, seq: u32, ack: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&sport.to_be_bytes());
        body.extend_from_slice(&dport.to_be_bytes());
        body.extend_from_slice(&seq.to_be_bytes());
        body.extend_from_slice(&ack.to_be_bytes());
        body.extend_from_slice(&[0x50, flags]);
        body.extend_from_slice(&512u16.to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(payload);
        body
    }

    #[test]
    fn attached_entry_matches_usbmuxd_usb_contract() {
        let entry = attached_entry("00008101-abc", 1);
        assert_eq!(
            entry.get("MessageType").and_then(Value::as_string),
            Some("Attached")
        );
        let properties = entry
            .get("Properties")
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            properties.get("ConnectionType").and_then(Value::as_string),
            Some("USB")
        );
        assert_eq!(
            properties.get("SerialNumber").and_then(Value::as_string),
            Some("00008101-abc")
        );
    }

    #[test]
    fn plist_frames_round_trip_tag_and_payload() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let (tag, request) = read_plist_frame(&mut socket).unwrap();
            assert_eq!(tag, 42);
            assert_eq!(
                request.get("MessageType").and_then(Value::as_string),
                Some("ListDevices")
            );
            write_frame(&mut socket, tag, result(0)).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        write_frame(
            &mut client,
            42,
            dict(&[("MessageType", Value::String("ListDevices".into()))]),
        )
        .unwrap();
        let (tag, reply) = read_plist_frame(&mut client).unwrap();
        assert_eq!(tag, 42);
        assert_eq!(
            reply.get("Number").and_then(Value::as_signed_integer),
            Some(0)
        );
        server.join().unwrap();
    }

    #[test]
    fn mux_version_payload_is_big_endian() {
        assert_eq!(
            version_payload(2, 0),
            vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn local_usbmuxd_handler_lists_and_listens_for_the_expected_device() {
        let (state, _, _) = test_connection();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(Mutex::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            handle_client(socket, state, server_stop).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        write_frame(
            &mut client,
            7,
            dict(&[("MessageType", Value::String("ListDevices".into()))]),
        )
        .unwrap();
        let (tag, reply) = read_plist_frame(&mut client).unwrap();
        assert_eq!(tag, 7);
        let devices = reply.get("DeviceList").and_then(Value::as_array).unwrap();
        let properties = devices[0]
            .as_dictionary()
            .unwrap()
            .get("Properties")
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            properties.get("SerialNumber").and_then(Value::as_string),
            Some("test")
        );

        write_frame(
            &mut client,
            8,
            dict(&[("MessageType", Value::String("Listen".into()))]),
        )
        .unwrap();
        assert_eq!(
            read_plist_frame(&mut client)
                .unwrap()
                .1
                .get("Number")
                .and_then(Value::as_signed_integer),
            Some(0)
        );
        assert_eq!(
            read_plist_frame(&mut client)
                .unwrap()
                .1
                .get("MessageType")
                .and_then(Value::as_string),
            Some("Attached")
        );
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn tcp_mux_reassembles_out_of_order_data_without_duplicate_retransmits() {
        let (state, connection, output) = test_connection();
        feed_tcp(&state, &tcp_body(62078, 1, 100, 1, SYN | ACK, &[]));
        assert!(connection.state.lock().unwrap().connected);
        assert!(!output.lock().unwrap().is_empty());

        feed_tcp(&state, &tcp_body(62078, 1, 106, 1, ACK, b"world"));
        feed_tcp(&state, &tcp_body(62078, 1, 101, 1, ACK, b"hello"));
        feed_tcp(&state, &tcp_body(62078, 1, 101, 1, ACK, b"hello"));
        let state = connection.state.lock().unwrap();
        assert_eq!(state.inbox, b"helloworld");
        assert_eq!(state.tx_ack, 111);
    }
}
