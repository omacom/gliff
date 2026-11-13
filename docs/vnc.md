# VNC: any device as one more screen

`gliff-server --vnc` serves a screen of this machine to VNC clients. With
`--headless` the screen is a new one, sized for the device that shows it, so
an old tablet or laptop with any VNC viewer becomes one more screen of this
desktop: windows move onto it like onto any monitor, and its touch screen,
mouse and keyboard control them.

```
printf '%s\n' 'a-secret' > ~/.config/gliff/vnc-password
chmod 600 ~/.config/gliff/vnc-password
gliff-server --vnc 0.0.0.0:5900 --headless --size 1024x768 \
    --position right --align center \
    --vnc-password-file ~/.config/gliff/vnc-password
```

Then point the device's VNC viewer at this machine's address, port 5900,
with that password. The screen stays while the server runs, so a device
that sleeps or loses the network comes back to the same windows. Stop the
server (Ctrl+C, or SIGTERM) to give the screen back; its windows move to the
other screens.

## Options

- `--vnc ADDR`: where to listen. `0.0.0.0:5900` takes connections from the
  network; `127.0.0.1:5900` only from this machine, for example through an
  ssh tunnel.
- `--headless`: serve a new screen. `--output NAME` serves an existing one
  instead (`auto` for the focused one).
- `--size WIDTHxHEIGHT`: the new screen's size in pixels, normally the
  device's resolution. The default, 1024x768, is an iPad 2's.
- `--scale S`: 2 for a high-density display, where 2048x1536 at scale 2
  shows things at the size 1024x768 would.
- `--position right|left|above|below` and `--align start|center|end`:
  which side of this machine's screens the new one stands on, and how it
  lines up along that side. Without them Hyprland chooses.
- `--vnc-password-file FILE`, or `GLIFF_VNC_PASSWORD`: the password. VNC
  Authentication only uses its first 8 characters.
- `--vnc-no-password`: serve without one. Anyone who reaches the address
  then controls this desktop; use it only with `127.0.0.1`.

## Clients

Any VNC viewer works, including old ones: the server speaks RFB 3.3, 3.7 and
3.8, takes every pixel format a client may ask for (32-bit, 16-bit and 8-bit
true colour in either byte order, and 8-bit colour maps), and encodes with
ZRLE, Hextile or Raw, whichever the client lists first. A full 1024x768
screen is about 135 KB in ZRLE at 32 bits, 85 KB at 16; later updates carry
only the parts that changed.

Typing works for any character, whatever this machine's keyboard layout:
the server types each key a client sends on a keymap of its own. Mouse
buttons, the wheel and touch gestures that viewers turn into them work as
usual.

One client is served at a time. A client that logs in takes over from the
one before, which helps a device that reconnects after sleeping; a client
with a wrong password cannot.

## Security

VNC Authentication keeps the password off the wire, but it is weak by
today's standards, and nothing else is encrypted: anyone on the network can
see the screen and the keys typed on it. Serve VNC on a network you trust,
or listen on `127.0.0.1` and reach it through an ssh tunnel
(`ssh -L 5900:127.0.0.1:5900 this-machine`) from devices that can. A wrong
password waits two seconds before it is refused.

## Not yet

- The clipboard is not shared with VNC clients.
- When this machine's screens are placed automatically by Hyprland (the
  `auto` position, Omarchy's default), `--align end` is centered: Hyprland
  has no position for the end of a side.
