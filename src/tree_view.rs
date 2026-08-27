use eframe::egui;
use egui::Ui;
use yarig::comp::comp_inst::{Comp, RifExt, RifInst, RifPageInst, RifRegInst, RifmuxInst};
use yarig::parser::remove_rif;

use crate::select::{SelectedItem, Selection};
use crate::widget::TreeNodeBuilder;
use crate::RifViewer;

#[derive(Clone)]
pub enum CompRef<'a> {
    External(&'a RifExt),
    Rifmux(&'a RifmuxInst),
    Rif(&'a RifInst),
    Page(&'a RifPageInst),
    Reg(&'a RifRegInst),
    None
}


#[derive(Debug)]
pub struct CompInfo {
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

impl RifViewer {
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
    pub fn search_updt_match(&mut self) {
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

    pub fn display_tree_comp(ui: &mut Ui, comp: &Comp, path: Vec<String>, selected: &mut Selection) {
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

    fn display_tree_comp_page(ui: &mut Ui, page: &RifPageInst, path: Vec<String>, selected: &mut Selection) {
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

    fn display_tree_comp_reg(ui: &mut Ui, reg: &RifRegInst, path: Vec<String>, selected: &mut Selection) {
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

    fn display_tree_leaf(ui: &mut Ui, label: String, current_path: &[String], selected: &mut Selection) {
        let is_selected = current_path == selected.path;
        let lbl = ui.selectable_label(is_selected, label);
        // lbl.scroll_to_me(None);
        if lbl.clicked() {
            selected.set_path(current_path.to_vec());
        } else  if is_selected && selected.updt==1 {
            lbl.scroll_to_me(None);
        }
    }

    pub fn get_comp_ref<'a>(comp_ref: CompRef<'a>, name: &str) -> (CompRef<'a>,u64) {
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

    // Helper function to recursively display components
    pub fn get_comp_info(path: &[String], comp: &Comp, addr: u64) -> Vec<CompInfo> {
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

    /// Find the compiled `RifInst` for `rif_type` anywhere in the compiled tree
    pub fn find_compiled_rif<'a>(comp: &'a Comp, rif_type: &str) -> Option<&'a RifInst> {
        match comp {
            Comp::Rif(rif) if rif.type_name == rif_type => Some(rif),
            Comp::Rifmux(rifmux) => rifmux.components.iter().find_map(|c| Self::find_compiled_rif(&c.inst, rif_type)),
            Comp::Rif(_) | Comp::External(_) => None,
        }
    }

    /// The compiled page containing register instance `inst_name` inside `rif`.
    pub fn find_compiled_reg_page<'a>(rif: &'a RifInst, inst_name: &str) -> Option<&'a RifPageInst> {
        rif.pages.iter().find(|p| p.regs.iter().any(|r| r.reg_name == inst_name))
    }
}
