#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // Oculta la consola de Windows en release para que sea una app de escritorio 100% nativa

mod app;
mod archiver;
mod config;
mod imap_client;
mod state;
mod storage;

use app::ImapBackupApp;
use eframe::egui::{self, Vec2, ViewportBuilder};

fn main() -> eframe::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("IMAP Email Backup & Migration Suite")
            .with_inner_size(Vec2::new(980.0, 720.0))
            .with_min_inner_size(Vec2::new(760.0, 540.0))
            .with_resizable(true),
        default_theme: eframe::Theme::Dark,
        ..Default::default()
    };

    eframe::run_native(
        "IMAP Email Backup & Migration Suite",
        native_options,
        Box::new(|cc| {
            // Configurar estilos visuales de egui (Dark theme moderno)
            let mut visuals = egui::Visuals::dark();
            visuals.window_rounding = 8.0.into();
            visuals.menu_rounding = 6.0.into();
            visuals.widgets.noninteractive.rounding = 4.0.into();
            visuals.widgets.inactive.rounding = 4.0.into();
            visuals.widgets.hovered.rounding = 4.0.into();
            visuals.widgets.active.rounding = 4.0.into();
            cc.egui_ctx.set_visuals(visuals);

            Box::new(ImapBackupApp::new(cc))
        }),
    )
}
