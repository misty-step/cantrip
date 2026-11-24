//! OS-free helpers for the macOS backend, compiled on macOS and in Linux test
//! builds so their behavior is proven without a Mac: UTF-16 ↔ char ranges for
//! AX, AX error interpretation, chord → key-code names, meter scaling, a
//! streaming downmix/resampler and a crash-tolerant PCM16 WAV writer, Cocoa ↔
//! top-left coordinate conversion, and the pasteboard / aggregate-device
//! constants. Nothing here calls an Apple API.

use cantrip_core::ports::{FieldSupport, InsertResult};
use cantrip_core::shortcut::Chord;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

// ───────────────────────────── AX text ranges ─────────────────────────────

/// Convert an AX (UTF-16 code unit) range in `text` to a char range. `None`
/// when the range is out of bounds or splits a surrogate pair — callers then
/// treat the field as unreadable rather than guessing.
pub fn utf16_range_to_chars(text: &str, location: usize, length: usize) -> Option<(usize, usize)> {
    let end16 = location.checked_add(length)?;
    let mut units = 0usize;
    let mut start = None;
    let mut end = None;
    for (ci, ch) in text.chars().enumerate() {
        if units == location && start.is_none() {
            start = Some(ci);
        }
        if units == end16 && end.is_none() {
            end = Some(ci);
        }
        if units > end16 {
            break;
        }
        units += ch.len_utf16();
    }
    let total_chars = text.chars().count();
    if units == location && start.is_none() {
        start = Some(total_chars);
    }
    if units == end16 && end.is_none() {
        end = Some(total_chars);
    }
    match (start, end) {
        (Some(s), Some(e)) if s <= e => Some((s, e)),
        _ => None,
    }
}

/// Number of UTF-16 code units in `text` (what kAXNumberOfCharacters counts).
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The value read is complete when its UTF-16 length equals the field's
/// reported kAXNumberOfCharacters.
pub fn value_is_complete(text: &str, reported_chars: Option<i64>) -> bool {
    matches!(reported_chars, Some(n) if n >= 0 && n as usize == utf16_len(text))
}

// ───────────────────────────── AX errors ─────────────────────────────

pub const AX_SUCCESS: i32 = 0;
pub const AX_FAILURE: i32 = -25200;
pub const AX_ILLEGAL_ARGUMENT: i32 = -25201;
pub const AX_INVALID_ELEMENT: i32 = -25202;
pub const AX_CANNOT_COMPLETE: i32 = -25204;
pub const AX_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
pub const AX_NOT_IMPLEMENTED: i32 = -25208;
pub const AX_API_DISABLED: i32 = -25211;
pub const AX_NO_VALUE: i32 = -25212;

/// Map the result of setting kAXSelectedText. Only errors that the AX server
/// returns *before* touching the field count as a refusal (tier 2 may follow);
/// anything else (timeouts, generic failure) may have changed it.
pub fn classify_insert(code: i32) -> InsertResult {
    match code {
        AX_SUCCESS => InsertResult::Done,
        AX_ATTRIBUTE_UNSUPPORTED | AX_API_DISABLED | AX_NOT_IMPLEMENTED | AX_ILLEGAL_ARGUMENT => {
            InsertResult::Refused
        }
        _ => InsertResult::Ambiguous,
    }
}

/// Map an AXUIElementIsAttributeSettable(kAXSelectedText) answer.
pub fn classify_support(code: i32, settable: bool) -> FieldSupport {
    match code {
        AX_SUCCESS if settable => FieldSupport::DirectInsert,
        AX_SUCCESS | AX_ATTRIBUTE_UNSUPPORTED | AX_NO_VALUE | AX_NOT_IMPLEMENTED => {
            FieldSupport::NoDirectInsert
        }
        // Destroyed element, hung app, or any other failure: it no longer answers.
        AX_INVALID_ELEMENT | AX_CANNOT_COMPLETE => FieldSupport::Gone,
        _ => FieldSupport::Gone,
    }
}

