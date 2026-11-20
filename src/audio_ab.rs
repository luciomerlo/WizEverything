//! A/B playback for "which copy sounds better".
//!
//! Both files are decoded to the same sample rate and played from the *same*
//! frame position, so they stay in lockstep. Only one is mixed to the output at
//! a time and the app can alternate automatically every N ms (or on demand).
//! That is the classic quick-switch comparison used in audio mastering.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const MAX_SECS: f32 = 120.0;

struct Shared {
    a: Arc<Vec<f32>>,
    b: Arc<Vec<f32>>,
    frames: usize,
    rate: u32,
    pos: AtomicUsize,
    playing: AtomicBool,
    /// 0 = A, 1 = B (as displayed).
    which: AtomicU8,
    auto: AtomicBool,
    interval_ms: AtomicU32,
    since: AtomicUsize,
    swapped: AtomicBool,
}

pub struct AbPlayer {
    pub rate: u32,
    pub frames: usize,
    pub a_name: String,
    pub b_name: String,
    shared: Arc<Shared>,
    _stream: cpal::Stream,
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("?")
        .to_string()
}

/// Decode up to `MAX_SECS` of native interleaved samples.
fn decode(path: &Path) -> Result<(Vec<f32>, u32, usize), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.ok_or("no sample rate")?;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2).max(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let max = (rate as f64 * MAX_SECS as f64) as usize;
    let mut out: Vec<f32> = Vec::with_capacity(1 << 18);
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        };
        let spec = *decoded.spec();
        if sbuf.is_none() {
            sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        }
        let buf = match sbuf.as_mut() {
            Some(b) => b,
            None => break,
        };
        buf.copy_interleaved_ref(decoded);
        out.extend_from_slice(buf.samples());
        if out.len() / channels >= max {
            break;
        }
    }
    if out.is_empty() {
        return Err("no samples".into());
    }
    Ok((out, rate, channels))
}

/// Linear resample to stereo at `dst` rate. Good enough for A/B listening.
fn to_stereo(samples: &[f32], ch: usize, src: u32, dst: u32) -> Vec<f32> {
    let ch = ch.max(1);
    let frames = samples.len() / ch;
    if frames == 0 {
        return Vec::new();
    }
    let out_frames = if src == dst {
        frames
    } else {
        ((frames as f64 * dst as f64 / src as f64).round() as usize).max(1)
    };
    let mut out = Vec::with_capacity(out_frames * 2);
    let sample = |frame: usize, c: usize| samples[frame * ch + c.min(ch - 1)];
    for i in 0..out_frames {
        let t = i as f64 * src as f64 / dst as f64;
        let i0 = (t.floor() as usize).min(frames - 1);
        let i1 = (i0 + 1).min(frames - 1);
        let frac = (t - i0 as f64) as f32;
        let l = sample(i0, 0) * (1.0 - frac) + sample(i1, 0) * frac;
        let r = if ch > 1 {
            sample(i0, 1) * (1.0 - frac) + sample(i1, 1) * frac
        } else {
            l
        };
        out.push(l);
        out.push(r);
    }
    out
}

fn fill<T: cpal::SizedSample + cpal::FromSample<f32>>(data: &mut [T], channels: usize, sh: &Shared) {
    if !sh.playing.load(Ordering::Relaxed) {
        for s in data.iter_mut() {
            *s = T::from_sample(0.0f32);
        }
        return;
    }
    let mut pos = sh.pos.load(Ordering::Relaxed);
    let mut which = sh.which.load(Ordering::Relaxed);
    let auto = sh.auto.load(Ordering::Relaxed);
    let swapped = sh.swapped.load(Ordering::Relaxed);
    let interval = (sh.interval_ms.load(Ordering::Relaxed) as usize * sh.rate as usize) / 1000;
    let mut since = sh.since.load(Ordering::Relaxed);
    let (first, second) = if swapped {
        (&sh.b, &sh.a)
    } else {
        (&sh.a, &sh.b)
    };
    let frames = sh.frames;
    for frame in data.chunks_mut(channels) {
        if pos >= frames {
            sh.playing.store(false, Ordering::Relaxed);
            for s in frame.iter_mut() {
                *s = T::from_sample(0.0f32);
            }
            continue;
        }
        let buf = if which == 0 { first } else { second };
        let l = buf[pos * 2];
        let r = buf[pos * 2 + 1];
        for (c, s) in frame.iter_mut().enumerate() {
            *s = T::from_sample(if c == 0 {
                l
            } else if c == 1 {
                r
            } else if c % 2 == 0 {
                l
            } else {
                r
            });
        }
        pos += 1;
        since += 1;
        if auto && interval > 0 && since >= interval {
            which = 1 - which;
            since = 0;
        }
    }
    sh.pos.store(pos, Ordering::Relaxed);
    sh.which.store(which, Ordering::Relaxed);
    sh.since.store(since, Ordering::Relaxed);
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, sh: Arc<Shared>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    device.build_output_stream(
        config,
        move |data: &mut [T], _| fill::<T>(data, channels, &sh),
        |e| eprintln!("audio output: {e}"),
        None,
    )
}

