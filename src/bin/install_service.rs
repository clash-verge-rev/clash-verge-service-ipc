use anyhow::Error;
use clash_verge_service_ipc::cli::install;

fn main() -> Result<(), Error> {
    install::main()
}
