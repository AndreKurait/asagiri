//! CI entrypoint for the desktop migration module, using real Docker contexts.
//! Linux forwards `asagiri image import` to Docker's rootfs-import command;
//! bare `asagiri import` is reserved for desktop migration and unsupported there.
//! Build this example only for the disposable engine migration fixture.

#[cfg(target_os = "linux")]
#[path = "../src/client.rs"]
mod client;
#[cfg(target_os = "linux")]
#[path = "../src/flavor.rs"]
mod flavor;
#[cfg(target_os = "linux")]
#[path = "../src/import.rs"]
mod import;

// The importer uses client::docker_cli(), not the app's HTTP client. Reject an
// unexpected direct HTTP call instead of substituting an app diagnostic stack.
#[cfg(target_os = "linux")]
mod diag {
    pub fn op_header() -> String {
        panic!("the import fixture must communicate through the real Docker CLI");
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match import::run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This CI fixture runs only inside disposable Linux test engines.");
    std::process::exit(2);
}
