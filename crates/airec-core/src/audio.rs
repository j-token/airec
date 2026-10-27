use std::collections::{HashMap, VecDeque};

use crate::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AirecError, AudioPacket, AudioSource, PipelineEncoder,
};

/// Audio is written this far behind the video clock, so a packet that the capture API delivers
/// late still lands in its place instead of after silence that was already written for it.
pub(crate) const AUDIO_DELIVERY_LATENCY_MS: u64 = 250;
/// A stream's packets are laid end to end while their capture times agree within this margin.
/// Beyond it the stream restarts at its capture time, as after playback was paused.
const STREAM_RESYNC_TOLERANCE_MS: u64 = 80;

/// Turns timestamped packets from several playback streams into one gapless track that starts
/// at the session timeline origin, filling time without packets with silence.
pub(crate) struct AudioTrack {
    source: Box<dyn AudioSource>,
    origin_frame: i64,
    written_frames: u64,
    /// Mixed interleaved samples from `written_frames` onward.
    pending: VecDeque<i32>,
    next_frame_by_stream: HashMap<u64, i64>,
}

impl AudioTrack {
    pub(crate) fn new(source: Box<dyn AudioSource>, origin_ms: u64) -> Self {
        Self {
            source,
            origin_frame: frames_at_us(origin_ms.saturating_mul(1_000)),
            written_frames: 0,
            pending: VecDeque::new(),
            next_frame_by_stream: HashMap::new(),
        }
    }

    /// Writes the track up to `t_ms` on the timeline relative to the origin.
    pub(crate) fn write_until(
        &mut self,
        encoder: &mut dyn PipelineEncoder,
        t_ms: u64,
    ) -> Result<(), AirecError> {
        for packet in self.source.drain() {
            self.mix(&packet);
        }
        let end_frame = t_ms.saturating_mul(u64::from(AUDIO_SAMPLE_RATE)) / 1_000;
        let samples = self.take_until(end_frame);
        if samples.is_empty() {
            return Ok(());
        }
        encoder.write_audio(&samples)
    }

    fn mix(&mut self, packet: &AudioPacket) {
        let frames = (packet.samples.len() / AUDIO_CHANNELS) as i64;
        if frames == 0 {
            return;
        }
        let captured_frame = frames_at_us(packet.t_us) - self.origin_frame;
        let tolerance = (STREAM_RESYNC_TOLERANCE_MS * u64::from(AUDIO_SAMPLE_RATE) / 1_000) as i64;
        let start_frame = match self.next_frame_by_stream.get(&packet.stream) {
            Some(&expected) if (captured_frame - expected).abs() <= tolerance => expected,
            _ => captured_frame,
        };
        self.next_frame_by_stream
            .insert(packet.stream, start_frame + frames);

        let written_frames = self.written_frames as i64;
        let late_frames = (written_frames - start_frame).max(0);
        if late_frames >= frames {
            return;
        }
        let samples = &packet.samples[late_frames as usize * AUDIO_CHANNELS..];
        let offset = (start_frame + late_frames - written_frames) as usize * AUDIO_CHANNELS;
        if self.pending.len() < offset + samples.len() {
            self.pending.resize(offset + samples.len(), 0);
        }
        for (mixed, &sample) in self.pending.iter_mut().skip(offset).zip(samples) {
            *mixed += i32::from(sample);
        }
    }

    fn take_until(&mut self, end_frame: u64) -> Vec<i16> {
        let Some(frames) = end_frame
            .checked_sub(self.written_frames)
            .filter(|&frames| frames > 0)
        else {
            return Vec::new();
        };
        let len = frames as usize * AUDIO_CHANNELS;
        if self.pending.len() < len {
            self.pending.resize(len, 0);
        }
        self.written_frames = end_frame;
        self.pending
            .drain(..len)
            .map(|sample| sample.clamp(i16::MIN.into(), i16::MAX.into()) as i16)
            .collect()
    }
}

