use nostrdb::{Config, IngestMetadata, Ndb};
use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let config = Config::new().skip_verification(true);
    let ndb = Ndb::new(&args.db_dir, &config)?;

    let file = File::open(&args.input)?;
    let reader = BufReader::new(file);
    let mut processed = 0usize;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let meta = IngestMetadata::new().relay(&args.relay);
        ndb.process_event_with(&line, meta)?;
        processed += 1;
    }

    println!(
        "Imported {processed} events from {} into {}",
        args.input.display(),
        args.db_dir
    );

    Ok(())
}

struct Args {
    input: PathBuf,
    db_dir: String,
    relay: String,
}

impl Args {
    fn parse() -> Self {
        let mut input = None;
        let mut db_dir = String::from("./data");
        let mut relay = String::from("fixture");

        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--db" => db_dir = iter.next().expect("missing value for --db"),
                "--relay" => relay = iter.next().expect("missing value for --relay"),
                "-h" | "--help" => Args::print_help(),
                other if input.is_none() => input = Some(PathBuf::from(other)),
                _ => Args::print_help(),
            }
        }

        let input = input.unwrap_or_else(|| Args::print_help());
        Args {
            input,
            db_dir,
            relay,
        }
    }

    fn print_help() -> ! {
        eprintln!(
            "usage: cargo run --example ingest -- <path-to-ldjson> [--db ./data] [--relay url]"
        );
        std::process::exit(1)
    }
}
