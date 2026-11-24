//! Private session drafts survive switching sessions and restarting the TUI.

use super::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const MAX_DRAFT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Draft {
    text: String,
    pastes: Vec<(String, String)>,
    images: Vec<ContentPart>,
}

impl App {
    fn draft_path(&self) -> std::path::PathBuf {
        self.session
            .handles()
            .checkpoints
            .sessions_dir()
            .join(self.session_id().to_string())
            .join("draft.json")
    }

    pub(super) fn save_draft(&self) -> Result<()> {
        let path = self.draft_path();
        let draft = Draft {
            text: self.input_text(),
            pastes: self.pending_pastes.clone(),
            images: self.pending_images.clone(),
        };
        if draft.text.is_empty() && draft.images.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(());
        }
        let size = draft.text.len()
            + draft
                .pastes
                .iter()
                .map(|(key, text)| key.len() + text.len())
                .sum::<usize>()
            + draft
                .images
                .iter()
                .map(|image| match image {
                    ContentPart::Image(image) => {
                        image.base64.as_ref().map(String::len).unwrap_or(0)
                    }
                    _ => 0,
                })
                .sum::<usize>();
        anyhow::ensure!(
            size <= MAX_DRAFT_BYTES,
            "draft exceeds the 32 MiB session limit"
        );
        let bytes = serde_json::to_vec(&draft)?;
        anyhow::ensure!(
            bytes.len() <= MAX_DRAFT_BYTES,
            "encoded draft exceeds the 32 MiB session limit"
        );
        let directory = path.parent().expect("draft directory");
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }

    pub(super) fn restore_draft(&mut self) {
        let result = (|| -> Result<Option<Draft>> {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let file = match options.open(self.draft_path()) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "saved draft is not a regular file"
            );
            let mut bytes = Vec::new();
            file.take(MAX_DRAFT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= MAX_DRAFT_BYTES, "saved draft exceeds 32 MiB");
            Ok(Some(serde_json::from_slice(&bytes)?))
        })();
        match result {
            Ok(Some(draft)) => {
                self.set_input_text(draft.text);
                self.pending_pastes = draft.pastes;
                self.pending_images.extend(draft.images);
            }
            Ok(None) => {}
            Err(error) => self.push_system(format!("Could not restore saved draft: {error:#}")),
        }
    }
}
