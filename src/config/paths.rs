//! Where yolop's own global config and data live.
//!
//! Every global path derives from one of two roots: the config root (settings,
//! profiles, hooks, extensions) and the data root (sessions, logs, models,
//! prompt history, materialized system skills). By default those are the
//! platform directories with a `yolop` folder inside, and both can be pointed
//! somewhere else with `--config-dir` / `--data-dir` or `YOLOP_CONFIG_DIR` /
//! `YOLOP_DATA_DIR`, which is what makes isolated test runs and several
//! side-by-side yolop identities possible without juggling `HOME`.
//!
//! Note the roots returned here are yolop's *own* directories, already
//! including the `yolop` folder, so callers join a leaf name directly. An
//! override replaces that whole path rather than the platform prefix: a user
//! who says `--config-dir ~/work/yolop` gets exactly that, not
//! `~/work/yolop/yolop`.
//!
//! Directories belonging to *other* applications (Zed's settings, Buzz's
//! harness directory, the cross-agent `~/.agents/skills` convention) resolve
//! through `dirs` directly and are deliberately left alone.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Environment override for the config root.
pub const CONFIG_DIR_ENV: &str = "YOLOP_CONFIG_DIR";
/// Environment override for the data root.
pub const DATA_DIR_ENV: &str = "YOLOP_DATA_DIR";

static CONFIG_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static DATA_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Fix both roots for this process straight from argv, before anything else
/// runs. The flags are read here rather than from parsed clap matches because
/// building the command line already opens the coordination store, and the
/// crash reporter installs its hook earlier still: by the time clap has
/// matches, several global paths have been resolved. Scanning argv keeps one
/// rule for every one of them.
///
/// clap still owns the flags themselves (help text, missing-value errors);
/// this only needs their values early.
pub fn init_from_args<I, T>(args: I)
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    init(
        flag_value(&args, "--config-dir"),
        flag_value(&args, "--data-dir"),
    );
}

/// Value of `--flag value` or `--flag=value`, last occurrence winning as clap
/// does. Scanning stops at `--`, after which nothing is a yolop flag.
fn flag_value(args: &[OsString], flag: &str) -> Option<PathBuf> {
    let inline = format!("{flag}=");
    let mut found = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == flag {
            found = args.next().cloned();
        } else if let Some(value) = text.strip_prefix(&inline) {
            found = Some(OsString::from(value));
        }
    }
    found.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Fix both roots for this process. Called by [`init_from_args`]; separate so
/// the precedence can be exercised without a command line.
pub fn init(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) {
    let _ = CONFIG_DIR.set(resolve(
        config_dir,
        env_override(CONFIG_DIR_ENV),
        dirs::config_dir,
    ));
    let _ = DATA_DIR.set(resolve(
        data_dir,
        env_override(DATA_DIR_ENV),
        dirs::data_dir,
    ));
}

/// yolop's config root, or `None` on a platform with no resolvable config
/// directory (minimal containers, CI without `HOME`).
pub fn config_dir() -> Option<PathBuf> {
    CONFIG_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| resolve(None, env_override(CONFIG_DIR_ENV), dirs::config_dir))
}

/// yolop's data root, or `None` when the platform has no data directory.
pub fn data_dir() -> Option<PathBuf> {
    DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| resolve(None, env_override(DATA_DIR_ENV), dirs::data_dir))
}

fn env_override(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// Flag beats environment beats platform default. An override names yolop's
/// directory itself; only the platform default gets the `yolop` folder joined
/// on. Relative overrides are resolved against the startup working directory,
/// since yolop may run turns from a worktree elsewhere.
fn resolve(
    flag: Option<PathBuf>,
    env: Option<OsString>,
    platform: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    // An exported-but-empty variable is how a shell spells "unset"; taking it
    // as a path would send everything to the working directory.
    let env = env.filter(|value| !value.is_empty());
    if let Some(dir) = flag.or_else(|| env.map(PathBuf::from)) {
        return Some(std::path::absolute(&dir).unwrap_or(dir));
    }
    platform().map(|dir| dir.join("yolop"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_flag_wins_over_the_environment_and_the_platform() {
        let resolved = resolve(
            Some(PathBuf::from("/flag/dir")),
            Some(OsString::from("/env/dir")),
            || Some(PathBuf::from("/platform")),
        );
        assert_eq!(resolved.as_deref(), Some(Path::new("/flag/dir")));
    }

    #[test]
    fn the_environment_wins_over_the_platform() {
        let resolved = resolve(None, Some(OsString::from("/env/dir")), || {
            Some(PathBuf::from("/platform"))
        });
        assert_eq!(resolved.as_deref(), Some(Path::new("/env/dir")));
    }

    #[test]
    fn an_override_is_the_directory_itself_but_the_platform_gains_a_yolop_folder() {
        let overridden = resolve(Some(PathBuf::from("/work/agent-a")), None, || {
            Some(PathBuf::from("/platform"))
        });
        assert_eq!(overridden.as_deref(), Some(Path::new("/work/agent-a")));

        let defaulted = resolve(None, None, || Some(PathBuf::from("/platform")));
        assert_eq!(defaulted.as_deref(), Some(Path::new("/platform/yolop")));
    }

    #[test]
    fn a_relative_override_is_made_absolute() {
        let resolved = resolve(Some(PathBuf::from("relative/dir")), None, || None)
            .expect("a flag always resolves");
        assert!(resolved.is_absolute(), "got: {}", resolved.display());
        assert!(
            resolved.ends_with("relative/dir"),
            "got: {}",
            resolved.display()
        );
    }

    #[test]
    fn flags_are_read_from_argv_in_both_spellings() {
        let args: Vec<OsString> = ["yolop", "--config-dir", "/a", "--data-dir=/b", "-p", "hi"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flag_value(&args, "--config-dir"), Some(PathBuf::from("/a")));
        assert_eq!(flag_value(&args, "--data-dir"), Some(PathBuf::from("/b")));
    }

    #[test]
    fn the_last_flag_wins_and_nothing_after_a_double_dash_counts() {
        let args: Vec<OsString> = [
            "yolop",
            "--data-dir",
            "/first",
            "--data-dir",
            "/second",
            "--",
            "--data-dir",
            "/ignored",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(
            flag_value(&args, "--data-dir"),
            Some(PathBuf::from("/second"))
        );
    }

    #[test]
    fn a_flag_without_a_value_is_left_to_clap() {
        let args: Vec<OsString> = ["yolop", "--data-dir"].iter().map(OsString::from).collect();
        assert_eq!(flag_value(&args, "--data-dir"), None);
    }

    #[test]
    fn no_override_and_no_platform_directory_resolves_to_nothing() {
        assert_eq!(resolve(None, None, || None), None);
    }

    #[test]
    fn an_empty_environment_override_is_ignored() {
        let resolved = resolve(None, OsString::from("").into(), || {
            Some(PathBuf::from("/platform"))
        });
        assert_eq!(resolved.as_deref(), Some(Path::new("/platform/yolop")));
    }
}
