use egui_material_icons;
use egui_commonmark::*;
use std::error::Error;
use pulldown_cmark::{Parser, Options};


/// We derive Deserialize/Serialize so we can persist app state on shutdown.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)] // if we add new fields, give them default values when deserializing old state
pub struct TemplateApp {
    // Example stuff:
    label: String,
    location: String,
    page: String,
    status: String,

    // for history
    back: Vec<String>,
    forward: Vec<String>,

    #[serde(skip)] // This how you opt-out of serialization of a field
    value: f32,
}

impl Default for TemplateApp {
    fn default() -> Self {
        Self {
            // Example stuff:
            label: "Hello World!".to_owned(),
            location: "https://example.com".to_owned(),
            page: "".to_owned(),
            status: "Loaded".to_owned(),
            back: Vec::new().to_owned(),
            forward: Vec::new().to_owned(),
            value: 2.7,
        }
    }
}

impl TemplateApp {
    /// Called once before the first frame.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // This is also where you can customize the look and feel of egui using
        // `cc.egui_ctx.set_visuals` and `cc.egui_ctx.set_fonts`.
        egui_material_icons::initialize(&cc.egui_ctx);

        // Load previous app state (if any).
        // Note that you must enable the `persistence` feature for this to work.
        if let Some(storage) = cc.storage {
            return eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
        }

        Default::default()
    }

    pub fn navigate(&mut self) -> String {
        self.status = "Loading...".to_string();
        println!("URL to navigate to: {}", self.location);
        //let resp = reqwest::blocking::get(location).unwrap().text();
        let resp = reqwest::blocking::get(&self.location)
            .and_then(|r| r.text())
            .map_err(|e| e.to_string());
        //println!("{:#?}", resp);
        self.status = "Loaded".to_string();
        return resp.unwrap_or_else(|_| "Failed to load page".to_string());
    }

    pub fn visit(&mut self) -> String {
        self.status = "Loading...".to_string();
        println!("Visiting URL: {}", self.location);
        //let resp = reqwest::blocking::get(location).unwrap().text();
        let resp = reqwest::blocking::get(&self.location)
            .and_then(|r| r.text())
            .map_err(|e| e.to_string());
        self.status = "Loaded".to_string();
        // update the current page in history
        println!("self back: {:?}", self.back);
        println!("self forward: {:?}", self.forward);
        return resp.unwrap_or_else(|_| "Failed to load page".to_string());
    }

    pub fn back(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(last) = self.back.pop() {
            self.forward.push(self.location.clone());
            self.location = last.clone();
            self.page = self.visit();
            Ok(())
        } else {
            Err("No back history".into())
        }
    }

    pub fn forward(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.location.clone());
            self.location = next.clone();
            self.page = self.visit();
            Ok(())
        } else {
            Err("No forward history".into())
        }
    }
}

impl eframe::App for TemplateApp {
    /// Called by the frame work to save state before shutdown.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    /// Called each time the UI needs repainting, which may be many times per second.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Put your widgets into a `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Window` or `Area`.
        // For inspiration and more examples, go to https://emilk.github.io/egui

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            // The top panel is often a good place for a menu bar:

            egui::menu::bar(ui, |ui| {
                // NOTE: no File->Quit on web pages!
                let is_web = cfg!(target_arch = "wasm32");
                if !is_web {
                    ui.menu_button(egui_material_icons::icons::ICON_MENU, |ui| {
                        egui::widgets::global_theme_preference_buttons(ui);
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.add_space(3.0);
                }
                ui.add_space(1.0);
                ui.button(egui_material_icons::icons::ICON_ARROW_BACK)
                    .on_hover_text("Back")
                    .clicked()
                    .then(|| {
                        if let Err(e) = self.back() {
                            self.status = e.to_string();
                        } else {
                            self.status = "Loaded".to_string();
                        }
                    });
                ui.add_space(1.0);
                ui.button(egui_material_icons::icons::ICON_ARROW_FORWARD)
                    .on_hover_text("Forward")
                    .clicked()
                    .then(|| {
                        if let Err(e) = self.forward() {
                            self.status = e.to_string();
                        } else {
                            self.status = "Loaded".to_string();
                        }
                    });
                ui.add_space(1.0);




                let button_width = 25.0;
                let text_edit_width = ui.available_width() - button_width;
                let response = ui.add_sized([text_edit_width.max(0.0), 20.0], egui::TextEdit::singleline(&mut self.location));
                if response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.status = "Loading...".to_string();
                    // update the current page in history
                    self.back.push(self.location.clone());
                    // clear the forward history
                    self.forward.clear();
                    self.page = self.navigate();
                }
                ui.add_space(1.0);
                ui.button(egui_material_icons::icons::ICON_KEYBOARD_DOUBLE_ARROW_RIGHT)
                    .on_hover_text("Go")
                    .clicked()
                    .then(|| {
                        self.status = "Loading...".to_string();
                        // update the current page in history
                        self.back.push(self.location.clone());
                        // clear the forward history
                        self.forward.clear();
                        self.page = self.navigate();
                    });
            });
        });

        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            //powered_by_egui_and_eframe(ui);
            let mut status_display: String = "Status: ".to_owned();
            let status: &str = self.status.as_str();
            status_display.push_str(status);
            ui.label(status_display);
        });

        egui::SidePanel::left("side_panel").show(ctx, |ui| {
            // The side panel is often a good place for a navigation menu:
            ui.separator();

            if ui.button("Home").clicked() {
                self.status = "Loading...".to_string();
                self.page = self.navigate();
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {

            let binding = self.page.clone();
            let markdown = binding.as_str();
            let mut all_links: Vec<String> = Vec::new();

            let mut cache = CommonMarkCache::default();

            ui.style_mut().url_in_tooltip = true;
            egui::ScrollArea::vertical().show(ui, |ui| {
                let parser = pulldown_cmark::Parser::new(markdown);
                for event in parser {

                    match event {
                        pulldown_cmark::Event::Start(contents) => {
                            match contents {
                                pulldown_cmark::Tag::Link{link_type: _, dest_url: url, title: _, id: _} => {
                                    cache.add_link_hook(url.to_string());
                                    all_links.push(url.to_string());
                                },
                                _ => {
                                }
                            }
                        },
                        _ => {
                        }
                    }
                }
                ui.style_mut().url_in_tooltip = true;
                CommonMarkViewer::new().show(ui, &mut cache, markdown);
                for link in all_links {
                    if cache.get_link_hook(&link) == Some(true) {
                        println!("Link was clicked {link}");
                        self.back.push(self.location.clone());
                        self.location = link.clone();
                        self.status = "Loading...".to_string();
                        // update the current page in history
                        // clear the forward history
                        self.forward.clear();
                        self.page = self.navigate();
                    }
                    //ui.hyperlink_to(link, link);
                }


            });



            //ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            //    powered_by_egui_and_eframe(ui);
            //    egui::warn_if_debug_build(ui);
            //});
            //ui.separator();
            //ui.label("end");
        });

    }

}


fn powered_by_egui_and_eframe(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label("Powered by ");
        ui.hyperlink_to("egui", "https://github.com/emilk/egui");
        ui.label(" and ");
        ui.hyperlink_to(
            "eframe",
            "https://github.com/emilk/egui/tree/master/crates/eframe",
        );
        ui.label(".");
    });
}
