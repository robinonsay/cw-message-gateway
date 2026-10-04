//! Whole-node test without hardware: synthesized field CW goes in as audio, through
//! the decoder, parser and session, and comes out as keyer text on a simulated radio.

use auth::CodeBook;
use civ::sim::SimRig;
use civ::Rig;
use cw::{Keyer, Noise};
use hfnode::audio::{self, Block};
use hfnode::config::Config;
use hfnode::inbox::Message;
use hfnode::node;
use hfnode::session::{SendError, Services, WxError};
use hfnode::station::{Station, StationConfig};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Fake {
    sent: Vec<(String, String)>,
}

impl Services for Fake {
    fn send_message(&mut self, dest: &str, _from_call: &str, text: &str) -> Result<(), SendError> {
        self.sent.push((dest.into(), text.into()));
        Ok(())
    }
    fn ready_messages(&mut self) -> Vec<Message> {
        Vec::new()
    }
    fn mark_read(&mut self, _: &[u64]) {}
    fn weather(&mut self, _: &str) -> Result<String, WxError> {
        Ok("SUNNY".into())
    }
}

#[test]
fn field_message_over_the_air_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("node.key");
    std::fs::write(&key, b"end-to-end test key 0123456789").unwrap();
    let _ = env_logger::builder().is_test(true).try_init();
    // The paths are set after parsing, so that nothing in them needs escaping.
    let mut cfg: Config = toml::from_str(
        r#"
        state_dir = ""
        [station]
        node_call = "N0DE"
        field_calls = ["W5XXX"]
        frequency_hz = 7030000
        serial_port = "/dev/null"
        chunk_pause_ms = 10
        [audio]
        end_of_message_ms = 2500
        [auth]
        key_file = ""
        [schedule]
        always = true
        [[contacts]]
        name = "MOM"
        address = "mom@example.com"
        "#,
    )
    .unwrap();
    cfg.state_dir = dir.path().join("state");
    cfg.auth.key_file = key.clone();
    cfg.validate().unwrap();

    let book = CodeBook::new(&std::fs::read(&key).unwrap());
    let sr = cfg.audio.sample_rate;
    let mut audio = Vec::new();
    let mut k = Keyer::new(sr, 610.0, 16.0);
    k.jitter = 0.06;
    // The field operator waits while the node sends its read-back before
    // committing, as they would on the air.
    let read_back_ms = cw::duration_ms("R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K", 18);
    for (text, wait_ms) in [
        (
            format!("W5XXX 42 {} TX MOM RUNNING LATE HOME SUN K", book.code(42)),
            read_back_ms + 5000,
        ),
        (format!("OK 43 {} K", book.code(43)), 8000),
    ] {
        audio.extend(k.render(&text, 3000.0));
        audio.extend(vec![0.0; sr as usize * wait_ms as usize / 1000]);
    }
    Noise::new(11).add(
        &mut audio,
        Noise::sigma_for_snr(k.amplitude, 3.0, sr, 2500.0),
    );

    let mut rig = SimRig::new();
    rig.time_scale = 100.0;
    let mut sc = StationConfig::from_config(&cfg.station);
    sc.poll = Duration::from_millis(2);
    sc.swr_delay = Duration::from_millis(2);
    let mut station = Station::new(rig, sc, None);
    station.configure().unwrap();

    // Feed 50 ms blocks as fast as the node takes them. Never full: every block
    // is delivered, as in real time.
    let (tx, rx) = audio::queue(usize::MAX);
    let blocks: Vec<Vec<f32>> = audio
        .chunks(sr as usize / 20)
        .map(<[f32]>::to_vec)
        .collect();
    let radio = station.rig();
    std::thread::spawn(move || {
        for b in blocks {
            // Paced by the node, not the wall clock: the next block goes out only
            // once the node has taken the last, and the operator's audio stands
            // still while the radio tunes or transmits, as the silence after a
            // read-back would on the air. A busy test machine then changes how
            // long the test takes, not which audio the node hears.
            while tx.queued() > 0 || {
                let mut r = radio.lock().unwrap();
                r.is_transmitting().unwrap_or(true) || r.tuner_busy().unwrap_or(true)
            } {
                std::thread::sleep(Duration::from_micros(200));
            }
            let block = Block {
                at: Instant::now(),
                samples: b,
            };
            if tx.send(block).is_err() {
                break;
            }
        }
    });

    let mut session = node::build_session(&cfg).unwrap();
    let mut svc = Fake::default();

    let end = node::run(&cfg, &mut station, &rx, &mut session, &mut svc).unwrap_err();
    assert!(end.to_string().contains("audio source ended"));

    assert_eq!(
        svc.sent,
        [("MOM".to_string(), "RUNNING LATE HOME SUN".to_string())]
    );
    let keyed = station.rig().lock().unwrap().sent.join(" ");
    assert_eq!(
        keyed,
        "R 42 TX MOM RUNNING LATE HOME SUN ? DE N0DE K SENT 43 DE N0DE K"
    );
    assert_eq!(session.last_seq(), 43);
}
