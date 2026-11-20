//! Acoustic profile of an audio file: decodes a bounded window and estimates the
//! noise floor, SNR, digital silence, high-frequency content and spectral
//! stability. That is what lets the app tell a denoised copy from the original,
//! and an aggressive denoise (hiss gone but dull or "musical noise") from a good
//! one. Purely local: no network, no uploads.

use std::path::Path;
use std::sync::Arc;

use rustfft::Fft;
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

/// Fast-first: 20 s is enough for a stable noise floor and keeps the scan quick.
const MAX_SECS: f32 = 20.0;
const FRAME: usize = 1024;

#[derive(Clone, Debug)]
pub struct Profile {
    pub seconds: f32,
    pub channels: usize,
    /// Signal-to-noise ratio estimated from loud vs quiet frames (dB).
    pub snr_db: f32,
    /// 10th percentile of frame level = background noise (dBFS).
    pub noise_floor_db: f32,
    /// 95th percentile of frame level = programme level (dBFS).
    pub signal_db: f32,
    /// Fraction of frames near the noise floor (silence/gaps).
    pub silence_ratio: f32,
    /// Fraction of samples that are exactly zero: a denoiser's digital gate.
    pub digital_silence: f32,
    /// High-band energy in quiet frames (hiss residue).
    pub hf_quiet_db: f32,
    /// High-band energy in loud frames (dullness / over-filtering check).
    pub hf_loud_db: f32,
    /// Spectral flatness in quiet frames (1 = noise-like, 0 = tonal).
    pub flatness_quiet: f32,
    /// Frame-to-frame spectral change in quiet frames (pumping/musical noise).
    pub flux_quiet: f32,
    /// Peak minus RMS: dynamics. A crushed master sounds worse.
    pub crest_db: f32,
    /// Fraction of samples at/over full scale.
    pub clipped: f32,
    pub frames: usize,
}

impl Profile {
    /// Heuristic: the file looks like it went through a denoiser/gate.
    pub fn denoised_hint(&self) -> bool {
        self.digital_silence > 0.01 || (self.silence_ratio > 0.25 && self.snr_db > 28.0)
    }

    /// Only meaningful against another copy: same recording, but this one
    /// fluctuates far more in its quiet passages (musical noise/pumping).
    pub fn artifacts_vs(&self, other: &Profile) -> bool {
        self.silence_ratio > 0.05
            && self.flux_quiet > 0.6
            && self.flux_quiet > other.flux_quiet * 1.6
    }

    /// This copy is cleaner but lost a lot of top end vs the other one.
    pub fn dull_vs(&self, other: &Profile) -> bool {
        self.snr_db > other.snr_db + 3.0 && self.hf_loud_db < other.hf_loud_db - 8.0
    }

    /// Relative quality used only between copies of the *same* recording.
    pub fn quality(&self) -> f32 {
        let snr = ((self.snr_db - 8.0) / 32.0).clamp(0.0, 1.0);
        let mut q = snr * 40.0;
        q -= (self.flux_quiet * 20.0).clamp(0.0, 10.0);
        q += (((self.hf_loud_db + 55.0) / 40.0).clamp(0.0, 1.0)) * 12.0;
        q += ((self.crest_db - 6.0) / 14.0).clamp(0.0, 1.0) * 8.0;
        q -= self.clipped * 200.0;
        q
    }

    /// How much of the file was actually measured.
    pub fn confidence(&self) -> &'static str {
        if self.seconds >= 12.0 {
            "alta"
        } else if self.seconds >= 6.0 {
            "media"
        } else {
            "baja"
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "fondo {:.0} dBFS · SNR {:.0} dB · silencio {:.0}% · agudos {:.0} dB",
            self.noise_floor_db,
            self.snr_db,
            self.silence_ratio * 100.0,
            self.hf_loud_db
        )
    }
}

/// Fast similarity between two profiles (0–100), used instead of expensive
/// sample-by-sample comparison: how alike they *sound*, not just metadata.
pub fn similarity_pct(a: &Profile, b: &Profile) -> f64 {
    let d = (a.snr_db - b.snr_db).abs() * 1.2
        + (a.noise_floor_db - b.noise_floor_db).abs() * 0.8
        + (a.hf_loud_db - b.hf_loud_db).abs() * 0.4
        + (a.crest_db - b.crest_db).abs() * 0.4;
    (100.0 - d as f64).clamp(0.0, 100.0)
}

