use std::{net::SocketAddr, path::Path, time::Duration};

use brother_scan::{Error, Limits, Notification, Scanner, Source, assemble_pdf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn jpeg() -> Vec<u8> {
    let mut bytes = Vec::new();
    jpeg_encoder::Encoder::new(&mut bytes, 90)
        .encode(&[100; 32], 8, 4, jpeg_encoder::ColorType::Luma)
        .unwrap();
    bytes
}

fn record(page: u16, jpeg: &[u8], lines: u32) -> Vec<u8> {
    let mut bytes = vec![0, 2];
    bytes.extend(page.to_le_bytes());
    bytes.extend(0x11u16.to_le_bytes());
    bytes.extend((jpeg.len() as u32).to_le_bytes());
    bytes.extend(lines.to_le_bytes());
    bytes.extend(jpeg);
    bytes
}

async fn command(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let b = stream.read_u8().await.unwrap();
        if b == 0x80 {
            return String::from_utf8(bytes).unwrap();
        }
        bytes.push(b);
    }
}

async fn printer(bytes: Vec<u8>, source: Source) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"+OK 200\r\n").await.unwrap();
        let settings = command(&mut stream).await;
        match source {
            Source::Feeder => {
                assert!(settings.contains("PSRC=ADF\n"));
                assert!(settings.contains("PAGE=0\n"));
            }
            Source::Platen => {
                assert!(settings.contains("PSRC=FB\n"));
                assert!(settings.contains("PAGE=1\n"));
            }
        }
        assert!(settings.contains("RESO=300,300\n"));
        stream.write_all(b"\0SSP\0\0\0\0").await.unwrap();
        let start = command(&mut stream).await;
        assert!(start.starts_with("\x1bXSC\n"));
        // Deliberately split every envelope and JPEG marker across writes.
        for byte in bytes {
            if stream.write_all(&[byte]).await.is_err() {
                break;
            }
        }
    });
    (addr, task)
}

async fn scan(
    bytes: Vec<u8>,
    source: Source,
    path: &Path,
) -> brother_scan::Result<brother_scan::Capture> {
    let (addr, task) = printer(bytes, source).await;
    let result = Scanner::new(addr, Limits::default())
        .scan(source, path)
        .await;
    task.await.unwrap();
    result
}

#[tokio::test]
async fn feeder_pdf() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = Vec::new();
    let original = jpeg();
    // The device's header can claim more rows than it actually transmitted.
    let mut inflated = original.clone();
    let sof = inflated.windows(2).position(|b| b == [0xff, 0xc0]).unwrap();
    inflated[sof + 5..sof + 7].copy_from_slice(&8u16.to_be_bytes());
    for page in 1..=3 {
        bytes.extend(record(page, &inflated, 4));
        bytes.extend([0, 0x21]);
        bytes.extend(page.to_le_bytes());
    }
    bytes.extend([0, 0x20]);
    let capture = scan(bytes, Source::Feeder, directory.path()).await.unwrap();
    let pdf = directory.path().join("document.pdf");
    assemble_pdf(&capture, &pdf).unwrap();

    let parsed = lopdf::Document::load(pdf).unwrap();
    assert_eq!(parsed.get_pages().len(), 3);
    let images: Vec<_> = parsed
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| {
            s.dict.get(b"Subtype").and_then(|v| v.as_name()).ok() == Some(b"Image".as_slice())
        })
        .collect();
    assert_eq!(images.len(), 3);
    for image in images {
        assert_eq!(image.dict.get(b"Height").unwrap().as_i64().unwrap(), 4);
        assert_eq!(image.dict.get(b"Width").unwrap().as_i64().unwrap(), 8);
        assert_eq!(image.content, original);
    }
}

#[tokio::test]
async fn incomplete_jobs() {
    let jpg = jpeg();
    let mut ended_page = record(1, &jpg, 4);
    ended_page.extend([0, 0x21, 1, 0]);
    let mut jammed = ended_page.clone();
    jammed.extend([0x91, 0]);
    let mut no_page_end = record(1, &jpg, 4);
    no_page_end.extend([0, 0x20]);
    let mut repeated_page = ended_page.clone();
    repeated_page.extend(record(1, &jpg, 4));
    for (name, bytes) in [
        ("missing job end", ended_page),
        ("missing page end", no_page_end),
        ("repeated page", repeated_page),
        ("jam after page", jammed),
        ("empty job", vec![0, 0x20]),
        ("empty feeder", vec![0x90, 0]),
        (
            "oversized record",
            vec![0, 2, 1, 0, 0x11, 0, 255, 255, 255, 127, 4, 0, 0, 0],
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            scan(bytes, Source::Feeder, directory.path()).await.is_err(),
            "{name}"
        );
        assert!(!directory.path().join("document.pdf").exists());
    }
}

#[tokio::test]
async fn platen_completion_and_limits() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = record(1, &jpeg(), 4);
    bytes.extend([0, 0x21, 1, 0, 0, 0x20]);
    let capture = scan(bytes.clone(), Source::Platen, directory.path())
        .await
        .unwrap();
    assert_eq!(capture.pages.len(), 1);

    let (addr, task) = printer(bytes, Source::Platen).await;
    let limits = Limits {
        max_bytes: 10,
        ..Limits::default()
    };
    assert!(matches!(
        Scanner::new(addr, limits)
            .scan(Source::Platen, directory.path())
            .await,
        Err(Error::Limit(_))
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn stalled_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let scanner = Scanner::new(
        listener.local_addr().unwrap(),
        Limits {
            idle_timeout: Duration::from_millis(20),
            ..Limits::default()
        },
    );
    let directory = tempfile::tempdir().unwrap();
    assert!(matches!(
        scanner.scan(Source::Platen, directory.path()).await,
        Err(Error::Timeout)
    ));
}

#[test]
fn notifications() {
    let valid = b"\x02\0\x76\x30TYPE=BR;BUTTON=SCAN;USER=\"Office\";HOST=192.0.2.2:54925;FUNC=FILE;REGID=123;SEQ=1;";
    let event = Notification::parse(valid, "Office", "192.0.2.2:54925").unwrap();
    assert_eq!(event.registration_id, "123");
    assert_eq!(event.sequence, "1");
    assert!(Notification::parse(valid, "Other", "192.0.2.2:54925").is_err());
    for suffix in [
        "SEQ=2;",
        "REGID=;",
        "BAD",
        "FUNC=PRINT;",
        "X=\"broken;",
        "X=\n;",
    ] {
        let mut bytes = valid.to_vec();
        bytes.extend(suffix.as_bytes());
        assert!(
            Notification::parse(&bytes, "Office", "192.0.2.2:54925").is_err(),
            "{suffix}"
        );
    }
}
