//! Microphone and meeting capture on macOS.
//!
//! * Dictation / `You`: an `AVCaptureSession` with an `AVCaptureDeviceInput`
//!   bound to the chosen device (by `uniqueID`; the system default otherwise)
//!   feeding an `AVCaptureAudioFileOutput` that writes 16 kHz mono 16-bit
//!   linear PCM WAVE. The input stays bound to that device through route
//!   changes (no default-device rebinding). Level comes from the output
//!   connection's `averagePowerLevel`.
//! * `Others`: a Core Audio process tap (macOS 14.2+) on all system output
//!   except Cantrip's own process, read through a private aggregate device on
//!   a dispatch queue, downmixed and resampled to 16 kHz PCM16 WAV.
//! * Both tracks are stamped on one monotonic clock (`Instant` origin taken
//!   at `start`); each offset is the arrival of that track's first audio.
//!
//! Microphone permission is requested only at the first recording. Not run on
//! a Mac yet; type-checked only (see `platform::macos` docs).

use super::ns;
use crate::platform::macos_logic::{self as logic, Resampler, WavWriter};
use block2::RcBlock;
use cantrip_core::ports::{
    MeetingCapture, MeetingRecording, MeetingTracks, MicError, Recorder, Recording,
};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureAudioFileOutput, AVCaptureConnection, AVCaptureDevice,
    AVCaptureDeviceInput, AVCaptureFileOutput, AVCaptureFileOutputRecordingDelegate,
    AVCaptureSession, AVFileTypeWAVE, AVMediaTypeAudio,
};
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioHardwarePropertyDefaultSystemOutputDevice,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubDeviceUIDKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat, AudioDeviceCreateIOProcIDWithBlock,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertySelector, CATapDescription, CATapMuteBehavior,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_foundation::{
    NSActivityOptions, NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol,
    NSProcessInfo, NSString, NSURL,
};
use std::ffi::{c_void, CStr};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RATE: u32 = cantrip_core::wav::SAMPLE_RATE;
/// kAudioFormatLinearPCM ('lpcm').
const FORMAT_LPCM: i64 = 0x6C70_636D;
/// kAudioFormatFlagIsFloat / kAudioFormatFlagIsNonInterleaved.
const FLAG_FLOAT: u32 = 1;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;
const SYSTEM_AUDIO_FIX: &str =
    "Allow system audio recording for Cantrip in System Settings › Privacy & Security";

// ───────────────────────────── recording delegate ─────────────────────────────

/// Events from AVFoundation's file-output callbacks.
enum RecEvent {
    Started(Instant),
    Finished(Result<(), String>),
}

struct DelegateIvars {
    tx: Mutex<Sender<RecEvent>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "CantripRecordingDelegate"]
    #[ivars = DelegateIvars]
    struct RecDelegate;

    unsafe impl NSObjectProtocol for RecDelegate {}

    unsafe impl AVCaptureFileOutputRecordingDelegate for RecDelegate {
        #[unsafe(method(captureOutput:didStartRecordingToOutputFileAtURL:fromConnections:))]
        fn did_start(
            &self,
            _output: &AVCaptureFileOutput,
            _url: &NSURL,
            _connections: &NSArray<AVCaptureConnection>,
        ) {
            if let Ok(tx) = self.ivars().tx.lock() {
                let _ = tx.send(RecEvent::Started(Instant::now()));
            }
        }

        #[unsafe(method(captureOutput:didFinishRecordingToOutputFileAtURL:fromConnections:error:))]
        fn did_finish(
            &self,
            _output: &AVCaptureFileOutput,
            _url: &NSURL,
            _connections: &NSArray<AVCaptureConnection>,
            error: Option<&NSError>,
        ) {
            let result = match error {
                None => Ok(()),
                Some(e) => Err(format!("recording error {}", e.code())),
            };
            if let Ok(tx) = self.ivars().tx.lock() {
                let _ = tx.send(RecEvent::Finished(result));
            }
        }
    }
);

