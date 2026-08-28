use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose;
use codex_terminal_detection::TerminalInfo;
use codex_terminal_detection::TerminalName;
use codex_terminal_detection::terminal_info;
use image::imageops::FilterType;

use super::sixel;

const ESC: &str = "\x1b";
const ST: &str = "\x1b\\";
const KITTY_CHUNK_SIZE: usize = 4096;
const SIXEL_CACHE_VERSION: &str = "v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Sixel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PetImageSupport {
    Supported(ImageProtocol),
    Unsupported(PetImageUnsupportedReason),
}

impl PetImageSupport {
    pub(crate) fn protocol(self) -> Option<ImageProtocol> {
        match self {
            Self::Supported(protocol) => Some(protocol),
            Self::Unsupported(_) => None,
        }
    }

    pub(crate) fn unsupported_message(self) -> Option<&'static str> {
        match self {
            Self::Supported(_) => None,
            Self::Unsupported(reason) => Some(reason.message()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PetImageUnsupportedReason {
    Terminal,
}

impl PetImageUnsupportedReason {
    fn message(self) -> &'static str {
        match self {
            Self::Terminal => {
                "Pets aren’t available in this terminal. Terminal pets need image support, and this Windows terminal doesn’t expose Kitty graphics or Sixel support."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolSelection {
    Auto,
    Kitty,
    Sixel,
}

impl ProtocolSelection {
    // Test builds replace the ambient protocol detector with a deterministic unsupported value.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn resolve(self) -> PetImageSupport {
        match self {
            Self::Kitty => PetImageSupport::Supported(ImageProtocol::Kitty),
            Self::Sixel => PetImageSupport::Supported(ImageProtocol::Sixel),
            Self::Auto => detect_pet_image_support(),
        }
    }
}

impl FromStr for ProtocolSelection {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "kitty" => Ok(Self::Kitty),
            "sixel" => Ok(Self::Sixel),
            other => bail!("unknown protocol {other}; expected auto, kitty, or sixel"),
        }
    }
}

pub(crate) fn detect_pet_image_support() -> PetImageSupport {
    if env::var_os("WEZTERM_EXECUTABLE").is_some() || env::var_os("WEZTERM_VERSION").is_some() {
        return PetImageSupport::Supported(ImageProtocol::Kitty);
    }

    pet_image_support_for_terminal(&terminal_info())
}

fn pet_image_support_for_terminal(info: &TerminalInfo) -> PetImageSupport {
    if supports_kitty_graphics(info) {
        return PetImageSupport::Supported(ImageProtocol::Kitty);
    }

    if supports_sixel(info) {
        return PetImageSupport::Supported(ImageProtocol::Sixel);
    }

    PetImageSupport::Unsupported(PetImageUnsupportedReason::Terminal)
}

fn supports_kitty_graphics(info: &TerminalInfo) -> bool {
    matches!(info.name, TerminalName::WezTerm)
        || terminal_field_contains(info.term.as_deref(), "kitty")
        || terminal_field_contains(info.term.as_deref(), "wezterm")
        || terminal_field_contains(info.term_program.as_deref(), "kitty")
        || terminal_field_contains(info.term_program.as_deref(), "wezterm")
}

fn supports_sixel(info: &TerminalInfo) -> bool {
    matches!(info.name, TerminalName::WindowsTerminal)
        || terminal_field_contains(info.term.as_deref(), "sixel")
        || terminal_field_contains(info.term.as_deref(), "mlterm")
        || terminal_field_contains(info.term.as_deref(), "foot")
}

fn terminal_field_contains(value: Option<&str>, needle: &str) -> bool {
    value.is_some_and(|value| value.to_ascii_lowercase().contains(needle))
}

pub fn kitty_delete_image(image_id: u32) -> String {
    format!("{ESC}_Ga=d,d=I,i={image_id},q=2;{ST}")
}

pub fn kitty_transmit_png_with_id(
    path: &Path,
    columns: u16,
    rows: u16,
    image_id: Option<u32>,
) -> Result<String> {
    let png = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let payload = general_purpose::STANDARD.encode(png);
    let chunks = payload
        .as_bytes()
        .chunks(KITTY_CHUNK_SIZE)
        .collect::<Vec<_>>();

    let mut command = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let chunk = std::str::from_utf8(chunk).context("base64 payload is not valid UTF-8")?;
        let has_more = index + 1 < chunks.len();
        let more_flag = u8::from(has_more);
        if index == 0 {
            let image_id = kitty_image_id_arg(image_id);
            command.push_str(&format!(
                "{ESC}_Ga=T,t=d,f=100,c={columns},r={rows},q=2{image_id},m={more_flag};{chunk}{ST}",
            ));
        } else {
            command.push_str(&format!("{ESC}_Gm={more_flag};{chunk}{ST}"));
        }
    }

    Ok(command)
}

fn kitty_image_id_arg(image_id: Option<u32>) -> String {
    image_id
        .map(|image_id| format!(",i={image_id}"))
        .unwrap_or_default()
}

pub fn sixel_frame(frame_path: &Path, cache_dir: &Path, height_px: u16) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;

    let stem = frame_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("frame path has no valid file stem")?;
    let path = cache_dir.join(format!("{stem}_h{height_px}_{SIXEL_CACHE_VERSION}.six"));
    if path.exists() {
        return Ok(path);
    }

    let frame =
        image::open(frame_path).with_context(|| format!("read {}", frame_path.display()))?;
    let height = u32::from(height_px).max(1);
    let width = ((u64::from(frame.width()) * u64::from(height)) / u64::from(frame.height()))
        .try_into()
        .unwrap_or(u32::MAX)
        .max(1);
    let rgba = frame.resize(width, height, FilterType::Lanczos3).to_rgba8();
    let (width, height) = rgba.dimensions();
    let sixel = sixel::encode_rgba(&rgba.into_raw(), width, height)?;

    fs::write(&path, sixel).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn new(name: &'static str, value: Option<&str>) -> Self {
            let previous = env::var_os(name);
            match value {
                Some(value) => unsafe { env::set_var(name, value) },
                None => unsafe { env::remove_var(name) },
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { env::set_var(self.name, value) },
                None => unsafe { env::remove_var(self.name) },
            }
        }
    }

