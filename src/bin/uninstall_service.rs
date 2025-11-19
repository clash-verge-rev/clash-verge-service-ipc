use anyhow::Error;
use clash_verge_service_ipc::cli::uninstall;

fn main() -> Result<(), Error> {
    uninstall::main()
}
