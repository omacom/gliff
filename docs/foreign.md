# RDP, VNC and RustDesk machines

gliff shows and drives machines that run a Remote Desktop (RDP) server, a
VNC server or RustDesk instead of gliff. Their names carry the protocol:

```
rdp://user@host         # port 3389
rdp://user@host:3390
rustdesk://host         # port 21118, RustDesk's direct IP access
rustdesk://192.168.1.9:21200
vnc://host              # port 5900
vnc://host:1            # display 1, port 5901
vnc://user@host:5902    # a user, for servers that ask for one
```

Type one in the address bar, or give it to `gliff` or to `--control`:

```
gliff rdp://ann@office-pc
gliff --control right:rustdesk://laptop
```

A tab shows the machine's screen; the mouse button in the header drives it
from the edge of this computer's screens you pick, with no window. Sound,
the clipboard and "Use as a screen" need gliff on the other machine.

## Logging in

gliff asks for the password the first time and, unless told not to, keeps it
in the desktop's keyring through the Secret Service (gnome-keyring, KeePassXC
or KWallet), under the machine's name, once the machine has taken it: a
mistyped password is never kept. A refused password is asked for
again; driving from the edge stops after one refusal rather than retry and
lock the account. Forgetting the machine's tab forgets its password. The
command-line `--control` reads the password from the keyring, so connect
from the window once first.

## Finding out what a machine runs

When a machine typed as `user@host` does not let ssh in (the connection is
refused, times out, or the key is not accepted), gliff looks for an RDP
server, RustDesk and a VNC server on it, and offers to use what answers. Each answer
is checked to speak the protocol. Accepting renames the tab, keeping its
place and screen side.

## RDP

gliff uses TLS, with Network Level Authentication (NTLM) when the server
asks for it; a server that offers NLA but takes the login over TLS alone,
such as weston, gets a second try without it. Kerberos and smart cards are
not supported.

Most RDP servers show a certificate of their own making. gliff keeps the
first one it sees for each machine and refuses another after, as ssh does
with host keys; forget the tab to accept a new one. A certificate a trusted
authority signed is checked as usual.

On Windows, Remote Desktop opens a session of its own and locks the screen
at the machine itself, so driving a Windows PC from the edge works on that
session, not on what its monitor shows. Servers that share the session
already open, such as GNOME's or KDE's desktop sharing, drive the screen in
front of you.

## RustDesk

The other machine needs RustDesk's "Enable direct IP access" option; gliff
does not use RustDesk's rendezvous servers or IDs. Its permanent password or
the one-time password it shows both work. RustDesk replaces a one-time
password once it has let gliff in, so keeping one in the keyring only helps
until then: next time gliff asks for the new one. With no password, the
machine asks its user to accept the connection.
Its picture comes as VP9 or VP8, decoded with the system's libvpx.

RustDesk does not encrypt direct IP connections: its direct server skips
the key exchange it makes through its rendezvous servers, so the screen and
the keys cross the network as they are, and nothing proves the machine is
the one you meant. The password itself never travels, only a hash salted by
the machine's own challenge. gliff's login dialog says so before the
password is typed. Use direct IP access on your own network, or reach the
machine through a VPN or an ssh tunnel.

A server that does send its identity with a key gets an encrypted session.
That encryption is weaker than it looks: RustDesk uses one key in both
directions, each side counting its messages from one, so a message each way
shares a nonce, and someone who records the connection can learn parts of
one from the other where either is predictable. gliff speaks the protocol
as RustDesk does and cannot change that alone.

On Wayland, the RustDesk machine asks its user to pick the screen to share
at each connection; gliff waits up to two minutes for that, or for the user
to accept the connection when RustDesk is set to ask.

## VNC

Any VNC server: wayvnc, TigerVNC, x11vnc, a Mac's screen sharing with "VNC
viewers may control screen with password" on, and the rest. gliff logs in
with:

- no password, when the server asks for none;
- the VNC password, which servers check with DES over a challenge: it keeps
  the password off the network, but nothing else is encrypted, and only its
  first eight characters count;
- VeNCrypt under TLS with a certificate, with a user and password (wayvnc
  with `enable_auth`), the VNC password, or nothing. The certificate is kept
  on first use, as RDP's.

VeNCrypt's anonymous TLS and its user and password without TLS are not
used: give the server a certificate. The picture comes as ZRLE, copied
rectangles or raw pixels. Keys go by their position on the keyboard when the
server takes QEMU's extended key events (wayvnc, TigerVNC, QEMU), as gliff's
own do; otherwise as what a US keyboard types, so another layout's symbols
can differ.

## Trying a machine without the window

```
FOREIGN_PASSWORD=… cargo run -p gliff-foreign --example foreign-check -- \
    rdp://ann@office-pc view 10 frame.ppm
FOREIGN_PASSWORD=… cargo run -p gliff-foreign --example foreign-check -- \
    vnc://pc control 5
```

prints what the host's ports answer, logs in, reports the screen and the
frames, moves the pointer, clicks and types "gliff", and writes the last
frame to a PPM file.
