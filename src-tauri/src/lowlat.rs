//! WASAPI renderer built directly on `IAudioClient3`.
//!
//! Shared mode is what the app opens by default: cpal is always locked to the
//! WASAPI default engine period (a fixed 10 ms here, whatever buffer it is
//! asked for), while `InitializeSharedAudioStream` accepts any period down to
//! the engine minimum. But the engine position is what it is: a keystroke
//! cannot be written into frames the engine has already passed, so shared mode
//! always costs a pickup wait of up to one period.
//!
//! Exclusive mode (opt in, "حالت انحصاری") takes the endpoint over
//! completely and runs it at the device's own minimum period — the only way to
//! get below the shared engine floor. The price is that no other app can play
//! audio while it is on, which is why it is a toggle and not the default, and
//! why every failure falls back to shared mode without stopping the sound.

use crate::audio::Mixer;
use crate::state::Shared;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{GUID, IUnknown, PCWSTR};
use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_E_BUFFER_ERROR, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentThread, SetThreadPriority,
    WaitForSingleObject, THREAD_PRIORITY_TIME_CRITICAL,
};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// How many engine periods of audio to keep queued in shared mode. One period
/// keeps the delay down to a single period: the event fires as the buffer runs
/// dry, and the fill has a whole period (about 10 ms here) to happen before the
/// engine starves. The mixing itself takes well under a millisecond, so that
/// is plenty of slack.
const QUEUED_PERIODS: u32 = 1;

/// 100 ns units per second, the unit the audio APIs call HNSTIME.
const HNS_PER_S: i64 = 10_000_000;

/// Set by `--selftest` before the audio thread starts: prints what the mixer
/// actually saw for the first few engine ticks, to check the queue depth.
pub static TRACE_FILLS: AtomicBool = AtomicBool::new(false);

/// How many engine ticks the trace reports.
const TRACE_TICKS: u32 = 24;

/// The mix format block returned by `GetMixFormat` lives in CoTaskMem.
struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const core::ffi::c_void)) }
    }
}

/// What the endpoint buffer holds, so the mixer can be specialised once.
#[derive(Clone, Copy, PartialEq)]
enum Fmt {
    F32,
    I16,
}

struct Stream {
    client: IAudioClient3,
    render: IAudioRenderClient,
    event: HANDLE,
    /// Frames the engine wants per callback.
    period: u32,
    /// Frames the endpoint buffer can hold.
    buffer: u32,
    /// Frames to keep queued, see `QUEUED_PERIODS`.
    target: u32,
    channels: usize,
    sample_rate: u32,
    fmt: Fmt,
    exclusive: bool,
    timeout_ms: u32,
    stalls: u32,
    /// Consecutive ticks the exclusive packet refused to be locked.
    busy_ticks: u32,
    /// Diagnostics for `TRACE_FILLS`: when the last fill happened and how many
    /// ticks are still reported.
    last_fill: Option<Instant>,
    trace_left: u32,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// The default render endpoint: the speakers the mix format, the session list
/// and this app's own stream all belong to.
pub unsafe fn default_device() -> Result<IMMDevice, String> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
            .map_err(|e| format!("endpoint enumerator: {e}"))?;
    enumerator
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(|e| format!("default render endpoint: {e}"))
}

unsafe fn default_client() -> Result<IAudioClient3, String> {
    default_device()?
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("activate IAudioClient3: {e}"))
}

/// Frame count of a duration expressed in 100 ns units.
fn hns_to_frames(hns: i64, rate: u32) -> u32 {
    ((hns as u128 * rate as u128) / HNS_PER_S as u128) as u32
}

