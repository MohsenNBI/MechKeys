//! The keycap bank: real switch recordings, converted to 48 kHz mono 16 bit WAV
//! by `tools/build-sounds.mjs` and compiled into the binary.
//!
//! Each set ships eight press takes (five row takes plus space, enter and
//! backspace) and four release takes. The four missing row releases are derived
//! at load time by resampling one take, which costs nothing at build size and
//! keeps fast typing from sounding like a single repeated sample.
//!
//! A take is a recording of a row, not of a key, so `timbre_for` supplies what
//! the recording cannot: the reason two neighbouring switches on a real board
//! never sound alike, and why no two presses of the same switch ever did.

/// Rate the assets are stored at, see `tools/build-sounds.mjs`.
const EMBED_RATE: u32 = 48_000;

/// Row takes reused for every ordinary key.
pub const ROWS: usize = 5;
/// Space, enter, backspace take the slots after the rows.
pub const SPACE: u8 = ROWS as u8;
pub const ENTER: u8 = ROWS as u8 + 1;
pub const BACKSPACE: u8 = ROWS as u8 + 2;
/// Press and release takes are indexed identically.
pub const SLOTS: usize = ROWS + 3;

pub const SWITCHES: [&str; 12] = [
    "mx-blue",
    "mx-brown",
    "mx-black",
    "holy-panda",
    "topre",
    "cream",
    "alpaca",
    "ink-black",
    "ink-red",
    "box-navy",
    "alps-blue",
    "model-m",
];

/// The custom tile's voices. These are not recorded: they are rendered from
/// scratch by `render_instrument`, in the order the picker lists them.
pub const INSTRUMENTS: [&str; 5] = ["piano", "epiano", "marimba", "bell", "guitar"];

/// Everything the picker can select: the recorded switch sets, then the
/// rendered instruments. The bank is indexed by the same order.
pub const PROFILES: [&str; SWITCHES.len() + INSTRUMENTS.len()] = {
    let mut all = [""; SWITCHES.len() + INSTRUMENTS.len()];
    let mut i = 0;
    while i < SWITCHES.len() {
        all[i] = SWITCHES[i];
        i += 1;
    }
    let mut j = 0;
    while j < INSTRUMENTS.len() {
        all[SWITCHES.len() + j] = INSTRUMENTS[j];
        j += 1;
    }
    all
};

/// Which instrument a selected profile is, or `None` for a recorded switch set.
pub fn instrument_index(profile: usize) -> Option<usize> {
    if profile < SWITCHES.len() {
        None
    } else {
        Some(profile - SWITCHES.len())
    }
}

/// Pitch and level offsets used to grow the release rows out of one take.
const RELEASE_DETUNE: [(f32, f32); ROWS] = [
    (1.0, 1.0),
    (1.019, 0.95),
    (0.982, 1.04),
    (1.031, 0.92),
    (0.973, 1.01),
];

/// The E0 prefix flag of a scan code, set by the hook. It tells the navigation
/// cluster and the arrows apart from the keypad that shares their codes.
pub const EXT: u32 = 0x100;

/// Which take a key plays. A key sounds like the row it physically sits in,
/// exactly like the recordings: R0 is the function row, R1 the number row, R2
/// the QWERTY row, R3 the home row and R4 the bottom row plus modifiers. This
/// is what makes typing feel like one board instead of random takes.
pub fn slot_for(vk: u32, scan: u32) -> u8 {
    match vk {
        0x20 => return SPACE,
        0x0D => return ENTER,
        0x08 => return BACKSPACE,
        // Pause is sent as an E1 sequence, so its scan code is unreliable.
        0x13 => return 0,
        _ => {}
    }
    if scan & EXT != 0 {
        // E0 prefixed: the edit cluster between the keypad and the letters, the
        // arrows below it, and the right side modifiers.
        return match scan & !EXT {
            0x1C => ENTER,            // keypad enter
            0x37 => 0,                // print screen
            0x47 | 0x49 | 0x52 => 1,  // home, page up, insert
            0x4F | 0x51 | 0x53 => 2,  // end, page down, delete
            _ => 4,                    // arrows, right ctrl and alt, menu
        };
    }
    match scan {
        0x01 => 0,          // esc
        0x02..=0x0D => 1,   // ` 1 .. 0 - = on the number row
        0x0F..=0x1B => 2,   // tab and Q .. ]
        0x1E..=0x28 => 3,   // A .. '
        0x29 => 1,           // grave, next to the number row
        0x2B => 2,           // backslash, next to ]
        0x37 => 1,           // keypad multiply, top keypad row
        0x3B..=0x44 => 0,   // F1 .. F10
        0x45 => 1,           // num lock, top keypad row
        0x46 => 0,           // scroll lock
        0x47..=0x4A => 1,   // keypad 7 8 9 and minus
        0x4B..=0x4E => 2,   // keypad 4 5 6 and plus
        0x4F..=0x51 => 3,   // keypad 1 2 3
        0x52 | 0x53 => 4,   // keypad 0 and dot
        0x57 | 0x58 => 0,   // F11, F12
        _ => 4,              // shifts, ctrl, alt, win, and anything unknown
    }
}

