//! One-time migration of the database written under the pre-rename `Mailthin` folder.
use std::fs;
use std::io;
use std::path::Path;

const DB_FILES: [&str; 3] = ["mail.db", "mail.db-wal", "mail.db-shm"];

/// Copies the encrypted database from `legacy_dir` into `data_dir` when `data_dir` has no
/// database yet. Returns `true` when files were copied. The legacy files are left in place.
///
/// Files are first copied under a `.partial` name and renamed only once all copies succeed, so
/// an interrupted copy never leaves a `mail.db` that later starts would mistake for a full one.
pub fn migrate_database(legacy_dir: &Path, data_dir: &Path) -> io::Result<bool> {
    if data_dir.join("mail.db").exists() || !legacy_dir.join("mail.db").is_file() {
        return Ok(false);
    }
    fs::create_dir_all(data_dir)?;

    let mut copied = Vec::new();
    for name in DB_FILES {
        let src = legacy_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let partial = data_dir.join(format!("{name}.partial"));
        if let Err(e) = fs::copy(&src, &partial) {
            for (_, partial) in &copied {
                let _ = fs::remove_file(partial);
            }
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
        copied.push((data_dir.join(name), partial));
    }
    for (dest, partial) in copied {
        fs::rename(partial, dest)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_database_files_once() {
        let base = std::env::temp_dir().join(format!("pigeonmail-migrate-{}", std::process::id()));
        let legacy = base.join("Mailthin");
        let data = base.join("PigeonMail");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("mail.db"), b"db").unwrap();
        fs::write(legacy.join("mail.db-wal"), b"wal").unwrap();

        assert!(migrate_database(&legacy, &data).unwrap());
        assert_eq!(fs::read(data.join("mail.db")).unwrap(), b"db");
        assert_eq!(fs::read(data.join("mail.db-wal")).unwrap(), b"wal");
        assert!(!data.join("mail.db-shm").exists());
        assert!(legacy.join("mail.db").exists(), "legacy database must be kept");

        assert!(!migrate_database(&legacy, &data).unwrap(), "must not overwrite an existing database");
        fs::remove_dir_all(&base).unwrap();
    }
}
