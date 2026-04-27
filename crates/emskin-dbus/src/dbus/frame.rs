//! DBus v1 message frame — one type for parse, encode, and inspect.
//!
//! Built on top of [`zvariant`]: header fields go through the `a(yv)`
//! signature, body is opaque bytes that callers decode on demand via
//! [`Frame::decode_body`]. The fixed 16-byte prefix
//! (endian / kind / flags / version / body_len / serial / fields_len)
//! is laid out by hand because it isn't a zvariant value.
//!
//! Layout:
//!
//! ```text
//! offset  size  field
//! ------  ----  ----------------------------------------------
//!   0     1     endianness marker: 'l' (little) or 'B' (big)
//!   1     1     message kind (1=call, 2=return, 3=error, 4=signal)
//!   2     1     flags
//!   3     1     protocol version (must be 1)
//!   4     4     body length (u32)
//!   8     4     serial (u32, must be non-zero)
//!  12     4     header-fields array length in bytes (u32)
//!  16     N     header fields (array of (byte, variant) structs)
//!  ...   pad    zero-pad to 8-byte boundary
//!  B     body_len  message body
//! ```

use std::borrow::Cow;
use std::ops::Range;
use std::{error, fmt};

use serde::ser::SerializeStruct;
use zvariant::serialized::{Context, Data};
use zvariant::{to_bytes, ObjectPath, Signature, Type, Value};

pub use zvariant::Endian;

/// Fixed prefix before the header-fields array.
pub const FIXED_HEADER_LEN: usize = 16;

/// dbus-daemon's default maximum message size (128 MiB). Mirrored here so
/// a malicious client can't make the broker allocate unbounded memory.
pub const MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

/// Header field codes. Public so external rule engines can match on a
/// field code without re-deriving the spec table.
pub mod field_code {
    pub const PATH: u8 = 1;
    pub const INTERFACE: u8 = 2;
    pub const MEMBER: u8 = 3;
    pub const ERROR_NAME: u8 = 4;
    pub const REPLY_SERIAL: u8 = 5;
    pub const DESTINATION: u8 = 6;
    pub const SENDER: u8 = 7;
    pub const SIGNATURE: u8 = 8;
    pub const UNIX_FDS: u8 = 9;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    MethodCall = 1,
    MethodReturn = 2,
    Error = 3,
    Signal = 4,
}

impl Kind {
    fn from_byte(b: u8) -> Result<Self, Error> {
        match b {
            1 => Ok(Self::MethodCall),
            2 => Ok(Self::MethodReturn),
            3 => Ok(Self::Error),
            4 => Ok(Self::Signal),
            _ => Err(Error::InvalidKind(b)),
        }
    }
}

/// Header fields, all optional. Same struct used for parse output and
/// build input — what the wire calls a `(yv)` is a typed Rust field
/// here, looked up by code via [`Fields::from_raw`] / [`Fields::to_raw`].
///
/// On parse, fields whose `Value` doesn't match the expected DBus type
/// (e.g. PATH carrying `s` instead of `o`) are silently dropped.
/// Strict daemon-side validation isn't the broker's job; the upstream
/// daemon will reject malformed traffic on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: Option<String>,
    pub reply_serial: Option<u32>,
    pub unix_fds: Option<u32>,
}

