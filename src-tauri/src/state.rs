use crate::config::Config;
use crate::sound::{
    instrument_index, note_for, note_voice, profile_name, slot_for, timbre_for, Bank, Timbre,
    NOTES, ROWS,
};
use crossbeam_queue::ArrayQueue;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub use crate::sound::profile_index;

pub const MAX_VOICES: usize = 24;

/// A keystroke to be rendered by the audio thread.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub down: bool,
    /// Which take to play: a row for a switch set, a note for an instrument.
    pub slot: u8,
    pub gain: f32,
    /// How this strike rings this particular switch, see `sound::timbre_for`.
    pub voice: Timbre,
    /// Microseconds since `Shared::epoch`, stamped by the hook thread.
    pub t_us: u64,
}

pub struct Shared {
    pub epoch: Instant,
    pub queue: ArrayQueue<Event>,
    pub enabled: AtomicBool,
    pub up_sound: AtomicBool,
    /// f32 bits
    pub volume: AtomicU32,
    pub profile: AtomicUsize,
    pub keys: AtomicU64,
    pub played: AtomicU64,
    pub latency_us: AtomicU64,
    /// Frames handed to one audio callback, i.e. the real output buffer size.
    pub cb_frames: AtomicU32,
    pub sample_rate: AtomicU32,
    pub last_vk: AtomicU32,
    /// The strikes the window has not been told about yet, oldest first. The
    /// board draws a key going down a moment after it really went down, so a
    /// fast run still shows every key that was touched.
    pub recent: ArrayQueue<u32>,
    /// Whether exclusive mode was asked for; the audio thread acts on it.
    pub want_exclusive: AtomicBool,
    /// Whether the stream actually runs exclusive (it falls back to shared).
    pub mode_exclusive: AtomicBool,
    /// Set by the UI to make the audio thread rebuild the stream.
    pub reopen: AtomicBool,
    /// The voices the mixer reads. Replaced whole off the real time thread and
    /// swapped in by the mixer between packets, so a new selection never stops
    /// the stream.
    bank: Mutex<Arc<Bank>>,
    /// Bumped whenever `bank` is replaced. The audio thread watches this one
    /// atomic instead of taking the lock on every packet.
    pub bank_gen: AtomicU64,
    /// What the bank on file was built from, to tell a rebuild from a no-op.
    bank_sr: AtomicU32,
    bank_profile: AtomicUsize,
    /// Held by whoever is building, so picking two instruments in quick
    /// succession cannot have the older build publish last.
    bank_build: Mutex<()>,
    /// When a keystroke last arrived, which is how the exclusive mode monitor
    /// tells typing from an empty desk.
    pub last_key_us: AtomicU64,
    /// Set while another application needs the shared mixer: the engine drops
    /// out of exclusive mode and stays out until it clears.
    pub hold_shared: AtomicBool,
    /// Whether the monitor is allowed to hand the endpoint over by itself.
    pub auto_release: AtomicBool,
    /// When each virtual key went down, 0 while it is up. A key that is still
    /// stamped down when its next press arrives can only be an auto repeat,
    /// and the stamp doubles as how long the press lasted.
    held: [AtomicU64; 256],
    /// Strikes per key, so no two presses of one switch sound the same.
    strikes: [AtomicU32; 256],
    /// When the last event arrived, which is how typing speed is measured.
    last_event: AtomicU64,
}

