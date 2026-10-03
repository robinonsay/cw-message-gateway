//! Whole-node test without hardware: synthesized field CW goes in as audio, through
//! the decoder, parser and session, and comes out as keyer text on a simulated radio.

use auth::CodeBook;
use civ::sim::SimRig;
use cw::{Keyer, Noise};
use hfnode::config::Config;
use hfnode::inbox::Message;
use hfnode::node;
use hfnode::session::Services;
use hfnode::station::{Station, StationConfig};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Default)]
struct Fake {
    sent: Vec<(String, String)>,
}

impl Services for Fake {
    fn send_message(&mut self, dest: &str, text: &str) -> Result<(), String> {
        self.sent.push((dest.into(), text.into()));
        Ok(())
    }
    fn ready_messages(&mut self) -> Vec<Message> {
        Vec::new()
    }
    fn mark_read(&mut self, _: &[u64]) {}
    fn weather(&mut self, _: Option<&str>) -> Result<String, String> {
        Ok("SUNNY".into())
    }
}

#[test]
fn field_message_over_the_air_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("node.key");
    std::fs::write(&key, b"end-to-end test key 0123456789").unwrap();
    let _ = env_logger::builder().is_test(true).try_init();
    let cfg: Config = toml::from_str(&format!(
        r#"
        state_dir = "{state}"
        [station]
        node_call = "N0DE"
        field_calls = ["W5XXX"]
        frequency_hz = 7030000
        serial_port = "/dev/null"
        chunk_pause_ms = 10
        [audio]
        end_of_message_ms = 2500
        [auth]
        key_file = "{key}"
        [schedule]
        always = true
        [[contacts]]
        name = "MOM"
        address = "mom@example.com"
        "#,
        state = dir.path().join("state").display(),
        key = key.display()
    ))
    .unwrap();
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

    // Feed 50 ms blocks 100x faster than real time, matching the simulated radio's
    // time scale, so audio heard while "transmitting" is discarded as it would be.
    let (tx, rx) = mpsc::channel();
    let blocks: Vec<Vec<f32>> = audio
        .chunks(sr as usize / 20)
        .map(<[f32]>::to_vec)
        .collect();
    std::thread::spawn(move || {
        for b in blocks {
            std::thread::sleep(Duration::from_micros(500));
            if tx.send(b).is_err() {
                break;
            }
        }
    });

    let mut rig = SimRig::new();
    rig.time_scale = 100.0;
    let mut sc = StationConfig::from_config(&cfg.station);
    sc.poll = Duration::from_millis(2);
    sc.swr_delay = Duration::from_millis(2);
    let mut station = Station::new(rig, sc, None);
    station.configure().unwrap();
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
