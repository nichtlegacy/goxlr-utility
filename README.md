[![Support Server](https://img.shields.io/discord/1124010710138106017.svg?label=Discord&logo=Discord&colorB=7289da&style=flat)](https://discord.gg/BRBjkkbvmZ)
[![GitHub tag (latest SemVer pre-release)](https://img.shields.io/github/v/tag/goxlr-on-linux/goxlr-utility?label=Latest)](http://github.com/goxlr-on-linux/goxlr-utility/releases/latest)
![GitHub Workflow Status (with event)](https://img.shields.io/github/actions/workflow/status/goxlr-on-linux/goxlr-utility/build.yml)

## GoXLR Configuration Utility

An unofficial tool to configure and control a TC-Helicon GoXLR or GoXLR Mini on Linux, MacOS and
Windows. [Click Here](https://discord.gg/BRBjkkbvmZ) to join our discord!

## This fork: virtual GoXLR audio on macOS

This fork grew out of unreliable GoXLR audio routing on macOS 26 and 27. The GoXLR Full appears to
macOS as one 23-channel input and one 10-channel output. Many apps need separate, selectable
devices for the microphone and playback channels instead. The original utility can configure the
GoXLR, but its macOS package does not provide those individual virtual audio devices. We needed a
processed microphone for Discord and speech-to-text, plus separate System, Game, Chat, and Music
outputs for app routing.

### From Loopback to a built-in route

Our temporary setup used [Rogue Amoeba Loopback](https://rogueamoeba.com/loopback/) to expose
Microphone, Chat, Music, and System as Mac devices and route them to the GoXLR channels. That worked,
but it required a separate routing app costing roughly US$100. We built the virtual devices into
this fork so Loopback is no longer needed for these routes. An existing Loopback installation can
remain as a fallback.

### What this fork adds

- A 48 kHz CoreAudio HAL plug-in, [GoXLRVirtual](macos/virtual-audio/), backed by libASPL. It
  presents individual Mac input and output devices instead of making apps select channels from one
  multichannel device.
- A separate **GoXLR Audio Bridge** helper inside the Utility app. It reads the physical GoXLR's
  capture channels and sends each enabled input route to its virtual device; in the other direction,
  it combines virtual playback routes into the GoXLR's physical output channels. The helper has its
  own macOS microphone permission. The audio bridge buffers short underruns to avoid broken-up
  microphone audio.
- USB vendor/product ID checks and the USB location ID to match the physical GoXLR to its CoreAudio
  device unambiguously, including when device names are duplicated.
- Switches in **System → Utility Settings** to show or hide optional routes immediately. The
  settings panel is scrollable and has English and German labels. Only enabled routes run bridge
  audio units. The selection is saved and restored when the daemon starts.

The default devices are **GoXLR Microphone** (input) and **GoXLR System**, **Game**, **Chat**, and
**Music** (outputs). The GoXLR Full's remaining capture routes and Sample output can be enabled in
the Utility; Dry Mic is the only mono route. See the [complete channel map](docs/plans/2026-09-23-macos-full-audio-design.md#device-layout)
for the physical channel assignments. This does not add a per-app output selector for apps that lack
one; those apps use the macOS default output unless another routing tool is used.

This implementation was built and tested locally with a GoXLR Full on macOS 27. macOS 26 motivated
the work but has not been validated with this driver. The upstream release badges and downloads
below refer to the original project: its `.pkg` does **not** include this fork's HAL driver and
audio bridge. Build and install this fork locally using the [macOS virtual audio guide](macos/virtual-audio/README.md)
and [`ci/build-macos-local`](ci/build-macos-local). The fork's UI switches also have source changes
in the separate [goxlr-ui fork](https://github.com/nichtlegacy/goxlr-ui/tree/macos-virtual-audio-ui).

## Features

* Full control over the GoXLR and GoXLR Mini (Similar to the official App)
* Compatibility with profiles created by the official application
* An accessible UI designed to work well with Assistive Technologies
* Remote Access. Control your GoXLR from another computer on your network
* A Sample 'Pre-Buffer'. Record audio from before you press the button
* Exit Actions, including saving profiles and loading other profiles / lighting
* Multiple Device Support. Run more than one GoXLR on one PC
* A CLI and API for basic or advanced scripting and automation
* Streamdeck Integration (
  through [The StreamDeck Repository](https://github.com/FrostyCoolSlug/goxlr-utility-streamdeck))

## Downloads

Downloads are available on the [Releases Page](https://github.com/GoXLR-on-Linux/goxlr-utility/releases/latest) under
the
'Assets' header, we currently provide the following files:

* `.exe` files, usable on Windows<sup>1</sup>
* `.pkg` files, usable on MacOS, both Intel and M1 based packages are available<sup>2</sup>
* `.deb` files, usable on Debian based systems (Ubuntu, Mint, Pop!_OS, etc)
* `.rpm` files, usable on Redhat based systems (CentOS, Fedora, etc)

### OS / Distro Specific Notes

* If you are running Ubuntu 24.04 or a derivitive (such as Linux Mint), please review
  [this issue](https://github.com/GoXLR-on-Linux/goxlr-utility/issues/221)
* If you're running the Mix 2 firmware and are seeing UCM errors, please
  review [this issue](https://github.com/GoXLR-on-Linux/goxlr-utility/issues/223)
* Arch users can install the `goxlr-utility` package from [AUR](https://aur.archlinux.org/packages/goxlr-utility)
* Fedora Atomic or Bazzite users please check the instructions
  [here](https://github.com/GoXLR-on-Linux/goxlr-utility/wiki/Fedora-Atomic-&-Bazzite)
* Windows users can also aquire the GoXLR Utility via `winget`

<sup>1</sup> Windows requires the official device drivers provided by TC-Helicon. If you have the official app
installed you don't need to do anything, otherwise download the latest drivers from 
[here](https://utility.frostycoolslug.com/update-site/drivers/TC-Helicon_GoXLR_Driver_5.57.zip).


<sup>2</sup> MacOS support is still somewhat experimental, and the package may conflict with the existing
GoXLR-MacOS project as they attempt to do the same thing in certain situations.

## Integrations

* [twitchat](https://twitchat.fr/) - Activate and change GoXLR settings based on twitch bits / donations (Thanks Durss!)
* [MacroGraph](https://www.macrograph.app/) - A visual programmer for Streamers. (Thanks JDUDE!)
* [OBS Fader Sync](https://github.com/parzival-space/obs-goxlr-fader-sync-plugin) - An OBS plugin to sync pre-mix
  volumes to fader volumes (Thanks parzival!)
* [Home Assistant](https://github.com/timmo001/homeassistant-integration-goxlr-utility) - A plugin that lets you tie the
  GoXLR into your home automation (Thanks timmmo!)

## Getting Started

Once installed, you can launch the Utility using the `GoXLR Utility` item in your Applications Menu, this will launch
the utility and configuration UI. The UI will then be accessible via the system tray icon, or (if you don't have a tray)
by re-running the `GoXLR Utility` menu item.

If you're running on Linux, a first configuration step should be to enable `Autostart on Login` via System -> Settings.
Windows users will get the choice during installation. If you change your mind, you can change the setting.

If you want to import your profiles from the official app, simply click on the folder icon in the top right of the
relevant profiles pane (either Main or Mic) which will open the directory in your file browser. Copy the profile across
from the Official App's directory (normally `Documents/GoXLR`) and they'll appear in the util ready to load, simply
double click them.

If you're setting up from scratch, the best place to start is configuring your microphone. Head over to the `Mic` tab
and hit `Mic Setup` to configure your microphone type and gain. It may be easier to configure if you first set your
Gate Amount to 0, then reconfigure it once your mic is working. Once done, go explore the UI!

## The UI

The Utility's UI is web based and served directly from the utility to your web browser of choice (if configured, it
can also be served to a web browser on another computer). The Utility also provides an 'Application' which wraps the
web UI into a dedicated app. If you're using the Utility on Windows this option is presented to you during install.
The UI design was modelled around the official application in an attempt to provide a familiar interface for those
moving from Windows to other platforms, rather than forcing people to learn a new configuration paradigm.

![image](https://github.com/GoXLR-on-Linux/goxlr-utility/assets/574943/8f14bd2c-e67a-42e5-bd9f-b3cb367e171d)

If you're running on Linux, the 'Application' isn't provided as part of the base utility installation. If you'd
prefer to use it, check out the [GoXLR UI Repository](https://github.com/frostyCoolSlug/goxlr-utility-ui/), which
provides various builds for distributions. Once installed, you should be able to go to System -> Utility Settings
and change the UI Handler there.

## Building

Build instructions and other useful information can be found on the
project's [wiki](https://github.com/GoXLR-on-Linux/goxlr-utility/wiki/Compilation-Guide).
While it's a little sparse at the moment, over time it should grow, and requests / feedback are always welcome!

## Disclaimer

This project is also not supported by, or affiliated in any way with, TC-Helicon. For the official GoXLR software,
please refer to their website.

In addition, this project accepts no responsibility or liability for use of this software, or any problems which may
occur from its use. Please read the [LICENSE](https://github.com/GoXLR-on-Linux/goxlr-utility/blob/main/LICENSE) for
more information.
