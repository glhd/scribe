use std::{env, fs, path::PathBuf};

use uuid::Uuid;

use crate::storage::Store;

const SKILL_TEMPLATE: &str = include_str!("../resources/planning-scribe/SKILL.md");
const MANAGED_MARKER: &str = "<!-- installed-by-scribe -->";

pub fn install(store: &Store) -> Result<PathBuf, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate the Scribe executable: {error}"))?;
    install_at(store, &executable, &home_dir()?)
}

fn install_at(
    store: &Store,
    executable: &std::path::Path,
    home: &std::path::Path,
) -> Result<PathBuf, String> {
    let shim = store.root().join("bin/scribe");
    install_shim(executable, &shim)?;

    let skill = home.join(".claude/skills/planning-scribe/SKILL.md");
    let existing = match fs::read_to_string(&skill) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "cannot inspect the existing planning-scribe skill at {}: {error}",
                skill.display()
            ));
        }
    };
    if existing.is_some_and(|content| !content.contains(MANAGED_MARKER)) {
        let backup = skill.with_file_name(format!("SKILL.md.before-scribe-{}", Uuid::new_v4()));
        fs::copy(&skill, &backup).map_err(|error| {
            format!(
                "cannot back up the existing planning-scribe skill to {}: {error}",
                backup.display()
            )
        })?;
    }
    let content = SKILL_TEMPLATE.replace("{{SCRIBE_BIN}}", &shim.to_string_lossy());
    write_atomic(&skill, content.as_bytes())?;
    Ok(skill)
}

#[cfg(unix)]
fn install_shim(executable: &std::path::Path, shim: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::symlink;

    let parent = shim
        .parent()
        .ok_or_else(|| format!("CLI path has no parent: {}", shim.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".scribe-shim-{}", Uuid::new_v4()));
    symlink(executable, &temporary)
        .and_then(|_| fs::rename(&temporary, shim))
        .map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!("cannot install CLI at {}: {error}", shim.display())
        })
}

#[cfg(not(unix))]
fn install_shim(executable: &std::path::Path, shim: &std::path::Path) -> Result<(), String> {
    let parent = shim
        .parent()
        .ok_or_else(|| format!("CLI path has no parent: {}", shim.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    fs::copy(executable, shim)
        .map(|_| ())
        .map_err(|error| format!("cannot install CLI at {}: {error}", shim.display()))
}

fn write_atomic(path: &std::path::Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("skill path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".SKILL.md.scribe-{}", Uuid::new_v4()));
    fs::write(&temporary, content)
        .and_then(|_| fs::rename(&temporary, path))
        .map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!("cannot install {}: {error}", path.display())
        })
}

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot locate the user home directory".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_skill_has_valid_identity_and_session_contract() {
        assert!(SKILL_TEMPLATE.starts_with("---\nname: planning-scribe\n"));
        assert!(SKILL_TEMPLATE.contains("session attach --repo"));
        assert!(SKILL_TEMPLATE.contains("tick --wait --cursor planning-scribe"));
        assert!(SKILL_TEMPLATE.contains("session finish"));
        assert!(SKILL_TEMPLATE.contains(MANAGED_MARKER));
    }

    #[test]
    fn installer_creates_stable_shim_and_managed_skill() {
        let root = env::temp_dir().join(format!("scribe-integration-test-{}", Uuid::new_v4()));
        let store = Store::open_at(root.join("data")).unwrap();
        let executable = root.join("Scribe");
        fs::create_dir_all(&root).unwrap();
        fs::write(&executable, "test executable").unwrap();
        let skill = install_at(&store, &executable, &root.join("home")).unwrap();
        let shim = store.root().join("bin/scribe");
        assert!(shim.exists());
        let content = fs::read_to_string(skill).unwrap();
        assert!(content.contains(MANAGED_MARKER));
        assert!(content.contains(&shim.to_string_lossy().to_string()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installer_backs_up_an_existing_unmanaged_skill() {
        let root = env::temp_dir().join(format!("scribe-integration-test-{}", Uuid::new_v4()));
        let store = Store::open_at(root.join("data")).unwrap();
        let executable = root.join("Scribe");
        let skill_directory = root.join("home/.claude/skills/planning-scribe");
        fs::create_dir_all(&skill_directory).unwrap();
        fs::write(&executable, "test executable").unwrap();
        fs::write(skill_directory.join("SKILL.md"), "original skill").unwrap();

        let skill = install_at(&store, &executable, &root.join("home")).unwrap();

        assert!(fs::read_to_string(skill).unwrap().contains(MANAGED_MARKER));
        let backups = fs::read_dir(skill_directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("SKILL.md.before-scribe-")
            })
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read_to_string(backups[0].path()).unwrap(),
            "original skill"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
