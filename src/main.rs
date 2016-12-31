#![feature(integer_atomics, test)]

extern crate sqa_jack;
extern crate rsndfile;
extern crate bounded_spsc_queue;
extern crate time;
extern crate arrayvec;
extern crate hound;
extern crate test;

use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicU64, AtomicPtr};
use std::sync::atomic::Ordering::*;
use bounded_spsc_queue::{Consumer, Producer};
use arrayvec::ArrayVec;
use std::sync::Arc;
use time::Duration;
use sqa_jack::*;

const PLAYERS_PER_STREAM: usize = 256;
const STREAM_BUFFER_SIZE: usize = 50_000;
const ONE_SECOND_IN_NANOSECONDS: u64 = 1_000_000_000;

struct Sender {
    position: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    start_time: Arc<AtomicU64>,
    output_patch: Arc<AtomicPtr<jack_port_t>>,
    buf: Producer<f32>,
    sample_rate: u64
}
impl Sender {
    fn buf(&mut self) -> &mut Producer<f32> {
        &mut self.buf
    }
    fn set_active(&mut self, active: bool) {
        self.active.store(active, Relaxed);
    }
    fn active(&self) -> bool {
        self.active.load(Relaxed)
    }
    fn alive(&self) -> bool {
        self.alive.load(Relaxed)
    }
    fn position_samples(&self) -> u64 {
        self.position.load(Relaxed)
    }
    fn position(&self) -> Duration {
        Duration::nanoseconds((self.position.load(Relaxed) / (self.sample_rate * ONE_SECOND_IN_NANOSECONDS)) as i64)
    }
    fn output_patch(&self) -> JackPort {
        unsafe { JackPort::from_ptr(self.output_patch.load(Relaxed)) }
    }
    fn set_output_patch(&mut self, patch: &JackPort) {
        self.output_patch.store(patch.as_ptr(), Relaxed);
    }
    fn set_start_time(&mut self, st: u64) {
        self.start_time.store(st, Relaxed);
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.active.store(false, Relaxed);
        self.alive.store(false, Relaxed);
    }
}
struct Player {
    buf: Consumer<f32>,
    sample_rate: u64,
    start_time: Arc<AtomicU64>,
    position: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    output_patch: Arc<AtomicPtr<jack_port_t>>
}
impl Drop for Player {
    fn drop(&mut self) {
        self.active.store(false, Relaxed);
        self.alive.store(false, Relaxed);
    }
}
struct DeviceContext {
    players: ArrayVec<[Player; PLAYERS_PER_STREAM]>,
    control: Consumer<AudioThreadCommand>,
    length: Arc<AtomicUsize>,
    sample_rate: u64
}
impl DeviceContext {
    #[inline(always)]
    fn handle(&mut self, cmd: AudioThreadCommand) {
        match cmd {
            AudioThreadCommand::AddPlayer(p) => {
                if self.players.push(p).is_none() {
                    let len = self.length.load(Acquire);
                    self.length.store(len + 1, Release);
                    self.players[self.players.len()-1].alive.store(true, Release);
                }
            }
        }
    }
}
impl JackHandler for DeviceContext {
    #[inline(always)]
    fn process(&mut self, out: &JackCallbackContext) -> JackControl {
        let time = time::precise_time_ns();
        if let Some(cmd) = self.control.try_pop() {
            self.handle(cmd);
        }
        let mut to_remove = None;
        'outer: for (idx, player) in self.players.iter_mut().enumerate() {
            if !player.alive.load(Relaxed) {
                if to_remove.is_none() {
                    to_remove = Some(idx);
                }
                continue;
            }
            if !player.active.load(Relaxed) {
                continue;
            }
            let outpatch = player.output_patch.load(Relaxed);
            if outpatch.is_null() {
                player.active.store(false, Relaxed);
                continue;
            }
            let start_time = player.start_time.load(Relaxed);
            if start_time > time {
                player.position.store(0, Relaxed);
                continue;
            }
            let sample_delta = (time - start_time) / (self.sample_rate * ONE_SECOND_IN_NANOSECONDS);
            let mut pos = player.position.load(Relaxed);
            while pos+1 < sample_delta {
                if player.buf.try_pop().is_none() {
                    continue 'outer;
                }
                pos += 1;
            }
            if player.buf.size() < out.nframes() as usize {
                continue;
            }
            let port = unsafe { JackPort::from_ptr(outpatch) };
            if let Some(buf) = out.get_port_buffer(&port) {
                for x in buf.iter_mut() {
                    if let Some(data) = player.buf.try_pop() {
                        *x = data;
                        pos += 1;
                    }
                }
            }
            player.position.store(pos, Relaxed);
        }
        if let Some(x) = to_remove {
            self.players.swap_remove(x);
            self.length.store(self.length.load(Relaxed) - 1, Relaxed);
        }
        let time2 = time::precise_time_ns();
        JackControl::Continue
    }
}
struct EngineContext {
    pub conn: JackConnection<Activated>,
    length: Arc<AtomicUsize>,
    control: Producer<AudioThreadCommand>,
}
impl EngineContext {
    fn new(name: &str) -> JackResult<Self> {
        let len = Arc::new(AtomicUsize::new(0));
        let (p, c) = bounded_spsc_queue::make(128);
        let mut conn = JackConnection::connect(name)?;
        let dctx = DeviceContext {
            players: ArrayVec::new(),
            control: c,
            length: len.clone(),
            sample_rate: conn.sample_rate() as u64
        };
        conn.set_handler(dctx)?;
        let conn = match conn.activate() {
            Ok(c) => c,
            Err((_, err)) => return Err(err)
        };
        Ok(EngineContext {
            conn: conn,
            length: len,
            control: p
        })
    }
    fn num_senders(&self) -> usize {
        self.length.load(Relaxed)
    }
    fn new_sender(&mut self, sample_rate: u64) -> Sender {
        let (p, c) = bounded_spsc_queue::make(STREAM_BUFFER_SIZE);
        let active = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(false));
        let position = Arc::new(AtomicU64::new(0));
        let start_time = Arc::new(AtomicU64::new(0));
        let output_patch = Arc::new(AtomicPtr::new(::std::ptr::null_mut()));

