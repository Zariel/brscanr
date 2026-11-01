use std::{net::Ipv4Addr, path::PathBuf};

use brother_scan::{Limits, Scanner, Source, assemble_pdf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: scan <printer IPv4> <platen|feeder> <new output directory>".into());
    }
    let printer: Ipv4Addr = args[1].parse()?;
    let source = match args[2].as_str() {
        "platen" => Source::Platen,
        "feeder" => Source::Feeder,
        _ => return Err("source must be platen or feeder".into()),
    };
    let directory = PathBuf::from(&args[3]);
    tokio::fs::create_dir(&directory).await?;

    let scanner = Scanner::new((printer, 54921).into(), Limits::default());
    let capture = scanner.scan(source, &directory).await?;
    let pdf = directory.join("document.pdf");
    tokio::task::spawn_blocking(move || assemble_pdf(&capture, &pdf)).await??;
    println!("{}", directory.join("document.pdf").display());
    Ok(())
}