impl Fields {
    fn from_raw(raw: Vec<(u8, Value<'_>)>) -> Self {
        let mut out = Self::default();
        for (code, value) in raw {
            match code {
                field_code::PATH => {
                    if let Value::ObjectPath(p) = &value {
                        out.path = Some(p.to_string());
                    }
                }
                field_code::INTERFACE => out.interface = String::try_from(&value).ok(),
                field_code::MEMBER => out.member = String::try_from(&value).ok(),
                field_code::ERROR_NAME => out.error_name = String::try_from(&value).ok(),
                field_code::REPLY_SERIAL => out.reply_serial = u32::try_from(&value).ok(),
                field_code::DESTINATION => out.destination = String::try_from(&value).ok(),
                field_code::SENDER => out.sender = String::try_from(&value).ok(),
                // zvariant wraps multi-element signatures in outer parens
                // (it models them as an implicit struct); DBus wire never
                // uses those, hence `to_string_no_parens`.
                field_code::SIGNATURE => {
                    if let Value::Signature(s) = &value {
                        out.signature = Some(s.to_string_no_parens());
                    }
                }
                field_code::UNIX_FDS => out.unix_fds = u32::try_from(&value).ok(),
                _ => {} // unknown field: forward verbatim, ignore here
            }
        }
        out
    }

    fn count(&self) -> usize {
        [
            self.path.is_some(),
            self.interface.is_some(),
            self.member.is_some(),
            self.error_name.is_some(),
            self.reply_serial.is_some(),
            self.destination.is_some(),
            self.sender.is_some(),
            self.signature.is_some(),
            self.unix_fds.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count()
    }
}

/// `Type` impl ties [`Fields`] to the `a(yv)` wire signature so
/// `to_bytes(ctxt, &fields)` produces a length-prefixed array of
/// `(byte, variant)` entries.
impl Type for Fields {
    const SIGNATURE: &'static Signature = &Signature::Array(zvariant::signature::Child::Static {
        child: &Signature::Structure(zvariant::signature::Fields::Static {
            fields: &[&Signature::U8, &Signature::Variant],
        }),
    });
}

impl serde::Serialize for Fields {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;

