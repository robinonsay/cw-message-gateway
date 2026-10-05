use super::*;
use keyer_core::keyer::Boot;

/// The example config with the keyer box switched on as its comments describe.
fn example_keyer() -> Config {
    let text = include_str!("../../../../hfnode.example.toml")
        .replace("# rig = \"keyer\"", "rig = \"keyer\"")
        .replace("# [keyer]\n", "[keyer]\n")
        .replace("# commissioned = \"none\"", "commissioned = \"none\"")
        .replace("# sidetone_hz = 600", "sidetone_hz = 600")
        .replace("# min_level_dbfs = -65", "min_level_dbfs = -65")
        .replace("max_key_seconds = 45", "max_key_seconds = 46");
    let cfg: Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.station.rig, RigKind::Keyer);
    cfg
}

#[test]
fn the_example_config_works_with_the_keyer_box() {
    let cfg = example_keyer();
    cfg.validate().unwrap();
    let k = cfg.keyer.as_ref().unwrap();
    assert_eq!(k.commissioned, Stage::None);
    assert_eq!(k.sidetone_hz, Some(600.0));
    // The figures the example's comment gives for max_key_seconds.
    let at = |wpm| {
        (longest_piece(wpm).unwrap() + KEY_SECONDS_SPARE)
            .as_secs_f32()
            .ceil()
    };
    assert_eq!(at(18), 46.0);
    assert_eq!(at(20), 42.0);
    let mut short = cfg.clone();
    short.station.max_key_seconds = 45;
    let e = short.validate().unwrap_err().to_string();
    assert!(e.contains("at least 46 at 18 wpm"), "{e}");
}

#[test]
fn keyer_settings_are_checked() {
    let bad = |edit: fn(&mut Config), says: &str| {
        let mut cfg = example_keyer();
        edit(&mut cfg);
        let e = cfg.validate().unwrap_err().to_string();
        assert!(e.contains(says), "{says}: {e}");
    };
    bad(|c| c.keyer = None, "no [keyer] section");
    bad(
        |c| c.station.frequency_hz = 5_357_000,
        "outside the US amateur bands",
    );
    bad(
        |c| c.station.frequency_hz = 7_300_001,
        "outside the US amateur bands",
    );
    bad(|c| c.station.serial_port = " ".into(), "serial_port");
    bad(|c| c.station.max_key_seconds = 61, "60 or less");
    bad(
        |c| c.station.key_speed_wpm = 13,
        "too slow for the keyer box",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().sidetone_hz = Some(250.0),
        "keyer.sidetone_hz",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().sidetone_hz = Some(1300.0),
        "keyer.sidetone_hz",
    );
    bad(
        |c| c.keyer.as_mut().unwrap().min_level_dbfs = -10.0,
        "keyer.min_level_dbfs",
    );
    // Outside the IC-7300's coverage but on 2 m: fine for any radio.
    let mut cfg = example_keyer();
    cfg.station.frequency_hz = 144_050_000;
    cfg.validate().unwrap();
    // The audio pitch is the sidetone's when sidetone_hz is left out.
    cfg.keyer.as_mut().unwrap().sidetone_hz = None;
    cfg.audio.pitch_hz = 1250.0;
    assert!(cfg.validate().is_err());
}

#[test]
fn each_keying_command_needs_its_bring_up_stage() {
    use Action::*;
    let allowed = |stage| {
        [Key, Test, Run]
            .into_iter()
            .filter(|&a| check_stage(stage, a).is_ok())
            .collect::<Vec<_>>()
    };
    assert_eq!(allowed(Stage::None), []);
    assert_eq!(allowed(Stage::Listen), [Key]);
    assert_eq!(allowed(Stage::Keying), [Key, Test]);
    assert_eq!(allowed(Stage::Done), [Key, Test, Run]);
    let e = check_stage(Stage::Keying, Run).unwrap_err().to_string();
    assert!(e.contains("`run` needs bring-up stage `done`"), "{e}");
}

#[test]
fn a_box_with_looser_limits_than_the_node_is_refused() {
    let ok = Hello {
        version: keyer_core::VERSION,
        run_limit: MAX_RUN_LIMIT,
        link_timeout: MAX_LINK_TIMEOUT,
        key_down_limit: MAX_KEY_DOWN_LIMIT,
        uptime: Duration::from_secs(5),
        boot: Boot::Power,
        name: keyer_core::NAME.into(),
    };
    check_hello(&ok).unwrap();
    let bad = |edit: fn(&mut Hello)| {
        let mut h = ok.clone();
        edit(&mut h);
        check_hello(&h).is_err()
    };
    assert!(bad(|h| h.version += 1));
    assert!(bad(|h| h.run_limit += Duration::from_millis(1)));
    assert!(bad(|h| h.run_limit = Duration::ZERO));
    assert!(bad(|h| h.key_down_limit += Duration::from_millis(1)));
    assert!(bad(|h| h.key_down_limit = Duration::ZERO));
    assert!(bad(|h| h.link_timeout += Duration::from_millis(1)));
    assert!(bad(|h| h.link_timeout = Duration::from_millis(999)));
}

#[test]
fn the_box_is_known_by_its_usb_product_name() {
    assert!(is_keyer_box("PICO2-KEYER"));
    assert!(!is_keyer_box("CP2102 USB to UART Bridge Controller"));
    assert!(!is_keyer_box("Pico"));
}
