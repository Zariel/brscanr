use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

use crate::{Error, Result, protocol};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Platen,
    Feeder,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_pages: u16,
    pub max_bytes: u64,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    pub job_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pages: 100,
            max_bytes: 256 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            job_timeout: Duration::from_secs(900),
        }
    }
}

/// Complete capture, returned only after page-end and job-end records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    pub source: Source,
    pub dpi: u16,
    pub pages: Vec<Page>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub file: PathBuf,
    pub lines: u32,
}

pub struct Scanner {
    address: SocketAddr,
    limits: Limits,
}

impl Scanner {
    pub fn new(address: SocketAddr, limits: Limits) -> Self {
        Self { address, limits }
    }

    /// Acquire the verified 300 dpi grayscale preset into an existing directory.
    /// Failed jobs retain complete JPEG pages and the current `.part` file.
    pub async fn scan(&self, source: Source, directory: &Path) -> Result<Capture> {
        if self.limits.max_pages == 0 || self.limits.max_bytes == 0 {
            return Err(Error::Limit("nonzero page/byte"));
        }
        timeout(self.limits.job_timeout, self.acquire(source, directory))
            .await
            .map_err(|_| Error::Timeout)?
    }

    async fn acquire(&self, source: Source, directory: &Path) -> Result<Capture> {
        let mut stream = timeout(
            self.limits.connect_timeout,
            TcpStream::connect(self.address),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        let greeting = self.read(&mut stream, 9).await?;
        if greeting != b"+OK 200\r\n" {
            return Err(protocol("device rejected session; it may be busy"));
        }
        let (settings, start) = commands(source);
        self.write(&mut stream, &settings).await?;
        let header = self.read(&mut stream, 8).await?;
        if &header[..4] != b"\0SSP" {
            return Err(protocol("unexpected settings response"));
        }
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        if size > 65536 {
            return Err(Error::Limit("settings response"));
        }
        self.read(&mut stream, size).await?;
        self.write(&mut stream, &start).await?;

        let mut capture = Capture {
            source,
            dpi: 300,
            pages: Vec::new(),
        };
        let mut current: Option<File> = None;
        let mut page_bytes = 0u64;
        let mut total = 0u64;
        let mut lines = 0;
        loop {
            let kind = self.read(&mut stream, 2).await?;
            let page = capture.pages.len() + 1;
            match kind.as_slice() {
                [0, 2] => {
                    let header = self.read(&mut stream, 12).await?;
                    let number = u16::from_le_bytes(header[..2].try_into().unwrap()) as usize;
                    let format = u16::from_le_bytes(header[2..4].try_into().unwrap());
                    let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
                    let record_lines = u32::from_le_bytes(header[8..12].try_into().unwrap());
                    if number != page || format != 0x11 || size == 0 || record_lines < lines {
                        return Err(protocol("invalid image record order or format"));
                    }
                    if page > usize::from(self.limits.max_pages) {
                        return Err(Error::Limit("page count"));
                    }
                    if size > 16 * 1024 * 1024
                        || page_bytes + size > 64 * 1024 * 1024
                        || total + size > self.limits.max_bytes
                    {
                        return Err(Error::Limit("image bytes"));
                    }
                    if current.is_none() {
                        current = Some(
                            File::create_new(directory.join(format!("page-{page:04}.part")))
                                .await?,
                        );
                    }
                    // Stream bounded chunks so malicious lengths cannot allocate a record-sized buffer.
                    let mut remaining = size;
                    while remaining > 0 {
                        let bytes = self
                            .read(&mut stream, remaining.min(65536) as usize)
                            .await?;
                        current.as_mut().unwrap().write_all(&bytes).await?;
                        remaining -= bytes.len() as u64;
                    }
                    total += size;
                    page_bytes += size;
                    lines = record_lines;
                }
                [0, 0x21] => {
                    let number = self.read(&mut stream, 2).await?;
                    if usize::from(u16::from_le_bytes(number.try_into().unwrap())) != page
                        || current.is_none()
                        || lines == 0
                    {
                        return Err(protocol("unexpected page end"));
                    }
                    let file = current.take().unwrap();
                    file.sync_all().await?;
                    drop(file);
                    let partial = directory.join(format!("page-{page:04}.part"));
                    let completed = directory.join(format!("page-{page:04}.jpg"));
                    tokio::fs::rename(&partial, &completed).await?;
                    capture.pages.push(Page {
                        file: completed,
                        lines,
                    });
                    page_bytes = 0;
                    lines = 0;
                }
                [0, 0x20] => {
                    if current.is_some() || capture.pages.is_empty() {
                        return Err(protocol("job ended without completed pages"));
                    }
                    return Ok(capture);
                }
                [0x90, 0] => return Err(Error::EmptyFeeder),
                [0x91, 0] => return Err(Error::Jam),
                _ => return Err(protocol(format!("unknown record {:02x?}", kind))),
            }
        }
    }

    async fn read(&self, stream: &mut TcpStream, size: usize) -> Result<Vec<u8>> {
        let mut bytes = vec![0; size];
        timeout(self.limits.idle_timeout, stream.read_exact(&mut bytes))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(bytes)
    }

    async fn write(&self, stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
        timeout(self.limits.idle_timeout, stream.write_all(bytes))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(())
    }
}

fn commands(source: Source) -> (Vec<u8>, Vec<u8>) {
    let (source, pages, area) = match source {
        Source::Platen => ("FB", 1, "0,0,2445,3435"),
        Source::Feeder => ("ADF", 0, "36,0,2514,3471"),
    };
    let settings = format!(
        "\x1bSSP\nOS=LNX\nPSRC={source}\nRESO=300,300\nCLR=GRAY256\nAREA=NORMAL\nMRGN=0,0,0,0\nDPLX=OFF\nBRIT=50\nCONT=50\nCOMP=JPEG\nJSF=420\nIPRC=NORMAL\nPTYPE=NORMAL\nPAGE={pages}\nLONG=OFF\nCARR=OFF\nRMGC=OFF\nDTDF=OFF\nDT4V=OFF\nDSKW=OFF\nLSMD=OFF\nRMBP=OFF\nRMMR=OFF\nGMMA=OFF\nTONE=OFF\nQTFD=OFF\nATCN=OFF\nATCRP=OFF\nUSER=\n"
    );
    let start = format!("\x1bXSC\nRESO=300,300\nAREA={area}\nMODE=NORMAL\n");
    let terminate = |s: String| {
        let mut b = s.into_bytes();
        b.push(0x80);
        b
    };
    (terminate(settings), terminate(start))
}
