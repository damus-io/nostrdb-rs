use futures::StreamExt;
use nostrdb::{Config, Filter, Ndb, Transaction};
use std::env;
use std::error::Error;
use tokio::time::{timeout, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let config = Config::new().skip_verification(true);
    let ndb = Ndb::new(&args.db_dir, &config)?;

    let mut builder = Filter::new();
    if let Some(kind) = args.kind {
        builder = builder.kinds([kind]);
    }
    if let Some(search) = &args.search {
        builder = builder.search(search);
    }
    let filter = builder.limit(args.batch as u64).build();

    let sub = ndb.subscribe(&[filter])?;
    println!(
        "subscription {} is live — open another terminal and ingest events",
        sub.id()
    );

    let mut stream = sub.stream(&ndb).notes_per_await(args.batch);

    loop {
        match timeout(Duration::from_secs(args.idle_timeout), stream.next()).await {
            Ok(Some(keys)) => {
                if keys.is_empty() {
                    continue;
                }
                let txn = Transaction::new(&ndb)?;
                for key in keys {
                    let note = ndb.get_note_by_key(&txn, key)?;
                    println!("{}", note.json()?);
                }
            }
            Ok(None) => {
                println!("subscription closed");
                break;
            }
            Err(_) => {
                println!("no new events in {}s, exiting", args.idle_timeout);
                break;
            }
        }
    }

    Ok(())
}

struct Args {
    db_dir: String,
    kind: Option<u32>,
    search: Option<String>,
    batch: u32,
    idle_timeout: u64,
}

impl Args {
    fn parse() -> Self {
        let mut db_dir = String::from("./data");
        let mut kind = None;
        let mut search = None;
        let mut batch = 32;
        let mut idle_timeout = 30;

        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--db" => db_dir = iter.next().expect("missing value for --db"),
                "--kind" => {
                    let v = iter.next().expect("missing value for --kind");
                    kind = Some(v.parse().expect("invalid kind"));
                }
                "--search" => search = Some(iter.next().expect("missing term")),
                "--batch" => {
                    let v = iter.next().expect("missing value for --batch");
                    batch = v.parse().expect("invalid batch size");
                }
                "--idle-timeout" => {
                    let v = iter.next().expect("missing value for --idle-timeout");
                    idle_timeout = v.parse().expect("invalid timeout");
                }
                "-h" | "--help" => Args::print_help(),
                _ => Args::print_help(),
            }
        }

        Args {
            db_dir,
            kind,
            search,
            batch,
            idle_timeout,
        }
    }

    fn print_help() -> ! {
        eprintln!(
            "usage: cargo run --example subscription -- [--db ./data] [--kind 1] [--search term] [--batch 32] [--idle-timeout 30]"
        );
        std::process::exit(1)
    }
}