impl RecDelegate {
    fn new(tx: Sender<RecEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { tx: Mutex::new(tx) });
        unsafe { msg_send![super(this), init] }
    }
}

// ───────────────────────────── microphone ─────────────────────────────

/// Ask for microphone access just in time; blocks until the user answers.
fn authorize_microphone() -> Result<(), MicError> {
    let media =
        unsafe { AVMediaTypeAudio }.ok_or(MicError::Unavailable("no audio media type".into()))?;
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    match status {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::NotDetermined => {
            let (tx, rx) = mpsc::channel::<bool>();
            let tx = Mutex::new(tx);
            let block = RcBlock::new(move |granted: objc2::runtime::Bool| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(granted.as_bool());
                }
            });
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block) };
            match rx.recv() {
                Ok(true) => Ok(()),
                _ => Err(MicError::Blocked),
            }
        }
        _ => Err(MicError::Blocked),
    }
}

fn audio_devices() -> Vec<Retained<AVCaptureDevice>> {
    let Some(media) = (unsafe { AVMediaTypeAudio }) else {
        return Vec::new();
    };
    #[allow(deprecated)]
    let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media) };
    devices.iter().collect()
}

fn find_device(id: Option<&str>) -> Result<Retained<AVCaptureDevice>, MicError> {
    match id {
        Some(id) => unsafe { AVCaptureDevice::deviceWithUniqueID(&ns(id)) }
            .ok_or_else(|| MicError::Unavailable("The chosen microphone is not connected".into())),
        None => {
            let media =
                unsafe { AVMediaTypeAudio }.ok_or(MicError::Unavailable("no audio".into()))?;
            unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media) }
                .ok_or_else(|| MicError::Unavailable("No microphone found".into()))
        }
    }
}

/// LPCM 16 kHz mono 16-bit little-endian interleaved.
fn wav_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys = [
        ns("AVFormatIDKey"),
        ns("AVSampleRateKey"),
        ns("AVNumberOfChannelsKey"),
        ns("AVLinearPCMBitDepthKey"),
        ns("AVLinearPCMIsFloatKey"),
        ns("AVLinearPCMIsBigEndianKey"),
        ns("AVLinearPCMIsNonInterleaved"),
    ];
    let values: [Retained<NSNumber>; 7] = [
        NSNumber::new_i64(FORMAT_LPCM),
        NSNumber::new_f64(RATE as f64),
        NSNumber::new_i32(1),
        NSNumber::new_i32(16),
        NSNumber::new_bool(false),
        NSNumber::new_bool(false),
        NSNumber::new_bool(false),
    ];
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let value_refs: Vec<&AnyObject> = values.iter().map(|v| v.as_ref() as &AnyObject).collect();
    NSDictionary::from_slices(&key_refs, &value_refs)
}

/// One running capture session writing a WAV file.
struct MicSession {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureAudioFileOutput>,
    _delegate: Retained<RecDelegate>,
    events: Receiver<RecEvent>,
    path: PathBuf,
}

// AVCaptureSession / file output are documented thread-safe for start/stop
// and level reads; Cantrip only calls those.
unsafe impl Send for MicSession {}