pub fn profile_index(name: &str) -> usize {
    PROFILES
        .iter()
        .position(|p| p.eq_ignore_ascii_case(name))
        .unwrap_or(0)
}

pub fn profile_name(index: usize) -> &'static str {
    PROFILES[index.min(PROFILES.len() - 1)]
}

/// How one strike rings the board: the acoustic fingerprint of a single
/// switch under a single keycap.
///
/// A real keyboard is a hundred small mechanical differences, not one sample
/// replayed. Housing tolerances put every switch a few percent off pitch, the
/// plate and the air behind it ring at a frequency that depends on where the
/// key sits, and the lubrication and keycap weight decide how long the strike
/// takes to die away. All five are modelled here from the scan code, so a key
/// keeps its own voice for the life of the board while no two presses come
/// out identical.
#[derive(Clone, Copy, Debug)]
pub struct Timbre {
    /// Take playback rate: above one is shorter and higher pitched.
    pub pitch: f32,
    /// Centre of the resonance this strike rings, in Hz.
    pub body_hz: f32,
    /// How much of that resonance comes through, 0..1.
    pub body: f32,
    /// High keyed rattle of the keycap and the metal leaf, 0..1.
    pub twang: f32,
    /// Seconds for the strike to die away.
    pub decay_s: f32,
    /// How hard this particular strike landed, as an amplitude factor.
    pub level: f32,
}

/// Avalanche mix, so a stable per key fingerprint falls out of the scan code
/// without a lookup table.
#[inline]
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 29;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 32)
}

/// A 0..1 value carved out of a hash.
#[inline]
fn unit(x: u64) -> f32 {
    ((x >> 40) as f32) / ((1u64 << 24) as f32)
}

/// How near the middle of the plate a key sits: 1 at the centre, 0 at the
/// edges, 0.5 for anything outside the alpha block.
///
/// Centre keys have the most unsupported plate and the largest cavity behind
/// them, which is exactly why they sound hollow and lower while the keys
/// bolted to the stiff rim of the case sound tight and bright.
fn plate_position(scan: u32) -> f32 {
    let s = scan & !EXT;
    let (first, width) = match s {
        0x02..=0x0D => (0x02u32, 12),  // number row
        0x10..=0x1B => (0x10, 12),     // QWERTY row
        0x1E..=0x28 => (0x1E, 11),     // home row
        0x2C..=0x33 => (0x2C, 8),      // bottom row
        _ => return 0.5,
    };
    let column = (s - first) as f32;
    let middle = (width - 1) as f32 / 2.0;
    1.0 - (column - middle).abs() / (width as f32 / 2.0)
}

/// The voice of one press of one key.
///
/// `press` counts how often this key has been struck, which is what makes the
/// same switch answer twice with two different sounds, the way a real one does
/// because no two finger strikes are ever the same.
pub fn timbre_for(vk: u32, scan: u32, press: u32, down: bool) -> Timbre {
    let key = (vk as u64) << 9 | (scan & 0x1FF) as u64;
    // Salting the identity by stroke keeps the release off the exact pitch of
    // the press: the up strike hits the housing from the other side.
    let stroke = if down { 0u64 } else { 0x9E37_79B9_7F4A_7C15 };
    let id = mix(key.wrapping_mul(0xff51_afd7_ed55_8ccd) ^ stroke);
    let hit = mix(key.wrapping_mul(0xd6e8_fe_b8_6659_fd93) ^ press as u64 ^ stroke);

    let centre = plate_position(scan);
    // Plus minus three percent across the board: an eighth tone at the click
    // centre, far enough apart to hear as a different key, too close to sound
    // like a different switch. One fifth of that moves press to press.
    let pitch = 0.970 + unit(id >> 3) * 0.060 + (unit(hit) - 0.5) * 0.012;
    // The plate ring drops and deepens towards the middle of the board.
    let body_hz = 250.0 + (1.0 - centre) * 270.0 + (unit(id >> 9) - 0.5) * 70.0;
    let body = 0.16 + centre * 0.24 + (unit(hit >> 6) - 0.5) * 0.05;
    // Rattle is the mirror image: the stiff rim has more high end to spare.
    let twang = 0.05 + (1.0 - centre) * 0.15 + (unit(id >> 15) - 0.5) * 0.07;
    // Lube and housing fit decide the tail; a release is damped by the keycap
    // falling back onto its own stroke.
    // Lube, housing fit and keycap weight decide how long a strike rings. The
    // floor is deliberate: damping that bites into the first milliseconds
    // softens the attack itself, and the attack is what a switch click is.
    let decay_s = 0.048 + unit(id >> 21) * 0.070 + (unit(hit >> 12) - 0.5) * 0.014;
    let decay_s = if down { decay_s } else { decay_s * 0.8 };

    Timbre {
        pitch,
        body_hz: body_hz.max(60.0),
        body: body.clamp(0.04, 0.46),
        twang: twang.clamp(0.0, 0.26),
        decay_s: decay_s.clamp(0.03, 0.14),
        // No two finger strikes carry the same weight into the housing.
        level: 0.93 + unit(hit >> 18) * 0.14,
    }
}

