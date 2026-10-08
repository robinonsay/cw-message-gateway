//! A serial port's control lines, lowered as soon as it is open.
//!
//! The IC-7300 can be set to transmit (USB SEND) or key CW (USB Keying (CW)) while
//! the computer raises DTR or RTS on its USB serial port: "DTR: Uses the DTR
//! terminal on the CI-V (PC) side" (p. 12-11; manual text lines 6895-6927). Opening
//! a port can raise both lines (see [`crate::ic7300`] for what Linux, macOS and
//! Windows do), so every port the node opens, the radio's, the keyer box's or a
//! handheld's, goes through [`lower_control_lines`] before anything else uses it:
//! DTR first, then RTS, and the port is not used if either fails. Dropping DTR
//! first never passes through the state where only DTR is up (a USB PTT cable may
//! key on that).
//!
//! The lines sit behind [`ControlLines`] so that tests can check the order and the
//! failures without a serial port.

use std::io;

/// The two control lines a serial port drives.
pub trait ControlLines {
    /// Raise (`true`) or lower DTR.
    fn set_dtr(&mut self, level: bool) -> io::Result<()>;
    /// Raise (`true`) or lower RTS.
    fn set_rts(&mut self, level: bool) -> io::Result<()>;
}

impl ControlLines for Box<dyn serialport::SerialPort> {
    fn set_dtr(&mut self, level: bool) -> io::Result<()> {
        self.write_data_terminal_ready(level)
            .map_err(io::Error::other)
    }

    fn set_rts(&mut self, level: bool) -> io::Result<()> {
        self.write_request_to_send(level).map_err(io::Error::other)
    }
}

/// Lower DTR, then RTS, on a port just opened. An error means a line may still be
/// up: the caller must not use the port (dropping it closes it, which lowers both
/// lines on Linux and macOS).
pub fn lower_control_lines<P: ControlLines + ?Sized>(port: &mut P) -> io::Result<()> {
    port.set_dtr(false)
        .map_err(|e| io::Error::new(e.kind(), format!("lowering DTR: {e}")))?;
    port.set_rts(false)
        .map_err(|e| io::Error::new(e.kind(), format!("lowering RTS: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what was done to its lines, and fails where told to.
    #[derive(Default)]
    struct Lines {
        log: Vec<&'static str>,
        fail_dtr: bool,
        fail_rts: bool,
    }

    impl ControlLines for Lines {
        fn set_dtr(&mut self, level: bool) -> io::Result<()> {
            self.log.push(if level { "DTR up" } else { "DTR down" });
            match self.fail_dtr {
                true => Err(io::Error::other("no")),
                false => Ok(()),
            }
        }
        fn set_rts(&mut self, level: bool) -> io::Result<()> {
            self.log.push(if level { "RTS up" } else { "RTS down" });
            match self.fail_rts {
                true => Err(io::Error::other("no")),
                false => Ok(()),
            }
        }
    }

    #[test]
    fn dtr_then_rts_go_down() {
        let mut l = Lines::default();
        lower_control_lines(&mut l).unwrap();
        assert_eq!(l.log, ["DTR down", "RTS down"]);
    }

    #[test]
    fn a_line_that_will_not_go_down_is_an_error() {
        let mut l = Lines {
            fail_dtr: true,
            ..Lines::default()
        };
        let e = lower_control_lines(&mut l).unwrap_err();
        assert!(e.to_string().contains("DTR"), "{e}");
        assert_eq!(l.log, ["DTR down"], "RTS is not touched after DTR fails");
        let mut l = Lines {
            fail_rts: true,
            ..Lines::default()
        };
        let e = lower_control_lines(&mut l).unwrap_err();
        assert!(e.to_string().contains("RTS"), "{e}");
        assert_eq!(l.log, ["DTR down", "RTS down"]);
    }
}
