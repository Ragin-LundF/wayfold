//! Scans raw PTY output for the shell-integration sequences OSC 7 (cwd) and OSC 133 (prompt
//! markers). The bytes are still fed to the terminal parser unchanged; this only listens.
//! Only sequences signed with the session's nonce (`k=<nonce>`) count: any program can print
//! OSC 7 / 133 (a remote shell's integration, `cat` of a log), but only our hooks know the nonce.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Longest OSC body we keep; longer ones (e.g. huge titles) are dropped, not buffered.
const MAX_OSC_LEN: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// OSC 7 with a local, absolute path.
    Cwd(PathBuf),
    /// OSC 133;A
    PromptStart,
    /// OSC 133;B
    InputStart,
    /// OSC 133;C
    CommandStart,
    /// OSC 133;D
    CommandDone,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
}

/// Incremental scanner; sequences may be split across reads.
pub struct OscScanner {
    state: State,
    body: Vec<u8>,
    overflow: bool,
    local_host: Vec<u8>,
    nonce: Vec<u8>,
}

impl OscScanner {
    pub fn new(local_host: impl Into<Vec<u8>>, nonce: &str) -> Self {
        Self {
            state: State::Ground,
            body: Vec::new(),
            overflow: false,
            local_host: local_host.into(),
            nonce: [b"k=", nonce.as_bytes()].concat(),
        }
    }

    pub fn scan(&mut self, bytes: &[u8], events: &mut Vec<ShellEvent>) {
        for &byte in bytes {
            self.state = match (std::mem::take(&mut self.state), byte) {
                (State::Ground, 0x1b) => State::Escape,
                (State::Ground, _) => State::Ground,
                (State::Escape, b']') => {
                    self.body.clear();
                    self.overflow = false;
                    State::Osc
                }
                (State::Escape, 0x1b) => State::Escape,
                (State::Escape, _) => State::Ground,
                (State::Osc, 0x07) => self.finish(events),
                (State::Osc, 0x1b) => State::OscEscape,
                (State::Osc, _) => {
                    self.push(byte);
                    State::Osc
                }
                (State::OscEscape, b'\\') => self.finish(events),
                // ESC inside an OSC aborts it and starts a new escape sequence.
                (State::OscEscape, b']') => {
                    self.body.clear();
                    self.overflow = false;
                    State::Osc
                }
                (State::OscEscape, _) => State::Ground,
            };
        }
    }

    fn push(&mut self, byte: u8) {
        if self.body.len() < MAX_OSC_LEN {
            self.body.push(byte);
        } else {
            self.overflow = true;
        }
    }

    fn finish(&mut self, events: &mut Vec<ShellEvent>) -> State {
        if !self.overflow
            && let Some(event) = parse_osc(&self.body, &self.local_host, &self.nonce)
        {
            events.push(event);
        }
        self.body.clear();
        State::Ground
    }
}

/// `7;k=<nonce>;<file url>` or `133;<letter>[;params];k=<nonce>`.
fn parse_osc(body: &[u8], local_host: &[u8], nonce: &[u8]) -> Option<ShellEvent> {
    if let Some(signed) = body.strip_prefix(b"7;") {
        let url = signed.strip_prefix(nonce)?.strip_prefix(b";")?;
        return parse_file_url(url, local_host).map(ShellEvent::Cwd);
    }
    let mut fields = body.strip_prefix(b"133;")?.split(|&b| b == b';');
    let event = match fields.next()? {
        b"A" => ShellEvent::PromptStart,
        b"B" => ShellEvent::InputStart,
        b"C" => ShellEvent::CommandStart,
        b"D" => ShellEvent::CommandDone,
        _ => return None,
    };
    fields.any(|field| field == nonce).then_some(event)
}

/// `file://host/abs/path` with percent-encoding; only empty, `localhost` or our own host name.
fn parse_file_url(url: &[u8], local_host: &[u8]) -> Option<PathBuf> {
    let rest = url.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (host, path) = rest.split_at(slash);
    let is_local = host.is_empty() || host.eq_ignore_ascii_case(b"localhost") || host.eq_ignore_ascii_case(local_host);
    if !is_local {
        return None;
    }
    let decoded = percent_decode(path)?;
    if decoded.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&decoded)))
}

fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            let high = hex_value(*bytes.next()?)?;
            let low = hex_value(*bytes.next()?)?;
            out.push(high << 4 | low);
        } else {
            out.push(byte);
        }
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_chunks(chunks: &[&[u8]]) -> Vec<ShellEvent> {
        let mut scanner = OscScanner::new("myhost", "n0nce");
        let mut events = Vec::new();
        for chunk in chunks {
            scanner.scan(chunk, &mut events);
        }
        events
    }

    #[test]
    fn osc7_and_markers_split_across_reads() {
        let stream: &[u8] =
            b"out\x1b]133;D;0;k=n0nce\x07\x1b]7;k=n0nce;file://myhost/tmp/a%20b%0Ac\x1b\\\x1b]133;A;k=n0nce\x07$ \x1b]133;B;k=n0nce\x07";
        let expected =
            vec![ShellEvent::CommandDone, ShellEvent::Cwd(PathBuf::from("/tmp/a b\nc")), ShellEvent::PromptStart, ShellEvent::InputStart];
        for split in 0..stream.len() {
            let (first, second) = stream.split_at(split);
            assert_eq!(scan_chunks(&[first, second]), expected, "split at {split}");
        }
        let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(scan_chunks(&bytewise), expected);
    }

    #[test]
    fn markers_without_the_session_nonce_are_ignored() {
        let forged: &[&[u8]] = &[
            b"\x1b]133;A\x07",
            b"\x1b]133;B\x07",
            b"\x1b]133;C\x07",
            b"\x1b]133;D;0\x07",
            b"\x1b]133;A;k=wrong\x07",
            b"\x1b]133;A;k=n0nce2\x07",
            b"\x1b]133;Ak=n0nce\x07",
            b"\x1b]7;file:///tmp\x07",
            b"\x1b]7;k=wrong;file:///tmp\x07",
            b"\x1b]7;k=n0ncefile:///tmp\x07",
            b"\x1b]7;file:///tmp;k=n0nce\x07",
        ];
        for sequence in forged {
            assert_eq!(scan_chunks(&[sequence]), vec![], "{sequence:?}");
        }
        assert_eq!(
            scan_chunks(&[b"\x1b]133;C;k=n0nce\x07\x1b]7;k=n0nce;file:///tmp\x07"]),
            vec![ShellEvent::CommandStart, ShellEvent::Cwd("/tmp".into())]
        );
    }

    fn cwd(url: &str) -> Vec<ShellEvent> {
        scan_chunks(&[format!("\x1b]7;k=n0nce;{url}\x07").as_bytes()])
    }

    #[test]
    fn rejects_remote_hosts_relative_paths_and_bad_escapes() {
        assert_eq!(cwd("file://other/tmp"), vec![]);
        assert_eq!(cwd("file://myhost"), vec![]);
        assert_eq!(cwd("file:///tmp/%zz"), vec![]);
        assert_eq!(cwd("file:///tmp/%00"), vec![]);
        assert_eq!(cwd("http://myhost/tmp"), vec![]);
        assert_eq!(cwd("file://LOCALHOST/tmp/%C3%BC"), vec![ShellEvent::Cwd(PathBuf::from("/tmp/\u{fc}"))]);
        assert_eq!(cwd("file:///x"), vec![ShellEvent::Cwd(PathBuf::from("/x"))]);
    }

    #[test]
    fn non_utf8_path_bytes_survive() {
        assert_eq!(cwd("file:///tmp/%FF"), vec![ShellEvent::Cwd(PathBuf::from(OsStr::from_bytes(b"/tmp/\xff")))]);
    }

    #[test]
    fn ignores_other_sequences_and_oversized_bodies() {
        assert_eq!(scan_chunks(&[b"\x1b[31mred\x1b]0;title\x07\x1b]133;X;k=n0nce\x07"]), vec![]);
        let mut huge = b"\x1b]7;k=n0nce;file:///".to_vec();
        huge.extend(std::iter::repeat_n(b'a', MAX_OSC_LEN + 10));
        huge.extend(b"\x07\x1b]133;C;k=n0nce\x07");
        assert_eq!(scan_chunks(&[&huge]), vec![ShellEvent::CommandStart]);
    }
}
