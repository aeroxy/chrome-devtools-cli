use tokio::fs;

/// The result of executing a CLI command.
///
/// Carries both the human-readable output string and structured metadata
/// about what happened during execution (e.g., whether a navigation occurred).
#[derive(Default)]
pub struct CommandResult {
    /// Human-readable output to display to the user.
    pub output: String,
    /// The URL the page navigated to during the command, if any.
    pub navigated_to: Option<String>,
    /// The target (page) ID this command ran against, if applicable.
    pub target_id: Option<String>,
    /// Optional error code for non-fatal issues (e.g., partial failures).
    pub error_code: Option<u32>,
    /// Lines for stderr beside the output (warnings, page errors), so they
    /// never mix into output that is data, such as a base64 screenshot.
    pub notes: Vec<String>,
    /// The command succeeded, but a cleanup write found the connection gone.
    /// The daemon retires its socket after returning this result; the caller
    /// keeps the success and is not told to repeat the action.
    pub connection_lost: bool,
}

impl CommandResult {
    /// Create a result with just an output string.
    pub fn output(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Default::default()
        }
    }

    /// Set the `navigated_to` field.
    pub fn with_navigated_to(mut self, url: impl Into<String>) -> Self {
        self.navigated_to = Some(url.into());
        self
    }

    /// Set the `target_id` field.
    #[allow(dead_code)]
    pub fn with_target_id(mut self, target_id: impl Into<String>) -> Self {
        self.target_id = Some(target_id.into());
        self
    }

    /// Set `navigated_to` if the URL changed from `before` to `after`.
    pub fn with_navigated_to_if_changed(mut self, after: String, before: String) -> Self {
        if after != before {
            self.navigated_to = Some(after);
        }
        self
    }

    /// Add a line for stderr.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Report a navigation when the URL changed or a new document loaded.
    /// The URL alone misses a reload, or a form that posts back to its own URL.
    pub fn with_navigation(self, after: String, before: String, new_document: bool) -> Self {
        if new_document {
            self.with_navigated_to(after)
        } else {
            self.with_navigated_to_if_changed(after, before)
        }
    }

    /// Set the `error_code` field.
    #[allow(dead_code)]
    pub fn with_error_code(mut self, code: u32) -> Self {
        self.error_code = Some(code);
        self
    }

    /// Write output to a file and return a result with a confirmation message,
    /// or return self unchanged if no output path is given.
    ///
    /// This is a shared helper to avoid duplicating the write-to-file pattern
    /// across evaluate, navigate, snapshot, and screenshot commands.
    pub async fn save_output(self, path: Option<&str>) -> Result<Self, std::io::Error> {
        match path {
            Some(p) => {
                fs::write(p, &self.output).await?;
                Ok(CommandResult {
                    notes: self.notes,
                    ..CommandResult::output(format!("Output saved to {p}"))
                })
            }
            None => Ok(self),
        }
    }
}