impl MicSession {
    fn start(device: Option<&str>, path: &Path) -> Result<MicSession, MicError> {
        authorize_microphone()?;
        let dev = find_device(device)?;
        let input =
            unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&dev) }.map_err(|e| {
                if e.code() == -11852 {
                    // AVErrorApplicationIsNotAuthorizedToUseDevice
                    MicError::Blocked
                } else {
                    MicError::Unavailable(format!("microphone input error {}", e.code()))
                }
            })?;
        let session = unsafe { AVCaptureSession::new() };
        let output = unsafe { AVCaptureAudioFileOutput::new() };
        unsafe {
            if !session.canAddInput(&input) {
                return Err(MicError::Unavailable("microphone busy".into()));
            }
            session.addInput(&input);
            output.setAudioSettings(Some(&wav_settings()));
            if !session.canAddOutput(&output) {
                return Err(MicError::Unavailable("cannot record audio".into()));
            }
            session.addOutput(&output);
            session.startRunning();
        }
        let (tx, events) = mpsc::channel();
        let delegate = RecDelegate::new(tx);
        let url = NSURL::fileURLWithPath(&ns(&path.to_string_lossy()));
        let file_type =
            unsafe { AVFileTypeWAVE }.ok_or(MicError::Unavailable("no WAVE type".into()))?;
        unsafe {
            output.startRecordingToOutputFileURL_outputFileType_recordingDelegate(
                &url,
                file_type,
                ProtocolObject::from_ref(&*delegate),
            );
        }
        Ok(MicSession {
            session,
            output,
            _delegate: delegate,
            events,
            path: path.to_path_buf(),
        })
    }

    fn level(&self) -> f32 {
        unsafe {
            let conns = self.output.connections();
            let Some(conn) = conns.iter().next() else {
                return 0.0;
            };
            let channels = conn.audioChannels();
            let n = channels.len();
            if n == 0 {
                return 0.0;
            }
            let sum: f32 = channels
                .iter()
                .map(|c| logic::db_to_level(c.averagePowerLevel()))
                .sum();
            sum / n as f32
        }
    }

    /// Wait for the first-sample callback (bounded) and return its instant.
    fn wait_started(&self, timeout: Duration) -> Option<Instant> {
        match self.events.recv_timeout(timeout) {
            Ok(RecEvent::Started(t)) => Some(t),
            _ => None,
        }
    }

    fn finish(self) -> Result<PathBuf, String> {
        unsafe { self.output.stopRecording() };
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(RecEvent::Finished(r)) => break r,
                Ok(RecEvent::Started(_)) => continue,
                Err(_) => break Err("recording did not finalize".to_string()),
            }
        };
        unsafe { self.session.stopRunning() };
        result.map(|_| self.path)
    }
}

pub struct MacRecorder;

impl MacRecorder {
    pub fn new() -> Self {
        MacRecorder
    }
}

impl Default for MacRecorder {
    fn default() -> Self {
        Self::new()
    }
}

struct MacRecording(MicSession);

impl Recording for MacRecording {
    fn level(&self) -> f32 {
        self.0.level()
    }

    fn stop(self: Box<Self>) -> Result<PathBuf, String> {
        self.0.finish()
    }

    fn cancel(self: Box<Self>) {
        let path = self.0.path.clone();
        let _ = self.0.finish();
        let _ = std::fs::remove_file(path);
    }
}

impl Recorder for MacRecorder {
    fn devices(&self) -> Vec<(String, String)> {
        audio_devices()
            .iter()
            .map(|d| unsafe { (d.uniqueID().to_string(), d.localizedName().to_string()) })
            .collect()
    }

    fn start(&self, device: Option<&str>, wav_path: &Path) -> Result<Box<dyn Recording>, MicError> {
        Ok(Box::new(MacRecording(MicSession::start(device, wav_path)?)))
    }
}

// ───────────────────────────── Core Audio helpers ─────────────────────────────

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read a fixed-size property.
unsafe fn get_property<T: Copy>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    qualifier: Option<&[u8]>,
    mut value: T,
) -> Result<T, i32> {
    let mut addr = address(selector);
    let mut size = std::mem::size_of::<T>() as u32;
    let (qsize, qptr) = match qualifier {
        Some(q) => (q.len() as u32, q.as_ptr() as *const c_void),
        None => (0, std::ptr::null()),
    };
    let status = AudioObjectGetPropertyData(
        object,
        NonNull::from(&mut addr),
        qsize,
        qptr,
        NonNull::from(&mut size),
        NonNull::from(&mut value).cast(),
    );
    if status == 0 {
        Ok(value)
    } else {
        Err(status)
    }
}

