use crate::sound::Bank;
use crate::state::{Event, Shared, MAX_VOICES};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, StreamConfig};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Keeps three or four overlapping voices comfortably inside full scale.
const VOICE_SCALE: f32 = 0.30;

/// One struck switch.
///
/// The take is only half of it: it is read back at a per key pitch, ringed
/// through a per key plate resonance and damped over a per key tail, which is
/// what turns one recording of a row into a hundred separate switches.
#[derive(Clone, Copy, Default)]
struct Voice {
    active: bool,
    /// 0 = key down, 1 = key up
    kind: u8,
    slot: u8,
    /// Sub sample read position and its per second step, i.e. the pitch.
    pos: f32,
    step: f32,
    gain: f32,
    /// Running tail damping, multiplied into the gain per frame.
    env: f32,
    decay: f32,
    /// Level of the body resonance mixed in with the dry click.
    body: f32,
    /// Band pass coefficients and state for that resonance. The pass band is
    /// symmetric, so `b2` is `-b0` and needs no field of its own.
    b0: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
    /// Keycap and leaf rattle, and the sample it differentiates from.
    twang: f32,
    prev: f32,
}

/// Additive voice mixer, shared by the low latency backend and the fallback.
pub struct Mixer {
    shared: Arc<Shared>,
    bank: Arc<Bank>,
    /// The generation `bank` was taken from; a new one means a new selection.
    bank_gen: u64,
    voices: Vec<Voice>,
    master: f32,
    sample_rate: f32,
}

impl Mixer {
    pub fn new(shared: Arc<Shared>) -> Self {
        let sr = shared.sample_rate.load(Ordering::Relaxed);
        let master = shared.volume();
        Self {
            bank_gen: shared.bank_gen.load(Ordering::Relaxed),
            bank: shared.bank_snapshot(),
            shared,
            voices: vec![Voice::default(); MAX_VOICES],
            master,
            sample_rate: if sr == 0 { 48_000.0 } else { sr as f32 },
        }
    }

    /// Fills one interleaved block. Must not allocate: it runs on the audio thread.
    pub fn render<T>(&mut self, data: &mut [T], channels: usize)
    where
        T: SizedSample + FromSample<f32>,
    {
        if channels == 0 {
            return;
        }
        let target = self.shared.volume();
        let gen = self.shared.bank_gen.load(Ordering::Relaxed);
        if gen != self.bank_gen {
            // A rebuild lands here, not mid packet: the lock is only ever held
            // for the length of one pointer copy, and only once per selection.
            self.bank_gen = gen;
            self.bank = self.shared.bank_snapshot();
            for voice in self.voices.iter_mut() {
                voice.active = false;
            }
            self.sample_rate = self.shared.sample_rate.load(Ordering::Relaxed).max(1) as f32;
        }
        let profile = self.shared.profile.load(Ordering::Relaxed);
        let profile = match self.bank.profiles.get(profile) {
            Some(p) if !p.down.is_empty() => p,
            _ => &self.bank.profiles[0],
        };
        let takes = profile.down.len();
        let mut master = self.master;

        for frame in data.chunks_mut(channels) {
            // Drain every frame so a keystroke that lands mid buffer is heard now.
            while let Some(event) = self.shared.queue.pop() {
                start_voice(&mut self.voices, &event, &self.shared, self.sample_rate, takes);
            }

            let mut s = 0.0f32;
            for voice in self.voices.iter_mut() {
                if !voice.active {
                    continue;
                }
                let slot = voice.slot as usize;
                let buf = match if voice.kind == 0 {
                    profile.down.get(slot)
                } else {
                    profile.up.get(slot)
                } {
                    Some(buf) => buf,
                    None => {
                        voice.active = false;
                        continue;
                    }
                };
                let i = voice.pos as usize;
                if i >= buf.len() {
                    voice.active = false;
                    continue;
                }
                // Reading between the samples is what lets a switch sit anywhere
                // within three percent of the recording it came from.
                let next = buf[(i + 1).min(buf.len() - 1)];
                let dry = buf[i] + (next - buf[i]) * (voice.pos - i as f32);
                voice.pos += voice.step;

                // The strike rings the plate and the cavity under it, at the
                // frequency that belongs to where the key sits on the board.
                let x = dry;
                let bp = voice.b0 * (x - voice.x2) + voice.a1 * voice.y1 + voice.a2 * voice.y2;
                voice.x2 = voice.x1;
                voice.x1 = x;
                voice.y2 = voice.y1;
                voice.y1 = bp;
                // A one pole treble lift on the way out is the rattle of the cap
                // against the housing and the spring blade inside it.
                let rattle = dry - voice.prev;
                voice.prev = dry;

                voice.env *= voice.decay;
                s += (dry + bp * voice.body + rattle * voice.twang) * voice.env * voice.gain;
            }

            master += (target - master) * 0.0009;
            let out = (s * VOICE_SCALE * master).clamp(-1.0, 1.0);
            let value = T::from_sample(out);
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }

        self.master = master;
    }
}

