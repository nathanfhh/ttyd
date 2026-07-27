//! Small helpers mirroring the semantics of the original C `src/utils.c`.

/// Signal names indexed by signal number, matching `sys_signame` in the C source.
/// Index 0 is a placeholder so that `SIG_NAMES[SIGHUP]` reads naturally.
#[cfg(target_os = "linux")]
pub const SIG_NAMES: [&str; 33] = [
    "zero", "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "UNUSED", "FPE", "KILL", "USR1", "SEGV",
    "USR2", "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU",
    "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS", "unknown",
];

#[cfg(not(target_os = "linux"))]
pub const SIG_NAMES: [&str; 33] = [
    "zero", "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "EMT", "FPE", "KILL", "BUS", "SEGV",
    "SYS", "PIPE", "ALRM", "TERM", "URG", "STOP", "TSTP", "CONT", "CHLD", "TTIN", "TTOU", "IO",
    "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "INFO", "USR1", "USR2", "unknown",
];

/// Renders a signal number as `SIG<NAME>`, falling back to `SIGUNKNOWN` like the C version.
pub fn get_sig_name(sig: i32) -> String {
    let name = if sig >= 0 && (sig as usize) < SIG_NAMES.len() {
        SIG_NAMES[sig as usize]
    } else {
        "unknown"
    };
    format!("SIG{}", name.to_uppercase())
}

/// Resolves a signal from `HUP`, `SIGHUP`, or a raw number. Returns 0 when unresolvable,
/// which callers treat as invalid (the C version tests `sig > 0`).
pub fn get_sig(name: &str) -> i32 {
    let bare = name
        .strip_prefix("SIG")
        .or_else(|| name.strip_prefix("sig"));
    for (sig, candidate) in SIG_NAMES.iter().enumerate().skip(1) {
        if *candidate == "unknown" {
            continue;
        }
        if candidate.eq_ignore_ascii_case(name)
            || bare.is_some_and(|b| candidate.eq_ignore_ascii_case(b))
        {
            return sig as i32;
        }
    }
    // Mirrors the C fallback to atoi(): an optional *leading* sign, then digits, stopping at
    // the first character that is neither. Accepting a sign anywhere in the run instead made
    // `9-1` collect as `9-1`, fail to parse and yield 0, where atoi answers 9 — so `-s 9-1`
    // was rejected here and accepted by the C build.
    let text = name.trim_start();
    let mut digits = String::with_capacity(text.len());
    let mut rest = text.chars().peekable();
    if matches!(rest.peek(), Some('+' | '-')) {
        digits.push(rest.next().expect("peeked"));
    }
    digits.extend(rest.take_while(|c| c.is_ascii_digit()));
    digits.parse().unwrap_or(0)
}

/// `strerror(errno)` as C prints it.
///
/// `io::Error`'s own `Display` appends the numeric code — "No such file or directory (os
/// error 2)" — which is friendlier but is not what the C build writes. Diagnostics that
/// reproduce a C message verbatim have to go through this instead of `{e}`.
pub fn strerror(e: &std::io::Error) -> String {
    let Some(code) = e.raw_os_error() else {
        return e.to_string();
    };
    let mut buf = vec![0u8; 256];
    // Safety: XSI strerror_r writes at most buf.len() bytes and NUL-terminates. On failure it
    // returns non-zero and the buffer contents are unspecified, which is why that path falls
    // back rather than reading the buffer.
    let rc = unsafe { libc::strerror_r(code, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return e.to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The machine's host name, used in the terminal window title.
pub fn hostname() -> String {
    let mut buf = vec![0u8; 256];
    // Safety: the buffer is large enough and we bound the result by an explicit NUL scan.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Opens a URI with the system browser. Returns false when no browser could be launched.
pub fn open_uri(uri: &str) -> bool {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    #[cfg(not(target_os = "macos"))]
    {
        // The C version refuses to spawn a browser when no X server is reachable.
        let has_display = std::process::Command::new("xset")
            .arg("-q")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_display {
            return false;
        }
    }

    std::process::Command::new(program)
        .arg(uri)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numeric_signal_is_parsed_the_way_atoi_would() {
        // Compared against the C build one input at a time; each of these is a value it
        // accepts, so rejecting them here would narrow what `--signal` takes.
        assert_eq!(get_sig("9"), 9);
        assert_eq!(get_sig("+9"), 9);
        assert_eq!(get_sig("-9"), -9);
        // atoi stops at the first character that is not part of the number.
        assert_eq!(get_sig("9-1"), 9);
        assert_eq!(get_sig("9+1"), 9);
        assert_eq!(get_sig("12abc"), 12);
        assert_eq!(get_sig("  15"), 15);
        // And yields zero when there is no number at all.
        assert_eq!(get_sig("nonsense"), 0);
        assert_eq!(get_sig("+"), 0);
        assert_eq!(get_sig("-"), 0);
        assert_eq!(get_sig(""), 0);
    }

    #[test]
    fn signal_names_round_trip() {
        assert_eq!(get_sig_name(1), "SIGHUP");
        assert_eq!(get_sig_name(15), "SIGTERM");
        assert_eq!(get_sig_name(9), "SIGKILL");
        assert_eq!(get_sig("HUP"), 1);
        assert_eq!(get_sig("SIGHUP"), 1);
        assert_eq!(get_sig("sigterm"), 15);
        assert_eq!(get_sig("9"), 9);
        assert_eq!(get_sig("nonsense"), 0);
    }

    #[test]
    fn unknown_signal_number_is_labelled() {
        assert_eq!(get_sig_name(200), "SIGUNKNOWN");
    }
}
