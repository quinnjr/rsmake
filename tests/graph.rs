//! Staleness edge cases that the differential corpus cannot express
//! portably: a future-dated file needs `std::fs::File::set_modified`, not a
//! GNU-make-and-rsmake shell fixture, so these are direct assertions against
//! the rsmake binary's `-n` output instead of corpus entries.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsmake-graph-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn set_mtime(path: &Path, when: SystemTime) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    f.set_modified(when)
        .unwrap_or_else(|e| panic!("set mtime on {}: {e}", path.display()));
}

fn run_dry(dir: &Path) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rsmake"))
        .arg("-n")
        .current_dir(dir)
        .output()
        .expect("run rsmake");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A target whose own mtime is in the future, but which is already newer
/// than every prerequisite, must not be rebuilt on that basis alone: GNU make
/// warns about the clock skew but does not remake an otherwise up-to-date
/// target just because its own timestamp looks wrong.
#[test]
fn own_future_mtime_does_not_force_rebuild_when_otherwise_up_to_date() {
    let dir = scratch("own-future");
    std::fs::write(dir.join("dep"), "d").unwrap();
    std::fs::write(dir.join("out"), "o").unwrap();
    std::fs::write(
        dir.join("Makefile"),
        "out: dep\n\techo building\n",
    )
    .unwrap();

    let now = SystemTime::now();
    set_mtime(&dir.join("dep"), now);
    set_mtime(&dir.join("out"), now + Duration::from_secs(1_000_000));

    let (code, stdout, stderr) = run_dry(&dir);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(
        !stdout.contains("echo building"),
        "a target already newer than its prerequisites must not rebuild just \
         because its own mtime is in the future\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("modification time in the future"),
        "clock skew on the target's own mtime should still be warned about\n\
         stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A prerequisite dated in the future still forces a rebuild -- that part of
/// the contract is unchanged -- and now also earns a clock-skew warning
/// naming the prerequisite.
#[test]
fn future_dated_prerequisite_forces_rebuild_and_warns() {
    let dir = scratch("dep-future");
    std::fs::write(dir.join("dep"), "d").unwrap();
    std::fs::write(dir.join("out"), "o").unwrap();
    std::fs::write(
        dir.join("Makefile"),
        "out: dep\n\techo building\n",
    )
    .unwrap();

    let now = SystemTime::now();
    set_mtime(&dir.join("out"), now);
    set_mtime(&dir.join("dep"), now + Duration::from_secs(1_000_000));

    let (code, stdout, stderr) = run_dry(&dir);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(
        stdout.contains("echo building"),
        "a future-dated prerequisite must still force a rebuild\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("dep") && stderr.contains("modification time in the future"),
        "the warning should name the future-dated prerequisite\nstderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A single future-dated prerequisite shared by two targets earns exactly one
/// clock-skew warning, not one per consuming target: GNU make warns once per
/// file, not once per edge in the dependency graph.
#[test]
fn shared_future_dated_prerequisite_warns_once_for_two_targets() {
    let dir = scratch("shared-future");
    std::fs::write(dir.join("h"), "h").unwrap();
    std::fs::write(dir.join("x"), "x").unwrap();
    std::fs::write(dir.join("y"), "y").unwrap();
    std::fs::write(
        dir.join("Makefile"),
        "all: x y\nx: h\n\techo bx\ny: h\n\techo by\n",
    )
    .unwrap();

    let now = SystemTime::now();
    set_mtime(&dir.join("x"), now);
    set_mtime(&dir.join("y"), now);
    set_mtime(&dir.join("h"), now + Duration::from_secs(1_000_000));

    let (code, stdout, stderr) = run_dry(&dir);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stdout.contains("echo bx"), "stdout:\n{stdout}");
    assert!(stdout.contains("echo by"), "stdout:\n{stdout}");
    assert_eq!(
        stderr.matches("modification time in the future").count(),
        1,
        "a prerequisite shared by two targets should be warned about once, \
         not once per target\nstderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
