// Copyright (C) 2026 Esteban Chávez / Contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // Oculta la consola de Windows en release

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

    let icon_data = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok();

    let mut viewport = ViewportBuilder::default()
        .with_title("IMAP Email Backup & Migration Suite")
        .with_inner_size(Vec2::new(980.0, 720.0))
        .with_min_inner_size(Vec2::new(760.0, 540.0))
        .with_resizable(true);

    if let Some(icon) = icon_data {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
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
