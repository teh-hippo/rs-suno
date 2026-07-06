//! Last-run marker persistence: the `.suno-last-run` Unix-seconds stamp.

use std::path::Path;

use crate::cli::wallclock;

const LAST_RUN_NAME: &str = ".suno-last-run";

pub(crate) fn read_last_run(dest: &Path) -> Option<u64> {
    std::fs::read_to_string(dest.join(LAST_RUN_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(crate) fn write_last_run(dest: &Path) {
    let _ = std::fs::write(dest.join(LAST_RUN_NAME), wallclock::now_secs().to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_run_marker_round_trips() {
        let dir = Path::new("target").join(format!(
            "run-last-run-{}-{}",
            std::process::id(),
            wallclock::now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_last_run(&dir);
        assert!(read_last_run(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
