use cw::{events_to_text, Decoder, DecoderConfig};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&a[1]).unwrap();
    let s: Vec<f32> = bytes
        .chunks(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let start: f32 = a.get(2).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let s = &s[(start * 8000.0) as usize..];
    let mut d = Decoder::new(DecoderConfig::new(8000, 600.0));
    let mut ev = Vec::new();
    for (i, c) in s.chunks(400).enumerate() {
        ev.extend(d.push(c));
        if i < 80 && i % 4 == 0 {
            eprintln!(
                "{:.2} {:?} wpm {:.1}",
                start + i as f32 * 0.05,
                d.levels(),
                d.wpm()
            );
        }
    }
    ev.extend(d.flush());
    println!("{}", events_to_text(&ev));
}
