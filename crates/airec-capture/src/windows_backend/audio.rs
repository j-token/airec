use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::Instant;

use airec_core::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AirecError, AudioPacket, AudioSource, RecordingOptions,
    ResolvedTarget,
};
use wasapi::{
    AudioCaptureClient, AudioClient, Direction, Handle, SampleType, StreamMode, WasapiError,
    WaveFormat, initialize_mta,
};

use crate::{AudioDiagnostics, audio_error};

const BUFFER_DURATION_HNS: i64 = 200_000;
/// How long the capture thread waits for audio before checking whether it should stop.
const STOP_POLL_MS: u32 = 100;

/// Captures what one process and its child processes play, through WASAPI process loopback.
/// A browser plays audio from a child process of the one that owns its window, so the tree is
/// included.
struct ProcessLoopbackSource {
    packets: Receiver<AudioPacket>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSource for ProcessLoopbackSource {
    fn drain(&mut self) -> Vec<AudioPacket> {
        self.packets.try_iter().collect()
    }
}

impl Drop for ProcessLoopbackSource {
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
    let pid = target
        .pid
        .ok_or_else(|| audio_error("the window's process id is unknown", target))?;
    let (ready_sender, ready) = mpsc::sync_channel(1);
    let (packet_sender, packets) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let started_at = options.started_at;
    let thread = std::thread::spawn({
        let stop = stop.clone();
        move || capture_process_audio(pid, started_at, &stop, &ready_sender, &packet_sender)
    });
    match ready.recv() {
        Ok(Ok(())) => Ok(Box::new(ProcessLoopbackSource {
            packets,
            stop,
            thread: Some(thread),
        })),
        Ok(Err(message)) => {
            let _ = thread.join();
            Err(audio_error(
                format!("cannot capture the audio of process {pid}: {message}"),
                target,
            ))
        }
        Err(_) => Err(audio_error(
            "the audio capture thread exited during startup",
            target,
        )),
    }
}

#[must_use]
pub fn diagnose_audio() -> AudioDiagnostics {
    let probe = std::thread::spawn(|| {
        let _ = initialize_mta();
        start_loopback(std::process::id()).map(drop)
    })
    .join()
    .unwrap_or_else(|_| Err(WasapiError::EventTimeout));
    AudioDiagnostics {
        available: probe.is_ok(),
        capture: "process_loopback",
        codec: "AAC",
        error: probe.err().map(|error| error.to_string()),
    }
}

struct LoopbackCapture {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
}

fn start_loopback(pid: u32) -> Result<LoopbackCapture, WasapiError> {
    let format = WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        AUDIO_SAMPLE_RATE as usize,
        AUDIO_CHANNELS,
        None,
    );
    let mut client = AudioClient::new_application_loopback_client(pid, true)?;
    client.initialize_client(
        &format,
        &Direction::Capture,
        &StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: BUFFER_DURATION_HNS,
        },
    )?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    Ok(LoopbackCapture {
        client,
        capture,
        event,
    })
}

fn capture_process_audio(
    pid: u32,
    started_at: Instant,
    stop: &AtomicBool,
    ready: &SyncSender<Result<(), String>>,
    packets: &Sender<AudioPacket>,
) {
    // Activation needs COM on this thread; a thread that already joined an apartment keeps it.
    let _ = initialize_mta();
    let loopback = match start_loopback(pid) {
        Ok(loopback) => {
            let _ = ready.send(Ok(()));
            loopback
        }
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let mut bytes = VecDeque::new();
    while !stop.load(Ordering::Acquire) {
        if loopback.event.wait_for_event(STOP_POLL_MS).is_err() {
            continue;
        }
        loop {
            match loopback.capture.get_next_packet_size() {
                Ok(Some(frames)) if frames > 0 => {}
                Ok(_) => break,
                Err(error) => {
                    eprintln!("warning: audio capture of process {pid} stopped: {error}");
                    return;
                }
            }
            let info = match loopback.capture.read_from_device_to_deque(&mut bytes) {
                Ok(info) => info,
                Err(error) => {
                    eprintln!("warning: audio capture of process {pid} stopped: {error}");
                    return;
                }
            };
            let received_us = started_at.elapsed().as_micros() as u64;
            let samples: Vec<i16> = if info.flags.silent {
                vec![0; bytes.len() / 2]
            } else {
                bytes
                    .make_contiguous()
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect()
            };
            bytes.clear();
            let frames = (samples.len() / AUDIO_CHANNELS) as u64;
            let duration_us = frames * 1_000_000 / u64::from(AUDIO_SAMPLE_RATE);
            if packets
                .send(AudioPacket {
                    stream: 0,
                    t_us: received_us.saturating_sub(duration_us),
                    samples,
                })
                .is_err()
            {
                return;
            }
        }
    }
    let _ = loopback.client.stop_stream();
}