    #[test]
    #[serial]
    fn kitty_png_transmission_encodes_inline_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.png");
        fs::write(&path, b"png").unwrap();

        let command = kitty_transmit_png_with_id(
            &path, /*columns*/ 4, /*rows*/ 3, /*image_id*/ None,
        )
        .unwrap();

        assert!(command.starts_with("\x1b_Ga=T,t=d,f=100,c=4,r=3,q=2,m=0;"));
        assert!(command.contains("cG5n"));
        assert!(command.ends_with("\x1b\\"));
    }

    #[test]
    fn parses_protocol_selection() {
        assert_eq!(
            "auto".parse::<ProtocolSelection>().unwrap(),
            ProtocolSelection::Auto
        );
        assert_eq!(
            "kitty".parse::<ProtocolSelection>().unwrap(),
            ProtocolSelection::Kitty
        );
        assert_eq!(
            "sixel".parse::<ProtocolSelection>().unwrap(),
            ProtocolSelection::Sixel
        );
    }

    #[test]
    fn pet_image_support_detects_kitty_graphics_terminals() {
        for info in [
            terminal_info_for_test(TerminalName::WezTerm, Some("WezTerm"), /*term*/ None),
            terminal_info_for_test(
                TerminalName::Unknown,
                /*term_program*/ None,
                Some("xterm-kitty"),
            ),
            terminal_info_for_test(
                TerminalName::Unknown,
                /*term_program*/ None,
                Some("wezterm"),
            ),
            terminal_info_for_test(
                TerminalName::Unknown,
                Some("WezTerm"),
                Some("xterm-256color"),
            ),
        ] {
            assert_eq!(
                pet_image_support_for_terminal(&info),
                PetImageSupport::Supported(ImageProtocol::Kitty)
            );
        }
    }

    #[test]
    fn pet_image_support_detects_sixel_terminals() {
        for info in [
            terminal_info_for_test(
                TerminalName::Unknown,
                /*term_program*/ None,
                Some("xterm-sixel"),
            ),
            terminal_info_for_test(
                TerminalName::WindowsTerminal,
                Some("WindowsTerminal"),
                Some("xterm-256color"),
            ),
        ] {
            assert_eq!(
                pet_image_support_for_terminal(&info),
                PetImageSupport::Supported(ImageProtocol::Sixel)
            );
        }
    }

    #[test]
    #[serial]
    fn wezterm_env_uses_kitty_graphics_for_ambient_pets() {
        let _wezterm = EnvVarGuard::new("WEZTERM_VERSION", Some("20240203"));
        let _wezterm_executable = EnvVarGuard::new("WEZTERM_EXECUTABLE", /*value*/ None);

        assert_eq!(
            detect_pet_image_support(),
            PetImageSupport::Supported(ImageProtocol::Kitty)
        );
    }

    #[test]
    fn pet_image_support_rejects_unknown_terminals() {
        assert_eq!(
            pet_image_support_for_terminal(&terminal_info_for_test(
                TerminalName::Unknown,
                /*term_program*/ None,
                Some("xterm-256color"),
            )),
            PetImageSupport::Unsupported(PetImageUnsupportedReason::Terminal)
        );
    }

    fn terminal_info_for_test(
        name: TerminalName,
        term_program: Option<&str>,
        term: Option<&str>,
    ) -> TerminalInfo {
        TerminalInfo {
            name,
            term_program: term_program.map(str::to_string),
            version: None,
            term: term.map(str::to_string),
        }
    }

    #[test]
    fn sixel_frame_encodes_without_external_crate() {
        let dir = tempfile::tempdir().unwrap();
        let frame_path = dir.path().join("frame.png");
        let rgba = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        rgba.save(&frame_path).unwrap();

        let sixel_path =
            sixel_frame(&frame_path, &dir.path().join("sixel"), /*height_px*/ 1).unwrap();
        let sixel = fs::read_to_string(sixel_path).unwrap();

        assert!(sixel.starts_with("\x1bP9;1;0q\"1;1;1;1"));
        assert!(sixel.contains("#224;2;100;0;0"));
        assert!(sixel.contains("#224@"));
        assert!(sixel.ends_with("\x1b\\"));
    }

    #[test]
    #[serial]
    fn kitty_png_transmission_includes_image_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.png");
        fs::write(&path, b"png").unwrap();

        let command = kitty_transmit_png_with_id(
            &path,
            /*columns*/ 4,
            /*rows*/ 3,
            /*image_id*/ Some(7),
        )
        .unwrap();

        assert_eq!(
            command,
            "\x1b_Ga=T,t=d,f=100,c=4,r=3,q=2,i=7,m=0;cG5n\x1b\\"
        );
    }
}
