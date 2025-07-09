use egui_material_icons;
use egui_commonmark::*;
use std::error::Error;
use pulldown_cmark::{Parser, Options};

/// We derive Deserialize/Serialize so we can persist app state on shutdown.
#[derive(serde::Deserialize, serde::Serialize)]
#[derive(Default)]
#[serde(default)] // if we add new fields, give them default values when deserializing old state
#[derive(Clone)]
struct Tab {
    // Example stuff:
    label: String,
    location: String,
    status: String,

    // currently loaded page of tab
    contents: String,

    // for history
    back: Vec<String>,
    forward: Vec<String>,
}

/// We derive Deserialize/Serialize so we can persist app state on shutdown.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)] // if we add new fields, give them default values when deserializing old state
pub struct TemplateApp {
    tabs: Vec<Tab>,
    current_tab: String,
    current_location: String,
    current_page: String,
    current_status: String,
}

impl Default for TemplateApp {
    fn default() -> Self {
        Self {
            tabs: vec![Tab {
                label: "Home".to_owned(),
                location: "https://example.com".to_owned(),
                status: "Loaded".to_owned(),
                contents: "".to_owned(),
                back: Vec::new().to_owned(),
                forward: Vec::new().to_owned(),
            }],
            current_tab: "Home".to_owned(),
            current_location: "https://example.com".to_owned(),
            current_status: "Loaded".to_owned(),


            // currently loaded page to display in main area
            current_page: "".to_owned(),

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


    pub fn back(&mut self) -> Result<(), Box<dyn Error>> {
        let mut tab_found = false;
        for tab in &mut self.tabs {
            if tab.label == self.current_tab {
                if let Some(last) = tab.back.pop() {
                    tab.forward.push(last.clone());
                    self.current_location = last.clone();
                    self.current_page = navigate(self.current_location.clone());
                    tab.contents = self.current_page.clone();
                    tab_found = true;
                    break;
                } else {
                    break;
                }
            }
        }
        if tab_found {
            Ok(())
        } else {
            Err("No back history".into())
        }
    }

    pub fn forward(&mut self) -> Result<(), Box<dyn Error>> {
        let mut tab_found = false;
        for tab in &mut self.tabs {
            if tab.label == self.current_tab {
                if let Some(next) = tab.forward.pop() {
                    tab.back.push(tab.location.clone());
                    self.current_location = next.clone();
                    self.current_page = navigate(self.current_location.clone());
                    tab.contents = self.current_page.clone();
                    tab_found = true;
                    break;
                } else {
                    break;
                }
            }
        }
        if tab_found {
            Ok(())
        } else {
            Err("No forward history".into())
        }
    }

    pub fn go(&mut self) {
        for tab in &mut self.tabs {
            if tab.label == self.current_tab {
                tab.back.push(tab.location.clone());
                tab.forward.clear(); // clear forward history
                tab.location = self.current_location.clone();
                tab.contents = navigate(self.current_location.clone());
                self.current_page = tab.contents.clone();
                tab.label = get_heading(tab.contents.clone());
                self.current_tab = tab.label.clone();
                return;
            }
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
                        powered_by_egui_and_eframe(ui);
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
                            self.current_status = e.to_string();
                        } else {
                            self.current_status = "Loaded".to_string();
                        }
                    });
                ui.add_space(1.0);
                ui.button(egui_material_icons::icons::ICON_ARROW_FORWARD)
                    .on_hover_text("Forward")
                    .clicked()
                    .then(|| {
                        if let Err(e) = self.forward() {
                            self.current_status = e.to_string();
                        } else {
                            self.current_status = "Loaded".to_string();
                        }
                    });
                ui.add_space(1.0);




                let button_width = 25.0;
                let text_edit_width = ui.available_width() - button_width;
                let response = ui.add_sized([text_edit_width.max(0.0), 20.0], egui::TextEdit::singleline(&mut self.current_location));
                if response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.current_status = "Loading...".to_string();
                    self.go();
                    self.current_status = "Loaded".to_string();
                }
                ui.add_space(1.0);
                ui.button(egui_material_icons::icons::ICON_KEYBOARD_DOUBLE_ARROW_RIGHT)
                    .on_hover_text("Go")
                    .clicked()
                    .then(|| {
                        self.current_status = "Loading...".to_string();
                        self.go();
                        self.current_status = "Loaded".to_string();
                    });
            });
        });

        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            //powered_by_egui_and_eframe(ui);
            let mut status_display: String = "Status: ".to_owned();
            let status: &str = self.current_status.as_str();
            status_display.push_str(status);
            ui.label(status_display);
        });

        egui::SidePanel::left("side_panel").show(ctx, |ui| {
            // The side panel is often a good place for a navigation menu:
            ui.separator();
            for tab in &self.tabs {
                if ui.button(&tab.label).clicked() {
                    self.current_location = tab.location.clone();
                    self.current_page = tab.contents.clone();
                    self.current_tab = tab.label.clone();
                }
            }

            if ui.button("+").clicked() {
                let new_tab = Tab {
                    label: "New Tab".to_owned(),
                    location: "https://example.com".to_owned(),
                    status: "Loaded".to_owned(),
                    contents: "".to_owned(),
                    back: Vec::new().to_owned(),
                    forward: Vec::new().to_owned(),
                };
                self.tabs.push(new_tab.clone());
                self.current_tab = new_tab.label.clone();
                self.current_location = new_tab.location.clone();
                self.current_page = new_tab.contents.clone();
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {

            let binding = self.current_page.clone();
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
                        self.current_location = link.clone();
                        self.current_status = "Loading...".to_string();
                        self.go();
                        self.current_status = "Loaded".to_string();
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
fn navigate(location: String) -> String {
    //self.current_status = "Loading...".to_string();
    println!("URL to navigate to: {}", location);
    //let resp = reqwest::blocking::get(location).unwrap().text();
    let resp = reqwest::blocking::get(location)
        .and_then(|r| r.text())
        .map_err(|e| e.to_string());
    //println!("{:#?}", resp);
    //self.current_status = "Loaded".to_string();
    return resp.unwrap_or_else(|_| "Failed to load page".to_string());
}

fn get_heading(contents: String) -> String {
    let mut heading = String::new();
    let mut in_heading = false;
    let parser = Parser::new_ext(&contents, Options::empty());
    for event in parser {
        match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Heading { .. }) => {
                in_heading = true;
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                if in_heading {
                    break;
                }
            }
            pulldown_cmark::Event::Text(text) => {
                if in_heading {
                    heading.push_str(&text);
                }
            }
            _ => {}
        }
    }
    heading
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
