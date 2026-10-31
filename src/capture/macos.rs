//! CoreAudio HAL capture without a second workflow or unsafe Send.
//!
//! Native objects stay on one owned thread. The input callback only renders,
//! validates and copies into a bounded, preallocated lock-free packet pool;
//! conversion and WAV I/O happen on that owning thread. Overruns/interruption
//! fail the take but retain its WAV.

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use cantrip_engine::audio::{
    verify_wav, InputSignal, Pcm16Converter, Pcm16WavWriter, PcmEncoding, PcmFormat, SignalMonitor,
};
use cantrip_engine::ipc::{MicrophoneAuthorization, MicrophoneGrantee};
use cantrip_engine::ports::MicrophoneAccessUnsupported;
use crossbeam_queue::ArrayQueue;
use objc2::rc::autoreleasepool;
use objc2::runtime::Bool;
use objc2_audio_toolbox::{
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioOutputUnitProperty_IsRunning, kAudioOutputUnitProperty_SetInputCallback,
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_MaximumFramesPerSlice,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit, AudioUnitAddPropertyListener, AudioUnitGetProperty,
    AudioUnitInitialize, AudioUnitRemovePropertyListenerWithUserData, AudioUnitRender,
    AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_audio::{
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, AudioBuffer,
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::{ns_string, NSBundle};
use std::cell::UnsafeCell;
use std::ffi::{c_void, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::mem::{size_of, MaybeUninit};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PACKET_FRAMES: usize = 256;
const WRITER_POLL: Duration = Duration::from_millis(2);
const DEVICE_POLL: Duration = Duration::from_millis(200);
const INTERRUPTION_GRACE: Duration = Duration::from_secs(5);
/// HAL input element (bus 1) and output element (bus 0) of an I/O unit.
const INPUT_ELEMENT: u32 = 1;
const OUTPUT_ELEMENT: u32 = 0;
/// Smallest per-callback capacity requested from the input unit.
const MIN_SLICE_FRAMES: u32 = 8_192;
/// Capture restarts allowed when the route changes before any audio arrived,
/// e.g. a Bluetooth headset switching from playback to its microphone profile.
const ROUTE_RESTARTS: u32 = 3;
const ROUTE_RETRY_DELAY: Duration = Duration::from_millis(100);
/// A Bluetooth headset settles into its microphone profile shortly after
/// capture opens it, sometimes after its first buffers. Route changes this
/// early restart capture and keep the audio already captured; later changes
/// end the take honestly.
const ROUTE_SETTLE: Duration = Duration::from_secs(2);
/// Longest skipped-I/O gap recorded as silence instead of failing the take.
const MAX_SILENCE_GAP: Duration = Duration::from_millis(250);
const FAILURE_NONE: u8 = 0;
const FAILURE_OVERRUN: u8 = 1;
const FAILURE_FORMAT: u8 = 2;
const FAILURE_CONFIGURATION: u8 = 3;
const FAILURE_DISCONTINUITY: u8 = 4;
const FAILURE_RENDER: u8 = 5;
const FIRST_SAMPLE_TIME: i64 = i64::MIN;
/// An unanswered prompt keeps its request thread at most this long; an answer
/// given later still reaches this process's authorization.
const MICROPHONE_DECISION_WAIT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    /// Stable CoreAudio UID: the value to store in config.audio_source.
    pub uid: String,
    pub name: String,
    pub is_default: bool,
}

/// This process's own authorization, as a query only: never a prompt. macOS
/// caches it per process, so only the capture owner's answer governs capture.
pub fn microphone_permission() -> MicrophoneAuthorization {
    let status = autoreleasepool(|_| unsafe {
        AVCaptureDevice::authorizationStatusForMediaType(
            AVMediaTypeAudio.expect("AVMediaTypeAudio is available on supported macOS"),
        )
    });
    match status {
        AVAuthorizationStatus::Authorized => MicrophoneAuthorization::Authorized,
        AVAuthorizationStatus::NotDetermined => MicrophoneAuthorization::NotDetermined,
        AVAuthorizationStatus::Denied => MicrophoneAuthorization::Denied,
        _ => MicrophoneAuthorization::Restricted,
    }
}

/// The capture owner's authorization and its one explicit request at a time.
/// The request awaits the operator's answer on its own thread, never the
/// engine's, and the authorization reads `Requesting` until it arrives.
pub struct MicrophonePrompt {
    request: Mutex<Option<JoinHandle<()>>>,
    grantee: MicrophoneGrantee,
}

impl Default for MicrophonePrompt {
    /// Finds who macOS grants for this process while its parents still run.
    fn default() -> Self {
        let grantee = find_grantee();
        tracing::info!(
            "[Capture] microphone grantee={}",
            match &grantee {
                MicrophoneGrantee::Cantrip => "cantrip",
                MicrophoneGrantee::Launcher(Some(_)) => "launcher",
                MicrophoneGrantee::Launcher(None) => "launcher-unknown",
            }
        );
        Self {
            request: Mutex::new(None),
            grantee,
        }
    }
}

impl MicrophonePrompt {
    pub fn authorization(&self) -> MicrophoneAuthorization {
        if self.requesting() {
            MicrophoneAuthorization::Requesting
        } else {
            microphone_permission()
        }
    }

    pub fn grantee(&self) -> &MicrophoneGrantee {
        &self.grantee
    }

    /// Ask macOS for the operator's explicit action and return at once. Only an
    /// undetermined authorization opens the system prompt; an answered one is
    /// returned as it stands.
    pub fn request(&self) -> Result<MicrophoneAuthorization> {
        let mut request = self.request.lock().unwrap_or_else(PoisonError::into_inner);
        if request.as_ref().is_some_and(|thread| !thread.is_finished()) {
            return Ok(MicrophoneAuthorization::Requesting);
        }
        let current = microphone_permission();
        if current != MicrophoneAuthorization::NotDetermined {
            return Ok(current);
        }
        // macOS terminates a process that asks without declaring its purpose.
        if autoreleasepool(|_| {
            NSBundle::mainBundle()
                .objectForInfoDictionaryKey(ns_string!("NSMicrophoneUsageDescription"))
                .is_none()
        }) {
            return Err(MicrophoneAccessUnsupported(
                "Only Cantrip.app can ask macOS for microphone access; run Cantrip from its app bundle.",
            )
            .into());
        }
        let thread = thread::Builder::new()
            .name("cantrip-microphone-access".to_owned())
            .spawn(|| autoreleasepool(|_| await_microphone_decision()))
            .context("starting the microphone access request")?;
        *request = Some(thread);
        tracing::info!("[Capture] microphone access requested");
        Ok(MicrophoneAuthorization::Requesting)
    }

    fn requesting(&self) -> bool {
        self.request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

/// Most ancestors walked looking for the app that started this process.
const MAX_ANCESTORS: usize = 32;

/// Who macOS asks and grants for this process. launchd starts Cantrip.app,
/// which then answers for itself. Any other process answers through the app
/// that started it, such as Terminal for a `cantrip daemon` run there. macOS
/// has no public call for that responsible app, so walk the parent processes
/// to the first app bundle.
fn find_grantee() -> MicrophoneGrantee {
    let mut pid = unsafe { libc::getppid() };
    if pid <= 1 {
        return MicrophoneGrantee::Cantrip;
    }
    let own = autoreleasepool(|_| PathBuf::from(NSBundle::mainBundle().bundlePath().to_string()));
    for _ in 0..MAX_ANCESTORS {
        let Some(path) = process_path(pid) else {
            break;
        };
        if let Some(app) = enclosing_app(&path) {
            return if app == own {
                MicrophoneGrantee::Cantrip
            } else {
                MicrophoneGrantee::Launcher(app_name(app))
            };
        }
        match parent_pid(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => break,
        }
    }
    MicrophoneGrantee::Launcher(None)
}

fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let length = usize::try_from(length).ok().filter(|length| *length > 0)?;
    buffer.truncate(length);
    Some(PathBuf::from(OsString::from_vec(buffer)))
}

/// The short BSD record needs no same-user check, so `login`'s root-owned
/// process between Terminal and its shell does not end the walk.
fn parent_pid(pid: libc::pid_t) -> Option<libc::pid_t> {
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    (filled == size).then(|| unsafe { info.assume_init() }.pbsi_ppid as libc::pid_t)
}

/// The outermost app bundle containing `path`, so a nested helper app counts
/// as the app that ships it.
fn enclosing_app(path: &Path) -> Option<&Path> {
    path.ancestors()
        .filter(|ancestor| ancestor.extension() == Some(OsStr::new("app")))
        .last()
}

fn app_name(app: &Path) -> Option<String> {
    app.file_stem()?.to_str().map(str::to_owned)
}

/// Block this request thread, never the engine's, until the operator answers.
fn await_microphone_decision() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let completion = RcBlock::new(move |granted: Bool| {
        let _ = sender.try_send(granted.as_bool());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(
            AVMediaTypeAudio.expect("AVMediaTypeAudio is available on supported macOS"),
            &completion,
        );
    }
    if receiver.recv_timeout(MICROPHONE_DECISION_WAIT).is_ok() {
        tracing::info!(
            "[Capture] microphone access answered state={:?}",
            microphone_permission()
        );
    } else {
        tracing::warn!("[Capture] microphone access unanswered class=microphone-request-timeout");
    }
}

pub fn input_devices() -> Result<Vec<InputDevice>> {
    autoreleasepool(|_| {
        Ok(native_devices()?
            .into_iter()
            .map(|device| device.public)
            .collect())
    })
}

/// The selected input as this process finds it: presence, not authorization,
/// which belongs to the capture owner.
pub fn diagnosis(source: Option<&str>) -> String {
    match autoreleasepool(|_| select_device(source)) {
        Ok(_) => {
            "CoreAudio input ready; hardware PCM is converted to 16 kHz mono locally".to_owned()
        }
        Err(error) => format!("{error:#}"),
    }
}

struct NativeDevice {
    id: AudioObjectID,
    public: InputDevice,
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn check_status(status: i32, operation: &'static str) -> Result<()> {
    if status != 0 {
        bail!("{operation} failed (CoreAudio status {status})");
    }
    Ok(())
}

fn audio_call<T>(call: impl FnOnce() -> Result<T>) -> Result<T> {
    // A native exception becomes a capture failure, never an abort or a
    // fallback device. NativeCapture's guarded teardown tolerates partial setup.
    objc2::exception::catch(std::panic::AssertUnwindSafe(call))
        .map_err(|_| anyhow!("native microphone API raised an audio exception"))?
}

fn property_size(object: u32, selector: u32, scope: u32) -> Result<u32> {
    let mut address = address(selector, scope);
    let mut size = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&mut address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    check_status(status, "querying input device property size")?;
    Ok(size)
}

/// Callers use only the exact POD type documented for their CoreAudio property.
unsafe fn property<T: Copy>(object: u32, selector: u32, scope: u32) -> Result<T> {
    let mut address = address(selector, scope);
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let status = AudioObjectGetPropertyData(
        object,
        NonNull::from(&mut address),
        0,
        ptr::null(),
        NonNull::from(&mut size),
        NonNull::from(&mut value).cast(),
    );
    check_status(status, "querying input device property")?;
    if size as usize != size_of::<T>() {
        bail!("input device returned an unexpected property size");
    }
    Ok(value.assume_init())
}

fn string_property(object: u32, selector: u32) -> Result<String> {
    // CoreAudio's CFString-valued properties return a caller-owned +1 object.
    let raw: *mut CFString =
        unsafe { property(object, selector, kAudioObjectPropertyScopeGlobal)? };
    let raw = NonNull::new(raw).context("input device returned no property string")?;
    let string = unsafe { CFRetained::from_raw(raw) };
    Ok(string.to_string())
}

fn native_devices() -> Result<Vec<NativeDevice>> {
    let default: AudioObjectID = unsafe {
        property(
            kAudioObjectSystemObject as u32,
            kAudioHardwarePropertyDefaultInputDevice,
            kAudioObjectPropertyScopeGlobal,
        )?
    };
    let size = property_size(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )?;
    if !(size as usize).is_multiple_of(size_of::<AudioObjectID>()) {
        bail!("CoreAudio returned an incomplete input device list");
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut ids = vec![0_u32; size as usize / size_of::<AudioObjectID>()];
    let mut address = address(
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut actual = size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as u32,
            NonNull::from(&mut address),
            0,
            ptr::null(),
            NonNull::from(&mut actual),
            NonNull::new(ids.as_mut_ptr())
                .expect("nonempty device list")
                .cast(),
        )
    };
    check_status(status, "enumerating microphone devices")?;
    if actual > size || !(actual as usize).is_multiple_of(size_of::<AudioObjectID>()) {
        bail!("CoreAudio changed the input device list during enumeration");
    }
    ids.truncate(actual as usize / size_of::<AudioObjectID>());
    let mut devices = Vec::with_capacity(ids.len());
    for id in ids {
        if property_size(
            id,
            kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeInput,
        )? == 0
        {
            continue;
        }
        let alive: u32 = unsafe {
            property(
                id,
                kAudioDevicePropertyDeviceIsAlive,
                kAudioObjectPropertyScopeGlobal,
            )?
        };
        if alive == 0 {
            continue;
        }
        devices.push(NativeDevice {
            id,
            public: InputDevice {
                uid: string_property(id, kAudioDevicePropertyDeviceUID)?,
                name: string_property(id, kAudioObjectPropertyName)?,
                is_default: id == default,
            },
        });
    }
    Ok(devices)
}

fn select_device(source: Option<&str>) -> Result<NativeDevice> {
    native_devices()?
        .into_iter()
        .find(|device| match source {
            Some(uid) => device.public.uid == uid,
            None => device.public.is_default,
        })
        .context(if source.is_some() {
            "configured microphone UID is unavailable; select an input device in Settings"
        } else {
            "no default microphone is available; select an input device in Settings"
        })
}

struct Control {
    stop_requested: AtomicBool,
    failure: AtomicU8,
    frames_seen: AtomicU64,
    next_sample_time: AtomicI64,
    /// First CoreAudio status of a failed render; an error code, never audio.
    render_status: AtomicI32,
    /// Skipped-I/O gaps recorded as silence, and their total length.
    gaps: AtomicU32,
    gap_frames: AtomicU64,
}

impl Control {
    fn new() -> Self {
        Self {
            stop_requested: AtomicBool::new(false),
            failure: AtomicU8::new(FAILURE_NONE),
            frames_seen: AtomicU64::new(0),
            next_sample_time: AtomicI64::new(FIRST_SAMPLE_TIME),
            render_status: AtomicI32::new(0),
            gaps: AtomicU32::new(0),
            gap_frames: AtomicU64::new(0),
        }
    }

    fn fail_render(&self, status: i32) {
        let _ = self
            .render_status
            .compare_exchange(0, status, Ordering::AcqRel, Ordering::Acquire);
        self.fail(FAILURE_RENDER);
    }

    fn fail(&self, failure: u8) {
        let _ = self.failure.compare_exchange(
            FAILURE_NONE,
            failure,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Forget a route change or render failure that capture is restarting
    /// from; any other latched failure stays terminal.
    fn rearm(&self) {
        let _ = self
            .failure
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |failure| {
                matches!(failure, FAILURE_CONFIGURATION | FAILURE_RENDER).then_some(FAILURE_NONE)
            });
        self.next_sample_time
            .store(FIRST_SAMPLE_TIME, Ordering::Release);
        self.render_status.store(0, Ordering::Release);
    }

    fn failure_class(&self) -> &'static str {
        match self.failure.load(Ordering::Acquire) {
            FAILURE_OVERRUN => "overrun",
            FAILURE_FORMAT => "format",
            FAILURE_CONFIGURATION => "configuration-changed",
            FAILURE_DISCONTINUITY => "discontinuity",
            FAILURE_RENDER => "render-failed",
            _ => "other",
        }
    }

    fn capture_failure(&self) -> Result<()> {
        match self.failure.load(Ordering::Acquire) {
            FAILURE_NONE => Ok(()),
            FAILURE_OVERRUN => {
                bail!("microphone transport overrun; retained audio may be incomplete")
            }
            FAILURE_FORMAT => bail!("microphone supplied an unsupported or incomplete PCM buffer"),
            FAILURE_CONFIGURATION => {
                bail!("microphone device/session configuration was interrupted")
            }
            FAILURE_DISCONTINUITY => bail!("microphone sample timeline was interrupted"),
            FAILURE_RENDER => bail!("microphone stopped delivering audio"),
            _ => bail!("microphone capture failed"),
        }
    }
}

struct Packet {
    bytes: Box<[u8]>,
    frames: usize,
}

struct Transport {
    free: ArrayQueue<Packet>,
    ready: ArrayQueue<Packet>,
}

impl Transport {
    fn new(format: PcmFormat) -> Self {
        // One second of hardware PCM plus a burst margin. Every input callback
        // takes at least one packet, so small packets keep that second for
        // HAL buffers down to PACKET_FRAMES. This is fixed for the take: even
        // a stalled disk cannot cause memory growth.
        let capacity = (format.sample_rate / PACKET_FRAMES as f64).ceil() as usize + 8;
        let transport = Self {
            free: ArrayQueue::new(capacity),
            ready: ArrayQueue::new(capacity),
        };
        for _ in 0..capacity {
            let packet = Packet {
                bytes: vec![0; PACKET_FRAMES * format.bytes_per_frame()].into_boxed_slice(),
                frames: 0,
            };
            // Pool construction is outside the callback and fills an empty queue.
            assert!(transport.free.push(packet).is_ok());
        }
        transport
    }

    fn recycle(&self, packet: Packet, control: &Control) {
        if let Err(packet) = self.free.push(packet) {
            // The fixed-pool accounting makes this unreachable. Fail closed
            // without deallocating on a realtime callback if it is violated.
            control.fail(FAILURE_OVERRUN);
            std::mem::forget(packet);
        }
    }

    /// Queue `frames` of silence where the HAL skipped I/O cycles. Realtime
    /// safe: it only zeroes preallocated packets. False after a failure.
    fn push_silence(&self, frames: usize, format: PcmFormat, control: &Control) -> bool {
        let bytes_per_frame = format.bytes_per_frame();
        let mut remaining = frames;
        while remaining > 0 {
            let Some(mut packet) = self.free.pop() else {
                control.fail(FAILURE_OVERRUN);
                return false;
            };
            packet.frames = remaining.min(PACKET_FRAMES);
            packet.bytes[..packet.frames * bytes_per_frame].fill(0);
            remaining -= packet.frames;
            if let Err(packet) = self.ready.push(packet) {
                control.fail(FAILURE_OVERRUN);
                self.recycle(packet, control);
                return false;
            }
        }
        control
            .frames_seen
            .fetch_add(frames as u64, Ordering::Release);
        true
    }

    /// The native buffer list is borrowed only for this call. Validation happens
    /// before copying, so a malformed format cannot cause an out-of-bounds read.
    ///
    /// # Safety
    /// Every non-null mData must remain readable for its advertised byte size
    /// until this call returns; the native tap owns that lifetime.
    unsafe fn copy_buffers(
        &self,
        buffers: &[AudioBuffer],
        frames: usize,
        format: PcmFormat,
        control: &Control,
    ) {
        if control.failure.load(Ordering::Acquire) != FAILURE_NONE || frames == 0 {
            return;
        }
        let planes = if format.interleaved {
            1
        } else {
            format.channels
        };
        let plane_channels = if format.interleaved {
            format.channels
        } else {
            1
        };
        let bytes_per_plane_frame = plane_channels * format.encoding.bytes_per_sample();
        let required = match frames.checked_mul(bytes_per_plane_frame) {
            Some(required) => required,
            None => {
                control.fail(FAILURE_FORMAT);
                return;
            }
        };
        if buffers.len() != planes
            || buffers.iter().any(|buffer| {
                buffer.mNumberChannels as usize != plane_channels
                    || buffer.mData.is_null()
                    || (buffer.mDataByteSize as usize) < required
            })
        {
            control.fail(FAILURE_FORMAT);
            return;
        }
        let mut start = 0;
        while start < frames {
            let Some(mut packet) = self.free.pop() else {
                control.fail(FAILURE_OVERRUN);
                return;
            };
            packet.frames = (frames - start).min(PACKET_FRAMES);
            let plane_bytes = packet.frames * bytes_per_plane_frame;
            for (plane, buffer) in buffers.iter().enumerate() {
                // SAFETY: the caller keeps mData valid throughout this call;
                // required/plane count validation above bounds both ranges.
                unsafe {
                    ptr::copy_nonoverlapping(
                        buffer.mData.cast::<u8>().add(start * bytes_per_plane_frame),
                        packet.bytes.as_mut_ptr().add(plane * plane_bytes),
                        plane_bytes,
                    );
                }
            }
            if let Err(packet) = self.ready.push(packet) {
                control.fail(FAILURE_OVERRUN);
                self.recycle(packet, control);
                return;
            }
            start += PACKET_FRAMES.min(frames - start);
        }
        control
            .frames_seen
            .fetch_add(frames as u64, Ordering::Release);
    }
}

/// Per-unit state the realtime input callback borrows. It is boxed so its
/// address is stable, and it is freed only after the unit is disposed.
struct InputContext {
    unit: AudioUnit,
    format: PcmFormat,
    capacity: usize,
    samples: UnsafeCell<Box<[f32]>>,
    transport: Arc<Transport>,
    control: Arc<Control>,
    hardware_changed: AtomicBool,
    /// Longest skipped-I/O gap, in device frames, recorded as silence.
    max_gap: i64,
}

impl InputContext {
    /// Runs on the HAL's realtime I/O thread: no allocation, locks or I/O.
    fn capture(
        &self,
        flags: NonNull<AudioUnitRenderActionFlags>,
        time: NonNull<AudioTimeStamp>,
        bus: u32,
        frames: usize,
    ) {
        let control = self.control.as_ref();
        if frames == 0 || control.failure.load(Ordering::Acquire) != FAILURE_NONE {
            return;
        }
        if frames > self.capacity {
            control.fail(FAILURE_FORMAT);
            return;
        }
        let bytes = frames * self.format.bytes_per_frame();
        // SAFETY: only this unit's single I/O thread runs the callback, and
        // the buffer outlives the unit; no other reference exists meanwhile.
        let samples = unsafe { &mut *self.samples.get() };
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: self.format.channels as u32,
                mDataByteSize: bytes as u32,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        let rendered = unsafe {
            AudioUnitRender(
                self.unit,
                flags.as_ptr(),
                time,
                bus,
                frames as u32,
                NonNull::from(&mut list),
            )
        };
        if rendered != 0 {
            control.fail_render(rendered);
            return;
        }
        let time = unsafe { time.as_ref() };
        if time.mFlags.contains(AudioTimeStampFlags::SampleTimeValid) {
            let sample_time = time.mSampleTime.round() as i64;
            let Some(next) = sample_time.checked_add(frames as i64) else {
                control.fail(FAILURE_DISCONTINUITY);
                return;
            };
            let expected = control.next_sample_time.swap(next, Ordering::AcqRel);
            if expected != FIRST_SAMPLE_TIME && expected != sample_time {
                // An overloaded HAL skips whole I/O cycles. Keep the take's
                // timeline by recording a short skip as silence and counting
                // it; anything longer or backwards is a real discontinuity.
                let gap = sample_time.saturating_sub(expected);
                if gap <= 0 || gap > self.max_gap {
                    control.fail(FAILURE_DISCONTINUITY);
                    return;
                }
                if !self
                    .transport
                    .push_silence(gap as usize, self.format, control)
                {
                    return;
                }
                control.gaps.fetch_add(1, Ordering::AcqRel);
                control.gap_frames.fetch_add(gap as u64, Ordering::AcqRel);
            }
        }
        // SAFETY: the unit rendered into our own buffer, which stays valid
        // and unaliased until this callback returns.
        unsafe {
            self.transport
                .copy_buffers(&list.mBuffers, frames, self.format, control)
        };
    }
}

unsafe extern "C-unwind" fn capture_input(
    context: NonNull<c_void>,
    flags: NonNull<AudioUnitRenderActionFlags>,
    time: NonNull<AudioTimeStamp>,
    bus: u32,
    frames: u32,
    _data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: registered with a pointer to the unit's boxed InputContext,
    // which outlives every callback from that unit.
    let context = unsafe { context.cast::<InputContext>().as_ref() };
    context.capture(flags, time, bus, frames as usize);
    0
}

unsafe extern "C-unwind" fn hardware_format_changed(
    context: NonNull<c_void>,
    _unit: AudioUnit,
    _property: u32,
    scope: u32,
    element: u32,
) {
    if scope == kAudioUnitScope_Input && element == INPUT_ELEMENT {
        // SAFETY: as for capture_input; the listener is removed before disposal.
        let context = unsafe { context.cast::<InputContext>().as_ref() };
        context.hardware_changed.store(true, Ordering::Release);
    }
}

/// A CoreAudio HAL input unit bound to exactly one device. Unlike
/// AVAudioEngine it has no output side and no default-device aggregate, so
/// nothing rebinds it to the system default while a Bluetooth route settles.
/// Constructed, used and destroyed on the capture thread.
struct NativeCapture {
    unit: AudioUnit,
    context: Option<Box<InputContext>>,
    listening: bool,
    initialized: bool,
    stopped: bool,
}

impl NativeCapture {
    fn new(device: AudioObjectID) -> Result<Self> {
        let description = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let component =
            unsafe { AudioComponentFindNext(ptr::null_mut(), NonNull::from(&description)) };
        if component.is_null() {
            bail!("CoreAudio has no microphone input unit");
        }
        let mut unit: AudioUnit = ptr::null_mut();
        check_status(
            unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) },
            "creating microphone input unit",
        )?;
        let capture = Self {
            unit,
            context: None,
            listening: false,
            initialized: false,
            stopped: true,
        };
        // Input must be enabled before a device is bound to the unit.
        capture.set(
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            INPUT_ELEMENT,
            &1_u32,
            "enabling microphone input",
        )?;
        capture.set(
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            OUTPUT_ELEMENT,
            &0_u32,
            "disabling audio output",
        )?;
        let selected = capture.set_status(
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            0,
            &device,
        );
        if selected != 0 {
            tracing::warn!("[Capture] selecting microphone failed status={selected}");
        }
        check_status(selected, "selecting configured microphone")?;
        if capture.current_device()? != device {
            bail!("CoreAudio did not select the requested microphone");
        }
        Ok(capture)
    }

    fn set_status<T: Copy>(&self, property: u32, scope: u32, element: u32, value: &T) -> i32 {
        unsafe {
            AudioUnitSetProperty(
                self.unit,
                property,
                scope,
                element,
                ptr::from_ref(value).cast(),
                size_of::<T>() as u32,
            )
        }
    }

    fn set<T: Copy>(
        &self,
        property: u32,
        scope: u32,
        element: u32,
        value: &T,
        operation: &'static str,
    ) -> Result<()> {
        check_status(self.set_status(property, scope, element, value), operation)
    }

    /// Callers use only the exact POD type documented for the property.
    fn get<T: Copy>(
        &self,
        property: u32,
        scope: u32,
        element: u32,
        operation: &'static str,
    ) -> Result<T> {
        let mut value = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        let status = unsafe {
            AudioUnitGetProperty(
                self.unit,
                property,
                scope,
                element,
                NonNull::from(&mut value).cast(),
                NonNull::from(&mut size),
            )
        };
        check_status(status, operation)?;
        if size as usize != size_of::<T>() {
            bail!("{operation} returned an unexpected size");
        }
        // SAFETY: CoreAudio filled exactly size_of::<T>() bytes of a POD value.
        Ok(unsafe { value.assume_init() })
    }

    fn current_device(&self) -> Result<AudioObjectID> {
        self.get(
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            0,
            "checking current microphone",
        )
    }

    fn is_running(&self) -> bool {
        self.get::<u32>(
            kAudioOutputUnitProperty_IsRunning,
            kAudioUnitScope_Global,
            0,
            "checking microphone capture",
        )
        .is_ok_and(|running| running != 0)
    }

    /// The device's own format. Capture asks the unit for the same rate and
    /// channels as packed interleaved Float32, so the unit never resamples.
    fn format(&self) -> Result<PcmFormat> {
        let hardware: AudioStreamBasicDescription = self.get(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            INPUT_ELEMENT,
            "reading microphone format",
        )?;
        let format = PcmFormat {
            sample_rate: hardware.mSampleRate,
            channels: hardware.mChannelsPerFrame as usize,
            encoding: PcmEncoding::Float32,
            interleaved: true,
        };
        format.validate()?;
        Ok(format)
    }

    /// Whether the device's rate or channel count no longer matches `format`.
    /// Checked when the unit reports a hardware format change, and on polls.
    fn hardware_changed(&self, format: PcmFormat, polling: bool) -> bool {
        let notified = self
            .context
            .as_ref()
            .is_some_and(|context| context.hardware_changed.swap(false, Ordering::AcqRel));
        if !notified && !polling {
            return false;
        }
        match self.format() {
            Ok(current) => {
                current.sample_rate != format.sample_rate || current.channels != format.channels
            }
            Err(_) => true,
        }
    }

    fn start(
        &mut self,
        format: PcmFormat,
        transport: Arc<Transport>,
        control: Arc<Control>,
    ) -> Result<()> {
        if self.context.is_some() {
            bail!("microphone input unit was already started");
        }
        self.stopped = false;
        let frame_bytes = format.bytes_per_frame() as u32;
        let client = AudioStreamBasicDescription {
            mSampleRate: format.sample_rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: frame_bytes,
            mFramesPerPacket: 1,
            mBytesPerFrame: frame_bytes,
            mChannelsPerFrame: format.channels as u32,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        self.set(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            INPUT_ELEMENT,
            &client,
            "setting microphone sample format",
        )?;
        let mut capacity: u32 = self.get(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            "reading microphone buffer size",
        )?;
        if capacity < MIN_SLICE_FRAMES {
            self.set(
                kAudioUnitProperty_MaximumFramesPerSlice,
                kAudioUnitScope_Global,
                0,
                &MIN_SLICE_FRAMES,
                "sizing microphone buffer",
            )?;
            capacity = MIN_SLICE_FRAMES;
        }
        let capacity = capacity as usize;
        let context = self.context.insert(Box::new(InputContext {
            unit: self.unit,
            format,
            capacity,
            samples: UnsafeCell::new(vec![0.0; capacity * format.channels].into_boxed_slice()),
            max_gap: (format.sample_rate * MAX_SILENCE_GAP.as_secs_f64()).round() as i64,
            transport,
            control,
            hardware_changed: AtomicBool::new(false),
        }));
        let context = ptr::from_ref(context.as_ref()).cast_mut().cast::<c_void>();
        self.set(
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            0,
            &AURenderCallbackStruct {
                inputProc: Some(capture_input),
                inputProcRefCon: context,
            },
            "installing microphone callback",
        )?;
        check_status(
            unsafe {
                AudioUnitAddPropertyListener(
                    self.unit,
                    kAudioUnitProperty_StreamFormat,
                    Some(hardware_format_changed),
                    context,
                )
            },
            "observing microphone format",
        )?;
        self.listening = true;
        check_status(
            unsafe { AudioUnitInitialize(self.unit) },
            "initializing microphone input",
        )?;
        self.initialized = true;
        let started = unsafe { AudioOutputUnitStart(self.unit) };
        if started != 0 {
            tracing::warn!("[Capture] microphone engine start failed code={started}");
        }
        check_status(started, "starting microphone capture")
    }

    fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        let stopped = check_status(
            unsafe { AudioOutputUnitStop(self.unit) },
            "stopping microphone capture",
        );
        let unobserved = if self.listening {
            self.listening = false;
            let context = self.context.as_deref().map_or(ptr::null_mut(), |context| {
                ptr::from_ref(context).cast_mut().cast::<c_void>()
            });
            check_status(
                unsafe {
                    AudioUnitRemovePropertyListenerWithUserData(
                        self.unit,
                        kAudioUnitProperty_StreamFormat,
                        Some(hardware_format_changed),
                        context,
                    )
                },
                "removing microphone format observer",
            )
        } else {
            Ok(())
        };
        let uninitialized = if self.initialized {
            self.initialized = false;
            check_status(
                unsafe { AudioUnitUninitialize(self.unit) },
                "releasing microphone input",
            )
        } else {
            Ok(())
        };
        let result = stopped.and(unobserved).and(uninitialized);
        self.stopped = result.is_ok();
        result
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        if self.stop().is_err() {
            tracing::warn!("[Capture] native audio teardown failed; original WAV retained");
        }
        // Disposal ends every callback; the boxed context is freed only after.
        unsafe { AudioComponentInstanceDispose(self.unit) };
    }
}

fn drain_packets(
    transport: &Transport,
    format: PcmFormat,
    converter: &mut Pcm16Converter,
    writer: &mut Pcm16WavWriter<File>,
    control: &Control,
) -> Result<bool> {
    let mut wrote = false;
    for _ in 0..transport.ready.capacity() {
        let Some(packet) = transport.ready.pop() else {
            break;
        };
        let count = packet.frames * format.bytes_per_frame();
        let result = converter.write_pcm(&packet.bytes[..count], packet.frames, writer);
        transport.recycle(packet, control);
        result?;
        wrote = true;
    }
    Ok(wrote)
}

/// A rejected native start is not an accepted take. Remove only our empty WAV
/// header, never captured PCM or a file replaced at the pathname.
fn remove_empty_failed_start(path: &Path, file: &File, frames_seen: u64) -> Result<()> {
    if frames_seen != 0 {
        return Ok(());
    }
    let owned = file.metadata().context("checking rejected capture WAV")?;
    if owned.len() != 44 {
        return Ok(());
    }
    let current = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("checking rejected capture pathname"),
    };
    if current.is_file() && current.dev() == owned.dev() && current.ino() == owned.ino() {
        std::fs::remove_file(path).context("removing empty rejected capture header")?;
    }
    Ok(())
}

/// A capture that ends early names its class in the log; never audio or text.
fn ended(class: &'static str, message: &'static str) -> anyhow::Error {
    tracing::warn!("[Capture] microphone capture failed class={class}");
    anyhow!(message)
}

struct Restarted {
    native: NativeCapture,
    format: PcmFormat,
    transport: Arc<Transport>,
    converter: Pcm16Converter,
}

/// Restart capture of `device` on a fresh input unit while its route settles,
/// reading the format the device settled on. `None` means stop or cancel
/// arrived first; the audio already kept is the whole take.
fn restart_capture(
    device: AudioObjectID,
    control: &Arc<Control>,
    restarts: &mut u32,
) -> Result<Option<Restarted>> {
    loop {
        if control.stop_requested.load(Ordering::Acquire) {
            // The route change lost no kept audio; this is an ordinary stop.
            control.rearm();
            return Ok(None);
        }
        *restarts += 1;
        tracing::info!(
            "[Capture] microphone route changed while settling; restarting attempt={restarts}"
        );
        let attempt = (|| -> Result<Restarted> {
            let mut native = audio_call(|| NativeCapture::new(device))?;
            let format = audio_call(|| native.format())?;
            let transport = Arc::new(Transport::new(format));
            let converter = Pcm16Converter::new(format)?;
            control.rearm();
            audio_call(|| native.start(format, Arc::clone(&transport), Arc::clone(control)))?;
            Ok(Restarted {
                native,
                format,
                transport,
                converter,
            })
        })();
        match attempt {
            Ok(restarted) => return Ok(Some(restarted)),
            // The route may still be settling; retry within the same budget.
            // A stop request unparks this wait and ends the take above.
            Err(_)
                if *restarts < ROUTE_RESTARTS || control.stop_requested.load(Ordering::Acquire) =>
            {
                thread::park_timeout(ROUTE_RETRY_DELAY);
            }
            Err(error) => {
                tracing::warn!("[Capture] microphone capture failed class=restart-failed");
                return Err(error);
            }
        }
    }
}

fn capture_to_wav(
    path: &Path,
    source: Option<&str>,
    control: &Arc<Control>,
    ready: &mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<()> {
    let permission = microphone_permission();
    if permission != MicrophoneAuthorization::Authorized {
        bail!("microphone access is not authorized: {permission:?}");
    }
    let selected = select_device(source).inspect_err(|_| {
        tracing::warn!(
            "[Capture] microphone unavailable configured={}",
            source.is_some()
        );
    })?;
    let mut native = audio_call(|| NativeCapture::new(selected.id))?;
    let mut format = audio_call(|| native.format())?;
    let mut transport = Arc::new(Transport::new(format));
    let mut converter = Pcm16Converter::new(format)?;
    // create_new is essential: a stale or recovered only copy is never replaced.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .context("creating retained microphone WAV")?;
    let mut writer = Pcm16WavWriter::new(file)?;
    let mut next_device_poll = Instant::now();
    let mut last_frames = 0;
    let mut last_audio = Instant::now();
    let mut started = false;
    let mut restarts = 0;
    let mut opened = Instant::now();
    let recording = (|| -> Result<()> {
        audio_call(|| native.start(format, Arc::clone(&transport), Arc::clone(control)))?;
        started = true;
        opened = Instant::now();
        if ready.send(Ok(())).is_err() {
            control.stop_requested.store(true, Ordering::Release);
        }
        while !control.stop_requested.load(Ordering::Acquire) {
            if drain_packets(&transport, format, &mut converter, &mut writer, control)? {
                writer.checkpoint()?;
            }
            let polling = Instant::now() >= next_device_poll;
            // A route change shows as a new device format, a failed render or
            // a unit that stopped itself. Any other latched failure is terminal.
            if native.hardware_changed(format, polling) {
                control.fail(FAILURE_CONFIGURATION);
            }
            let failure = control.failure.load(Ordering::Acquire);
            let route_changed = matches!(failure, FAILURE_CONFIGURATION | FAILURE_RENDER)
                || (failure == FAILURE_NONE && polling && !native.is_running());
            if route_changed && restarts < ROUTE_RESTARTS {
                // A Bluetooth headset switching to its microphone profile (or
                // another route change) stops the engine just after start.
                // While the route settles, restart capture and keep the WAV.
                let stopped = audio_call(|| native.stop());
                let alive = unsafe {
                    property::<u32>(
                        selected.id,
                        kAudioDevicePropertyDeviceIsAlive,
                        kAudioObjectPropertyScopeGlobal,
                    )
                };
                if !matches!(alive, Ok(alive) if alive != 0) {
                    return Err(ended(
                        "device-changed",
                        "selected microphone disconnected or changed during capture",
                    ));
                }
                if let Err(error) = stopped {
                    tracing::warn!("[Capture] microphone capture failed class=stop-failed");
                    return Err(error);
                }
                let captured = control.frames_seen.load(Ordering::Acquire);
                if captured == 0 || opened.elapsed() < ROUTE_SETTLE {
                    let trigger = match failure {
                        FAILURE_CONFIGURATION => "configuration-changed",
                        FAILURE_RENDER => "render-failed",
                        _ => "engine-stopped",
                    };
                    tracing::info!(
                        "[Capture] microphone route change detected trigger={trigger} status={} captured_ms={}",
                        control.render_status.load(Ordering::Acquire),
                        (captured as f64 / format.sample_rate * 1_000.0).round()
                    );
                    // Keep what the old route captured: the WAV stays 16 kHz
                    // mono, and each hardware format gets its own converter.
                    drain_packets(&transport, format, &mut converter, &mut writer, control)?;
                    converter.finish(&mut writer)?;
                    writer.checkpoint()?;
                    let Some(restarted) = restart_capture(selected.id, control, &mut restarts)?
                    else {
                        return Ok(());
                    };
                    native = restarted.native;
                    format = restarted.format;
                    transport = restarted.transport;
                    converter = restarted.converter;
                    last_audio = Instant::now();
                    continue;
                }
            }
            if let Err(error) = control.capture_failure() {
                tracing::warn!(
                    "[Capture] microphone capture failed class={} status={}",
                    control.failure_class(),
                    control.render_status.load(Ordering::Acquire)
                );
                return Err(error);
            }
            let now = Instant::now();
            let frames = control.frames_seen.load(Ordering::Acquire);
            if frames != last_frames {
                last_frames = frames;
                last_audio = now;
            } else if now.duration_since(last_audio) >= INTERRUPTION_GRACE {
                return Err(ended(
                    "no-audio",
                    "microphone stopped supplying audio; retained take is incomplete",
                ));
            }
            if polling {
                if microphone_permission() != MicrophoneAuthorization::Authorized {
                    return Err(ended(
                        "permission-revoked",
                        "microphone permission was revoked during capture",
                    ));
                }
                // A device query that fails means the device went away.
                let alive = unsafe {
                    property::<u32>(
                        selected.id,
                        kAudioDevicePropertyDeviceIsAlive,
                        kAudioObjectPropertyScopeGlobal,
                    )
                };
                let bound = native.current_device();
                if !matches!(alive, Ok(alive) if alive != 0)
                    || !matches!(bound, Ok(bound) if bound == selected.id)
                {
                    return Err(ended(
                        "device-changed",
                        "selected microphone disconnected or changed during capture",
                    ));
                }
                if !native.is_running() {
                    return Err(ended(
                        "engine-stopped",
                        "microphone engine stopped during capture",
                    ));
                }
                next_device_poll = now + DEVICE_POLL;
            }
            thread::park_timeout(WRITER_POLL);
        }
        Ok(())
    })();
    let gaps = control.gaps.load(Ordering::Acquire);
    if gaps != 0 {
        tracing::warn!(
            "[Capture] microphone skipped audio recorded as silence gaps={gaps} silence_ms={}",
            (control.gap_frames.load(Ordering::Acquire) as f64 / format.sample_rate * 1_000.0)
                .round()
        );
    }
    // Stop/remove the native tap before draining the final bounded queue. No
    // callback can append after finalization, including on an abandoned take.
    let stopped = native.stop();
    let drained = drain_packets(&transport, format, &mut converter, &mut writer, control);
    let conversion = converter.finish(&mut writer);
    let finalization = writer.finish();
    let durable = writer
        .get_ref()
        .sync_all()
        .context("syncing retained microphone WAV");
    let cleanup = if !started {
        remove_empty_failed_start(
            path,
            writer.get_ref(),
            control.frames_seen.load(Ordering::Acquire),
        )
    } else {
        Ok(())
    };
    // Evaluate every finalization step even after an earlier error. The first
    // failure still reaches the engine, so incomplete audio is never sent as a
    // successful take and never silently retried through another capture device.
    recording
        .and(control.capture_failure())
        .and(stopped)
        .and(drained.map(|_| ()))
        .and(conversion)
        .and(finalization)
        .and(durable)
        .and(cleanup)
}

/// Send by construction: only Rust data, atomics and a JoinHandle cross the
/// engine/worker boundary. Native objects are neither stored here nor marked Send.
pub struct Recorder {
    wav_path: PathBuf,
    control: Arc<Control>,
    worker: Option<JoinHandle<Result<()>>>,
    signal_monitor: SignalMonitor,
    signal_monitor_warned: bool,
}

impl Recorder {
    pub fn start(wav_path: &Path, source: Option<&str>) -> Result<Self> {
        let permission = microphone_permission();
        if permission != MicrophoneAuthorization::Authorized {
            tracing::warn!("[Capture] microphone permission not authorized state={permission:?}");
            bail!("microphone access is not authorized: {permission:?}");
        }
        let started_at = Instant::now();
        let control = Arc::new(Control::new());
        let capture_control = Arc::clone(&control);
        let path = wav_path.to_path_buf();
        let source = source.map(str::to_owned);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("cantrip-microphone".to_owned())
            .spawn(move || {
                let result = autoreleasepool(|_| {
                    capture_to_wav(&path, source.as_deref(), &capture_control, &ready_tx)
                });
                if let Err(error) = &result {
                    let _ = ready_tx.try_send(Err(format!("{error:#}")));
                }
                result
            })
            .context("starting native microphone worker")?;
        let mut recorder = Self {
            wav_path: wav_path.to_path_buf(),
            control,
            worker: Some(worker),
            signal_monitor: SignalMonitor::new(started_at),
            signal_monitor_warned: false,
        };
        match ready_rx.recv() {
            Ok(Ok(())) => {
                tracing::info!("[Capture] microphone recording started");
                Ok(recorder)
            }
            Ok(Err(error)) => {
                let _ = recorder.finish_stop();
                Err(anyhow!(error))
            }
            Err(_) => {
                recorder.finish_stop()?;
                bail!("microphone worker ended before capture became ready");
            }
        }
    }

    pub fn input_signal(&mut self) -> Option<InputSignal> {
        match self.signal_monitor.sample(&self.wav_path, Instant::now()) {
            Ok(signal) => signal,
            Err(_) => {
                if !self.signal_monitor_warned {
                    tracing::warn!(
                        "[Capture] input signal monitor unavailable class=storage-failed"
                    );
                    self.signal_monitor_warned = true;
                }
                None
            }
        }
    }

    /// Nonblocking/idempotent stop request; the owned writer finalizes in order.
    pub fn request_stop(&mut self) -> Result<()> {
        self.control.stop_requested.store(true, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
        Ok(())
    }

    fn finish_stop(&mut self) -> Result<()> {
        self.request_stop()?;
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
                anyhow!("native microphone worker panicked; original WAV retained")
            })??;
        }
        Ok(())
    }

    pub fn stop(mut self) -> Result<PathBuf> {
        self.finish_stop()?;
        verify_wav(&self.wav_path)?;
        tracing::info!("[Capture] microphone recording stopped");
        Ok(std::mem::take(&mut self.wav_path))
    }
}