        let mut seq = serializer.serialize_seq(Some(self.count()))?;
        if let Some(p) = &self.path {
            // Best-effort: skip on invalid object path.
            if let Ok(op) = ObjectPath::try_from(p.as_str()) {
                seq.serialize_element(&(field_code::PATH, Value::ObjectPath(op)))?;
            }
        }
        if let Some(s) = &self.interface {
            seq.serialize_element(&(field_code::INTERFACE, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.member {
            seq.serialize_element(&(field_code::MEMBER, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.error_name {
            seq.serialize_element(&(field_code::ERROR_NAME, Value::Str(s.as_str().into())))?;
        }
        if let Some(n) = self.reply_serial {
            seq.serialize_element(&(field_code::REPLY_SERIAL, Value::U32(n)))?;
        }
        if let Some(s) = &self.destination {
            seq.serialize_element(&(field_code::DESTINATION, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.sender {
            seq.serialize_element(&(field_code::SENDER, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.signature {
            // Use SignatureWire (see below) to write the SIGNATURE field's
            // variant value as a raw signature string — `Value::Signature`
            // would wrap multi-element signatures in `()`.
            seq.serialize_element(&(field_code::SIGNATURE, SignatureWire(s.as_str())))?;
        }
        if let Some(n) = self.unix_fds {
            seq.serialize_element(&(field_code::UNIX_FDS, Value::U32(n)))?;
        }
        seq.end()
    }
}

/// Variant-shaped wrapper that serializes a body signature *without*
/// the outer `()` zvariant adds for multi-element signatures.
///
/// zvariant 5 models `Signature` as an implicit struct so `Value::Signature`
/// emits e.g. `(a(si)i)` instead of `a(si)i`. GDBus / fcitx5 clients
/// reject signal bodies whose declared signature includes those parens
/// — the signal silently never reaches the client, breaking IM.
///
/// Borrowed verbatim from
/// [`zbus::message::fields::SignatureSerializer`](https://github.com/dbus2/zbus/blob/main/zbus/src/message/fields.rs)
/// — same trick, same justification.
#[derive(Debug, Clone, Copy)]
struct SignatureWire<'a>(&'a str);

impl Type for SignatureWire<'_> {
    const SIGNATURE: &'static Signature = &Signature::Variant;
}

impl serde::Serialize for SignatureWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_struct("Variant", 2)?;
        s.serialize_field("signature", &Signature::Signature)?;
        s.serialize_field("value", self.0)?;
        s.end()
    }
}

/// One complete DBus message. Created by [`Frame::parse`] (body is
/// borrowed from the input buffer, zero-copy) or by [`Builder::build`]
/// (body is owned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub endian: Endian,
    pub kind: Kind,
    pub flags: u8,
    pub serial: u32,
    pub fields: Fields,
    pub body: Cow<'a, [u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidEndian(u8),
    InvalidKind(u8),
    WrongProtocolVersion(u8),
    ZeroSerial,
    TooShort,
    SizeOverflow,
    MessageTooLarge(usize),
    HeaderFieldsParse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndian(b) => write!(f, "invalid endian byte: 0x{b:02x}"),
            Self::InvalidKind(b) => write!(f, "invalid message kind byte: {b}"),
            Self::WrongProtocolVersion(v) => write!(f, "unsupported protocol version: {v}"),
            Self::ZeroSerial => f.write_str("message serial is zero"),
            Self::TooShort => f.write_str("buffer shorter than declared frame"),
            Self::SizeOverflow => f.write_str("frame size computation overflowed"),
            Self::MessageTooLarge(n) => write!(f, "frame size {n} exceeds maximum"),
            Self::HeaderFieldsParse(s) => write!(f, "header fields parse: {s}"),
        }
    }
}

impl error::Error for Error {}

impl<'a> Frame<'a> {
    /// How many bytes does the frame at `buf[0..]` occupy?
    ///
    /// - `Ok(None)` when `buf` is shorter than [`FIXED_HEADER_LEN`].
    /// - `Ok(Some(n))` when the frame's full size is known (may exceed
    ///   `buf.len()` — caller should keep reading).
    /// - `Err` on a malformed prefix; close the connection.
    pub fn bytes_needed(buf: &[u8]) -> Result<Option<usize>, Error> {
        if buf.len() < FIXED_HEADER_LEN {
            return Ok(None);
        }
        let endian = parse_endian(buf[0])?;
        if buf[3] != 1 {
            return Err(Error::WrongProtocolVersion(buf[3]));
        }
        let body_len = endian.read_u32(&buf[4..8]) as usize;
        let fields_len = endian.read_u32(&buf[12..16]) as usize;
        let header_section = FIXED_HEADER_LEN
            .checked_add(fields_len)
            .ok_or(Error::SizeOverflow)?;
        let body_start = align8(header_section).ok_or(Error::SizeOverflow)?;
        let total = body_start
            .checked_add(body_len)
            .ok_or(Error::SizeOverflow)?;
        if total > MAX_MESSAGE_SIZE {
            return Err(Error::MessageTooLarge(total));
        }
        Ok(Some(total))
    }

    /// Parse the frame at the start of `buf`. Body is borrowed from
    /// `buf`, zero-copy — the resulting `Frame<'a>` cannot outlive
    /// `buf`. Use `frame.into_owned()` to lift to `'static` if needed.
    pub fn parse(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < FIXED_HEADER_LEN {
            return Err(Error::TooShort);
        }
        let endian = parse_endian(buf[0])?;
        if buf[3] != 1 {
            return Err(Error::WrongProtocolVersion(buf[3]));
        }
        let kind = Kind::from_byte(buf[1])?;
        let flags = buf[2];
        let body_len = endian.read_u32(&buf[4..8]) as usize;
        let serial = endian.read_u32(&buf[8..12]);
        if serial == 0 {
            return Err(Error::ZeroSerial);
        }
        let fields_len = endian.read_u32(&buf[12..16]) as usize;

        let fields_section_end = FIXED_HEADER_LEN
            .checked_add(fields_len)
            .ok_or(Error::SizeOverflow)?;
        if buf.len() < fields_section_end {
            return Err(Error::TooShort);
        }

        // Decode header fields with zvariant. Slice starts at byte 12
        // (which contains the array length prefix), so position=12 lets
        // zvariant compute the right alignment for the first struct.
        let ctxt = Context::new_dbus(endian, 12);
        let data = Data::new(&buf[12..fields_section_end], ctxt);
        let (raw, _): (Vec<(u8, Value<'_>)>, _) = data
            .deserialize::<Vec<(u8, Value<'_>)>>()
            .map_err(|e| Error::HeaderFieldsParse(e.to_string()))?;
        let fields = Fields::from_raw(raw);

        let body_start = align8(fields_section_end).ok_or(Error::SizeOverflow)?;
        let body_end = body_start
            .checked_add(body_len)
            .ok_or(Error::SizeOverflow)?;
        if buf.len() < body_end {
            return Err(Error::TooShort);
        }

        Ok(Frame {
            endian,
            kind,
            flags,
            serial,
            fields,
            body: Cow::Borrowed(&buf[body_start..body_end]),
        })
    }

    /// Serialize this frame as DBus wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let ctxt = Context::new_dbus(self.endian, 12);
        let fields_with_len = to_bytes(ctxt, &self.fields)
            .expect("fields serialize")
            .bytes()
            .to_vec();

        let mut out = Vec::with_capacity(16 + fields_with_len.len() + self.body.len() + 8);
        out.push(match self.endian {
            Endian::Little => b'l',
            Endian::Big => b'B',
        });
        out.push(self.kind as u8);
        out.push(self.flags);
        out.push(1); // protocol version
        out.extend_from_slice(&endian_u32(self.endian, self.body.len() as u32));
        out.extend_from_slice(&endian_u32(self.endian, self.serial));
        // `fields_with_len` already starts with the u32 array length —
        // zvariant emits the length-prefix when serializing a `Vec`.
        out.extend_from_slice(&fields_with_len);
        // Body starts at the next 8-aligned offset.
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        out.extend_from_slice(&self.body);
        out
    }

    /// Decode the body as a single typed value, using the body
    /// signature from `self.fields.signature` (the wire signature) —
    /// **not** `T::SIGNATURE`.
    ///
    /// This matters for multi-arg method bodies: e.g. fcitx5's
    /// `ProcessKeyEvent` body has wire signature `uubuu` (five
    /// top-level args, no outer struct), and we want to decode it as a
    /// Rust tuple `(u32, u32, u32, bool, u32)`. If zvariant used
    /// `T::SIGNATURE` it would derive `(uubuu)` (a struct) which is the
    /// wrong wire-format reading even though the bytes are bit-identical
    /// for this specific case — for other types (`oay` vs `(oay)`) the
    /// alignments differ and decode would silently fail.
    pub fn decode_body<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let sig = self.fields.signature.as_deref()?;
        Data::new(&self.body[..], Context::new_dbus(self.endian, 0))
            .deserialize_for_signature::<&str, T>(sig)
            .ok()
            .map(|(v, _)| v)
    }

    /// Lift to `'static` by cloning any borrowed body bytes. Use when
    /// the input buffer's lifetime can't reach where the frame needs to
    /// live (e.g. moving through a channel).
    pub fn into_owned(self) -> Frame<'static> {
        Frame {
            endian: self.endian,
            kind: self.kind,
            flags: self.flags,
            serial: self.serial,
            fields: self.fields,
            body: Cow::Owned(self.body.into_owned()),
        }
    }
}

// --------------------------------------------------------------------
// Builder API for synthesizing frames.
// --------------------------------------------------------------------

impl Frame<'_> {
    /// Start a method_return reply. `reply_to` provides the
    /// `reply_serial` and the symmetric sender/destination swap (the
    /// reply's destination is the caller's sender, etc.).
    pub fn method_return(reply_to: &Frame<'_>) -> Builder {
        Builder::new(Kind::MethodReturn).fill_from_request(reply_to)
    }

    /// Start a signal frame. `path` / `interface` / `member` are
    /// required by the DBus spec.
    pub fn signal(
        path: impl Into<String>,
        interface: impl Into<String>,
        member: impl Into<String>,
    ) -> Builder {
        let mut b = Builder::new(Kind::Signal);
        b.fields.path = Some(path.into());
        b.fields.interface = Some(interface.into());
        b.fields.member = Some(member.into());
        b
    }

    /// Start an error reply. Same sender/destination semantics as
    /// [`Frame::method_return`].
    pub fn error(reply_to: &Frame<'_>, error_name: impl Into<String>) -> Builder {
        let mut b = Builder::new(Kind::Error).fill_from_request(reply_to);
        b.fields.error_name = Some(error_name.into());
        b
    }
}

/// Frame builder. Outputs little-endian frames — every modern Linux
/// DBus client is LE and the parser side still accepts BE inputs, so
/// we don't need a builder option for it.
#[derive(Debug)]
pub struct Builder {
    kind: Kind,
    serial: u32,
    flags: u8,
    fields: Fields,
    body: Vec<u8>,
}

impl Builder {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            serial: 0,
            flags: 0,
            fields: Fields::default(),
            body: Vec::new(),
        }
    }

    fn fill_from_request(mut self, request: &Frame<'_>) -> Self {
        self.fields.reply_serial = Some(request.serial);
        self.fields.destination = request.fields.sender.clone();
        self.fields.sender = request.fields.destination.clone();
        self
    }

    pub fn serial(mut self, n: u32) -> Self {
        self.serial = n;
        self
    }

    pub fn flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    pub fn destination(mut self, s: impl Into<String>) -> Self {
        self.fields.destination = Some(s.into());
        self
    }

    pub fn sender(mut self, s: impl Into<String>) -> Self {
        self.fields.sender = Some(s.into());
        self
    }

    /// Override (or clear) the destination set by [`Frame::method_return`]
    /// / [`Frame::error`] — useful when the request lacked a sender.
    pub fn no_destination(mut self) -> Self {
        self.fields.destination = None;
        self
    }

    /// Body is a single typed arg; `T`'s DBus signature comes from
    /// [`zvariant::Type::SIGNATURE`].
    pub fn body<T>(mut self, value: &T) -> Self
    where
        T: serde::Serialize + zvariant::Type,
    {
        let ctxt = Context::new_dbus(Endian::Little, 0);
        self.body = to_bytes(ctxt, value)
            .expect("body serialize")
            .bytes()
            .to_vec();
        self.fields.signature = Some(T::SIGNATURE.to_string_no_parens());
        self
    }

    /// Begin building a multi-arg body. DBus method bodies are
    /// implicitly tuples of independent top-level args (signature is the
    /// concatenation, *not* a struct), so each call to [`Args::arg`]
    /// appends one more arg with the proper cumulative-offset alignment.
    pub fn body_args(self) -> Args {
        Args {
            inner: self,
            sig: String::new(),
        }
    }

    pub fn build(self) -> Frame<'static> {
        Frame {
            endian: Endian::Little,
            kind: self.kind,
            flags: self.flags,
            serial: self.serial,
            fields: self.fields,
            body: Cow::Owned(self.body),
        }
    }
}

