// SPDX-License-Identifier: GPL-3.0-or-later
//! Which language this process draws its own text in.
//!
//! wlRIX's interface comes in American English and Japanese, the same two the C# applications
//! carry in their `.resx` catalogs, and it is chosen the same way: from the locale the session
//! was started under, once, for the life of the process.
//!
//! Text is written as a pair where it is shown -- `tr("Close", "閉じる")` -- rather than kept
//! in a catalog. With two languages a lookup table is only indirection, and a pair cannot drift
//! away from the English it translates without the drift being in plain sight.
//!
//! The same file is in each wlRIX crate that draws or sends text of its own, kept in step by
//! hand.

use std::sync::OnceLock;

/// A language wlRIX has text for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    Japanese,
}

impl Language {
    /// The language this process is running in.
    ///
    /// Always English under `cargo test`: the tests assert on the English text, and the
    /// machine running them may well be in a Japanese session. Tests of the Japanese side ask
    /// for it by name with [`Language::pick`].
    pub fn current() -> Self {
        static CURRENT: OnceLock<Language> = OnceLock::new();
        if cfg!(test) {
            return Self::English;
        }
        *CURRENT.get_or_init(Self::from_env)
    }

    /// Read the language from the environment.
    ///
    /// POSIX's order for the category that covers user-visible text: `LC_ALL` overrides
    /// everything, then `LC_MESSAGES`, then `LANG` for both. An empty value counts as unset.
    /// This is also the order .NET reads, so a C# window and a Rust one in the same session
    /// never disagree. `LANGUAGE`, GNU gettext's priority list, is not consulted, for the same
    /// reason.
    ///
    /// With none of the three set, the machine's own locale from `/etc/locale.conf`. A login
    /// session always has `LANG`, but the login screen is started by greetd and on some
    /// distributions gets nothing, and the screen a Japanese machine shows first should not be
    /// the one place it is in English.
    pub fn from_env() -> Self {
        for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            match std::env::var(name) {
                Ok(value) if !value.trim().is_empty() => return Self::parse(&value),
                _ => continue,
            }
        }
        std::fs::read_to_string("/etc/locale.conf")
            .ok()
            .and_then(|conf| system_locale(&conf))
            .map_or(Self::English, |spec| Self::parse(&spec))
    }

    /// The language a locale name asks for: `ja`, `ja_JP`, `ja_JP.UTF-8` and the like are
    /// Japanese, and everything else -- `C`, `POSIX`, any language wlRIX has no text for -- is
    /// English.
    pub fn parse(spec: &str) -> Self {
        let lang = spec.trim().split(['_', '.', '@']).next().unwrap_or("");
        match lang {
            "ja" => Self::Japanese,
            _ => Self::English,
        }
    }

    /// The one of the two that belongs to this language.
    pub fn pick<T>(self, english: T, japanese: T) -> T {
        match self {
            Self::English => english,
            Self::Japanese => japanese,
        }
    }
}

/// The locale a `locale.conf` sets, by the same precedence as the environment.
///
/// The file is a list of shell-style `NAME=value` assignments, the value possibly quoted.
fn system_locale(conf: &str) -> Option<String> {
    let value = |wanted: &str| {
        conf.lines().find_map(|line| {
            let (name, value) = line.trim().split_once('=')?;
            let value = value.trim().trim_matches(['"', '\'']);
            (name.trim() == wanted && !value.is_empty()).then(|| value.to_owned())
        })
    };
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(value)
}

/// `english` or `japanese`, whichever this process is running in.
pub fn tr(english: &'static str, japanese: &'static str) -> &'static str {
    Language::current().pick(english, japanese)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_is_recognized_in_every_spelling() {
        for spec in [
            "ja",
            "ja_JP",
            "ja_JP.UTF-8",
            "ja_JP.eucJP",
            "ja@modifier",
            " ja_JP.UTF-8 ",
        ] {
            assert_eq!(Language::parse(spec), Language::Japanese, "{spec}");
        }
    }

    #[test]
    fn everything_else_is_english() {
        for spec in [
            "",
            "C",
            "POSIX",
            "C.UTF-8",
            "en_US.UTF-8",
            "de_DE",
            "jav_ID",
            "_JP",
        ] {
            assert_eq!(Language::parse(spec), Language::English, "{spec}");
        }
    }

    #[test]
    fn locale_conf_is_read_like_the_environment() {
        assert_eq!(
            system_locale("LANG=ja_JP.UTF-8\n").as_deref(),
            Some("ja_JP.UTF-8")
        );
        assert_eq!(
            system_locale("LANG=\"ja_JP.UTF-8\"").as_deref(),
            Some("ja_JP.UTF-8")
        );
        assert_eq!(
            system_locale("LANG=en_US.UTF-8\nLC_MESSAGES=ja_JP.UTF-8\n").as_deref(),
            Some("ja_JP.UTF-8")
        );
        assert_eq!(
            system_locale("# LANG=ja_JP.UTF-8\nLC_TIME=ja_JP.UTF-8\n"),
            None
        );
        assert_eq!(system_locale(""), None);
    }

    #[test]
    fn tests_run_in_english() {
        assert_eq!(tr("Close", "閉じる"), "Close");
    }
}