pub struct BankProfile {
    /// Indexed by `slot_for`: five row takes, then space, enter, backspace.
    pub down: Vec<Vec<f32>>,
    pub up: Vec<Vec<f32>>,
}

pub struct Bank {
    pub profiles: Vec<BankProfile>,
}

/// Builds every set at the device rate. Runs once per sound selection, off the
/// audio thread: the recorded sets are always in there, but only the one
/// instrument the picker is on, because a dozen piano tails are megabytes.
pub fn build_bank(sample_rate: u32, selected: usize) -> Bank {
    let step = EMBED_RATE as f32 / sample_rate.max(1) as f32;
    let mut profiles = Vec::with_capacity(PROFILES.len());
    for takes in TAKES.iter() {
        let mut down = Vec::with_capacity(SLOTS);
        for i in 0..SLOTS {
            down.push(resample(&decode_wav(takes[i]), step));
        }

        let mut up = Vec::with_capacity(SLOTS);
        let release = decode_wav(takes[UP_GENERIC]);
        for (pitch, gain) in RELEASE_DETUNE {
            let mut take = resample(&release, step * pitch);
            for s in take.iter_mut() {
                *s *= gain;
            }
            up.push(take);
        }
        for i in UP_SPACE..UP_SPACE + 3 {
            up.push(resample(&decode_wav(takes[i]), step));
        }

        profiles.push(BankProfile { down, up });
    }

    for (k, _) in INSTRUMENTS.iter().enumerate() {
        let profile = if SWITCHES.len() + k == selected {
            render_instrument(k, sample_rate)
        } else {
            // Nothing to read until it is picked: the mixer goes quiet for a
            // profile with no takes rather than playing the wrong one.
            BankProfile {
                down: Vec::new(),
                up: Vec::new(),
            }
        };
        profiles.push(profile);
    }
    Bank { profiles }
}

/// A bank with nothing in it, for the moment before the first one is built.
impl Bank {
    pub fn empty() -> Bank {
        Bank {
            profiles: (0..PROFILES.len())
                .map(|_| BankProfile {
                    down: Vec::new(),
                    up: Vec::new(),
                })
                .collect(),
        }
    }
}

/* ============================================================
   Instrument voices — what the custom tile plays instead of a switch.
   ============================================================ */

/// Notes each instrument is built from: one C major scale, two octaves wide.
/// Every key on the board keeps one fixed pitch, so the same sentence typed
/// twice plays the same melody twice.
pub const NOTES: usize = 12;

/// Semitones above middle C for the twelve notes.
const SCALE: [i32; NOTES] = [0, 2, 4, 5, 7, 9, 11, 12, 14, 16, 17, 19];

fn note_hz(i: usize) -> f32 {
    261.626 * 2f32.powf(SCALE[i] as f32 / 12.0)
}

/// Which note a key plays. The column the key sits in walks up the scale and
/// each row starts one step later than the row above it, so the board reads
/// left to right the way a piano does.
pub fn note_for(vk: u32, scan: u32) -> u8 {
    if vk == 0x20 {
        // Space is the thumb, and the thumb gets the lowest note on the board.
        return 0;
    }
    let s = scan & !EXT;
    let column: usize = match s {
        0x02..=0x0D => (s - 0x02) as usize,        // number row
        0x10..=0x1B => (s - 0x10) as usize + 1,    // QWERTY row
        0x1E..=0x28 => (s - 0x1E) as usize + 2,    // home row
        0x2C..=0x33 => (s - 0x2C) as usize + 3,    // bottom row
        0x3B..=0x44 => (s - 0x3B) as usize,        // function row
        0x47..=0x49 => (s - 0x47) as usize + 8,    // keypad top row: the high end
        0x4A..=0x53 => (s - 0x4A) as usize + 4,
        _ => s as usize,
    };
    (column % NOTES) as u8
}

/// The voice of one note. None of the switch modelling belongs here: the take
/// already carries its own body and its own tail, so the ring filter and the
/// cap rattle are switched off and all that is left is how hard the key landed
/// and how far off the written pitch this particular key sits.
pub fn note_voice(vk: u32, scan: u32, press: u32, down: bool) -> Timbre {
    let key = ((vk as u64) << 9) | (scan & 0x1FF) as u64;
    let id = mix(key ^ if down { 0 } else { 0x9E37_79B9_7F4A_7C15 });
    let hit = mix(key.wrapping_mul(0xd6e8_fe_b8_6659_fd93) ^ press as u64);
    Timbre {
        // A typing piano is never quite in tune with itself, and that is part
        // of the charm: a key a few cents off still sits inside the chord.
        pitch: 1.0 + (unit(id) - 0.5) * 0.006,
        body_hz: 1_000.0,
        body: 0.0,
        twang: 0.0,
        // Long enough to be transparent; the take decides how long a note rings.
        decay_s: 20.0,
        level: if down {
            0.70 + unit(hit >> 12) * 0.32
        } else {
            0.34
        },
    }
}

