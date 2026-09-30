//! Note audition (scrub preview) worker.
//!
//! Preview strikes run on their own thread that OWNS the destination sinks:
//! every strike carries a deadline the worker enforces itself, so a hung or
//! torn-down UI can never strand a sounding note. Rapid pitch drags are
//! debounced — strikes queued for the same (destination, channel) inside one
//! processing tick collapse to the most recent pitch.

use midi_io::EventSink;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// Hard cap on a strike's sustain — even a missed AllOff releases within this.
const MAX_HOLD_MS: u64 = 8_000;
/// Worker wake period; also the granularity at which queued strikes coalesce.
const TICK: Duration = Duration::from_millis(8);

/// UI → worker commands. `Sender::send` never blocks the UI.
enum AudMsg {
    /// (re)attach the sink for one destination index. Replacing a sink
    /// panics the old one first so nothing rings on a stale connection.
    SetSink { dest: usize, sink: Box<dyn EventSink> },
    /// drop one destination's sink (its identity changed — e.g. the VST3
    /// slot underneath was unloaded); panics it on the way out
    DropSink { dest: usize },
    /// forget every sink (destination catalog remap / rescan)
    ClearSinks,
    /// wire bytes (bank MSB/LSB, program change) to send right before the
    /// next strike on (dest, channel)
    Setup { dest: usize, ch: u8, bytes: Vec<Vec<u8>> },
    /// strike a pitch; the worker auto-sends its note-off after `dur_ms`
    Strike {
        dest: usize,
        ch: u8,
        key: u8,
        vel: u8,
        dur_ms: u64,
    },
    /// release everything immediately (mouse-up, focus loss, doc swap, quit)
    AllOff,
}

pub struct Audition {
    tx: Sender<AudMsg>,
}

impl Audition {
    pub fn spawn() -> Self {
        let (tx, rx) = channel();
        std::thread::spawn(move || worker(rx));
        Self { tx }
    }

    pub fn set_sink(&self, dest: usize, sink: Box<dyn EventSink>) {
        let _ = self.tx.send(AudMsg::SetSink { dest, sink });
    }

    pub fn clear_sinks(&self) {
        let _ = self.tx.send(AudMsg::ClearSinks);
    }

    pub fn drop_sink(&self, dest: usize) {
        let _ = self.tx.send(AudMsg::DropSink { dest });
    }

    pub fn setup(&self, dest: usize, ch: u8, bytes: Vec<Vec<u8>>) {
        let _ = self.tx.send(AudMsg::Setup { dest, ch, bytes });
    }

    pub fn strike(&self, dest: usize, ch: u8, key: u8, vel: u8, dur_ms: u64) {
        let _ = self.tx.send(AudMsg::Strike {
            dest,
            ch,
            key,
            vel,
            dur_ms,
        });
    }

    /// Release every sounding/pending preview note right now.
    pub fn all_off(&self) {
        let _ = self.tx.send(AudMsg::AllOff);
    }
}

struct StrikeReq {
    key: u8,
    vel: u8,
    dur: Duration,
}