impl Stream {
    fn open(shared: &Arc<Shared>) -> Result<Self, String> {
        unsafe {
            if shared.want_exclusive.load(Ordering::Relaxed)
                && !shared.hold_shared.load(Ordering::Relaxed)
            {
                match Self::open_exclusive() {
                    Ok(stream) => {
                        shared.mode_exclusive.store(true, Ordering::Relaxed);
                        return Ok(stream);
                    }
                    Err(e) => {
                        eprintln!("MechKeys: exclusive mode unavailable ({e}), using shared mode");
                    }
                }
            }
            let stream = Self::open_shared()?;
            shared.mode_exclusive.store(false, Ordering::Relaxed);
            Ok(stream)
        }
    }

    unsafe fn open_shared() -> Result<Self, String> {
        let client = default_client()?;
        let mix = MixFormat(client.GetMixFormat().map_err(|e| format!("mix format: {e}"))?);
        let (channels, sample_rate) = parse_format(mix.0)?;

        let mut default_period = 0u32;
        let mut fundamental = 0u32;
        let mut min_period = 0u32;
        let mut max_period = 0u32;
        client
            .GetSharedModeEnginePeriod(
                mix.0,
                &mut default_period,
                &mut fundamental,
                &mut min_period,
                &mut max_period,
            )
            .map_err(|e| format!("engine period query: {e}"))?;
        let period = min_period.max(1);

        client
            .InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, mix.0, None)
            .map_err(|e| format!("shared stream init at {period} frames: {e}"))?;

