//! The parts of RustDesk's protocol messages gliff uses, by field number.
//! Fields left out are skipped when read and absent when written, as
//! protobuf allows. Enumerations are plain `int32`, as on the wire.

/// Every message travels in this one, as one of its cases.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Message {
    #[prost(
        oneof = "message::Union",
        tags = "3, 4, 5, 6, 7, 8, 9, 10, 15, 19, 25, 27"
    )]
    pub union: Option<message::Union>,
}

pub mod message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Union {
        #[prost(message, tag = "3")]
        SignedId(super::SignedId),
        #[prost(message, tag = "4")]
        PublicKey(super::PublicKey),
        #[prost(message, tag = "5")]
        TestDelay(super::TestDelay),
        #[prost(message, tag = "6")]
        VideoFrame(super::VideoFrame),
        #[prost(message, tag = "7")]
        LoginRequest(super::LoginRequest),
        #[prost(message, tag = "8")]
        LoginResponse(super::LoginResponse),
        #[prost(message, tag = "9")]
        Hash(super::Hash),
        #[prost(message, tag = "10")]
        MouseEvent(super::MouseEvent),
        #[prost(message, tag = "15")]
        KeyEvent(super::KeyEvent),
        #[prost(message, tag = "19")]
        Misc(super::Misc),
        #[prost(message, tag = "25")]
        PeerInfo(super::PeerInfo),
        #[prost(message, tag = "27")]
        Auth2Fa(super::Auth2Fa),
    }
}

