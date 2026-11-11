mod authenticator;
mod config;
mod glitch;
mod helper_message;
mod keyboard;
mod lock;
mod pam_helper;
mod secret;
mod shm;
mod wallpaper;

use std::process::ExitCode;

use config::{Config, USAGE};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    if let [flag, service] = arguments.as_slice()
        && flag == "--pam-helper"
    {
        pam_helper::run(service);
    }

    let config = match Config::parse(&arguments) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("glitchlock: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match lock::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("glitchlock: {message}");
            ExitCode::FAILURE
        }
    }
}