impl AbPlayer {
    pub fn open(a: &Path, b: &Path) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no hay dispositivo de salida de audio")?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("configuración de audio: {e}"))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        let rate = config.sample_rate.0;
        let a_name = name_of(a);
        let b_name = name_of(b);
        let (sa, ra, ca) = decode(a)?;
        let (sb, rb, cb) = decode(b)?;
        let a = Arc::new(to_stereo(&sa, ca, ra, rate));
        let b = Arc::new(to_stereo(&sb, cb, rb, rate));
        let frames = (a.len().min(b.len())) / 2;
        if frames == 0 {
            return Err("los archivos no tienen muestras comparables".into());
        }
        let shared = Arc::new(Shared {
            a,
            b,
            frames,
            rate,
            pos: AtomicUsize::new(0),
            playing: AtomicBool::new(false),
            which: AtomicU8::new(0),
            auto: AtomicBool::new(true),
            interval_ms: AtomicU32::new(1500),
            since: AtomicUsize::new(0),
            swapped: AtomicBool::new(false),
        });
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, shared.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, shared.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, shared.clone()),
            other => return Err(format!("formato de salida no soportado: {other:?}")),
        }
        .map_err(|e| format!("no se pudo abrir la salida: {e}"))?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            rate,
            frames,
            a_name,
            b_name,
            shared,
            _stream: stream,
        })
    }

    pub fn duration(&self) -> f64 {
        self.frames as f64 / self.rate as f64
    }
    pub fn pos_secs(&self) -> f64 {
        self.shared.pos.load(Ordering::Relaxed) as f64 / self.rate as f64
    }
    pub fn seek_secs(&self, s: f64) {
        let f = (s.max(0.0) * self.rate as f64) as usize;
        self.shared.pos.store(f.min(self.frames), Ordering::Relaxed);
    }
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }
    pub fn toggle_play(&self) {
        let playing = self.shared.playing.load(Ordering::Relaxed);
        if !playing && self.shared.pos.load(Ordering::Relaxed) >= self.frames {
            self.shared.pos.store(0, Ordering::Relaxed);
        }
        self.shared.playing.store(!playing, Ordering::Relaxed);
    }
    pub fn side(&self) -> u8 {
        self.shared.which.load(Ordering::Relaxed)
    }
    pub fn set_side(&self, side: u8) {
        self.shared.which.store(side.min(1), Ordering::Relaxed);
        self.shared.since.store(0, Ordering::Relaxed);
    }
    pub fn auto(&self) -> bool {
        self.shared.auto.load(Ordering::Relaxed)
    }
    pub fn set_auto(&self, on: bool) {
        self.shared.auto.store(on, Ordering::Relaxed);
        self.shared.since.store(0, Ordering::Relaxed);
    }
    pub fn interval_ms(&self) -> u32 {
        self.shared.interval_ms.load(Ordering::Relaxed)
    }
    pub fn set_interval_ms(&self, ms: u32) {
        self.shared.interval_ms.store(ms.clamp(200, 10_000), Ordering::Relaxed);
        self.shared.since.store(0, Ordering::Relaxed);
    }
    pub fn swapped(&self) -> bool {
        self.shared.swapped.load(Ordering::Relaxed)
    }
    pub fn set_swapped(&self, on: bool) {
        self.shared.swapped.store(on, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_stereo_and_rate() {
        // 1 kHz mono at 8 kHz -> 44.1 kHz stereo
        let src: Vec<f32> = (0..8000).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = to_stereo(&src, 1, 8000, 44100);
        assert_eq!(out.len() % 2, 0);
        assert!((out.len() / 2).abs_diff(44100) <= 1);
        assert!((out[0] - out[1]).abs() < 1e-6, "mono duplicated to L/R");
    }

    fn write_silence_wav(path: &Path, rate: u32, secs: u32) {
        let data = rate * secs * 2;
        let mut v = Vec::with_capacity(44 + data as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        v.resize(44 + data as usize, 0);
        std::fs::write(path, &v).unwrap();
    }

    /// Opt-in: needs a real output device. Plays silence and checks the clock moves.
    #[test]
    fn plays_and_advances_with_a_real_device() {
        if std::env::var_os("WIZE_AUDIO_TEST").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("wize-ab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        write_silence_wav(&a, 44100, 2);
        write_silence_wav(&b, 44100, 2);
        let p = AbPlayer::open(&a, &b).expect("open device");
        p.set_auto(true);
        p.set_interval_ms(200);
        p.toggle_play();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(p.pos_secs() > 0.05, "clock did not advance: {}", p.pos_secs());
        p.toggle_play();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resample_same_rate_keeps_frames() {
        let src = vec![0.1f32; 200];
        let out = to_stereo(&src, 2, 44100, 44100);
        assert_eq!(out.len(), 200);
    }
}
