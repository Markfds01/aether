//! OSC 10/11 terminal-palette probe and colour blending (SPEC §5.2). Owner: W1.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use super::colors::ColorDepth;
use super::tokens::{Background, Rgb, TerminalPalette};

/// The probe's deadline: long enough for a local terminal, short enough to hide.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(100);

/// The deadline under tmux or SSH, where the reply crosses another hop. A
/// terminal that answers DA1 ends the wait at once (tmux 3.2a does), so this
/// is only paid by one that stays silent; it stays well under a second.
pub const REMOTE_PROBE_DEADLINE: Duration = Duration::from_millis(400);

/// Overrides for the probed colours: `#rrggbb`, `rrggbb` or `rgb:RR/GG/BB`.
pub const BG_ENV: &str = "AETHER_TERMINAL_BG";
pub const FG_ENV: &str = "AETHER_TERMINAL_FG";

/// Default fg, default bg, then DA1. Every terminal answers DA1 and replies
/// arrive in order, so its answer means nothing else is coming.
const QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c";
/// Bounds what a probe can swallow if the user types or pastes meanwhile.
const MAX_REPLY_BYTES: usize = 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Colours and positively detected truecolor support from the terminal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TerminalProbe {
    pub palette: Option<TerminalPalette>,
    pub truecolor: bool,
}

const TRUECOLOR_QUERY: &[u8] =
    b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1bP+q524742\x1b\\\x1b[c";

pub fn probe_terminal(deadline: Duration, depth: ColorDepth) -> TerminalProbe {
    match query_tty(deadline, query_for(depth)) {
        Ok(reply) => {
            let probe = parse_probe_reply(&reply);
            if probe.palette.is_none() {
                log_missing_palette(&reply, deadline);
            }
            probe
        }
        Err(error) => {
            tracing::warn!(%error, "terminal probe failed; using fallback colours");
            TerminalProbe::default()
        }
    }
}

/// Said once, at startup: why the user-message band is the fallback grey.
fn log_missing_palette(reply: &[u8], deadline: Duration) {
    let why = if reply.is_empty() {
        format!("no reply within {} ms", deadline.as_millis())
    } else if has_device_attributes(reply) {
        "the terminal answered device attributes only (tmux before 3.4 does not forward colour queries)"
            .to_owned()
    } else {
        "the reply was incomplete".to_owned()
    };
    tracing::info!(
        "terminal palette unknown ({why}); using fallback colours. Set {BG_ENV}=#rrggbb \
         (and {FG_ENV}) to give the exact ones"
    );
}

/// The deadline for this environment: longer under tmux or SSH.
fn deadline_for(var: impl Fn(&str) -> Option<String>) -> Duration {
    let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    if set("TMUX") || set("SSH_CONNECTION") || set("SSH_TTY") {
        REMOTE_PROBE_DEADLINE
    } else {
        PROBE_DEADLINE
    }
}

/// A colour from an override variable; `None` when unset or unparseable.
fn parse_override(value: &str) -> Option<Rgb> {
    let value = value.trim();
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
        return Some(Rgb(channel(0)?, channel(2)?, channel(4)?));
    }
    parse_osc_color(value)
}

/// The palette an override pair gives: the background decides, and a missing
/// foreground is the probed one, else black or white by contrast.
fn override_palette(
    bg: Option<Rgb>,
    fg: Option<Rgb>,
    probed: Option<TerminalPalette>,
) -> Option<TerminalPalette> {
    let bg = bg?;
    let fg = fg.or(probed.map(|p| p.fg)).unwrap_or_else(|| {
        if contrast_ratio(bg, Rgb(0, 0, 0)) > contrast_ratio(bg, Rgb(255, 255, 255)) {
            Rgb(0, 0, 0)
        } else {
            Rgb(255, 255, 255)
        }
    });
    Some(TerminalPalette { fg, bg })
}

/// The startup probe: the environment's deadline, then `AETHER_TERMINAL_BG` /
/// `AETHER_TERMINAL_FG` over whatever the terminal said. With a background
/// given and no truecolor question left to ask, the terminal is not queried.
pub fn startup_probe(depth: ColorDepth) -> TerminalProbe {
    let var = |name: &str| std::env::var(name).ok();
    let colour = |name: &str| {
        let value = var(name)?;
        let parsed = parse_override(&value);
        if parsed.is_none() {
            tracing::warn!("{name}={value:?} is not #rrggbb; ignoring it");
        }
        parsed
    };
    let (bg, fg) = (colour(BG_ENV), colour(FG_ENV));
    if bg.is_some() && depth != ColorDepth::Indexed256 {
        return TerminalProbe {
            palette: override_palette(bg, fg, None),
            truecolor: false,
        };
    }
    let mut probe = probe_terminal(deadline_for(var), depth);
    if bg.is_some() {
        probe.palette = override_palette(bg, fg, probe.palette);
    }
    probe
}

