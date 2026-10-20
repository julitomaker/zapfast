//! The text in a picture, read on this computer with macOS's Vision framework
//! (no download, no network). Elsewhere it is unavailable.

use std::path::Path;

/// The text Vision reads in an image file, line by line, top to bottom.
#[cfg(target_os = "macos")]
pub fn read_text(path: &Path) -> Result<String, String> {
    use objc2::{AllocAnyThread, rc::Retained};
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    objc2::rc::autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let request = unsafe { VNRecognizeTextRequest::init(VNRecognizeTextRequest::alloc()) };
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        // Fixed languages: detecting them made a page take tens of seconds.
        let languages = NSArray::from_retained_slice(&[
            NSString::from_str("es-ES"),
            NSString::from_str("en-US"),
            NSString::from_str("pt-BR"),
        ]);
        request.setRecognitionLanguages(&languages);
        let options = NSDictionary::new();
        let handler = unsafe {
            VNImageRequestHandler::initWithURL_options(
                VNImageRequestHandler::alloc(),
                &url,
                &options,
            )
        };
        let as_request: Retained<VNRequest> =
            Retained::into_super(Retained::into_super(request.clone()));
        handler
            .performRequests_error(&NSArray::from_retained_slice(&[as_request]))
            .map_err(|error| {
                format!(
                    "Vision could not read the image: {}",
                    error.localizedDescription()
                )
            })?;
        let mut lines = Vec::new();
        for observation in request.results().iter().flat_map(|results| results.iter()) {
            if let Some(best) = observation.topCandidates(1).firstObject() {
                let line = best.string().to_string();
                if !line.trim().is_empty() {
                    lines.push(line);
                }
            }
        }
        Ok(lines.join("\n"))
    })
}

#[cfg(not(target_os = "macos"))]
pub fn read_text(_path: &Path) -> Result<String, String> {
    Err("Reading text in images is only available on macOS".into())
}

/// The text of a PDF, page by page, read with macOS's PDFKit. Scanned PDFs
/// have no text layer and come back empty.
#[cfg(target_os = "macos")]
pub fn pdf_text(bytes: &[u8]) -> Result<String, String> {
    use objc2::AllocAnyThread;
    use objc2_foundation::NSData;
    use objc2_pdf_kit::PDFDocument;

    objc2::rc::autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        let document = unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }
            .ok_or("This file is not a readable PDF")?;
        let text = unsafe { document.string() }
            .map(|text| text.to_string())
            .unwrap_or_default();
        Ok(text.trim().to_owned())
    })
}

#[cfg(not(target_os = "macos"))]
pub fn pdf_text(_bytes: &[u8]) -> Result<String, String> {
    Err("Reading PDFs is only available on macOS".into())
}
