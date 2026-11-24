// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    io,
    path::{Path, PathBuf},
};

use anyhow::Context;

/// Files a law keeps across restarts, in its own folder under the instance
/// folder. A name is a plain file name: no separators, `.` or `..`.
pub struct LawStore {
    dir: PathBuf,
}

impl LawStore {
    pub(crate) fn new(folder: &Path, name: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            valid_path_component(name),
            "Invalid law store directory name {name:?}"
        );
        Ok(Self {
            dir: folder.join(name),
        })
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

    /// The whole file, or `None` if it does not exist.
    // Only script laws call these; a hook is already a blocking call.
    #[cfg_attr(not(feature = "lua"), allow(dead_code))]
    pub fn read_blocking(&self, name: &str) -> anyhow::Result<Option<String>> {
        let path = self.path(name)?;
        match std::fs::read_to_string(&path) {
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

    /// Replaces the whole file. A reader sees either the old or the new contents.
    // Only script laws call these; a hook is already a blocking call.
    #[cfg_attr(not(feature = "lua"), allow(dead_code))]
    pub fn write_blocking(&self, name: &str, contents: &str) -> anyhow::Result<()> {
        let path = self.path(name)?;
        let tmp = self.dir.join(format!(".{name}.tmp"));
        {
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(&tmp, contents)?;
            std::fs::rename(&tmp, &path)
        }
        .with_context(|| format!("Writing {}", path.display()))
    }

    fn path(&self, name: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            valid_path_component(name),
            "Invalid law store file name {name:?}"
        );
        Ok(self.dir.join(name))
    }
}

fn valid_path_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}
