//! `mulpex-ptyd --home <mulpex home>` — started by the Mulpex app, never by
//! hand. See the crate docs and `docs/ptyd.md`.

fn main() {
    let mut args = std::env::args().skip(1);
    let mut home = None;
    while let Some(a) = args.next() {
        if a == "--home" {
            home = args.next().map(std::path::PathBuf::from);
        }
    }
    let Some(home) = home else {
        eprintln!("usage: mulpex-ptyd --home <dir>");
        std::process::exit(2);
    };
    eprintln!("[ptyd] serving {}", mulpex_ptyd::daemon::describe(&home));
    if let Err(e) = mulpex_ptyd::daemon::run(home) {
        eprintln!("[ptyd] {e:#}");
        std::process::exit(1);
    }
}
