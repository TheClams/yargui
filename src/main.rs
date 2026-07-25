mod search;
mod widget;
mod select;

const VERSION: &str = env!("CARGO_PKG_VERSION");

use std::{collections::HashMap, path::{Path, PathBuf}};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use egui_file_dialog::{FileDialog, Filter};
use egui::{RichText, ViewportBuilder};
use search::SearchMatches;
use select::{SelectedItem, Selection};
use widget::TreeNodeBuilder;
use yarig::{
    comp::comp_inst::{Comp, RifExt, RifFieldInst, RifInst, RifPageInst, RifRegInst, RifmuxInst},
    parser::{ParserCfg, RifGenSrc, RsvdKeywordSel, parser_expr::ParamValues, remove_rif},
    rifgen::SuffixInfo
};

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default().with_inner_size((1000.0,700.0)),
        ..Default::default()
    };

    let args: Vec<String> = std::env::args().collect();
    let mut rif_viewer = RifViewer::default();

    if let Some(path) = args.get(1) {
        rif_viewer.file_path = path.into();
        rif_viewer.open_file();
    }

    eframe::run_native(
         &format!("RIF Viewer v{VERSION}"),
        options,
        Box::new(|_| Ok(Box::new(rif_viewer))),
    )
}

struct RifViewer {
    file_dialog : FileDialog,
    file_path: PathBuf,
    //-----------------
    // Data to display
    rif_src: Option<RifGenSrc>,
    rif_comp: Option<Comp>,
    last_err: Option<String>,
    /// Selection
    selected : Selection,
    // Search feature
    search_query: String,
    search_results: SearchMatches,
    current_result: usize,
    search_by_name: bool,
    search_by_desc: bool,
}

impl Default for RifViewer {
    fn default() -> Self {
        let file_dialog = FileDialog::new()
            .add_file_filter("RIFs", Filter::new(|p: &Path| p.extension().unwrap_or_default() == "rif"))
            .default_file_filter("RIFs")
            .show_new_folder_button(false);
        Self {
            file_dialog,
            file_path : PathBuf::from("."),
            rif_src: None,
            rif_comp: None,
            last_err: None,
            selected: Selection::default(),
            search_query: String::new(),
            search_results: SearchMatches::new(),
            current_result: 0,
            search_by_name: true,
            search_by_desc: false,
        }
    }
}

