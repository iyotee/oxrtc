//! The command line: go2rtc's `-config <path>` (Go's flag package also takes
//! `--config`, and `=` in place of the space), and `--version`.

use std::path::PathBuf;

/// One line of help, printed with every usage error.
pub const USAGE: &str = "usage: oxrtc -config <path> | --version";

/// What `--version` prints: go2rtc's line (`go2rtc version 1.9.15
/// windows/amd64`) with this program's name, so that a tool reading the
/// version after the word `version` (openIVR's `install.py`) reads ours,
/// and the platform in Go's words.
pub fn version_line(version: &str) -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    format!("oxrtc version {version} {}/{arch}", std::env::consts::OS)
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run with this configuration file.
    Run(PathBuf),
    /// Print the version and exit.
    Version,
}

/// Why the command line is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    /// No `-config` was given.
    #[error("no configuration file given")]
    NoConfig,
    /// `-config` came last, without its value.
    #[error("{0} needs a path")]
    MissingValue(String),
    /// An argument this program does not know.
    #[error("unknown argument {0:?}")]
    Unknown(String),
    /// `-config` given twice.
    #[error("the configuration file is given twice")]
    Twice,
}

/// The name of a flag without its one or two leading dashes, or `None` for
/// something that is not a flag.
fn flag_name(arg: &str) -> Option<&str> {
    let name = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'))?;
    (!name.is_empty() && !name.starts_with('-')).then_some(name)
}

/// Parses the arguments that follow the program name. It stops at the first
/// error, and a second `-config` is one, so it reads at most three arguments.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, CliError> {
    let mut args = args.into_iter();
    let mut config: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        let Some(name) = flag_name(&arg) else { return Err(CliError::Unknown(arg)) };
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (name, None),
        };
        match name {
            "version" if inline.is_none() => return Ok(Command::Version),
            "config" => {
                let value = match inline {
                    Some(value) => value,
                    None => args.next().ok_or_else(|| CliError::MissingValue(arg.clone()))?,
                };
                if value.is_empty() {
                    return Err(CliError::MissingValue(arg));
                }
                if config.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError::Twice);
                }
            }
            _ => return Err(CliError::Unknown(arg)),
        }
    }
    config.map(Command::Run).ok_or(CliError::NoConfig)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command, CliError> {
        parse(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn every_spelling_of_the_config_flag() {
        for args in [&["-config", "a.yaml"][..], &["--config", "a.yaml"], &["-config=a.yaml"], &["--config=a.yaml"]] {
            assert_eq!(run(args), Ok(Command::Run(PathBuf::from("a.yaml"))), "{args:?}");
        }
        assert_eq!(
            run(&["-config", "C:/with space/go2rtc.yaml"]),
            Ok(Command::Run("C:/with space/go2rtc.yaml".into()))
        );
    }

    #[test]
    fn version_wins_whatever_comes_after() {
        assert_eq!(run(&["--version"]), Ok(Command::Version));
        assert_eq!(run(&["-version"]), Ok(Command::Version));
        assert_eq!(run(&["--version", "junk"]), Ok(Command::Version));
    }

    #[test]
    fn the_version_line_reads_like_go2rtcs() {
        let line = version_line("0.1.0");
        let words: Vec<&str> = line.split(' ').collect();
        assert_eq!(words.get(..3), Some(&["oxrtc", "version", "0.1.0"][..]), "{line}");
        let platform = words.get(3).copied().unwrap_or_default();
        assert!(
            ["windows/amd64", "linux/amd64", "linux/arm64"].contains(&platform)
                || cfg!(not(any(windows, target_os = "linux"))),
            "{line}"
        );
        assert_eq!(words.len(), 4, "{line}");
    }

    #[test]
    fn usage_errors() {
        assert_eq!(run(&[]), Err(CliError::NoConfig));
        assert_eq!(run(&["-config"]), Err(CliError::MissingValue("-config".into())));
        assert_eq!(run(&["--config="]), Err(CliError::MissingValue("--config=".into())));
        assert_eq!(run(&["a.yaml"]), Err(CliError::Unknown("a.yaml".into())));
        assert_eq!(run(&["-daemon"]), Err(CliError::Unknown("-daemon".into())));
        assert_eq!(run(&["---config", "a"]), Err(CliError::Unknown("---config".into())));
        assert_eq!(run(&["-version=1"]), Err(CliError::Unknown("-version=1".into())));
        assert_eq!(run(&["-config", "a", "-config", "b"]), Err(CliError::Twice));
    }
}