fn worker(rx: Receiver<AudMsg>) {
    let mut sinks: HashMap<usize, Box<dyn EventSink>> = HashMap::new();
    // (dest, ch) -> setup bytes to prefix the next strike with
    let mut setups: HashMap<(usize, u8), Vec<Vec<u8>>> = HashMap::new();
    // (dest, ch) -> latest queued strike (debounce: older ones are dropped)
    let mut pending: HashMap<(usize, u8), StrikeReq> = HashMap::new();
    // (dest, ch) -> (sounding key, off deadline)
    let mut sounding: HashMap<(usize, u8), (u8, Instant)> = HashMap::new();

    fn offs_due(
        sinks: &mut HashMap<usize, Box<dyn EventSink>>,
        sounding: &mut HashMap<(usize, u8), (u8, Instant)>,
    ) {
        let now = Instant::now();
        sounding.retain(|(d, c), (key, t)| {
            if now >= *t {
                if let Some(s) = sinks.get_mut(d) {
                    s.send_at(&[0x80 | c, *key, 0], 0);
                }
                false
            } else {
                true
            }
        });
    }

    loop {
        offs_due(&mut sinks, &mut sounding);
        let first = match rx.recv_timeout(TICK) {
            Ok(m) => m,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut disconnected = false;
        {
            let mut handle = |m: AudMsg| match m {
                AudMsg::SetSink { dest, sink } => {
                    if let Some(mut old) = sinks.insert(dest, sink) {
                        old.panic();
                    }
                    // strikes tied to a dead sink identity must not resound
                    pending.retain(|(d, _), _| *d != dest);
                    sounding.retain(|(d, _), _| *d != dest);
                }
                AudMsg::DropSink { dest } => {
                    if let Some(mut s) = sinks.remove(&dest) {
                        s.panic();
                    }
                    setups.retain(|(d, _), _| *d != dest);
                    pending.retain(|(d, _), _| *d != dest);
                    sounding.retain(|(d, _), _| *d != dest);
                }
                AudMsg::ClearSinks => {
                    for s in sinks.values_mut() {
                        s.panic();
                    }
                    sinks.clear();
                    setups.clear();
                    pending.clear();
                    sounding.clear();
                }
                AudMsg::Setup { dest, ch, bytes } => {
                    setups.insert((dest, ch), bytes);
                }
                AudMsg::Strike {
                    dest,
                    ch,
                    key,
                    vel,
                    dur_ms,
                } => {
                    let req = StrikeReq {
                        key,
                        vel,
                        dur: Duration::from_millis(dur_ms.clamp(30, MAX_HOLD_MS)),
                    };
                    pending.insert((dest, ch), req);
                }
                AudMsg::AllOff => {
                    pending.clear();
                    sounding.clear();
                    for s in sinks.values_mut() {
                        s.panic();
                    }
                }
            };
            handle(first);
            // drain the burst that accumulated while we were asleep so only
            // the freshest pitch per channel is struck (drag debounce)
            loop {
                match rx.try_recv() {
                    Ok(m) => handle(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        // fire the coalesced strikes
        for ((d, c), req) in pending.drain() {
            let Some(s) = sinks.get_mut(&d) else { continue };
            if let Some(setup) = setups.remove(&(d, c)) {
                for b in &setup {
                    s.send_at(b, 0);
                }
            }
            if let Some((old_key, _)) = sounding.remove(&(d, c)) {
                s.send_at(&[0x80 | c, old_key, 0], 0);
            }
            s.send_at(&[0x90 | c, req.key, req.vel], 0);
            sounding.insert((d, c), (req.key, Instant::now() + req.dur));
        }
        if disconnected {
            break;
        }
    }
    for s in sinks.values_mut() {
        s.panic();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl EventSink for RecordingSink {
        fn send_at(&mut self, bytes: &[u8], _rem_us: u64) {
            self.0.lock().unwrap().push(bytes.to_vec());
        }
        fn panic(&mut self) {
            for ch in 0u8..16 {
                self.0.lock().unwrap().push(vec![0xB0 | ch, 123, 0]);
            }
        }
    }

    fn wait_for(log: &Arc<Mutex<Vec<Vec<u8>>>>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while log.lock().unwrap().len() < n {
            assert!(Instant::now() < deadline, "worker produced no events");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn strike_is_followed_by_matching_off() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let aud = Audition::spawn();
        aud.set_sink(0, Box::new(RecordingSink(log.clone())));
        aud.strike(0, 0, 60, 100, 50);
        wait_for(&log, 2);
        let snapshot = log.lock().unwrap().clone();
        assert_eq!(snapshot[0], vec![0x90, 60, 100]);
        assert_eq!(snapshot[1], vec![0x80, 60, 0]);
        aud.all_off();
    }

    #[test]
    fn rapid_strikes_same_channel_coalesce() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let aud = Audition::spawn();
        aud.set_sink(0, Box::new(RecordingSink(log.clone())));
        // 30 pitches faster than the worker tick: only the last sounds
        for k in 40..70 {
            aud.strike(0, 0, k, 100, 200);
        }
        // one on + (eventual off + all-off panic rows)
        wait_for(&log, 1);
        let first = log.lock().unwrap()[0].clone();
        assert_eq!(first, vec![0x90, 69, 100], "only newest pitch strikes");
        aud.all_off();
    }

    #[test]
    fn all_off_silences_before_deadline() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let aud = Audition::spawn();
        aud.set_sink(0, Box::new(RecordingSink(log.clone())));
        aud.strike(0, 0, 60, 100, 60_000); // clamped, but AllOff lands first
        wait_for(&log, 1);
        aud.all_off();
        wait_for(&log, 17);
        aud.strike(0, 0, 62, 100, 100);
        wait_for(&log, 18);
        let snapshot = log.lock().unwrap().clone();
        // after the 0x90 there must be 16 panic rows before the next strike
        assert_eq!(snapshot[0], vec![0x90, 60, 100]);
        assert!(
            snapshot[1..17].iter().all(|b| b[0] & 0xF0 == 0xB0),
            "rows after the strike should be panic CCs: {snapshot:?}"
        );
        assert_eq!(snapshot[17], vec![0x90, 62, 100]);
        aud.all_off();
    }

    #[test]
    fn setup_bytes_precede_the_strike() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let aud = Audition::spawn();
        aud.set_sink(0, Box::new(RecordingSink(log.clone())));
        aud.setup(0, 0, vec![vec![0xB0, 0, 12], vec![0xC0, 34]]);
        aud.strike(0, 0, 60, 100, 50);
        wait_for(&log, 4);
        let snapshot = log.lock().unwrap().clone();
        assert_eq!(snapshot[0], vec![0xB0, 0, 12]);
        assert_eq!(snapshot[1], vec![0xC0, 34]);
        assert_eq!(snapshot[2], vec![0x90, 60, 100]);
        aud.all_off();
    }
}
