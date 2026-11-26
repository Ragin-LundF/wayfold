//! Shell detection, shell-specific quoting and the opt-in integration scripts that make the
//! shell report its cwd (OSC 7) and prompt state (OSC 133).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use portable_pty::CommandBuilder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

/// Whether automatic directory sync can work for a session, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Integration {
    Supported(ShellKind),
    Unsupported(String),
}

pub fn shell_kind(shell: &Path) -> Option<ShellKind> {
    match shell.file_name()?.as_bytes() {
        b"zsh" => Some(ShellKind::Zsh),
        b"bash" => Some(ShellKind::Bash),
        b"fish" => Some(ShellKind::Fish),
        _ => None,
    }
}

/// The user's login shell: `$SHELL`, falling back to `/bin/zsh`.
pub fn login_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map_or_else(|| PathBuf::from("/bin/zsh"), PathBuf::from)
}

/// The full line typed at an idle prompt to change directory, including Enter.
pub fn cd_command(kind: ShellKind, path: &Path) -> Vec<u8> {
    let mut line = match kind {
        ShellKind::Zsh | ShellKind::Bash => b"cd -- ".to_vec(),
        // Paths are always absolute, so no option can be mistaken; fish's cd has no `--`.
        ShellKind::Fish => b"cd ".to_vec(),
    };
    line.extend(quote(kind, path.as_os_str().as_bytes()));
    line.push(b'\r');
    line
}

/// Quotes bytes so the shell's line editor and parser see exactly them: no control
/// character is ever typed raw (a newline would submit the line).
pub fn quote(kind: ShellKind, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let (open, close_escape_open): (&[u8], bool) = match kind {
        ShellKind::Zsh | ShellKind::Bash => (b"$'", false),
        // Fish single quotes cannot hold escapes: close, escape outside, reopen.
        ShellKind::Fish => (b"'", true),
    };
    out.extend(open);
    for (byte, escape) in classify(bytes) {
        // Interactive bash/zsh history expansion runs before parsing and reads `\'` inside
        // `$'…'` as a quote end, so a later `!` would expand: neither byte is ever typed raw.
        let escape = escape || (!close_escape_open && matches!(byte, b'\'' | b'!'));
        match byte {
            b'\\' => out.extend([b'\\', byte]),
            b'\'' if close_escape_open => out.extend([b'\\', byte]),
            _ if escape && close_escape_open => {
                out.push(b'\'');
                out.extend(hex_escape(byte));
                out.push(b'\'');
            }
            _ if escape => out.extend(hex_escape(byte)),
            _ => out.push(byte),
        }
    }
    out.push(b'\'');
    out
}

/// Marks bytes that must not be typed raw: control characters (C0, DEL, C1) and, in
/// non-UTF-8 names, every non-ASCII byte.
fn classify(bytes: &[u8]) -> Vec<(u8, bool)> {
    match std::str::from_utf8(bytes) {
        Ok(text) => text
            .chars()
            .flat_map(|ch| {
                let mut buf = [0; 4];
                let escape = ch.is_control();
                ch.encode_utf8(&mut buf)
                    .as_bytes()
                    .iter()
                    .map(move |&b| (b, escape))
                    .collect::<Vec<_>>()
            })
            .collect(),
        Err(_) => bytes
            .iter()
            .map(|&b| (b, !(0x20..0x7f).contains(&b)))
            .collect(),
    }
}

fn hex_escape(byte: u8) -> [u8; 4] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        b'\\',
        b'x',
        HEX[usize::from(byte >> 4)],
        HEX[usize::from(byte & 0xf)],
    ]
}

/// Builds the command for `shell`, with integration hooks if `enabled` and supported. The
/// hooks sign every marker with `nonce`, so output of other programs cannot forge them.
pub fn shell_command(shell: &Path, enabled: bool, nonce: &str) -> (CommandBuilder, Integration) {
    let mut command = CommandBuilder::new(shell);
    let Some(kind) = shell_kind(shell) else {
        let name = shell
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let reason = format!("Shell integration supports zsh, bash and fish, not {name}.");
        return (command, Integration::Unsupported(reason));
    };
    if !enabled {
        command.arg("-l");
        return (
            command,
            Integration::Unsupported("Shell integration is turned off.".into()),
        );
    }
    let dir = match integration_dir() {
        Ok(dir) => dir,
        Err(error) => {
            command.arg("-l");
            return (
                command,
                Integration::Unsupported(format!(
                    "Shell integration could not be installed: {error}"
                )),
            );
        }
    };
    // The scripts move it into a non-exported shell variable, so child processes never see it.
    command.env("WAYFOLD_NONCE", nonce);
    match kind {
        ShellKind::Zsh => {
            let user_zdotdir = std::env::var_os("ZDOTDIR")
                .or_else(|| std::env::var_os("HOME"))
                .unwrap_or_default();
            command.env("WAYFOLD_USER_ZDOTDIR", user_zdotdir);
            command.env("ZDOTDIR", dir.join("zsh"));
            command.arg("-l");
        }
        ShellKind::Bash => {
            // `--rcfile` only works for non-login shells; the rc file sources the login files itself.
            command.args([
                OsString::from("--rcfile"),
                dir.join("bash-rc").into_os_string(),
                OsString::from("-i"),
            ]);
        }
        ShellKind::Fish => {
            let source = [
                b"source ".as_slice(),
                &quote(kind, dir.join("wayfold.fish").as_os_str().as_bytes()),
            ]
            .concat();
            command.args([
                OsString::from("-l"),
                OsString::from("--init-command"),
                std::ffi::OsStr::from_bytes(&source).to_owned(),
            ]);
        }
    }
    (command, Integration::Supported(kind))
}