        Self::finish(
            client,
            channels,
            sample_rate,
            Fmt::F32,
            period,
            false,
            format!(
                "shared: {channels}ch float @ {sample_rate} Hz, engine period min {min_period} def {default_period} max {max_period} (fundamental {fundamental})"
            ),
        )
    }

    /// Takes the endpoint at the device's own minimum period. The format is
    /// 16 bit PCM because that is what virtually every endpoint accepts in
    /// exclusive mode; the rate follows the mix format so no resampling is
    /// needed on the usual machine.
    unsafe fn open_exclusive() -> Result<Self, String> {
        let client = default_client()?;
        let mix = MixFormat(client.GetMixFormat().map_err(|e| format!("mix format: {e}"))?);
        let (channels, sample_rate) = parse_format(mix.0)?;

        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: sample_rate,
            wBitsPerSample: 16,
            nBlockAlign: 4,
            nAvgBytesPerSec: sample_rate * 4,
            cbSize: 0,
        };
        let hr = client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &fmt, None);
        if hr.is_err() {
            return Err(format!("16 bit PCM not accepted ({hr:?})"));
        }

        // The device's own period budget. Exclusive event driven streams need
        // a buffer that is a multiple of it.
        let mut default_period = 0i64;
        let mut min_period = 0i64;
        client
            .GetDevicePeriod(Some(&mut default_period), Some(&mut min_period))
            .map_err(|e| format!("device period query: {e}"))?;
        if min_period <= 0 {
            min_period = default_period.max(1);
        }

        // The exclusive mode walkthrough in the docs: ask for the device's
        // minimum period as both buffer size and periodicity, and when the
        // device answers BUFFER_SIZE_NOT_ALIGNED, read back the size it chose
        // and ask again with that. Requesting anything else silently clamps
        // the buffer to a size the stream then refuses to hand out.
        let mut last_err = String::from("exclusive init was not accepted");
        for mul in [1i64, 2] {
            let mut hns = min_period * mul;
            let mut init = client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                hns,
                hns,
                &fmt,
                None,
            );
            if matches!(&init, Err(e) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED) {
                let frames = client
                    .GetBufferSize()
                    .map_err(|e| format!("aligned buffer size: {e}"))?;
                hns = (HNS_PER_S as f64 / sample_rate as f64 * frames as f64 + 0.5) as i64;
                init = client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    hns,
                    hns,
                    &fmt,
                    None,
                );
            }
            match init {
                Ok(()) => {
                    return Self::finish(
                        client,
                        channels,
                        sample_rate,
                        Fmt::I16,
                        hns_to_frames(hns, sample_rate).max(1),
                        true,
                        format!("exclusive: 2ch 16 bit @ {sample_rate} Hz, device minimum period"),
                    );
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(format!("exclusive init: {last_err}"))
    }

    /// Everything after a successful `Initialize`: event handle, render
    /// client, shared bookkeeping. The buffer is prefilled with silence
    /// before `Start`, which exclusive mode demands of the first pass.
    unsafe fn finish(
        client: IAudioClient3,
        channels: usize,
        sample_rate: u32,
        fmt: Fmt,
        period: u32,
        exclusive: bool,
        log: String,
    ) -> Result<Self, String> {
        let buffer = client.GetBufferSize().map_err(|e| format!("buffer size: {e}"))?.max(1);
        // An exclusive event driven stream swaps the whole endpoint buffer once
        // per event, so the callback period is the buffer size itself, whatever
        // was requested at init.
        let period = if exclusive { buffer } else { period };
        let render: IAudioRenderClient = client.GetService().map_err(|e| format!("render client: {e}"))?;
        let event = CreateEventW(None, BOOL(0), BOOL(0), PCWSTR::null())
            .map_err(|e| format!("audio event: {e}"))?;
        client
            .SetEventHandle(event)
            .map_err(|e| format!("event handle: {e}"))?;

        let timeout_ms = (period as u64 * 4 * 1000 / sample_rate.max(1) as u64).max(40) as u32;
        let period_ms = period as f64 * 1000.0 / sample_rate.max(1) as f64;

        eprintln!("MechKeys: render {log}, buffer {buffer} frames ({period_ms:.2} ms/period)");

        let stream = Self {
            client,
            render,
            event,
            period,
            buffer,
            target: if exclusive { buffer } else { (period * QUEUED_PERIODS).min(buffer) },
            channels,
            sample_rate,
            fmt,
            exclusive,
            timeout_ms,
            stalls: 0,
            busy_ticks: 0,
            last_fill: None,
            trace_left: if TRACE_FILLS.load(Ordering::Relaxed) {
                TRACE_TICKS
            } else {
                0
            },
        };
        stream.prefill()?;
        Ok(stream)
    }

    /// Writes silence through the whole endpoint buffer before the engine
    /// runs, so an exclusive stream starts clean instead of with a glitch.
    unsafe fn prefill(&self) -> Result<(), String> {
        if let Ok(frames) = self.render.GetBuffer(self.buffer) {
            let len = self.buffer as usize * self.channels;
            match self.fmt {
                Fmt::F32 => {
                    std::slice::from_raw_parts_mut(frames as *mut f32, len).fill(0.0);
                }
                Fmt::I16 => {
                    std::slice::from_raw_parts_mut(frames as *mut i16, len).fill(0);
                }
            }
            let _ = self.render.ReleaseBuffer(self.buffer, 0);
        }
        Ok(())
    }

    fn start(&self) -> Result<(), String> {
        unsafe {
            self.client
                .Start()
                .map_err(|e| format!("stream start: {e}"))
        }
    }

    /// Waits for one engine tick and tops the endpoint buffer back up.
    fn pump(&mut self, mixer: &mut Mixer) -> Result<(), String> {
        unsafe {
            let wait = WaitForSingleObject(self.event, self.timeout_ms);
            if wait.0 == WAIT_TIMEOUT.0 {
                // A missed tick is harmless, a silent engine is not.
                self.stalls += 1;
                if self.stalls > 8 {
                    return Err("audio engine stopped signalling".into());
                }
                return Ok(());
            }
            if wait.0 != WAIT_OBJECT_0.0 {
                return Err(format!("audio event wait: 0x{:08X}", wait.0));
            }
            self.stalls = 0;

            // Shared mode mixes several clients into one long engine buffer, so
            // the fill tops it up to `target`. An exclusive event driven stream
            // has no engine in the way: its whole buffer is the single packet
            // that swaps on every event, and padding there is not meaningful.
            let queued = if self.exclusive {
                self.buffer
            } else {
                let padding = self
                    .client
                    .GetCurrentPadding()
                    .map_err(|e| format!("padding: {e}"))?;

                // In steady state the engine signals one period after the previous
                // fill, so this is the wake up delay: the part of the period not
                // spent playing our samples.
                if self.trace_left > 0 {
                    if let Some(prev) = self.last_fill {
                        self.trace_left -= 1;
                        let tick = Duration::from_secs_f64(
                            self.period as f64 / self.sample_rate.max(1) as f64,
                        );
                        let late = Instant::now().saturating_duration_since(prev + tick);
                        eprintln!(
                            "MechKeys tick {}: padding {padding}, late {:.2} ms",
                            TRACE_TICKS - self.trace_left,
                            late.as_secs_f64() * 1000.0
                        );
                    }
                }

                self.target
                    .saturating_sub(padding)
                    .min(self.buffer.saturating_sub(padding))
            };
            if queued == 0 {
                return Ok(());
            }

            let frames = match self.render.GetBuffer(queued) {
                Ok(f) => f,
                // Exclusive mode answers this when the packet the event just
                // freed has not been handed back yet. Waiting one more tick
                // costs far less than what the mode saves.
                Err(e) if self.exclusive && e.code() == AUDCLNT_E_BUFFER_ERROR => {
                    self.busy_ticks += 1;
                    if self.busy_ticks > 64 {
                        return Err("exclusive packet never became available".into());
                    }
                    return Ok(());
                }
                Err(e) => return Err(format!("render buffer: {e}")),
            };
            self.busy_ticks = 0;
            let len = queued as usize * self.channels;
            match self.fmt {
                Fmt::F32 => {
                    let samples = std::slice::from_raw_parts_mut(frames as *mut f32, len);
                    mixer.render(samples, self.channels);
                }
                Fmt::I16 => {
                    let samples = std::slice::from_raw_parts_mut(frames as *mut i16, len);
                    mixer.render(samples, self.channels);
                }
            }
            self.render
                .ReleaseBuffer(queued, 0)
                .map_err(|e| format!("release buffer: {e}"))?;
            self.last_fill = Some(Instant::now());
        }
        Ok(())
    }
}