// ───────────────────────────── shortcut ─────────────────────────────

/// Virtual key code of `V` on the ANSI layout (kVK_ANSI_V), for ⌘V.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const KVK_ANSI_V: u16 = 9;

/// The W3C `KeyboardEvent.code` name (as `keyboard_types::Code` parses it) for
/// a chord key name, or `None` when Cantrip cannot register it.
pub fn chord_key_code_name(key: &str) -> Option<String> {
    let k = key.trim();
    let named = match k {
        "Space" => Some("Space"),
        "Enter" | "Return" => Some("Enter"),
        "Tab" => Some("Tab"),
        "Backspace" => Some("Backspace"),
        "Delete" => Some("Delete"),
        "Home" => Some("Home"),
        "End" => Some("End"),
        "PageUp" => Some("PageUp"),
        "PageDown" => Some("PageDown"),
        "Up" | "ArrowUp" => Some("ArrowUp"),
        "Down" | "ArrowDown" => Some("ArrowDown"),
        "Left" | "ArrowLeft" => Some("ArrowLeft"),
        "Right" | "ArrowRight" => Some("ArrowRight"),
        "Minus" | "-" => Some("Minus"),
        "Equal" | "=" => Some("Equal"),
        "Comma" | "," => Some("Comma"),
        "Period" | "." => Some("Period"),
        "Slash" | "/" => Some("Slash"),
        "Semicolon" | ";" => Some("Semicolon"),
        "Quote" | "'" => Some("Quote"),
        "Backquote" | "`" => Some("Backquote"),
        "BracketLeft" | "[" => Some("BracketLeft"),
        "BracketRight" | "]" => Some("BracketRight"),
        "Backslash" | "\\" => Some("Backslash"),
        _ => None,
    };
    if let Some(n) = named {
        return Some(n.to_string());
    }
    let mut chars = k.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(format!("Key{}", c.to_ascii_uppercase()));
        }
        if c.is_ascii_digit() {
            return Some(format!("Digit{c}"));
        }
        return None;
    }
    if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=20).contains(&n) {
            return Some(format!("F{n}"));
        }
    }
    None
}

/// Modifier flags for a chord as (ctrl, alt/option, shift, meta/command).
pub fn chord_modifiers(chord: &Chord) -> (bool, bool, bool, bool) {
    (chord.ctrl, chord.alt, chord.shift, chord.meta)
}

// ───────────────────────────── audio ─────────────────────────────

/// AVCaptureAudioChannel.averagePowerLevel (dBFS, ≤ 0) → 0..1 meter level.
/// -50 dB and below reads as silence.
pub fn db_to_level(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db + 50.0) / 50.0).clamp(0.0, 1.0)
}

