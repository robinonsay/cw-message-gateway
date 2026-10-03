//! Decoder accuracy against SNR, for tuning. `cargo run --release -p cw --example sweep`
use cw::{events_to_text, Decoder, DecoderConfig, Keyer, Noise};

const SR: u32 = 8000;
const MSG: &str = "W5XXX 42 KRTPQMLD TX MOM RUNNING LATE HOME SUN K";

fn lev(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn main() {
    let bws: Vec<f32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let bws = if bws.is_empty() { vec![80.0, 150.0, 250.0] } else { bws };
    for bw in bws {
        print!("bw {bw:>5} Hz:");
        for snr in [3.0, 0.0, -3.0, -6.0, -9.0, -12.0] {
            let mut total = 0;
            for seed in 0..5u64 {
                let mut k = Keyer::new(SR, 600.0 + seed as f32 * 7.0, 16.0);
                k.jitter = 0.08;
                k.seed = seed;
                let mut audio = k.render(MSG, 800.0);
                Noise::new(seed + 100).add(&mut audio, Noise::sigma_for_snr(k.amplitude, snr, SR, 2500.0));
                let mut cfg = DecoderConfig::new(SR, 600.0);
                cfg.bandwidth_hz = bw;
                let mut d = Decoder::new(cfg);
                let mut ev = d.push(&audio);
                ev.extend(d.flush());
                total += lev(&events_to_text(&ev), MSG);
            }
            print!("  {snr:>4} dB: {:>5.1}%", 100.0 * total as f32 / (5 * MSG.len()) as f32);
        }
        println!();
    }
}
