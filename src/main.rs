#![forbid(unsafe_code)]

use std::io::{self, Write as _};
use std::process;
use std::sync::Arc;

use mc_server_download_tool::application::run;
use mc_server_download_tool::cli::try_parse_localized_from;
use mc_server_download_tool::error::AppError;
use mc_server_download_tool::i18n::{Localizer, resolve_language};

#[cfg(windows)]
static PAUSE_AFTER_RUN: std::sync::Once = std::sync::Once::new();

fn main() {
    if is_console_owner_probe() {
        process::exit(console_owner_exit_code());
    }
    install_panic_pause_hook();
    let exit_code = run_installer();
    pause_if_explorer_console();
    process::exit(exit_code);
}

fn run_installer() -> i32 {
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
                return 70;
            }
            return failure.exit_code();
        }
    };

    let language = resolve_language(cli.lang, system_locale.as_deref());
    let localizer = Localizer::new(language);
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let error = AppError::CurrentExecutable(error);
            eprintln!("{}", localizer.fatal_error(&error));
            return error.exit_code().as_i32();
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
            return error.exit_code().as_i32();
        }
    }
    0
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
    i32::from(!launched_from_explorer())
}

#[cfg(windows)]
fn pause_if_explorer_console() {
    use std::io::IsTerminal as _;
    if !io::stdin().is_terminal() || !launched_from_explorer() {
        return;
    }
    PAUSE_AFTER_RUN.call_once(|| {
        print!("按 Enter 键关闭此窗口 / Press Enter to close this window...");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    });
}

#[cfg(windows)]
fn install_panic_pause_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        default_hook(panic);
        pause_if_explorer_console();
    }));
}

#[cfg(windows)]
fn launched_from_explorer() -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..8 {
        let Some(parent) = system.process(pid).and_then(sysinfo::Process::parent) else {
            return false;
        };
        let Some(process) = system.process(parent) else {
            return false;
        };
        if process.name().eq_ignore_ascii_case("explorer.exe") {
            return true;
        }
        pid = parent;
    }
    false
}

#[cfg(not(windows))]
const fn console_owner_exit_code() -> i32 {
    1
}

#[cfg(not(windows))]
const fn pause_if_explorer_console() {}

#[cfg(not(windows))]
const fn install_panic_pause_hook() {}
