//! The function records (`tests/fixtures/function-records/`) pin what each
//! host function answered in 1.12, and `ORION_BLESS=1` rewrites them. A
//! record that carries something of the machine that blessed it — an
//! absolute path, a temp directory — fails on every other machine, or worse,
//! passes only where it was written. None may.

use std::path::Path;

/// Spellings a record only carries when it captured the blessing machine.
const MACHINE_SPECIFIC: &[&str] = &[
    "/Users/",
    "/home/",
    "/private/tmp",
    "/var/folders",
    "C:\\\\",
];

#[test]
fn function_records_carry_nothing_of_the_machine_that_blessed_them() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut checked = 0;
    for krate in std::fs::read_dir(&crates).expect("read crates/") {
        let dir = krate
            .expect("dir entry")
            .path()
            .join("tests/fixtures/function-records");
        let Ok(records) = std::fs::read_dir(&dir) else {
            continue;
        };
        for record in records {
            let path = record.expect("dir entry").path();
            let text = std::fs::read_to_string(&path).expect("read a record file");
            for needle in MACHINE_SPECIFIC {
                assert!(
                    !text.contains(needle),
                    "{} carries `{needle}`: a record must not capture the machine it was blessed on",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "found no function records under {}",
        crates.display()
    );
}
