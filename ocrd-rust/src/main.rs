//! Private resident OCR worker for route66-snapbot.
//!
//! Snapbot owns the browser, object publication, attestation, and OCR lifecycle
//! inside one Lambda image. The old public NATS daemon transport therefore has
//! no role: the Node handler starts a fixed pool of these children, writes one
//! base64-bearing JSON request per line, and reads one JSON response per line.
//! Each child owns one warm Tesseract engine, making the child count the exact
//! OCR lane count without a second scheduler or a host-installed dependency.

mod kstream;

use base64::Engine as _;
use leptess::tesseract::TessApi;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

const VERSION: &str = concat!("snapbot-ocr-worker ", env!("CARGO_PKG_VERSION"));

/// Every OCR policy choice stays in the request so snapbot remains generic.
/// Zero values normalize to the historical defaults because Go callers encode
/// unset numeric fields as zero instead of omitting them.
#[derive(Debug, Deserialize)]
struct Request {
    image: String,
    #[serde(default = "default_psm")]
    psm: u32,
    #[serde(default = "default_lang")]
    lang: String,
    #[serde(default = "default_dpi")]
    dpi: u32,
    #[serde(default = "default_upscale")]
    upscale: u32,
    #[serde(default = "default_pixel_budget")]
    pixel_budget: u64,
}

fn default_psm() -> u32 {
    3
}
fn default_lang() -> String {
    "eng".to_string()
}
fn default_dpi() -> u32 {
    300
}
fn default_upscale() -> u32 {
    1
}
fn default_pixel_budget() -> u64 {
    20_000_000
}

impl Request {
    fn normalize(&mut self) {
        if self.psm == 0 {
            self.psm = default_psm();
        }
        if self.lang.is_empty() {
            self.lang = default_lang();
        }
        if self.dpi == 0 {
            self.dpi = default_dpi();
        }
        if self.upscale == 0 {
            self.upscale = default_upscale();
        }
        if self.pixel_budget == 0 {
            self.pixel_budget = default_pixel_budget();
        }
    }
}

/// Both fields are always present. A missing reply would look like an empty OCR
/// result at the caller, so even malformed input receives an explicit error.
#[derive(Debug, Serialize)]
struct Response {
    text: String,
    error: String,
}

impl Response {
    fn ok(text: String) -> Self {
        Self {
            text,
            error: String::new(),
        }
    }
    fn err(error: String) -> Self {
        Self {
            text: String::new(),
            error,
        }
    }

    fn payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|err| {
            format!("{{\"text\":\"\",\"error\":\"encode reply: {err}\"}}").into_bytes()
        })
    }
}

/// Reads one image with the resident engine while preserving the measured ocrd
/// preprocessing behavior. Scale one feeds the original encoded image to
/// Leptonica: tall pages made Tesseract's grayscale binarization path orders of
/// magnitude slower. Scale two or greater uses the retained kstream pipeline.
fn read_page(engine: &mut TessApi, req: &Request, raw: &[u8]) -> Result<String, String> {
    let scale = kstream::planned_scale(raw, req.upscale as usize, req.pixel_budget).unwrap_or(1);
    if scale == 1 {
        let pix = leptess::leptonica::pix_read_mem(raw)
            .map_err(|err| format!("pix_read_mem: {err:?}"))?;
        engine.set_image(&pix);
        engine.set_source_resolution(req.dpi as i32);
        return engine.get_utf8_text().map_err(|err| format!("ocr: {err}"));
    }

    let (pixels, width, height) = kstream::preprocess(raw, req.upscale as usize, req.pixel_budget)?;
    engine
        .raw
        .set_image(&pixels, width as i32, height as i32, 1, width as i32)
        .map_err(|err| format!("set_image: {err}"))?;
    engine.set_source_resolution(req.dpi as i32);
    engine.get_utf8_text().map_err(|err| format!("ocr: {err}"))
}

/// A language change is the only reason to rebuild a child's engine. Keeping
/// ownership in one slot avoids sharing a C handle across threads.
fn ensure_engine<'a>(
    slot: &'a mut Option<(String, TessApi)>,
    lang: &str,
) -> Result<&'a mut TessApi, String> {
    let rebuild = match slot {
        Some((have, _)) => have != lang,
        None => true,
    };
    if rebuild {
        let tess =
            TessApi::new(None, lang).map_err(|err| format!("init tesseract ({lang}): {err}"))?;
        *slot = Some((lang.to_string(), tess));
    }
    Ok(&mut slot.as_mut().expect("engine inserted above").1)
}

fn handle_read(engine: &mut Option<(String, TessApi)>, payload: &[u8]) -> Response {
    let mut request: Request = match serde_json::from_slice(payload) {
        Ok(request) => request,
        Err(err) => return Response::err(format!("malformed request: {err}")),
    };
    request.normalize();

    let raw = match base64::engine::general_purpose::STANDARD.decode(request.image.as_bytes()) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return Response::err("empty image".to_string()),
        Err(err) => return Response::err(format!("decode base64 image: {err}")),
    };
    let tess = match ensure_engine(engine, &request.lang) {
        Ok(engine) => engine,
        Err(err) => return Response::err(err),
    };

    // TessApi exposes the underlying tesseract-plumbing API for variables; set
    // the page segmentation mode on every request so warm state never leaks.
    if let (Ok(name), Ok(value)) = (
        std::ffi::CString::new("tessedit_pageseg_mode"),
        std::ffi::CString::new(request.psm.to_string()),
    ) {
        let _ = tess.raw.set_variable(&name, &value);
    }

    match read_page(tess, &request, &raw) {
        Ok(text) => Response::ok(text),
        Err(err) => Response::err(err),
    }
}

const USAGE: &str = "usage: snapbot-ocr-worker --stdio\n       snapbot-ocr-worker --version";

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments == ["--version"] {
        println!("{VERSION}");
        return;
    }
    if arguments == ["--help"] || arguments == ["-h"] {
        println!("{USAGE}");
        return;
    }
    if arguments != ["--stdio"] {
        eprintln!("snapbot-ocr-worker: exactly one of --stdio, --version, or --help is required");
        eprintln!("{USAGE}");
        std::process::exit(2);
    }

    // One line always yields one flushed line. Buffering without the explicit
    // flush would deadlock the parent waiting for a response still held here.
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut engine: Option<(String, TessApi)> = None;
    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) if !line.is_empty() => handle_read(&mut engine, line.as_bytes()),
            Ok(_) => Response::err("empty request line".to_string()),
            Err(err) => {
                eprintln!("snapbot-ocr-worker: read stdin: {err}");
                std::process::exit(1);
            }
        };
        if stdout.write_all(&response.payload()).is_err()
            || stdout.write_all(b"\n").is_err()
            || stdout.flush().is_err()
        {
            eprintln!("snapbot-ocr-worker: response pipe closed");
            std::process::exit(1);
        }
    }
}
