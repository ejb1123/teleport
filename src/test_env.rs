//! Ignored tests that need something a CI runner lacks (a writable
//! `/dev/uinput`, a display or GPU for SDL's renderer) fail when it is
//! missing, so a developer machine that should have it notices. CI sets
//! `TELEPORT_TEST_SKIP_UNAVAILABLE=1` to skip them explicitly instead, with a
//! `SKIPPED` line naming the test and what it needs.

/// Whether `test` should return early: `false` when `available`; `true`, after
/// printing why, when `TELEPORT_TEST_SKIP_UNAVAILABLE=1`; panics otherwise.
pub fn skip_unless(test: &str, available: bool, needs: &str) -> bool {
    if available {
        return false;
    }
    if std::env::var_os("TELEPORT_TEST_SKIP_UNAVAILABLE").is_some_and(|value| value == "1") {
        eprintln!("SKIPPED {test}: needs {needs} (TELEPORT_TEST_SKIP_UNAVAILABLE=1)");
        return true;
    }
    panic!("{test} needs {needs}; set TELEPORT_TEST_SKIP_UNAVAILABLE=1 to skip it");
}