/// Renders one instrument: twelve notes and a key off for each of them.
fn render_instrument(kind: usize, sample_rate: u32) -> BankProfile {
    let sr = sample_rate.max(8_000) as f32;
    let release = damper(sr);
    let mut down = Vec::with_capacity(NOTES);
    let mut up = Vec::with_capacity(NOTES);
    for i in 0..NOTES {
        let f = note_hz(i);
        let mut take = match kind {
            0 => piano(f, sr),
            1 => electric_piano(f, sr),
            2 => marimba(f, sr),
            3 => bell(f, sr),
            _ => guitar(f, sr),
        };
        shape(&mut take, sr);
        down.push(take);
        up.push(release.clone());
    }
    BankProfile { down, up }
}

/// A deterministic noise source, so a build of the bank twice gives the same
/// instrument back.
#[inline]
fn noise(i: u64) -> f32 {
    unit(mix(i.wrapping_mul(0x2545_F491_4F6C_DD1D))) * 2.0 - 1.0
}

/// Fades both ends and brings the take up to a level the mixer can share with
/// three or four other voices.
fn shape(take: &mut [f32], sr: f32) {
    let edge_in = ((0.003 * sr) as usize).max(2);
    let edge_out = ((0.02 * sr) as usize).max(2);
    let n = take.len();
    // The fades come first, so the level being normalised is the one that is
    // actually heard: a pluck whose loudest moment is its first sample would
    // otherwise come out a third under everything else.
    for (i, s) in take.iter_mut().enumerate() {
        if i < edge_in {
            *s *= i as f32 / edge_in as f32;
        }
        if i + edge_out > n {
            *s *= (n - i) as f32 / edge_out as f32;
        }
    }
    let peak = take.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-6);
    let gain = 0.62 / peak;
    for s in take.iter_mut() {
        *s *= gain;
    }
}

/// The felt hammer leaving the string: a few milliseconds of broadband knock.
fn knock(out: &mut [f32], sr: f32, dur: f32, level: f32) {
    let n = ((dur * sr) as usize).min(out.len());
    let mut lp = 0.0f32;
    for i in 0..n {
        let w = noise(i as u64 + 7) * (1.0 - i as f32 / n as f32) * level;
        lp += (w - lp) * 0.6;
        out[i] += lp;
    }
}

/// Damper falling back onto a string: the release for every instrument.
fn damper(sr: f32) -> Vec<f32> {
    let n = (0.09 * sr) as usize;
    let mut out = Vec::with_capacity(n);
    let mut lp = 0.0f32;
    for i in 0..n {
        let t = i as f32 / sr;
        let w = noise(i as u64 + 91) * (-t / 0.012).exp() * 0.5;
        lp += (w - lp) * 0.22;
        out.push(lp);
    }
    shape(&mut out, sr);
    out
}

/// Hammered strings: partials that are not quite multiples of the fundamental,
/// each one dying at its own rate.
fn piano(f: f32, sr: f32) -> Vec<f32> {
    const PARTIALS: usize = 9;
    let mut out = vec![0.0f32; (1.7 * sr) as usize];
    // Stiffness pulls a real string's overtones sharp, most of all on the thick
    // low ones, and that stretch is why a piano is tuned by ear rather than by
    // formula. Without it the instrument sounds like an organ.
    let stretch = 0.0004 + 0.0022 * (261.6 / f).min(2.5);
    for n in 1..=PARTIALS {
        let nf = n as f32;
        let freq = f * nf * (1.0 + stretch * nf * nf).sqrt();
        let amp = 1.0 / nf.powf(1.25 + (f / 900.0) * 0.5);
        // The top of the string's own register dies first.
        let tau = (1.9 / (1.0 + 0.55 * nf * nf)).max(0.045) * (440.0 / f).powf(0.35);
        let phase = unit(mix(n as u64 * 0x1234_5678)) * std::f32::consts::TAU;
        let step = std::f32::consts::TAU * freq / sr;
        let fall = (-1.0 / (sr * tau)).exp();
        let mut env = 1.0f32;
        let mut acc = 0.0f32;
        for s in out.iter_mut() {
            *s += amp * (phase + acc).sin() * env;
            env *= fall;
            acc += step;
            if acc > std::f32::consts::TAU {
                acc -= std::f32::consts::TAU;
            }
        }
    }
    knock(&mut out, sr, 0.006, 0.22);
    out
}

