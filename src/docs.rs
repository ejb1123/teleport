//! Where the documentation that help text and messages cite can be read:
//! the copy a package installed (`/usr/share/doc/teleport`, or
//! `usr/share/doc/teleport` in an unpacked tarball, nix/bundle.nix), else the
//! repository's `docs/` on GitHub at this version's tag.

use std::path::{Path, PathBuf};

const INSTALLED: &str = "/usr/share/doc/teleport";
const REPOSITORY: &str = "https://github.com/ejb1123/teleport";

/// A page every copy has, to tell the documentation from an unrelated
/// directory of the same name.
const MARKER: &str = "wake-and-unattended.md";

/// The local documentation directory, if there is one that outlives this
/// process (not an AppImage's temporary mount).
fn local() -> Option<PathBuf> {
    if std::env::var_os("APPIMAGE").is_some() {
        return None;
    }
    let exe = crate::self_exe().ok()?.canonicalize().ok()?;
    candidates(exe.parent()?)
        .into_iter()
        .find(|candidate| candidate.join(MARKER).is_file())?
        .canonicalize()
        .ok()
}

/// Installed packages first, then relative to the executable: a tarball's
/// `AppRun` (`usr/share/doc/teleport` next to it) or a bundle's
/// `usr/bin/teleport` (`../share/doc/teleport`).
fn candidates(exe_directory: &Path) -> [PathBuf; 3] {
    [
        PathBuf::from(INSTALLED),
        exe_directory.join("usr/share/doc/teleport"),
        exe_directory.join("../share/doc/teleport"),
    ]
}

/// `docs/` at this version's tag; `kind` is `blob` (a page) or `tree`.
fn online(kind: &str, name: &str) -> String {
    format!(
        "{REPOSITORY}/{kind}/v{}/docs/{name}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Where to read `name` (`guests.md`): a local path or a URL.
pub fn page(name: &str) -> String {
    match local() {
        Some(directory) => directory.join(name).display().to_string(),
        None => online("blob", name),
    }
}

/// The footer of every command's `--help`: where the pages it names are.
pub fn help() -> String {
    let location = match local() {
        Some(directory) => format!("{}/", directory.display()),
        None => online("tree", ""),
    };
    format!("Documentation (the .md pages named above): {location}")
}

/// Adds `help()` to `command` and every subcommand under it.
pub fn with_help(command: clap::Command) -> clap::Command {
    let footer = help();
    add_help(command, &footer)
}

fn add_help(command: clap::Command, footer: &str) -> clap::Command {
    command
        .after_help(footer.to_owned())
        .mut_subcommands(|subcommand| add_help(subcommand, footer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_pages_are_pinned_to_this_version() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            online("blob", "guests.md"),
            format!("https://github.com/ejb1123/teleport/blob/v{version}/docs/guests.md")
        );
        assert_eq!(
            online("tree", ""),
            format!("https://github.com/ejb1123/teleport/tree/v{version}/docs/")
        );
    }

    #[test]
    fn packages_come_before_the_bundle() {
        let [installed, tarball, bundle] = candidates(Path::new("/opt/teleport"));
        assert_eq!(installed, Path::new("/usr/share/doc/teleport"));
        assert_eq!(tarball, Path::new("/opt/teleport/usr/share/doc/teleport"));
        assert_eq!(bundle, Path::new("/opt/teleport/../share/doc/teleport"));
    }

    #[test]
    fn every_subcommand_gets_the_footer() {
        let command = clap::Command::new("teleport")
            .subcommand(clap::Command::new("host-admin").subcommand(clap::Command::new("status")));
        let mut command = add_help(command, "Documentation: here");
        let mut nested = command
            .find_subcommand_mut("host-admin")
            .unwrap()
            .find_subcommand_mut("status")
            .unwrap()
            .clone();
        assert!(
            nested
                .render_help()
                .to_string()
                .contains("Documentation: here")
        );
        assert!(
            command
                .render_help()
                .to_string()
                .contains("Documentation: here")
        );
    }
}