/// Streaming downmix-to-mono + linear resampler to 16 kHz PCM16.
#[derive(Debug)]
pub struct Resampler {
    in_rate: f64,
    out_rate: f64,
    /// Position of the next output sample, in input-sample units, relative to
    /// the start of the pending buffer.
    pos: f64,
    /// Last mono input sample of the previous call (for interpolation).
    prev: f32,
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: f64, out_rate: f64) -> Self {
        Resampler {
            in_rate: in_rate.max(1.0),
            out_rate: out_rate.max(1.0),
            pos: 0.0,
            prev: 0.0,
            primed: false,
        }
    }

    /// Feed interleaved float frames with `channels` channels; returns PCM16
    /// mono samples at the output rate.
    pub fn process_interleaved(&mut self, data: &[f32], channels: usize) -> Vec<i16> {
        let ch = channels.max(1);
        let mono: Vec<f32> = data
            .chunks_exact(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32)
            .collect();
        self.process_mono(&mono)
    }

    /// Feed non-interleaved planes (one slice per channel, equal lengths).
    pub fn process_planar(&mut self, planes: &[&[f32]]) -> Vec<i16> {
        if planes.is_empty() {
            return Vec::new();
        }
        let n = planes.iter().map(|p| p.len()).min().unwrap_or(0);
        let mono: Vec<f32> = (0..n)
            .map(|i| planes.iter().map(|p| p[i]).sum::<f32>() / planes.len() as f32)
            .collect();
        self.process_mono(&mono)
    }

    fn process_mono(&mut self, mono: &[f32]) -> Vec<i16> {
        if mono.is_empty() {
            return Vec::new();
        }
        let step = self.in_rate / self.out_rate;
        // Sample i of the virtual buffer: index -1 is `prev`.
        let at = |i: isize, prev: f32| -> f32 {
            if i < 0 {
                prev
            } else {
                mono[i as usize]
            }
        };
        if !self.primed {
            // Start exactly on the first real sample (index 0 of `mono`).
            self.prev = mono[0];
            self.pos = 1.0;
            self.primed = true;
        }
        let mut out = Vec::with_capacity((mono.len() as f64 / step) as usize + 2);
        // `pos` indexes the virtual buffer [prev, mono...]: 0.0 is `prev`.
        while self.pos <= mono.len() as f64 + 1e-9 {
            let p = self.pos - 1.0; // index in mono, may be in [-1, len-1]
            let i0 = p.floor() as isize;
            let frac = (p - i0 as f64) as f32;
            let a = at(i0, self.prev);
            let b = if (i0 + 1) as usize >= mono.len() {
                a
            } else {
                at(i0 + 1, self.prev)
            };
            let v = a + (b - a) * frac;
            out.push(f32_to_i16(v));
            self.pos += step;
        }
        self.pos -= mono.len() as f64;
        self.prev = *mono.last().unwrap();
        out
    }
}

pub fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Crash-tolerant streaming 16-bit mono WAV writer: the header is written up
/// front with zero sizes (Cantrip's WAV reader tolerates that after a crash)
/// and patched on `finish`.
#[derive(Debug)]
pub struct WavWriter {
    file: File,
    data_bytes: u64,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> io::Result<WavWriter> {
        let mut file = File::create(path)?;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * 2).to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes());
        file.write_all(&h)?;
        Ok(WavWriter {
            file,
            data_bytes: 0,
        })
    }

    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        self.file.write_all(&buf)?;
        self.data_bytes += buf.len() as u64;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn samples_written(&self) -> u64 {
        self.data_bytes / 2
    }

    /// Patch sizes and fsync.
    pub fn finish(mut self) -> io::Result<()> {
        let data = self.data_bytes.min(u32::MAX as u64 - 36) as u32;
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&(36 + data).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&data.to_le_bytes())?;
        self.file.sync_all()
    }
}

// ───────────────────────────── geometry ─────────────────────────────

/// Convert a Cocoa point (origin bottom-left of the primary screen, y up) to
/// top-left logical coordinates (y down) given the primary screen height.
pub fn cocoa_to_top_left(primary_height: f64, x: f64, y: f64) -> (f64, f64) {
    (x, primary_height - y)
}

/// HUD anchor: bottom-centre of a visible frame given in Cocoa coordinates
/// (x, y, w, h), converted to top-left logical coordinates, lifted by
/// `margin` points from the bottom edge.
pub fn hud_anchor_from_visible_frame(
    primary_height: f64,
    frame: (f64, f64, f64, f64),
    margin: f64,
) -> (f32, f32) {
    let (x, y, w, _h) = frame;
    let (tx, ty) = cocoa_to_top_left(primary_height, x + w / 2.0, y + margin);
    (tx as f32, ty as f32)
}

/// Whether Cocoa point (px, py) lies within frame (x, y, w, h).
pub fn frame_contains(frame: (f64, f64, f64, f64), px: f64, py: f64) -> bool {
    let (x, y, w, h) = frame;
    px >= x && px <= x + w && py >= y && py <= y + h
}

