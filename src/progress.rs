use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;

#[derive(Clone)]
pub struct ProgressTracker {
    mp: Arc<MultiProgress>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            mp: Arc::new(MultiProgress::new()),
        }
    }

    pub fn create_account_spinner(&self, account_email: &str) -> ProgressBar {
        let pb = self.mp.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
                .template("{spinner:.green} [{prefix:.bold}] {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        pb.set_prefix(account_email.to_string());
        pb.set_message("Conectando...");
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        pb
    }

    pub fn create_folder_progress_bar(&self, folder_name: &str, total_msgs: u64) -> ProgressBar {
        let pb = self.mp.add(ProgressBar::new(total_msgs));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "  {spinner:.blue} [{prefix}] [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({percent}%) {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        pb.set_prefix(folder_name.to_string());
        pb.set_message("Descargando...");
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        pb
    }

    pub fn println(&self, msg: impl AsRef<str>) {
        let _ = self.mp.println(msg.as_ref());
    }
}
