use std::process::ExitCode;

fn main() -> ExitCode {
    match interdriver_platform_macos::probe_controller() {
        Ok(()) => {
            println!("IOUSBHost controller created successfully");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
