//! CLI argument parsing for the updater. Args are always passed by wupi.exe
//! as `--flag value` pairs; malformed input fails fast (the launcher never
//! produces it, but we never want to apply a payload to the wrong dir).

use std::path::PathBuf;

/// The parsed updater invocation.
pub struct Args {
    /// The PID of the spawning wupi.exe (waited on until it exits).
    pub pid: u32,
    /// The WUPI install root (absolute path to the dir containing wupi.exe).
    pub target_dir: PathBuf,
    /// The downloaded portable zip to apply.
    pub zip: PathBuf,
    /// The version being updated TO (surfaces in the result-marker toast).
    pub version: String,
}

/// Parse `std::env::args()` (skipping the program name).
pub fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Parse an arbitrary arg iterator. Split out so tests can drive it directly.
pub fn parse_args_from<I: Iterator<Item = String>>(mut it: I) -> Result<Args, String> {
    let mut pid: Option<u32> = None;
    let mut target_dir: Option<PathBuf> = None;
    let mut zip: Option<PathBuf> = None;
    let mut version: Option<String> = None;
    while let Some(flag) = it.next() {
        let val = it
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--pid" => pid = Some(val.parse().map_err(|e| format!("--pid parse: {e}"))?),
            "--target-dir" => target_dir = Some(PathBuf::from(val)),
            "--zip" => zip = Some(PathBuf::from(val)),
            "--version" => version = Some(val),
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(Args {
        pid: pid.ok_or("missing --pid")?,
        target_dir: target_dir.ok_or("missing --target-dir")?,
        zip: zip.ok_or("missing --zip")?,
        version: version.ok_or("missing --version")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_all_flags() {
        let args = parse_args_from(
            s(&[
                "--pid",
                "1234",
                "--target-dir",
                "C:/WUPI",
                "--zip",
                "C:/x.zip",
                "--version",
                "0.18.0",
            ])
            .into_iter(),
        )
        .unwrap();
        assert_eq!(args.pid, 1234);
        assert_eq!(args.target_dir, PathBuf::from("C:/WUPI"));
        assert_eq!(args.zip, PathBuf::from("C:/x.zip"));
        assert_eq!(args.version, "0.18.0");
    }

    #[test]
    fn rejects_missing_flag() {
        assert!(parse_args_from(s(&["--pid", "1"]).into_iter()).is_err());
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(parse_args_from(
            s(&[
                "--bogus", "x", "--pid", "1", "--target-dir", "/", "--zip", "/z", "--version", "v"
            ])
            .into_iter()
        )
        .is_err());
    }

    #[test]
    fn rejects_non_numeric_pid() {
        assert!(parse_args_from(
            s(&["--pid", "notnum", "--target-dir", "/", "--zip", "/z", "--version", "v"])
                .into_iter()
        )
        .is_err());
    }
}
