# Teleport vs Parsec: feature catalogue and gap analysis

Date: 2026-10-16. Teleport state: `integration/2026-10-16b` at `6b2c2a9` (main
plus the H7/H9/M2 merges). Parsec state: support site as of 2026-10-16 and
changelog up to 150-105c (2026-10-08).

## How this was researched

- **Parsec.** The support site (support.parsec.app, Zendesk) returns 403 to
  WebFetch, so all 194 en-us articles were downloaded through its public
  Help Center API (`/api/v2/help_center/en-us/articles.json`) and read as text.
  The changelog came from the JSON embedded in https://parsec.app/changelog
  (67 releases, 150-66 to 150-105c). The 240 FPS blog post and the Warp page
  were fetched directly. Every Parsec claim below cites the article it comes
  from. Anything taken from memory or a third party is marked **(unverified)**.
- **Parsec's client.** The shipped Linux client (`parsec-bin` 150_97c from
  nixpkgs) was inspected statically: libraries, embedded strings, its
  configuration keys and settings help text. See section 1.19. The Windows
  installer, Virtual Display Driver and Virtual USB Driver were downloaded
  from `builds.parsec.app` and inspected the same way. See section 1.20.
- **Teleport.** README.md, docs/*.md (especially `docs/review-2026-10.md`),
  the clap structs in `src/host.rs`, `src/client.rs` and `src/main.rs`,
  `src/launcher.rs`, plus spot checks in `src/audio.rs`, `src/ffmpeg_codec.c`
  and the input code. Nothing was built or run.
- **Moonlight/Sunshine and Steam Link** are mentioned only where they show
  what is possible. They were not researched in depth.

Short names for the Parsec sources cited below:

| Key | URL |
| --- | --- |
| ADV | https://support.parsec.app/hc/en-us/articles/32381443626516-All-Advanced-Configuration-Options |
| FM | https://support.parsec.app/hc/en-us/articles/32381463419924-Feature-Matrix |
| HW | https://support.parsec.app/hc/en-us/articles/32381568346644-Hardware-and-Software-Compatibility |
| NET | https://support.parsec.app/hc/en-us/articles/32381460716180-Parsec-Connectivity-Requirements |
| SEQ | https://support.parsec.app/hc/en-us/articles/32361410290324-Components-and-Connection-Sequence |
| OVR | https://support.parsec.app/hc/en-us/articles/32361354307348-Overview |
| SEC | https://support.parsec.app/hc/en-us/articles/32361366289940-Security-At-Parsec |
| WIN | https://support.parsec.app/hc/en-us/articles/32381199341716-Parsec-App-for-Windows |
| MAC | https://support.parsec.app/hc/en-us/articles/32381394408596-Parsec-App-for-macOS |
| LIN | https://support.parsec.app/hc/en-us/articles/32381494397332-Parsec-App-for-Linux |
| WEB | https://support.parsec.app/hc/en-us/articles/32381650129300-Use-the-Web-App-browser |
| AND | https://support.parsec.app/hc/en-us/articles/32381582866452-Install-Parsec-App-on-Android |
| STATS | https://support.parsec.app/hc/en-us/articles/32381603663636-Stream-Overlay-Stats-and-Logging |
| LAG | https://support.parsec.app/hc/en-us/articles/32381352822804-Troubleshooting-Lag-Latency-and-Quality-Issues |
| IMM | https://support.parsec.app/hc/en-us/articles/32361385571860-Immersive-Mode-Setting |
| HK | https://support.parsec.app/hc/en-us/articles/32381778420372-Configure-Hotkeys |
| VSYNC | https://support.parsec.app/hc/en-us/articles/32361386984212-VSync-Setting-In-Parsec-Window |
| GSYNC | https://support.parsec.app/hc/en-us/articles/32361399779092-Latency-Caused-By-FreeSync-and-G-Sync |
| FPS | https://support.parsec.app/hc/en-us/articles/32361393622548-Video-Stream-FPS-Setting |
| RES | https://support.parsec.app/hc/en-us/articles/32361400566804-Changing-the-Resolution-of-the-Video-Stream |
| QUAL | https://support.parsec.app/hc/en-us/articles/32381785123860-Improve-Stream-Quality-and-Color-Accuracy |
| QP | https://support.parsec.app/hc/en-us/articles/32381815770772-Encoder-Video-Quality-Settings |
| BR | https://support.parsec.app/hc/en-us/articles/32381776828820-Setting-The-Maximum-Bitrate-Of-The-Stream |
| MM | https://support.parsec.app/hc/en-us/articles/32381733729044-Multiple-Monitors-and-Virtual-Displays |
| VDD | https://support.parsec.app/hc/en-us/articles/32381178803604-VDD-Overview-Prerequisites-and-Installation |
| VDDADV | https://support.parsec.app/hc/en-us/articles/32361359271444-VDD-Advanced-Configuration |
| PRIV | https://support.parsec.app/hc/en-us/articles/32361381211284-Privacy-Mode |
| HEADLESS | https://support.parsec.app/hc/en-us/articles/32381854731156-Running-Parsec-On-A-Headless-Gaming-PC-Or-A-Server |
| PERM | https://support.parsec.app/hc/en-us/articles/32381747079572-Hosting-and-Permissions |
| MAXC | https://support.parsec.app/hc/en-us/articles/32361376782228-Max-Client-Connections-To-Your-Host |
| COOP | https://support.parsec.app/hc/en-us/articles/32381846885780-People-Are-Controlling-the-Same-Player-Co-Op-Controls-Aren-t-Working |
| GP | https://support.parsec.app/hc/en-us/articles/32381705301908-Setup-Gamepad |
| MKB | https://support.parsec.app/hc/en-us/articles/32381827815188-Mouse-and-Keyboard-Isn-t-Working-Correctly-When-Connected |
| MOUSE | https://support.parsec.app/hc/en-us/articles/32361352556180-Mouse-Is-Moving-Too-Fast-or-Slow-In-Some-Scenarios |
| SWAP | https://support.parsec.app/hc/en-us/articles/32361367389972-Swap-Command-and-Ctrl-for-MacOS |
| WACOM | https://support.parsec.app/hc/en-us/articles/32381684728852-Using-Wacom-Tablets |
| MIC | https://support.parsec.app/hc/en-us/articles/32380350695956-Use-your-Microphone-with-Parsec |
| MCA | https://support.parsec.app/hc/en-us/articles/46395533300372-Multi-Channel-Audio |
| AB | https://support.parsec.app/hc/en-us/articles/32361377721364-Audio-Buffer-Setting |
| ABR | https://support.parsec.app/hc/en-us/articles/32381530145172-Increase-Your-Audio-Bitrate |
| APPS | https://support.parsec.app/hc/en-us/articles/32381776666388-Configure-Approved-Apps |
| IDLE | https://support.parsec.app/hc/en-us/articles/32361536128148-Idle-Kick-Timer |
| YUBI | https://support.parsec.app/hc/en-us/articles/33164294687380-Yubikey-Passthrough |
| HPR | https://support.parsec.app/hc/en-us/articles/32381058430740-High-Performance-Relay-HPR-Server-Overview-and-Prerequisites |
| LOGIN | https://support.parsec.app/hc/en-us/articles/32381618319124-Access-Login-Screen-on-macOS |
| GUEST | https://support.parsec.app/hc/en-us/articles/32381271120532-Guest-Access-to-a-Team-for-Events-User-Research-and-More |
| SESS | https://support.parsec.app/hc/en-us/articles/32361341012372-Session-Duration-Settings |
| E15106 | https://support.parsec.app/hc/en-us/articles/32361385772052-Error-Codes-15106-Your-Encoder-Cannot-Capture-Your-Display |
| COLORS | https://support.parsec.app/hc/en-us/articles/32381130731924-Issues-with-the-video-stream-having-wrong-colors-glitching-flickering-or-blinking |
| CL | https://parsec.app/changelog |
| B240 | https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5 |
| WARP | https://parsec.app/warp |

---

## 1. Parsec catalogue

### 1.1 Platforms

| Role | Windows | macOS | Linux | Android | iOS/iPad | Web | Raspberry Pi |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Host | Yes (Win 10/11, Server 2016+) | Yes (10.15+; limited, see below) | **No** ("Ubuntu does not support hosting") | No | No | No | No |
| Client | Yes (64-bit Win 10/11 only) | Yes (10.15+) | Ubuntu 22.04 only (X11; runs on XWayland) | Yes, "experimental, a lot of bugs" (Android 9+) | **No** | Chrome/Chromium only (WebRTC, software decode, cannot join macOS hosts) | Deprecated Oct 2022 |

Sources: HW, LIN, AND, WEB, FM. Linux client dependencies include
`libX11` and FFmpeg `libavcodec.so.58` (LIN). The changelog sets Ubuntu 22.04
as the minimum Linux client from 150-103a (CL).

macOS host limits: "best for productivity, and do not have controller or full
support for mouse in first person games" (PERM). The feature matrix lists no
gamepad, "gaming-capable mouse", microphone, camera, YubiKey, Approved Apps,
echo cancelling or 4:4:4 on macOS hosts, and resolution is "only adjustable
from macOS settings" (FM).

### 1.2 Tiers

Free / Warp ($9.99 a month, or $8.33 billed annually) / Teams ($30 per user per
month) / Enterprise ($45 per user per month) (WARP). Warp-only features: up to
3 monitors at once (5 from 150-105a, CL), 4:4:4, privacy mode, Wacom
pressure/tilt, virtual displays, camera passthrough (FM, WARP, CL).
Enterprise only: High Performance Relay (HPR), REST API (OVR, HPR).

### 1.3 Every client setting (defaults)

From ADV unless noted. Config file `config.txt`/`config.json`; Windows
registry policy `HKLM\SOFTWARE\Parsec\Configuration` overrides it (WIN). Teams
precedence: admin group override > global > registry > local file (ADV).

| Setting | Key = default | Notes |
| --- | --- | --- |
| VSync | `client_vsync=1` (on) | "increases latency, but eliminates screen tearing"; off can still be tear-free on some Intel decoders; LAG also says VSync can smooth frame delivery |
| Window mode | `client_windowed=0` (fullscreen) | macOS fullscreen uses Spaces (MAC) |
| Auto-open extra screens | `client_automatic_displays=false` | Opens up to 3 host screens (Warp/Teams; CL 150-97c) |
| Overlay button | `client_overlay=1` | |
| Overlay warnings | `client_overlay_warnings=1` | Decode, network and privacy-mode warnings (STATS) |
| Swap Cmd/Ctrl | `client_macos_key_swap=true` | Swaps Ctrl and Command/Win between Mac and Windows (SWAP) |
| Decoder | `decoder_software=0` (hardware) | Windows offers NVIDIA, Intel, AMD, Software, FFmpeg (WIN) |
| H.265 | `client_decoder_h265=1` (on) | Falls back to H.264 if any party lacks support |
| Renderer | `client_renderer=3` (D3D11 on Windows) | OpenGL on Linux, Metal on Mac, D3D12 beta, Vulkan experimental |
| HID mode (macOS) | `app_hid_keyboard=true` | Lower-level keyboard capture; needs Accessibility |
| Immersive mode | `client_immersive=0` | 0 off, 1 both, 2 keyboard, 3 mouse |
| Mic passthrough | `client_mic_passthrough=false` | Windows/macOS client, Windows host (MIC) |
| 10-bit colour | `client_decoder_10bit=false` | HEVC only; "minimally better color accuracy at the cost of latency" |
| Zero copy (Windows) | `client_zero_copy=false` | Decoder output stays in VRAM, D3D11 only (experimental since 150-96a, CL) |
| Client UDP port | `network_client_start_port=0` | 0 picks a pseudorandom port |
| Audio buffer | `client_audio_min_buffer_ms=50`, `client_audio_max_buffer_ms=100` | Min defaults to half of max (AB) |
| Enhanced pen (Warp) | `client_enhanced_pen=1` | Pressure, tilt, rotation |
| Prefer 4:4:4 (Warp) | `client_decoder_444=false` | Hardware decode on NVIDIA Turing+ and Intel 11th gen+; CPU otherwise |
| Web UI preview | `client_web_ui=false` | |
| Input compatibility, "Always Relative" mouse | in-app overlay | New in 150-105a; 150-102 added "compatibility option to prevent client mouse warps" (CL) |

### 1.4 Every host setting (defaults)

| Setting | Key = default | Notes |
| --- | --- | --- |
| Hosting enabled | `app_host=1` (Windows), 0 (macOS) | |
| Host name | `host_name=` | |
| Stay awake (macOS) | `host_prevent_sleep` | ADV lists default false; text contradictory |
| Resolution (Windows) | `server_resolution_x/y=0` | 0 = use the client's resolution; 65535 = keep. Owner connections only. Changes the **real display mode** and reverts on disconnect (RES) |
| Orientation | `host_rotated=false` | Rotation from the overlay since 150-92 (CL) |
| Bandwidth limit | `encoder_bitrate=10` Mbps | Shared by all guests; "going higher than 50 will definitely have a material impact on your latency" (BR) |
| Lock desktop | `host_lock_desktop=false` | Lock on last disconnect if the owner was first |
| Kick guests on owner leave | `host_protective_mode=false` | |
| Fallback to virtual display | `host_virtual_monitor_fallback=true` | Free tier; Windows; needs the VDD driver |
| FPS (Windows) | `encoder_fps=60` | "above 60 may cause instability"; tries to change the display refresh rate to match. FPS says 0 means match the display |
| Block daisy-chaining | `app_daisychaining=1` | |
| Idle kick | `host_idle_kick_time=0` | Minimum 5 minutes; only button, key or pen input counts (IDLE) |
| Exclusive input | `host_exclusive_input=0` | One guest controls the mouse at a time |
| Echo cancelling | `host_audio_cancel=1` | Legacy, or "New" per-app exclusion (`echo_app_selection=discord.exe`) |
| Virtual microphone | `host_virtual_microphone=1` | 2 = persistent; needs the VUSB driver |
| Gamepad driver | `host_gamepad_mode=0` | Parsec VUSB, else ViGEm |
| Virtual gamepad type | `host_gamepad_type=1` | 1 Xbox 360, 2 DualShock 4, 3 DualSense |
| Virtual mouse | `host_virtual_mouse=1` | Keeps the Windows cursor visible on headless hosts (HEADLESS) |
| Quality | `encoder_min_qp=5` | 5 "Lowest Latency" (default), 0 "Balanced", 64 "Highest Quality" (multi-pass); "automatically scales... based on network performance" (QP) |
| Approved Apps | `host_require_whitelist=0` | Freezes the stream and guest input when a non-approved app has focus (APPS) |
| Host start port | `network_server_start_port=0` | One sequential UDP port per guest |
| Congestion algorithm | `network_cg_level=1` | 1 new sensitive, 2 new relaxed, 0 old |
| Mute host speakers | `server_admin_mute=1` | Owner connections |
| Max clients | `server_max_clients=20` | (MAXC) |
| Constant FPS | `host_full_fps=false` | "May improve quality on the desktop after the screen goes static" |
| Audio bitrate | `host_audio_bitrate=128` (kbps), up to 512 | (ABR) |
| Virtual displays (Warp) | `host_virtual_monitors=0` | Up to 3 (5 from 150-105a); owner connections; Win 10 1607+/Server 2019+ need VDD; macOS is driverless |
| Privacy mode (Warp) | `host_privacy_mode=0` | Physical displays off while connected, lock on last disconnect (PRIV) |
| Virtual tablet (Warp) | `host_virtual_tablet=false` | Virtual Wacom Intuos Pro M with ExpressKeys and Touch Ring (WACOM) |
| UI display/audio picker | in app | Monitor and GPU selection; audio device selection with fallback to the default device (WIN) |

Other: `network_raw_audio=0` (Opus; 1 = uncompressed 48 kHz stereo PCM),
`app_copy_paste` (clipboard on by default), `app_log_level`, `app_proxy*`
(HTTP proxy), `app_watermark`, `app_friends`. Enterprise:
`app_stun_address`, `app_client_stun_address` (HPR addresses),
`app_force_relay` (1 WAN only, 2 WAN and LAN), `network_fast_relay_ping`.
Network-tab UPnP toggle (WIN, MAC, LIN).

### 1.5 Codecs and colour

- H.264 and H.265. H.265 is tried by default and the whole session falls back
  to H.264 if any party lacks it (ADV, HW). Host H.265 needs NVIDIA GTX 900+,
  AMD Radeon/Carrizo+, or Intel Cherry Trail/Braswell+ (HW).
- **No AV1** is mentioned anywhere in the support corpus or the changelog
  (absence verified only in those two sources).
- 4:4:4 (Warp/Teams): Windows host only, H.265, NVIDIA Pascal+ or Intel 11th
  gen+. Client hardware decode on NVIDIA Turing+ or Intel 11th gen+, otherwise
  CPU or FFmpeg (HW).
- 10-bit: experimental, HEVC. Delta-E ~0.5 between modern Intel/NVIDIA (QUAL;
  CL 150-84). Full-range colour is automatic, but disabled on AMD encoders
  (CL 150-84b).
- **HDR is not supported.** "Parsec doesn't support HDR. If the host computer
  has it turned on, turn it off" (E15106). HDR on the host washes out colours
  (COLORS). Since 150-81, Windows NVIDIA HDR hosts are allowed with "color
  inaccurate" output (CL).
- Up to 7680x4320 with H.265 since 150-96a (CL). Marketing: "4K at 60"
  (WARP).

### 1.6 Encoders per GPU vendor (host)

- NVIDIA NVENC + DirectX 11.1, GTX 600 or later. GT/MX parts without NVENC are
  unsupported (HW).
- Intel Quick Sync + DX11.1, Haswell or later (HW).
- AMD VCE/VCN + DX11.1, GCN 1 or later. RX 6500 XT and 6400 have no encoder
  (HW). Parsec itself says "AMD is usually known for having worse encode"
  (LAG).
- Software encoder fallback since 150-89d, with a red warning (CL, LAG).
- macOS host encoder: VideoToolbox **(unverified, not documented)**.
  Changelog mentions M1 Pro/Ultra encoder performance work (CL 150-82a).
- OVR: "zero-copy GPU pipeline to the encoder".

### 1.7 Frame rate and resolution

- Default 60 FPS. "Above 60 may cause instability" (ADV, WIN). Parsec's own
  2017-era test streamed 240 FPS on LAN, "roughly 4–8 milliseconds" for the
  whole pipeline, "2 frames behind" with VSync on, on a GTX 1070 host and
  GTX 1050 Ti client. It adds "We do not recommend using Parsec at 240 FPS. We
  only test the software at 60 FPS" (B240).
- Parsec does **not** scale the stream: "The resolution of the Parsec stream
  is always the same as resolution of the host's display" (RES). Matching is
  done by changing the host display mode ("Use Client Resolution", default for
  owners) or adding a virtual display at the client's resolution (CL 150-98).
- Resolution presets were added in 150-89d, including ultrawide 3440x1440 and
  3840x1600 (CL).

### 1.8 VSync and VRR

- Client VSync is on by default and can be turned off (VSYNC).
- No client-side VRR or frame-pacing setting is documented. The only VRR text
  advises **turning G-Sync/FreeSync off on the host** because it adds
  latency (GSYNC, LAG).

### 1.9 Latency features

- Hardware decode by default, with vendor-specific decoders and software or
  FFmpeg fallback. Faulting decoders are skipped automatically (CL 150-94).
- D3D11 zero-copy decode (experimental, Windows) (ADV).
- Quality slider (min QP), bandwidth limit, two congestion algorithms
  (ADV).
- "BUD" (Better User Datagrams): their own UDP protocol, "97% NAT traversal
  success rate", "ultra-responsive dynamic bitrate adjustment", DTLS 1.2
  (OVR, SEC).
- Damage-based sending: no frames when the screen is static, unless Constant
  FPS is on (STATS).
- Claim: "On our test setup on a LAN ethernet connection, Parsec adds only 7
  milliseconds of latency" (OVR). No methodology is given.
- Low bandwidth: no buffering "due to the additional lag". Advice is to lower
  bitrate, resolution or 30 FPS by hand. There is no automatic
  resolution step-down (https://support.parsec.app/hc/en-us/articles/32361399881620-How-To-Use-Parsec-On-Low-Bandwidth-Connections).

### 1.10 Networking

- Signalling over a WebSocket API (`kessel-ws.parsec.app:443`). STUN at
  `stun.parsec.app:3478` (IPv4) and `stun6` (IPv6). UPnP port mapping is on
  by default (permanent lease if temporary ones fail, CL 150-99). Both peers
  attempt the connection at once, the "simultaneous open" (SEQ, NET).
- Requirements: "Host and client must not be inside a 'Double NAT' or CGNAT
  network" (NET). The suggested workaround is ZeroTier
  (https://support.parsec.app/hc/en-us/articles/32381869150868-Using-a-VPN-ZeroTier-to-Connect-on-Parsec).
- **There is no relay for Free or Warp users.** HPR is customer-hosted on
  Linux and part of the Teams Enterprise licence. It uses UDP 5000 for BUD
  and 4900 for STUN, has a fast-relay ping and force-relay modes (HPR, ADV).
- Ports: random by default, settable host start port (one per guest) and
  client port (ADV, WIN).
- Encryption: DTLS 1.2 (AES-128 per SEC; AES-128/256 per OVR). Per-user
  certificates are "validated via the Parsec backend", with a one-time
  connection token checked by the backend (SEC). The trust anchor is Parsec's
  backend, not the peer.
- The web client uses WebRTC, which "fails to negotiate a connection more"
  often (WEB).

### 1.11 Input

- Keyboard immersive mode (Alt+Tab, Win keys) and mouse immersive mode (pointer
  confinement). Ctrl+Shift+I toggles; Ctrl+Alt+Z detaches input until the next
  click. macOS needs HID mode and Accessibility (IMM, HK).
- Configurable hotkeys: Disconnect Ctrl+Alt+\`, Menu Ctrl+Shift+M, Chat
  Ctrl+Shift+C, Switch Display Ctrl+Shift+D, Add Screen Ctrl+Shift+S, Enhanced
  Pen Ctrl+Shift+P, Send Ctrl+Alt+Del Ctrl+Alt+End, Window mode Ctrl+Shift+W,
  Immersive Ctrl+Shift+I, Detach Ctrl+Alt+Z. Host: Accept All Ctrl+F1, Kick All
  Ctrl+F3. "Ignore Hotkey" stops one chord being forwarded (for push-to-talk)
  (HK, WIN).
- Relative mouse: "Always Relative" mouse mode from 150-105a; a no-warp
  compatibility option from 150-102 (CL). Before that, cursor speed
  mismatches when games warp the pointer were documented (MOUSE).
- Cursor: sent as an image and drawn on the client. Cursor images are cached
  (CL 150-88a); there were cursor sizing and sharpness fixes (CL 150-93B,
  150-95a) and a cursor-scaling crash fix (CL 150-105a). That this is
  client-side drawing is **inferred from the changelog**, not stated.
- Gamepads: forwarded through the Parsec VUSB driver (or ViGEm). Emulated
  type Xbox 360 (default; buttons and vibration), DualShock 4 (adds touchpad),
  or DualSense (touchpad, haptics, adaptive triggers, motion; needs a USB
  DualSense on a Windows client). Gamepads are Windows-host only. The client
  has per-controller remapping (Map/Unmap/Show raw data). Guests attach a
  pad by pressing a face button (GP, FM).
- Pen: Windows Ink injection, or a virtual Wacom Intuos Pro (WinTab, pressure,
  tilt, rotation, ExpressKeys, Touch Ring); Warp/Teams only (WACOM).
- Touch: Android client touch input (cursor hidden on touch) (MKB). No
  touchscreen forwarding to the host is documented.
- Function keys up to F19 (CL 150-88a). Swap Cmd/Ctrl (SWAP).
- Known: anti-cheats such as Vanguard block injected input (MKB).

### 1.12 Audio

- Opus by default (or raw PCM), 48 kHz. Stereo, with 5.1/7.1 since 150-102
  (Windows) and 150-104a (macOS), only when host and client speaker layouts
  match (MCA, CL).
- Host audio bitrate 128 kbps by default, up to 512 (ABR). Client buffer
  50–100 ms (AB).
- Picks the host audio device and follows default-device changes (CL 150-76;
  WIN).
- Mic passthrough from a Windows/macOS client to a Windows host's virtual mic
  (VUSB). Owner connections only, mutable from the overlay (MIC, CL 150-92).
- Echo cancelling integrated with Discord, or per-app exclusion (ADV).
- Mutes the host speakers on owner connection (ADV).

### 1.13 Displays

- Switch display (free). "Add Screens": up to 3 windows (5 from 150-105a),
  each with its own bitrate, resolution, chroma and FPS (Warp/Teams) (MM, CL).
- Virtual displays: up to 3 (now 5), owner connections only, removed on
  disconnect. Custom resolutions via the registry (up to 5), parent-GPU
  selection, and the client resolution added dynamically (VDD, VDDADV, CL
  150-98).
- Privacy mode: physical displays off and the host locks on last disconnect
  (Windows needs VDD; macOS keeps physical displays connected) (PRIV).
- Headless hosting: VDD, or a dummy HDMI plug, plus the virtual mouse
  (HEADLESS).

### 1.14 Clipboard and file transfer

- Text clipboard sync is on by default (`app_copy_paste`) (ADV). There is a
  size limit: large pastes are dropped instead of disconnecting (CL).
- **No file transfer** in the support corpus or changelog (absence verified
  only there).

### 1.15 Multi-user and co-op

- Up to 20 concurrent guests by default (MAXC). One encode is shared:
  "bandwidth is split between the people connected" (ADV).
- Guests get **controller-only** permission by default. The host grants
  keyboard and mouse per guest. Per-friend defaults include "Can connect
  without your approval". The host must Accept each request (Ctrl+F1) or it is
  rejected. Kick all with Ctrl+F3 (PERM, LIN).
- Host UI shows per-guest latency and live mouse/keyboard/pad activity icons
  (WIN, MAC).
- Exclusive input mode and idle kick (ADV).
- Co-op guidance: keyboards and mice all merge into one; separate players need
  pads (COOP).
- In-session text chat (Ctrl+Shift+C) (HK).

### 1.16 Sharing, accounts and Teams

- A Parsec account is mandatory. Email confirmation for new devices,
  optional TOTP 2FA (SEC).
- Friends list, share links (host must accept), peer IDs (PERM, LIN).
- Arcade was removed after 15 April 2023 (CL).
- Teams: SAML SSO, SCIM, groups and roles, audit logs, app rules (pushed
  config), guest access with time windows up to 5 days and guest credits,
  session duration policy (5 minutes to 6 months), domain and IP
  verification, Teams API keys, silent installer provisioning (GUEST, SESS,
  WIN, and the support-site Teams articles).
- Login-screen access: Windows (service runs as SYSTEM), and macOS with a
  separate installer that needs FileVault off (LOGIN, WIN).
- Camera passthrough (Windows 10/11 client to Windows 11 host, Warp/Teams;
  Mac client from 150-101) and YubiKey 5 USB passthrough (Windows 11 to
  Windows 11) (CL, YUBI).

### 1.17 Stats overlay (STATS)

- Overlay header: decode latency, encode latency, network RTT, bitrate,
  decoder type, codec, resolution, 4:2:0 or 4:4:4, Full Range, 10-bit, audio
  codec.
- Metrics graph (Basic, Audio or Advanced): host/client video total, host
  capture ms, host encode ms, client decode ms, frame time, P2P RTT, P2P
  bitrate, audio encode/decode/frame time.
- Warnings: decode too slow, network loss/congestion, privacy-mode failure,
  and a red software-encoding warning (LAG).
- Host console line every few seconds: `FPS:sent/skipped L:enc+capture avg/max
  B:bitrate/max N:slow retransmits/fast retransmits/congestion events`.

### 1.18 Known limitations, from Parsec's own docs

Linux cannot host. The macOS host has no gamepads, no gaming mouse and no
4:4:4. There is no HDR, no iOS, no file transfer, no relay below Enterprise,
and CGNAT/double NAT is unsupported. Android is experimental. The web client
uses software decoding over WebRTC. FPS above 60 is "unstable" and 240 is
not recommended. G-Sync/FreeSync on the host adds latency. Hybrid-GPU laptops
must use the iGPU (-14003). Anti-cheat blocks input. A paid tier is needed for
multi-monitor, 4:4:4, virtual displays and pen. A cloud backend is required
for signalling and authentication. (HW, FM, E15106, NET, HPR, GSYNC, MKB.)

### 1.19 The Linux client, inspected (2026-10-17)

The manual was checked against Parsec's shipped Linux client: nixpkgs
`parsec-bin` 150_97c, a small `parsecd` launcher plus the engine
`parsecd-150-97c.so` (3.4 MB, entry `wx_main`). Static inspection only
(linked and `dlopen`ed libraries, embedded strings and settings text); it was
not run or connected.

- **Display and input: X11 only.** The engine links `libX11`, `libXi`,
  `libXfixes` and `libXcursor` and has no Wayland or SDL; on a Wayland desktop
  it runs under XWayland. Renderers: OpenGL and Vulkan on Linux (D3D11/12 and
  Metal elsewhere). The settings UI is Dear ImGui, plus an optional web UI.
  The app layer is libmatoya (snowcone-ltd), Parsec's open-source
  windowing/input/audio library.
- **Decoding through the system's FFmpeg.** It `dlopen`s whichever
  `libavcodec` 57–61 is installed, with no VA-API/VDPAU/NVDEC libraries of its
  own, so hardware decoding is whatever that FFmpeg offers. 4:4:4, 10-bit and
  H.265 are client feature flags (`client_decoder_444/10bit/h265`), with
  H.265 "falling back to H.264 if both computers don't support it". No AV1
  decoder strings were found.
- **The cursor is drawn client-side** (answers open question 2): cached
  cursor images (`_cache_cursor`), PNG cursors from the host
  (`client_png_cursor`), applied as real X11 cursors
  (`XCreatePixmapCursor`/`XDefineCursor`).
- **Transport:** usrsctp (SCTP, as in WebRTC) strings throughout, i.e. SCTP
  over UDP with OpenSSL DTLS; a user-selectable "Congestion Algorithm"
  (`network_cg_level`: "behavior when network conditions deteriorate"). No
  FEC, NACK or parity strings were found (open question 3: no visible FEC).
  NAT traversal: STUN on `stun.parsec.app` and `stun6.parsec.app` (IPv6),
  candidate exchange ("candex"), relay keys (`app_force_relay`,
  `answer_relay`), and a SOAP client consistent with UPnP-IGD
  (`network_upnp`). Backend hosts: `kessel-api`, `public`, `builds` and
  `deps.parsec.app`.
- **Settings:** the engine recognises about 148 configuration keys; the
  manual's ADV list documents 83. Undocumented but shipped:
  - encoder internals: `encoder_slices`, `encoder_idr_interval`,
    `encoder_min_qp`/`max_qp`, `encoder_vbv_initial/max/multi`;
  - `host_software_encode` and `host_encoder_min_bitrate`;
  - `network_protocol` and `network_video_container`;
  - client audio buffers (`client_audio_min/max_buffer_ms`) and
    `client_bad_decoders`.
- **Features visible only in the client's help text:**
  - **Uncompressed audio:** 48 kHz stereo PCM instead of Opus, "when host
    bandwidth is not limited" (`network_raw_audio`).
  - **USB passthrough:** generation-5 YubiKeys via libusb, owner connections
    only (`client_usb_passthrough`, `host_virtual_usb_passthrough`).
  - **Per-hotkey exclusions:** keep a hotkey on the client ("Useful for your
    Discord push-to-talk hotkey").
  - **Ctrl/Command swap** between macOS and other systems.
  - Extra client windows for multiple monitors.

### 1.20 The Windows build, inspected (2026-10-17)

Downloaded from `builds.parsec.app` and unpacked with 7-Zip; static inspection
only, nothing was run. Packages (SHA-256 prefix):
- `package/parsec-windows.exe` (4.4 MB, b8a9cd51…), the installer;
- `vdd/parsec-vdd-0.45.0.0.exe` (e2333244…), the Virtual Display Driver;
- `vud/parsec-vud-0.3.0.0.exe` (fb73da59…), the Virtual USB Driver.

The installer holds the `parsecd.exe` launcher, the engine
`parsecd-150-104a.dll` (6.2 MB, three releases newer than Linux 150-97c),
`pservice.exe`, `teams.exe`, and nested VDD and VUD installers.
`setup.json` selects a shared ProgramData install.

- **Codecs and GPUs without FFmpeg.** The engine loads vendor SDKs directly:
  - encoding: NVENC (`nvEncodeAPI64`), AMD AMF (`amfrt64`, with
    `AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY`), and Intel oneVPL/Media SDK
    (`libvpl`, `libmfxhw64`);
  - decoding: NVDEC (`nvcuvid`) and D3D11/D3D12, with an option to keep
    decoder output in video memory (D3D11 only), i.e. zero-copy
    presentation like Teleport's;
  - 4:4:4 on Windows asks the user to download FFmpeg separately.
  AV1 parameter structures (`AV1BitstreamParam`, `AV1TileParam`,
  `AV1FilmGrainParam`) are present, so AV1 code paths exist on Windows even
  though no Parsec documentation mentions AV1 (open question 6). Encoder
  internals match the hidden keys in 1.19 (`encoder_slices`,
  `encoder_vbv_*`, long-term reference strings).
- **Virtual Display Driver.** `mm.dll` is a UMDF Indirect Display driver
  (IddCx 1.2, `UmdfLibraryVersion 2.25.0`): "Parsec Virtual Display
  Adapter", a root device `Root\Parsec\VDA`, installed with Nefarius's
  `nefconw`. The engine uses it to:
  - add extra virtual displays during owner connections;
  - add one when no monitor is present ("Fallback To Virtual Display",
    headless hosting);
  - run privacy mode, which extends to a virtual display and blanks the
    real ones.
  This is the Windows form of gap G17.
- **Virtual USB Driver.**
  - `parsecvusba.sys` is a virtual USB host controller (`Root\Parsec\VUSBA`)
    with 15 persisted device slots.
  - `parsecvirtualds.sys` is a demand-start kernel service, "Parsec Virtual
    DS".
  - Guests' gamepads go through VUSB, falling back to ViGEmBus 1.17
    automatically. DualShock 4 (with touchpad) and DualSense are supported.
  - YubiKey passthrough (1.19) uses the same bus.
  This is the Windows form of gap G13.
- **Unattended access through `pservice.exe`.** A service running as
  SYSTEM:
  - enumerates sessions (WTSAPI), duplicates the token of `winlogon.exe`,
    and launches `parsecd.exe` into the active console session with
    `CreateProcessAsUserW`, so Parsec can see and drive the login screen,
    lock screen and UAC prompts;
  - sends Ctrl+Alt+Del through `SAS.dll`;
  - talks to the app over `\\.\pipe\PARSEC-NP`;
  - checks signatures with WinVerifyTrust and carries an embedded RSA public
    key.
  This is how gap G24 is solved on Windows. On Linux the equivalent is a
  system service with greeter/logind integration; Teleport's `systemLogin`
  mode covers part of it.
- **Host features only in the Windows help text:**
  - virtual microphone for mic passthrough, plus echo cancellation for
    Discord and "certain apps" (G20);
  - virtual camera passthrough of a client webcam (Windows 11);
  - a virtual Wacom Intuos Pro (M) for tablets, and enhanced pen (tilt,
    rotation, pressure via Windows Ink);
  - a dummy virtual mouse so a headless host still shows a cursor;
  - "Allow guests to only see and interact with certain applications";
  - automatic host resolution change for owner connections;
  - lock the desktop when the last guest leaves, and kick non-owner guests
    when the owner disconnects;
  - one guest controls the mouse at a time, and the host can always take
    control back;
  - excluding an app from the streamed audio;
  - a bandwidth cap split between all guests;
  - an idle kick after a set number of minutes;
  - "Prevents this machine from simultaneously acting as a host and a
    client" (daisy-chaining guard).

---

## 2. Teleport today (relevant to this comparison)

What is on `main`/this branch, from README, docs and code:

- **Host:** Linux only (Wayland portal plus PipeWire, or X11). One client at a
  time. Encoders: NVENC, QSV, VA-API (AMD/Intel), x264/x265 fallback, probed in
  that order (`--encoder auto`).
  - NVENC preset `p1`/`ull`/CBR, zero-latency, no B-frames
    (`src/ffmpeg_codec.c:140-152`).
  - DMA-BUF zero-copy capture into NVENC and VA-API (M2). Not covered: QSV,
    software encoders, HDR, X11 and AMD.
- **Client:** Linux and macOS (SDL). Hardware decode probe (NVDEC, VA-API,
  QSV, VideoToolbox) with software fallback and mid-session fallback.
  - Zero-copy presentation for CUDA→GL and VA-API→EGL (M1). macOS NV12 and
    CVPixelBuffer→Metal still need hardware verification (M3). Mac Metal vsync
    (H10) is deferred.
  - The SDL canvas is created without `present_vsync`
    (`src/client.rs:765`, `src/multi_display.rs:156`); there is no VSync
    setting.
- **Codecs:** H.264 or H.265. (Since 2026-10-16, G5: `--codec auto`, the
  default, picks H.265 when both ends have it in hardware.) HDR10: HEVC Main10 on the experimental patched-KWin
  path to a Mac Metal/EDR client (docs/hdr.md). No AV1, no 4:4:4.
- **Rates:** host `--fps` 1..=120 (`src/host.rs:74`). The launcher offers
  30/60/120 FPS, 8/20/40 Mbps (CLI 0.5–100 Mbps), and native or scaled widths
  (720p, 1080p, 1440p, 4K). The stream is **scaled** to the requested width;
  the host display mode is never changed.
  - **In progress (not counted as gaps):** 180/240 FPS with a negotiated rate,
    resolution matching, VRR, Opus buffer tuning.
- **Network:**
  - QUIC via MoQ (moq-lite-05). Direct UDP 4443 needs reachability (port
    forward or VPN). (Since 2026-10-16, G1: with a relay, `auto` also hole
    punches a direct path through NATs, using STUN and opt-in NAT-PMP.)
  - Optional E2E-encrypted relay: self-hosted `teleport relay` or a provider
    (Cloudflare not yet verified). Route `direct`/`relay`/`auto` (2.5 s
    direct, then relay). Signed presence.
  - QUIC idle 8 s and keep-alive 1 s, MTU discovery, keyframe on request, 4 s
    safety GOP, RTT/loss-based rate controller (H12). Pointer motion goes as
    latest-wins datagrams.
- **Security and auth:** pinned host fingerprint (SHA-256). Code pairing
  (SPAKE2), Teleport account (OPAQUE), optional PAM login, U2F (NEO)
  enrollment. Per-device Ed25519 keys, revocation in Host settings, no cloud
  account.
- **Input:**
  - Physical scancodes; Cmd→Super and Option→Alt (fixed). Keyboard capture
    with Ctrl+Alt+K. Mouse lock (relative) with Ctrl+Alt+M.
  - Disconnect with Ctrl+Alt+Q; hotkeys are fixed.
  - Mouse buttons left, middle, right and (since 2026-10-16, G14)
    back/forward. Precise wheel deltas are forwarded.
  - Gamepads: SDL to uinput Xbox 360, up to 4, with rumble (since 2026-10-16,
    G12). No motion, touchpad or other pad types.
  - Local cursor on X11 hosts only; on Wayland the cursor stays in the video.
- **Audio:** desktop output only (an explicit `.monitor`). Opus stereo 128
  kbps with 10 ms frames, no in-band FEC (M8). The client queue is ≤100 ms,
  leaky (`src/audio.rs`). No microphone.
- **Displays:** select a monitor; multiple independent remote monitors map to
  up to 4 extra local fullscreen windows, each with its own stream. Virtual
  displays and privacy mode on KDE since 2026-10-17 (G17). No hotplug.
- **Clipboard:** explicit text-only Send and Get, 64 KiB, opt-in on both
  ends. No file transfer.
- **Stats (F8):** requested, accepted and current bitrate. Control RTT
  (includes scheduling). Encoded payload. Received, decoded and displayed FPS.
  Skipped groups. Decoder queue. Receive→decoded, decode stages, host encode
  time, decoder name, renderer and pixel path. No capture time, no
  glass-to-glass number, no warnings.
- **Distribution:** Nix (flake, NixOS module, systemd user unit). A native
  `.app` that depends on the Nix store; no signed installer and no Windows
  build.

---

## 3. Feature-by-feature comparison

Legend: **Has** = shipped on main; **Partial** = exists with material limits;
**WIP** = being built now; **Missing**.

| Area | Parsec | Teleport | Status | Notes |
| --- | --- | --- | --- | --- |
| Windows host | Yes | No | Missing | Most gamers' gaming PC runs Windows |
| macOS host | Yes (no gamepad or FPS mouse) | No | Missing | Low priority |
| Linux host | **No** | Yes (Wayland + X11) | **Teleport ahead** | Including SteamOS-style setups (unverified on SteamOS) |
| Windows client | Yes | No | Missing | Largest client population |
| macOS client | Yes | Yes | Partial | Mac latency items M3/H10 unverified on hardware |
| Linux client | Ubuntu 22.04, X11 | Any Nix Linux, X11 or Wayland | **Teleport ahead** | |
| Android / iOS / web | Android experimental; no iOS; Chrome web | None | Missing | No iOS from Parsec is an opening |
| H.264 / H.265 | Yes, HEVC tried by default with fallback | Yes, auto: H.265 when both ends have hardware (2026-10-16) | Parity | G5 |
| AV1 | No | No | Parity | Moonlight/Sunshine have it; an opening |
| 4:4:4 | Warp, Windows host, NVIDIA/Intel | No | Missing | |
| 10-bit SDR | Experimental | Main10 only for HDR | Partial | |
| HDR | **Not supported** | Experimental (patched KWin → Mac EDR) | **Teleport ahead** (experimental) | |
| Encoders | NVENC, QSV, AMF/VCE, software fallback | NVENC, QSV, VA-API (AMD/Intel), x264/x265 | Parity | AMD zero-copy unverified |
| Zero-copy capture→encode | Claimed (OVR) | NVENC + VA-API (DMA-BUF) | Parity (partial coverage) | QSV and HEVC zero-copy unmeasured |
| Zero-copy decode→present | Windows D3D11 experimental | CUDA→GL, VA-API→EGL; Mac pending | Parity / ahead on Linux | |
| FPS | 60 default, 240 possible but "not recommended" | 120 max, 180/240 WIP | WIP | |
| Resolution matching | Changes host mode or VDD | Scales stream; matching WIP | WIP | Teleport's scaling is a bandwidth advantage |
| VSync toggle | Yes (default on) | No setting | Missing (VRR WIP) | |
| VRR | None (advises disabling G-Sync) | WIP | WIP | Chance to lead |
| Quality preset (QP / multi-pass) | Yes | Bitrate only | Missing | S |
| Constant FPS / static refinement | Yes | Keepalive frame only | Missing | |
| Congestion control | BUD, 2 modes | QUIC RTT/loss controller | Partial | Capped-link utilisation and resolution/FPS step-down still open (H12) |
| FEC | Unknown (BUD internals) | None (QUIC retransmit) | Unknown | |
| P2P NAT traversal (STUN, hole punch, UPnP) | Yes | STUN + hole punching via relay signalling, NAT-PMP opt-in (2026-10-16) | Mostly closed | No UPnP-IGD/PCP; symmetric NATs use the relay (G1) |
| Relay | Enterprise only, customer-hosted, not E2E (DTLS via relay unclear) | Any user, self-hosted, E2E encrypted, auto fallback | **Teleport ahead** | |
| CGNAT / double NAT | Unsupported | Works via relay | **Teleport ahead** | |
| Cloud account required | Yes | No | **Teleport ahead** | |
| Encryption | DTLS 1.2, backend-validated certs | QUIC TLS 1.3, pinned host key, E2E AEAD over relay | **Teleport ahead** | |
| Keyboard capture | Immersive keyboard | Ctrl+Alt+K | Parity | |
| Pointer confinement / relative | Immersive mouse, "Always Relative" | Ctrl+Alt+M relative lock | Parity | Auto-lock on hidden cursor missing |
| Local cursor | Yes (inferred) | X11 only | Partial | Wayland hosts are the main target |
| Extra mouse buttons | Yes (unverified) | Back/forward (2026-10-16) | Parity | G14 |
| Configurable hotkeys / ignore hotkey | Yes | Fixed chords | Missing | S |
| Cmd/Ctrl swap | Yes | Fixed Cmd→Super | Missing | S |
| Gamepad (Xbox 360) | Yes (Windows host) | Yes (Linux host, 4 pads) | Parity | |
| Rumble | Yes | Yes (2026-10-16) | Parity | G12 |
| DS4 / DualSense emulation, touchpad, motion, haptics | Yes (Windows host) | No | Missing | |
| Pad remapping UI | Yes | SDL database only | Missing | Low |
| Pen / tablet | Warp | No | Missing | Low for gamers |
| Mic passthrough | Windows host | No | Missing | Voice chat in games |
| Multi-channel audio | 5.1/7.1 | Stereo | Missing | |
| Audio bitrate / raw PCM | 128–512, raw option | Fixed 128k Opus | Partial | |
| Audio FEC / buffer tuning | 50–100 ms buffer | ≤100 ms leaky; tuning WIP | WIP | |
| Multi-monitor | 3→5 screens (paid) | Up to 5 windows (1+4), free | **Parity / ahead (free)** | Per-screen settings partly |
| Virtual displays / headless | VDD (paid), fallback (free) | Portal virtual outputs (KDE): headless fallback, `always`, extra monitor sized to the client (2026-10-17) | **Parity (free)** | G17; GNOME and a connector-less machine untested |
| Privacy mode | Paid | Physical outputs off + virtual output, opt-in, guarded restore (KDE, 2026-10-17) | Parity | G17 |
| Login screen / after reboot | Windows, macOS | No (running session only) | Missing | |
| Wake-on-LAN | Not documented | No | Parity | An opening |
| Clipboard | Automatic text sync | Explicit text, 64 KiB | Partial (by design) | |
| File transfer | No | No | Parity | |
| Multiple guests / co-op | Up to 20, per-guest permissions | One client | **Missing** | Parsec's signature feature |
| Host approval prompt / share link | Yes | Pairing codes and links; no live approve | Partial | |
| Guest permission levels | Pad, keyboard, mouse per guest | All or nothing | Missing | |
| Chat | Yes | No | Do not copy | |
| Stats overlay | Rich, plus graph and warnings | Rich F8, no warnings or graph | Partial | |
| Published latency numbers | "7 ms LAN", "4–8 ms at 240 FPS" | Component timings only | Missing | |
| Teams / SSO / audit / API | Yes | No | Not a gamer gap | |
| Installers | Signed Windows/macOS/deb/APK | Nix only | **Missing** | |

---

## 4. Where Teleport already beats Parsec

1. **Linux hosting.** Parsec cannot host on Linux at all (HW, LIN). Teleport
   hosts on KDE/GNOME Wayland and X11, with DMA-BUF zero-copy into NVENC and
   VA-API. That makes it the natural choice for Linux gaming PCs, Bazzite or
   SteamOS-style boxes (unverified) and NixOS setups.
2. **NAT traversal that works for everyone.**
   - Parsec requires no CGNAT or double NAT (NET). Its only relay is a
     customer-hosted Enterprise product (HPR).
   - Teleport's relay is available to any user, self-hostable on a small VPS,
     falls back automatically (`--route auto`) and shows signed presence.
3. **End-to-end security without a vendor in the loop.**
   - Parsec trusts its backend to validate peer certificates and connection
     tokens (SEC), and needs an account.
   - Teleport pins the host key, uses PAKE pairing and per-device keys,
     encrypts end to end over relays, and needs no account or cloud.
4. **Open source and self-hostable**, Nix-reproducible. Parsec is
   proprietary (OVR).
5. **HDR (experimental).** Parsec explicitly does not support HDR (E15106).
   Teleport has a working, if experimental, HDR10 path to Mac EDR.
6. **Free multi-monitor.** Parsec needs Warp for more than one screen.
   Teleport maps up to 1+4 remote monitors to local displays at no cost.
7. **Stream scaling.** Teleport can send a 1080p stream from a 4K desktop
   without touching the host's display mode. Parsec can only change the host
   resolution (RES), which games and multi-monitor desktops do not always
   tolerate.
8. **Linux client quality.** Parsec's Linux client is Ubuntu 22.04 and X11
   only. Teleport's runs on any Nix-capable distribution, Wayland or X11, with
   zero-copy presentation on NVIDIA and VA-API.
9. **Observability.** F8 already separates queue, decode, download and
   conversion stages and accepted versus current bitrate, finer than
   Parsec's overlay header. There is also a `tc netem` network test harness
   with published results (docs/review-2026-10.md).
10. **Fail-closed input.** Stuck-key release on focus loss and heartbeat
    expiry, and keys never dropped under pressure (H3, H6).

---

## 5. Prioritised gaps

Ranking is by impact on gamers (latency and quality first, then input, then
convenience and reach), with effort S (days), M (1–3 weeks), L (more than a
month) and XL (a quarter or more). Items marked WIP in section 2 are excluded.
File references are the likely touch points.

### Tier 1: latency and image quality

**G1. Direct P2P across NATs: STUN reflexive addresses, QUIC hole punching,
UPnP/NAT-PMP/PCP. Effort M–L.**

- **Status (2026-10-16): implemented** (docs/relay.md "Peer-to-peer through
  NATs", review tracker R7). `--route auto` races the direct address against
  hole punching signalled through the relay (end-to-end sealed,
  domain-separated handshake); STUN shares the QUIC socket; `teleport relay`
  answers STUN on UDP 3478; `--upnp` adds a NAT-PMP mapping (not UPnP-IGD or
  PCP); Stats shows `p2p` with the candidate type. Namespace test: two
  MASQUERADE NATs connect directly (input p50 24 ms vs 86 ms via the relay);
  symmetric NAT and blocked UDP fall back to the relay. Still open:
  symmetric NATs (no port prediction; the relay is the only TURN-like path),
  moving a running relayed session to a direct path, and a STUN source when
  a provider relay is used.

- **Why:** today a WAN session either needs a port forward or goes through the
  relay, which adds +5–30 ms (docs/review-2026-10.md R6). Parsec gets a direct
  path for most home users this way (SEQ: "97% NAT traversal" claimed in OVR).
- **Signalling:** use the existing E2E relay channel; the presence and
  enrollment tracks already run there (`src/relay.rs`, `src/presence.rs`,
  `src/bridge.rs`).
- **Steps:**
  1. Each side learns its server-reflexive address. Add a minimal STUN
     binding responder to `teleport relay`, or use a public STUN server.
  2. Exchange candidates (host, srflx, and the UPnP-mapped address) inside
     the E2E session.
  3. Both sides send QUIC Initials / UDP punch packets from the *same* Quinn
     endpoint socket. The host keeps its server endpoint; the client
     connects to each candidate in parallel, and the host sends a few
     throwaway UDP datagrams to the client's srflx address to open its NAT.
  4. The first handshake to pin-verify wins. Keep the relay as the fallback;
     `--route auto` gains a `p2p` step.
- **Port mapping:** UPnP-IGD (e.g. the `igd-next` crate) or NAT-PMP/PCP on the
  host maps UDP 4443 for the session lifetime and removes the mapping on exit.
  Make it opt-in, since some users object to UPnP.
- **Mid-session migration:** QUIC path migration can move a relay-started
  session to direct, but MoQ through a relay is a different transport. Simplest
  is to reconnect the session on the direct path when it comes up; reconnect
  already exists (H1).
- **Security:** the pinned host certificate still authenticates the direct
  path, so no new trust is needed.
- **Tests:** extend `tests/network.rs` with two netns NATs (iptables
  MASQUERADE, endpoint-independent versus symmetric).

**G2. macOS client presentation on real hardware: H10 and M3. Effort S–M,
blocked on Mac hardware.**

- **Why:** the Mac is the primary documented client. The deferred Metal
  `displaySyncEnabled`/`maximumDrawableCount` change (H10) and NV12/
  CVPixelBuffer→Metal (M3) are the largest remaining client-side latency items
  (6–9 ms per 1080p frame of NV12→RGB24 conversion, per the tracker).
- **Steps:**
  1. Verify `TELEPORT_MAC_GPU_VIDEO=1` and make it the default.
  2. Expose a VSync on/off choice (G4).
  3. Measure with the existing presentation summary
     (`windowed_presentation_latency`).
- **Files:** `src/hdr_present.rs`, the Mac path in `src/client.rs`,
  `src/decoder.rs`.

**G3. A published glass-to-glass latency number and harness. Effort S–M.**

- **Why:** Parsec markets "adds only 7 ms on LAN" (OVR) and "4–8 ms at 240
  FPS" (B240). Teleport has component timings but no headline number, and
  "beats Parsec" needs a like-for-like measurement.
- **Steps:**
  1. Software first: the host stamps the capture time (already available as
     `mean_capture_us`, M6) into frame metadata. The client estimates the host
     clock offset from the control RTT (NTP-style, min-RTT filtered) and
     reports capture→present p50/p95 in F8.
  2. Then hardware: a photodiode or high-speed camera rig with a flashing test
     pattern. `--source test` already exists; add a pattern that toggles on
     an input event to get click-to-photon.
  3. Publish numbers against Parsec on the same machines (Windows client
     against a Windows Parsec host is not possible for a Linux host, so
     compare Mac/Linux client → Linux Teleport versus Mac client → Windows
     Parsec on the same hardware).
- **Files:** `src/stats.rs`, `src/protocol.rs` (frame metadata), `src/media.rs`.

**G4. Presentation controls: VSync toggle and frame-pacing mode. Effort S.
VRR itself is WIP.**

- **Why:** Parsec ships VSync on by default with an off switch (VSYNC).
  Moonlight has pacing modes (a fork exposes a VRR pacing buffer of 2.5, 4.5 or
  6 ms; https://newreleases.io/project/github/Nonary/moonlight-qt/release/v6.1.0-vrr18,
  third-party fork). Teleport presents without vsync on SDL (no
  `present_vsync`), which tears on fixed-refresh displays.
- **Steps:** add `--present immediate|vsync|vrr` (VRR from the WIP work), a
  launcher toggle, and per-mode stats (late or dropped presents).
- **Files:** `src/client.rs:765`, `src/multi_display.rs:156`, `src/gl_nv12.c`,
  `src/hdr_present.rs`.

**G5. Automatic codec negotiation, HEVC by default. Effort S.**

- **Status (2026-10-16): implemented** (review tracker R9). `--codec auto`
  is the client and launcher default: H.265 when the host's encoder probe
  (`Desktop.hardware_codecs`) and the client's decoder probe both find
  hardware, otherwise H.264. The client sends `Event::SelectCodec` first to
  hosts advertising `codec_selection`, which wait for it before starting
  video, so there is no restart. Older hosts and clients keep H.264 unless
  asked; HDR10 stays H.265; an explicit `--codec` wins. F8 and both logs
  show the codec and why. Not done: a fallback when a probed hardware path
  later fails mid-session (the existing decoder recovery applies), and AV1.

- **Why:** Parsec tries H.265 by default and silently falls back (ADV, HW).
  Teleport defaults to H.264 and makes the user choose. HEVC is roughly half
  the bitrate at equal quality per Parsec's own claim (HW), which matters most
  on WAN.
- **Steps:**
  1. The host advertises encodable codecs in `Desktop` metadata, from its
     probe cache (H7).
  2. The client intersects that with its decode probe and picks the best,
     with `--codec auto` as the new default.
  3. Leave space for AV1 (G8).
- **Files:** `src/protocol.rs` (capability gating as for `keyframe_requests`),
  `src/host.rs`, `src/client.rs`, `src/launcher.rs`.

**G6. Congestion behaviour on capped links, and resolution/FPS step-down.
Effort M.**

- **Status (2026-10-17): implemented** (review tracker H12 follow-up,
  "Capped links and loss recovery"). Clients send `NetworkFeedback` (one-way
  queueing delay of each frame's first byte, received rate, every 100 ms)
  to hosts advertising `network_feedback`; the host holds the encoder at 85%
  of what a busy link delivered and probes fast far from, and slowly near,
  the last bottleneck. Below 0.05 bit per pixel it steps width to 75%, 50%,
  then halves the FPS, at most every 8 s, back up with hysteresis; never
  above the negotiated stream or the user's bitrate. F8 shows the step.
  Utilisation of 10 and 5 Mbit/s bottlenecks 50–57% and 36–39% → 75–81%
  and 57–79%; a 3 Mbit/s link 23% → 70–74%. Not done: a quality metric
  (PSNR/VMAF) in the harness to tune the step floors; the first seconds of
  a session still start at the full bitrate.

- **Why:** the tracker's own follow-up to H12. A 3 Mbps capped link carries
  only ~1.8 Mbps. There is no resolution or FPS step-down, so heavy
  congestion costs frame rate instead of sharpness. Parsec relies on manual
  steps (low-bandwidth article), so automatic step-down would be ahead.
- **Steps:**
  1. Treat rising RTT plus loss as congestion.
  2. Add a ladder: bitrate first, then scale width (e.g. 100% → 75% → 50%)
     before dropping FPS. The scaler already exists (`--width`).
  3. Make step-ups hysteretic.
  4. Score with a quality metric in the harness (PSNR/VMAF on `snow`), as the
     tracker suggests.
- **Files:** `src/media.rs` (`NetworkRateController`), `src/host.rs`,
  `tests/network.rs`.

**G7. Loss recovery without IDR bursts: per-frame groups (R1) and intra
refresh. Effort M.**

- **Status (2026-10-17): intra refresh done, R1 rejected.** libx264 and
  h264_nvenc refresh intra blocks over one second instead of sending
  periodic IDRs (keyframes per 20 s 6 → 1, largest keyframe 10–30% smaller
  against the mean frame). H.265 and VA-API/QSV keep IDRs: FFmpeg's H.265
  decoder cannot decode past a missing reference, and FFmpeg has no VA-API
  intra refresh. One group per frame (R1) cut tail latency on capped links
  but added 1–11 s freezes (newest-first group delivery in moq-lite, IDR
  retries on slow links, FFmpeg's H.264 decoder showing nothing after
  longer gaps until the refresh completes), so loss still recovers with a
  requested IDR. Details in the tracker.

- **Why:** keyframe-on-request (C3) still sends a full IDR after loss, which is
  a burst on exactly the link that just lost packets.
- **Steps:**
  1. NVENC `intra-refresh=1` (with `forced-idr` only for new subscribers) and
     VA-API/QSV intra refresh where drivers support it. Recovery then spreads
     over N frames.
  2. Combine with R1 (one MoQ group per frame), so a stalled frame is
     superseded rather than blocking.
- **Files:** `src/ffmpeg_codec.c`, `src/ffmpeg_media.rs`; the protocol change
  for R1.

**G8. AV1 (and 4:4:4 / 10-bit SDR as quality options). Effort M each.**

- **AV1:** Parsec has none. Sunshine/Moonlight do
  (https://docs.lizardbyte.dev/projects/sunshine). Use FFmpeg `av1_nvenc`
  (Ada+), `av1_vaapi` (Intel Arc / Meteor Lake+, AMD RDNA3+) and `av1_qsv`.
  Decode with NVDEC/VA-API, and on Mac with VideoToolbox AV1 on M3 or later
  **(unverified)**. Better quality per bit for WAN. It reuses the
  direct-FFmpeg plumbing and the codec negotiation from G5.
- **4:4:4:** `hevc_nvenc` with yuv444p and Intel VA-API HEVC 4:4:4 (Ice Lake+).
  Decode needs NVDEC Turing+ or Intel 11th gen+, otherwise software. Matters
  for desktop text more than games; Parsec puts it behind Warp.
  - DMA-BUF import currently converts to NV12 (`src/dmabuf_encode.c`), so the
    zero-copy path needs a 4:4:4 target format.
- **10-bit SDR:** Main10 is already wired for HDR, so this is mostly a
  negotiation flag. It reduces banding in dark game scenes.

**G9. Static-scene refinement ("Constant FPS" equivalent). Effort S.**

- **Status (2026-10-16): implemented** (docs/ffmpeg.md "Static-scene
  refinement"), as a burst of 8 repeated frames 200 ms after the screen
  stops rather than a low-QP frame: per-frame QP or bitrate boosts gained
  little (VA-API, x264) or forced an IDR (NVENC). At 1440p60 5 Mbps on
  NVENC the still frame gains 1.6 dB (H.264) / 3.9 dB (H.265) within
  ~130 ms for 95 kB, what the keepalive alone takes two seconds to reach.
  Only below 0.05 bits per pixel; not on Intel VA-API/Quick Sync, where
  repeats did not refine. Not verified on a real still KDE desktop.

- **Why:** Parsec offers Constant FPS because damage-driven capture leaves the
  last, motion-quality frame on screen (ADV, QUAL). Teleport's capture is also
  damage-driven, with only a 250 ms keepalive.
- **Steps:** after N ms without damage, encode one or two refinement frames at
  low QP within the bitrate budget (e.g. a forced high-quality P-frame or
  CQP), so text sharpens when the desktop settles.
- **Files:** `src/capture.rs`, `src/ffmpeg_media.rs`.

**G10. Encoder quality preset. Effort S.**

- **Status (2026-10-16): implemented** (docs/ffmpeg.md "Encoder presets").
  `--encoder-preset latency|balanced|quality` on the host (default for its
  clients) and the client/launcher (per session, in `SelectCodec`, gated on
  `Desktop.encoder_presets`); F8 shows it. Default stays `latency`. At 1440p60
  20 Mbps on desktop text, NVENC H.264 `quality` (p6, hq, two-pass) gained
  3.5 dB PSNR for +5.5 ms per frame; H.265 `quality` (p6, hq) raised SSIM
  0.987 → 0.992 at half the bits for +10 ms. VA-API levels changed encode
  time, not quality, on the UHD 630.

- **Why:** Parsec exposes Lowest Latency / Balanced / Highest Quality (QP).
  Teleport is fixed at NVENC `p1`/`ull` (`src/ffmpeg_codec.c:147-152`).
- **Steps:** add `--quality latency|balanced|quality` mapping to NVENC
  p1/p4/p6 plus multipass on the strongest setting, and the VA-API/QSV
  equivalents. Measure encode time in F8 before recommending a level.

### Tier 2: input

**G11. Wayland local cursor and automatic pointer lock. Effort M.**

- **Status (2026-10-16): step 1–2 implemented** (docs/cursor.md, tracker
  R5). Not with a separate PipeWire consumer: `pipewiresrc` already
  negotiates `SPA_META_Cursor`, and a pad probe (`src/pw_cursor.rs`) reads it
  from the PipeWire buffer behind each GStreamer buffer, so it rides on the
  zero-copy DMA-BUFs. The portal session is opened in metadata mode and
  switched back to an embedded pointer for clients that do not opt in.
  Verified on KDE Plasma 6.7.5. Gap: with DMA-BUF capture KWin's first
  bitmap is lost, so the shape arrives with its first change.
- **Status (2026-10-16): step 3 implemented** (G11b, tracker R3):
  `--relative-mouse auto|always|never` (launcher button, F8 line). Auto locks
  while the host hides its pointer and a session window has focus with the
  pointer over video (keyboard capture is not required); Ctrl+Alt+M or the
  pointer reappearing releases it. Verified in an Xvfb UI test, not with a
  real game. Step 4 (scaling the cursor) is open.

- **Why:** local cursor only works on X11 hosts (docs/cursor.md), and KDE
  Wayland is the primary host. On Wayland the pointer has a full
  encode→network→decode round trip, which is the most visible latency in
  desktop use. Auto pointer lock when the host hides its cursor (R3/R5) is
  what makes FPS games "just work". Parsec added "Always Relative" in
  150-105a (CL).
- **Steps:**
  1. Replace `pipewiresrc` cursor handling with a PipeWire consumer that reads
     `SPA_META_Cursor` (request `CursorMode::Metadata` from the portal). That
     can be a small `pipewire-rs` stream alongside, or an appsrc feeding the
     existing pipeline.
  2. Send `Update::Cursor` exactly as the X11 watcher does (`src/x11_cursor.rs`
     → a new `src/pw_cursor.rs`).
  3. Auto-lock: when the host reports a hidden cursor and the window has focus
     with keyboard capture on, switch to relative mode. Keep the never-trap
     rules.
  4. Scale the cursor with the video (docs/cursor.md gap).
- **Files:** `src/capture.rs`, `src/client.rs`, `src/protocol.rs`.

**G12. Rumble / force feedback. Effort S–M.**

- **Status (2026-10-16): implemented** as below (docs/gamepad.md, review
  tracker R4), gated on `ClientFeatures.rumble` instead of a `Desktop`
  field: the client declares it, and only then do new pads offer
  `FF_RUMBLE`. Verified with real uinput pads (`EVIOCSFF`/`EV_FF`, and SDL
  rumbling the virtual pad); not with a physical controller.

- **Why:** Parsec's Xbox 360 emulation includes vibration (GP). Teleport has
  none (docs/gamepad.md). Rumble is the most noticeable missing pad feature.
- **Steps:**
  1. Declare `EV_FF` / `FF_RUMBLE` on the uinput pad.
  2. Service `UI_FF_UPLOAD` / `UI_FF_ERASE` (`UI_BEGIN_FF_UPLOAD` and related
     ioctls) and `EV_FF` play events in the gamepad thread.
  3. Send `Update::Rumble { pad, low, high, duration_ms }` on the updates track,
     capability-gated.
  4. The client calls `SDL_GameControllerRumble`.
- **Files:** `src/gamepad.rs`, `src/controllers.rs`, `src/protocol.rs`.

**G13. DualShock 4 / DualSense emulation, with touchpad, gyro and LEDs.
Effort M–L.**

- **Why:** Parsec emulates DS4/DualSense on Windows hosts (GP). Sunshine
  emulates DualSense on Linux (https://docs.lizardbyte.dev/projects/sunshine).
  It is needed for PlayStation prompts and for gyro aiming (Steam Input).
- **Approach:** create the pad through `/dev/uhid` with the Sony HID report
  descriptor, so the kernel `hid-playstation` driver binds and exposes
  motion, touchpad and LED nodes. Reportedly the approach of Sunshine's
  inputtino **(unverified)**.
- **Client side:** SDL already exposes touchpad and sensor events
  (`SDL_CONTROLLERTOUCHPAD*`, `SDL_CONTROLLERSENSORUPDATE`).
- **Protocol:** add lossy sensor and touchpad events, and per-pad type
  selection.

**G14. Extra mouse buttons and minor input parity. Effort S.**

- **Mouse buttons (done 2026-10-16, review tracker R10):** SDL `X1`/`X2` go
  on the wire as buttons 8/9 to hosts advertising
  `Desktop.extra_mouse_buttons`, which inject evdev `BTN_SIDE`/`BTN_EXTRA`
  (portal `NotifyPointerButton`) or XTest buttons 8/9. Verified end to end on
  an Xvfb X11 host; the portal mapping is unit-tested only.
- **Hotkeys:** configurable hotkeys and an "ignore hotkey" list (Discord
  push-to-talk). Parsec has both (HK). Teleport's chords are hard-coded in
  `src/keyboard_capture.rs` and the client.
- **Swap Cmd/Ctrl:** an optional toggle (SWAP).
- **Mismatch warning:** warn about OS pointer-acceleration or DPI mismatch in
  relative mode (MOUSE). Low priority.

### Tier 3: convenience, social and reach

**G15. Multiple concurrent guests with per-guest permissions (co-op). Effort
L.**

- **Why:** this is Parsec's signature gaming feature. Friends join a game
  with their own pads. The host accepts each one; guests get gamepad-only by
  default; the host grants keyboard and mouse per guest (PERM, COOP).
- **Fit:** MoQ is a good fit. One `desktop` broadcast fans out to many
  subscribers, and through a relay it is encoded once and fanned out by the
  relay.
- **Work needed:**
  1. Host session model from one controller to N (`src/host.rs` session loop;
     `Desktop` advertisement).
  2. Per-guest input permission (pad, keyboard, mouse) enforced on the host
     before injection.
  3. Gamepad slot allocation across guests (global slots 0..N rather than per
     client 0..3).
  4. Per-guest heartbeats and ReleaseAll so one guest disconnecting never
     releases another's keys.
  5. A host-side approval prompt and live guest list with kick.
  6. Bitrate as min(guests) or per-guest simulcast later.
  7. E2E relay keys per guest.
- Pairs with G16.

**G16. Invite links, guest credentials and a host approval prompt. Effort M.**

- **Why:** Parsec's flow is "click Share, send link, accept the request"
  (PERM). Teleport has pairing codes and relay pairing links that issue a
  permanent device credential.
- **Steps:**
  1. Add time-limited, scope-limited guest credentials (gamepad only, view
     only, expiry). The access record already has per-device entries
     (`src/access.rs`).
  2. Show a host-side Accept/Reject notification for unknown or guest devices
     (`src/host_ui.rs`), with reject on timeout.
  3. Keep the strict default: no "connect without approval" for guests.

**G17. Virtual display, headless hosting and privacy mode on Linux. Effort M.**

- **Status (2026-10-17): implemented for KDE Plasma 6**
  (docs/virtual-display.md, review tracker R13). The ScreenCast portal's
  VIRTUAL source makes KWin create an output for the session's lifetime:
  `--virtual-display fallback` (default) streams one when no monitor can be
  shared and lets clients add one as an extra monitor (`--virtual-monitor`);
  `always` streams one sized to the client (exact resolution matching). The
  host resizes it to the client's display with `kscreen-doctor` custom
  modes; input goes through the same RemoteDesktop session. Privacy mode
  (`--privacy-mode request|always`, client `--privacy`, launcher toggle)
  disables the physical outputs and streams a virtual one; DPMS was rejected
  (input wakes it, and its wake hotplug destroys portal sessions). Restore is
  layered: on session end, by a heartbeat guard process when the host dies or
  hangs, by `ExecStopPost`/`teleport privacy-restore`, and by KWin's stored
  layout. Verified in a nested headless KWin with its own portal (resize,
  video, input on both kinds of virtual output, privacy on/off, hung and
  killed host). Open: a real headless machine with no connector, GNOME
  (`RecordVirtual`, same code path, untested), DRM virtual output into
  NVENC/VA-API zero-copy, `loginctl lock-session` on last disconnect. KDE's
  portal gives every virtual output of an app the same name, so a virtual
  monitor is offered only beside physical monitors.
- **Why:** Parsec charges for virtual displays and privacy mode (MM, PRIV) and
  needs a driver on Windows. On Linux the compositor can provide them.
- **Steps:**
  1. Request a **virtual** source: the ScreenCast portal `SourceType` VIRTUAL,
     KWin's `zkde_screencast_unstable_v1.stream_virtual_output`, or Mutter's
     `RecordVirtual`. Make it the client's size and refresh rate, so it doubles
     as resolution matching (coordinate with the WIP resolution work) and
     works on a host with no monitor attached. Which compositors support which
     interface is **unverified**; check the KDE and GNOME versions on target
     distributions.
  2. Privacy mode: disable physical outputs for the session (KScreen/
     `kscreen-doctor`, or wlr-output-management), restore them on exit, and
     `loginctl lock-session` on last disconnect.
- **Files:** `src/capture.rs`, `src/host.rs`; the existing KWin patch
  experience (`docs/kwin-hdr.md`) applies.

**G18. Windows client. Effort L.**

- **Why:** most gamers' laptops and secondary PCs run Windows. Rust, SDL2,
  FFmpeg (D3D11VA/NVDEC) and Quinn are all portable.
- **Work needed:** D3D11 or ANGLE presentation for zero-copy, Windows keyboard
  capture (a low-level keyboard hook for Alt+Tab and Win), an installer, and
  code signing. Without it Teleport cannot be "the one gamers use", whatever
  the latency.

**G19. Windows host. Effort XL; a strategic decision.**

- **Why:** most gaming PCs run Windows, and Parsec's whole user base hosts
  there.
- **Work needed:** DXGI Desktop Duplication or Windows.Graphics.Capture, a
  D3D11 texture into NVENC/AMF/QSV, SendInput, ViGEm or a virtual HID
  driver for pads, a service for UAC and the login screen, and an IDD virtual
  display.
- **Alternative:** position Teleport as *the* Linux/SteamOS host and best
  Mac/Linux client, and leave Windows hosting out of scope. Decide explicitly;
  see open questions.

**G20. Microphone passthrough. Effort M.**

- **Why:** in-game and Discord voice on the host (MIC). Parsec gates it to
  owner connections; do the same, or use per-device consent.
- **Steps:**
  1. The client captures with SDL audio capture or GStreamer
     `osxaudiosrc`/`pulsesrc` and encodes Opus 10 ms.
  2. Send it on a new client-published `mic` track (priority between input and
     video).
  3. The host plays it into a PipeWire virtual source. Create a node with
     `media.class=Audio/Source/Virtual` through `pipewiresink` with stream
     properties, or `pw-loopback`, so apps see a "Teleport microphone".
  4. Add an explicit per-session opt-in on both ends, matching the current
     consent posture (`src/audio.rs` already refuses mics on the host side).

**G21. Audio quality options: in-band FEC (M8), surround, bitrate. Effort S
each.**

- **FEC:** `inband-fec=true` plus a `packet-loss-percentage` hint from QUIC
  stats.
- **Surround:** 5.1/7.1 via Opus multistream (channel-mapping family 1), when
  the host sink and client output match, as Parsec does (MCA).
- **Bitrate:** 128–256 kbps option (ABR). Buffer tuning is WIP.

**G22. Overlay warnings and a metrics graph. Effort S.**

- **Why:** Parsec's actionable warnings ("host is using software encoding",
  "decode too slow for this resolution/FPS", "network loss/congestion") cut
  support load (STATS, LAG).
- **Steps:**
  1. Thresholds on existing F8 data: decode time > frame interval, software
     encoder in use, skipped groups or loss rising.
  2. A small sparkline of RTT, bitrate and decode time.
  3. Add host capture time to F8 (M6).
- **Files:** `src/stats.rs`.

**G23. Wake-on-LAN and "my PC is asleep". Effort S–M.**

- **Why:** neither Parsec nor Teleport documents WoL. Moonlight has it
  (unverified, from memory). Gamers leave the PC asleep.
- **Steps:**
  1. The host records its MAC and broadcast address in the profile at pairing
     time.
  2. The launcher sends a magic packet on the LAN.
  3. For WAN, any always-on LAN device running `teleport relay` or a tiny
     "waker" agent can do it on a signed request.
  4. Show presence "asleep" (last seen and WoL-capable) instead of "offline".

**G24. Unattended start after reboot / login screen. Effort L.**

- **Why:** Parsec reaches the Windows and macOS login screens (FM, LOGIN).
  Teleport attaches only to a running graphical session (README).
- **Gamer-friendly middle ground:** document autologin plus the lock screen,
  and make the host survive the lock screen (portal restore token behaviour
  under KDE/GNOME lock is **unverified**). A greeter-level capture (as GNOME
  Remote Desktop's system/headless mode does) is the full solution and is L.

**G25. Signed installers: Mac `.app`/notarisation, AppImage/Flatpak or
.deb/.rpm, and Windows later. Effort M.**

- **Why:** Parsec is a one-click install. Teleport requires Nix
  (docs/distribution.md). This is the biggest adoption barrier after
  platform reach, and it is not a feature.

**G26. Android, iOS/tvOS and browser clients. Effort L each.**

- **Why:** Parsec has no iOS client at all (HW), so an iOS/iPadOS/tvOS
  client is a clear differentiator. Moonlight's existence there shows it is
  feasible.
- **Browser:** MoQ is designed for WebTransport and WebCodecs, so a browser
  client could beat Parsec's WebRTC/software-decode web client (WEB). It
  could also be served by the relay. E2E would need WebCrypto AEAD. Note that
  provider relays speak WebTransport already (docs/relay.md).
- **Steam Deck:** test the Linux client on SteamOS game mode (gamescope) as a
  cheap win **(unverified)**.

### Lower priority (parity, not gamer-critical)

Pen/tablet with pressure and tilt (uinput tablet; SDL2 has no pen API, so
SDL3 or native), the controller remapping UI, idle kick timer, host speaker
mute on connect, camera passthrough (v4l2loopback), USB/YubiKey passthrough
(see `docs/peripheral-forwarding.md`; Parsec ships it for generation-5
YubiKeys over libusb, 1.19), and display rotation.

Found in Parsec's client (1.19), not in its manual:
- **Uncompressed PCM audio option** for unconstrained links (S): an
  alternative to Opus in `src/audio.rs`, capability-gated.
- **Per-hotkey exclusions** (S): keep chosen hotkeys (push-to-talk) on the
  client while keyboard capture is on (`docs/keyboard-capture.md`).
- **Ctrl/Command swap** between macOS and other systems (S).

Where the inspection shows Teleport ahead on Linux: Parsec's Linux client is
X11-only (XWayland on Wayland desktops) and decodes only through whatever
system FFmpeg it finds. Teleport's client runs native Wayland through SDL,
ships its own FFmpeg with CUDA and VA-API, and presents zero-copy.

---

## 6. Things Parsec does that Teleport should deliberately not copy

- **Mandatory cloud account and backend-trusted certificates** (SEC, NET).
  Teleport's account-less, pinned-key model is a selling point. Any
  rendezvous for G1 should carry only E2E-encrypted, signed data.
- **"Can connect without your approval"** for friends (PERM, LIN). It is
  convenient and dangerous. If offered at all, scope it to the owner's own
  devices.
- **Approved Apps** (APPS). It is focus-based screen freezing on Windows, and
  Parsec itself notes guests still see everything around a windowed app. It
  is a weak security boundary. Prefer scoped permissions (G16) and window
  capture if needed.
- **Changing the host's real display mode** as the main resolution tool (RES).
  It disturbs multi-monitor layouts and can leave the host in a bad mode
  after a crash, which Parsec itself warns about. Prefer scaling (which
  Teleport has) plus virtual outputs (G17).
- **UPnP on by default with permanent leases** (SEQ, CL 150-99). Make port
  mapping opt-in, session-scoped and visible.
- **Paywalling basics** (multi-monitor, 4:4:4, virtual displays). Teleport
  already gives multi-monitor away. Keep it that way.
- **Default-on automatic clipboard sync.** Teleport's explicit text transfer
  is a deliberate privacy choice. An opt-in "sync" mode is fine; default-on is
  not.
- **In-app chat, friends lists, Arcade.** Arcade was removed (CL). Chat and
  social graphs duplicate Discord and Steam. Invite links (G16) cover the need.
- **Config sprawl** (about 60 `config.txt` keys, several marked "don't change
  this unless..."). Prefer negotiated defaults (G5) and a few presets.
- **"Lowest latency" QP floors and congestion modes as user-facing knobs.**
  Expose outcome-oriented presets (G10), not encoder internals.
- **Advising users to disable VRR on the host** (GSYNC). Teleport is building
  VRR; treat host VRR as something to handle, not avoid.

---

## 7. Open questions / couldn't verify

1. **Parsec's real behaviour at more than 60 FPS today.** Docs say "above 60
   may cause instability" and 240 is "not recommended" (ADV, B240). The 240 FPS
   test hardware (GTX 1070/1050 Ti) suggests a 2017–2018 post. No current
   144/240 Hz figures were found.
2. ~~Whether Parsec renders the cursor client-side.~~ **Answered (1.19):**
   yes. Cached PNG cursors from the host are set as real X11 cursors.
3. **Whether Parsec uses FEC, and how BUD congestion control works.** The
   client (1.19) shows SCTP (usrsctp) transport and a selectable "Congestion
   Algorithm", but no FEC, NACK or parity strings. That suggests no FEC; it
   doesn't prove it.
4. **Whether HPR relays DTLS end to end.** DTLS between peers implies yes, but
   the HPR docs do not say. It does see BUD packets on UDP 5000.
5. **Parsec's macOS host encoder (VideoToolbox?) and its latency.** Not
   documented.
6. **Parsec AV1.** Absent from the support corpus and changelog, but the
   Windows engine (1.20) contains AV1 encoder parameter structures, so AV1
   code paths exist there. Whether they are enabled, and on which GPUs, is
   unknown.
7. **Parsec extra mouse buttons (X1/X2).** Not documented; assumed supported.
8. **Parsec "Always Relative" mouse mode (150-105a)**: only the changelog line
   exists; whether it auto-engages when a game hides the cursor is unknown.
9. ~~Linux virtual-output support.~~ **Answered for KDE (G17):**
   xdg-desktop-portal-kde 6.7.5 advertises VIRTUAL (`AvailableSourceTypes`
   7), creates a 1920x1080 KWin output for a RemoteDesktop session with
   source type VIRTUAL, and input reaches it. GNOME (`RecordVirtual`) is
   still unverified.
10. **Whether Teleport's QUIC endpoints listen on IPv6.** Not checked in code.
    Parsec supports IPv6 (STUN6, error codes 800097/6112).
11. **Inputtino's use of uhid for DualSense (G13), Moonlight WoL, and
    VideoToolbox AV1 on M3+.** Stated from memory or third-party sources.
12. **Teleport on SteamOS game mode / Steam Deck as a client.** Untested.
13. **Strategy: is a Windows host (G19) in scope?** This decides whether
    Teleport competes with Parsec head-on or wins the Linux-host niche plus
    the best Mac/Linux client.
14. **Moonlight/Sunshine and Steam Link** were not catalogued in depth. A
    follow-up comparison would sharpen G8, G13 and G23, since those projects
    lead Parsec there.