#[derive(Clone)]
enum CompRef<'a> {
    External(&'a RifExt),
    Rifmux(&'a RifmuxInst),
    Rif(&'a RifInst),
    Page(&'a RifPageInst),
    Reg(&'a RifRegInst),
    None
}


#[derive(Debug)]
struct CompInfo {
    pub name: String,
    pub path: Vec<String>,
    pub addr: u64,
    pub desc: String
}

impl CompInfo {
    pub fn new(name: String, path: &[String], addr: u64, desc: String, has_path: bool) -> Self {
        let mut path = path.to_vec();
        if has_path {
            path.push(name.to_owned());
        }
        CompInfo { name: remove_rif(&name).to_owned(), path, addr, desc }
    }
}


impl eframe::App for RifViewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Re-open last file When hitting F5
        if ui.input(|i|  i.key_pressed(egui::Key::F5)) {
            self.open_file();
        }
        // Bottom Panel: open/search
        self.show_bottom_panel(ui);
        // Tree view
        egui::Panel::left("tree_view").min_size(220.0).resizable(true).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                if let Some(ref comp) = self.rif_comp {
                    Self::display_tree_comp(ui, comp, Vec::new(), &mut self.selected);
                }
                if self.selected.updt > 0 {
                    self.selected.updt -= 1;
                }
            });
        });
        // Content
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                if let Some(e) = &self.last_err {
                    ui.heading("Compilation error !");
                    ui.label(e);
                    return;
                }
                //
                if let Some(comp) = &self.rif_comp {
                    let mut comp_ref = match comp {
                        Comp::Rifmux(rifmux_inst) => CompRef::Rifmux(rifmux_inst),
                        Comp::Rif(rif_inst) => CompRef::Rif(rif_inst),
                        Comp::External(rif_ext) => CompRef::External(rif_ext),
                    };
                    let mut addr = 0;
                    let mut offset;
                    let mut rif_ref : Option<CompRef> = None;
                    for name in self.selected.path.iter().skip(1) {
                        (comp_ref, offset) = Self::get_comp_ref(comp_ref, name);
                        if matches!(comp_ref, CompRef::Rif(_)) {
                            rif_ref = Some(comp_ref.clone());
                        }
                        addr += offset;
                    }
                    // Clickable title
                    ui.horizontal(|ui| {
                        ui.style_mut().override_text_style = Some(egui::TextStyle::Heading);
                        ui.style_mut().spacing.item_spacing.x = 2.0;
                        let mut path_elements = self.selected.path.iter().enumerate().peekable();
                        let mut lvl = 0;
                        while let Some((i,part)) = path_elements.next() {
                            let part = remove_rif(part);
                            if path_elements.peek().is_some() {
                                if ui.selectable_label(false, RichText::new(part).strong()).clicked() {
                                    lvl = i+1;
                                }
                                ui.label(RichText::new(".").strong());
                            } else {
                                ui.label(RichText::new(part).strong());
                            }
                        }
                        if lvl!=0 {
                            self.selected.path.truncate(lvl);
                            self.selected.updt = 1;
                        }
                        if !self.selected.path.is_empty() {
                            ui.label(RichText::new(format!(" @ 0x{addr:04x}")).strong());
                        }
                    });
                    // Reset the selected item when no longer relevant
                    match self.selected.item {
                        SelectedItem::Reg(_) => if !matches!(comp_ref,CompRef::Rif(_)|CompRef::Page(_)) {self.selected.item = SelectedItem::None;}
                        SelectedItem::Field(_) => if !matches!(comp_ref,CompRef::Reg(_)) {self.selected.item = SelectedItem::None;}
                        SelectedItem::None => {},
                    }

                    // Create table summary (RIFs or register contents)
                    match comp_ref {
                        CompRef::External(ext)  => Self::display_content_ext(ui, ext),
                        CompRef::Rifmux(rifmux) => Self::display_content_rifmux(ui, &mut self.selected, rifmux),
                        CompRef::Rif(rif)       => Self::display_content_rif(ui, &mut self.selected, rif),
                        CompRef::Page(page)     => Self::display_content_page(ui, &mut self.selected, page),
                        CompRef::Reg(reg)       => {
                            if let Some(name) = Self::display_content_reg(ui, &self.selected, reg) {
                                self.selected.item = SelectedItem::Field(name);
                            }
                        }
                        CompRef::None => {},
                    }
                    // Add field description panel when a field is selected
                    if let (SelectedItem::Field(field_name), CompRef::Reg(reg)) = (&self.selected.item, &comp_ref) {
                        let basename = field_name.split('[').next().unwrap_or(field_name);
                        if let Some(field) = reg.fields.iter().find(|f| f.name == basename) {
                            let mut enum_def = None;
                            let mut enum_rst  = None;
                            if let (Some(enum_type),Some(CompRef::Rif(rif))) = (field.enum_kind.name(), rif_ref) {
                                enum_def = rif.enum_defs.iter().find(|e| e.name==*enum_type);
                                if let Some(d) = enum_def {
                                    enum_rst = d.values.iter().find(|e| e.value==field.reset.to_u128(field.width() as u8) as u8).map(|e| e.name.clone());
                                }
                            }
                            let rst_str = enum_rst.unwrap_or(get_field_rst_str(field));
                            ui.separator();
                            ui.add_space(10.0);
                            ui.heading(format!("{} = {rst_str}",field.name));
                            ui.label(field.description.get(false));
                            if let Some(d) = enum_def {
                                let desc = if d.description != field.description.get_short(false) {&d.description} else {""};
                                ui.add_space(5.0);
                                ui.label(format!("Enum {} : {desc}", d.name));
                                for enum_entry in &d.values {
                                    ui.label(format!("  - {} ({}) : {}", enum_entry.name, enum_entry.value, enum_entry.description.get_short(false)));
                                }
                            }
                       }
                    }
                }
            });
        });
    }
}

impl RifViewer {

