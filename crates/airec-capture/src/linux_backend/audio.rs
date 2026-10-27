use std::collections::HashMap;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use airec_core::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AirecError, AudioPacket, AudioSource, RecordingOptions,
    ResolvedTarget,
};
use pulseaudio::protocol::stream::{BufferAttr, StreamFlags};
use pulseaudio::protocol::{
    self, AuthParams, AuthReply, ChannelMap, Command, CommandReply, CreateRecordStreamReply,
    DESCRIPTOR_SIZE, GetSinkInfo, Prop, Props, ProtocolError, RecordStreamParams, SampleFormat,
    SampleSpec, SetClientNameReply, SinkInfo, SinkInputInfoList,
};

use crate::{AudioDiagnostics, audio_error};

const BYTES_PER_FRAME: u32 = 2 * AUDIO_CHANNELS as u32;
/// The server delivers record data in fragments of about this length.
const FRAGMENT_MS: u32 = 20;
/// New playback streams of the application are picked up within this interval.
const RESCAN_INTERVAL: Duration = Duration::from_millis(500);
/// How long the capture thread waits for data before checking whether it should stop.
const STOP_POLL_MS: i32 = 100;
const MAX_PROCESS_TREE_DEPTH: usize = 64;

/// Records each PulseAudio playback stream (sink input) of one process and its descendants
/// directly, so other applications playing to the same output are not included. PipeWire serves
/// the same protocol through `pipewire-pulse`.
struct PulseStreamSource {
    packets: Receiver<AudioPacket>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSource for PulseStreamSource {
    fn drain(&mut self) -> Vec<AudioPacket> {
        self.packets.try_iter().collect()
    }
}

impl Drop for PulseStreamSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn open_audio(
    target: &ResolvedTarget,
    options: &RecordingOptions,
) -> Result<Box<dyn AudioSource>, AirecError> {
    let pid = target.pid.ok_or_else(|| {
        audio_error(
            "the window does not publish _NET_WM_PID, so its application is unknown",
            target,
        )
    })?;
    let (ready_sender, ready) = mpsc::sync_channel(1);
    let (packet_sender, packets) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let started_at = options.started_at;
    let thread = std::thread::spawn({
        let stop = stop.clone();
        move || capture_process_audio(pid, started_at, &stop, &ready_sender, &packet_sender)
    });
    match ready.recv() {
        Ok(Ok(())) => Ok(Box::new(PulseStreamSource {
            packets,
            stop,
            thread: Some(thread),
        })),
        Ok(Err(message)) => {
            let _ = thread.join();
            Err(audio_error(message, target))
        }
        Err(_) => Err(audio_error(
            "the audio capture thread exited during startup",
            target,
        )),
    }
}

#[must_use]
pub fn diagnose_audio() -> AudioDiagnostics {
    let connection = PulseConnection::connect();
    AudioDiagnostics {
        available: connection.is_ok(),
        capture: "pulseaudio",
        codec: "Opus",
        error: connection.err(),
    }
}

fn capture_process_audio(
    pid: u32,
    started_at: Instant,
    stop: &AtomicBool,
    ready: &SyncSender<Result<(), String>>,
    packets: &Sender<AudioPacket>,
) {
    let mut connection = match PulseConnection::connect() {
        Ok(connection) => {
            let _ = ready.send(Ok(()));
            connection
        }
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    let mut capture = StreamCapture {
        pid,
        started_at,
        packets,
        sink_input_by_channel: HashMap::new(),
    };
    let mut next_scan = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let result = if Instant::now() >= next_scan {
            next_scan = Instant::now() + RESCAN_INTERVAL;
            capture.record_new_streams(&mut connection)
        } else {
            connection
                .wait_readable(STOP_POLL_MS)
                .and_then(|readable| {
                    if readable {
                        let packet = connection.read_packet()?;
                        capture.handle(packet);
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())
        };
        if let Err(error) = result {
            eprintln!("warning: audio capture of process {pid} stopped: {error}");
            return;
        }
    }
}

struct StreamCapture<'a> {
    pid: u32,
    started_at: Instant,
    packets: &'a Sender<AudioPacket>,
    sink_input_by_channel: HashMap<u32, u32>,
}

impl StreamCapture<'_> {
    fn handle(&mut self, packet: Packet) {
        match packet {
            Packet::Data { channel, bytes } => self.deliver(channel, &bytes),
            Packet::Control(message) => {
                if let Some(Command::RecordStreamKilled(channel)) = message.server_command() {
                    self.sink_input_by_channel.remove(&channel);
                }
            }
        }
    }

    fn deliver(&self, channel: u32, bytes: &[u8]) {
        if !self.sink_input_by_channel.contains_key(&channel) {
            return;
        }
        let received_us = self.started_at.elapsed().as_micros() as u64;
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let frames = (samples.len() / AUDIO_CHANNELS) as u64;
        let duration_us = frames * 1_000_000 / u64::from(AUDIO_SAMPLE_RATE);
        let _ = self.packets.send(AudioPacket {
            stream: u64::from(channel),
            t_us: received_us.saturating_sub(duration_us),
            samples,
        });
    }

    fn record_new_streams(&mut self, connection: &mut PulseConnection) -> Result<(), String> {
        let inputs: SinkInputInfoList = connection
            .request(Command::GetSinkInputInfoList, &mut |packet| {
                self.handle(packet)
            })?;
        for input in inputs {
            let recorded = self
                .sink_input_by_channel
                .values()
                .any(|&index| index == input.index);
            let owned = input
                .props
                .get(Prop::ApplicationProcessId)
                .and_then(parse_prop_u32)
                .is_some_and(|stream_pid| is_in_process_tree(stream_pid, self.pid));
            if recorded || !owned {
                continue;
            }
            let sink: SinkInfo = connection.request(
                Command::GetSinkInfo(GetSinkInfo {
                    index: Some(input.sink_index),
                    name: None,
                }),
                &mut |packet| self.handle(packet),
            )?;
            let created: Result<CreateRecordStreamReply, String> = connection.request(
                Command::CreateRecordStream(record_params(sink.monitor_source_index, input.index)),
                &mut |packet| self.handle(packet),
            );
            // The stream may have ended between listing and connecting; the next scan retries.
            if let Ok(stream) = created {
                self.sink_input_by_channel
                    .insert(stream.channel, input.index);
            }
        }
        Ok(())
    }
}

fn record_params(monitor_source: Option<u32>, sink_input: u32) -> RecordStreamParams {
    let mut props = Props::new();
    props.set(Prop::MediaName, c"airec window audio");
    RecordStreamParams {
        sample_spec: SampleSpec {
            format: SampleFormat::S16Le,
            channels: AUDIO_CHANNELS as u8,
            sample_rate: AUDIO_SAMPLE_RATE,
        },
        channel_map: ChannelMap::stereo(),
        source_index: monitor_source,
        direct_on_input_index: Some(sink_input),
        buffer_attr: BufferAttr {
            fragment_size: AUDIO_SAMPLE_RATE / 1_000 * FRAGMENT_MS * BYTES_PER_FRAME,
            ..BufferAttr::default()
        },
        flags: StreamFlags {
            adjust_latency: true,
            ..StreamFlags::default()
        },
        props,
        ..RecordStreamParams::default()
    }
}

fn parse_prop_u32(value: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(value).ok()?;
    text.trim_end_matches('\0').trim().parse().ok()
}

/// Browsers and other multi-process applications play audio from a child of the process that
/// owns the window.
fn is_in_process_tree(pid: u32, root: u32) -> bool {
    let mut current = pid;
    for _ in 0..MAX_PROCESS_TREE_DEPTH {
        if current == root {
            return true;
        }
        match parent_pid(current) {
            Some(parent) if parent != 0 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may itself contain spaces or parentheses.
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

enum Packet {
    Data { channel: u32, bytes: Vec<u8> },
    Control(ControlMessage),
}

/// A command or reply, kept with its descriptor so typed replies can be decoded by the library.
struct ControlMessage {
    message: Vec<u8>,
    version: u16,
}

impl ControlMessage {
    fn tag(&self) -> Result<(u32, Command), ProtocolError> {
        Command::read_tag_prefixed(
            &mut Cursor::new(&self.message[DESCRIPTOR_SIZE..]),
            self.version,
        )
    }

    fn server_command(&self) -> Option<Command> {
        self.tag().ok().map(|(_, command)| command)
    }
}

struct PulseConnection {
    socket: BufReader<UnixStream>,
    version: u16,
    next_seq: u32,
}

impl PulseConnection {
    fn connect() -> Result<Self, String> {
        let path = socket_path().ok_or_else(|| {
            "no PulseAudio or PipeWire server socket was found (checked PULSE_SERVER, PULSE_RUNTIME_PATH, XDG_RUNTIME_DIR, and /run/user/<uid>)".to_string()
        })?;
        let stream = UnixStream::connect(&path)
            .map_err(|error| format!("cannot connect to {}: {error}", path.display()))?;
        let mut connection = Self {
            socket: BufReader::new(stream),
            version: protocol::MAX_VERSION,
            next_seq: 0,
        };
        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|path| std::fs::read(path).ok())
            .unwrap_or_default();
        let auth: AuthReply = connection.request(
            Command::Auth(AuthParams {
                version: protocol::MAX_VERSION,
                supports_shm: false,
                supports_memfd: false,
                cookie,
            }),
            &mut |_| {},
        )?;
        connection.version = protocol::MAX_VERSION.min(auth.version);
        let mut props = Props::new();
        props.set(Prop::ApplicationName, c"airec");
        let _: SetClientNameReply =
            connection.request(Command::SetClientName(props), &mut |_| {})?;
        Ok(connection)
    }

    /// Sends a command and returns its reply, passing packets that arrive meanwhile to
    /// `on_other`.
    fn request<R: CommandReply>(
        &mut self,
        command: Command,
        on_other: &mut dyn FnMut(Packet),
    ) -> Result<R, String> {
        let seq = self.next_seq;
        self.next_seq += 1;
        protocol::write_command_message(self.socket.get_mut(), seq, &command, self.version)
            .map_err(|error| error.to_string())?;
        loop {
            let packet = self.read_packet().map_err(|error| error.to_string())?;
            let Packet::Control(message) = &packet else {
                on_other(packet);
                continue;
            };
            match message.tag() {
                Ok((reply_seq, Command::Reply)) if reply_seq == seq => {
                    return protocol::read_reply_message::<R>(
                        &mut Cursor::new(&message.message),
                        self.version,
                    )
                    .map(|(_, reply)| reply)
                    .map_err(|error| error.to_string());
                }
                Ok((reply_seq, Command::Error(error))) if reply_seq == seq => {
                    return Err(format!("PulseAudio server refused the request: {error:?}"));
                }
                _ => on_other(packet),
            }
        }
    }

    fn read_packet(&mut self) -> Result<Packet, ProtocolError> {
        let descriptor = protocol::read_descriptor(&mut self.socket)?;
        let mut payload = vec![0; descriptor.length as usize];
        self.socket.read_exact(&mut payload)?;
        if descriptor.channel != u32::MAX {
            return Ok(Packet::Data {
                channel: descriptor.channel,
                bytes: payload,
            });
        }
        let mut message = Vec::with_capacity(DESCRIPTOR_SIZE + payload.len());
        protocol::write_descriptor(&mut message, &descriptor)?;
        message.extend_from_slice(&payload);
        Ok(Packet::Control(ControlMessage {
            message,
            version: self.version,
        }))
    }

    fn wait_readable(&mut self, timeout_ms: i32) -> Result<bool, ProtocolError> {
        if !self.socket.buffer().is_empty() {
            return Ok(true);
        }
        let mut descriptor = libc::pollfd {
            fd: self.socket.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` is one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error.into());
        }
        if ready > 0 && descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // Reading reports the closed connection as an error.
            let _ = self.socket.fill_buf()?;
        }
        Ok(ready > 0)
    }
}

/// The library's own lookup stops at the first candidate that does not exist, and a detached
/// recorder can run without `XDG_RUNTIME_DIR`, so every location is tried here.
fn socket_path() -> Option<PathBuf> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let from_server = std::env::var("PULSE_SERVER")
        .ok()
        .and_then(|value| value.strip_prefix("unix:").map(PathBuf::from));
    let from_runtime_path =
        std::env::var_os("PULSE_RUNTIME_PATH").map(|path| PathBuf::from(path).join("native"));
    let from_xdg =
        std::env::var_os("XDG_RUNTIME_DIR").map(|path| PathBuf::from(path).join("pulse/native"));
    let from_uid = Some(PathBuf::from(format!("/run/user/{uid}/pulse/native")));
    [from_server, from_runtime_path, from_xdg, from_uid]
        .into_iter()
        .flatten()
        .find(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_id_property_is_parsed_without_its_terminator() {
        assert_eq!(parse_prop_u32(b"4321\0"), Some(4321));
        assert_eq!(parse_prop_u32(b"not-a-pid\0"), None);
    }

    #[test]
    fn a_process_belongs_to_its_own_tree_and_to_its_ancestors_trees() {
        let own = std::process::id();
        let parent = parent_pid(own).unwrap();

        assert!(is_in_process_tree(own, own));
        assert!(is_in_process_tree(own, parent));
        assert!(!is_in_process_tree(parent, own));
    }

    #[test]
    fn record_stream_targets_the_sink_input_on_the_sink_monitor() {
        let params = record_params(Some(7), 42);

        assert_eq!(params.source_index, Some(7));
        assert_eq!(params.direct_on_input_index, Some(42));
        assert_eq!(params.sample_spec.sample_rate, AUDIO_SAMPLE_RATE);
        assert_eq!(params.buffer_attr.fragment_size, 3_840);
    }
}
