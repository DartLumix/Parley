//! One-off account creation, for bootstrap and scripts. The app's
//! management page is the normal path for accounts and roles now.
//!
//! Usage: parley-adduser <data_dir> <username> <password>
//! Use the same data_dir the server's config.toml points at. Best run
//! with the server stopped; WAL usually tolerates it running, but a
//! BUSY error means: stop server, retry.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <data_dir> <username> <password>", args[0]);
        std::process::exit(2);
    }
    let db_path = std::path::Path::new(&args[1]).join("parley.db");
    let db = match parley_server::db::Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot open {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };
    if let Err(e) = db.create_user(&args[2], &args[3], false) {
        eprintln!("cannot create user: {e}");
        std::process::exit(1);
    }
    println!("user \"{}\" created ({})", args[2], db_path.display());
}