/// Only a terminal claiming 256 colours is asked whether it does truecolor.
fn query_for(depth: ColorDepth) -> &'static [u8] {
    if depth == ColorDepth::Indexed256 {
        TRUECOLOR_QUERY
    } else {
        QUERY
    }
}

fn parse_probe_reply(reply: &[u8]) -> TerminalProbe {
    let truecolor = reply.windows(2).enumerate().any(|(at, prefix)| {
        if prefix != b"\x1bP" {
            return false;
        }
        let rest = &reply[at + 2..];
        let Some(end) = rest.windows(2).position(|w| w == b"\x1b\\") else {
            return false;
        };
        let payload = &rest[..end];
        if payload == b"1+r524742" || payload.starts_with(b"1+r524742=") {
            return true;
        }
        let Some(sgr) = payload
            .strip_prefix(b"1$r")
            .and_then(|s| s.strip_suffix(b"m"))
        else {
            return false;
        };
        let Ok(sgr) = std::str::from_utf8(sgr) else {
            return false;
        };
        let parameters: Vec<&str> = sgr.split(';').collect();
        parameters
            .windows(5)
            .any(|p| p == ["48", "2", "1", "2", "3"])
            || parameters
                .iter()
                .any(|p| matches!(*p, "48:2:1:2:3" | "48:2::1:2:3" | "48:2:0:1:2:3"))
    });
    TerminalProbe {
        palette: parse_palette_reply(reply),
        truecolor,
    }
}

/// Input typed during the bounded probe window is consumed with the replies.
fn query_tty(deadline: Duration, query: &[u8]) -> io::Result<Vec<u8>> {
    debug_assert!(query.starts_with(b"\x1b]10;?"));
    debug_assert!(query.ends_with(b"\x1b[c"));
    let mut writer = OpenOptions::new().write(true).open("/dev/tty")?;
    let mut reader = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open("/dev/tty")?;
    let _raw = RawModeGuard::enable()?;
    writer.write_all(query)?;
    writer.flush()?;
    read_reply(&mut reader, Instant::now() + deadline)
}

fn read_reply(reader: &mut File, until: Instant) -> io::Result<Vec<u8>> {
    let mut reply = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(reply),
            Ok(n) => {
                reply.extend_from_slice(&chunk[..n]);
                if has_device_attributes(&reply) || reply.len() >= MAX_REPLY_BYTES {
                    return Ok(reply);
                }
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        if Instant::now() >= until {
            return Ok(reply);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Raw mode for the probe window when the caller has not enabled it yet:
/// a cooked tty would echo the replies and hold them until Enter.
struct RawModeGuard {
    restore: bool,
}

impl RawModeGuard {
    fn enable() -> io::Result<RawModeGuard> {
        if crossterm::terminal::is_raw_mode_enabled()? {
            return Ok(RawModeGuard { restore: false });
        }
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawModeGuard { restore: true })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        if let Err(error) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(%error, "could not restore the terminal after the palette probe");
        }
    }
}

/// A DA1 answer: `ESC [ ? <digits and ;> c`.
fn has_device_attributes(reply: &[u8]) -> bool {
    reply.windows(3).enumerate().any(|(at, w)| {
        w == b"\x1b[?"
            && reply[at + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                .is_some_and(|b| *b == b'c')
    })
}

/// Both colours from one buffer of replies; either missing fails the pair.
fn parse_palette_reply(reply: &[u8]) -> Option<TerminalPalette> {
    Some(TerminalPalette {
        fg: osc_reply(reply, b"\x1b]10;").and_then(parse_osc_color)?,
        bg: osc_reply(reply, b"\x1b]11;").and_then(parse_osc_color)?,
    })
}

/// The reply starting with `prefix`, through its BEL or ST terminator.
fn osc_reply<'a>(reply: &'a [u8], prefix: &[u8]) -> Option<&'a str> {
    let start = reply.windows(prefix.len()).position(|w| w == prefix)?;
    let rest = &reply[start..];
    let end = rest.iter().enumerate().find_map(|(at, b)| match b {
        0x07 => Some(at + 1),
        0x1b if rest.get(at + 1) == Some(&b'\\') => Some(at + 2),
        _ => None,
    })?;
    std::str::from_utf8(&rest[..end]).ok()
}