/// One operator folded back into itself with an index that collapses in the
/// first fifth of a second: the chirp that makes an electric piano itself, plus
/// the tine ringing on top of it.
fn electric_piano(f: f32, sr: f32) -> Vec<f32> {
    let w = std::f32::consts::TAU * f / sr;
    let mut out = vec![0.0f32; (1.5 * sr) as usize];
    let (mut body, mut index, mut tine, mut hum) = (1.0f32, 3.4f32, 1.0f32, 1.0f32);
    let (d_body, d_index, d_tine, d_hum) = (
        (-1.0 / (sr * 1.25)).exp(),
        (-1.0 / (sr * 0.16)).exp(),
        (-1.0 / (sr * 0.09)).exp(),
        (-1.0 / (sr * 1.1)).exp(),
    );
    let mut acc = 0.0f32;
    for s in out.iter_mut() {
        *s = (acc + index * acc.sin()).sin() * body + (acc * 7.04).sin() * tine * 0.10
            + (acc * 2.0).sin() * hum * 0.30;
        body *= d_body;
        index *= d_index;
        tine *= d_tine;
        hum *= d_hum;
        acc += w;
        if acc > std::f32::consts::TAU {
            acc -= std::f32::consts::TAU;
        }
    }
    knock(&mut out, sr, 0.004, 0.10);
    out
}

/// A wooden bar: the fundamental carries the note, a fourth above carries the
/// knock of the mallet and is gone in sixty milliseconds.
fn marimba(f: f32, sr: f32) -> Vec<f32> {
    let w = std::f32::consts::TAU * f / sr;
    let mut out = vec![0.0f32; (1.1 * sr) as usize];
    let (mut bar, mut knock, mut third) = (1.0f32, 1.0f32, 1.0f32);
    let (d_bar, d_knock, d_third) = (
        (-1.0 / (sr * 0.42)).exp(),
        (-1.0 / (sr * 0.055)).exp(),
        (-1.0 / (sr * 0.12)).exp(),
    );
    let mut acc = 0.0f32;
    for s in out.iter_mut() {
        *s = acc.sin() * bar + (acc * 4.0).sin() * knock * 0.42 + (acc * 3.0).sin() * third * 0.10;
        bar *= d_bar;
        knock *= d_knock;
        third *= d_third;
        acc += w;
        if acc > std::f32::consts::TAU {
            acc -= std::f32::consts::TAU;
        }
    }
    out
}

/// Inharmonic on purpose: these are the partials of a cast bell, and the lowest
/// one is the hum note that gives it size. Nothing about it decays together,
/// which is what makes a bell ring rather than sound.
fn bell(f: f32, sr: f32) -> Vec<f32> {
    const RATIOS: [f32; 5] = [0.5, 1.0, 1.186, 2.021, 2.526];
    const LEVELS: [f32; 5] = [0.35, 1.0, 0.62, 0.34, 0.22];
    const SECONDS: [f32; 5] = [1.6, 1.15, 0.6, 0.32, 0.16];
    let mut out = vec![0.0f32; (2.2 * sr) as usize];
    for (k, &ratio) in RATIOS.iter().enumerate() {
        let step = std::f32::consts::TAU * f * ratio / sr;
        let fall = (-1.0 / (sr * SECONDS[k])).exp();
        let phase = unit(mix(k as u64 + 3)) * std::f32::consts::TAU;
        let mut acc = 0.0f32;
        let mut env = 1.0f32;
        for s in out.iter_mut() {
            *s += (phase + acc).sin() * env * LEVELS[k];
            env *= fall;
            acc += step;
            if acc > std::f32::consts::TAU {
                acc -= std::f32::consts::TAU;
            }
        }
    }
    knock(&mut out, sr, 0.003, 0.14);
    out
}

/// A plucked string. The Karplus-Strong loop is the famous way to one, and it is
/// not used here on purpose: its own averaging filter drags the fundamental down
/// with the brightness once a period is only sixty samples long, so the top of
/// the range dies in a fifth of the time the bottom takes. The modes are
/// therefore decayed by hand like the piano's, which keeps the pick's spectrum
/// and the string's stiffness while letting every note ring for its own length.
fn guitar(f: f32, sr: f32) -> Vec<f32> {
    let mut out = vec![0.0f32; (1.8 * sr) as usize];
    let top = ((sr * 0.4 / f).floor() as usize).clamp(4, 24);
    for k in 1..=top {
        let kf = k as f32;
        // Where the finger let go, a sixth of the way up the string: a comb with
        // a hole at every sixth harmonic, so a pluck is never a sawtooth.
        let x = std::f32::consts::PI * kf / 6.0;
        let amp = (if x < 1e-3 { 1.0 } else { x.sin() / x }) / (kf * kf);
        if amp < 1e-4 {
            continue;
        }
        // Stiffness pulls the overtones sharp, a little more each one.
        let freq = f * kf * (1.0 + 0.0006 * kf * kf).sqrt();
        // The bridge drinks the thin top partials first; the fundamental and its
        // octave are what is left of the note after half a second.
        let tau = (1.15 / (1.0 + 0.16 * kf * kf)).max(0.03) * (330.0 / f).powf(0.3).min(1.4);
        let phase = unit(mix(k as u64 * 0x9E37_79B9)) * std::f32::consts::TAU;
        let step = std::f32::consts::TAU * freq / sr;
        let fall = (-1.0 / (sr * tau)).exp();
        let mut env = 1.0f32;
        let mut acc = 0.0f32;
        for s in out.iter_mut() {
            *s += amp * (phase + acc).sin() * env;
            env *= fall;
            acc += step;
            if acc > std::f32::consts::TAU {
                acc -= std::f32::consts::TAU;
            }
        }
    }
    // The string beating against the fret for four milliseconds on the way out.
    knock(&mut out, sr, 0.004, 0.12);
    out
}

