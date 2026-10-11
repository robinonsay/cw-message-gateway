//! The loop against a board in memory, with synthesized Morse as the radio's
//! audio, quantized as the box's ADC would read it.

use super::*;
use crate::frontend::Frontend;
use crate::{ADC_RATE, DECIMATE};
use cw::{Keyer, Noise};
use std::collections::VecDeque;

/// A board in memory. The clock moves on by the audio each pass takes, as on the
/// real box, where the ADC paces the loop's work.
struct Board {
    now: u64,
    epoch: u32,
    /// Host to box.
    rx: VecDeque<u8>,
    /// Box to host.
    tx: Vec<u8>,
    /// Bytes `usb_write` takes per call.
    room: usize,
    adc: VecDeque<u16>,
    /// What the next `adc_read` reports instead of samples.
    loss: Option<AudioLoss>,
    adc_reads: u32,
    held_low: u32,
    led: bool,
    led_on_passes: u32,
    feeds: u32,
}

impl Board {
    fn new() -> Self {
        Self {
            now: 0,
            epoch: 0,
            rx: VecDeque::new(),
            tx: Vec::new(),
            room: usize::MAX,
            adc: VecDeque::new(),
            loss: None,
            adc_reads: 0,
            held_low: 0,
            led: false,
            led_on_passes: 0,
            feeds: 0,
        }
    }

    fn send(&mut self, id: u8, body: &str) {
        let l = frame::encode(id, format_args!("{body}")).unwrap();
        self.rx.extend(l.as_bytes());
        self.rx.push_back(b'\n');
    }

    /// Everything the box sent, as text.
    fn text(&self) -> String {
        String::from_utf8(self.tx.clone()).unwrap()
    }

    /// Every framed line the box sent: (id, body). Panics on a line that does not
    /// decode.
    fn lines(&self) -> Vec<(u8, String)> {
        self.text()
            .lines()
            .map(|l| {
                let (id, body) = frame::decode(l.as_bytes())
                    .unwrap_or_else(|e| panic!("{l:?} does not decode: {e:?}"));
                (id, body.to_string())
            })
            .collect()
    }
}

impl Hardware for Board {
    fn now_ms(&self) -> u64 {
        self.now
    }

    fn usb_poll(&mut self) {}

    fn link_epoch(&self) -> u32 {
        self.epoch
    }

    fn usb_read(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.rx.len());
        for b in buf.iter_mut().take(n) {
            *b = self.rx.pop_front().unwrap();
        }
        n
    }

    fn usb_write(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.room);
        self.tx.extend_from_slice(&bytes[..n]);
        n
    }

    fn adc_read(&mut self, buf: &mut [u16]) -> Result<usize, AudioLoss> {
        self.adc_reads += 1;
        if let Some(loss) = self.loss.take() {
            return Err(loss);
        }
        let n = buf.len().min(self.adc.len());
        for s in buf.iter_mut().take(n) {
            *s = self.adc.pop_front().unwrap();
        }
        // The audio this pass took, in time.
        self.now += (n as u64 * 1000).div_ceil(u64::from(ADC_RATE));
        Ok(n)
    }

    fn hold_outputs_low(&mut self) {
        self.held_low += 1;
    }

    fn set_led(&mut self, on: bool) {
        self.led = on;
        self.led_on_passes += on as u32;
    }

    fn feed_watchdog(&mut self) {
        self.feeds += 1;
    }
}

/// The box's ADC readings of `text` keyed at `wpm` on `pitch_hz`, with band noise
/// `snr_db` below it (in the decoder's 150 Hz), 300 ms of noise before and
/// `tail_ms` after.
fn adc_cw(text: &str, wpm: f32, pitch_hz: f32, snr_db: f32, tail_ms: f32, seed: u64) -> Vec<u16> {
    let mut k = Keyer::new(ADC_RATE, pitch_hz, wpm);
    k.amplitude = 0.3;
    k.seed = seed;
    let mut x = k.render(text, 300.0);
    x.extend(std::iter::repeat_n(
        0.0,
        (tail_ms / 1000.0 * ADC_RATE as f32) as usize,
    ));
    let sigma = Noise::sigma_for_snr(k.amplitude, snr_db, ADC_RATE, 150.0);
    Noise::new(seed).add(&mut x, sigma);
    to_adc(&x)
}

