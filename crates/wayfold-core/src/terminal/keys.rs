//! Translates key presses (GPUI key names, without depending on GPUI) to the bytes an
//! xterm-compatible terminal sends.

#[derive(Clone, Copy, Debug, Default)]
pub struct KeyMods {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS: never terminal input, left to the app's bindings.
    pub platform: bool,
}

/// `key` is the key name (`"enter"`, `"a"`), `key_char` the typed text if any.
/// `app_cursor` is the terminal's DECCKM mode. `None`: not terminal input.
pub fn encode_key(
    key: &str,
    key_char: Option<&str>,
    mods: KeyMods,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    if mods.platform {
        return None;
    }
    let cursor = |code: u8| -> Vec<u8> {
        if app_cursor {
            vec![0x1b, b'O', code]
        } else {
            vec![0x1b, b'[', code]
        }
    };
    let bytes = match key {
        "enter" => vec![b'\r'],
        "tab" if mods.shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "backspace" => vec![0x7f],
        "escape" => vec![0x1b],
        "up" => cursor(b'A'),
        "down" => cursor(b'B'),
        "right" => cursor(b'C'),
        "left" => cursor(b'D'),
        "home" => cursor(b'H'),
        "end" => cursor(b'F'),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "space" if mods.control => vec![0],
        _ if mods.control => return control_byte(key).map(|byte| vec![byte]),
        _ => {
            return key_char
                .filter(|text| !text.is_empty())
                .map(|text| text.as_bytes().to_vec());
        }
    };
    Some(bytes)
}

fn control_byte(key: &str) -> Option<u8> {
    let [byte] = key.as_bytes() else { return None };
    match byte.to_ascii_lowercase() {
        letter @ b'a'..=b'z' => Some(letter - b'a' + 1),
        b'@' | b'2' => Some(0),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' | b'6' => Some(0x1e),
        b'_' | b'-' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(key: &str, key_char: Option<&str>) -> Option<Vec<u8>> {
        encode_key(key, key_char, KeyMods::default(), false)
    }

    #[test]
    fn printable_named_and_control_keys() {
        assert_eq!(plain("a", Some("A")), Some(b"A".to_vec()));
        assert_eq!(plain("space", Some(" ")), Some(b" ".to_vec()));
        assert_eq!(plain("enter", Some("\n")), Some(b"\r".to_vec()));
        assert_eq!(plain("backspace", None), Some(vec![0x7f]));
        assert_eq!(plain("up", None), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            encode_key("up", None, KeyMods::default(), true),
            Some(b"\x1bOA".to_vec())
        );
        let ctrl = KeyMods {
            control: true,
            ..KeyMods::default()
        };
        assert_eq!(encode_key("c", None, ctrl, false), Some(vec![0x03]));
        assert_eq!(encode_key("[", None, ctrl, false), Some(vec![0x1b]));
        assert_eq!(encode_key("f5", None, KeyMods::default(), false), None);
    }

    #[test]
    fn command_keys_are_left_to_the_app() {
        let cmd = KeyMods {
            platform: true,
            ..KeyMods::default()
        };
        assert_eq!(encode_key("v", Some("v"), cmd, false), None);
    }
}