/// RIFF reader for the 16 bit mono PCM files the build step writes.
fn decode_wav(wav: &[u8]) -> Vec<f32> {
    let mut pos = 12;
    let mut data: &[u8] = &[];
    let mut channels = 0u16;
    let mut rate = 0u32;
    let mut bits = 0u16;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let end = (pos + 8 + size).min(wav.len());
        let body = &wav[pos + 8..end];
        if id == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
        } else if id == b"data" {
            data = body;
        }
        pos = end + (size & 1);
    }
    assert!(data.len() > 2, "empty wav data chunk");
    assert!(
        channels == 1 && bits == 16 && rate == EMBED_RATE,
        "unexpected wav format: {channels}ch {bits}bit {rate} Hz"
    );
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0)
        .collect()
}

/// Half width of the windowed sinc kernel, in source samples.
const TAPS: isize = 24;

/// Reads `src` at `step` source samples per output sample, so `step > 1` is both
/// shorter and higher pitched. The kernel is scaled by the same factor, which is
/// what keeps a downsample to 44.1 kHz from aliasing.
fn resample(src: &[f32], step: f32) -> Vec<f32> {
    if (step - 1.0).abs() < 1e-6 {
        return src.to_vec();
    }
    let cutoff = (1.0 / step).min(1.0);
    let half = (TAPS as f32 / cutoff).ceil() as isize;
    let out_len = (((src.len() as f32 - 1.0) / step).floor() as usize) + 1;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let pos = i as f32 * step;
        let base = pos.floor() as isize;
        let frac = pos - base as f32;
        let mut acc = 0.0f32;
        let mut weight = 0.0f32;
        for k in -half..=half {
            let index = base + k;
            if index < 0 || index as usize >= src.len() {
                continue;
            }
            let d = k as f32 - frac;
            let w = blackman(d / half as f32) * sinc(d * cutoff) * cutoff;
            acc += src[index as usize] * w;
            weight += w;
        }
        out.push(if weight.abs() > 1e-6 { acc / weight } else { 0.0 });
    }
    out
}

fn sinc(x: f32) -> f32 {
    if x.abs() < 1e-6 {
        1.0
    } else {
        let px = std::f32::consts::PI * x;
        px.sin() / px
    }
}

fn blackman(t: f32) -> f32 {
    let x = (t + 1.0) * 0.5;
    0.42 - 0.5 * (std::f32::consts::TAU * x).cos() + 0.08 * (2.0 * std::f32::consts::TAU * x).cos()
}

macro_rules! takes {
    ($dir:literal) => {
        [
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_r0.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_r1.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_r2.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_r3.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_r4.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_space.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_enter.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/down_backspace.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/up_generic.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/up_space.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/up_enter.wav")),
            include_bytes!(concat!("../assets/sounds/", $dir, "/up_backspace.wav")),
        ]
    };
}

/// `SLOTS` press takes, then the four release takes, for each recorded set.
const TAKES: [[&[u8]; SLOTS + 4]; SWITCHES.len()] = [
    takes!("mx-blue"),
    takes!("mx-brown"),
    takes!("mx-black"),
    takes!("holy-panda"),
    takes!("topre"),
    takes!("cream"),
    takes!("alpaca"),
    takes!("ink-black"),
    takes!("ink-red"),
    takes!("box-navy"),
    takes!("alps-blue"),
    takes!("model-m"),
];

/// First release take; the row releases are grown from it.
const UP_GENERIC: usize = SLOTS;