/// Samples (full scale 1.0) as the ADC reads them: 12 bits around mid-scale.
fn to_adc(x: &[f32]) -> Vec<u16> {
    x.iter()
        .map(|v| (2048.0 + v * 2048.0).round().clamp(0.0, 4095.0) as u16)
        .collect()
}

/// `ms` of band noise alone at the level `adc_cw` uses for `snr_db`.
fn adc_noise(ms: f32, snr_db: f32, seed: u64) -> Vec<u16> {
    let mut x = vec![0.0; (ms / 1000.0 * ADC_RATE as f32) as usize];
    let sigma = Noise::sigma_for_snr(0.3, snr_db, ADC_RATE, 150.0);
    Noise::new(seed).add(&mut x, sigma);
    to_adc(&x)
}

/// Pass until the board's ADC samples are all taken.
fn run(c: &mut Control, b: &mut Board) {
    while !b.adc.is_empty() {
        c.pass(b);
    }
    c.pass(b);
}

fn start(b: &Board) -> Control {
    Control::new(Boot::Power, "abcd1234", b)
}

/// The text the `CH` lines carry, in order, with `_` for word gaps.
fn ch_text(lines: &[(u8, String)]) -> String {
    lines
        .iter()
        .filter_map(|(_, body)| body.strip_prefix("CH "))
        .map(|rest| rest.split_once(' ').unwrap().1.to_string())
        .collect()
}

/// The sample counts of the lines starting `kind`.
fn stamps(lines: &[(u8, String)], kind: &str) -> Vec<u64> {
    lines
        .iter()
        .filter_map(|(_, body)| body.strip_prefix(kind))
        .map(|rest| rest.split(' ').next().unwrap().parse().unwrap())
        .collect()
}

/// The reply with `id`.
fn reply(lines: &[(u8, String)], id: u8) -> String {
    let r: Vec<&String> = lines
        .iter()
        .filter(|(i, _)| *i == id)
        .map(|(_, b)| b)
        .collect();
    assert_eq!(r.len(), 1, "replies to {id}: {r:?}");
    r[0].clone()
}

#[test]
fn text_mode_types_what_it_hears() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.adc
        .extend(adc_cw("CQ CQ DE K8ONS K", 20.0, 600.0, 20.0, 3000.0, 1));
    run(&mut c, &mut b);
    assert!(!c.is_hfnode());
    assert_eq!(b.text(), "CQ CQ DE K8ONS K \r\n");
}

#[test]
fn text_mode_starts_a_new_line_for_each_transmission() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.adc
        .extend(adc_cw("CQ DE K8ONS", 18.0, 600.0, 20.0, 3000.0, 2));
    b.adc.extend(adc_cw("TEST", 25.0, 600.0, 20.0, 3000.0, 3));
    run(&mut c, &mut b);
    assert_eq!(b.text(), "CQ DE K8ONS \r\nTEST \r\n");
}