    fn open_file(&mut self) {
        self.selected.path.clear();
        let rsvd_sel = RsvdKeywordSel { sv: false, vhdl: false, error: false};
        let parser_cfg = ParserCfg::new(rsvd_sel, false);
        match RifGenSrc::from_file(&self.file_path, &[], &parser_cfg) {
            Ok(src) => {
                // TODO: parameters/suffixes could come from a json file ?
                let params = ParamValues::new();
                let suffixes : HashMap<String, SuffixInfo> = HashMap::new();
                match Comp::compile(&src, &suffixes, &params) {
                    Ok(o) => {
                        self.selected.path.push(o.get_name().to_owned());
                        self.rif_comp = Some(o);
                        self.last_err = None;
                    },
                    Err(e) => {
                        self.rif_comp = None;
                        self.last_err = Some(e);
                    },
                }
                // Save source (in the future the GUI should be able to edit the source)
                self.rif_src = Some(src);
            }
            Err(e) => {
                self.rif_src = None;
                self.rif_comp = None;
                self.last_err = Some(e.to_string());
            },
        }
    }

    fn show_bottom_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("bottom_panel").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Load File button
                if ui.button("Load File").clicked() || ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::O)) {
                    // let path = self.file_path.clone();
                    // self.file_dialog.show_left_panel(show_left_panel)
                    self.file_dialog.pick_file();
                }

                // Handle picked file
                self.file_dialog.update(ui.ctx());
                if let Some(path) = self.file_dialog.take_picked() {
                    self.file_dialog.config_mut().initial_directory = path.clone();
                    self.file_path = path;
                    self.open_file();
                }

                ui.separator();
                ui.label("Search:");

                // Search bar with focus handling and tooltip
                let search_response = {
                    let text_edit = egui::TextEdit::singleline(&mut self.search_query)
                        .id("search_bar".into())
                        .hint_text("Press Ctrl+F");
                    ui.add(text_edit)
                };
                // Handle Ctrl+F to focus the search bar
                if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
                    search_response.request_focus();
                    // Select all text in the search box
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), search_response.id) {
                        state.cursor.set_char_range(
                            Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(self.search_query.len()))));
                        state.store(ui.ctx(), search_response.id);
                    }
                }

                let do_search = search_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    || ui.button(RichText::new("🔍").size(16.0)).clicked();

                if do_search {
                    self.search_results.clear();
                    self.current_result = 0;

                    if !self.search_query.is_empty() && (self.search_by_name || self.search_by_desc) {
                        if let Some(ref comp) = self.rif_comp {
                            self.search_results = SearchMatches::search(
                                comp,
                                Vec::new(),
                                &self.search_query,
                                self.search_by_name,
                                self.search_by_desc
                            );
                            if !self.search_results.is_empty() {
                                self.search_updt_match();
                            }
                        }
                    }

                }

                // Search checkboxes
                ui.checkbox(&mut self.search_by_name, "Names")
                    .on_hover_text("Search in register and field names");
                ui.checkbox(&mut self.search_by_desc, "Descriptions")
                    .on_hover_text("Search in register and field descriptions");

                // Display result
                if !self.search_results.is_empty() {
                    ui.label(format!("{}/{}", self.current_result + 1, self.search_results.len()));

                    if ui.button("⬆").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::F3) && i.modifiers.shift) {
                        self.current_result = self.current_result.wrapping_sub(1) % self.search_results.len();
                        self.search_updt_match();
                    }

                    if ui.button("⬇").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::F3)
                            || (!do_search && i.key_pressed(egui::Key::Enter))) {
                        self.current_result = (self.current_result + 1) % self.search_results.len();
                        self.search_updt_match();
                    }
                }

            });
        });
    }

    // // Add this helper function to expand tree nodes given a path
    fn ensure_path_expanded(ui: &egui::Ui, path: &[String]) -> bool {
        let id_str = path.join("");
        let id = ui.make_persistent_id(format!("tree_node_{id_str}"));
        // println!("Expand {:?} == {id_str} -> {id:?}", path);
        if let Some(mut state) = egui::collapsing_header::CollapsingState::load(ui.ctx(), id) {
            state.set_open(true);
            state.store(ui.ctx());
            true
        } else {
            false
        }
    }

    // Modify the search_updt_match function
    fn search_updt_match(&mut self) {
        self.selected.item = SelectedItem::None;
        if let Some(search_match) = self.search_results.get(self.current_result) {
            if let Some(field_name) = &search_match.field_name {
                self.selected.path = search_match.path.clone();
                self.selected.item = SelectedItem::Field(field_name.clone());
            } else {
                self.selected.path = search_match.path.clone();
                self.selected.item = SelectedItem::Reg(self.selected.path.pop().unwrap().clone());
            }
            // Tree expansion may take multiple frame: max is the path level
            self.selected.updt = self.selected.path.len();
        }
    }

    fn display_tree_comp(ui: &mut egui::Ui, comp: &Comp, path: Vec<String>, selected: &mut Selection) {
        let mut current_path = path.clone();
        current_path.push(comp.get_name().to_owned());
        if selected.matching(&current_path) {
            Self::ensure_path_expanded(ui, &current_path);
        }
        let is_selected = current_path==selected.path;
        let scroll = selected.updt==1 && is_selected;
        let id = current_path.join("");
        let mut new_sel_path = None;
        match comp {
            Comp::Rifmux(rifmux_inst) => {
                let label = remove_rif(&rifmux_inst.inst_name);
                TreeNodeBuilder::new(&id, label)
                    .selected(is_selected)
                    .scroll(scroll)
                    .default_open(current_path.len()==1)
                    .build(ui,
                        || {new_sel_path = Some(current_path.clone());},
                        |ui| {
                            for subcomp in rifmux_inst.components.iter() {
                                Self::display_tree_comp(ui, &subcomp.inst, current_path.clone(), selected);
                            }
                        }
                    );
                if let Some(sel_path) = new_sel_path {
                    selected.set_path(sel_path);
                }
            }
            Comp::Rif(rif_inst) => {
                let label = remove_rif(&rif_inst.inst_name);
                TreeNodeBuilder::new(&id, label)
                    .selected(is_selected)
                    .scroll(scroll)
                    .build(ui,
                        || {new_sel_path = Some(current_path.clone());},
                        |ui| {
                            if rif_inst.pages.len()==1 {
                                for reg in rif_inst.pages[0].regs.iter() {
                                    Self::display_tree_comp_reg(&mut *ui, reg, current_path.clone(), selected);
                                }
                            } else {
                                for page in rif_inst.pages.iter() {
                                    Self::display_tree_comp_page(ui, page, current_path.clone(), selected);
                                }
                            }
                        }
                    );
                //
                if let Some(sel_path) = new_sel_path {
                    selected.set_path(sel_path);
                }
            }
            Comp::External(ext) => {
                Self::display_tree_leaf(ui, ext.inst_name.to_owned(), &current_path, selected);
            }
        }
    }

    fn display_tree_comp_page(ui: &mut egui::Ui, page: &RifPageInst, path: Vec<String>, selected: &mut Selection) {
        let label = format!("📃 {}", &page.name);
        let mut current_path = path.clone();
        current_path.push(page.name.to_owned());
        if selected.matching(&current_path) {
            Self::ensure_path_expanded(ui, &current_path);
        }
        let is_selected = current_path==selected.path;
        let id = current_path.join("");
        let mut new_sel_path = None;
        TreeNodeBuilder::new(&id, &label)
            .selected(is_selected)
            .scroll(selected.updt==1 && is_selected)
            .build(ui,
                || {new_sel_path = Some(current_path.clone());},
                |ui| {
                    for reg in page.regs.iter() {
                        Self::display_tree_comp_reg(ui, reg, current_path.clone(), selected);
                    }
                }
            );
        if let Some(sel_path) = new_sel_path {
            selected.set_path(sel_path);
        }
    }

    fn display_tree_comp_reg(ui: &mut egui::Ui, reg: &RifRegInst, path: Vec<String>, selected: &mut Selection) {
        if reg.array.idx() > 0 {
            return;
        }
        let mut label = format!("📄 {}", &reg.reg_name);
        if reg.array.dim() > 0 {
            label.push_str(&format!("[{}]", reg.array.dim()));
        }
        let mut current_path = path.clone();
        current_path.push(reg.reg_name.to_owned());
        Self::display_tree_leaf(ui, label, &current_path, selected);
        //
    }

    fn display_tree_leaf(ui: &mut egui::Ui, label: String, current_path: &[String], selected: &mut Selection) {
        let is_selected = current_path == selected.path;
        let lbl = ui.selectable_label(is_selected, label);
        // lbl.scroll_to_me(None);
        if lbl.clicked() {
            selected.set_path(current_path.to_vec());
        } else  if is_selected && selected.updt==1 {
            lbl.scroll_to_me(None);
        }
    }

    fn get_comp_ref<'a>(comp_ref: CompRef<'a>, name: &str) -> (CompRef<'a>,u64) {
        let mut c = CompRef::None;
        let mut addr = 0;
        match comp_ref {
            CompRef::Rifmux(rifmux) => {
                if let Some(comp) = rifmux.components.iter().find(|c| c.get_name()==name) {
                    addr = comp.addr;
                    match &comp.inst {
                        Comp::Rifmux(rifmux_inst) => c = CompRef::Rifmux(rifmux_inst),
                        Comp::Rif(rif_inst)       => c = CompRef::Rif(rif_inst),
                        Comp::External(rif_ext)   => c = CompRef::External(rif_ext),
                    }
                }
            },
            CompRef::Rif(rif) => {
                if rif.pages.len()>1 {
                    if let Some(page) = rif.pages.iter().find(|p| p.name==*name) {
                        addr = page.addr;
                        c = CompRef::Page(page);
                    }
                }
                else if let Some(reg) = rif.pages[0].regs.iter().find(|r| r.reg_name==*name) {
                    addr = reg.addr;
                    c = CompRef::Reg(reg);
                }
            },
            CompRef::Page(page) =>
                if let Some(reg) = page.regs.iter().find(|r| r.reg_name==*name) {
                    addr = reg.addr;
                    c = CompRef::Reg(reg);
                },
            CompRef::Reg(reg) => c = CompRef::Reg(reg),
            _ => {}
        }
        (c, addr)
    }

    fn display_content_ext(ui: &mut egui::Ui, ext: &RifExt) {
        ui.label(ext.description.get(false));
    }

    fn display_content_rifmux(ui: &mut egui::Ui, selected: &mut Selection, rifmux: &RifmuxInst) {
        let comps_info : Vec<CompInfo> = rifmux.components.iter().flat_map(|c| Self::get_comp_info(&selected.path, &c.inst, c.addr)).collect();
        ui.label(rifmux.description.get(false));
        ui.separator();
        let table = TableBuilder::new(ui).striped(true)
            .column(Column::auto().at_least(70.0))   // Address column
            .column(Column::auto().at_least(150.0))  // Name column
            .column(Column::remainder());            // Description column
        table
            .header(20.0, |mut header| {
                header.col(|ui| { ui.strong("Address"); });
                header.col(|ui| { ui.strong("Name"); });
                header.col(|ui| { ui.strong("Description"); });
            })
            .body(|mut body| {
                for info in comps_info {
                    body.row(18.0, |mut row| {
                        row.col(|ui| { ui.label(format!("0x{:04x}", info.addr)); });
                        row.col(|ui| {
                            if info.path.is_empty() {
                                ui.label(info.name);
                            } else if ui.selectable_label(false, info.name).clicked() {
                                selected.set_path(info.path);
                            }
                        });
                        row.col(|ui| { ui.label(info.desc); });
                    });
                }
            });
    }

    // Helper function to recursively display components
    fn get_comp_info(path: &[String], comp: &Comp, addr: u64) -> Vec<CompInfo> {
        let mut infos = Vec::new();
        match comp {
            Comp::Rifmux(rifmux) => {
                let mut path_rifmux = path.to_vec();
                path_rifmux.push(rifmux.inst_name.to_owned());
                for subcomp in &rifmux.components {
                    infos.extend(Self::get_comp_info(&path_rifmux, &subcomp.inst, addr + subcomp.addr));
                }
            }
            Comp::Rif(rif) => {
                infos.push(CompInfo::new(rif.inst_name.to_owned(), path, addr, rif.description.get_short(false).to_owned(), true));
            },
            Comp::External(ext) => {
                infos.push(CompInfo::new(ext.inst_name.to_owned(), path, addr, ext.description.get_short(false).to_owned(), false));
            }
        }
        infos
    }

    fn display_content_rif(ui: &mut egui::Ui, selected: &mut Selection, rif: &RifInst) {
        ui.label(rif.description.get(false));
        for (i,page) in rif.pages.iter().enumerate() {
            ui.push_id(i, |ui| {
                let is_multipage = rif.pages.len() > 1;
                if  is_multipage {
                    ui.heading(page.name.to_owned());
                    ui.label(page.description.get(false));
                }
                Self::display_reg_summary(ui, selected, page, is_multipage);
            });
        }
    }

    fn display_content_page(ui: &mut egui::Ui, selected: &mut Selection, page: &RifPageInst) {
        ui.label(page.description.get(false));
        Self::display_reg_summary(ui, selected, page, false);
    }

    fn display_reg_summary(ui: &mut egui::Ui, selected: &mut Selection, page: &RifPageInst, is_multipage: bool) {
        ui.separator();

        // Create a table to display register summary
        let table = TableBuilder::new(ui).striped(true)
            .column(Column::auto().at_least(70.0))  // Address column
            .column(Column::auto().at_least(150.0)) // Name column
            .column(Column::auto().at_least(100.0)) // Reset value column
            .column(Column::remainder());           // Description column

        table
            .header(20.0, |mut header| {
                header.col(|ui| { ui.strong("Address"); });
                header.col(|ui| { ui.strong("Name"); });
                header.col(|ui| { ui.strong("Reset"); });
                header.col(|ui| { ui.strong("Description"); });
            })
            .body(|mut body| {
                for reg in page.regs.iter() {
                    body.row(18.0, |mut row| {
                        // Address column - display in hexadecimal
                        row.col(|ui| { ui.label(format!("0x{:04x}", reg.addr)); });

                        // Name column
                        let is_selected = Some(reg.reg_name.as_str()) == selected.item.name() && selected.item.is_reg();
                        let col_resp = row.col(|ui| {
                            if ui.selectable_label(is_selected, reg.name_i()).clicked() {
                                if is_multipage && selected.path.last()!=Some(&page.name) {
                                    selected.path.push(page.name.to_owned());
                                }
                                selected.path.push(reg.reg_name.to_owned());
                                selected.updt = 1;
                            }
                        }).1;
                        if is_selected {col_resp.scroll_to_me(None);}

                        // Reset value column - display in hexadecimal for consistency
                        row.col(|ui| { ui.label(format!("0x{:08x}", reg.reset)); });

                        // Description column
                        row.col(|ui| { ui.label(reg.get_desc_short(false)); });
                    });
                }
            });
    }

    fn display_content_reg(ui: &mut egui::Ui, selected: &Selection, reg: &RifRegInst) -> Option<String> {
        let mut selected_field = None;
        ui.label(reg.description.get(false));
        ui.separator();

        // TODO: estimate max length of name
        let width_name = reg.fields.iter().map(|f| f.name().len()).max().unwrap_or(0) as f32 * 7.9;
        let width_reset = ((reg.fields.iter().map(|f| f.width()).max().unwrap_or(0) >> 2) + 2) as f32 * 7.9;
        let table = TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(18.0*16.0+20.0) // Limit to 16 rows (include the header)
            .column(Column::auto().at_least(35.0))
            .column(Column::auto().at_least(width_name))
            .column(Column::auto().at_least(45.0))
            .column(Column::auto().at_least(width_reset))
            .column(Column::remainder());
        table
            .header(20.0, |mut header| {
                header.col(|ui| {ui.strong("Bits"  );});
                header.col(|ui| {ui.strong("Name"  );});
                header.col(|ui| {ui.strong("Access");});
                header.col(|ui| {ui.strong("Reset" );});
                header.col(|ui| {ui.strong("Desc." );});
            })
            .body(|mut body| {
                // Auto select first field if only one present
                if reg.fields.len() == 1 {
                    selected_field = reg.fields.first().map(|f| f.name());
                }
                for f in reg.fields.iter() {
                    body.row(18.0, |mut row| {
                        let pos = if f.width()==1 {format!("{}",f.lsb)} else {format!("{}:{}",f.msb(), f.lsb)};
                        row.col(|ui| {ui.label(pos);});
                        row.col(|ui| {
                            let n = f.name();
                            let selected = Some(n.as_str()) == selected.item.name() && selected.item.is_field();
                            if ui.selectable_label(selected, n).clicked() {
                                selected_field = Some(f.name())
                            }
                        });
                        row.col(|ui| {ui.label(f.sw_kind.access_str());});
                        row.col(|ui| {ui.label(get_field_rst_str(f));});
                        row.col(|ui| {ui.label(f.description.get_short(false));});
                    });
                }
            });
        selected_field
    }

}

fn get_field_rst_str(field: &RifFieldInst) -> String {
    let width = field.width() as u8;
    let val = field.reset.to_u128(width);
    let w = (width >> 2) as usize;
    if width > 12 {
        format!("0x{val:0w$X}")
    } else {
        format!("{val}")
    }
}