//! Command-line behaviour: exit codes, diagnostics and the help/version output.
//!
//! Run against the C build with `TTYD_BIN=../build-c/ttyd cargo test` to confirm the port
//! reproduces the original behaviour. `TTYD_REFERENCE=1` is no longer needed — the harness
//! asks the binary which implementation it is — and setting only one of the two is what used
//! to hang the whole suite.

mod common;

use std::time::Duration;

use common::{is_c_reference, run_cli, Server};

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
fn a_malformed_auth_url_is_rejected_at_startup() {
    // Forward auth fails closed, so a typo here would start cleanly and then turn the first
    // real traffic into a total outage — every request answered 500 by a subrequest that can
    // never succeed. Better to refuse to start.
    if common::is_c_reference() {
        return; // The C build has no forward auth.
    }
    for bad in ["notaurl", "ftp://host/verify", "http:///no-host"] {
        let result = run_cli(&["--auth-url", bad, "bash"]);
        assert_eq!(result.code, 255, "{bad} was accepted");
        assert!(
            result.stderr.contains("invalid auth-url"),
            "{bad}: stderr was {:?}",
            result.stderr
        );
    }
}

#[test]
fn a_malformed_auth_method_is_rejected_at_startup() {
    if common::is_c_reference() {
        return;
    }
    let result = run_cli(&[
        "--auth-url",
        "http://x/v",
        "--auth-method",
        "BAD METHOD",
        "bash",
    ]);
    assert_eq!(result.code, 255);
    assert!(
        result.stderr.contains("invalid auth-method"),
        "stderr was {:?}",
        result.stderr
    );
}

#[test]
fn an_option_missing_its_argument_is_reported() {
    // getopt_long's own diagnostics, reproduced: the long form names the option with two
    // dashes, the short form with one.
    // The option must be last, or the next word is taken as its argument.
    let long = run_cli(&["--credential"]);
    assert_eq!(long.code, 255);
    assert!(
        long.stderr.contains("requires an argument"),
        "stderr was {:?}",
        long.stderr
    );

    let short = run_cli(&["-c"]);
    assert_eq!(short.code, 255);
    assert!(
        short.stderr.contains("requires an argument"),
        "stderr was {:?}",
        short.stderr
    );
}

#[tokio::test]
async fn an_unrecognized_option_is_reported_but_not_fatal() {
    // Surprising, and worth pinning precisely because it is: `getopt_long` prints the
    // diagnostic and returns `?`, which the C build does not treat as an error — it carries
    // on and serves. Verified against the C binary, which does the same. Anyone expecting a
    // typo'd flag to stop the server is wrong about both builds.
    let mut server = Server::start(&["--no-such-option", "bash"]);
    let response = common::http_client()
        .get(server.http_url("/token"))
        .send()
        .await
        .expect("the server should still be serving");
    assert_eq!(response.status(), 200);
    assert!(
        server.wait_for_log("unrecognized option", Duration::from_secs(5)),
        "the option should still have been reported"
    );
}

#[test]
fn out_of_range_option_values_are_rejected() {
    // Each of these has its own message in the C build, and each is a separate branch here.
    for (args, needle) in [
        (vec!["-P", "-1", "bash"], "invalid ping interval"),
        (vec!["-f", "-1", "bash"], "invalid srv-buf-size"),
        (vec!["-t", "novalue", "bash"], "invalid client option"),
    ] {
        let result = run_cli(&args);
        assert_eq!(result.code, 255, "{args:?} was accepted");
        assert!(
            result.stderr.contains(needle),
            "{args:?}: stderr was {:?}",
            result.stderr
        );
    }
}

#[test]
fn a_hexadecimal_integer_is_accepted() {
    // The C build parses integer options with `strtol(…, 0)`, which reads an `0x` prefix as
    // hex. This port keeps that while rejecting the octal and trailing-garbage forms.
    let server = common::Server::start(&["-f", "0x2000", "bash"]);
    assert!(server.port > 0, "the server should have started");
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
