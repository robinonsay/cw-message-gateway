//! `deploy/hfnode.service`, the systemd unit: what it lets the node reach and what
//! it runs after a stop. systemd itself is not run here; these read the file.

const UNIT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/hfnode.service"
));

fn lines(key: &str) -> Vec<&'static str> {
    UNIT.lines()
        .filter_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .collect()
}

#[test]
fn the_unit_lets_the_node_open_either_serial_device_and_the_sound_card() {
    // DevicePolicy=closed hides every device not allowed here. The IC-7300's CP210x
    // is a ttyUSB device; the keyer box, a Pico 2 on USB CDC, is a ttyACM one (the
    // safety audit's KB-12).
    assert_eq!(lines("DevicePolicy"), ["closed"]);
    let allowed = lines("DeviceAllow");
    for dev in ["char-ttyUSB rw", "char-ttyACM rw", "char-alsa rw"] {
        assert!(allowed.contains(&dev), "no DeviceAllow={dev}: {allowed:?}");
    }
    assert!(lines("PrivateDevices").is_empty());
}

#[test]
fn every_stop_ends_with_a_receive_check() {
    // `hfnode radio rx`, which with station.rig = "keyer" is the box's rx; a
    // failure there must not mark the stop as failed (the leading "-").
    let post = lines("ExecStopPost");
    assert_eq!(post.len(), 1, "{post:?}");
    let words: Vec<&str> = post[0].split_whitespace().collect();
    assert!(
        words[0].starts_with('-') && words[0].ends_with("/hfnode"),
        "{post:?}"
    );
    assert_eq!(words[1], "radio");
    assert_eq!(words.last(), Some(&"rx"));
}