// ───────────────────────────── pasteboard / Core Audio data ─────────────────────────────

/// nspasteboard.org markers: clipboard managers skip transient and concealed
/// items, so Cantrip's dictation does not enter clipboard histories.
pub const PB_TRANSIENT: &str = "org.nspasteboard.TransientType";
pub const PB_CONCEALED: &str = "org.nspasteboard.ConcealedType";
pub const PB_PLAIN_TEXT: &str = "public.utf8-plain-text";

/// The types Cantrip writes for one dictation.
pub fn pasteboard_types(transient: bool) -> Vec<&'static str> {
    let mut v = vec![PB_PLAIN_TEXT];
    if transient {
        v.push(PB_TRANSIENT);
        v.push(PB_CONCEALED);
    }
    v
}

/// UID of Cantrip's private tap aggregate device for one meeting.
pub fn aggregate_uid(pid: u32, n: u64) -> String {
    format!("io.mistystep.cantrip.meeting-tap.{pid}.{n}")
}

/// Instance discriminator surviving PID reuse.
pub fn app_instance(pid: u32, launch_unix: Option<f64>) -> String {
    match launch_unix {
        Some(t) => format!("{pid}:{}", t.round() as i64),
        None => format!("{pid}:?"),
    }
}

/// Convert a Core Audio four-char code to a string (for error messages).
pub fn fourcc(code: i32) -> String {
    let b = code.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        code.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ranges_map_emoji_and_combining_marks_to_chars() {
        let t = "a😀e\u{301}b"; // a, 😀 (2 units), e, ◌́, b
        assert_eq!(utf16_len(t), 6);
        assert_eq!(utf16_range_to_chars(t, 0, 0), Some((0, 0)));
        assert_eq!(utf16_range_to_chars(t, 1, 2), Some((1, 2)));
        assert_eq!(utf16_range_to_chars(t, 3, 2), Some((2, 4)));
        assert_eq!(utf16_range_to_chars(t, 6, 0), Some((5, 5)));
        // Splitting the surrogate pair or overrunning is unreadable.
        assert_eq!(utf16_range_to_chars(t, 2, 0), None);
        assert_eq!(utf16_range_to_chars(t, 5, 2), None);
    }

    #[test]
    fn completeness_needs_exact_utf16_count() {
        assert!(value_is_complete("a😀", Some(3)));
        assert!(!value_is_complete("a😀", Some(2)));
        assert!(!value_is_complete("abc", None));
    }

    #[test]
    fn only_pre_change_ax_errors_are_refusals() {
        assert_eq!(classify_insert(AX_SUCCESS), InsertResult::Done);
        assert_eq!(
            classify_insert(AX_ATTRIBUTE_UNSUPPORTED),
            InsertResult::Refused
        );
        assert_eq!(classify_insert(AX_API_DISABLED), InsertResult::Refused);
        assert_eq!(classify_insert(AX_CANNOT_COMPLETE), InsertResult::Ambiguous);
        assert_eq!(classify_insert(AX_FAILURE), InsertResult::Ambiguous);
        assert_eq!(
            classify_support(AX_SUCCESS, true),
            FieldSupport::DirectInsert
        );
        assert_eq!(
            classify_support(AX_SUCCESS, false),
            FieldSupport::NoDirectInsert
        );
        assert_eq!(
            classify_support(AX_INVALID_ELEMENT, false),
            FieldSupport::Gone
        );
        assert_eq!(
            classify_support(AX_CANNOT_COMPLETE, true),
            FieldSupport::Gone
        );
    }

    #[test]
    fn chord_keys_map_to_keyboard_codes() {
        assert_eq!(chord_key_code_name("Space").as_deref(), Some("Space"));
        assert_eq!(chord_key_code_name("a").as_deref(), Some("KeyA"));
        assert_eq!(chord_key_code_name("7").as_deref(), Some("Digit7"));
        assert_eq!(chord_key_code_name("F12").as_deref(), Some("F12"));
        assert_eq!(chord_key_code_name("F0"), None);
        assert_eq!(chord_key_code_name("Fn"), None);
        assert_eq!(chord_key_code_name("é"), None);
        assert_eq!(
            chord_modifiers(&Chord::platform_default()),
            (true, true, false, false)
        );
    }

    #[test]
    fn meter_scales_decibels() {
        assert_eq!(db_to_level(0.0), 1.0);
        assert_eq!(db_to_level(-60.0), 0.0);
        assert!((db_to_level(-25.0) - 0.5).abs() < 1e-6);
        assert_eq!(db_to_level(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn resampler_48k_stereo_to_16k_mono_keeps_duration_across_chunks() {
        let mut r = Resampler::new(48_000.0, 16_000.0);
        let mut total = 0;
        for _ in 0..100 {
            // 480 stereo frames (10 ms) of constant 0.5 / -0.5 → mono 0.0..
            let mut buf = Vec::new();
            for _ in 0..480 {
                buf.push(0.5);
                buf.push(0.25);
            }
            let out = r.process_interleaved(&buf, 2);
            assert!(out.iter().all(|&s| (s - f32_to_i16(0.375)).abs() <= 1));
            total += out.len();
        }
        // One second in, 16 000 ± 1 samples out.
        assert!((15_999..=16_001).contains(&total), "{total}");
    }

    #[test]
    fn resampler_planar_matches_interleaved() {
        let l = [0.1f32, 0.2, 0.3, 0.4];
        let rr = [0.3f32, 0.2, 0.1, 0.0];
        let inter: Vec<f32> = l
            .iter()
            .zip(rr.iter())
            .flat_map(|(a, b)| [*a, *b])
            .collect();
        let a = Resampler::new(16_000.0, 16_000.0).process_planar(&[&l, &rr]);
        let b = Resampler::new(16_000.0, 16_000.0).process_interleaved(&inter, 2);
        assert_eq!(a, b);
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn wav_writer_is_readable_before_and_after_finish() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        let mut w = WavWriter::create(&p, 16_000).unwrap();
        w.write(&[1, -2, 3]).unwrap();
        // Unfinalized (crash) file still decodes.
        let (s, rate) = cantrip_core::wav::read_pcm16(&p).unwrap();
        assert_eq!((s, rate), (vec![1, -2, 3], 16_000));
        w.write(&[4]).unwrap();
        assert_eq!(w.samples_written(), 4);
        w.finish().unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(
            cantrip_core::wav::read_pcm16(&p).unwrap().0,
            vec![1, -2, 3, 4]
        );
    }

    #[test]
    fn geometry_converts_cocoa_to_top_left() {
        assert_eq!(cocoa_to_top_left(900.0, 10.0, 100.0), (10.0, 800.0));
        // Visible frame above an 80-pt dock on a 1440×900 primary screen.
        let a = hud_anchor_from_visible_frame(900.0, (0.0, 80.0, 1440.0, 795.0), 24.0);
        assert_eq!(a, (720.0, 796.0));
        assert!(frame_contains((0.0, 0.0, 10.0, 10.0), 5.0, 5.0));
        assert!(!frame_contains((0.0, 0.0, 10.0, 10.0), 11.0, 5.0));
    }

    #[test]
    fn pasteboard_and_identity_helpers() {
        assert_eq!(pasteboard_types(false), vec![PB_PLAIN_TEXT]);
        assert_eq!(pasteboard_types(true).len(), 3);
        assert_eq!(app_instance(42, Some(1700000000.4)), "42:1700000000");
        assert_ne!(app_instance(42, Some(1.0)), app_instance(42, Some(2.0)));
        assert_eq!(fourcc(0x77686174), "what");
        assert!(aggregate_uid(1, 2).ends_with(".1.2"));
    }
}