pub fn start(shared: Arc<Shared>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("mechkeys-audio".into())
        .spawn(move || {
            // The IAudioClient3 path can reach the device minimum period, which the
            // shared mode cpal path cannot: it is always locked to the ~10 ms engine
            // period. Fall back only if that path is unavailable.
            if let Err(e) = crate::lowlat::run(shared.clone(), tx.clone()) {
                eprintln!("MechKeys: low latency audio unavailable ({e}), using fallback");
                match build_fallback(&shared) {
                    Ok(stream) => {
                        // The stream must stay alive for the whole process, so park here.
                        let _stream = stream;
                        let _ = tx.send(Ok(()));
                        loop {
                            std::thread::park();
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => Err("audio device did not start in time".into()),
    }
}

fn build_fallback(shared: &Arc<Shared>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let format = supported.sample_format();

    if !matches!(
        format,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
    ) {
        return Err(format!("unsupported sample format: {format:?}"));
    }

    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    shared.ensure_bank();
    let mut config: StreamConfig = supported.config();
    let mut last_error = String::from("no stream configuration was accepted");

    // Small buffers first (low latency), fall back to the device default.
    for size in [128u32, 256, 512, 0] {
        config.buffer_size = if size == 0 {
            BufferSize::Default
        } else {
            BufferSize::Fixed(size)
        };
        let err_fn = |e| eprintln!("MechKeys audio stream error: {e}");
        let result = match format {
            SampleFormat::F32 => device.build_output_stream(
                &config,
                make_callback::<f32>(shared.clone(), channels),
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                &config,
                make_callback::<i16>(shared.clone(), channels),
                err_fn,
                None,
            ),
            _ => device.build_output_stream(
                &config,
                make_callback::<u16>(shared.clone(), channels),
                err_fn,
                None,
            ),
        };
        match result {
            Ok(stream) => {
                stream.play().map_err(|e| e.to_string())?;
                return Ok(stream);
            }
            Err(e) => last_error = e.to_string(),
        }
    }
    Err(format!("could not open audio output: {last_error}"))
}

fn make_callback<T>(
    shared: Arc<Shared>,
    channels: usize,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static
where
    T: SizedSample + FromSample<f32>,
{
    let mut mixer = Mixer::new(shared.clone());
    move |data: &mut [T], _| {
        if channels == 0 {
            return;
        }
        shared
            .cb_frames
            .store((data.len() / channels) as u32, Ordering::Relaxed);
        mixer.render(data, channels);
    }
}

#[inline]
fn start_voice(
    voices: &mut [Voice],
    event: &Event,
    shared: &Shared,
    sample_rate: f32,
    takes: usize,
) {
    // A bank that is still empty, or one that holds fewer takes than the event
    // asks for, would index out of bounds a frame later.
    let Some(last) = takes.checked_sub(1) else { return };
    let slot = voices
        .iter()
        .position(|v| !v.active)
        .or_else(|| {
            voices
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.pos.total_cmp(&b.1.pos))
                .map(|(i, _)| i)
        });
    let Some(i) = slot else { return };

    // A constant Q band pass, with the denominators folded in so the render
    // loop only adds. Q stays low: the plate ring is a broad shoulder, not a
    // whistle, and pushing it further turns a keyboard into a flute.
    let sr = sample_rate.max(8_000.0);
    let fc = event.voice.body_hz.clamp(40.0, sr * 0.45);
    let (sin_w0, cos_w0) = (std::f32::consts::TAU * fc / sr).sin_cos();
    let alpha = sin_w0 / 2.2;
    let a0 = 1.0 + alpha;
    voices[i] = Voice {
        active: true,
        kind: if event.down { 0 } else { 1 },
        slot: event.slot.min(last as u8),
        pos: 0.0,
        step: event.voice.pitch,
        gain: event.gain,
        env: 1.0,
        decay: (-1.0 / (sr * event.voice.decay_s)).exp(),
        body: event.voice.body,
        b0: alpha / a0,
        // Recurrence form: y += a1*y1 + a2*y2, hence the sign flip here.
        a1: 2.0 * cos_w0 / a0,
        a2: (alpha - 1.0) / a0,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
        twang: event.voice.twang,
        prev: 0.0,
    };

    shared.played.fetch_add(1, Ordering::Relaxed);
    let now = shared.epoch.elapsed().as_micros() as u64;
    shared
        .latency_us
        .store(now.saturating_sub(event.t_us), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const SR: u32 = 48_000;

    fn rig() -> (Arc<Shared>, Mixer) {
        let shared = Shared::new(&Config::default());
        shared.sample_rate.store(SR, Ordering::Relaxed);
        // Full master so the levels the assertions look at are the voices' own.
        shared.volume.store(1.0f32.to_bits(), Ordering::Relaxed);
        shared.ensure_bank();
        let mixer = Mixer::new(shared.clone());
        (shared, mixer)
    }

    /// Renders one press of one key and returns the mono signal.
    fn strike(shared: &Arc<Shared>, mixer: &mut Mixer, vk: u32, scan: u32) -> Vec<f32> {
        shared.on_key(vk, scan, true);
        let mut buf = vec![0.0f32; (SR as usize / 5) * 2];
        mixer.render(&mut buf[..], 2);
        // Put the key back down again and let the release play out into a
        // scratch buffer, so the next strike starts from a quiet board and is
        // never mistaken for an auto repeat.
        shared.on_key(vk, scan, false);
        let mut scratch = vec![0.0f32; (SR as usize / 2) * 2];
        mixer.render(&mut scratch[..], 2);
        buf.chunks(2).map(|f| f[0]).collect()
    }

    /// Mean absolute difference relative to the louder of two takes.
    fn divergence(a: &[f32], b: &[f32]) -> f32 {
        let sum: f32 = a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum();
        let energy: f32 = a.iter().chain(b.iter()).map(|x| x.abs()).sum();
        2.0 * sum / energy.max(1e-6)
    }

    /// Share of the energy above roughly a kilohertz: the difference between a
    /// bright key and a hollow one.
    fn brightness(x: &[f32]) -> f32 {
        let mut low = 0.0f32;
        let mut high = 0.0f32;
        let mut total = 0.0f32;
        for &s in x {
            // One pole low pass; whatever it lets through is the top band.
            low += (s - low) * 0.1;
            let h = s - low;
            high += h * h;
            total += s * s;
        }
        (high / total.max(1e-9)).sqrt()
    }

    #[test]
    fn voices_are_clean_and_decay() {
        let (shared, mut mixer) = rig();
        let x = strike(&shared, &mut mixer, 0x41, 0x1E);
        let peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(x.iter().all(|s| s.is_finite()), "non finite output");
        assert!(peak > 0.15 && peak <= 1.0, "peak {peak} is off scale");
        // Nothing may be left ringing a quarter of a second later.
        let tail = x[x.len() - 200..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(tail < peak * 0.02, "tail {tail} vs peak {peak}");
    }

    #[test]
    fn two_presses_of_one_key_never_replay_the_same_take() {
        let (shared, mut mixer) = rig();
        let a = strike(&shared, &mut mixer, 0x41, 0x1E);
        let b = strike(&shared, &mut mixer, 0x41, 0x1E);
        let d = divergence(&a, &b);
        assert!(d > 0.05, "repeated press is a byte for byte echo: {d:.4}");
    }

    #[test]
    fn keys_in_one_row_sound_like_different_switches() {
        // A and S share a row take and a recording; only the switch identity
        // can tell them apart.
        let (shared, mut mixer) = rig();
        let a = strike(&shared, &mut mixer, 0x41, 0x1E);
        let s = strike(&shared, &mut mixer, 0x53, 0x1F);
        let d = divergence(&a, &s);
        assert!(d > 0.05, "row mates are indistinguishable: {d:.4}");
    }

    #[test]
    fn centre_of_the_board_rings_lower_than_the_edge() {
        let (shared, mut mixer) = rig();
        // G is the middle of the home row, the plate's loosest span; the
        // adjacent shift and the row keys bolted near the case are stiffer.
        let mut centre = Vec::new();
        let mut edge = Vec::new();
        for scan in [0x22u32, 0x23, 0x24] {
            centre.push(brightness(&strike(&shared, &mut mixer, 0x100 + scan, scan)));
        }
        for scan in [0x1Eu32, 0x28, 0x2C] {
            edge.push(brightness(&strike(&shared, &mut mixer, 0x100 + scan, scan)));
        }
        let mean = |v: &Vec<f32>| v.iter().sum::<f32>() / v.len() as f32;
        assert!(
            mean(&centre) < mean(&edge),
            "centre {:.4} should be duller than edge {:.4}",
            mean(&centre),
            mean(&edge)
        );
    }

    /// Switching to an instrument has to reach the mixer through the live
    /// stream, not by reopening it: the swap lands between two packets.
    #[test]
    fn a_new_voice_bank_reaches_the_mixer_mid_stream() {
        let (shared, mut mixer) = rig();
        let second = || vec![0.0f32; (SR as usize) * 2];

        shared.on_key(0x41, 0x1E, true);
        shared.on_key(0x41, 0x1E, false);
        let mut switch = second();
        mixer.render(&mut switch[..], 2);
        let tail = &switch[switch.len() - 200..];
        assert!(
            tail.iter().all(|s| s.abs() < 1e-3),
            "a switch should be silent after a second"
        );

        shared.profile.store(crate::sound::SWITCHES.len(), Ordering::Relaxed);
        shared.ensure_bank();
        shared.on_key(0x42, 0x1F, true);
        shared.on_key(0x42, 0x1F, false);
        let mut note = second();
        mixer.render(&mut note[..], 2);
        assert!(note.iter().all(|s| s.is_finite()));
        let tail = &note[note.len() - 200..];
        assert!(
            tail.iter().fold(0.0f32, |m, s| m.max(s.abs())) > 1e-3,
            "the instrument bank never reached the mixer"
        );
    }

    #[test]
    fn fast_typing_hits_harder_than_slow_typing() {
        let (shared, mut mixer) = rig();
        // First strike after a gap, then an immediate second one.
        std::thread::sleep(Duration::from_millis(400));
        let slow = strike(&shared, &mut mixer, 0x41, 0x1E);
        let fast = strike(&shared, &mut mixer, 0x41, 0x1E);
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        assert!(
            rms(&fast) > rms(&slow),
            "urgent keystrokes must not come out softer: {} vs {}",
            rms(&fast),
            rms(&slow)
        );
    }
}
