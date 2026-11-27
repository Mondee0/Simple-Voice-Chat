//! Microphone capture and voice playback, both owned by this mod: Weft's SDK plays registered
//! sound assets and has no raw PCM input or output, so we open our own cpal streams.

use crate::codec::{self, FRAME, HEADER, RATE};
use crate::state;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use weft_sdk::{info, warn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct Audio {
    state: Arc<Mutex<Engine>>,
    // Streams stop when dropped; keep them for as long as the client is alive.
    _streams: Vec<Stream>,
    run: Arc<AtomicBool>,
}

static AUDIO: Mutex<Option<Audio>> = Mutex::new(None);
static FAILED: AtomicBool = AtomicBool::new(false);
/// The devices chosen in the settings panel; `None` means the system default.
static SELECTED: Mutex<(Option<String>, Option<String>)> = Mutex::new((None, None));
static REBUILD: AtomicBool = AtomicBool::new(false);

pub fn device_names(input: bool) -> Vec<String> {
    let host = cpal::default_host();
    let found = if input { host.input_devices() } else { host.output_devices() };
    let mut names: Vec<String> = found
        .into_iter()
        .flatten()
        .filter_map(|device| device.description().ok().map(|d| d.name().to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The name shown in the settings panel for the currently selected side.
pub fn device_label(input: bool) -> String {
    let chosen = {
        let selected = SELECTED.lock();
        if input { selected.0.clone() } else { selected.1.clone() }
    };
    if let Some(name) = chosen {
        return name;
    }
    let host = cpal::default_host();
    let default = if input { host.default_input_device() } else { host.default_output_device() };
    default
        .and_then(|d| d.description().ok().map(|description| description.name().to_string()))
        .unwrap_or_else(|| "none".into())
}

/// Cycle to the next available device and mark the streams for rebuilding.
pub fn select_next(input: bool) {
    let names = device_names(input);
    if names.is_empty() {
        return;
    }
    let mut selected = SELECTED.lock();
    let current = if input { selected.0.clone() } else { selected.1.clone() };
    let next = current
        .as_ref()
        .and_then(|name| names.iter().position(|n| n == name))
        .map_or(0, |i| (i + 1) % names.len());
    let name = names[next].clone();
    if input {
        selected.0 = Some(name);
    } else {
        selected.1 = Some(name);
    }
    drop(selected);
    REBUILD.store(true, Ordering::Relaxed);
}

/// Select a specific device by name and rebuild the streams on the next frame.
pub fn select(input: bool, name: &str) {
    let mut selected = SELECTED.lock();
    if input {
        selected.0 = Some(name.to_string());
    } else {
        selected.1 = Some(name.to_string());
    }
    drop(selected);
    REBUILD.store(true, Ordering::Relaxed);
}

pub fn take_rebuild() -> bool {
    REBUILD.swap(false, Ordering::Relaxed)
}

/// Drops the old streams and stops its worker, so the next `state()` reopens the chosen devices.
pub fn rebuild() {
    let mut audio = AUDIO.lock();
    if let Some(old) = audio.as_mut() {
        old.run.store(false, Ordering::Relaxed);
    }
    *audio = None;
    FAILED.store(false, Ordering::Relaxed);
}

pub fn state() -> Option<Arc<Mutex<Engine>>> {
    if FAILED.load(Ordering::Relaxed) {
        return None;
    }
    let mut audio = AUDIO.lock();
    if audio.is_none() {
        match Audio::start() {
            Ok(a) => *audio = Some(a),
            Err(e) => {
                FAILED.store(true, Ordering::Relaxed);
                warn!("simple_voice_chat: audio unavailable: {e}");
                return None;
            }
        }
    }
    audio.as_ref().map(|a| a.state.clone())
}

#[derive(Clone, Copy, Default)]
pub struct Settings {
    pub mode: u32,
    pub muted: bool,
    pub threshold: f32,
    pub gain: f32,
    pub volume: f32,
    pub range: f32,
    pub recording: bool,
    pub suppress: bool,
    pub auto_gain: bool,
    pub audio3d: u32,
}

pub struct Engine {
    in_rate: u32,
    in_channels: u16,
    out_rate: u32,
    raw: VecDeque<f32>,
    pos: f64,
    frame: [f32; FRAME],
    filled: usize,
    gate: bool,
    seq: u32,
    speakers: HashMap<u64, Speaker>,
    pub settings: Settings,
    recording: Vec<i16>,
    recording_on: bool,
    hp_x: f32,
    hp_y: f32,
}

struct Speaker {
    samples: VecDeque<f32>,
    pos: f64,
    seq: u32,
    last: Instant,
    world_pos: [f32; 3],
    group: u64,
    volume: f32,
}

impl Audio {
    fn start() -> Result<Audio, String> {
        let host = cpal::default_host();
        let selected = SELECTED.lock().clone();
        let find = |input: bool, name: &Option<String>| {
            if let Some(wanted) = name {
                let devices = if input { host.input_devices() } else { host.output_devices() };
                devices.ok().and_then(|ds| {
                    ds.into_iter().find(|d| {
                        d.description().map_or(false, |desc| desc.name() == wanted.as_str())
                    })
                })
            } else if input {
                host.default_input_device()
            } else {
                host.default_output_device()
            }
        };
        let input = find(true, &selected.0).ok_or("no matching microphone")?;
        let output = find(false, &selected.1).ok_or("no matching playback device")?;
        let in_supported = input
            .default_input_config()
            .map_err(|e| format!("microphone config: {e}"))?;
        let in_format = in_supported.sample_format();
        let in_cfg: StreamConfig = in_supported.into();
        let out_supported = output
            .default_output_config()
            .map_err(|e| format!("speaker config: {e}"))?;
        let out_format = out_supported.sample_format();
        let out_cfg: StreamConfig = out_supported.into();

        let engine = Arc::new(Mutex::new(Engine::new(
            in_cfg.sample_rate,
            in_cfg.channels,
            out_cfg.sample_rate,
        )));
        let error = |e| warn!("simple_voice_chat audio: {e}");

        let mic = engine.clone();
        let in_stream = match in_format {
            SampleFormat::F32 => input.build_input_stream(
                in_cfg,
                move |data: &[f32], _| mic.lock().push_input(data),
                error.clone(),
                None,
            ),
            SampleFormat::I16 => input.build_input_stream(
                in_cfg,
                move |data: &[i16], _| {
                    let v: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                    mic.lock().push_input(&v);
                },
                error.clone(),
                None,
            ),
            f => return Err(format!("unsupported microphone sample format {f:?}")),
        }
        .map_err(|e| format!("microphone stream: {e}"))?;

        let ears = engine.clone();
        let channels = out_cfg.channels as usize;
        let out_stream = match out_format {
            SampleFormat::F32 => output.build_output_stream(
                out_cfg,
                move |data: &mut [f32], _| ears.lock().render(data, channels),
                error.clone(),
                None,
            ),
            SampleFormat::I16 => output.build_output_stream(
                out_cfg,
                move |data: &mut [i16], _| {
                    let mut v = vec![0.0f32; data.len()];
                    ears.lock().render(&mut v, channels);
                    for (d, s) in data.iter_mut().zip(v) {
                        *d = (s * 32767.0) as i16;
                    }
                },
                error,
                None,
            ),
            f => return Err(format!("unsupported playback sample format {f:?}")),
        }
        .map_err(|e| format!("playback stream: {e}"))?;

        let worker = engine.clone();
        let run = Arc::new(AtomicBool::new(true));
        let worker_run = run.clone();
        std::thread::Builder::new()
            .name("svc-microphone".into())
            .spawn(move || {
                while worker_run.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                    worker.lock().pump();
                }
            })
            .map_err(|e| format!("audio thread: {e}"))?;

        in_stream.play().map_err(|e| format!("microphone start: {e}"))?;
        out_stream.play().map_err(|e| format!("playback start: {e}"))?;
        Ok(Audio { state: engine, _streams: vec![in_stream, out_stream], run })
    }
}

impl Engine {
    fn new(in_rate: u32, in_channels: u16, out_rate: u32) -> Engine {
        Engine {
            in_rate,
            in_channels,
            out_rate,
            raw: VecDeque::new(),
            pos: 0.0,
            frame: [0.0; FRAME],
            filled: 0,
            gate: false,
            seq: 0,
            speakers: HashMap::new(),
            settings: Settings::default(),
            recording: Vec::new(),
            recording_on: false,
            hp_x: 0.0,
            hp_y: 0.0,
        }
    }

    pub fn configure(&mut self, s: Settings) {
        if s.recording != self.recording_on {
            if s.recording && !self.recording.is_empty() {
                self.recording.clear();
            }
            self.recording_on = s.recording;
        }
        self.settings = s;
    }

    /// Called from the microphone callback on cpal's thread.
    fn push_input(&mut self, data: &[f32]) {
        let cap = self.in_rate as usize * 2;
        let channels = self.in_channels.max(1) as usize;
        if channels == 1 {
            self.raw.extend(data.iter().copied());
        } else {
            // Microphones often come in as interleaved stereo; average them to mono or the
            // resampler would hop between channels and turn speech into noise.
            for frame in data.chunks(channels) {
                self.raw.push_back(frame.iter().sum::<f32>() / channels as f32);
            }
        }
        while self.raw.len() > cap {
            self.raw.pop_front();
            self.pos = (self.pos - 1.0).max(0.0);
        }
    }

    /// 20 ms tick on our own thread: turn device-rate mic samples into a voice frame.
    fn pump(&mut self) {
        let step = self.in_rate as f64 / RATE as f64;
        let gain = self.settings.gain;
        while self.pos + 1.0 < self.raw.len() as f64 && self.filled < FRAME {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = self.raw[i];
            let b = self.raw[i + 1];
            let mut sample = (a + (b - a) * frac) * gain;
            if self.settings.suppress {
                // A simple DC blocker removes rumble and breath noise from the microphone.
                let filtered = sample - self.hp_x + 0.995 * self.hp_y;
                self.hp_x = sample;
                self.hp_y = filtered;
                sample = filtered;
            }
            self.frame[self.filled] = sample;
            self.filled += 1;
            self.pos += step;
        }
        let drop = (self.pos as usize).saturating_sub(1);
        for _ in 0..drop {
            self.raw.pop_front();
        }
        self.pos -= drop as f64;
        if self.filled < FRAME {
            return;
        }

        let power: f32 = self.frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
        let mut rms = power.sqrt();
        if self.settings.auto_gain {
            let factor = (0.08 / rms.max(0.001)).clamp(0.5, 4.0);
            for s in self.frame.iter_mut() {
                *s = (*s * factor).clamp(-1.0, 1.0);
            }
            rms = (rms * factor).min(1.0);
        }
        state::MIC_LEVEL.store((rms * 12.0).clamp(0.0, 1.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
        let speaking = match self.settings.mode {
            0 => {
                if rms > self.settings.threshold {
                    self.gate = true;
                } else if rms < self.settings.threshold * 0.5 {
                    self.gate = false;
                }
                self.gate
            }
            1 => state::PTT.load(std::sync::atomic::Ordering::Relaxed),
            _ => true,
        };
        let speaking = speaking && !self.settings.muted;
        state::SELF_SPEAKING.store(speaking, std::sync::atomic::Ordering::Relaxed);

        if speaking {
            state::OUTGOING.lock().push_back(codec::encode_frame(&self.frame));
            self.seq = self.seq.wrapping_add(1);
            if self.recording_on && self.recording.len() < RATE as usize * 300 {
                self.recording.extend(self.frame.iter().map(|s| (s * 32767.0) as i16));
            }
        }
        self.filled = 0;
    }

    /// A decoded frame from a `AUDIO_OUT` packet, on the network callback's thread.
    pub fn push_network(&mut self, bytes: &[u8], own_id: u64, allow_self: bool) {
        if bytes.len() < HEADER + 1 {
            return;
        }
        let Some(seq) = codec::get_u32(bytes, 0) else { return };
        let Some(speaker) = codec::get_u64(bytes, 4) else { return };
        let Some(group) = codec::get_u64(bytes, 16) else { return };
        // Group members hear their group at full volume and also hear nearby players who are
        // not in a group, with the usual distance falloff. Other groups stay private.
        let my_group = state::MY_GROUP.load(std::sync::atomic::Ordering::Relaxed);
        if group != my_group && !(my_group != 0 && group == 0) {
            return;
        }
        let (Some(x), Some(y), Some(z)) =
            (codec::get_f32(bytes, 24), codec::get_f32(bytes, 28), codec::get_f32(bytes, 32))
        else {
            return;
        };
        let Some(count) = codec::get_u16(bytes, 36) else { return };
        let end = HEADER + (count as usize).min(FRAME);
        if bytes.len() < end || (speaker == own_id && !allow_self) {
            return;
        }
        let mut frame = [0.0f32; FRAME];
        codec::decode_frame(&bytes[HEADER..end], &mut frame);

        let s = self.speakers.entry(speaker).or_insert_with(|| Speaker {
            samples: VecDeque::new(),
            pos: 0.0,
            seq: seq.wrapping_sub(1),
            last: Instant::now(),
            world_pos: [x, y, z],
            group,
            volume: state::volume_of(speaker),
        });
        if seq.wrapping_sub(s.seq) > 1 << 31 || seq == s.seq {
            return;
        }
        s.seq = seq;
        s.last = Instant::now();
        s.world_pos = [x, y, z];
        s.group = group;
        s.volume = state::volume_of(speaker);
        s.samples.extend(frame);
        while s.samples.len() > RATE as usize {
            s.samples.pop_front();
        }
        if self.recording_on && self.recording.len() < RATE as usize * 300 {
            self.recording.extend(frame.iter().map(|v| (v * 32767.0) as i16));
        }
        state::mark_speaking(speaker);
    }

    /// Fill cpal's buffer with every audible speaker, with distance falloff and stereo panning.
    fn render(&mut self, out: &mut [f32], channels: usize) {
        out.fill(0.0);
        if channels == 0 {
            return;
        }
        let step = RATE as f64 / self.out_rate as f64;
        let listener = *state::LISTENER.lock();
        let yaw = f32::from_bits(state::YAW.load(std::sync::atomic::Ordering::Relaxed));
        let range = self.settings.range.max(1.0);
        let master = self.settings.volume * 0.7;

        let stale: Vec<u64> = self
            .speakers
            .iter()
            .filter(|(_, s)| s.last.elapsed() > Duration::from_secs(2))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.speakers.remove(&id);
        }

        for s in self.speakers.values_mut() {
            let dx = s.world_pos[0] - listener[0];
            let dy = s.world_pos[1] - listener[1];
            let dz = s.world_pos[2] - listener[2];
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();
            let atten = if s.group != 0 || self.settings.audio3d == 1 {
                1.0
            } else {
                let effective = if self.settings.audio3d == 2 { range * 0.5 } else { range };
                let t = (1.0 - dist / effective).clamp(0.0, 1.0);
                t * t
            };
            if atten <= 0.0 || s.samples.len() < 640 {
                continue;
            }
            let pan = if dist > 0.001 {
                ((dx * yaw.cos() + dz * yaw.sin()) / dist).clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let left_gain = (1.0 - pan).max(0.0);
            let right_gain = (1.0 + pan).max(0.0);
            let gain = atten * s.volume * master;

            for frame in out.chunks_mut(channels) {
                let mut value = 0.0;
                if s.pos + 1.0 < s.samples.len() as f64 {
                    let i = s.pos as usize;
                    let frac = (s.pos - i as f64) as f32;
                    let a = s.samples[i];
                    let b = s.samples[i + 1];
                    value = a + (b - a) * frac;
                    s.pos += step;
                    while s.pos >= 1.0 {
                        s.samples.pop_front();
                        s.pos -= 1.0;
                    }
                }
                let value = value * gain;
                if channels == 1 {
                    frame[0] += value * (left_gain + right_gain) * 0.5;
                } else {
                    frame[0] += value * left_gain;
                    frame[1] += value * right_gain;
                    for ch in frame.iter_mut().skip(2) {
                        *ch += value * 0.5;
                    }
                }
            }
        }

        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }

    /// Called when recording is turned off: writes `run/simple_voice_chat/recording-*.wav`.
    pub fn take_recording(&mut self, id: u64) {
        if self.recording.is_empty() {
            return;
        }
        let Some(dir) = state::svc_dir() else {
            warn!("simple_voice_chat: no Downloads folder to save the recording in");
            return;
        };
        let path = format!("{dir}/recording-{id}-{}.wav", now_millis());
        match codec::write_wav(&path, &self.recording, RATE) {
            Ok(()) => info!("simple_voice_chat: recording saved to {path}"),
            Err(e) => warn!("simple_voice_chat: could not save recording: {e}"),
        }
        self.recording.clear();
    }
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}
