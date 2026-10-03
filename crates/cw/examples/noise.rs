use cw::{events_to_text, Decoder, DecoderConfig, Noise};
fn main() {
    let sigma: f32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(0.316);
    let mut audio = vec![0.0; 8000 * 5];
    Noise::new(11).add(&mut audio, sigma);
    let mut d = Decoder::new(DecoderConfig::new(8000, 600.0));
    for (i, c) in audio.chunks(400).enumerate() {
        let ev = d.push(c);
        if i % 10 == 0 || !ev.is_empty() { println!("{:.2}s {:?} {:?} key={}", i as f32 * 0.05, d.levels(), ev, d.key_down()); }
    }
    println!("{}", events_to_text(&d.flush()));
}