/// Compare two copies and return which one sounds better plus why.
pub fn compare(
    a: &Profile,
    b: &Profile,
    a_name: &str,
    b_name: &str,
) -> (std::cmp::Ordering, Vec<String>) {
    let mut notes = Vec::new();
    let floor_delta = b.noise_floor_db - a.noise_floor_db;
    if floor_delta.abs() >= 3.0 {
        let (quieter, louder) = if floor_delta > 0.0 { (a_name, b_name) } else { (b_name, a_name) };
        notes.push(format!(
            "{quieter} tiene {:.0} dB menos de ruido de fondo que {louder} (SNR {:.0} vs {:.0} dB).",
            floor_delta.abs(),
            a.snr_db.max(b.snr_db),
            a.snr_db.min(b.snr_db)
        ));
    }
    let lvl = a.signal_db - b.signal_db;
    if lvl.abs() >= 3.0 {
        let (lower, higher) = if lvl < 0.0 { (a_name, b_name) } else { (b_name, a_name) };
        notes.push(format!(
            "{lower} está {:.0} dB más bajo que {higher}; igualá el volumen antes de comparar a oído.",
            lvl.abs()
        ));
    }
    let hf = a.hf_loud_db - b.hf_loud_db;
    if hf.abs() >= 4.0 {
        let brighter = if hf > 0.0 { a_name } else { b_name };
        notes.push(format!(
            "{brighter} conserva {:.0} dB más de agudos (aire); el otro puede estar apagado.",
            hf.abs()
        ));
    }
    if a.artifacts_vs(b) {
        notes.push(format!(
            "{a_name} fluctúa mucho más en los silencios: posible ruido musical/pumping del denoise."
        ));
    }
    if b.artifacts_vs(a) {
        notes.push(format!(
            "{b_name} fluctúa mucho más en los silencios: posible ruido musical/pumping del denoise."
        ));
    }
    if a.dull_vs(b) {
        notes.push(format!(
            "{a_name} es más limpio pero perdió {:.0} dB de agudos: puede sonar apagado.",
            b.hf_loud_db - a.hf_loud_db
        ));
    }
    if b.dull_vs(a) {
        notes.push(format!(
            "{b_name} es más limpio pero perdió {:.0} dB de agudos: puede sonar apagado.",
            a.hf_loud_db - b.hf_loud_db
        ));
    }
    if a.clipped > 0.001 {
        notes.push(format!("{a_name} tiene clipping (saturación)."));
    }
    if b.clipped > 0.001 {
        notes.push(format!("{b_name} tiene clipping (saturación)."));
    }
    (a.quality().total_cmp(&b.quality()), notes)
}

/// Decode a window and build the profile. Returns `None` for formats the
/// decoder does not support (WMA/APE/WV) or files that cannot be read.
pub fn analyze(path: &Path) -> Option<Profile> {
    analyze_secs(path, MAX_SECS)
}

/// Deep mode can afford a longer window; fast mode uses `MAX_SECS`.
pub fn analyze_secs(path: &Path, secs: f32) -> Option<Profile> {
    let (samples, rate, channels) = decode(path, secs).ok()?;
    profile_from(&samples, rate, channels)
}

fn decode(path: &Path, max_secs: f32) -> Result<(Vec<f32>, u32, usize), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
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
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("sin pista de audio")?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.ok_or("sin sample rate")?;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(1).max(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;

    let max_frames = (rate as f64 * max_secs.clamp(2.0, 300.0) as f64) as usize;
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
            // A corrupt packet should not kill the profile.
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
        if out.len() / channels >= max_frames {
            break;
        }
    }
    if out.is_empty() {
        return Err("sin muestras".into());
    }
    Ok((out, rate, channels))
}

