//! Who else is using the speakers.
//!
//! Exclusive mode is what buys the 3 ms output, and it locks every other
//! application out of the endpoint for as long as it lasts. That is the right
//! trade while the user is typing and the wrong one the moment they press play
//! on a video, so something has to answer the question the monitor thread waits
//! on: is another process rendering audio right now?
//!
//! The catch is that the question cannot be asked while the endpoint is ours.
//! Nobody else can play, so every session reads inactive. The monitor
//! therefore lets go of the device on its own once the keyboard has been quiet
//! for a while, asks the question from shared mode, and takes the endpoint back
//! only once the answer has been no for a stretch and the keys are moving
//! again.

use crate::lowlat::default_device;
use crate::state::Shared;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateInactive, IAudioSessionControl2,
    IAudioSessionManager2,
};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// How long the keyboard has to be silent before exclusive mode steps aside.
/// Typing is the only thing the mode is for, so giving it up after a pause
/// costs nothing and lets a clip start on the first press of play.
const IDLE_MS: u64 = 12_000;
/// Poll interval, and so the unit of the counters below.
const POLL: Duration = Duration::from_millis(500);
/// Polls the speakers must stay clear before the endpoint is taken back: one
/// pause between two clips is not the end of the watching.
const CLEAN_POLLS: u32 = 3;
/// How recent the last keystroke has to be to count as "back at the keys", so
/// the reacquire lands when the latency would actually be noticed.
const TYPING_MS: u64 = 3_000;
/// A session has to be audible, not merely active. Plenty of applications keep a
/// stream open and render nothing at all — remote desktop tools and every
/// browser with a page loaded among them — and one of those would otherwise keep
/// exclusive mode away for good.
const PEAK_FLOOR: f32 = 0.001;

/// One application's stream on the default render endpoint.
struct Session {
    name: String,
    pid: u32,
    /// Active: the stream is rendering right now, not merely open.
    active: bool,
    /// The session's own level meter, 0 to 1.
    peak: f32,
}

impl Session {
    /// Whether this is somebody else's stream that anybody can actually hear.
    fn blocking(&self) -> bool {
        self.pid != std::process::id() && self.active && self.peak > PEAK_FLOOR
    }
}

/// Every session on the default render endpoint.
unsafe fn list() -> Result<Vec<Session>, String> {
    let device = default_device()?;
    let manager: IAudioSessionManager2 = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("activate session manager: {e}"))?;
    let sessions = manager
        .GetSessionEnumerator()
        .map_err(|e| format!("session enumerator: {e}"))?;
    let count = sessions.GetCount().map_err(|e| format!("session count: {e}"))?;
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count {
        let Ok(session) = sessions.GetSession(i) else {
            continue;
        };
        // The name comes back as a string the callee allocated, so it has to be
        // handed back or this poll leaks a little every half second.
        let mut name = String::new();
        if let Ok(title) = session.GetDisplayName() {
            name = unsafe { title.to_string() }.unwrap_or_default();
            unsafe { CoTaskMemFree(Some(title.0 as *const _)) };
        }
        out.push(Session {
            name,
            pid: session
                .cast::<IAudioSessionControl2>()
                .and_then(|c| c.GetProcessId())
                .unwrap_or(0),
            active: session.GetState().unwrap_or(AudioSessionStateInactive)
                == AudioSessionStateActive,
            peak: session
                .cast::<IAudioMeterInformation>()
                .and_then(|m| m.GetPeakValue())
                .unwrap_or(0.0),
        });
    }
    Ok(out)
}

/// The streams that are keeping exclusive mode away, named as well as Windows
/// is willing to say.
fn blockers() -> Result<Vec<String>, String> {
    Ok(unsafe { list()? }
        .iter()
        .filter(|s| s.blocking())
        .map(Session::label)
        .collect())
}

impl Session {
    /// How the monitor names a stream in the log.
    fn label(&self) -> String {
        if self.name.is_empty() {
            format!("pid {}", self.pid)
        } else {
            self.name.clone()
        }
    }
}

