//! Reading a secret a person pastes, such as the enrolment token. When standard input is a
//! terminal, its echo is off while the line is read, as a password prompt's is, so the secret never
//! shows on screen or stays in the terminal's scrollback. Piped input is read as it is.

use std::io::{self, BufRead};
use std::os::fd::AsFd;

use rustix::termios::{self, LocalModes, OptionalActions, Termios};
use zeroize::Zeroizing;

/// Echo off on a terminal until dropped, which restores the settings it found. `ECHONL` stays on,
/// so the person's Enter still ends the line on screen.
struct EchoOff<Fd: AsFd> {
    terminal: Fd,
    saved: Termios,
}

impl<Fd: AsFd> EchoOff<Fd> {
    fn new(terminal: Fd) -> io::Result<Self> {
        let saved = termios::tcgetattr(&terminal)?;
        let mut quiet = saved.clone();
        quiet.local_modes.remove(LocalModes::ECHO);
        quiet.local_modes.insert(LocalModes::ECHONL);
        termios::tcsetattr(&terminal, OptionalActions::Now, &quiet)?;
        Ok(Self { terminal, saved })
    }
}

impl<Fd: AsFd> Drop for EchoOff<Fd> {
    fn drop(&mut self) {
        // Nothing more can be done if the terminal refuses: it has gone, or it is no longer ours.
        let _restored = termios::tcsetattr(&self.terminal, OptionalActions::Now, &self.saved);
    }
}

/// One line of `input`, with `terminal`'s echo off while it is read when there is one: the
/// terminal `input` reads from.
pub(crate) fn read_line<Fd: AsFd>(
    input: &mut impl BufRead,
    terminal: Option<Fd>,
) -> io::Result<Zeroizing<String>> {
    let _quiet = terminal.map(EchoOff::new).transpose()?;
    let mut line = Zeroizing::new(String::new());
    input.read_line(&mut line)?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::io::{BufReader, Cursor, ErrorKind, Read as _, Write as _};
    use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
    use std::time::{Duration, Instant};

    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{LocalModes, tcgetattr};

    use super::read_line;

    /// A pseudo-terminal: the side a terminal emulator holds, and the side a program reads.
    fn pty() -> (OwnedFd, File) {
        let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&controller).unwrap();
        unlockpt(&controller).unwrap();
        let name = ptsname(&controller, Vec::new()).unwrap();
        let program = OpenOptions::new()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        (controller, program)
    }

    fn echoes(terminal: BorrowedFd<'_>) -> bool {
        tcgetattr(terminal)
            .unwrap()
            .local_modes
            .contains(LocalModes::ECHO)
    }

    /// What the terminal shows: everything the program's side wrote back so far.
    fn shown(controller: &OwnedFd) -> String {
        rustix::io::ioctl_fionbio(controller, true).unwrap();
        let mut screen = File::from(controller.try_clone().unwrap());
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 256];
        loop {
            match screen.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("reading the terminal: {error}"),
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn piped_input_is_read_as_it_is() {
        let line = read_line(
            &mut Cursor::new("abc_enr_piped\nmore\n"),
            None::<BorrowedFd<'_>>,
        )
        .unwrap();
        assert_eq!(line.as_str(), "abc_enr_piped\n");
    }

    #[test]
    fn a_terminal_does_not_show_the_pasted_secret_and_gets_its_echo_back() {
        let (controller, program) = pty();
        assert!(echoes(program.as_fd()), "a terminal echoes by default");
        let typist = controller.try_clone().unwrap();
        // The person pastes once the prompt is up: once echo is off, or after five seconds, so a
        // prompt that never turns it off shows the token and fails below rather than hanging.
        let paste = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while echoes(typist.as_fd()) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            File::from(typist).write_all(b"abc_enr_secret\n").unwrap();
        });
        let mut input = BufReader::new(program.try_clone().unwrap());
        let line = read_line(&mut input, Some(program.as_fd())).unwrap();
        paste.join().unwrap();
        assert_eq!(line.as_str(), "abc_enr_secret\n");
        let screen = shown(&controller);
        assert!(
            !screen.contains("abc_enr"),
            "the token was echoed: {screen:?}"
        );
        assert!(
            screen.contains('\n'),
            "Enter still ends the line: {screen:?}"
        );
        assert!(echoes(program.as_fd()), "echo is back after the prompt");
    }
}