impl Shared {
    pub fn new(cfg: &Config) -> Arc<Self> {
        Arc::new(Self {
            epoch: Instant::now(),
            queue: ArrayQueue::new(512),
            enabled: AtomicBool::new(cfg.enabled),
            up_sound: AtomicBool::new(cfg.up_sound),
            volume: AtomicU32::new(cfg.volume.clamp(0.0, 1.0).to_bits()),
            profile: AtomicUsize::new(profile_index(&cfg.profile)),
            keys: AtomicU64::new(0),
            played: AtomicU64::new(0),
            latency_us: AtomicU64::new(0),
            cb_frames: AtomicU32::new(0),
            sample_rate: AtomicU32::new(0),
            last_vk: AtomicU32::new(0),
            recent: ArrayQueue::new(64),
            want_exclusive: AtomicBool::new(cfg.exclusive),
            mode_exclusive: AtomicBool::new(false),
            reopen: AtomicBool::new(false),
            bank: Mutex::new(Arc::new(Bank::empty())),
            bank_gen: AtomicU64::new(0),
            bank_sr: AtomicU32::new(0),
            bank_profile: AtomicUsize::new(usize::MAX),
            bank_build: Mutex::new(()),
            last_key_us: AtomicU64::new(0),
            hold_shared: AtomicBool::new(false),
            auto_release: AtomicBool::new(cfg.auto_release),
            held: std::array::from_fn(|_| AtomicU64::new(0)),
            strikes: std::array::from_fn(|_| AtomicU32::new(0)),
            last_event: AtomicU64::new(0),
        })
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Whether the selected voice is one of the rendered instruments rather than
    /// a recorded switch set. Instruments route by note, switches by row.
    pub fn is_instrument(&self) -> bool {
        instrument_index(self.profile.load(Ordering::Relaxed)).is_some()
    }

    /// The bank the mixer should read right now.
    pub fn bank_snapshot(&self) -> Arc<Bank> {
        self.bank.lock().unwrap().clone()
    }

    /// Builds the bank for the current device rate and selection, unless the
    /// one on file already matches. Safe to call from any thread but the audio
    /// one: decoding and resampling a dozen sets is not real time work.
    pub fn ensure_bank(&self) {
        let sr = self.sample_rate.load(Ordering::Relaxed);
        let profile = self.profile.load(Ordering::Relaxed);
        if sr != 0
            && self.bank_sr.load(Ordering::Relaxed) == sr
            && self.bank_profile.load(Ordering::Relaxed) == profile
        {
            return;
        }
        if sr == 0 {
            // No stream yet: the engine builds the bank as soon as it opens one.
            return;
        }
        // Held for the whole build so the waiter re-reads the selection after
        // the older build has published, and the newest one wins.
        let _serial = self.bank_build.lock().unwrap();
        let sr = self.sample_rate.load(Ordering::Relaxed);
        let profile = self.profile.load(Ordering::Relaxed);
        if self.bank_sr.load(Ordering::Relaxed) == sr
            && self.bank_profile.load(Ordering::Relaxed) == profile
        {
            return;
        }
        let bank = Arc::new(crate::sound::build_bank(sr, profile));
        *self.bank.lock().unwrap() = bank;
        self.bank_sr.store(sr, Ordering::Relaxed);
        self.bank_profile.store(profile, Ordering::Relaxed);
        self.bank_gen.fetch_add(1, Ordering::Release);
    }

    /// Asks for a rebuild on a worker thread, so the UI command returns at once.
    pub fn rebuild_bank_soon(self: &Arc<Self>) {
        let shared = self.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("mechkeys-bank".into())
            .spawn(move || shared.ensure_bank())
        {
            eprintln!("MechKeys: bank rebuild failed ({e})");
        }
    }

    /// Called from the low-level keyboard hook thread. Must stay allocation free.
    pub fn on_key(&self, vk: u32, scan: u32, down: bool) {
        let key = (vk & 0xFF) as usize;
        let t = self.epoch.elapsed().as_micros() as u64;
        self.last_key_us.store(t, Ordering::Relaxed);
        // The stamp is forced away from zero so that an unpressed key reads 0.
        // A held key keeps sending down events at the OS repeat rate, and each
        // one would start another take that outlives the gap between repeats,
        // turning a held switch into a continuous tone. Tracking runs while
        // muted too, so a mute mid press cannot leave a stale stamp behind.
        let was = self.held[key].swap(if down { t | 1 } else { 0 }, Ordering::Relaxed);
        if (down && was != 0) || !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if !down && !self.up_sound.load(Ordering::Relaxed) {
            return;
        }

        if down {
            self.keys.fetch_add(1, Ordering::Relaxed);
            self.last_vk.store(vk, Ordering::Relaxed);
            // A window that is hidden, minimized or simply slow must not be able
            // to stop the hook: when the ring is full the oldest strike is the
            // one that goes.
            if self.recent.push(vk).is_err() {
                self.recent.pop();
                let _ = self.recent.push(vk);
            }
        }

        // How hard the board is being driven: a run of keys lands with more
        // weight behind the cap than a careful hunt, and a key set down after
        // a long hold is released far more gently than one snapped off.
        let since = t.saturating_sub(self.last_event.swap(t, Ordering::Relaxed));
        let urgency = 1.0 - (since as f32 / 210_000.0).min(1.0);
        let started = if was == 0 { t } else { was & !1 };
        let held_for = (t - started).min(1_500_000) as f32 / 1_500_000.0;

        let press = self.strikes[key].load(Ordering::Relaxed);
        // A switch plays the recording of the row it sits in; an instrument
        // plays the note its column carries. Everything else, including how
        // hard the key landed, is shared by the two.
        let (slot, voice) =
            if instrument_index(self.profile.load(Ordering::Relaxed)).is_some() {
                (note_for(vk, scan), note_voice(vk, scan, press, down))
            } else {
                (slot_for(vk, scan), timbre_for(vk, scan, press, down))
            };
        if down {
            self.strikes[key].fetch_add(1, Ordering::Relaxed);
        }
        let base = if down {
            0.88 + 0.24 * urgency
        } else {
            (0.54 + 0.16 * urgency) * (1.0 - held_for * 0.3)
        };

        let _ = self.queue.push(Event {
            down,
            slot,
            gain: base * voice.level,
            voice,
            t_us: t,
        });
    }

    /// Preview sound triggered from the UI (works even while muted).
    pub fn push_test(&self) {
        let t = self.epoch.elapsed().as_micros() as u64;
        let n = self.keys.fetch_add(1, Ordering::Relaxed) as usize;
        // Cycles the row takes, so repeated previews walk through the board the
        // same way typing does, with the same per switch voice a real key of
        // that row would get.
        let scan = 0x1E + (n % ROWS) as u32 * 4;
        let vk = 0x1F + scan;
        let press = (n / ROWS) as u32;
        // An instrument preview runs up its own scale instead, so one click per
        // note is enough to hear the whole voice.
        let (slot, voice) = if self.is_instrument() {
            (
                (n % NOTES) as u8,
                note_voice(vk, scan, press, true),
            )
        } else {
            ((n % ROWS) as u8, timbre_for(vk, scan, press, true))
        };
        let _ = self.queue.push(Event {
            down: true,
            slot,
            gain: voice.level,
            voice,
            t_us: t,
        });
    }

    /// The preview of one particular key, so a click on the board answers with
    /// that cap's own voice rather than wherever the cycling preview has got
    /// to. It walks the strike counter too, so tapping one cap twice does not
    /// play the same recording twice.
    pub fn push_key(&self, vk: u32, scan: u32) {
        let t = self.epoch.elapsed().as_micros() as u64;
        let key = (vk & 0xFF) as usize;
        let press = self.strikes[key].load(Ordering::Relaxed);
        self.strikes[key].fetch_add(1, Ordering::Relaxed);
        let (slot, voice) = if self.is_instrument() {
            (note_for(vk, scan), note_voice(vk, scan, press, true))
        } else {
            (slot_for(vk, scan), timbre_for(vk, scan, press, true))
        };
        let _ = self.queue.push(Event {
            down: true,
            slot,
            gain: voice.level,
            voice,
            t_us: t,
        });
    }

    /// The strikes nobody has been shown yet, oldest first. Draining is the
    /// whole point: the board is a few frames behind reality, and it should
    /// walk through them rather than only light up the last key.
    pub fn take_recent(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.recent.len());
        while let Some(vk) = self.recent.pop() {
            out.push(vk);
        }
        out
    }

