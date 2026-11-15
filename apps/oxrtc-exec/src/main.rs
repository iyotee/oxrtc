//! `oxrtc-exec`: confine, then become the program. See the library's
//! documentation for what is applied.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use oxrtc_exec::{Plan, REFUSED};

fn main() -> ExitCode {
    // Never print the program's arguments: FFmpeg's carry the camera's URL.
    match Plan::parse(std::env::args_os().skip(1)) {
        Ok(plan) => run(&plan),
        Err(error) => {
            eprintln!("oxrtc-exec: {error}");
            ExitCode::from(REFUSED)
        }
    }
}

#[cfg(target_os = "linux")]
fn run(plan: &Plan) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let report = match oxrtc_exec::confine(plan) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("oxrtc-exec: refused: {error}");
            return ExitCode::from(REFUSED);
        }
    };
    for warning in &report.warnings {
        eprintln!("oxrtc-exec: warning: {warning}");
    }
    if plan.check {
        println!("{}", report.landlock.summary());
        return ExitCode::SUCCESS;
    }
    let Some(program) = &plan.program else {
        return ExitCode::from(REFUSED);
    };
    // Returns only if the program could not be run.
    let error = std::process::Command::new(program).args(&plan.args).exec();
    eprintln!("oxrtc-exec: cannot run {}: {error}", program.display());
    ExitCode::from(if error.kind() == std::io::ErrorKind::NotFound {
        oxrtc_exec::NOT_FOUND
    } else {
        oxrtc_exec::NOT_RUN
    })
}

#[cfg(not(target_os = "linux"))]
fn run(_plan: &Plan) -> ExitCode {
    eprintln!("oxrtc-exec: Linux only (elsewhere oxrtc starts FFmpeg itself)");
    ExitCode::from(REFUSED)
}