/// First dedicated release take, i.e. the space release. Enter and backspace
/// follow it, matching the press slot order.
const UP_SPACE: usize = SLOTS + 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_key_keeps_its_voice_across_presses() {
        let a = timbre_for(0x1E, 0x1E, 0, true);
        let b = timbre_for(0x1E, 0x1E, 7, true);
        // The ring of the plate under this keycap does not move between
        // presses, only the way the finger lands does.
        assert_eq!(a.body_hz, b.body_hz);
        assert!((a.body - b.body).abs() < 0.06);
        assert!((a.pitch - b.pitch).abs() < 0.02, "pitch drifted off the key");
        assert!(a.pitch != b.pitch, "two presses are identical");
        assert!(a.decay_s != b.decay_s);
    }

    #[test]
    fn the_release_is_not_the_press_played_backwards() {
        let down = timbre_for(0x1E, 0x1E, 3, true);
        let up = timbre_for(0x1E, 0x1E, 3, false);
        assert!(down.pitch != up.pitch && down.body_hz != up.body_hz);
        assert!(up.decay_s < down.decay_s, "a release rings longer than a strike");
    }

    #[test]
    fn neighbours_on_the_board_are_separate_switches() {
        let mut seen = std::collections::HashSet::new();
        for scan in 0x1E..=0x28 {
            let t = timbre_for(scan, scan, 0, true);
            // Quantised pitch plus resonance: keys must not collapse onto each
            // other or the board reads as one switch again.
            let fingerprint = (
                (t.pitch * 400.0).round() as i32,
                (t.body_hz * 0.5).round() as i32,
            );
            assert!(seen.insert(fingerprint), "key {scan:#04x} sounds like its neighbour");
        }
        assert_eq!(seen.len(), 11);
    }

    #[test]
    fn plate_position_colours_the_middle_of_the_board() {
        // G is dead centre of the home row, A is at the stiff edge.
        assert!(plate_position(0x22) > plate_position(0x1E));
        assert!(timbre_for(0x22, 0x22, 0, true).body > timbre_for(0x1E, 0x1E, 0, true).body);
        assert!(timbre_for(0x22, 0x22, 0, true).twang < timbre_for(0x1E, 0x1E, 0, true).twang);
        // Off the alpha block the model gives up gracefully rather than lying.
        assert_eq!(plate_position(0x39), 0.5);
    }

    #[test]
    fn timbres_stay_in_range_for_every_key() {
        for vk in 0..0x100u32 {
            for scan in (0..0x60u32).chain([EXT | 0x47, EXT | 0x1C]) {
                for press in [0u32, 1, 17, 4_000] {
                    for down in [true, false] {
                        let t = timbre_for(vk, scan, press, down);
                        assert!((0.955..=1.045).contains(&t.pitch), "pitch {}", t.pitch);
                        assert!((60.0..=20_000.0).contains(&t.body_hz));
                        assert!((0.04..=0.46).contains(&t.body));
                        assert!((0.0..=0.26).contains(&t.twang));
                        assert!((0.03..=0.14).contains(&t.decay_s));
                        assert!((0.9..=1.1).contains(&t.level));
                    }
                }
            }
        }
    }

    #[test]
    fn every_take_decodes() {
        for (set, takes) in TAKES.iter().enumerate() {
            for (slot, wav) in takes.iter().enumerate() {
                let samples = decode_wav(wav);
                assert!(samples.len() > 200, "{}/{slot} is too short", PROFILES[set]);
                assert!(samples.iter().any(|s| s.abs() > 0.3), "{}/{slot} is silent", PROFILES[set]);
            }
        }
    }

    #[test]
    fn bank_is_clean_on_every_sample_rate() {
        for sr in [44_100u32, 48_000, 96_000] {
            let bank = build_bank(sr, 0);
            assert_eq!(bank.profiles.len(), PROFILES.len());
            for profile in bank.profiles.iter().take(SWITCHES.len()) {
                assert_eq!(profile.down.len(), SLOTS);
                assert_eq!(profile.up.len(), SLOTS);
                for take in profile.down.iter().chain(profile.up.iter()) {
                    assert!(!take.is_empty());
                    let mut peak = 0.0f32;
                    for &s in take {
                        assert!(s.is_finite(), "non finite sample at {sr} Hz");
                        peak = peak.max(s.abs());
                    }
                    assert!(peak > 0.2 && peak <= 1.2, "peak {peak} at {sr} Hz");
                    assert!(take[0].abs() < 0.05, "no fade in");
                    assert!(take[take.len() - 1].abs() < 0.05, "no fade out");
                }
            }
        }
    }

    #[test]
    fn every_instrument_voice_is_playable() {
        for k in 0..INSTRUMENTS.len() {
            let bank = build_bank(48_000, SWITCHES.len() + k);
            let profile = &bank.profiles[SWITCHES.len() + k];
            assert_eq!(profile.down.len(), NOTES, "{}", INSTRUMENTS[k]);
            assert_eq!(profile.up.len(), NOTES, "{}", INSTRUMENTS[k]);
            for take in profile.down.iter().chain(profile.up.iter()) {
                let peak = take.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                assert!(
                    (0.4..=0.9).contains(&peak) && take.iter().all(|s| s.is_finite()),
                    "{} peak {peak}",
                    INSTRUMENTS[k]
                );
                assert!(take[0].abs() < 0.05 && take[take.len() - 1].abs() < 0.05);
            }
            // The scale has to run upwards, or the tile is a buzzer not a piano.
            let crossings = |t: &Vec<f32>| t.windows(2).filter(|w| w[0].signum() != w[1].signum()).count();
            assert!(
                crossings(&profile.down[NOTES - 1]) > crossings(&profile.down[0]),
                "{} plays its top note lower than its bottom one",
                INSTRUMENTS[k]
            );
        }
    }

    #[test]
    fn only_the_selected_instrument_is_built() {
        let bank = build_bank(48_000, SWITCHES.len() + 2);
        assert!(bank.profiles[0].down.len() == SLOTS, "the sets are always there");
        assert!(bank.profiles[SWITCHES.len()].down.is_empty());
        assert_eq!(bank.profiles[SWITCHES.len() + 2].down.len(), NOTES);
    }

    #[test]
    fn notes_walk_the_board_left_to_right() {
        assert_eq!(note_for(0x20, 0x39), 0, "space is the lowest note");
        assert!(note_for(0x51, 0x10) < note_for(0x50, 0x15), "Q is below P");
        assert!(note_for(0x31, 0x02) < note_for(0x30, 0x0D), "1 is below 0");
        // A key keeps its note for the life of the board.
        for _ in 0..4 {
            assert_eq!(note_for(0x1E, 0x1E), note_for(0x1E, 0x1E));
        }
        for vk in 0..0x100u32 {
            for scan in (0..0x60u32).chain([EXT | 0x47]) {
                assert!((note_for(vk, scan) as usize) < NOTES);
            }
        }
    }

    #[test]
    fn a_note_does_not_bring_the_switch_model_with_it() {
        let v = note_voice(0x1E, 0x1E, 2, true);
        assert_eq!(v.body, 0.0);
        assert_eq!(v.twang, 0.0);
        assert!((0.997..=1.003).contains(&v.pitch), "the take is already in tune");
        assert!(v.decay_s > 5.0, "the envelope belongs to the take");
        assert!(note_voice(0x1E, 0x1E, 2, false).level < v.level, "a key off is not a pluck");
    }

    #[test]
    fn slots_route_the_special_keys() {
        assert_eq!(slot_for(0x20, 0x39), SPACE);
        assert_eq!(slot_for(0x0D, 0x1C), ENTER);
        assert_eq!(slot_for(0x08, 0x0E), BACKSPACE);
        assert_eq!(slot_for(0x0D, EXT | 0x1C), ENTER); // keypad enter
    }

    #[test]
    fn slots_route_by_physical_row() {
        // function row
        assert_eq!(slot_for(0x70, 0x3B), 0);
        assert_eq!(slot_for(0x7B, 0x57), 0);
        assert_eq!(slot_for(0x01, 0x01), 0);
        // number row
        assert_eq!(slot_for(0x31, 0x02), 1);
        assert_eq!(slot_for(0xC0, 0x29), 1);
        // qwerty row
        assert_eq!(slot_for(0x51, 0x10), 2);
        assert_eq!(slot_for(0x0F, 0x0F), 2);
        assert_eq!(slot_for(0xDC, 0x2B), 2);
        // home row
        assert_eq!(slot_for(0x41, 0x1E), 3);
        assert_eq!(slot_for(0xDE, 0x28), 3);
        // bottom row and modifiers
        assert_eq!(slot_for(0x5A, 0x2C), 4);
        assert_eq!(slot_for(0xA0, 0x2A), 4);
        assert_eq!(slot_for(0xA2, 0x1D), 4);
        assert_eq!(slot_for(0x5B, 0x5B), 4);
        assert_eq!(slot_for(0, 0x56), 4); // ISO extra key
    }

    #[test]
    fn slots_route_the_edit_cluster_and_keypad() {
        // navigation cluster is E0 prefixed and shares codes with the keypad
        assert_eq!(slot_for(0x24, EXT | 0x47), 1); // home
        assert_eq!(slot_for(0x22, EXT | 0x49), 1); // page up
        assert_eq!(slot_for(0x2D, EXT | 0x52), 1); // insert
        assert_eq!(slot_for(0x23, EXT | 0x4F), 2); // end
        assert_eq!(slot_for(0x2E, EXT | 0x53), 2); // delete
        assert_eq!(slot_for(0x25, EXT | 0x4B), 4); // arrows
        assert_eq!(slot_for(0x28, EXT | 0x48), 4);
        assert_eq!(slot_for(0x2C, EXT | 0x37), 0); // print screen
        // the keypad without the prefix walks the same rows as the letters
        assert_eq!(slot_for(0x67, 0x47), 1); // 7
        assert_eq!(slot_for(0x6A, 0x37), 1); // *
        assert_eq!(slot_for(0x90, 0x45), 1); // num lock
        assert_eq!(slot_for(0x66, 0x4B), 2); // 4
        assert_eq!(slot_for(0x61, 0x4F), 3); // 1
        assert_eq!(slot_for(0x60, 0x52), 4); // 0
    }

    #[test]
    fn release_variants_differ_for_natural_feel() {
        let bank = build_bank(48_000, 0);
        let a = &bank.profiles[0].up[0];
        let b = &bank.profiles[0].up[1];
        let diff: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum();
        assert!(diff > 0.5, "release variants are identical");
    }

    #[test]
    fn resampler_keeps_length_and_level() {
        let tone: Vec<f32> = (0..4_800)
            .map(|i| (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin())
            .collect();
        for step in [44_100.0 / 48_000.0, 1.0, 96_000.0 / 48_000.0, 1.021] {
            let out = resample(&tone, step);
            let expected = (tone.len() as f32 / step).round() as i32;
            assert!((out.len() as i32 - expected).abs() <= 2, "length {} vs {expected}", out.len());
            let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!((peak - 1.0).abs() < 0.05, "level shifted to {peak} at step {step}");
        }
    }
}