struct Session {
    stream: Stream,
    mixer: Mixer,
}

impl Session {
    fn open(shared: &Arc<Shared>) -> Result<Self, String> {
        let stream = Stream::open(shared)?;
        shared
            .sample_rate
            .store(stream.sample_rate, Ordering::Relaxed);
        shared.cb_frames.store(stream.period, Ordering::Relaxed);
        shared.ensure_bank();
        Ok(Self {
            mixer: Mixer::new(shared.clone()),
            stream,
        })
    }

    fn open_started(shared: &Arc<Shared>) -> Result<Self, String> {
        let session = Self::open(shared)?;
        session.stream.start()?;
        Ok(session)
    }

    fn pump(&mut self) -> Result<(), String> {
        self.stream.pump(&mut self.mixer)
    }
}

/// Runs the audio engine for the life of the process. Only returns if the very
/// first stream could not be opened, so the caller can try the fallback backend.
pub fn run(shared: Arc<Shared>, ready: Sender<Result<(), String>>) -> Result<(), String> {
    // This thread never pumps messages, so it has to live in the MTA.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            return Err(format!("CoInitializeEx: {hr:?}"));
        }
        // MMCSS "Pro Audio" is how Windows schedules audio work: it raises the
        // priority and the timing guarantees in one step. With only one period
        // queued, a late wake up would leave a gap, so the raw boost is kept as
        // the fallback for machines where the scheduler refuses the task.
        let mut task_index = 0u32;
        if AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task_index).is_err() {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
    }

    let mut session = Session::open_started(&shared)?;
    let _ = ready.send(Ok(()));

    loop {
        if shared.reopen.swap(false, Ordering::Relaxed) {
            // The endpoint must be released before it can be taken over in
            // exclusive mode, so the old stream goes first.
            drop(session);
            session = reopen(&shared);
            continue;
        }
        match session.pump() {
            Ok(()) => {}
            Err(e) => {
                eprintln!("MechKeys: audio stream lost ({e}), reopening");
                drop(session);
                session = reopen(&shared);
            }
        }
    }
}

