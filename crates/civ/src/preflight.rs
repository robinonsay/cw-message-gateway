//! Read-only checks of the radio before the node writes anything to it, and a
//! read-back of the settings after it has.
//!
//! [`preflight`] only reads: every command it sends is a "Send/read" item sent
//! without data, which reads it (Section 19 of the *IC-7300 Full Manual*, see
//! [`crate::ic7300`] for the citations), plus `19 00` and `1C 00`. It fails if the
//! radio is not an IC-7300 answering at the configured address, is transmitting,
//! could be keyed by the USB serial control lines, or would transmit somewhere other
//! than the dial frequency (split or ∂TX on). With `unattended`, it also requires the
//! radio's own Time-Out Timer (CI-V) to be set, as a backstop that does not depend
//! on this software.
//!
//! [`verify_setup`] reads back what [`crate::Rig`]'s set commands are meant to have
//! done, since an OK (FB) only says the radio accepted a command, not that it did
//! what the node expects.
//!
//! The 1A 05 item numbers are those of the manual revision the driver cites
//! (`IC-7300_ENG_FM_12b`). Firmware that numbers them differently would read other
//! items, so the first time on a radio every value here is compared with the radio's
//! own menu screens (docs/hardware-test-plan.md, step 1).

use crate::ic7300::{
    break_in_delay_level, key_speed_level, key_speed_wpm, power_level, Ic7300, Port, UsbLine,
    IC7300_ID,
};
use crate::{Rig, RigError};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Reported for the record; never stops anything.
    Info,
    Pass,
    /// Not what the setup guide asks for, but safe.
    Warn,
    /// The node must not go on.
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Info => "info",
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    /// The CI-V command read, as sent after the addresses.
    pub command: &'static str,
    pub value: String,
    pub level: Level,
    pub note: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// No check failed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.level != Level::Fail)
    }

    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.level == Level::Fail)
    }

    fn add(
        &mut self,
        name: &'static str,
        command: &'static str,
        value: impl Into<String>,
        level: Level,
        note: impl Into<String>,
    ) {
        self.checks.push(Check {
            name,
            command,
            value: value.into(),
            level,
            note: note.into(),
        });
    }

    /// A read that is required to pass: an error is a failure.
    fn required<T>(
        &mut self,
        name: &'static str,
        command: &'static str,
        read: crate::Result<T>,
        judge: impl FnOnce(&T) -> (String, Level, String),
    ) {
        match read {
            Ok(v) => {
                let (value, level, note) = judge(&v);
                self.add(name, command, value, level, note);
            }
            Err(e) => self.add(name, command, "-", Level::Fail, unread(&e)),
        }
    }

    /// A read reported for the record: an error is only a warning.
    fn info<T>(
        &mut self,
        name: &'static str,
        command: &'static str,
        read: crate::Result<T>,
        show: impl FnOnce(&T) -> String,
    ) {
        match read {
            Ok(v) => self.add(name, command, show(&v), Level::Info, ""),
            Err(e) => self.add(name, command, "-", Level::Warn, unread(&e)),
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in &self.checks {
            write!(
                f,
                "{:<4}  {:<28} {:<14} {}",
                c.level, c.name, c.command, c.value
            )?;
            if !c.note.is_empty() {
                write!(f, "  ({})", c.note)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

fn unread(e: &RigError) -> String {
    match e {
        RigError::Rejected => {
            "radio answered NG: its firmware may not have this item; check it on the radio".into()
        }
        e => format!("not read: {e}"),
    }
}

fn line_name(l: UsbLine) -> &'static str {
    match l {
        UsbLine::Off => "OFF",
        UsbLine::Dtr => "DTR",
        UsbLine::Rts => "RTS",
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "ON"
    } else {
        "OFF"
    }
}

/// Mode codes of 01, 04 and 06 (p. 19-9).
pub fn mode_name(mode: u8) -> &'static str {
    match mode {
        0x00 => "LSB",
        0x01 => "USB",
        0x02 => "AM",
        0x03 => "CW",
        0x04 => "RTTY",
        0x05 => "FM",
        0x07 => "CW-R",
        0x08 => "RTTY-R",
        _ => "?",
    }
}

/// Time-Out Timer (CI-V) codes: "00=OFF, 01=3 min., 02=5 min., 03=10min., 04=20
/// min., 05=30 min." (1A 05 00 29, p. 19-4).
fn tot_name(v: u8) -> &'static str {
    match v {
        0 => "OFF",
        1 => "3 min",
        2 => "5 min",
        3 => "10 min",
        4 => "20 min",
        _ => "30 min",
    }
}

/// Read-only checks before the node writes to the radio. Stops after the first
/// check if nothing answers at the configured address, since every other read would
/// only time out too.
pub fn preflight<P: Port>(r: &mut Ic7300<P>, unattended: bool) -> Report {
    let mut rep = Report::default();

    rep.required("transceiver ID", "19 00", r.transceiver_id(), |&id| {
        if id == IC7300_ID {
            (format!("{id:02X}h"), Level::Pass, "IC-7300".into())
        } else {
            (
                format!("{id:02X}h"),
                Level::Fail,
                format!("expected {IC7300_ID:02X}h, the IC-7300's ID; is this an IC-7300?"),
            )
        }
    });
    if !rep.passed() {
        return rep;
    }

    rep.required("on receive", "1C 00", r.is_transmitting(), |&tx| {
        if tx {
            (
                "TX".into(),
                Level::Fail,
                "the radio is transmitting; find out why before anything else".into(),
            )
        } else {
            ("RX".into(), Level::Pass, String::new())
        }
    });

    // A raised DTR or RTS line keys the radio with these set; the driver drops both
    // lines, but nothing else on the computer is bound to.
    match r.usb_lines() {
        Ok(lines) => {
            for (name, command, line) in [
                ("USB SEND", "1A 05 00 78", lines.send),
                ("USB Keying (CW)", "1A 05 00 79", lines.keying_cw),
                ("USB Keying (RTTY)", "1A 05 00 80", lines.keying_rtty),
            ] {
                if line == UsbLine::Off {
                    rep.add(name, command, "OFF", Level::Pass, "");
                } else {
                    rep.add(
                        name,
                        command,
                        line_name(line),
                        Level::Fail,
                        "a serial control line can key the radio; set it OFF on the radio",
                    );
                }
            }
        }
        Err(e) => rep.add(
            "USB SEND / USB Keying",
            "1A 05 00 78-80",
            "-",
            Level::Fail,
            unread(&e),
        ),
    }

    rep.required("split", "0F", r.split(), |&on| {
        if on {
            (
                "ON".into(),
                Level::Fail,
                "would transmit on the other VFO; turn SPLIT off".into(),
            )
        } else {
            ("OFF".into(), Level::Pass, String::new())
        }
    });

    rep.required("∂TX", "21 02", r.delta_tx(), |&on| {
        if on {
            (
                "ON".into(),
                Level::Fail,
                "would transmit off the dial frequency; turn ∂TX off".into(),
            )
        } else {
            ("OFF".into(), Level::Pass, String::new())
        }
    });

    rep.required(
        "Time-Out Timer (CI-V)",
        "1A 05 00 29",
        r.time_out_timer(),
        |&v| {
            let level = match (v, unattended) {
                (0, true) => Level::Fail,
                (0, false) => Level::Warn,
                _ => Level::Pass,
            };
            let note = if v == 0 {
                "set it to 3 min: the radio's own limit on CI-V transmissions"
            } else {
                ""
            };
            (tot_name(v).into(), level, note.into())
        },
    );

    // Linked, the USB port shares the bus with [REMOTE]: another controller's
    // replies could be taken for the radio's, and the USB baud rate and echo items
    // no longer apply (p. 12-10 and 12-11).
    match r.civ_usb_unlinked() {
        Ok(true) => rep.add(
            "CI-V USB port",
            "1A 05 00 74",
            "Unlink from [REMOTE]",
            Level::Pass,
            "",
        ),
        Ok(false) => rep.add(
            "CI-V USB port",
            "1A 05 00 74",
            "Link to [REMOTE]",
            Level::Warn,
            "set Unlink from [REMOTE] (the default) unless nothing is on [REMOTE]",
        ),
        Err(e) => rep.add("CI-V USB port", "1A 05 00 74", "-", Level::Warn, unread(&e)),
    }

    match r.usb_inhibit_timer() {
        Ok(true) => rep.add("USB inhibit timer", "1A 05 01 97", "ON", Level::Pass, ""),
        Ok(false) => rep.add(
            "USB inhibit timer",
            "1A 05 01 97",
            "OFF",
            Level::Warn,
            "ICOM's default is ON; it covers the moment the port opens",
        ),
        Err(e) => rep.add(
            "USB inhibit timer",
            "1A 05 01 97",
            "-",
            Level::Warn,
            unread(&e),
        ),
    }

    // The node's keying times assume PARIS timing, dash = 3 dots; a longer dash
    // makes every piece run long and trips the stuck-transmitter check.
    match r.keyer_ratio() {
        Ok(ratio) if (ratio - 3.0).abs() < 0.05 => rep.add(
            "keyer dot/dash ratio",
            "1A 05 01 61",
            "1:1:3.0",
            Level::Pass,
            "",
        ),
        Ok(ratio) => rep.add(
            "keyer dot/dash ratio",
            "1A 05 01 61",
            format!("1:1:{ratio:.1}"),
            Level::Warn,
            "the node times keying at 1:1:3.0; set the ratio to 3.0",
        ),
        Err(e) => rep.add(
            "keyer dot/dash ratio",
            "1A 05 01 61",
            "-",
            Level::Warn,
            unread(&e),
        ),
    }

    // Whether peak hold applies to the meters read over CI-V is not documented; if it
    // does, the Po meter could show output after the key has gone up.
    match r.meter_peak_hold() {
        Ok(false) => rep.add("meter peak hold", "1A 05 00 84", "OFF", Level::Pass, ""),
        Ok(true) => rep.add(
            "meter peak hold",
            "1A 05 00 84",
            "ON",
            Level::Warn,
            "set it OFF so the Po meter reads output only while there is output",
        ),
        Err(e) => rep.add(
            "meter peak hold",
            "1A 05 00 84",
            "-",
            Level::Warn,
            unread(&e),
        ),
    }

    // With the scope and its data output ON (27 10, 27 11; line 9353), waveform
    // frames stream to this port unasked. They are skipped, but the link is never
    // quiet, so every command after a timeout first waits out the drain, the stop
    // commands included.
    match r.scope_data_output() {
        Ok(false) => rep.add("scope data output", "27 11", "OFF", Level::Pass, ""),
        Ok(true) => rep.add(
            "scope data output",
            "27 11",
            "ON",
            Level::Warn,
            "close any panadapter program: its waveform stream delays the stop commands",
        ),
        Err(e) => rep.add("scope data output", "27 11", "-", Level::Warn, unread(&e)),
    }

    rep.info("frequency", "03", r.frequency(), |hz| format!("{hz} Hz"));
    rep.info(
        "transmit frequency",
        "1C 03",
        r.transmit_frequency(),
        |hz| format!("{hz} Hz"),
    );
    rep.info("mode", "04", r.read_mode(), |&(m, f)| {
        format!("{} FIL{f}", mode_name(m))
    });
    rep.info("RF power", "14 0A", r.rf_power_level(), |&l| {
        format!("level {l} ({:.0}%)", l as f32 * 100.0 / 255.0)
    });
    rep.info("break-in", "16 47", r.break_in(), |&b| {
        ["OFF", "semi", "full"][b as usize].into()
    });
    rep.info("tuner", "1C 01", r.tuner_state(), |&t| {
        ["OFF", "ON", "tuning"][t as usize].into()
    });
    rep.info("CI-V Transceive", "1A 05 00 71", r.civ_transceive(), |&b| {
        on_off(b).into()
    });
    rep.info(
        "USB Echo Back (raw)",
        "1A 05 00 75",
        r.usb_echo_back_raw(),
        |&v| format!("{v:02X} (table: 00=ON, 01=OFF)"),
    );
    rep
}

/// What [`crate::Rig`]'s set commands were asked to do.
#[derive(Debug, Clone)]
pub struct Setup {
    pub frequency_hz: u64,
    pub power_watts: u32,
    pub key_speed_wpm: u32,
    pub break_in_delay_dots: f32,
}

/// Read back the radio's state after setting it up, and check it is what was asked
/// for and still safe: on receive, CW, semi break-in, no split or ∂TX, and power,
/// keyer speed and break-in delay at the levels sent. Levels may be stored a step
/// or two off what was sent, so they are compared within a small tolerance.
pub fn verify_setup<P: Port>(r: &mut Ic7300<P>, s: &Setup) -> Report {
    let mut rep = Report::default();
    rep.required("on receive", "1C 00", r.is_transmitting(), |&tx| {
        if tx {
            ("TX".into(), Level::Fail, String::new())
        } else {
            ("RX".into(), Level::Pass, String::new())
        }
    });
    rep.required("frequency", "03", r.frequency(), |&hz| {
        let level = if hz == s.frequency_hz {
            Level::Pass
        } else {
            Level::Fail
        };
        (
            format!("{hz} Hz"),
            level,
            format!("set {} Hz", s.frequency_hz),
        )
    });
    rep.required(
        "transmit frequency",
        "1C 03",
        r.transmit_frequency(),
        |&hz| {
            let level = if hz == s.frequency_hz {
                Level::Pass
            } else {
                Level::Fail
            };
            (
                format!("{hz} Hz"),
                level,
                format!("set {} Hz", s.frequency_hz),
            )
        },
    );
    rep.required("mode", "04", r.read_mode(), |&(m, f)| {
        let level = if m == 0x03 { Level::Pass } else { Level::Fail };
        (format!("{} FIL{f}", mode_name(m)), level, "set CW".into())
    });
    rep.required("split", "0F", r.split(), |&on| {
        let level = if on { Level::Fail } else { Level::Pass };
        (on_off(on).into(), level, String::new())
    });
    rep.required("∂TX", "21 02", r.delta_tx(), |&on| {
        let level = if on { Level::Fail } else { Level::Pass };
        (on_off(on).into(), level, String::new())
    });
    rep.required("break-in", "16 47", r.break_in(), |&b| {
        let level = if b == 0x01 { Level::Pass } else { Level::Fail };
        (
            ["OFF", "semi", "full"][b as usize].into(),
            level,
            "set semi".into(),
        )
    });
    let sent = power_level(s.power_watts);
    rep.required("RF power", "14 0A", r.rf_power_level(), |&l| {
        // Never above what was sent by more than a rounding step.
        let level = if l <= sent + 1 && l + 3 >= sent {
            Level::Pass
        } else {
            Level::Fail
        };
        (
            format!("level {l}"),
            level,
            format!("set level {sent} for {} W", s.power_watts),
        )
    });
    let wpm = s.key_speed_wpm.clamp(6, 48) as f32;
    rep.required("key speed", "14 0C", r.key_speed_level(), |&l| {
        let got = key_speed_wpm(l);
        let level = if (got - wpm).abs() <= 1.0 {
            Level::Pass
        } else {
            Level::Fail
        };
        (
            format!("level {l} ({got:.1} wpm)"),
            level,
            format!(
                "set level {} for {wpm} wpm",
                key_speed_level(s.key_speed_wpm)
            ),
        )
    });
    let dots = s.break_in_delay_dots.clamp(2.0, 13.0);
    rep.required("break-in delay", "14 0F", r.break_in_delay_level(), |&l| {
        let got = 2.0 + l as f32 * 11.0 / 255.0;
        let level = if (got - dots).abs() <= 0.2 {
            Level::Pass
        } else {
            Level::Fail
        };
        (
            format!("level {l} ({got:.1} dots)"),
            level,
            format!("set level {} for {dots} dots", break_in_delay_level(dots)),
        )
    });
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{take_frame, Frame, CONTROLLER};
    use std::collections::HashMap;
    use std::io::{Read, Write};

    /// A radio that answers reads from a table and records every frame written.
    #[derive(Default)]
    struct Table {
        answers: HashMap<Vec<u8>, Vec<u8>>,
        written: Vec<u8>,
        pending: Vec<u8>,
    }

    impl Port for Table {
        fn discard_input(&mut self) -> std::io::Result<()> {
            self.pending.clear();
            Ok(())
        }
    }

    impl Read for Table {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }

    impl Write for Table {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            let mut b = buf.to_vec();
            let f = take_frame(&mut b).expect("one frame per write");
            let body = self
                .answers
                .get(&f.body)
                .cloned()
                .map(|data| [f.body.clone(), data].concat())
                .unwrap_or_else(|| vec![0xFA]);
            self.pending
                .extend(Frame::new(CONTROLLER, 0x94, &body).encode());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A radio at factory defaults with the guide's settings: on receive, USB lines
    /// OFF, Time-Out Timer 3 min.
    fn good() -> Table {
        let mut t = Table::default();
        for (cmd, data) in [
            (&[0x19, 0x00][..], &[0x94][..]),
            (&[0x1C, 0x00], &[0x00]),
            (&[0x1A, 0x05, 0x00, 0x78], &[0x00]),
            (&[0x1A, 0x05, 0x00, 0x79], &[0x00]),
            (&[0x1A, 0x05, 0x00, 0x80], &[0x00]),
            (&[0x0F], &[0x00]),
            (&[0x21, 0x02], &[0x00]),
            (&[0x1A, 0x05, 0x00, 0x29], &[0x01]),
            (&[0x1A, 0x05, 0x01, 0x97], &[0x01]),
            (&[0x1A, 0x05, 0x00, 0x74], &[0x01]),
            (&[0x03], &[0x00, 0x00, 0x03, 0x07, 0x00]),
            (&[0x04], &[0x03, 0x01]),
            (&[0x14, 0x0A], &[0x00, 0x26]),
            (&[0x14, 0x0C], &[0x00, 0x73]),
            (&[0x14, 0x0F], &[0x01, 0x85]),
            (&[0x16, 0x47], &[0x01]),
            (&[0x1C, 0x01], &[0x01]),
            (&[0x1A, 0x05, 0x00, 0x71], &[0x00]),
            (&[0x1A, 0x05, 0x00, 0x75], &[0x01]),
            (&[0x1A, 0x05, 0x01, 0x61], &[0x30]),
            (&[0x1A, 0x05, 0x00, 0x84], &[0x00]),
            (&[0x27, 0x11], &[0x00]),
            (&[0x1C, 0x03], &[0x00, 0x00, 0x03, 0x07, 0x00]),
        ] {
            t.answers.insert(cmd.to_vec(), data.to_vec());
        }
        t
    }

    fn rig(t: Table) -> Ic7300<Table> {
        Ic7300::with_port(t, 0x94)
    }

    fn sent_bodies(r: &Ic7300<Table>) -> Vec<Vec<u8>> {
        let mut w = r.port().written.clone();
        std::iter::from_fn(|| take_frame(&mut w))
            .map(|f| {
                assert_eq!((f.to, f.from), (0x94, CONTROLLER));
                f.body
            })
            .collect()
    }

    fn level_of(rep: &Report, name: &str) -> Level {
        rep.checks.iter().find(|c| c.name == name).unwrap().level
    }

    #[test]
    fn a_well_set_radio_passes_and_nothing_is_written() {
        let mut r = rig(good());
        let rep = preflight(&mut r, true);
        assert!(rep.passed(), "{rep}");
        // Every frame sent is one of the reads in the table, without data: nothing
        // that sets a value, keys or tunes.
        let reads: Vec<Vec<u8>> = good().answers.into_keys().collect();
        let sent = sent_bodies(&r);
        assert_eq!(sent.len(), 21);
        for body in sent {
            assert!(reads.contains(&body), "{body:02X?} is not a read");
        }
    }

    #[test]
    fn usb_lines_split_delta_tx_and_transmit_fail() {
        for (cmd, data, name) in [
            (&[0x1A, 0x05, 0x00, 0x78][..], 0x01, "USB SEND"),
            (&[0x1A, 0x05, 0x00, 0x79], 0x02, "USB Keying (CW)"),
            (&[0x1A, 0x05, 0x00, 0x80], 0x01, "USB Keying (RTTY)"),
            (&[0x0F], 0x01, "split"),
            (&[0x21, 0x02], 0x01, "∂TX"),
            (&[0x1C, 0x00], 0x01, "on receive"),
        ] {
            let mut t = good();
            t.answers.insert(cmd.to_vec(), vec![data]);
            let rep = preflight(&mut rig(t), false);
            assert!(!rep.passed(), "{name}");
            assert_eq!(level_of(&rep, name), Level::Fail, "{name}\n{rep}");
        }
    }

    #[test]
    fn an_unreadable_usb_setting_fails() {
        let mut t = good();
        t.answers.remove(&[0x1A, 0x05, 0x00, 0x79][..]);
        assert!(!preflight(&mut rig(t), false).passed());
    }

    #[test]
    fn time_out_timer_is_required_only_unattended() {
        let mut t = good();
        t.answers.insert(vec![0x1A, 0x05, 0x00, 0x29], vec![0x00]);
        let rep = preflight(&mut rig(t), false);
        assert!(rep.passed());
        assert_eq!(level_of(&rep, "Time-Out Timer (CI-V)"), Level::Warn);
        let mut t = good();
        t.answers.insert(vec![0x1A, 0x05, 0x00, 0x29], vec![0x00]);
        assert!(!preflight(&mut rig(t), true).passed());
    }

    #[test]
    fn another_radio_stops_at_the_first_check() {
        let mut t = good();
        t.answers.insert(vec![0x19, 0x00], vec![0xB6]); // IC-7300MK2
        let mut r = rig(t);
        let rep = preflight(&mut r, false);
        assert!(!rep.passed());
        assert_eq!(rep.checks.len(), 1);
        assert_eq!(sent_bodies(&r), [vec![0x19, 0x00]]);
    }

    #[test]
    fn warning_items_do_not_fail_the_preflight() {
        let mut t = good();
        t.answers.insert(vec![0x1A, 0x05, 0x01, 0x61], vec![0x45]);
        t.answers.insert(vec![0x1A, 0x05, 0x00, 0x84], vec![0x01]);
        t.answers.insert(vec![0x1A, 0x05, 0x00, 0x74], vec![0x00]);
        t.answers.insert(vec![0x27, 0x11], vec![0x01]);
        let rep = preflight(&mut rig(t), true);
        assert!(rep.passed(), "{rep}");
        assert_eq!(level_of(&rep, "keyer dot/dash ratio"), Level::Warn);
        assert_eq!(level_of(&rep, "meter peak hold"), Level::Warn);
        assert_eq!(level_of(&rep, "CI-V USB port"), Level::Warn);
        assert_eq!(level_of(&rep, "scope data output"), Level::Warn);
    }

    #[test]
    fn missing_info_items_only_warn() {
        let mut t = good();
        t.answers.remove(&[0x1A, 0x05, 0x00, 0x71][..]);
        t.answers.remove(&[0x1A, 0x05, 0x01, 0x97][..]);
        let rep = preflight(&mut rig(t), true);
        assert!(rep.passed(), "{rep}");
        assert_eq!(level_of(&rep, "CI-V Transceive"), Level::Warn);
    }

    fn setup() -> Setup {
        Setup {
            frequency_hz: 7_030_000,
            power_watts: 10,
            key_speed_wpm: 18,
            break_in_delay_dots: 10.0,
        }
    }

    #[test]
    fn read_back_matches_the_setup() {
        let rep = verify_setup(&mut rig(good()), &setup());
        assert!(rep.passed(), "{rep}");
    }

    #[test]
    fn read_back_catches_a_wrong_setting() {
        for (cmd, data) in [
            (&[0x03][..], &[0x00, 0x00, 0x04, 0x07, 0x00][..]),
            (&[0x04], &[0x07, 0x01]),       // CW-R
            (&[0x14, 0x0A], &[0x00, 0x51]), // 32%: more power than asked for
            (&[0x14, 0x0C], &[0x01, 0x00]),
            (&[0x14, 0x0F], &[0x00, 0x50]),
            (&[0x16, 0x47], &[0x02]),
            (&[0x0F], &[0x01]),
            (&[0x1C, 0x03], &[0x00, 0x00, 0x03, 0x07, 0x00, 0x00]),
            (&[0x1C, 0x03], &[0x00, 0x10, 0x03, 0x07, 0x00]), // ∂TX +1 kHz
        ] {
            let mut t = good();
            t.answers.insert(cmd.to_vec(), data.to_vec());
            let rep = verify_setup(&mut rig(t), &setup());
            assert!(!rep.passed(), "{cmd:02X?} {data:02X?}\n{rep}");
        }
    }
}
