use nostrdb::{Config, Filter, Ndb, Transaction};
use std::env;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
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
    builder = builder.limit(args.limit as u64);
    let filter = builder.build();

    let txn = Transaction::new(&ndb)?;
    let results = ndb.query(&txn, &[filter], args.limit)?;

    if results.is_empty() {
        println!("no matches");
        return Ok(());
    }

    for row in results {
        let json = row.note.json()?;
        println!("{}\n", json);
    }

    Ok(())
}

struct Args {
    db_dir: String,
    kind: Option<u32>,
    search: Option<String>,
    limit: i32,
}

impl Args {
    fn parse() -> Self {
        let mut db_dir = String::from("./data");
        let mut kind = None;
        let mut search = None;
        let mut limit = 10;

        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--db" => db_dir = iter.next().expect("missing value for --db"),
                "--kind" => {
                    let v = iter.next().expect("missing value for --kind");
                    kind = Some(v.parse().expect("invalid kind"));
                }
                "--search" => search = Some(iter.next().expect("missing term")),
                "--limit" => {
                    let v = iter.next().expect("missing value for --limit");
                    limit = v.parse().expect("invalid limit");
                }
                "-h" | "--help" => Args::print_help(),
                _ => Args::print_help(),
            }
        }

        Args {
            db_dir,
            kind,
            search,
            limit,
        }
    }

    fn print_help() -> ! {
        eprintln!("usage: cargo run --example query -- [--db ./data] [--kind 1] [--search term] [--limit 20]");
        std::process::exit(1)
    }
}
