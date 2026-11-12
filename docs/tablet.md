# A tablet as one more screen

A tablet, or anything with a web browser, can be one more screen of this
desktop without installing an app on it. The screen is served with
`gliff-server --vnc` (see [vnc.md](vnc.md)) and shown by a web page that
speaks VNC over a WebSocket, so it works on old devices such as an iPad 2 on
iOS 8.

## Set up

```
bin/install-tablet
sudo ufw allow from <tablet address> to any port 6080 proto tcp
```

The installer puts the page and its bridge in `~/.local/share/gliff-tablet` (under `XDG_DATA_HOME` when it is set)
(noVNC 0.6.2, the last release that runs on iOS 8's Safari, checked against
its checksum, and websockify in a virtualenv) and two user units:

- `gliff-tablet-vnc`: the new screen, served over VNC on 127.0.0.1:5900
  only. Its side, alignment and size come from `~/.config/gliff/tablet.env`.
- `gliff-tablet-web`: the page and the bridge to that screen, on port 6080.

## Use

The tablet button in gliff's window opens a dialog: where the screen stands
next to this computer's, its size, and the address to open on the tablet,
with a button that copies it. Start Screen starts both units; they keep
running while gliff's window is closed.

Two choices there help an old tablet keep up:

- Colors: 256 colors send one byte a pixel instead of four, from the
  server's palette.
- Frames: 15, 10 or 5 a second space out the page's requests for a new
  frame; the server gathers the changes in between, and the tablet has
  time left to answer touches.

The page connects again with them as soon as they are saved. Changing the
side, alignment or size restarts the screen instead, and its windows move
to the other screens.

The server keeps the screen's mode: a Hyprland config reload, which Omarchy
does when a monitor comes or goes, drops it, and the server sets it back
within two seconds.

On the tablet, open the address. On an iPad, add it to the Home Screen
(Share, then Add to Home Screen) and open it from that icon: Safari before
iOS 12 cannot show a page full screen, but a Home Screen web app can. The
default size, 1024x720, is an iPad's screen less iOS's status bar and the
page's own bar.

The page's bar has:

- Full screen, where the browser has it; on an iPad it explains the icon.
- Disconnect and Connect: the screen and its windows stay on the desktop.
- Remove Screen and Put Screen Back: stops or starts `gliff-tablet-vnc`,
  so the windows move back to the other screens, and come back with it.

The page reconnects by itself after the tablet sleeps or the network drops.
The screen takes one viewer at a time: the newest one that connects holds it,
and the one it replaced says so and stops reconnecting.

## Security

The address carries the VNC password after `#pw=`, so a Home Screen icon
opens without asking for it; the part after `#` is not sent over the
network, but anyone who sees the address can use the screen. The page and
VNC are not encrypted: keep port 6080 open only to your own devices.
