//! The VNC (RFB) protocol, so that any VNC client, however old, can show and
//! control a Hyprland output served by gliff-server: the handshake and VNC
//! Authentication, every pixel format a client may ask for, the Raw,
//! Hextile and ZRLE encodings, tracking what changed since the client was
//! last sent the screen, and typing the keysyms clients send.

#![forbid(unsafe_code)]

pub mod auth;
pub mod encode;
pub mod keys;
pub mod pixel;
pub mod proto;
pub mod screen;