#[test]
fn the_box_decodes_what_the_host_decoder_does() {
    // Different speeds, pitches and noise; the box's path (32 kHz, 12 bits,
    // filtered and decimated) must give the text, as hfnode's decoder does at 8 kHz.
    let cases = [
        ("CALL 42 K8ONS TX MOM", 15.0, 600.0, 18.0),
        ("OK 42 7713 K", 22.0, 600.0, 15.0),
        ("WX 43 1234 KN", 30.0, 700.0, 20.0),
        ("AGN 44 5678 B K", 12.0, 500.0, 20.0),
        ("CQ CQ DE K8ONS K", 25.0, 800.0, 25.0),
    ];
    for (i, &(text, wpm, pitch, snr)) in cases.iter().enumerate() {
        let mut b = Board::new();
        let mut c = start(&b);
        b.send(1, &format!("CONFIG {pitch} 150 {pitch}"));
        b.adc
            .extend(adc_cw(text, wpm, pitch, snr, 3000.0, 10 + i as u64));
        run(&mut c, &mut b);
        let got = b.text();
        let words: Vec<&str> = got
            .lines()
            .filter(|l| !l.starts_with("01 "))
            .flat_map(|l| l.split_whitespace())
            .collect();
        assert_eq!(
            words.join(" "),
            text,
            "{wpm} wpm at {pitch} Hz, {snr} dB: {got:?}"
        );
    }
}

#[test]
fn hfnode_mode_sends_framed_lines_stamped_in_samples() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(0x21, "HELLO");
    c.pass(&mut b);
    assert!(c.is_hfnode());
    let hello = reply(&b.lines(), 0x21);
    assert_eq!(
        hello,
        "OK HELLO 1 32000 8000 80 0 POWER abcd1234 PICO2-DECODER"
    );

    let audio = adc_cw("CQ DE K8ONS K", 20.0, 600.0, 20.0, 1500.0, 4);
    let total = (audio.len() / DECIMATE) as u64;
    b.adc.extend(audio);
    // Keep the link up, as hfnode does with STATUS.
    while !b.adc.is_empty() {
        for _ in 0..100 {
            c.pass(&mut b);
        }
        b.send(0x22, "STATUS");
    }
    c.pass(&mut b);
    assert!(c.is_hfnode());
    let lines = b.lines();
    // Everything the box says on its own has id 00; replies have the command's.
    assert!(lines
        .iter()
        .all(|(id, _)| *id == 0 || *id == 0x21 || *id == 0x22));
    assert_eq!(ch_text(&lines), "CQ_DE_K8ONS_K_");
    // ST every 400 samples, at the end of each block; SL every 400, stamped with
    // the samples before their first measurement.
    let st = stamps(&lines, "ST ");
    assert_eq!(st, (1..=total / 400).map(|k| k * 400).collect::<Vec<_>>());
    let sl = stamps(&lines, "SL ");
    assert_eq!(sl, (0..total / 400).map(|k| k * 400).collect::<Vec<_>>());
    assert_eq!(c.samples(), total);
    // Each CH line comes before the ST of its block, stamped no later.
    let mut last_st = 0;
    for (_, body) in &lines {
        if let Some(rest) = body.strip_prefix("CH ") {
            let n: u64 = rest.split(' ').next().unwrap().parse().unwrap();
            assert!(
                n > last_st && n <= last_st + 400,
                "{body} after ST {last_st}"
            );
        }
        if let Some(rest) = body.strip_prefix("ST ") {
            last_st = rest.split(' ').next().unwrap().parse().unwrap();
        }
    }
    // The decoder's state: quiet at the end, a speed near 20 wpm.
    let last = lines
        .iter()
        .rev()
        .find(|(_, b)| b.starts_with("ST "))
        .unwrap();
    let f: Vec<&str> = last.1.split(' ').collect();
    assert_eq!((f[2], f[3]), ("0", "0"), "{}", last.1);
    assert!(f[4].parse::<u64>().unwrap() >= 1000, "{}", last.1);
    let wpm: f32 = f[5].parse::<f32>().unwrap() / 10.0;
    assert!((wpm - 20.0).abs() < 3.0, "{}", last.1);
}

