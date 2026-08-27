use std::process::ExitCode;

fn main() -> ExitCode {
    match interdriver_platform_macos::probe_export() {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