/// Multi-arg body builder; see [`Builder::body_args`].
#[derive(Debug)]
pub struct Args {
    inner: Builder,
    sig: String,
}

impl Args {
    pub fn arg<T>(mut self, value: &T) -> Self
    where
        T: serde::Serialize + zvariant::Type,
    {
        let ctxt = Context::new_dbus(Endian::Little, self.inner.body.len());
        let encoded = to_bytes(ctxt, value).expect("arg serialize");
        self.inner.body.extend_from_slice(encoded.bytes());
        self.sig.push_str(&T::SIGNATURE.to_string_no_parens());
        self
    }

    pub fn done(mut self) -> Builder {
        if !self.sig.is_empty() {
            self.inner.fields.signature = Some(self.sig);
        }
        self.inner
    }
}

// --------------------------------------------------------------------
// Helpers used by parse/encode.
// --------------------------------------------------------------------

/// Range helper for slicing a frame out of a larger buffer.
pub fn span(offset: usize, length: usize) -> Range<usize> {
    offset..offset + length
}

fn parse_endian(b: u8) -> Result<Endian, Error> {
    match b {
        b'l' => Ok(Endian::Little),
        b'B' => Ok(Endian::Big),
        _ => Err(Error::InvalidEndian(b)),
    }
}

fn align8(n: usize) -> Option<usize> {
    n.checked_add(7).map(|v| v & !7)
}

