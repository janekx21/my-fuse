use clap::Parser;
use log::info;
use my_fuse::ServerSession;

/// Custom FUSE filesystem
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the moint point of the filesystem. Example: /mnt
    mount_point: String,
}

fn main() {
    let args = Args::parse();
    pretty_env_logger::init();

    let mut server_session = ServerSession::new(args.mount_point.as_str());
    {
        let session = server_session.session.clone();

        // Handle Ctrl-C and other signals and unmount properly
        ctrlc::set_handler(move || {
            info!("Ctrl-C was pressed. Start unmounting");
            let mut session = session.write().unwrap();
            session.umount().unwrap();
        })
        .expect("Error setting Ctrl-C handler");
    }

    info!("Waiting for Ctrl-C...");
    server_session.start();
}
