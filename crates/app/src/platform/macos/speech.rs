//! Fail-closed on-device recognition with the Speech framework. Used only for
//! a finished dictation when there is no OpenRouter key or no connection.
//! Every attempt re-checks authorization and `supportsOnDeviceRecognition`,
//! sets `requiresOnDeviceRecognition = YES` and reads it back; if any check
//! fails nothing is sent anywhere and the caller keeps the recording.
//!
//! Not run on a Mac yet; type-checked only (see `platform::macos` docs).

use super::ns;
use block2::RcBlock;
use cantrip_core::ports::{LocalAvailability, LocalRecognizer};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSError, NSLocale, NSOperationQueue, NSURL};
use objc2_speech::{
    SFSpeechRecognitionResult, SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
    SFSpeechURLRecognitionRequest,
};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

/// Upper bound for one on-device pass over a dictation-length file.
const RECOGNIZE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct MacLocalRecognizer;

impl MacLocalRecognizer {
    pub fn new() -> Self {
        MacLocalRecognizer
    }
}

impl Default for MacLocalRecognizer {
    fn default() -> Self {
        Self::new()
    }
}

fn authorization() -> SFSpeechRecognizerAuthorizationStatus {
    let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
    if status != SFSpeechRecognizerAuthorizationStatus::NotDetermined {
        return status;
    }
    // Just in time: the first no-key dictation asks once.
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let block = RcBlock::new(move |s: SFSpeechRecognizerAuthorizationStatus| {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(s);
        }
    });
    unsafe { SFSpeechRecognizer::requestAuthorization(&block) };
    rx.recv()
        .unwrap_or(SFSpeechRecognizerAuthorizationStatus::Denied)
}

fn english_recognizer() -> Option<Retained<SFSpeechRecognizer>> {
    let locale = NSLocale::initWithLocaleIdentifier(NSLocale::alloc(), &ns("en-US"));
    unsafe { SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale) }
}

impl LocalRecognizer for MacLocalRecognizer {
    fn availability(&self) -> LocalAvailability {
        if authorization() != SFSpeechRecognizerAuthorizationStatus::Authorized {
            return LocalAvailability::NotAuthorized;
        }
        match english_recognizer() {
            Some(r) if unsafe { r.supportsOnDeviceRecognition() } => LocalAvailability::Ready,
            _ => LocalAvailability::NoOnDeviceEnglish,
        }
    }

    fn recognize_on_device(&self, wav: &Path) -> Result<String, String> {
        if self.availability() != LocalAvailability::Ready {
            return Err("on-device recognition unavailable".into());
        }
        let recognizer = english_recognizer().ok_or("no English recognizer")?;
        unsafe {
            if !recognizer.supportsOnDeviceRecognition() {
                return Err("on-device recognition unavailable".into());
            }
            // Results arrive on a private queue, never the main thread.
            recognizer.setQueue(&NSOperationQueue::new());
        }
        let url = NSURL::fileURLWithPath(&ns(&wav.to_string_lossy()));
        let request = unsafe {
            SFSpeechURLRecognitionRequest::initWithURL(SFSpeechURLRecognitionRequest::alloc(), &url)
        };
        unsafe {
            request.setRequiresOnDeviceRecognition(true);
            request.setShouldReportPartialResults(false);
            // Never relaxed: refuse unless the flag really holds.
            if !request.requiresOnDeviceRecognition() {
                return Err("strict on-device flag not honored".into());
            }
        }
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        let tx = Mutex::new(Some(tx));
        let handler = RcBlock::new(
            move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                let outcome = unsafe {
                    if let Some(err) = error.as_ref() {
                        Some(Err(format!("speech error {}", err.code())))
                    } else if let Some(r) = result.as_ref() {
                        r.isFinal()
                            .then(|| Ok(r.bestTranscription().formattedString().to_string()))
                    } else {
                        None
                    }
                };
                if let Some(o) = outcome {
                    if let Ok(mut slot) = tx.lock() {
                        if let Some(tx) = slot.take() {
                            let _ = tx.send(o);
                        }
                    }
                }
            },
        );
        let task =
            unsafe { recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler) };
        match rx.recv_timeout(RECOGNIZE_TIMEOUT) {
            Ok(r) => r,
            Err(_) => {
                unsafe { task.cancel() };
                Err("on-device recognition timed out".into())
            }
        }
    }
}