/// The other machine's identity, signed: a 64-byte signature, then an
/// encoded [`IdPk`].
#[derive(Clone, PartialEq, prost::Message)]
pub struct SignedId {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct IdPk {
    #[prost(string, tag = "1")]
    pub id: String,
    /// Its key for this connection only.
    #[prost(bytes = "vec", tag = "2")]
    pub pk: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub kx_version: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PublicKey {
    #[prost(bytes = "vec", tag = "1")]
    pub asymmetric_value: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub symmetric_value: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub kx_version: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Hash {
    #[prost(string, tag = "1")]
    pub salt: String,
    #[prost(string, tag = "2")]
    pub challenge: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LoginRequest {
    #[prost(string, tag = "1")]
    pub username: String,
    #[prost(bytes = "vec", tag = "2")]
    pub password: Vec<u8>,
    #[prost(string, tag = "4")]
    pub my_id: String,
    #[prost(string, tag = "5")]
    pub my_name: String,
    #[prost(message, optional, tag = "6")]
    pub option: Option<OptionMessage>,
    #[prost(bool, tag = "9")]
    pub video_ack_required: bool,
    #[prost(uint64, tag = "10")]
    pub session_id: u64,
    #[prost(string, tag = "11")]
    pub version: String,
    #[prost(string, tag = "13")]
    pub my_platform: String,
}

/// `ImageQuality.Low`.
pub const IMAGE_QUALITY_LOW: i32 = 2;
/// `BoolOption.Yes`.
pub const YES: i32 = 2;

#[derive(Clone, PartialEq, prost::Message)]
pub struct OptionMessage {
    #[prost(int32, tag = "1")]
    pub image_quality: i32,
    #[prost(int32, tag = "7")]
    pub disable_audio: i32,
    #[prost(int32, tag = "8")]
    pub disable_clipboard: i32,
    #[prost(message, optional, tag = "10")]
    pub supported_decoding: Option<SupportedDecoding>,
    #[prost(int32, tag = "11")]
    pub custom_fps: i32,
}

/// `PreferCodec.VP9`.
pub const PREFER_VP9: i32 = 1;

#[derive(Clone, PartialEq, prost::Message)]
pub struct SupportedDecoding {
    #[prost(int32, tag = "1")]
    pub ability_vp9: i32,
    #[prost(int32, tag = "2")]
    pub ability_h264: i32,
    #[prost(int32, tag = "3")]
    pub ability_h265: i32,
    #[prost(int32, tag = "4")]
    pub prefer: i32,
    #[prost(int32, tag = "5")]
    pub ability_vp8: i32,
    #[prost(int32, tag = "6")]
    pub ability_av1: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LoginResponse {
    #[prost(oneof = "login_response::Union", tags = "1, 2")]
    pub union: Option<login_response::Union>,
}

pub mod login_response {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Union {
        #[prost(string, tag = "1")]
        Error(String),
        #[prost(message, tag = "2")]
        PeerInfo(super::PeerInfo),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PeerInfo {
    #[prost(string, tag = "1")]
    pub username: String,
    #[prost(string, tag = "2")]
    pub hostname: String,
    #[prost(string, tag = "3")]
    pub platform: String,
    #[prost(message, repeated, tag = "4")]
    pub displays: Vec<DisplayInfo>,
    #[prost(int32, tag = "5")]
    pub current_display: i32,
    #[prost(string, tag = "7")]
    pub version: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DisplayInfo {
    #[prost(sint32, tag = "1")]
    pub x: i32,
    #[prost(sint32, tag = "2")]
    pub y: i32,
    #[prost(int32, tag = "3")]
    pub width: i32,
    #[prost(int32, tag = "4")]
    pub height: i32,
    #[prost(string, tag = "5")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct VideoFrame {
    #[prost(oneof = "video_frame::Union", tags = "6, 10, 11, 12, 13")]
    pub union: Option<video_frame::Union>,
    #[prost(int32, tag = "14")]
    pub display: i32,
}

pub mod video_frame {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Union {
        #[prost(message, tag = "6")]
        Vp9s(super::EncodedVideoFrames),
        #[prost(message, tag = "10")]
        H264s(super::EncodedVideoFrames),
        #[prost(message, tag = "11")]
        H265s(super::EncodedVideoFrames),
        #[prost(message, tag = "12")]
        Vp8s(super::EncodedVideoFrames),
        #[prost(message, tag = "13")]
        Av1s(super::EncodedVideoFrames),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EncodedVideoFrames {
    #[prost(message, repeated, tag = "1")]
    pub frames: Vec<EncodedVideoFrame>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EncodedVideoFrame {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
    #[prost(bool, tag = "2")]
    pub key: bool,
    #[prost(int64, tag = "3")]
    pub pts: i64,
}

pub const MOUSE_TYPE_MOVE: i32 = 0;
pub const MOUSE_TYPE_DOWN: i32 = 1;
pub const MOUSE_TYPE_UP: i32 = 2;
pub const MOUSE_TYPE_WHEEL: i32 = 3;
pub const MOUSE_BUTTON_LEFT: i32 = 0x01;
pub const MOUSE_BUTTON_RIGHT: i32 = 0x02;
pub const MOUSE_BUTTON_WHEEL: i32 = 0x04;
pub const MOUSE_BUTTON_BACK: i32 = 0x08;
pub const MOUSE_BUTTON_FORWARD: i32 = 0x10;

#[derive(Clone, PartialEq, prost::Message)]
pub struct MouseEvent {
    /// `buttons << 3 | type`.
    #[prost(int32, tag = "1")]
    pub mask: i32,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
    #[prost(int32, repeated, tag = "4")]
    pub modifiers: Vec<i32>,
}

/// `ControlKey` values gliff sends.
pub const CONTROL_KEY_CAPS_LOCK: i32 = 3;
pub const CONTROL_KEY_PAUSE: i32 = 46;
pub const CONTROL_KEY_NUM_LOCK: i32 = 63;
/// `KeyboardMode`.
pub const KEYBOARD_MODE_LEGACY: i32 = 0;
pub const KEYBOARD_MODE_MAP: i32 = 1;

#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyEvent {
    #[prost(bool, tag = "1")]
    pub down: bool,
    #[prost(bool, tag = "2")]
    pub press: bool,
    #[prost(oneof = "key_event::Union", tags = "3, 4")]
    pub union: Option<key_event::Union>,
    #[prost(int32, repeated, tag = "8")]
    pub modifiers: Vec<i32>,
    #[prost(int32, tag = "9")]
    pub mode: i32,
}

pub mod key_event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Union {
        #[prost(int32, tag = "3")]
        ControlKey(i32),
        /// The key's code on the other machine's platform.
        #[prost(uint32, tag = "4")]
        Chr(u32),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TestDelay {
    #[prost(int64, tag = "1")]
    pub time: i64,
    #[prost(bool, tag = "2")]
    pub from_client: bool,
    #[prost(uint32, tag = "3")]
    pub last_delay: u32,
    #[prost(uint32, tag = "4")]
    pub target_bitrate: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Misc {
    #[prost(oneof = "misc::Union", tags = "5, 9")]
    pub union: Option<misc::Union>,
}

pub mod misc {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Union {
        #[prost(message, tag = "5")]
        SwitchDisplay(super::SwitchDisplay),
        #[prost(string, tag = "9")]
        CloseReason(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SwitchDisplay {
    #[prost(int32, tag = "1")]
    pub display: i32,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
    #[prost(int32, tag = "4")]
    pub width: i32,
    #[prost(int32, tag = "5")]
    pub height: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Auth2Fa {
    #[prost(string, tag = "1")]
    pub code: String,
}
