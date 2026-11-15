//! The confinement itself (Linux): applied to this process, then kept across
//! the `exec()` of the program, which is the same process.

use landlock::{
    ABI, Access, AccessFs, BitFlags, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
};
use rustix::process::{Resource, Rlimit, Signal};

use crate::plan::{Landlock, Plan};

/// The newest Landlock ABI asked for; an older kernel enforces what it knows
/// (best effort), which [`Landlock::Partial`] reports.
const ABI_ASKED: ABI = ABI::V9;

/// What held, besides the guarantees an error would have refused.
#[derive(Debug)]
pub struct Report {
    /// The file-system ruleset.
    pub landlock: Landlock,
    /// What could not be applied without being worth refusing to run.
    pub warnings: Vec<String>,
}

/// Confines this process as `plan` says. An error means nothing must run:
/// the death signal, the parent check, the limits and no-new-privileges are
/// guarantees; a kernel without Landlock or a refused `nice` is a warning.
pub fn confine(plan: &Plan) -> Result<Report, String> {
    // Armed first: from here on, the parent's death kills this process (and
    // the program it becomes). Fired by the death of the *thread* that started
    // us, so oxrtc starts FFmpeg from one thread that lives as long as it.
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))
        .map_err(|e| format!("cannot arm the parent-death signal: {e}"))?;
    // The parent may have died before the signal was armed: then the parent
    // is init or a subreaper, and nothing would ever kill the program.
    let parent = rustix::process::getppid().map_or(0, |pid| pid.as_raw_nonzero().get());
    if u32::try_from(parent).ok() != Some(plan.parent) {
        return Err(format!("parent {} is gone (now {parent})", plan.parent));
    }

    // No core dump: it would hold the input URL, credentials included.
    let limits = [
        (Resource::As, plan.limits.address_space),
        (Resource::Fsize, plan.limits.file_size),
        (Resource::Nofile, plan.limits.open_files),
        (Resource::Core, Some(0)),
    ];
    for (resource, limit) in limits {
        if let Some(limit) = limit {
            lower(resource, limit)?;
        }
    }

    let mut warnings = Vec::new();
    if plan.limits.nice != 0
        && let Err(error) = rustix::process::nice(plan.limits.nice)
    {
        warnings.push(format!("nice {} refused: {error}", plan.limits.nice));
    }

    rustix::thread::set_no_new_privs(true).map_err(|e| format!("cannot set no-new-privileges: {e}"))?;

    let landlock = ruleset(plan).map_err(|e| format!("landlock: {e}"))?;
    if landlock == Landlock::None {
        warnings.push("this kernel has no Landlock: the program is not confined to its paths".into());
    }
    Ok(Report { landlock, warnings })
}

/// Lowers `resource` to `limit`, soft and hard, never above the inherited
/// hard limit (raising it would need a privilege, and is not the point).
fn lower(resource: Resource, limit: u64) -> Result<(), String> {
    let inherited = rustix::process::getrlimit(resource).maximum.unwrap_or(u64::MAX);
    let value = Some(limit.min(inherited));
    rustix::process::setrlimit(resource, Rlimit { current: value, maximum: value })
        .map_err(|e| format!("cannot limit {resource:?} to {limit}: {e}"))
}

/// Restricts the file system to the plan's paths: read and execute beneath
/// `read`, also write to `write` (opening only: nothing created, removed or
/// renamed anywhere), also connect to the sockets beneath `connect`; signals
/// and abstract UNIX sockets scoped to the sandbox. A path that does not exist
/// is skipped.
fn ruleset(plan: &Plan) -> Result<Landlock, landlock::RulesetError> {
    let read = AccessFs::from_read(ABI_ASKED);
    let write: BitFlags<AccessFs> = read | AccessFs::WriteFile;
    let connect: BitFlags<AccessFs> = read | AccessFs::ResolveUnix;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI_ASKED))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?
        .add_rules(path_beneath_rules(&plan.read, read))?
        .add_rules(path_beneath_rules(&plan.write, write))?
        .add_rules(path_beneath_rules(&plan.connect, connect))?
        .restrict_self()?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Landlock::Full,
        RulesetStatus::PartiallyEnforced => Landlock::Partial,
        RulesetStatus::NotEnforced => Landlock::None,
    })
}