impl cantrip_engine::ports::Recorder for Recorder {
    fn input_signal(&mut self) -> Option<InputSignal> {
        Recorder::input_signal(self)
    }

    fn request_stop(&mut self) -> Result<()> {
        Recorder::request_stop(self)
    }

    fn stop(self: Box<Self>) -> Result<PathBuf> {
        Recorder::stop(*self)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.worker.is_some() && self.finish_stop().is_err() {
            tracing::warn!(
                "[Capture] abandoned microphone take retained after capture/finalization failure"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    struct WavFixture(PathBuf);

    impl WavFixture {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "cantrip-start-failure-{}-{nonce}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .unwrap();
            Self(root)
        }

        fn writer(&self, name: &str) -> Pcm16WavWriter<File> {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.0.join(name))
                .unwrap();
            Pcm16WavWriter::new(file).unwrap()
        }
    }

    impl Drop for WavFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_launching_app_is_the_outermost_bundle_on_its_executable_path() {
        let app = |path: &str| enclosing_app(Path::new(path)).and_then(app_name);
        assert_eq!(
            app("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal").as_deref(),
            Some("Terminal")
        );
        assert_eq!(
            app("/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper")
                .as_deref(),
            Some("Visual Studio Code")
        );
        assert_eq!(app("/bin/zsh"), None);
        assert_eq!(app("/usr/bin/login"), None);
    }

    #[test]
    fn rejected_start_removes_empty_headers_but_preserves_any_audio_prefix() {
        let fixture = WavFixture::new();
        let empty = fixture.writer("empty.wav");
        let empty_path = fixture.0.join("empty.wav");
        remove_empty_failed_start(&empty_path, empty.get_ref(), 0).unwrap();
        assert!(!empty_path.exists());

        let mut prefix = fixture.writer("prefix.wav");
        prefix.write_samples(&[123, -321]).unwrap();
        prefix.finish().unwrap();
        let prefix_path = fixture.0.join("prefix.wav");
        remove_empty_failed_start(&prefix_path, prefix.get_ref(), 0).unwrap();
        let samples = hound::WavReader::open(&prefix_path)
            .unwrap()
            .into_samples::<i16>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(samples, [123, -321]);

        let pending = fixture.writer("pending.wav");
        let pending_path = fixture.0.join("pending.wav");
        let original = std::fs::read(&pending_path).unwrap();
        remove_empty_failed_start(&pending_path, pending.get_ref(), 1).unwrap();
        assert_eq!(std::fs::read(&pending_path).unwrap(), original);
    }

    #[test]
    fn rejected_start_never_removes_a_replaced_pathname() {
        let fixture = WavFixture::new();
        let original = fixture.writer("recording.wav");
        let path = fixture.0.join("recording.wav");
        std::fs::rename(&path, fixture.0.join("original.wav")).unwrap();
        let mut replacement = fixture.writer("recording.wav");
        replacement.write_samples(&[789]).unwrap();
        replacement.finish().unwrap();
        remove_empty_failed_start(&path, original.get_ref(), 0).unwrap();
        assert_eq!(
            hound::WavReader::open(&path)
                .unwrap()
                .into_samples::<i16>()
                .next()
                .unwrap()
                .unwrap(),
            789,
        );
    }

    fn mono_format() -> PcmFormat {
        PcmFormat {
            sample_rate: 16_000.0,
            channels: 1,
            encoding: PcmEncoding::Signed16,
            interleaved: true,
        }
    }

    #[test]
    fn rearm_forgets_only_route_failures_before_audio() {
        for terminal in [FAILURE_OVERRUN, FAILURE_FORMAT, FAILURE_DISCONTINUITY] {
            let control = Control::new();
            control.fail(terminal);
            control.rearm();
            assert_eq!(control.failure.load(Ordering::Acquire), terminal);
            assert!(control.capture_failure().is_err());
        }
        let control = Control::new();
        control.fail(FAILURE_CONFIGURATION);
        control.rearm();
        assert!(control.capture_failure().is_ok());
        control.fail_render(-10863);
        assert_eq!(control.failure_class(), "render-failed");
        control.rearm();
        assert!(control.capture_failure().is_ok());
        assert_eq!(control.render_status.load(Ordering::Acquire), 0);
    }

    #[test]
    fn stop_during_a_route_restart_ends_as_an_ordinary_stop() {
        let control = Arc::new(Control::new());
        control.fail(FAILURE_CONFIGURATION);
        control.stop_requested.store(true, Ordering::Release);
        let mut restarts = 0;
        // Stop is honored before any device is touched.
        let restarted = restart_capture(0, &control, &mut restarts).unwrap();
        assert!(restarted.is_none());
        assert_eq!(restarts, 0);
        assert!(control.capture_failure().is_ok());
    }

    #[test]
    fn transport_overrun_is_explicit_and_keeps_the_queued_prefix() {
        let format = mono_format();
        let transport = Transport::new(format);
        let control = Control::new();
        let mut samples = vec![123_i16; PACKET_FRAMES];
        let buffer = AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: (samples.len() * 2) as u32,
            mData: samples.as_mut_ptr().cast(),
        };
        for _ in 0..transport.ready.capacity() {
            unsafe { transport.copy_buffers(&[buffer], PACKET_FRAMES, format, &control) };
        }
        assert!(control.capture_failure().is_ok());
        unsafe { transport.copy_buffers(&[buffer], PACKET_FRAMES, format, &control) };
        assert_eq!(control.failure.load(Ordering::Acquire), FAILURE_OVERRUN);
        assert_eq!(transport.ready.len(), transport.ready.capacity());
        while let Some(packet) = transport.ready.pop() {
            assert_eq!(packet.frames, PACKET_FRAMES);
            assert!(packet
                .bytes
                .as_chunks::<2>()
                .0
                .iter()
                .all(|bytes| *bytes == 123_i16.to_ne_bytes()));
        }
    }