#[test]
fn the_monitor_measurements_are_cws_on_the_decoded_audio() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    b.send(2, "CONFIG 600 150 700");
    let audio = adc_cw("EEE", 20.0, 700.0, 25.0, 200.0, 5);
    b.adc.extend(audio.iter().copied());
    run(&mut c, &mut b);
    // The same audio through a frontend of its own, measured at 700 Hz.
    let mut f = Frontend::new();
    let x: Vec<f32> = audio.iter().filter_map(|&r| f.push(r)).collect();
    let coeff = level::goertzel_coeff(700.0, RATE);
    let want: Vec<(i32, i32)> = x
        .chunks_exact(SLICE)
        .map(|s| level::measure(s, coeff))
        .map(|l| (tenths(db(l.tone)), tenths(db(l.total))))
        .collect();
    let got: Vec<(i32, i32)> = b
        .lines()
        .iter()
        .filter_map(|(_, body)| body.strip_prefix("SL "))
        .flat_map(|rest| {
            rest.split(' ')
                .skip(1)
                .map(|p| {
                    let (t, a) = p.split_once(',').unwrap();
                    (t.parse().unwrap(), a.parse().unwrap())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(got, want[..got.len()]);
    assert_eq!(got.len(), want.len() / SLICES_PER_LINE * SLICES_PER_LINE);
    // The tone is at the monitor's pitch: some slices nearly all tone, some band
    // noise alone, far below.
    let keyed = got.iter().filter(|(t, a)| a - t < 10).count();
    let quiet = got.iter().filter(|(t, a)| a - t > 100).count();
    assert!(keyed >= 3 && quiet >= 10, "{got:?}");
}

#[test]
fn flush_sends_the_word_in_progress_before_its_reply() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    // No quiet after the word: it is still in progress.
    b.adc.extend(adc_cw("TEST", 20.0, 600.0, 25.0, 0.0, 6));
    run(&mut c, &mut b);
    assert!(c.decoder().has_partial());
    assert_eq!(ch_text(&b.lines()), "");
    b.send(2, "FLUSH");
    c.pass(&mut b);
    let lines = b.lines();
    let at = lines.iter().position(|(id, _)| *id == 2).unwrap();
    assert!(lines[at].1.starts_with("OK FLUSH "), "{:?}", lines[at]);
    // A flush ends the word but hears no word gap (cw's `flush_with`).
    assert_eq!(ch_text(&lines[..at]), "TEST");
    assert!(!c.decoder().has_partial());
}

#[test]
fn hold_stops_decoding_but_not_the_measurements() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    b.send(2, "HOLD");
    // The node's own sending, as its sidetone sounds through the radio.
    b.adc
        .extend(adc_cw("OK 42 1234 K", 20.0, 600.0, 25.0, 1500.0, 7));
    while !b.adc.is_empty() {
        for _ in 0..100 {
            c.pass(&mut b);
        }
        b.send(3, "STATUS");
    }
    let held = b.lines();
    assert!(reply(&held, 2).starts_with("OK HOLD "));
    assert_eq!(ch_text(&held), "");
    assert!(!stamps(&held, "SL ").is_empty());
    let status = held.iter().rev().find(|(id, _)| *id == 3).unwrap();
    assert!(status.1.starts_with("OK STATUS HFNODE 1 "), "{status:?}");
    b.tx.clear();
    b.send(4, "RESUME");
    b.adc
        .extend(adc_cw("CALL 43 K8ONS RX", 20.0, 600.0, 20.0, 1500.0, 8));
    while !b.adc.is_empty() {
        for _ in 0..100 {
            c.pass(&mut b);
        }
        b.send(5, "STATUS");
    }
    let lines = b.lines();
    assert!(reply(&lines, 4).starts_with("OK RESUME "));
    assert_eq!(ch_text(&lines), "CALL_43_K8ONS_RX_");
}

#[test]
fn commands_and_their_errors() {
    let mut b = Board::new();
    let mut c = start(&b);
    // Answered in text mode too, framed.
    b.send(1, "STATUS");
    b.send(2, "CONFIG 700 200 650");
    b.send(3, "STATUS");
    b.send(4, "CONFIG 700 200");
    b.send(5, "CONFIG 700 abc 650");
    b.send(6, "CONFIG 2000 150 600");
    b.send(7, "CONFIG 700 5 600");
    b.send(8, "CONFIG 700 150 100");
    b.send(9, "RESET SPEED");
    b.send(10, "RESET LEVELS");
    b.send(11, "RESET");
    b.send(12, "NEW");
    b.send(13, "KEY DOWN");
    b.send(14, "HELLO THERE");
    b.send(15, "TEXT");
    c.pass(&mut b);
    assert!(!c.is_hfnode());
    let l = b.lines();
    assert_eq!(reply(&l, 1), "OK STATUS TEXT 0 600 150 600 0 0 0 0");
    assert_eq!(reply(&l, 2), "OK CONFIG 0");
    assert_eq!(reply(&l, 3), "OK STATUS TEXT 0 700 200 650 0 0 0 0");
    assert_eq!(reply(&l, 4), "ERR CONFIG ARGS");
    assert_eq!(reply(&l, 5), "ERR CONFIG ARGS");
    assert_eq!(reply(&l, 6), "ERR CONFIG RANGE");
    assert_eq!(reply(&l, 7), "ERR CONFIG RANGE");
    assert_eq!(reply(&l, 8), "ERR CONFIG RANGE");
    assert_eq!(reply(&l, 9), "OK RESET SPEED 0");
    assert_eq!(reply(&l, 10), "OK RESET LEVELS 0");
    assert_eq!(reply(&l, 11), "ERR RESET ARGS");
    assert_eq!(reply(&l, 12), "OK NEW 0");
    assert_eq!(reply(&l, 13), "ERR KEY UNKNOWN");
    assert_eq!(reply(&l, 14), "ERR HELLO ARGS");
    assert_eq!(reply(&l, 15), "OK TEXT");
}

#[test]
fn damaged_lines_are_ignored() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.rx.extend(b"21 HELLO*00\n");
    b.rx.extend(b"21 HELLO\n");
    b.rx.extend([b'X'; 200]);
    b.rx.push_back(b'\n');
    c.pass(&mut b);
    assert!(b.tx.is_empty());
    assert!(!c.is_hfnode());
}

