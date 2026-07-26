//! Command-line behaviour: exit codes, diagnostics and the help/version output.
//!
//! Run against the C build with `TTYD_BIN=... TTYD_REFERENCE=1 cargo test` to confirm the
//! port reproduces the original behaviour.

mod common;

use common::{is_c_reference, run_cli};

#[test]
fn version_goes_to_stdout() {
    let result = run_cli(&["--version"]);
    assert_eq!(result.code, 0);
    assert!(
        result.stdout.starts_with("ttyd version "),
        "stdout was {:?}",
        result.stdout
    );
}

#[test]
fn short_version_flag_matches_the_long_one() {
    assert_eq!(run_cli(&["-v"]).stdout, run_cli(&["--version"]).stdout);
}

#[test]
fn help_goes_to_stderr_and_exits_zero() {
    let result = run_cli(&["--help"]);
    assert_eq!(result.code, 0);
    assert!(result.stdout.is_empty(), "help must not go to stdout");
    assert!(result
        .stderr
        .contains("ttyd is a tool for sharing terminal over the web"));
    assert!(result.stderr.contains("USAGE:"));
}

#[test]
fn help_documents_every_original_option() {
    let stderr = run_cli(&["--help"]).stderr;
    for option in [
        "--port",
        "--interface",
        "--socket-owner",
        "--credential",
        "--auth-header",
        "--uid",
        "--gid",
        "--signal",
        "--cwd",
        "--url-arg",
        "--writable",
        "--client-option",
        "--terminal-type",
        "--check-origin",
        "--max-clients",
        "--once",
        "--exit-no-conn",
        "--browser",
        "--index",
        "--base-path",
        "--ping-interval",
        "--srv-buf-size",
        "--ipv6",
        "--ssl",
        "--ssl-cert",
        "--ssl-key",
        "--ssl-ca",
        "--debug",
        "--version",
        "--help",
    ] {
        assert!(stderr.contains(option), "help is missing {option}");
    }
}

#[test]
fn bare_invocation_prints_help() {
    let result = run_cli(&[]);
    assert_eq!(result.code, 0);
    assert!(result.stderr.contains("USAGE:"));
}

#[test]
fn missing_command_is_rejected() {
    let result = run_cli(&["-p", "0"]);
    assert_eq!(result.code, 255);
    assert!(result.stderr.contains("ttyd: missing start command"));
}

#[test]
fn credential_without_a_colon_is_rejected() {
    let result = run_cli(&["-c", "nocolon", "bash"]);
    assert_eq!(result.code, 255);
    assert!(result
        .stderr
        .contains("ttyd: invalid credential, format: username:password"));
}

#[test]
fn an_unknown_signal_name_is_rejected() {
    let result = run_cli(&["-s", "NOTASIGNAL", "bash"]);
    assert_eq!(result.code, 255);
    assert!(result.stderr.contains("ttyd: invalid signal"));
}

#[test]
fn a_non_numeric_integer_option_exits_one() {
    let result = run_cli(&["-p", "notanumber", "bash"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("invalid value for port"));
}

#[test]
fn a_missing_custom_index_is_rejected() {
    let result = run_cli(&["-I", "/nonexistent/index.html", "bash"]);
    assert_eq!(result.code, 255);
    assert!(result.stderr.contains("Can not stat index.html"));
}

#[test]
fn a_directory_as_custom_index_is_rejected() {
    let result = run_cli(&["-I", "/tmp", "bash"]);
    assert_eq!(result.code, 255);
    assert!(result.stderr.contains("is it a dir?"));
}

#[test]
fn forward_auth_options_are_documented() {
    if is_c_reference() {
        // Forward authentication is added by this port; the C build has no such options.
        return;
    }
    let stderr = run_cli(&["--help"]).stderr;
    for option in [
        "--auth-url",
        "--auth-request-header",
        "--auth-user-header",
        "--auth-method",
        "--auth-cache-ttl",
    ] {
        assert!(stderr.contains(option), "help is missing {option}");
    }
}
