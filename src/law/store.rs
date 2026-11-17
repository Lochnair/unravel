// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{io, path::PathBuf};

use anyhow::Context;

/// Files a law keeps across restarts, in its own folder under the instance
/// folder. A name is a plain file name: no separators, `.` or `..`.
pub struct LawStore {
    dir: PathBuf,
}

impl LawStore {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The whole file, or `None` if it does not exist.
    pub async fn read(&self, name: &str) -> anyhow::Result<Option<String>> {
        let path = self.path(name)?;
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("Reading {}", path.display())),
        }
    }

    /// Replaces the whole file. A reader sees either the old or the new contents.
    pub async fn write(&self, name: &str, contents: &str) -> anyhow::Result<()> {
        let path = self.path(name)?;
        let tmp = self.dir.join(format!(".{name}.tmp"));
        async {
            tokio::fs::create_dir_all(&self.dir).await?;
            tokio::fs::write(&tmp, contents).await?;
            tokio::fs::rename(&tmp, &path).await
        }
        .await
        .with_context(|| format!("Writing {}", path.display()))
    }

    fn path(&self, name: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']),
            "Invalid law store file name {name:?}"
        );
        Ok(self.dir.join(name))
    }
}