/// Pure DSP so it can be tested with synthetic signals.
pub fn profile_from(samples: &[f32], rate: u32, channels: usize) -> Option<Profile> {
    let channels = channels.max(1);
    let frames_total = samples.len() / channels;
    if frames_total < FRAME * 2 || rate == 0 {
        return None;
    }
    // Downmix to mono: noise floor is about the recording, not the channel layout.
    let mono: Vec<f32> = (0..frames_total)
        .map(|i| {
            let base = i * channels;
            (0..channels).map(|c| samples[base + c]).sum::<f32>() / channels as f32
        })
        .collect();

    let peak = mono.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    let clipped = mono.iter().filter(|x| x.abs() >= 0.999).count() as f32 / mono.len() as f32;
    let digital = mono.iter().filter(|x| x.abs() < 1e-7).count() as f32 / mono.len() as f32;
    let rms_all = (mono.iter().map(|x| x * x).sum::<f32>() / mono.len() as f32).sqrt();

    let mut planner = FftPlanner::<f32>::new();
    let fft: Arc<dyn Fft<f32>> = planner.plan_fft_forward(FRAME);
    let hann: Vec<f32> = (0..FRAME)
        .map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (FRAME - 1) as f32).cos())
        .collect();
    let nyquist = rate as f32 / 2.0;
    let hf_lo = ((6000.0 / nyquist) * (FRAME / 2) as f32).round().clamp(2.0, (FRAME / 2 - 1) as f32) as usize;

    struct Frame {
        rms_db: f32,
        /// Broadband noise estimate from the median spectral bin (robust to tones).
        noise_db: f32,
        hf_power: f32,
        flatness: f32,
        flux: f32,
    }
    let mut frames: Vec<Frame> = Vec::new();
    let mut prev: Vec<f32> = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); FRAME];
    for chunk in mono.chunks_exact(FRAME) {
        let rms = (chunk.iter().map(|x| x * x).sum::<f32>() / FRAME as f32).sqrt();
        let rms_db = 20.0 * (rms + 1e-9).log10();
        for (b, (s, w)) in buf.iter_mut().zip(chunk.iter().zip(&hann)) {
            *b = Complex32::new(s * w, 0.0);
        }
        fft.process(&mut buf);
        let half = FRAME / 2;
        let mut mags = Vec::with_capacity(half);
        for m in buf.iter().take(half) {
            // Normalise so a full-scale sine has a peak near 1.
            mags.push((m.norm() / half as f32) * 2.0);
        }
        let hf_power = mags[hf_lo..]
            .iter()
            .map(|m| m * m)
            .sum::<f32>()
            .max(1e-12);
        // Noise floor: noise spreads over every bin, a tone does not. The median
        // bin power (corrected for the exponential distribution and the Hann
        // window's power loss) estimates the broadband floor even without silence.
        let mut powers: Vec<f32> = mags.iter().skip(1).map(|m| m * m).collect();
        let nbins = powers.len() as f32;
        powers.sort_by(|a, b| a.total_cmp(b));
        let med = powers.get(powers.len() / 2).copied().unwrap_or(0.0);
        // Parseval: sigma^2 = sum_half(mag^2)/3 for this normalisation, and the
        // median of an exponential bin distribution is mean*ln(2).
        let noise_power = (med / std::f32::consts::LN_2) * nbins / 3.0;
        let noise_db = 20.0 * (noise_power.sqrt() + 1e-9).log10();
        let (mut log_sum, mut lin_sum) = (0.0f32, 0.0f32);
        for m in mags.iter().skip(1) {
            let p = m * m + 1e-20;
            log_sum += p.ln();
            lin_sum += p;
        }
        let n = (half - 1) as f32;
        let flatness = if lin_sum > 0.0 {
            ((log_sum / n).exp() / (lin_sum / n)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let flux = if prev.len() == mags.len() {
            let num: f32 = mags.iter().zip(&prev).map(|(a, b)| (a - b).abs()).sum();
            let den: f32 = mags.iter().sum::<f32>().max(1e-9);
            (num / den).min(10.0)
        } else {
            0.0
        };
        prev = mags;
        frames.push(Frame { rms_db, noise_db, hf_power, flatness, flux });
    }
    if frames.len() < 2 {
        return None;
    }
    let levels: Vec<f32> = frames.iter().map(|f| f.rms_db).collect();
    let floors: Vec<f32> = frames.iter().map(|f| f.noise_db).collect();
    let noise_floor_db = pct(&floors, 0.50).clamp(-120.0, 0.0);
    let signal_db = pct(&levels, 0.95).clamp(-120.0, 0.0);
    let threshold = noise_floor_db + 6.0;
    let silence_ratio = frames.iter().filter(|f| f.rms_db <= threshold).count() as f32 / frames.len() as f32;

    // Quiet frames above digital silence: residue lives here.
    let quiet: Vec<&Frame> = frames
        .iter()
        .filter(|f| f.rms_db <= threshold && f.rms_db > -110.0)
        .collect();
    let loud: Vec<&Frame> = frames.iter().filter(|f| f.rms_db > threshold).collect();
    let all: Vec<&Frame> = frames.iter().collect();
    let use_q: &[&Frame] = if quiet.is_empty() { &all } else { &quiet };
    let mean = |v: &[&Frame], f: fn(&Frame) -> f32| -> f32 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().map(|x| f(x)).sum::<f32>() / v.len() as f32
        }
    };
    let hf_quiet_db = 10.0 * mean(use_q, |f| f.hf_power).max(1e-12).log10();
    let hf_loud_db = if loud.is_empty() {
        hf_quiet_db
    } else {
        10.0 * mean(&loud, |f| f.hf_power).max(1e-12).log10()
    };
    let crest_db = 20.0 * (peak + 1e-9).log10() - 20.0 * (rms_all + 1e-9).log10();

    Some(Profile {
        seconds: frames_total as f32 / rate as f32,
        channels,
        snr_db: (signal_db - noise_floor_db).clamp(0.0, 80.0),
        noise_floor_db,
        signal_db,
        silence_ratio,
        digital_silence: digital,
        hf_quiet_db,
        hf_loud_db,
        flatness_quiet: mean(use_q, |f| f.flatness),
        flux_quiet: mean(use_q, |f| f.flux),
        crest_db: crest_db.clamp(0.0, 40.0),
        clipped,
        frames: frames.len(),
    })
}