    pub fn stats(&self) -> StatsDto {
        StatsDto {
            keys: self.keys.load(Ordering::Relaxed),
            played: self.played.load(Ordering::Relaxed),
            latency_us: self.latency_us.load(Ordering::Relaxed),
            last_key: key_name(self.last_vk.load(Ordering::Relaxed)),
            enabled: self.enabled.load(Ordering::Relaxed),
            volume: self.volume(),
            profile: profile_name(self.profile.load(Ordering::Relaxed)).to_string(),
            up_sound: self.up_sound.load(Ordering::Relaxed),
            exclusive: self.mode_exclusive.load(Ordering::Relaxed),
            holding: self.hold_shared.load(Ordering::Relaxed),
            cb_frames: self.cb_frames.load(Ordering::Relaxed),
            sample_rate: self.sample_rate.load(Ordering::Relaxed),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatsDto {
    pub keys: u64,
    pub played: u64,
    pub latency_us: u64,
    pub last_key: String,
    pub enabled: bool,
    pub volume: f32,
    pub profile: String,
    pub up_sound: bool,
    pub exclusive: bool,
    /// Exclusive mode is on but stepped aside for another application.
    pub holding: bool,
    /// Frames in one output packet, i.e. how much audio is always in flight.
    pub cb_frames: u32,
    pub sample_rate: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UiState {
    pub enabled: bool,
    pub volume: f32,
    pub profile: String,
    pub up_sound: bool,
    pub exclusive: bool,
    pub auto_release: bool,
    pub check_updates: bool,
    pub autostart: bool,
    pub portable: bool,
    pub lang: String,
    pub errors: Vec<UiError>,
    pub version: String,
}

/// One thing that failed at startup, split into which part gave out and what
/// the system said about it. The sentence around it belongs to the interface,
/// which is the only place that knows what language it is in.
#[derive(Serialize, Clone)]
pub struct UiError {
    pub kind: String,
    pub message: String,
}

pub fn key_name(vk: u32) -> String {
    let s = match vk {
        0x00 => return String::new(),
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x10 | 0xA0 | 0xA1 => "Shift",
        0x11 | 0xA2 | 0xA3 => "Ctrl",
        0x12 | 0xA4 | 0xA5 => "Alt",
        0x13 => "Pause",
        0x14 => "CapsLock",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "←",
        0x26 => "↑",
        0x27 => "→",
        0x28 => "↓",
        0x2C => "PrtSc",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5B | 0x5C => "Win",
        0x5D => "Menu",
        0x90 => "NumLock",
        0x91 => "ScrollLock",
        0xBA => "؛",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        _ => "",
    };
    if !s.is_empty() {
        return s.to_string();
    }
    if (0x30..=0x39).contains(&vk) {
        return ((b'0' + (vk - 0x30) as u8) as char).to_string();
    }
    if (0x41..=0x5A).contains(&vk) {
        return ((b'A' + (vk - 0x41) as u8) as char).to_string();
    }
    if (0x60..=0x69).contains(&vk) {
        return format!("Num{}", vk - 0x60);
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x6F);
    }
    format!("0x{vk:02X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_lookup_roundtrips() {
        for (i, p) in crate::sound::PROFILES.iter().enumerate() {
            assert_eq!(profile_index(p), i);
        }
        assert_eq!(profile_index("nonsense"), 0);
    }

    #[test]
    fn key_names_are_sane() {
        assert_eq!(key_name(0x41), "A");
        assert_eq!(key_name(0x31), "1");
        assert_eq!(key_name(0x20), "Space");
        assert_eq!(key_name(0x7A), "F11");
        assert_eq!(key_name(0), "");
    }

    #[test]
    fn queue_accepts_events_when_enabled() {
        let cfg = Config::default();
        let shared = Shared::new(&cfg);
        shared.on_key(0x41, 30, true);
        shared.on_key(0x41, 30, false);
        assert_eq!(shared.queue.len(), 2);
        assert_eq!(shared.keys.load(Ordering::Relaxed), 1);
        let e = shared.queue.pop().unwrap();
        assert!(e.down);
    }

    /// The board is drawn from what the window is told, so the ring has to hand
    /// a run over in the order it happened, once, and then be empty.
    #[test]
    fn recent_keys_drain_in_the_order_they_went_down() {
        let shared = Shared::new(&Config::default());
        for vk in [0x41, 0x42, 0x43] {
            shared.on_key(vk, 30, true);
            shared.on_key(vk, 30, false);
        }
        assert_eq!(shared.take_recent(), vec![0x41, 0x42, 0x43]);
        assert!(shared.take_recent().is_empty(), "a drain is a drain");
    }

    #[test]
    fn a_window_that_never_asks_cannot_stall_the_hook() {
        let shared = Shared::new(&Config::default());
        for i in 0..200u32 {
            let vk = 0x41 + (i % 40);
            shared.on_key(vk, 30, true);
            shared.on_key(vk, 30, false);
        }
        let seen = shared.take_recent();
        // The ring is bounded, and what it keeps is the end of the run.
        assert_eq!(seen.len(), shared.recent.capacity());
        assert_eq!(seen.last(), Some(&shared.last_vk.load(Ordering::Relaxed)));
        assert_eq!(shared.keys.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn held_key_does_not_retrigger() {
        let cfg = Config::default();
        let shared = Shared::new(&cfg);
        shared.on_key(0x41, 30, true);
        assert_eq!(shared.queue.len(), 1, "first press should sound");
        for _ in 0..30 {
            shared.on_key(0x41, 30, true);
        }
        assert_eq!(shared.queue.len(), 1, "auto repeats must stay silent");
        assert_eq!(shared.keys.load(Ordering::Relaxed), 1, "repeats are not key presses");

        shared.on_key(0x41, 30, false);
        assert_eq!(shared.queue.len(), 2, "release should sound");
        shared.on_key(0x41, 30, true);
        assert_eq!(shared.queue.len(), 3, "a fresh press after release sounds again");
    }

    #[test]
    fn a_key_held_while_muted_sounds_on_its_next_press() {
        let cfg = Config::default();
        let shared = Shared::new(&cfg);
        shared.enabled.store(false, Ordering::Relaxed);
        shared.on_key(0x41, 30, true);
        shared.on_key(0x41, 30, false);
        assert_eq!(shared.queue.len(), 0);
        shared.enabled.store(true, Ordering::Relaxed);
        shared.on_key(0x41, 30, true);
        assert_eq!(shared.queue.len(), 1, "muted press left a stale held bit");
    }

    #[test]
    fn an_instrument_routes_by_note_and_a_switch_by_row() {
        let shared = Shared::new(&Config::default());
        shared.on_key(0x1E, 0x1E, true);
        let e = shared.queue.pop().unwrap();
        assert_eq!(e.slot, slot_for(0x1E, 0x1E));
        // Let go, or the next press of the same key is an auto repeat and stays
        // silent whatever the selection is.
        shared.on_key(0x1E, 0x1E, false);
        assert!(!shared.queue.pop().unwrap().down);

        let first_instrument = crate::sound::SWITCHES.len();
        shared.profile.store(first_instrument, Ordering::Relaxed);
        shared.on_key(0x1E, 0x1E, true);
        let e = shared.queue.pop().unwrap();
        assert_eq!(e.slot, note_for(0x1E, 0x1E));
        // The plate ring and the cap rattle are keyboard acoustics; a note has
        // neither, or a piano would come out buzzing.
        assert_eq!(e.voice.body, 0.0);
        assert_eq!(e.voice.twang, 0.0);
        assert!(e.voice.decay_s > 5.0, "the take owns the tail of a note");
    }

    #[test]
    fn the_bank_is_only_rebuilt_when_the_selection_moves() {
        let shared = Shared::new(&Config::default());
        // No device rate yet, so there is nothing to build for.
        shared.ensure_bank();
        assert!(shared.bank_snapshot().profiles[0].down.is_empty());

        shared.sample_rate.store(48_000, Ordering::Relaxed);
        shared.ensure_bank();
        let built = shared.bank_gen.load(Ordering::Relaxed);
        assert!(!shared.bank_snapshot().profiles[0].down.is_empty());
        shared.ensure_bank();
        assert_eq!(
            shared.bank_gen.load(Ordering::Relaxed),
            built,
            "a no-op request rebuilt the whole bank"
        );

        shared.profile.store(crate::sound::SWITCHES.len(), Ordering::Relaxed);
        shared.ensure_bank();
        assert!(shared.bank_gen.load(Ordering::Relaxed) > built);
        // Only the picked instrument is filled in.
        let bank = shared.bank_snapshot();
        assert!(!bank.profiles[crate::sound::SWITCHES.len()].down.is_empty());
        assert!(bank.profiles[crate::sound::SWITCHES.len() + 1].down.is_empty());
    }
}