/// Read a CFString property (e.g. a device UID).
unsafe fn get_cfstring(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Option<String> {
    let ptr: *const CFString = get_property(object, selector, None, std::ptr::null()).ok()?;
    let s = CFRetained::from_raw(NonNull::new(ptr as *mut CFString)?);
    Some(s.to_string())
}

fn cf_key(k: &CStr) -> CFRetained<CFString> {
    CFString::from_str(k.to_str().unwrap_or_default())
}

/// Signature of a Core Audio IO block (`AudioDeviceIOBlock`).
type IoBlock = dyn Fn(
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
);

/// The process tap + private aggregate device feeding the `Others` track.
struct SystemTap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    _queue: DispatchRetained<DispatchQueue>,
    _block: RcBlock<IoBlock>,
    writer: Arc<Mutex<Option<WavWriter>>>,
    first: Arc<Mutex<Option<Instant>>>,
}

unsafe impl Send for SystemTap {}

static TAP_SEQ: AtomicU64 = AtomicU64::new(0);

impl SystemTap {
    fn start(path: &Path) -> Result<SystemTap, String> {
        unsafe {
            // Exclude Cantrip's own sounds.
            let pid = std::process::id() as i32;
            let own: AudioObjectID = get_property(
                kAudioObjectSystemObject as AudioObjectID,
                kAudioHardwarePropertyTranslatePIDToProcessObject,
                Some(&pid.to_ne_bytes()),
                0,
            )
            .unwrap_or(0);
            let exclude: Vec<Retained<NSNumber>> = if own != 0 {
                vec![NSNumber::new_u32(own)]
            } else {
                Vec::new()
            };
            let exclude = NSArray::from_retained_slice(&exclude);
            let desc = CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &exclude,
            );
            desc.setPrivate(true);
            desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
            let mut tap: AudioObjectID = 0;
            let status = AudioHardwareCreateProcessTap(Some(&desc), &mut tap);
            if status != 0 || tap == 0 {
                return Err(SYSTEM_AUDIO_FIX.to_string());
            }
            let tap_uid = desc.UUID().UUIDString().to_string();

            let output_dev: AudioObjectID = get_property(
                kAudioObjectSystemObject as AudioObjectID,
                kAudioHardwarePropertyDefaultSystemOutputDevice,
                None,
                0,
            )
            .unwrap_or(0);
            let output_uid = get_cfstring(output_dev, kAudioDevicePropertyDeviceUID);

            let agg_uid = logic::aggregate_uid(pid as u32, TAP_SEQ.fetch_add(1, Ordering::SeqCst));
            let one = CFNumber::new_i32(1);
            let tap_entry = CFDictionary::<CFString, CFType>::from_slices(
                &[&*cf_key(kAudioSubTapUIDKey)],
                &[&*CFString::from_str(&tap_uid) as &CFType],
            );
            let taps = CFArray::from_retained_objects(&[tap_entry]);
            let uid_cf = CFString::from_str(&agg_uid);
            let name_cf = CFString::from_str("Cantrip meeting audio");
            let mut keys: Vec<CFRetained<CFString>> = vec![
                cf_key(kAudioAggregateDeviceUIDKey),
                cf_key(kAudioAggregateDeviceNameKey),
                cf_key(kAudioAggregateDeviceIsPrivateKey),
                cf_key(kAudioAggregateDeviceTapAutoStartKey),
                cf_key(kAudioAggregateDeviceTapListKey),
            ];
            let mut values: Vec<CFRetained<CFType>> = vec![
                CFRetained::cast_unchecked(uid_cf),
                CFRetained::cast_unchecked(name_cf),
                CFRetained::cast_unchecked(one.clone()),
                CFRetained::cast_unchecked(one),
                CFRetained::cast_unchecked(taps),
            ];
            if let Some(out_uid) = output_uid {
                let sub = CFDictionary::<CFString, CFType>::from_slices(
                    &[&*cf_key(kAudioSubDeviceUIDKey)],
                    &[&*CFString::from_str(&out_uid) as &CFType],
                );
                keys.push(cf_key(kAudioAggregateDeviceSubDeviceListKey));
                values.push(CFRetained::cast_unchecked(CFArray::from_retained_objects(
                    &[sub],
                )));
                keys.push(cf_key(kAudioAggregateDeviceMainSubDeviceKey));
                values.push(CFRetained::cast_unchecked(CFString::from_str(&out_uid)));
            }
            let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
            let value_refs: Vec<&CFType> = values.iter().map(|v| &**v).collect();
            let description = CFDictionary::<CFString, CFType>::from_slices(&key_refs, &value_refs);
            let mut aggregate: AudioObjectID = 0;
            let status = AudioHardwareCreateAggregateDevice(
                description.as_opaque(),
                NonNull::from(&mut aggregate),
            );
            if status != 0 {
                AudioHardwareDestroyProcessTap(tap);
                return Err(format!(
                    "system audio device error {}",
                    logic::fourcc(status)
                ));
            }

            let format: AudioStreamBasicDescription = get_property(
                tap,
                kAudioTapPropertyFormat,
                None,
                std::mem::zeroed::<AudioStreamBasicDescription>(),
            )
            .unwrap_or(AudioStreamBasicDescription {
                mSampleRate: 48_000.0,
                mFormatID: FORMAT_LPCM as u32,
                mFormatFlags: FLAG_FLOAT,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            });
            if format.mFormatFlags & FLAG_FLOAT == 0 || format.mBitsPerChannel != 32 {
                AudioHardwareDestroyAggregateDevice(aggregate);
                AudioHardwareDestroyProcessTap(tap);
                return Err("unsupported system audio format".into());
            }
            let interleaved = format.mFormatFlags & FLAG_NON_INTERLEAVED == 0;
            let channels = format.mChannelsPerFrame.max(1) as usize;

            let writer = Arc::new(Mutex::new(Some(
                WavWriter::create(path, RATE)
                    .map_err(|e| format!("cannot write audio: {}", e.kind()))?,
            )));
            let first = Arc::new(Mutex::new(None::<Instant>));
            let resampler = Mutex::new(Resampler::new(format.mSampleRate, RATE as f64));
            let (w, f) = (writer.clone(), first.clone());
            let block = RcBlock::new(
                move |_now: NonNull<AudioTimeStamp>,
                      input: NonNull<AudioBufferList>,
                      _in_time: NonNull<AudioTimeStamp>,
                      _out: NonNull<AudioBufferList>,
                      _out_time: NonNull<AudioTimeStamp>| {
                    if let Ok(mut first) = f.lock() {
                        first.get_or_insert_with(Instant::now);
                    }
                    let list = input.as_ref();
                    let n = list.mNumberBuffers as usize;
                    let buffers: &[AudioBuffer] =
                        std::slice::from_raw_parts(list.mBuffers.as_ptr(), n);
                    let mut rs = match resampler.lock() {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    let samples = if interleaved {
                        match buffers.first() {
                            Some(b) if !b.mData.is_null() => {
                                let data = std::slice::from_raw_parts(
                                    b.mData as *const f32,
                                    b.mDataByteSize as usize / 4,
                                );
                                rs.process_interleaved(data, channels)
                            }
                            _ => Vec::new(),
                        }
                    } else {
                        let planes: Vec<&[f32]> = buffers
                            .iter()
                            .filter(|b| !b.mData.is_null())
                            .map(|b| {
                                std::slice::from_raw_parts(
                                    b.mData as *const f32,
                                    b.mDataByteSize as usize / 4,
                                )
                            })
                            .collect();
                        rs.process_planar(&planes)
                    };
                    if let Ok(mut guard) = w.lock() {
                        if let Some(wr) = guard.as_mut() {
                            let _ = wr.write(&samples);
                        }
                    }
                },
            );
            let queue = DispatchQueue::new("io.mistystep.cantrip.meeting-tap", None);
            let mut proc_id: AudioDeviceIOProcID = None;
            let status = AudioDeviceCreateIOProcIDWithBlock(
                NonNull::from(&mut proc_id),
                aggregate,
                Some(&queue),
                RcBlock::as_ptr(&block),
            );
            if status != 0 {
                AudioHardwareDestroyAggregateDevice(aggregate);
                AudioHardwareDestroyProcessTap(tap);
                return Err(SYSTEM_AUDIO_FIX.to_string());
            }
            let status = AudioDeviceStart(aggregate, proc_id);
            if status != 0 {
                AudioDeviceDestroyIOProcID(aggregate, proc_id);
                AudioHardwareDestroyAggregateDevice(aggregate);
                AudioHardwareDestroyProcessTap(tap);
                return Err(SYSTEM_AUDIO_FIX.to_string());
            }
            Ok(SystemTap {
                tap,
                aggregate,
                proc_id,
                _queue: queue,
                _block: block,
                writer,
                first,
            })
        }
    }