#[test]
fn back_to_text_mode_when_hfnode_goes_quiet() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    c.pass(&mut b);
    assert!(c.is_hfnode());
    // A valid line every second keeps hfnode mode.
    for _ in 0..5 {
        b.adc.extend(adc_noise(1000.0, 20.0, 9));
        run(&mut c, &mut b);
        assert!(c.is_hfnode(), "at {} ms", b.now);
        b.send(2, "STATUS");
    }
    // A damaged line does not count.
    b.rx.extend(b"03 STATUS*00\n");
    b.adc.extend(adc_noise(1990.0, 20.0, 9));
    run(&mut c, &mut b);
    assert!(c.is_hfnode(), "at {} ms", b.now);
    b.adc.extend(adc_noise(20.0, 20.0, 9));
    run(&mut c, &mut b);
    assert!(!c.is_hfnode(), "at {} ms", b.now);
    // And then the box types what it hears again.
    b.tx.clear();
    b.adc.extend(adc_cw("QRL", 20.0, 600.0, 20.0, 2500.0, 9));
    run(&mut c, &mut b);
    assert_eq!(b.text(), "QRL \r\n");
}

#[test]
fn back_to_text_mode_when_usb_goes_away() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    c.pass(&mut b);
    assert!(c.is_hfnode());
    // Half a line, then the cable comes out and goes back in.
    b.rx.extend(b"02 STA");
    c.pass(&mut b);
    b.epoch += 1;
    b.rx.extend(b"TUS*00\n");
    c.pass(&mut b);
    assert!(!c.is_hfnode());
    let l = b.lines();
    assert_eq!(l.len(), 1, "{l:?}");
}