/// Whether a process other than this one is currently making a sound.
///
/// An error is not a no: the monitor leaves well enough alone rather than
/// taking the endpoint back while it cannot see what is playing.
pub fn others_playing() -> Result<bool, String> {
    Ok(!blockers()?.is_empty())
}

/// What the monitor is looking at, for `mechkeys --sessions`. An exclusive mode
/// that never comes back is almost always one program holding a stream open, and
/// this is the list that says which.
pub fn report() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    match unsafe { list() } {
        Ok(sessions) => {
            println!(
                "{:<34} {:>7}  {:>6}  {:>6}  {}",
                "session", "pid", "active", "peak", "in the way"
            );
            for s in &sessions {
                println!(
                    "{:<34.34} {:>7}  {:>6}  {:>6.3}  {}",
                    s.label(),
                    s.pid,
                    s.active,
                    s.peak,
                    s.blocking()
                );
            }
            println!(
                "{} is holding the device out.",
                sessions.iter().filter(|s| s.blocking()).count()
            );
        }
        Err(e) => eprintln!("sessions: {e}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Drop exclusive mode and stay shared until the speakers are clear.
    Release,
    /// The other application has finished: take the endpoint back.
    Reacquire,
}

/// The monitor's memory: how long the speakers have been quiet.
#[derive(Default)]
pub struct Watch {
    clean: u32,
}

impl Watch {
    /// One poll. `others` is what the session list answered, or `None` when it
    /// could not be read. The real device calls stay out of here so the timing
    /// rules can be tried on their own.
    pub fn step(
        &mut self,
        holding: bool,
        exclusive: bool,
        quiet_ms: u64,
        others: Option<bool>,
    ) -> Action {
        self.clean = match others {
            Some(true) | None => 0,
            Some(false) => (self.clean + 1).min(CLEAN_POLLS),
        };
        if holding {
            if self.clean >= CLEAN_POLLS && quiet_ms < TYPING_MS {
                self.clean = 0;
                return Action::Reacquire;
            }
            return Action::Nothing;
        }
        if exclusive && quiet_ms > IDLE_MS {
            self.clean = 0;
            return Action::Release;
        }
        Action::Nothing
    }
}

/// Milliseconds since the last keystroke.
///
/// The stamps in `Shared` count microseconds and every threshold in this module
/// is written in milliseconds, so the two only meet after this conversion. Read
/// straight, twelve thousand microseconds looks like twelve seconds and is in
/// fact twelve milliseconds: the monitor would hand the endpoint away the moment
/// the typing paused, and never take it back.
fn quiet_ms(shared: &Shared) -> u64 {
    let now = shared.epoch.elapsed().as_micros() as u64;
    now.saturating_sub(shared.last_key_us.load(Ordering::Relaxed)) / 1000
}

/// Runs the monitor for the life of the process.
pub fn start(shared: Arc<Shared>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("mechkeys-watch".into())
        .spawn(move || {
            // The session manager is a COM object like the rest of this module.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut watch = Watch::default();
            // A monitor that cannot see the speakers is a monitor that never hands
            // exclusive mode back, so the reason is worth one line in the log — but
            // only the first time it changes.
            let mut said = String::new();
            loop {
                std::thread::sleep(POLL);
                if !shared.auto_release.load(Ordering::Relaxed) {
                    if shared.hold_shared.swap(false, Ordering::Relaxed) {
                        shared.reopen.store(true, Ordering::Relaxed);
                    }
                    continue;
                }
                let holding = shared.hold_shared.load(Ordering::Relaxed);
                let exclusive = shared.mode_exclusive.load(Ordering::Relaxed);
                if !holding && !exclusive {
                    // Nothing is being held, so there is nothing to hand back.
                    continue;
                }
                let quiet = quiet_ms(&shared);
                // While the endpoint is ours nobody else can have a stream, so
                // the list is only worth reading from shared mode.
                let others = if holding {
                    match others_playing() {
                        Ok(playing) => Some(playing),
                        Err(e) => {
                            if e != said {
                                said = e.clone();
                                eprintln!("MechKeys: {e}");
                            }
                            None
                        }
                    }
                } else {
                    Some(false)
                };
                match watch.step(holding, exclusive, quiet, others) {
                    Action::Nothing => {}
                    Action::Release => {
                        shared.hold_shared.store(true, Ordering::Relaxed);
                        shared.reopen.store(true, Ordering::Relaxed);
                    }
                    Action::Reacquire => {
                        shared.hold_shared.store(false, Ordering::Relaxed);
                        shared.reopen.store(true, Ordering::Relaxed);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_board_hands_the_endpoint_over() {
        let mut w = Watch::default();
        assert_eq!(w.step(false, true, 1_000, Some(false)), Action::Nothing);
        assert_eq!(w.step(false, true, IDLE_MS, Some(false)), Action::Nothing);
        assert_eq!(w.step(false, true, IDLE_MS + 1, Some(false)), Action::Release);
        // Already holding: the same idle time must not keep reopening the stream.
        assert_eq!(w.step(true, false, IDLE_MS + 1, Some(false)), Action::Nothing);
    }

    #[test]
    fn exclusive_off_leaves_everything_alone() {
        let mut w = Watch::default();
        for _ in 0..40 {
            assert_eq!(w.step(false, false, 60_000, Some(false)), Action::Nothing);
        }
    }

    #[test]
    fn it_waits_for_a_real_gap_in_the_playing() {
        let mut w = Watch::default();
        // Three clear polls, then the clip resumes: the count starts over.
        for _ in 0..(CLEAN_POLLS - 1) {
            assert_eq!(w.step(true, false, 500, Some(false)), Action::Nothing);
        }
        assert_eq!(w.step(true, false, 500, Some(true)), Action::Nothing);
        assert_eq!(w.step(true, false, 500, Some(false)), Action::Nothing);
        for _ in 0..CLEAN_POLLS {
            if w.step(true, false, 500, Some(false)) == Action::Reacquire {
                return;
            }
        }
        panic!("the endpoint was never handed back");
    }

    #[test]
    fn an_unreadable_session_list_is_not_a_clear_one() {
        let mut w = Watch::default();
        for _ in 0..10 {
            assert_eq!(w.step(true, false, 500, None), Action::Nothing);
        }
    }

    #[test]
    fn it_only_takes_the_endpoint_back_for_someone_who_is_typing() {
        let mut w = Watch::default();
        for _ in 0..CLEAN_POLLS {
            assert_eq!(w.step(true, false, 60_000, Some(false)), Action::Nothing);
        }
        // The speakers have been clear for long, but the desk is empty.
        assert_eq!(w.step(true, false, 60_000, Some(false)), Action::Nothing);
        // The first keystroke brings the low latency path back with it.
        assert_eq!(w.step(true, false, 200, Some(false)), Action::Reacquire);
    }

    #[test]
    fn only_an_audible_stream_is_somebody_playing() {
        let seen = |peak: f32, active: bool, pid: u32| Session {
            name: String::new(),
            pid,
            active,
            peak,
        };
        let other = || if std::process::id() == 4242 { 4243 } else { 4242 };
        // A stream left open and rendering nothing must not keep exclusive mode
        // away for the rest of the session.
        assert!(!seen(0.0, true, other()).blocking());
        assert!(seen(0.02, true, other()).blocking());
        assert!(!seen(0.9, false, other()).blocking());
        // Our own voice never blocks itself.
        assert!(!seen(0.9, true, std::process::id()).blocking());
    }

    #[test]
    fn the_idle_clock_counts_milliseconds() {
        let shared = Shared::new(&crate::config::Config::default());
        std::thread::sleep(Duration::from_millis(300));
        // The reading has to be on the same scale as the thresholds it is
        // compared against: 300 of them, not 300 000 microseconds.
        let quiet = quiet_ms(&shared);
        assert!((250..2_000).contains(&quiet), "{quiet} is not milliseconds");
        // A keystroke resets the clock to nothing.
        shared.last_key_us.store(
            shared.epoch.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        assert!(quiet_ms(&shared) < TYPING_MS);
    }
}
