use super::*;
use proptest::prelude::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn full() -> Plan {
    Plan {
        check: false,
        parent: 4242,
        limits: Limits { address_space: Some(4 << 30), file_size: Some(0), open_files: Some(256), nice: 2 },
        read: vec!["/usr".into(), "/etc/hosts".into()],
        write: vec!["/dev/null".into()],
        connect: vec!["/run/nscd".into()],
        program: Some("/usr/bin/ffmpeg".into()),
        args: os(&["-i", "rtsp://user:pass@cam/live", "--", "--read", "-f", "rtsp"]),
    }
}

#[test]
fn the_command_line_reads_as_written() {
    let args = full().to_args();
    let expected = os(&[
        "--parent",
        "4242",
        "--max-address-space",
        "4294967296",
        "--max-file-size",
        "0",
        "--max-open-files",
        "256",
        "--nice",
        "2",
        "--read",
        "/usr",
        "--read",
        "/etc/hosts",
        "--write",
        "/dev/null",
        "--connect",
        "/run/nscd",
        "--",
        "/usr/bin/ffmpeg",
        "-i",
        "rtsp://user:pass@cam/live",
        "--",
        "--read",
        "-f",
        "rtsp",
    ]);
    assert_eq!(args, expected);
    // What comes after the program is its own, options of ours included.
    assert_eq!(Plan::parse(args).unwrap(), full());
}

#[test]
fn unset_limits_are_left_out_and_check_needs_no_program() {
    let plan = Plan { check: true, parent: 7, ..Plan::default() };
    assert_eq!(plan.to_args(), os(&["--check", "--parent", "7"]));
    assert_eq!(Plan::parse(plan.to_args()).unwrap(), plan);
}

#[test]
fn bad_command_lines_are_refused() {
    let cases: [(&[&str], PlanError); 8] = [
        (&["--", "/bin/true"], PlanError::NoParent),
        (&["--parent", "1"], PlanError::NoProgram),
        (&["--parent", "1", "--"], PlanError::NoProgram),
        (&["--parent"], PlanError::Missing("--parent")),
        (&["--parent", "0", "--", "x"], PlanError::Invalid("--parent")),
        (&["--parent", "1", "--nice", "high", "--", "x"], PlanError::Invalid("--nice")),
        (&["--parent", "1", "--max-open-files", "-3", "--", "x"], PlanError::Invalid("--max-open-files")),
        (&["--parent", "1", "--shell", "--", "x"], PlanError::Unknown("--shell".into())),
    ];
    for (args, expected) in cases {
        assert_eq!(Plan::parse(os(args)), Err(expected.clone()), "{args:?}");
        assert!(!expected.to_string().is_empty());
    }
}

#[test]
fn the_check_line_reads_back() {
    for state in [Landlock::Full, Landlock::Partial { abi: 4 }, Landlock::None] {
        assert_eq!(Landlock::from_summary(&format!("{}\n", state.summary())), Some(state));
    }
    assert_eq!(Landlock::from_summary("landlock=partial abi=6").unwrap().summary(), "landlock=partial abi=6");
    for bad in ["landlock=maybe", "landlock=partial", "landlock=partial abi=", "landlock=partial abi=x"] {
        assert_eq!(Landlock::from_summary(bad), None, "{bad}");
    }
}

/// A partial ruleset names what this kernel leaves out, so the startup log
/// says it rather than a bare "partial": a kernel of 6.6 (ABI 4, the
/// Raspberry Pi's until 6.12) neither scopes signals nor restricts device
/// ioctls.
#[test]
fn a_partial_ruleset_says_what_the_kernel_leaves_out() {
    let gaps = Landlock::Partial { abi: 4 }.unenforced();
    assert_eq!(gaps.len(), 2, "{gaps:?}");
    assert!(gaps.iter().any(|g| g.starts_with("device ioctls")), "{gaps:?}");
    assert!(gaps.iter().any(|g| g.starts_with("signal and abstract-socket scoping")), "{gaps:?}");
    let gaps = Landlock::Partial { abi: 1 }.unenforced();
    assert!(gaps.iter().any(|g| g.starts_with("truncation")), "{gaps:?}");
    assert_eq!(gaps.len(), 3);
    assert!(Landlock::Partial { abi: 6 }.unenforced().is_empty());
    assert!(Landlock::Full.unenforced().is_empty());
}

fn text() -> impl Strategy<Value = String> {
    // Anything an argument can carry, dashes and spaces included.
    "[ -~]{0,24}"
}

proptest! {
    #[test]
    fn any_plan_reads_back_as_it_was_written(
        check in any::<bool>(),
        parent in 1u32..,
        address_space in proptest::option::of(any::<u64>()),
        file_size in proptest::option::of(any::<u64>()),
        open_files in proptest::option::of(any::<u64>()),
        nice in -20i32..20,
        read in proptest::collection::vec("[a-z/]{1,12}", 0..4),
        write in proptest::collection::vec("[a-z/]{1,12}", 0..2),
        program in proptest::option::of("[a-z/]{1,12}"),
        args in proptest::collection::vec(text(), 0..6),
    ) {
        let has_program = program.is_some();
        let plan = Plan {
            check: check || !has_program,
            parent,
            limits: Limits { address_space, file_size, open_files, nice },
            read: read.into_iter().map(PathBuf::from).collect(),
            write: write.into_iter().map(PathBuf::from).collect(),
            connect: Vec::new(),
            program: program.map(PathBuf::from),
            args: if has_program { args.into_iter().map(OsString::from).collect() } else { Vec::new() },
        };
        prop_assert_eq!(Plan::parse(plan.to_args()).unwrap(), plan);
    }
}

/// A limit is only ever lowered: an inherited soft limit below it (an
/// operator's `ulimit -S -v`) stays, and neither value ever goes above what
/// was inherited.
#[test]
fn a_limit_lowers_and_never_raises() {
    let gib = 1 << 30;
    // Unlimited: both become the limit.
    assert_eq!(Limits::lowered(4 * gib, None, None), (4 * gib, 4 * gib));
    // Inherited above the limit: lowered to it.
    assert_eq!(Limits::lowered(256, Some(1024), Some(524_288)), (256, 256));
    // A soft limit already below it stays where the operator put it.
    assert_eq!(Limits::lowered(4 * gib, Some(2 * gib), None), (2 * gib, 4 * gib));
    // A hard limit below it is the ceiling of both.
    assert_eq!(Limits::lowered(4 * gib, Some(gib), Some(gib)), (gib, gib));
    assert_eq!(Limits::lowered(0, Some(1024), None), (0, 0));
}

proptest! {
    #[test]
    fn a_lowered_limit_is_never_above_what_was_inherited_or_asked(
        limit: u64,
        soft: Option<u64>,
        hard: Option<u64>,
    ) {
        // The kernel keeps soft <= hard.
        let soft = match (soft, hard) {
            (Some(s), Some(h)) => Some(s.min(h)),
            (None, Some(h)) => Some(h),
            (s, None) => s,
        };
        let (new_soft, new_hard) = Limits::lowered(limit, soft, hard);
        prop_assert!(new_soft <= new_hard);
        prop_assert!(new_hard <= limit && new_soft <= limit);
        prop_assert!(new_soft <= soft.unwrap_or(u64::MAX));
        prop_assert!(new_hard <= hard.unwrap_or(u64::MAX));
    }
}