/// Device changes and format changes invalidate the client, so build a new one.
fn reopen(shared: &Arc<Shared>) -> Session {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        match Session::open_started(shared) {
            Ok(session) => return session,
            Err(e) => eprintln!("MechKeys: audio reopen failed ({e})"),
        }
    }
}

/// Shared mode always mixes to 32 bit float; anything else means this device
/// needs the cpal path, which can convert.
///
/// The format structs are 1 byte packed, so every field is copied out by value:
/// taking a reference to a packed field is undefined behaviour.
unsafe fn parse_format(fmt: *const WAVEFORMATEX) -> Result<(usize, u32), String> {
    let tag = (*fmt).wFormatTag;
    let bits = (*fmt).wBitsPerSample;
    let channels = (*fmt).nChannels;
    let sample_rate = (*fmt).nSamplesPerSec;

    let float = if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = std::ptr::read_unaligned(fmt as *const WAVEFORMATEXTENSIBLE);
        // cbSize covers the union, mask and subformat tail.
        let cb_size = ext.Format.cbSize;
        let sub_format = ext.SubFormat;
        cb_size >= 22 && sub_format == SUBTYPE_IEEE_FLOAT
    } else {
        false
    };

    if !float || bits != 32 {
        return Err(format!(
            "mix format is not 32 bit float (tag {tag:#06x}, {bits} bits)"
        ));
    }
    if channels == 0 || sample_rate == 0 {
        return Err("mix format has no channels".into());
    }
    Ok((channels as usize, sample_rate))
}

/// Reports what the device can actually do, so the exclusive mode decision is
/// not guesswork. Run as `mechkeys --probe`.
pub fn probe() -> ! {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            eprintln!("CoInitializeEx: {hr:?}");
            std::process::exit(1);
        }
        let client = match default_client() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        let mix = MixFormat(match client.GetMixFormat() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("mix format: {e}");
                std::process::exit(1);
            }
        });
        let (channels, rate) = match parse_format(mix.0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        println!("mix format    : {channels}ch float @ {rate} Hz");

        let mut def = 0u32;
        let mut fun = 0u32;
        let mut min = 0u32;
        let mut max = 0u32;
        if let Err(e) = client.GetSharedModeEnginePeriod(mix.0, &mut def, &mut fun, &mut min, &mut max) {
            println!("shared periods: query failed ({e})");
        } else {
            println!(
                "shared periods: min {min} def {def} max {max} frames (fundamental {fun}) @ {rate} Hz"
            );
            println!(
                "               : min {:.2} ms / def {:.2} ms",
                min as f64 * 1000.0 / rate as f64,
                def as f64 * 1000.0 / rate as f64
            );
        }

        let mut ddef = 0i64;
        let mut dmin = 0i64;
        if let Err(e) = client.GetDevicePeriod(Some(&mut ddef), Some(&mut dmin)) {
            println!("device periods : query failed ({e})");
        } else {
            let frames = |hns: i64| hns_to_frames(hns, rate);
            println!(
                "device periods : min {dmin} hns ({:.2} ms, {} frames) / def {ddef} hns ({:.2} ms, {} frames)",
                dmin as f64 / 10_000.0,
                frames(dmin),
                ddef as f64 / 10_000.0,
                frames(ddef)
            );
        }

        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: rate,
            wBitsPerSample: 16,
            nBlockAlign: 4,
            nAvgBytesPerSec: rate * 4,
            cbSize: 0,
        };
        let hr = client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &fmt, None);
        if hr.is_ok() {
            println!("exclusive      : 2ch 16 bit @ {rate} Hz accepted");
        } else {
            println!("exclusive      : 2ch 16 bit @ {rate} Hz rejected ({hr:?})");
        }
        std::process::exit(0);
    }
}