    fn first_audio(&self) -> Option<Instant> {
        self.first.lock().ok().and_then(|f| *f)
    }

    fn stop(self) -> Result<(), String> {
        unsafe {
            AudioDeviceStop(self.aggregate, self.proc_id);
            AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            AudioHardwareDestroyProcessTap(self.tap);
        }
        let writer = self
            .writer
            .lock()
            .map_err(|_| "audio writer poisoned")?
            .take();
        match writer {
            Some(w) => w
                .finish()
                .map_err(|e| format!("cannot finalize audio: {}", e.kind())),
            None => Ok(()),
        }
    }
}

// ───────────────────────────── meetings ─────────────────────────────

/// Keeps the machine from idle-sleeping while a meeting records.
struct NoIdleSleep(Retained<ProtocolObject<dyn NSObjectProtocol>>);
unsafe impl Send for NoIdleSleep {}

impl NoIdleSleep {
    fn begin() -> Self {
        let info = NSProcessInfo::processInfo();
        let token = info.beginActivityWithOptions_reason(
            NSActivityOptions::IdleSystemSleepDisabled | NSActivityOptions::UserInitiated,
            &ns("Recording a meeting"),
        );
        NoIdleSleep(token)
    }

    fn end(self) {
        unsafe { NSProcessInfo::processInfo().endActivity(&self.0) };
    }
}