fn pct(v: &[f32], q: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let i = ((s.len() - 1) as f32 * q).round() as usize;
    s[i.min(s.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    fn tone_noise(secs: f32, noise: f32) -> Vec<f32> {
        let rate = 44100.0;
        let n = (rate * secs) as usize;
        let mut seed = 7u64;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate;
                (0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() + noise * lcg(&mut seed))
                    .clamp(-1.0, 1.0)
            })
            .collect()
    }

    #[test]
    fn noisy_track_has_measurable_snr() {
        let s = tone_noise(6.0, 0.02);
        let p = profile_from(&s, 44100, 1).expect("profile");
        assert!(p.snr_db > 15.0, "snr={}", p.snr_db);
        assert!(p.noise_floor_db < -30.0, "floor={}", p.noise_floor_db);
        assert_eq!(p.channels, 1);
    }

    #[test]
    fn quieter_copy_wins() {
        let noisy = profile_from(&tone_noise(6.0, 0.05), 44100, 1).unwrap();
        let clean = profile_from(&tone_noise(6.0, 0.003), 44100, 1).unwrap();
        assert!(clean.quality() > noisy.quality());
        assert_eq!(compare(&clean, &noisy, "clean", "noisy").0, std::cmp::Ordering::Greater);
    }

    #[test]
    fn digital_silence_is_detected() {
        let rate = 44100;
        let mut s = vec![0.0f32; rate as usize];
        s.extend(tone_noise(4.0, 0.01));
        let p = profile_from(&s, rate, 1).unwrap();
        assert!(p.digital_silence > 0.15, "digital={}", p.digital_silence);
        assert!(p.denoised_hint());
    }

    #[test]
    fn similarity_is_high_for_equal_copies_and_drops_with_noise() {
        let a = profile_from(&tone_noise(5.0, 0.02), 44100, 1).unwrap();
        let b = profile_from(&tone_noise(5.0, 0.02), 44100, 1).unwrap();
        let c = profile_from(&tone_noise(5.0, 0.08), 44100, 1).unwrap();
        assert!(similarity_pct(&a, &b) > 95.0);
        assert!(similarity_pct(&a, &c) < similarity_pct(&a, &b));
        assert_eq!(a.confidence(), "baja");
        let long = profile_from(&tone_noise(12.0, 0.02), 44100, 1).unwrap();
        assert_eq!(long.confidence(), "alta");
    }

    #[test]
    fn clipping_is_detected() {
        let mut s = tone_noise(4.0, 0.0);
        for x in s.iter_mut().take(4000) {
            *x = 1.0;
        }
        let p = profile_from(&s, 44100, 1).unwrap();
        assert!(p.clipped > 0.001, "clipped={}", p.clipped);
    }

    #[test]
    fn compare_explains_the_difference() {
        let a = profile_from(&tone_noise(6.0, 0.05), 44100, 1).unwrap();
        let b = profile_from(&tone_noise(6.0, 0.004), 44100, 1).unwrap();
        let (_ord, notes) = compare(&b, &a, "denoised", "original");
        assert!(notes.iter().any(|n| n.contains("ruido de fondo")), "{notes:?}");
    }
}