    #[test]
    fn transport_rejects_incomplete_frames_without_reading_outside_buffer() {
        let format = mono_format();
        let transport = Transport::new(format);
        let control = Control::new();
        let mut sample = 1_i16;
        let buffer = AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: 1,
            mData: ptr::from_mut(&mut sample).cast(),
        };
        unsafe { transport.copy_buffers(&[buffer], 1, format, &control) };
        assert_eq!(control.failure.load(Ordering::Acquire), FAILURE_FORMAT);
        assert!(transport.ready.is_empty());
    }

    #[test]
    fn planar_packets_pack_only_valid_frames_across_chunk_boundaries() {
        let format = PcmFormat {
            channels: 2,
            interleaved: false,
            ..mono_format()
        };
        let transport = Transport::new(format);
        let control = Control::new();
        let mut left = vec![500_i16; PACKET_FRAMES + 3];
        let mut right = vec![-700_i16; PACKET_FRAMES + 3];
        let buffers = [
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: (left.len() * 2) as u32,
                mData: left.as_mut_ptr().cast(),
            },
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: (right.len() * 2) as u32,
                mData: right.as_mut_ptr().cast(),
            },
        ];
        unsafe { transport.copy_buffers(&buffers, left.len(), format, &control) };
        for frames in [PACKET_FRAMES, 3] {
            let packet = transport.ready.pop().expect("captured packet");
            assert_eq!(packet.frames, frames);
            assert!(packet.bytes[..frames * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|bytes| *bytes == 500_i16.to_ne_bytes()));
            assert!(packet.bytes[frames * 2..frames * 4]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|bytes| *bytes == (-700_i16).to_ne_bytes()));
        }
        assert!(transport.ready.is_empty());
    }
}