#[test]
fn lost_audio_is_reported_and_a_halted_dma_restarts_the_box() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    b.adc.extend(adc_noise(100.0, 20.0, 9));
    run(&mut c, &mut b);
    let n = c.samples();
    b.loss = Some(AudioLoss::Overrun);
    c.pass(&mut b);
    b.loss = Some(AudioLoss::Fifo);
    c.pass(&mut b);
    b.adc.extend(adc_noise(100.0, 20.0, 9));
    run(&mut c, &mut b);
    b.send(2, "STATUS");
    c.pass(&mut b);
    let l = b.lines();
    let gaps: Vec<&String> = l
        .iter()
        .map(|(_, b)| b)
        .filter(|b| b.starts_with("GAP"))
        .collect();
    assert_eq!(
        gaps,
        [&format!("GAP {n} OVERRUN"), &format!("GAP {n} FIFO")]
    );
    assert!(reply(&l, 2).ends_with(" 0 2 0"), "{}", reply(&l, 2));
    let feeds = b.feeds;
    assert_eq!(feeds, b.held_low);

    b.loss = Some(AudioLoss::Halted);
    c.pass(&mut b);
    let reads = b.adc_reads;
    b.adc.extend(adc_noise(100.0, 20.0, 9));
    for _ in 0..50 {
        c.pass(&mut b);
    }
    // No more feeding, so the watchdog restarts the box; no more reading a
    // channel that has stopped. The outputs are still held low every pass.
    assert_eq!(b.feeds, feeds);
    assert_eq!(b.adc_reads, reads);
    assert_eq!(b.held_low, feeds + 51);
    let halted = b
        .lines()
        .iter()
        .filter(|(_, body)| body.starts_with("GAP") && body.ends_with("HALTED"))
        .count();
    assert_eq!(halted, 1);
}

#[test]
fn outputs_low_and_watchdog_fed_every_pass_led_on_a_tone() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.adc.extend(adc_cw("T T T", 10.0, 600.0, 25.0, 500.0, 10));
    let mut passes = 0;
    while !b.adc.is_empty() {
        c.pass(&mut b);
        passes += 1;
        assert_eq!(b.led, c.decoder().key_down());
    }
    assert_eq!(b.held_low, passes);
    assert_eq!(b.feeds, passes);
    // Three dashes at 10 wpm, 360 ms each, in passes of 4 ms of audio.
    assert!(b.led_on_passes > 3 * 60, "{}", b.led_on_passes);
}

#[test]
fn a_full_queue_drops_whole_lines() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    b.room = 0;
    b.adc
        .extend(adc_cw("CQ CQ CQ DE K8ONS K", 20.0, 600.0, 20.0, 1000.0, 11));
    while !b.adc.is_empty() {
        for _ in 0..100 {
            c.pass(&mut b);
        }
        b.send(2, "STATUS");
    }
    assert!(b.tx.is_empty());
    assert!(c.lost_lines() > 0);
    // USB takes bytes again, a few at a time: every line that arrives is whole.
    b.room = 7;
    for _ in 0..2000 {
        c.pass(&mut b);
    }
    let l = b.lines();
    assert!(l.len() > 40, "{}", l.len());
    assert!(b.tx.ends_with(b"\n"));
}

#[test]
fn a_new_decoder_forgets_the_word_in_progress() {
    let mut b = Board::new();
    let mut c = start(&b);
    b.send(1, "HELLO");
    b.adc.extend(adc_cw("NOISE", 20.0, 600.0, 25.0, 0.0, 12));
    run(&mut c, &mut b);
    assert!(c.decoder().has_partial());
    b.send(2, "NEW");
    b.send(3, "FLUSH");
    c.pass(&mut b);
    let l = b.lines();
    assert_eq!(ch_text(&l), "");
    assert!(reply(&l, 3).starts_with("OK FLUSH "));
}