/// Parse one `rgb:RRRR/GGGG/BBBB` (or 2/1-digit) OSC colour reply.
/// Accepts the bare payload or the whole `ESC ] 1x ; … BEL|ST` reply.
pub fn parse_osc_color(reply: &str) -> Option<Rgb> {
    let payload = reply
        .strip_suffix('\x07')
        .or_else(|| reply.strip_suffix("\x1b\\"))
        .unwrap_or(reply);
    let payload = match payload.strip_prefix("\x1b]") {
        Some(rest) => rest.split_once(';')?.1,
        None => payload,
    };
    let (scheme, channels) = payload.trim().split_once(':')?;
    let alpha = match scheme.to_ascii_lowercase().as_str() {
        "rgb" => false,
        "rgba" => true,
        _ => return None,
    };
    let mut parts = channels.split('/');
    let r = scale_channel(parts.next()?)?;
    let g = scale_channel(parts.next()?)?;
    let b = scale_channel(parts.next()?)?;
    if alpha {
        scale_channel(parts.next()?)?;
    }
    parts.next().is_none().then_some(Rgb(r, g, b))
}

/// One 1–4 hex-digit channel, scaled from its own range to 0..=255.
fn scale_channel(hex: &str) -> Option<u8> {
    if !(1..=4).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1u32 << (4 * hex.len())) - 1;
    u8::try_from((value * 255 + max / 2) / max).ok()
}

