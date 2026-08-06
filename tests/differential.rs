//! The differential oracle.
//!
//! Real GNU make is on the build host, so rsmake's conformance is a measured
//! quantity rather than an opinion. Each corpus entry is run under both
//! implementations with `-n` and the emitted recipe streams are diffed: dry
//! run means intended commands are compared without executing anything, so the
//! harness is fast and hermetic.
//!
//! The corpus is a ratchet. An entry listed in the baseline that stops
//! agreeing fails the build, and an entry that starts agreeing fails it too —
//! with an instruction to add it. Silence in either direction would let
//! coverage drift without anyone noticing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const BASELINE: &str = "tests/conformance-baseline.txt";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create destination");
    for e in std::fs::read_dir(src).expect("read corpus entry") {
        let e = e.expect("read dir entry");
        let to = dst.join(e.file_name());
        if e.file_type().expect("file type").is_dir() {
            copy_tree(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), &to).expect("copy fixture");
        }
    }
}

/// Both implementations name themselves in their diagnostics, and that is the
/// one difference the oracle must not report.
fn normalise(s: &str) -> String {
    s.lines()
        .map(|l| {
            l.strip_prefix("make: ")
                .or_else(|| l.strip_prefix("rsmake: "))
                .map(|r| format!("MAKE: {r}"))
                .unwrap_or_else(|| l.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Outcome {
    stdout: String,
    code: i32,
}

fn invoke(program: &str, dir: &Path, args: &[String]) -> Outcome {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("could not run {program}: {e}"));
    Outcome {
        stdout: normalise(&String::from_utf8_lossy(&out.stdout)),
        code: out.status.code().unwrap_or(-1),
    }
}

fn corpus_entries() -> Vec<String> {
    let dir = repo_root().join("tests/corpus");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("corpus directory is present")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

fn baseline() -> BTreeSet<String> {
    let path = repo_root().join(BASELINE);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Run one entry under both implementations. `Ok(())` means they agree.
fn compare(name: &str, scratch: &Path) -> Result<(), String> {
    let src = repo_root().join("tests/corpus").join(name);
    let work_gnu = scratch.join(format!("{name}.gnu"));
    let work_rs = scratch.join(format!("{name}.rs"));
    copy_tree(&src, &work_gnu);
    copy_tree(&src, &work_rs);

    let args: Vec<String> = match std::fs::read_to_string(src.join("ARGS")) {
        Ok(s) => s.split_whitespace().map(str::to_string).collect(),
        Err(_) => vec!["-n".to_string()],
    };

    let gnu = invoke("make", &work_gnu, &args);
    let rs = invoke(env!("CARGO_BIN_EXE_rsmake"), &work_rs, &args);

    if gnu.code != 0 {
        return Err(format!(
            "GNU make itself failed on this entry (exit {}), so it cannot serve as an \
             oracle. Fix the corpus entry.\n--- gnu stdout ---\n{}",
            gnu.code, gnu.stdout
        ));
    }
    if rs.code != 0 {
        return Err(format!(
            "rsmake exited {} where GNU make exited 0\n--- rsmake stdout ---\n{}",
            rs.code, rs.stdout
        ));
    }
    if gnu.stdout != rs.stdout {
        return Err(format!(
            "recipe streams differ\n--- gnu ---\n{}\n--- rsmake ---\n{}",
            gnu.stdout, rs.stdout
        ));
    }
    Ok(())
}

#[test]
fn corpus_agrees_with_gnu_make() {
    let probe = Command::new("make").arg("--version").output();
    let ok = probe.map(|o| o.status.success()).unwrap_or(false);
    assert!(
        ok,
        "GNU make is not on this host. It is the oracle this suite is built \
         around, so its absence is a failure rather than a reason to skip: \
         without it rsmake's conformance is unmeasured."
    );

    let scratch = std::env::temp_dir().join(format!("rsmake-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("create scratch directory");

    let expected = baseline();
    let entries = corpus_entries();
    let mut passing = BTreeSet::new();
    let mut report = String::new();

    for name in &entries {
        match compare(name, &scratch) {
            Ok(()) => {
                passing.insert(name.clone());
            }
            Err(why) => {
                if expected.contains(name) {
                    report.push_str(&format!("\n=== REGRESSED: {name} ===\n{why}\n"));
                }
            }
        }
    }

    let stale: Vec<&String> = expected.iter().filter(|n| !entries.contains(n)).collect();
    if !stale.is_empty() {
        report.push_str(&format!(
            "\nbaseline names entries that no longer exist: {stale:?}\n"
        ));
    }

    let gained: Vec<&String> = passing.difference(&expected).collect();
    if !gained.is_empty() {
        report.push_str(&format!(
            "\nthese entries now agree with GNU make but are not in the baseline: {gained:?}\n\
             Add them to {BASELINE}. The ratchet only holds if it is tightened.\n"
        ));
    }

    let _ = std::fs::remove_dir_all(&scratch);
    assert!(
        report.is_empty(),
        "conformance ratchet failed ({} of {} entries agree){report}",
        passing.len(),
        entries.len()
    );
}