        self.control.push(AudioThreadCommand::AddPlayer(Player {
            buf: c,
            sample_rate: sample_rate,
            start_time: start_time.clone(),
            position: position.clone(),
            active: active.clone(),
            alive: alive.clone(),
            output_patch: output_patch.clone()
        }));

        Sender {
            buf: p,
            position: position,
            active: active,
            alive: alive,
            output_patch: output_patch,
            start_time: start_time,
            sample_rate: sample_rate
        }
    }
}
enum AudioThreadCommand {
    AddPlayer(Player)
}

use std::thread;
fn main() {
    let mut ec = EngineContext::new("SQA Engine beta0").unwrap();
    let mut reader = hound::WavReader::open("test.wav").unwrap();
    let mut chans = vec![];
    for ch in 0..reader.spec().channels {
        let st = format!("channel {}", ch);
        let p = ec.conn.register_port(&st, PORT_IS_OUTPUT).unwrap();
        let mut send = ec.new_sender(reader.spec().sample_rate as u64);
        send.set_output_patch(&p);
        chans.push((p, send));
    }
    for (i, port) in ec.conn.get_ports(None, None, Some(PORT_IS_INPUT | PORT_IS_PHYSICAL)).unwrap().into_iter().enumerate() {
        if let Some(ch) = chans.get(i) {
            ec.conn.connect_ports(&ch.0, &port).unwrap();
        }
    }
    let thr = thread::spawn(move || {
        let mut idx = 0;
        let mut act = false;
        for samp in reader.samples::<f32>() {
            chans[idx].1.buf().push(samp.unwrap());
            idx += 1;
            if idx >= chans.len() {
                if !act {
                    act = true;
                    let time = time::precise_time_ns();
                    for ch in chans.iter_mut() {
                        ch.1.set_start_time(time);
                        ch.1.set_active(true);
                    }
                }
                idx = 0;
            }
        }
    });
    thread::sleep(::std::time::Duration::new(1000, 0));
    thr.join().unwrap();
}
