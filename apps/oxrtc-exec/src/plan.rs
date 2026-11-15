//! The command line of `oxrtc-exec`: what to confine and what to run.
//! oxrtc-ffmpeg builds it with [`Plan::to_args`]; the binary reads it back
//! with [`Plan::parse`]. One definition on both sides, so they cannot drift.
//!
//! ```text
//! oxrtc-exec [--check] --parent PID [--max-address-space BYTES]
//!            [--max-file-size BYTES] [--max-open-files N] [--nice N]
//!            [--read PATH]... [--write PATH]... [--connect PATH]...
//!            [-- PROGRAM [ARG]...]
//! ```

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;

/// Resource limits; `None` leaves a limit as inherited. A limit is only ever
/// lowered, soft and hard, never raised above what was inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// `RLIMIT_AS`: bytes of address space.
    pub address_space: Option<u64>,
    /// `RLIMIT_FSIZE`: largest file the process may write, in bytes (0: none
    /// at all; pipes and sockets are not files).
    pub file_size: Option<u64>,
    /// `RLIMIT_NOFILE`: open file descriptors.
    pub open_files: Option<u64>,
    /// Added to the niceness (0: unchanged).
    pub nice: i32,
}

impl Limits {
    /// The soft and hard values that apply `limit` to a resource inherited at
    /// `soft` and `hard` (`None`: unlimited): each one lowered to `limit`,
    /// never raised.
    pub fn lowered(limit: u64, soft: Option<u64>, hard: Option<u64>) -> (u64, u64) {
        let hard = limit.min(hard.unwrap_or(u64::MAX));
        (hard.min(soft.unwrap_or(u64::MAX)), hard)
    }
}

/// What `oxrtc-exec` is asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Confine itself, print what holds ([`Landlock::summary`]), run nothing.
    pub check: bool,
    /// The process that started it: if that is no longer its parent (it died
    /// before the death signal was armed), nothing runs.
    pub parent: u32,
    /// Resource limits.
    pub limits: Limits,
    /// Paths the program may read and execute beneath.
    pub read: Vec<PathBuf>,
    /// Paths it may also write (open for writing; never create or remove).
    pub write: Vec<PathBuf>,
    /// Paths it may also connect to as UNIX sockets (a name service).
    pub connect: Vec<PathBuf>,
    /// The program, run with the plan's own environment; `None` only with
    /// `check`.
    pub program: Option<PathBuf>,
    /// Its arguments.
    pub args: Vec<OsString>,
}

/// A command line `oxrtc-exec` refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// An option it does not know.
    Unknown(String),
    /// An option without its value.
    Missing(&'static str),
    /// A value that is not what the option takes.
    Invalid(&'static str),
    /// No `--parent`.
    NoParent,
    /// No program, and not `--check`.
    NoProgram,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(option) => write!(f, "unknown option {option}"),
            Self::Missing(option) => write!(f, "{option} needs a value"),
            Self::Invalid(option) => write!(f, "invalid value for {option}"),
            Self::NoParent => f.write_str("--parent is required"),
            Self::NoProgram => f.write_str("no program after --"),
        }
    }
}

impl std::error::Error for PlanError {}

const CHECK: &str = "--check";
const PARENT: &str = "--parent";
const ADDRESS_SPACE: &str = "--max-address-space";
const FILE_SIZE: &str = "--max-file-size";
const OPEN_FILES: &str = "--max-open-files";
const NICE: &str = "--nice";
const READ: &str = "--read";
const WRITE: &str = "--write";
const CONNECT: &str = "--connect";
const END: &str = "--";

impl Plan {
    /// The command line, `argv[0]` excluded.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if self.check {
            args.push(CHECK.into());
        }
        let mut option = |name: &str, value: OsString| {
            args.push(name.into());
            args.push(value);
        };
        option(PARENT, self.parent.to_string().into());
        let limits = [
            (ADDRESS_SPACE, self.limits.address_space),
            (FILE_SIZE, self.limits.file_size),
            (OPEN_FILES, self.limits.open_files),
        ];
        for (name, value) in limits {
            if let Some(value) = value {
                option(name, value.to_string().into());
            }
        }
        if self.limits.nice != 0 {
            option(NICE, self.limits.nice.to_string().into());
        }
        for (name, paths) in [(READ, &self.read), (WRITE, &self.write), (CONNECT, &self.connect)] {
            for path in paths {
                option(name, path.clone().into_os_string());
            }
        }
        if let Some(program) = &self.program {
            args.push(END.into());
            args.push(program.clone().into_os_string());
            args.extend(self.args.iter().cloned());
        }
        args
    }

    /// Reads a command line, `argv[0]` excluded.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, PlanError> {
        let mut plan = Self::default();
        let mut parent = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let name = arg.to_str().ok_or_else(|| PlanError::Unknown(arg.to_string_lossy().into_owned()))?;
            if name == END {
                plan.program = args.next().map(PathBuf::from);
                plan.args = args.collect();
                break;
            }
            if name == CHECK {
                plan.check = true;
                continue;
            }
            let (name, value) = match name {
                PARENT => (PARENT, args.next()),
                ADDRESS_SPACE => (ADDRESS_SPACE, args.next()),
                FILE_SIZE => (FILE_SIZE, args.next()),
                OPEN_FILES => (OPEN_FILES, args.next()),
                NICE => (NICE, args.next()),
                READ => (READ, args.next()),
                WRITE => (WRITE, args.next()),
                CONNECT => (CONNECT, args.next()),
                other => return Err(PlanError::Unknown(other.to_owned())),
            };
            let value = value.ok_or(PlanError::Missing(name))?;
            match name {
                PARENT => match number::<u32>(&value, PARENT)? {
                    0 => return Err(PlanError::Invalid(PARENT)),
                    pid => parent = Some(pid),
                },
                ADDRESS_SPACE => plan.limits.address_space = Some(number(&value, ADDRESS_SPACE)?),
                FILE_SIZE => plan.limits.file_size = Some(number(&value, FILE_SIZE)?),
                OPEN_FILES => plan.limits.open_files = Some(number(&value, OPEN_FILES)?),
                NICE => plan.limits.nice = number(&value, NICE)?,
                READ => plan.read.push(value.into()),
                WRITE => plan.write.push(value.into()),
                _ => plan.connect.push(value.into()),
            }
        }
        plan.parent = parent.ok_or(PlanError::NoParent)?;
        if plan.program.is_none() && !plan.check {
            return Err(PlanError::NoProgram);
        }
        Ok(plan)
    }
}

fn number<T: std::str::FromStr>(value: &OsStr, option: &'static str) -> Result<T, PlanError> {
    value.to_str().and_then(|text| text.parse().ok()).ok_or(PlanError::Invalid(option))
}

/// What the Landlock ruleset became on this kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landlock {
    /// Every right asked for is enforced.
    Full,
    /// Enforced, with the rights an older kernel does not know left out.
    Partial,
    /// The kernel has no Landlock (or it is disabled): paths are not confined.
    None,
}

impl Landlock {
    /// The line `--check` prints.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Full => "landlock=full",
            Self::Partial => "landlock=partial",
            Self::None => "landlock=none",
        }
    }

    /// Reads back a [`summary`](Self::summary) line.
    pub fn from_summary(line: &str) -> Option<Self> {
        match line.trim() {
            "landlock=full" => Some(Self::Full),
            "landlock=partial" => Some(Self::Partial),
            "landlock=none" => Some(Self::None),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
