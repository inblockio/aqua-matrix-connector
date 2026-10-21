//! `aqua-seat-listen`: the push adapter. See `aqua_seat::listen` for the
//! contract (exit 0 timeout / second instance, 1 error, 2 direct address).

use std::io::Write as _;

use aqua_seat::client::SeatClient;
use aqua_seat::listen::{self, Lock, Outcome};
use aqua_seat::paths::{state_root, SeatPaths};
use aqua_seat::seatname;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "aqua-seat-listen",
    version,
    about = "Block until a direct address (DM or @mention) arrives for this seat; exit 2 then"
)]
struct Args {
    /// Seat name (default: derived like the MCP server: CLAUDE_SEAT, tmux session, ad hoc).
    #[arg(long)]
    seat: Option<String>,
    /// Give up after this many seconds (exit 0). 0 = wait indefinitely.
    #[arg(long, default_value_t = 0)]
    timeout: u64,
    /// UserPromptSubmit hook mode: print the additionalContext JSON when direct messages are unread.
    #[arg(long)]
    nudge: bool,
    /// Hook mode (implied by --nudge): never start `aqua-seatd`, and log any failure to
    /// `<state>/hook.log` and exit 0, so a seat problem never reaches the session.
    #[arg(long)]
    hook: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let hook = args.hook || args.nudge;
    let seat = match args.seat.as_deref() {
        Some(s) => match seatname::sanitize(s) {
            Some(n) => n,
            None => {
                if hook {
                    listen::log_hook_failure("-", &format!("{s:?} is not a usable seat name"));
                    return;
                }
                eprintln!("aqua-seat-listen: {s:?} is not a usable seat name");
                std::process::exit(listen::EXIT_ERROR);
            }
        },
        None => seatname::derive(&seatname::Inputs::from_env()).name,
    };
    let client = if hook {
        SeatClient::from_env().without_spawn()
    } else {
        SeatClient::from_env()
    };

    if args.nudge {
        if listen::nudge_breaker_open() {
            return;
        }
        match listen::unread_count_checked(&client, &seat).await {
            Ok(n) => {
                if let Some(out) = listen::nudge_output(n) {
                    println!("{out}");
                }
            }
            Err(e) => {
                listen::trip_nudge_breaker();
                listen::log_hook_failure(&seat, &format!("nudge: {e}"));
            }
        }
        return;
    }

    let paths = match SeatPaths::new(&state_root(), &seat) {
        Ok(p) => p,
        Err(e) => {
            if hook {
                listen::log_hook_failure(&seat, &format!("{e:#}"));
                return;
            }
            eprintln!("aqua-seat-listen: {e:#}");
            std::process::exit(listen::EXIT_ERROR);
        }
    };
    let _lock = match listen::acquire_lock(&paths) {
        Ok(Lock::Held(f)) => f,
        // Another listener already serves this seat: nothing to do.
        Ok(Lock::Busy) => return,
        Err(e) => {
            if hook {
                listen::log_hook_failure(&seat, &format!("cannot take the listener lock: {e}"));
                return;
            }
            eprintln!("aqua-seat-listen: cannot take the listener lock: {e}");
            std::process::exit(listen::EXIT_ERROR);
        }
    };

    let outcome = listen::run_with(&client, &seat, args.timeout, hook).await;
    match &outcome {
        Outcome::Wake(w) => {
            let [a, b] = listen::stderr_lines(w);
            eprintln!("{a}\n{b}");
            println!("{}", listen::stdout_line(w));
            let _ = std::io::stdout().flush();
        }
        Outcome::Timeout => {}
        Outcome::Error(e) if hook => {
            listen::log_hook_failure(&seat, e);
            return;
        }
        Outcome::Error(e) => eprintln!("aqua-seat-listen: {e}"),
    }
    std::process::exit(outcome.exit_code());
}
