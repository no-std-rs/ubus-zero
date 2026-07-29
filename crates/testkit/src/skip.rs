//! Visible test skips for optional upstream UBUS compatibility
//! fixtures. Lenient mode keeps fresh-clone developer workflows
//! usable; strict mode turns any skip into a hard failure for CI jobs
//! that are supposed to exercise the full matrix.

/// Report a skipped compat test. In lenient mode this only prints a
/// `SKIP ...` line; with `UBUS_ZERO_COMPAT=require` it panics so a
/// missing fixture cannot masquerade as a passing test run.
pub fn or_skip(test: &str, reason: &str) {
    or_skip_with(test, reason, strict_mode());
}

fn strict_mode() -> bool {
    std::env::var("UBUS_ZERO_COMPAT").as_deref() == Ok("require")
}

fn or_skip_with(test: &str, reason: &str, strict: bool) {
    eprintln!("SKIP {test}: {reason}");
    assert!(
        !strict,
        "UBUS_ZERO_COMPAT=require: skipped {test}: {reason}"
    );
}

#[cfg(test)]
mod tests {
    use super::or_skip_with;

    #[test]
    fn lenient_skip_returns_normally() {
        or_skip_with("compat_case", "fixture absent", false);
    }

    #[test]
    fn strict_skip_panics_with_test_and_reason() {
        let panic = std::panic::catch_unwind(|| {
            or_skip_with("compat_case", "fixture absent", true);
        })
        .expect_err("strict skip must panic");
        let msg = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("panic payload is string-like");
        assert!(msg.contains("compat_case"), "panic missing test: {msg}");
        assert!(
            msg.contains("fixture absent"),
            "panic missing reason: {msg}"
        );
    }
}