/// `a` over `b` at `alpha` (0.0 = all `b`, 1.0 = all `a`).
pub fn blend(a: Rgb, b: Rgb, alpha: f32) -> Rgb {
    let alpha = if alpha.is_nan() {
        0.0
    } else {
        alpha.clamp(0.0, 1.0)
    };
    let mix = |x: u8, y: u8| (f32::from(x) * alpha + f32::from(y) * (1.0 - alpha)).round() as u8;
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// WCAG relative luminance, 0.0 (black) to 1.0 (white).
pub fn luminance(c: Rgb) -> f64 {
    let linear = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(c.0) + 0.7152 * linear(c.1) + 0.0722 * linear(c.2)
}

/// WCAG contrast ratio, 1.0 to 21.0.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Light or dark from the probed bg; without one, `COLORFGBG`, then dark.
pub fn background_of(palette: Option<TerminalPalette>, colorfgbg: Option<&str>) -> Background {
    if let Some(palette) = palette {
        // Light when black text would read better on it than white.
        let black = contrast_ratio(palette.bg, Rgb(0, 0, 0));
        let white = contrast_ratio(palette.bg, Rgb(255, 255, 255));
        return if black > white {
            Background::Light
        } else {
            Background::Dark
        };
    }
    // `fg;bg` or `fg;default;bg`; ANSI 0–6 and 8 are the dark backgrounds.
    let bg = colorfgbg
        .and_then(|v| v.rsplit(';').next())
        .and_then(|v| v.trim().parse::<u8>().ok());
    match bg {
        Some(7 | 9..=15) => Background::Light,
        _ => Background::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn tmux_and_ssh_get_the_longer_deadline() {
        assert_eq!(deadline_for(env(&[])), PROBE_DEADLINE);
        assert_eq!(deadline_for(env(&[("TMUX", "")])), PROBE_DEADLINE);
        for var in ["TMUX", "SSH_CONNECTION", "SSH_TTY"] {
            assert_eq!(
                deadline_for(env(&[(var, "x")])),
                REMOTE_PROBE_DEADLINE,
                "{var}"
            );
        }
        assert!(REMOTE_PROBE_DEADLINE < Duration::from_millis(1000));
    }

    #[test]
    fn overrides_parse_and_win_over_the_probe() {
        assert_eq!(parse_override("#300a24"), Some(Rgb(0x30, 0x0a, 0x24)));
        assert_eq!(parse_override(" FFFFFF "), Some(Rgb(255, 255, 255)));
        assert_eq!(parse_override("rgb:30/0a/24"), Some(Rgb(0x30, 0x0a, 0x24)));
        for bad in ["", "#fff", "#gggggg", "red", "#1234567"] {
            assert_eq!(parse_override(bad), None, "{bad:?}");
        }
        let probed = TerminalPalette {
            fg: Rgb(1, 2, 3),
            bg: Rgb(4, 5, 6),
        };
        let aubergine = Rgb(0x30, 0x0a, 0x24);
        let bg = Some(aubergine);
        assert_eq!(
            override_palette(None, Some(Rgb(9, 9, 9)), Some(probed)),
            None
        );
        assert_eq!(
            override_palette(bg, None, Some(probed)),
            Some(TerminalPalette {
                fg: Rgb(1, 2, 3),
                bg: aubergine
            })
        );
        assert_eq!(
            override_palette(bg, None, None).map(|p| p.fg),
            Some(Rgb(255, 255, 255))
        );
        assert_eq!(
            override_palette(Some(Rgb(250, 250, 250)), None, None).map(|p| p.fg),
            Some(Rgb(0, 0, 0))
        );
    }

    #[test]
    fn a_late_reply_is_read_and_a_silent_terminal_times_out() {
        let pipe = || {
            let (r, w) = nix::unistd::pipe().expect("a pipe");
            nix::fcntl::fcntl(
                &r,
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            )
            .expect("nonblocking");
            (File::from(r), File::from(w))
        };
        let (mut reader, mut writer) = pipe();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            writer
                .write_all(
                    b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:3030/0a0a/2424\x07\x1b[?1;2c",
                )
                .unwrap();
        });
        let reply = read_reply(&mut reader, Instant::now() + Duration::from_secs(5)).unwrap();
        late.join().unwrap();
        assert_eq!(
            parse_probe_reply(&reply).palette,
            Some(TerminalPalette {
                fg: Rgb(255, 255, 255),
                bg: Rgb(0x30, 0x0a, 0x24)
            })
        );
        // Nothing arrives: the deadline, not a hang, ends the read.
        let (mut reader, _writer) = pipe();
        let started = Instant::now();
        let reply = read_reply(&mut reader, started + Duration::from_millis(30)).unwrap();
        assert!(reply.is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
        // DA1 alone (tmux 3.2a) returns at once with no palette.
        let (mut reader, mut writer) = pipe();
        writer.write_all(b"\x1b[?1;2c").unwrap();
        let started = Instant::now();
        let reply = read_reply(&mut reader, started + Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(parse_probe_reply(&reply).palette, None);
    }

    #[test]
    fn only_a_256_colour_terminal_is_asked_about_truecolor() {
        let asks = |query: &[u8]| {
            let window = |needle: &[u8]| query.windows(needle.len()).any(|w| w == needle);
            (window(b"\x1bP$qm\x1b\\"), window(b"\x1bP+q524742\x1b\\"))
        };
        assert_eq!(asks(query_for(ColorDepth::Indexed256)), (true, true));
        for depth in [ColorDepth::Truecolor, ColorDepth::Basic16] {
            assert_eq!(query_for(depth), QUERY, "{depth:?}");
            assert_eq!(asks(query_for(depth)), (false, false), "{depth:?}");
        }
        for depth in [
            ColorDepth::Indexed256,
            ColorDepth::Truecolor,
            ColorDepth::Basic16,
        ] {
            assert!(query_for(depth).ends_with(b"\x1b[c"), "DA1 stays last");
        }
    }

    #[test]
    fn truecolor_needs_a_positive_complete_capability_reply() {
        for reply in [
            &b"\x1bP1$r0;48;2;1;2;3m\x1b\\"[..],
            &b"\x1bP1$r48:2:1:2:3m\x1b\\"[..],
            &b"\x1bP1$r48:2::1:2:3m\x1b\\"[..],
            &b"\x1bP1$r48:2:0:1:2:3m\x1b\\"[..],
            &b"\x1bP1+r524742\x1b\\"[..],
            &b"\x1bP1+r524742=31\x1b\\"[..],
        ] {
            let probe = parse_probe_reply(reply);
            assert!(probe.truecolor, "{reply:?}");
            assert_eq!(probe.palette, None);
        }
        for reply in [
            &b""[..],
            &b"garbage"[..],
            &b"\x1bP0$r48;2;1;2;3m\x1b\\"[..],
            &b"\x1bP0+r524742\x1b\\"[..],
            &b"\x1bP1$r48;2;9;2;3m\x1b\\"[..],
            &b"\x1bP1$r48:2:1:2:3m"[..],
            &b"\x1bP1+r524742"[..],
        ] {
            assert_eq!(
                parse_probe_reply(reply),
                TerminalProbe::default(),
                "{reply:?}"
            );
        }
        assert!(TRUECOLOR_QUERY.ends_with(b"\x1b[c"));
        assert!(!TRUECOLOR_QUERY.windows(3).any(|w| w == b"[>c"));
    }

    #[test]
    fn parses_four_two_and_one_digit_channels() {
        assert_eq!(
            parse_osc_color("rgb:ffff/8080/0000"),
            Some(Rgb(255, 128, 0))
        );
        assert_eq!(parse_osc_color("rgb:0d/11/17"), Some(Rgb(13, 17, 23)));
        assert_eq!(parse_osc_color("rgb:f/8/0"), Some(Rgb(255, 136, 0)));
        // 3-digit channels scale from 0xfff, mixed widths each from their own.
        assert_eq!(parse_osc_color("rgb:fff/000/80"), Some(Rgb(255, 0, 128)));
        assert_eq!(
            parse_osc_color("RGBA:ffff/ffff/ffff/ffff"),
            Some(Rgb(255, 255, 255))
        );
    }

    #[test]
    fn parses_whole_replies_with_bel_or_st() {
        assert_eq!(
            parse_osc_color("\x1b]11;rgb:0d0d/1111/1717\x07"),
            Some(Rgb(13, 17, 23))
        );
        assert_eq!(
            parse_osc_color("\x1b]10;rgb:e6e6/eded/f3f3\x1b\\"),
            Some(Rgb(230, 237, 243))
        );
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "rgb:",
            "rgb:ff/ff",
            "rgb:ff/ff/ff/ff",
            "rgb:fffff/0/0",
            "rgb:gg/00/00",
            "rgb:+f/0/0",
            "hsl:1/2/3",
            "#ffffff",
            "\x1b]11;?\x07",
            "rgba:ff/ff/ff",
        ] {
            assert_eq!(parse_osc_color(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_reply_buffer_yields_both_colours_amid_noise() {
        let reply =
            b"x\x1b]10;rgb:e6e6/eded/f3f3\x1b\\\x1b]11;rgb:0d0d/1111/1717\x07\x1b[?64;1;22c";
        assert!(has_device_attributes(reply));
        assert_eq!(
            parse_probe_reply(reply).palette,
            Some(TerminalPalette {
                fg: Rgb(230, 237, 243),
                bg: Rgb(13, 17, 23),
            })
        );
        // A terminal that only answers DA1 has no palette.
        assert!(has_device_attributes(b"\x1b[?1;2c"));
        assert_eq!(parse_palette_reply(b"\x1b[?1;2c"), None);
        // One colour alone is not a palette; an unterminated reply is not a colour.
        assert_eq!(parse_palette_reply(b"\x1b]11;rgb:00/00/00\x07"), None);
        assert_eq!(
            parse_palette_reply(b"\x1b]10;rgb:ff/ff/ff\x07\x1b]11;rgb:00/00/00"),
            None
        );
        assert!(!has_device_attributes(b"\x1b[?1;2"));
    }

    #[test]
    fn blend_weights_the_first_colour_by_alpha() {
        let white = Rgb(255, 255, 255);
        let black = Rgb(0, 0, 0);
        assert_eq!(blend(white, black, 0.0), black);
        assert_eq!(blend(white, black, 1.0), white);
        assert_eq!(blend(white, black, 0.5), Rgb(128, 128, 128));
        assert_eq!(blend(white, Rgb(13, 17, 23), 0.07), Rgb(30, 34, 39));
        assert_eq!(blend(black, white, 0.045), Rgb(244, 244, 244));
        // Out-of-range alphas clamp rather than wrap.
        assert_eq!(blend(white, black, 2.0), white);
        assert_eq!(blend(white, black, -1.0), black);
        assert_eq!(blend(white, black, f32::NAN), black);
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        let white = Rgb(255, 255, 255);
        let black = Rgb(0, 0, 0);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 1e-9);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 1e-9);
        // #767676 is the classic just-passes-AA grey on white.
        assert!(contrast_ratio(Rgb(0x76, 0x76, 0x76), white) >= 4.5);
        assert!(contrast_ratio(Rgb(0x77, 0x77, 0x77), white) < 4.5);
    }

    #[test]
    fn background_comes_from_the_probe_then_colorfgbg_then_dark() {
        let palette = |bg| TerminalPalette {
            fg: Rgb(128, 128, 128),
            bg,
        };
        assert_eq!(
            background_of(Some(palette(Rgb(13, 17, 23))), Some("0;15")),
            Background::Dark
        );
        assert_eq!(
            background_of(Some(palette(Rgb(250, 250, 250))), Some("15;0")),
            Background::Light
        );
        assert_eq!(background_of(None, Some("0;15")), Background::Light);
        assert_eq!(background_of(None, Some("0;default;7")), Background::Light);
        assert_eq!(background_of(None, Some("15;0")), Background::Dark);
        assert_eq!(background_of(None, Some("15;8")), Background::Dark);
        assert_eq!(background_of(None, Some("garbage")), Background::Dark);
        assert_eq!(background_of(None, None), Background::Dark);
    }
}
