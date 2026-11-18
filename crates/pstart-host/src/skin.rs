//! Skin colors loaded from a small TOML file.

use serde::Deserialize;

/// Skin load or validation failure.
#[derive(Debug, thiserror::Error)]
pub enum SkinError {
    /// TOML structure is invalid.
    #[error("skin parse failed: {0}")]
    Parse(String),
    /// A color is not `#RRGGBB`.
    #[error("skin color is invalid: {0}")]
    Color(String),
}

/// GDI colors used by the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skin {
    /// Window background.
    pub background: u32,
    /// Selected row background.
    pub selection: u32,
    /// Item text.
    pub text: u32,
    /// Item path.
    pub path: u32,
}

#[derive(Deserialize)]
struct SkinFile {
    colors: Colors,
}

#[derive(Deserialize)]
struct Colors {
    background: String,
    selection: String,
    text: String,
    path: String,
}

const fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red | (green << 8) | (blue << 16)
}

impl Skin {
    /// Default dark skin.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            background: rgb(22, 22, 27),
            selection: rgb(42, 58, 78),
            text: rgb(235, 235, 240),
            path: rgb(150, 150, 160),
        }
    }

    /// Light skin.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            background: rgb(245, 245, 240),
            selection: rgb(200, 220, 245),
            text: rgb(30, 30, 35),
            path: rgb(90, 90, 100),
        }
    }

    /// Parse a skin from TOML.
    pub fn from_toml(input: &str) -> Result<Self, SkinError> {
        let file: SkinFile =
            toml::from_str(input).map_err(|error| SkinError::Parse(error.to_string()))?;
        Ok(Self {
            background: parse_color(&file.colors.background)?,
            selection: parse_color(&file.colors.selection)?,
            text: parse_color(&file.colors.text)?,
            path: parse_color(&file.colors.path)?,
        })
    }
}

fn parse_color(value: &str) -> Result<u32, SkinError> {
    let hex = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| SkinError::Color(value.to_owned()))?;
    let red = u32::from_str_radix(&hex[0..2], 16).expect("validated hex digits");
    let green = u32::from_str_radix(&hex[2..4], 16).expect("validated hex digits");
    let blue = u32::from_str_radix(&hex[4..6], 16).expect("validated hex digits");
    Ok(rgb(red, green, blue))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_differ() {
        assert_ne!(Skin::dark(), Skin::light());
        assert_eq!(Skin::dark().background, rgb(22, 22, 27));
        assert_eq!(Skin::light().text, rgb(30, 30, 35));
    }

    #[test]
    fn parses_custom_skin() {
        let skin = Skin::from_toml(
            r##"
            [colors]
            background = "#112233"
            selection = "#445566"
            text = "#DDEEFF"
            path = "#778899"
            "##,
        )
        .unwrap();
        assert_eq!(skin.background, rgb(0x11, 0x22, 0x33));
        assert_eq!(skin.selection, rgb(0x44, 0x55, 0x66));
        assert_eq!(skin.text, rgb(0xdd, 0xee, 0xff));
        assert_eq!(skin.path, rgb(0x77, 0x88, 0x99));
    }

    #[test]
    fn rejects_invalid_skin() {
        assert!(matches!(
            Skin::from_toml("not toml"),
            Err(SkinError::Parse(_))
        ));
        assert!(matches!(
            Skin::from_toml("[colors]\nbackground='#112233'"),
            Err(SkinError::Parse(_))
        ));
        for color in ["red", "#12345", "#1234567", "#GGGGGG"] {
            let text = format!(
                "[colors]\nbackground='{color}'\nselection='#445566'\ntext='#ddeeff'\npath='#778899'\n"
            );
            assert!(matches!(Skin::from_toml(&text), Err(SkinError::Color(_))));
        }
        for text in [
            "[colors]\nbackground='#112233'\nselection='red'\ntext='#ddeeff'\npath='#778899'\n",
            "[colors]\nbackground='#112233'\nselection='#445566'\ntext='red'\npath='#778899'\n",
            "[colors]\nbackground='#112233'\nselection='#445566'\ntext='#ddeeff'\npath='red'\n",
        ] {
            assert!(matches!(Skin::from_toml(text), Err(SkinError::Color(_))));
        }
    }
}
