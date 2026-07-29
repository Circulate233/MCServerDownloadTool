#![forbid(unsafe_code)]

use std::io::{self, Write as _};
use std::process;
use std::sync::Arc;

use mc_server_download_tool::application::run;
use mc_server_download_tool::cli::try_parse_localized_from;
use mc_server_download_tool::error::AppError;
use mc_server_download_tool::i18n::{Localizer, resolve_language};

fn main() {
    if is_console_owner_probe() {
        process::exit(console_owner_exit_code());
    }
    let system_locale = sys_locale::get_locale();
    let cli = match try_parse_localized_from(std::env::args_os(), system_locale.as_deref()) {
        Ok(cli) => cli,
        Err(failure) => {
            let write_result = if failure.use_stderr() {
                io::stderr().lock().write_all(failure.rendered().as_bytes())
            } else {
                io::stdout().lock().write_all(failure.rendered().as_bytes())
            };
            if let Err(error) = write_result {
                eprintln!("failed to write command output: {error}");
                process::exit(70);
            }
            process::exit(failure.exit_code());
        }
    };

    let language = resolve_language(cli.lang, system_locale.as_deref());
    let localizer = Localizer::new(language);
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let error = AppError::CurrentExecutable(error);
            eprintln!("{}", localizer.fatal_error(&error));
            process::exit(error.exit_code().as_i32());
        }
    };

    let event_localizer = localizer;
    let observer = Arc::new(move |event| {
        println!("{}", event_localizer.install_event(&event));
    });
    match run(cli, &executable, system_locale.as_deref(), observer) {
        Ok(result) => println!(
            "{}",
            localizer.installation_complete(&result.installation.server_root)
        ),
        Err(error) => {
            eprintln!("{}", localizer.fatal_error(&error));
            process::exit(error.exit_code().as_i32());
        }
    }
}

fn is_console_owner_probe() -> bool {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    arguments
        .next()
        .is_some_and(|argument| argument == "--mcsdt-console-owner")
        && arguments.next().is_none()
}

#[cfg(windows)]
fn console_owner_exit_code() -> i32 {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..8 {
        let Some(parent) = system.process(pid).and_then(sysinfo::Process::parent) else {
            return 1;
        };
        let Some(process) = system.process(parent) else {
            return 1;
        };
        if process.name().eq_ignore_ascii_case("explorer.exe") {
            return 0;
        }
        pid = parent;
    }
    1
}

#[cfg(not(windows))]
const fn console_owner_exit_code() -> i32 {
    1
}