fn frames_at_us(t_us: u64) -> i64 {
    (i128::from(t_us) * i128::from(AUDIO_SAMPLE_RATE) / 1_000_000) as i64
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::CaptureFrame;

    struct QueuedSource(Arc<Mutex<Vec<AudioPacket>>>);

    impl AudioSource for QueuedSource {
        fn drain(&mut self) -> Vec<AudioPacket> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    #[derive(Default)]
    struct RecordingEncoder(Vec<i16>);

    impl PipelineEncoder for RecordingEncoder {
        fn write_frame(&mut self, _frame: &CaptureFrame) -> Result<(), AirecError> {
            Ok(())
        }

        fn write_audio(&mut self, samples: &[i16]) -> Result<(), AirecError> {
            self.0.extend_from_slice(samples);
            Ok(())
        }

        fn finish(&mut self) -> Result<(), AirecError> {
            Ok(())
        }
    }

    fn packet(stream: u64, t_us: u64, frames: usize, value: i16) -> AudioPacket {
        AudioPacket {
            stream,
            t_us,
            samples: vec![value; frames * AUDIO_CHANNELS],
        }
    }

    fn track(origin_ms: u64) -> (AudioTrack, Arc<Mutex<Vec<AudioPacket>>>) {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let track = AudioTrack::new(Box::new(QueuedSource(queue.clone())), origin_ms);
        (track, queue)
    }

    fn left_channel(samples: &[i16]) -> Vec<i16> {
        samples.iter().step_by(AUDIO_CHANNELS).copied().collect()
    }

    #[test]
    fn time_without_packets_is_written_as_silence() {
        let (mut track, _) = track(0);
        let mut encoder = RecordingEncoder::default();

        track.write_until(&mut encoder, 10).unwrap();

        assert_eq!(encoder.0, vec![0; 480 * AUDIO_CHANNELS]);
    }

    #[test]
    fn jittered_packets_of_one_stream_are_laid_end_to_end() {
        let (mut track, queue) = track(0);
        let mut encoder = RecordingEncoder::default();
        queue.lock().unwrap().extend([
            packet(1, 0, 2, 1),
            // Two frames are 41.6 µs; a capture time 1 ms late is jitter, not a gap.
            packet(1, 1_000, 2, 2),
        ]);

        track.write_until(&mut encoder, 1).unwrap();

        assert_eq!(left_channel(&encoder.0)[..6], [1, 1, 2, 2, 0, 0]);
    }

    #[test]
    fn a_stream_resumed_after_a_pause_starts_at_its_capture_time() {
        let (mut track, queue) = track(0);
        let mut encoder = RecordingEncoder::default();
        queue
            .lock()
            .unwrap()
            .extend([packet(1, 0, 1, 1), packet(1, 100_000, 1, 2)]);

        track.write_until(&mut encoder, 101).unwrap();

        let left = left_channel(&encoder.0);
        assert_eq!(left[0], 1);
        assert_eq!(left[1], 0);
        assert_eq!(left[4_800], 2);
    }

    #[test]
    fn packets_before_the_origin_are_trimmed() {
        let (mut track, queue) = track(1);
        let mut encoder = RecordingEncoder::default();
        // Starts 1 ms (48 frames) before the origin.
        queue.lock().unwrap().push(packet(1, 0, 50, 7));

        track.write_until(&mut encoder, 1).unwrap();

        assert_eq!(left_channel(&encoder.0)[..3], [7, 7, 0]);
    }

    #[test]
    fn concurrent_streams_are_mixed_and_clipped() {
        let (mut track, queue) = track(0);
        let mut encoder = RecordingEncoder::default();
        queue.lock().unwrap().extend([
            packet(1, 0, 1, 20_000),
            packet(2, 0, 1, 20_000),
            packet(3, 0, 1, 5),
        ]);

        track.write_until(&mut encoder, 1).unwrap();

        assert_eq!(left_channel(&encoder.0)[0], i16::MAX);
    }

    #[test]
    fn audio_that_arrives_after_its_time_was_written_is_dropped() {
        let (mut track, queue) = track(0);
        let mut encoder = RecordingEncoder::default();
        track.write_until(&mut encoder, 1).unwrap();
        queue.lock().unwrap().push(packet(1, 0, 50, 3));

        track.write_until(&mut encoder, 2).unwrap();

        let left = left_channel(&encoder.0);
        assert_eq!(left.len(), 96);
        assert_eq!(left[47], 0);
        assert_eq!(left[48], 3);
        assert_eq!(left[50], 0);
    }
}