const ZSH_ENV: &str = r#"_wayfold_nonce=$WAYFOLD_NONCE
unset WAYFOLD_NONCE
ZDOTDIR="${WAYFOLD_USER_ZDOTDIR:-$HOME}"
unset WAYFOLD_USER_ZDOTDIR
[[ -f "$ZDOTDIR/.zshenv" ]] && source "$ZDOTDIR/.zshenv"
if [[ -o interactive ]]; then
  _wayfold_urlencode() {
    emulate -L zsh
    setopt no_multibyte
    local LC_ALL=C s=$1 out= c hex i
    for (( i = 1; i <= ${#s}; i++ )); do
      c=$s[i]
      if [[ $c == [A-Za-z0-9/._~-] ]]; then out+=$c; else printf -v hex '%%%02X' "'$c"; out+=$hex; fi
    done
    REPLY=$out
  }
  _wayfold_precmd() {
    local k=$_wayfold_nonce
    _wayfold_urlencode "$PWD"
    printf '\e]133;D;k=%s\a\e]7;k=%s;file://%s%s\a\e]133;A;k=%s\a\e]133;B;k=%s\a' "$k" "$k" "$HOST" "$REPLY" "$k" "$k"
  }
  _wayfold_preexec() { printf '\e]133;C;k=%s\a' "$_wayfold_nonce" }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd _wayfold_precmd
  add-zsh-hook preexec _wayfold_preexec
fi
"#;

const BASH_RC: &str = r#"__wayfold_nonce=$WAYFOLD_NONCE
unset WAYFOLD_NONCE
[ -f /etc/profile ] && . /etc/profile
if [ -f ~/.bash_profile ]; then . ~/.bash_profile
elif [ -f ~/.bash_login ]; then . ~/.bash_login
elif [ -f ~/.profile ]; then . ~/.profile
fi
__wayfold_urlencode() {
  local LC_ALL=C s=$1 out= c hex i
  for (( i = 0; i < ${#s}; i++ )); do
    c=${s:i:1}
    case $c in
      [A-Za-z0-9/._~-]) out+=$c ;;
      # bash 3.2 prints bytes >= 0x80 as negative numbers; the last two digits are right.
      *) printf -v hex '%02X' "'$c"; out+=%${hex: -2} ;;
    esac
  done
  REPLY=$out
}
__wayfold_precmd() {
  local status=$? k=$__wayfold_nonce
  __wayfold_urlencode "$PWD"
  printf '\e]133;D;k=%s\a\e]7;k=%s;file://%s%s\a\e]133;A;k=%s\a\e]133;B;k=%s\a' "$k" "$k" "$HOSTNAME" "$REPLY" "$k" "$k"
  return $status
}
PROMPT_COMMAND="__wayfold_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
PS0="${PS0}\[\e]133;C;k=${__wayfold_nonce}\a\]"
"#;

const FISH_INIT: &str = r#"set -g __wayfold_nonce $WAYFOLD_NONCE
set -e WAYFOLD_NONCE
function __wayfold_prompt --on-event fish_prompt
  set -l k $__wayfold_nonce
  printf '\e]133;D;k=%s\a\e]7;k=%s;file://%s%s\a\e]133;A;k=%s\a\e]133;B;k=%s\a' $k $k $hostname (string escape --style=url -- $PWD) $k $k
end
function __wayfold_preexec --on-event fish_preexec
  printf '\e]133;C;k=%s\a' $__wayfold_nonce
end
"#;

/// Writes the scripts once per process into a private directory under the user's temp dir.
fn integration_dir() -> Result<PathBuf, String> {
    static DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    DIR.get_or_init(|| {
        install_scripts(&std::env::temp_dir().join("wayfold-shell-integration-v2"))
            .map_err(|e| e.to_string())
    })
    .clone()
}

fn install_scripts(dir: &Path) -> io::Result<PathBuf> {
    create_private_dir(dir)?;
    create_private_dir(&dir.join("zsh"))?;
    fs::write(dir.join("zsh/.zshenv"), ZSH_ENV)?;
    fs::write(dir.join("bash-rc"), BASH_RC)?;
    fs::write(dir.join("wayfold.fish"), FISH_INIT)?;
    Ok(dir.to_path_buf())
}

/// Refuses a directory someone else owns or can write, so nobody can plant startup code.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        Err(_) => {}
    }
    let meta = fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is not a private directory",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const AWKWARD_NAMES: &[&str] = &[
        "with space",
        "it's",
        "dq\"x",
        "$HOME",
        "`tick`",
        "back\\slash",
        "line\nbreak",
        "tab\there",
        "\u{fc}nicode \u{85}c1",
        "-dash",
        "*glob?[x]",
        "semi;colon && echo pwned",
        "x'!#;touch pwned;#",
        "bang!! !$ !-1",
    ];

    /// Types the cd line into an interactive shell (history expansion on), like the PTY does.
    fn assert_interactive_cd_is_literal(shell: &str, args: &[&str], kind: ShellKind) {
        let root = tempfile::tempdir().expect("tempdir");
        for name in ["x'!#;touch pwned;#", "a'!!'b", "q'$(touch pwned)'!x"] {
            let target = root.path().join(name);
            fs::create_dir(&target).expect("create dir");
            let result = root.path().join("result");
            let mut input = b"echo warm-up history\n".to_vec();
            input.extend(cd_command(kind, &target));
            input.pop();
            input.extend(b"\nprintf '%s' \"$PWD\" > ");
            input.extend(quote(kind, result.as_os_str().as_bytes()));
            input.extend(b"\nexit\n");
            let mut child = Command::new(shell)
                .args(args)
                .current_dir(root.path())
                .env_remove("CDPATH")
                .env("HISTFILE", root.path().join("history"))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("run shell");
            std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), &input)
                .expect("write");
            child.wait().expect("wait");
            assert!(
                !root.path().join("pwned").exists(),
                "{shell}: {name:?} ran code"
            );
            assert_eq!(
                fs::read(&result).expect("result"),
                target.as_os_str().as_bytes(),
                "{shell}: {name:?}"
            );
            fs::remove_file(&result).expect("cleanup");
        }
    }

    #[test]
    fn interactive_bash_history_expansion_cannot_inject() {
        assert_interactive_cd_is_literal("/bin/bash", &["--norc", "-i"], ShellKind::Bash);
    }

    #[test]
    fn interactive_zsh_history_expansion_cannot_inject() {
        assert_interactive_cd_is_literal("/bin/zsh", &["-f", "-i"], ShellKind::Zsh);
    }

    fn run_cd(shell: &str, kind: ShellKind, target: &Path) -> Vec<u8> {
        let mut script = cd_command(kind, target);
        script.pop(); // the Enter key
        script.extend(b" && printf '%s' \"$PWD\"");
        let output = Command::new(shell)
            .arg("-c")
            .arg(std::ffi::OsStr::from_bytes(&script))
            .env_remove("CDPATH")
            .output()
            .expect("run shell");
        assert!(
            output.status.success(),
            "{shell} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn assert_round_trip(shell: &str, kind: ShellKind) {
        let root = tempfile::tempdir().expect("tempdir");
        for name in AWKWARD_NAMES {
            let target = root.path().join(name);
            fs::create_dir(&target).expect("create awkward dir");
            assert_eq!(
                run_cd(shell, kind, &target),
                target.as_os_str().as_bytes(),
                "{shell}: {name:?}"
            );
        }
    }

    #[test]
    fn zsh_quoting_round_trips() {
        assert_round_trip("/bin/zsh", ShellKind::Zsh);
    }

    #[test]
    fn bash_quoting_round_trips() {
        assert_round_trip("/bin/bash", ShellKind::Bash);
    }

    #[test]
    fn fish_quoting_round_trips_when_installed() {
        let Some(fish) = ["/opt/homebrew/bin/fish", "/usr/local/bin/fish"]
            .into_iter()
            .find(|p| Path::new(p).exists())
        else {
            eprintln!("fish not installed; skipped");
            return;
        };
        assert_round_trip(fish, ShellKind::Fish);
    }

    #[test]
    fn quoted_line_never_contains_control_bytes() {
        for name in AWKWARD_NAMES {
            for kind in [ShellKind::Zsh, ShellKind::Fish] {
                let quoted = quote(kind, name.as_bytes());
                assert!(
                    !quoted.iter().any(|&b| b < 0x20 || b == 0x7f),
                    "{kind:?} {name:?}"
                );
                assert!(
                    !quoted.windows(2).any(|w| w == [0xc2, 0x85]),
                    "C1 control kept raw"
                );
            }
        }
        assert_eq!(quote(ShellKind::Bash, b"\xff\n"), b"$'\\xff\\x0a'");
        assert_eq!(quote(ShellKind::Zsh, b"a'!"), b"$'a\\x27\\x21'");
        assert_eq!(quote(ShellKind::Fish, b"a'\nb"), b"'a\\''\\x0a'b'");
    }

    #[test]
    fn detects_shells_by_name() {
        assert_eq!(
            shell_kind(Path::new("/opt/homebrew/bin/fish")),
            Some(ShellKind::Fish)
        );
        assert_eq!(shell_kind(Path::new("/bin/sh")), None);
        let (_, integration) = shell_command(Path::new("/bin/sh"), true, "n");
        assert!(matches!(integration, Integration::Unsupported(reason) if reason.contains("sh")));
        let (_, integration) = shell_command(Path::new("/bin/zsh"), false, "n");
        assert!(matches!(integration, Integration::Unsupported(_)));
    }
}