fn endian_u32(endian: Endian, n: u32) -> [u8; 4] {
    match endian {
        Endian::Little => n.to_le_bytes(),
        Endian::Big => n.to_be_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_call() -> Frame<'static> {
        Frame::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
        )
        .serial(1)
        .destination("org.freedesktop.DBus")
        .build()
        .with_kind(Kind::MethodCall)
    }

    impl Frame<'static> {
        fn with_kind(mut self, kind: Kind) -> Self {
            self.kind = kind;
            self
        }
    }

    #[test]
    fn parse_round_trip_method_call() {
        let frame = hello_call();
        let bytes = frame.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, Kind::MethodCall);
        assert_eq!(parsed.serial, 1);
        assert_eq!(parsed.fields.member.as_deref(), Some("Hello"));
        assert_eq!(parsed.fields.path.as_deref(), Some("/org/freedesktop/DBus"));
        assert_eq!(
            parsed.fields.destination.as_deref(),
            Some("org.freedesktop.DBus")
        );
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn bytes_needed_reports_full_frame_size() {
        let bytes = hello_call().encode();
        let needed = Frame::bytes_needed(&bytes[..FIXED_HEADER_LEN])
            .unwrap()
            .unwrap();
        assert_eq!(needed, bytes.len());
    }

    #[test]
    fn bytes_needed_returns_none_before_full_fixed_header() {
        let bytes = hello_call().encode();
        for n in 0..FIXED_HEADER_LEN {
            assert_eq!(Frame::bytes_needed(&bytes[..n]).unwrap(), None);
        }
    }

    #[test]
    fn bytes_needed_rejects_wrong_protocol_version() {
        let mut buf = vec![b'l', 1, 0, 99];
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(
            Frame::bytes_needed(&buf),
            Err(Error::WrongProtocolVersion(99))
        );
    }

    #[test]
    fn parse_rejects_zero_serial() {
        let mut bytes = hello_call().encode();
        bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Frame::parse(&bytes), Err(Error::ZeroSerial));
    }

    #[test]
    fn method_return_round_trip() {
        let request = hello_call();
        let reply = Frame::method_return(&request)
            .serial(42)
            .body(&true)
            .build();
        let bytes = reply.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, Kind::MethodReturn);
        assert_eq!(parsed.serial, 42);
        assert_eq!(parsed.fields.reply_serial, Some(1));
        assert_eq!(parsed.fields.signature.as_deref(), Some("b"));
        assert_eq!(parsed.body.len(), 4); // bool = u32 LE
        assert_eq!(parsed.decode_body::<bool>(), Some(true));
    }

    #[test]
    fn signal_round_trip_with_string_body() {
        let signal = Frame::signal(
            "/ic/7",
            "org.fcitx.Fcitx.InputContext1",
            "CommitString",
        )
        .serial(99)
        .destination(":1.42")
        .body(&"你好".to_string())
        .build();
        let bytes = signal.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, Kind::Signal);
        assert_eq!(parsed.fields.member.as_deref(), Some("CommitString"));
        assert_eq!(parsed.fields.signature.as_deref(), Some("s"));
        assert_eq!(parsed.decode_body::<String>().as_deref(), Some("你好"));
    }

    #[test]
    fn body_args_two_args_oay() {
        // fcitx5 CreateInputContext reply: ObjectPath + byte array, two
        // top-level args (signature `oay`, *not* `(oay)`).
        let request = hello_call();
        let path = ObjectPath::try_from("/ic/7").unwrap();
        let uuid: Vec<u8> = vec![0xAB; 16];
        let reply = Frame::method_return(&request)
            .serial(11)
            .body_args()
            .arg(&path)
            .arg(&uuid)
            .done()
            .build();
        let bytes = reply.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.fields.signature.as_deref(), Some("oay"));
        // body = 4 (path len) + 5 ("/ic/7") + 1 (NUL) + 2 (pad) + 4 (array len) + 16 = 32
        assert_eq!(parsed.body.len(), 32);
    }

    #[test]
    fn body_args_three_strings_for_name_owner_changed() {
        let signal = Frame::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .serial(7)
        .body_args()
        .arg(&"org.fcitx.Fcitx5".to_string())
        .arg(&"".to_string())
        .arg(&":1.42".to_string())
        .done()
        .build();
        let bytes = signal.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.fields.signature.as_deref(), Some("sss"));
        assert_eq!(
            parsed.decode_body::<(String, String, String)>(),
            Some((
                "org.fcitx.Fcitx5".into(),
                "".into(),
                ":1.42".into()
            ))
        );
    }

    #[test]
    fn empty_body_method_return_has_no_signature() {
        let request = hello_call();
        let reply = Frame::method_return(&request).serial(5).build();
        let bytes = reply.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.fields.signature, None);
        assert_eq!(parsed.body.len(), 0);
    }

    #[test]
    fn error_round_trip() {
        let request = hello_call();
        let frame = Frame::error(&request, "org.example.Error.NoSuchIC")
            .serial(7)
            .body(&"ic_id not found".to_string())
            .build();
        let bytes = frame.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, Kind::Error);
        assert_eq!(
            parsed.fields.error_name.as_deref(),
            Some("org.example.Error.NoSuchIC")
        );
        assert_eq!(parsed.fields.reply_serial, Some(1));
    }

    #[test]
    fn parse_borrowed_body_zero_copy() {
        let request = hello_call();
        let owned = Frame::method_return(&request).serial(2).body(&42u32).build();
        let bytes = owned.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        // Borrowed body: pointer should be inside `bytes`.
        let body_ptr = parsed.body.as_ptr();
        let buf_start = bytes.as_ptr();
        let buf_end = unsafe { buf_start.add(bytes.len()) };
        assert!(body_ptr >= buf_start && body_ptr <= buf_end);
    }

    #[test]
    fn into_owned_lifts_to_static() {
        let request = hello_call();
        let frame = Frame::method_return(&request).serial(2).build();
        let bytes = frame.encode();
        let owned: Frame<'static> = Frame::parse(&bytes).unwrap().into_owned();
        drop(bytes);
        // owned still usable
        assert_eq!(owned.serial, 2);
    }

    #[test]
    fn signature_field_does_not_wrap_in_parens() {
        // Regression: zvariant's `Value::Signature` wraps multi-element
        // signatures in `()` (it models them as an implicit struct).
        // GDBus / fcitx5 clients reject signal bodies whose declared
        // SIGNATURE includes those parens — IM signals get silently
        // dropped. `SignatureWire` must serialize the raw string.
        let request = hello_call();
        let path = ObjectPath::try_from("/ic/7").unwrap();
        let chunks: Vec<(String, i32)> = vec![("hello".into(), 0)];
        let frame = Frame::method_return(&request)
            .serial(11)
            .body_args()
            .arg(&chunks)
            .arg(&0i32)
            .done()
            .build();
        let bytes = frame.encode();

        // Find the SIGNATURE field's wire bytes inside the encoded
        // header. Header field format per spec: 8-aligned struct
        // containing (byte code, variant). The SIGNATURE variant for a
        // body of `a(si)i` should encode as `1g\0` (variant sig "g") +
        // `<len> a(si)i \0` — *without* outer parens.
        let needle: &[u8] = b"a(si)i\0";
        let bad: &[u8] = b"(a(si)i)\0";
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "expected raw signature {:?} in wire bytes, got {:x?}",
            std::str::from_utf8(needle).unwrap(),
            bytes
        );
        assert!(
            !bytes.windows(bad.len()).any(|w| w == bad),
            "SIGNATURE field must not contain wrapped {:?}",
            std::str::from_utf8(bad).unwrap()
        );

        // Round-trip parse should still report the unwrapped signature.
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.fields.signature.as_deref(), Some("a(si)i"));
    }

    #[test]
    fn fields_with_invalid_typed_value_dropped_silently() {
        // PATH carrying a `Value::Str` (signature `s` not `o`) should be
        // silently dropped — the spec says only `o` is allowed there,
        // and the broker isn't the place to enforce that.
        // We can't easily build such bad bytes by hand, so just check
        // that `Fields::from_raw` silently drops mismatched types.
        let raw: Vec<(u8, Value<'_>)> = vec![
            (field_code::PATH, Value::Str("not-a-path".into())),
            (field_code::MEMBER, Value::Str("Hello".into())),
        ];
        let fields = Fields::from_raw(raw);
        assert_eq!(fields.path, None);
        assert_eq!(fields.member.as_deref(), Some("Hello"));
    }
}