pub struct MacMeetingCapture {
    _recorder: Arc<MacRecorder>,
}

impl MacMeetingCapture {
    pub fn new(recorder: Arc<MacRecorder>) -> Self {
        MacMeetingCapture {
            _recorder: recorder,
        }
    }
}

struct MacMeetingRecording {
    origin: Instant,
    you: MicSession,
    you_started: Option<Instant>,
    others: SystemTap,
    others_path: PathBuf,
    awake: NoIdleSleep,
}

fn offset_ms(origin: Instant, at: Option<Instant>) -> u64 {
    at.map(|t| t.saturating_duration_since(origin).as_millis() as u64)
        .unwrap_or(0)
}

impl MeetingRecording for MacMeetingRecording {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }

    fn stop(self: Box<Self>) -> Result<MeetingTracks, String> {
        let me = *self;
        let others_first = me.others.first_audio();
        let tap_result = me.others.stop();
        let you = me.you.finish();
        me.awake.end();
        let you = you?;
        tap_result?;
        Ok(MeetingTracks {
            you,
            others: me.others_path,
            you_offset_ms: offset_ms(me.origin, me.you_started),
            others_offset_ms: offset_ms(me.origin, others_first),
        })
    }
}

impl MeetingCapture for MacMeetingCapture {
    fn start(
        &self,
        device: Option<&str>,
        you_path: &Path,
        others_path: &Path,
    ) -> Result<Box<dyn MeetingRecording>, String> {
        let origin = Instant::now();
        let you = MicSession::start(device, you_path).map_err(|e| match e {
            MicError::Blocked => {
                "Microphone blocked. Allow Cantrip in System Settings › Privacy & Security"
                    .to_string()
            }
            MicError::Unavailable(m) => m,
        })?;
        let others = match SystemTap::start(others_path) {
            Ok(t) => t,
            Err(e) => {
                let path = you.path.clone();
                let _ = you.finish();
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
        };
        let you_started = you.wait_started(Duration::from_secs(2));
        Ok(Box::new(MacMeetingRecording {
            origin,
            you,
            you_started,
            others,
            others_path: others_path.to_path_buf(),
            awake: NoIdleSleep::begin(),
        }))
    }
}
