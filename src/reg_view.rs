use eframe::egui;
use egui::RichText;
use egui_extras::{Column, TableBuilder};
use egui_file_dialog::FileDialog;
use yarig::comp::comp_inst::{RifExt, RifFieldInst, RifInst, RifPageInst, RifRegInst, RifmuxInst};
use yarig::hdl::ModuleInfo;
use yarig::parser::parser_expr::{parse_expr, ParamValues};
use yarig::parser::{RifGenSrc, get_rif};
use yarig::rifgen::{Access, AddressKind, DataWidth, ExternalKind, Field, FieldPos, Interface, InterruptClr, InterruptRegKind, InterruptTrigger, RegDef, RegInst, Rif, RifPage, Visibility, Width};

use crate::intr_desc_editor::{FieldIntrDescEditor, RegIntrDescEditor};
use crate::reg_editor::{
    AddRegKind, PendingRegAddrConflict, PendingRegArrayChange, RegAddEditor, RegArrayChangeKind,
    RegDefVals, RegEditor, RegOverrideEditor,
};
use crate::rif_editor::{ClockingRow, GenericRow, ParamRow, RifClockingEditor, RifEditor, RifParamsEditor};
use crate::select::Selection;
use crate::tree_view::CompInfo;
use crate::apply_pending::EditAction;
use crate::{EditTarget, PendingDeleteReg, RifViewer, adv_label, find_reg_page, find_regdef, get_field_rst_str, intr_clear_label, intr_kind_label, intr_trigger_label, swap_positions};

/// Result of interacting with the register-content table.
pub enum RegClick {
    Select(String),
    AddField,
    /// Swap two adjacent fields' bit positions, triggered from the table's compact reorder column.
    MoveField { moves: Vec<(String, FieldPos)> },
}

/// Result of interacting with a page's register-summary table.
pub enum PageClick {
    /// The page-level "convert to manual addressing" button was clicked.
    ConvertToManual { page_name: String },
    /// A compact reorder icon was clicked: swap these two instances' addresses.
    SwapRegisters { a_inst_name: String, b_inst_name: String },
    /// The ghost "➕ add register" row was clicked: open the modal.
    OpenAddRegister { page_name: String },
    /// A compact delete icon was clicked. The caller decides whether this needs confirmation
    /// (`display_reg_summary` doesn't have enough context — see `PendingDeleteReg`).
    DeleteRegister { page_name: String, reg_type: String, inst_name: Option<String> },
}

impl RifViewer {
    pub fn display_content_ext(ui: &mut egui::Ui, ext: &RifExt) {
        ui.label(ext.description.get(false));
    }

    pub fn display_content_rifmux(ui: &mut egui::Ui, selected: &mut Selection, rifmux: &RifmuxInst) {
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

    /// Render the Rif-level property editor (name, address/data width, description)
    #[allow(clippy::too_many_arguments)]
    pub fn show_rif_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<RifEditor>,
        clocking_editor: &mut Option<RifClockingEditor>,
        params_editor: &mut Option<RifParamsEditor>,
        intf_file_dialog: &mut FileDialog,
        edit_err: &Option<String>,
        rif_type: &str,
        rif: &Rif,
    ) -> Option<EditAction> {
        if editor.as_ref().map(|e| e.rif_type.as_str()) != Some(rif_type) {
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
                && let Some(action) = old.build_action()
            {
                return Some(action);
            }
            *editor = Some(RifEditor::from_rif(rif_type, rif));
        }
        let ed = editor.as_mut().expect("editor just set");

        let mut auto_apply = false;
        // Name shares its line with address/data width — half the field width Name would get
        // on its own row, to leave room for the other two.
        ui.horizontal(|ui| {
            ui.label("Name");
            if ui.add(egui::TextEdit::singleline(&mut ed.name).desired_width(160.0)).lost_focus() { auto_apply = true; }
            ui.add_space(12.0);
            ui.label("Address width");
            if ui.add(egui::TextEdit::singleline(&mut ed.addr_width).desired_width(50.0))
                .on_hover_text("Address bus width, in bits.")
                .lost_focus() { auto_apply = true; }
            ui.add_space(12.0);
            ui.label("Data width");
            egui::ComboBox::from_id_salt("rif_data_width").selected_text(format!("{}", ed.data_width)).show_ui(ui, |ui| {
                for w in [DataWidth::W8(8), DataWidth::W16(16), DataWidth::W32(32), DataWidth::W64(64)] {
                    if ui.selectable_value(&mut ed.data_width, w, format!("{w}")).changed() { auto_apply = true; }
                }
            });
        });
        ui.add_space(6.0);

        // Software interface: Default/APB/UAUX pick themselves; Custom needs a `.sv` file
        ui.horizontal(|ui| {
            ui.label("Interface");
            let label = match &ed.interface {
                Interface::Default => "Default".to_owned(),
                Interface::Apb => "APB".to_owned(),
                Interface::Uaux => "UAUX".to_owned(),
                Interface::Custom(name, _) if name.is_empty() => "Custom".to_owned(),
                Interface::Custom(name, _) => format!("Custom ({name})"),
            };
            egui::ComboBox::from_id_salt("rif_interface").selected_text(label).show_ui(ui, |ui| {
                if ui.selectable_label(matches!(ed.interface, Interface::Default), "Default").clicked()
                    && !matches!(ed.interface, Interface::Default)
                {
                    ed.interface = Interface::Default;
                    auto_apply = true;
                }
                if ui.selectable_label(matches!(ed.interface, Interface::Apb), "APB").clicked()
                    && !matches!(ed.interface, Interface::Apb)
                {
                    ed.interface = Interface::Apb;
                    auto_apply = true;
                }
                if ui.selectable_label(matches!(ed.interface, Interface::Uaux), "UAUX").clicked()
                    && !matches!(ed.interface, Interface::Uaux)
                {
                    ed.interface = Interface::Uaux;
                    auto_apply = true;
                }
                if ui.selectable_label(matches!(ed.interface, Interface::Custom(..)), "Custom").clicked()
                    && !matches!(ed.interface, Interface::Custom(..))
                {
                    ed.interface = Interface::Custom(String::new(), String::new());
                    intf_file_dialog.pick_file();
                }
            });
            if matches!(ed.interface, Interface::Custom(..)) {
                ui.add_space(8.0);
                if ui.button("Browse...").clicked() {
                    intf_file_dialog.pick_file();
                }
                if let Interface::Custom(name, path) = &ed.interface {
                    let shown = if path.is_empty() { "<no file selected>".to_owned() } else { format!("{name}  ({path})") };
                    ui.label(RichText::new(shown).weak());
                }
            }
        });
        intf_file_dialog.update(ui.ctx());
        if let Some(path) = intf_file_dialog.take_picked() {
            match ModuleInfo::from_file(path.clone()) {
                Ok(info) => {
                    let path_str = path.to_string_lossy().replace('\\', "/");
                    ed.interface = Interface::Custom(info.name, path_str);
                    ed.parse_err = None;
                    auto_apply = true;
                }
                Err(e) => ed.parse_err = Some(format!("{}: {e}", path.display())),
            }
        }
        ui.add_space(6.0);

        ui.label("Description");
        if ui.add(egui::TextEdit::multiline(&mut ed.desc).desired_width(320.0).desired_rows(3))
            .on_hover_text("First line is the short summary; further lines become a `description:` block.")
            .lost_focus() { auto_apply = true; }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Clocking...").clicked() {
                *clocking_editor = Some(RifClockingEditor::open(rif_type, rif));
            }
            if ui.button("Parameters...").clicked() {
                *params_editor = Some(RifParamsEditor::open(rif_type, rif));
            }
        });

        ui.add_space(6.0);
        if let Some(err) = &ed.parse_err {
            ui.label(RichText::new(err).color(egui::Color32::RED));
        } else if let Some(err) = edit_err {
            ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
        }

        if !auto_apply {
            return None;
        }
        ed.build_action()
    }

    /// Render the clocking-table modal
    pub fn show_rif_clocking_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<RifClockingEditor>,
        edit_err: &Option<String>,
    ) -> Option<EditAction> {
        let Some(ed) = editor else { return None; };
        let mut action = None;
        let mut cancel = false;
        egui::Window::new("Edit clocking scheme")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                let mut remove_idx = None;
                TableBuilder::new(ui)
                    .striped(true)
                    .column(Column::auto().at_least(50.0))
                    .column(Column::auto().at_least(90.0))
                    .column(Column::auto().at_least(90.0))
                    .column(Column::auto().at_least(70.0))
                    .column(Column::auto().at_least(70.0))
                    .column(Column::auto().at_least(80.0))
                    .column(Column::auto().at_least(80.0))
                    .column(Column::auto().at_least(20.0))
                    .header(20.0, |mut header| {
                        header.col(|ui| { ui.strong("Type"); });
                        header.col(|ui| { ui.strong("Clock"); });
                        header.col(|ui| { ui.strong("Reset"); });
                        header.col(|ui| { ui.strong("Polarity"); });
                        header.col(|ui| { ui.strong("Sync"); });
                        header.col(|ui| { ui.strong("Clk En"); });
                        header.col(|ui| { ui.strong("Clear"); });
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for (i, row) in ed.rows.iter_mut().enumerate() {
                            body.row(18.0, |mut r| {
                                r.col(|ui| {
                                    egui::ComboBox::from_id_salt(format!("rif_clk_hw_{i}"))
                                        .selected_text(if row.is_hw { "HW" } else { "SW" })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut row.is_hw, false, "SW");
                                            ui.selectable_value(&mut row.is_hw, true, "HW");
                                        });
                                });
                                r.col(|ui| { ui.text_edit_singleline(&mut row.clk); });
                                r.col(|ui| { ui.text_edit_singleline(&mut row.rst_name); });
                                r.col(|ui| {
                                    egui::ComboBox::from_id_salt(format!("rif_clk_pol_{i}"))
                                        .selected_text(if row.active_high { "High" } else { "Low" })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut row.active_high, false, "Low");
                                            ui.selectable_value(&mut row.active_high, true, "High");
                                        });
                                });
                                r.col(|ui| {
                                    egui::ComboBox::from_id_salt(format!("rif_clk_sync_{i}"))
                                        .selected_text(if row.sync { "Sync" } else { "Async" })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut row.sync, false, "Async");
                                            ui.selectable_value(&mut row.sync, true, "Sync");
                                        });
                                });
                                r.col(|ui| { ui.text_edit_singleline(&mut row.clk_en); });
                                r.col(|ui| { ui.text_edit_singleline(&mut row.clear); });
                                r.col(|ui| {
                                    if ui.small_button("✖").clicked() {
                                        remove_idx = Some(i);
                                    }
                                });
                            });
                        }
                        // Discoverable "add clock" affordance, mirroring the enum table's ghost row.
                        body.row(18.0, |mut r| {
                            r.col(|ui| {
                                let lbl = RichText::new("➕ add clock").weak();
                                if ui.selectable_label(false, lbl).clicked() {
                                    ed.rows.push(ClockingRow::new_default(false));
                                }
                            });
                            r.col(|_| {});
                            r.col(|_| {});
                            r.col(|_| {});
                            r.col(|_| {});
                            r.col(|_| {});
                            r.col(|_| {});
                            r.col(|_| {});
                        });
                    });
                if let Some(i) = remove_idx {
                    ed.rows.remove(i);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match ed.build_values() {
                            Ok((sw, hw)) => {
                                ed.parse_err = None;
                                action = Some(EditAction::UpdateRifClocking { rif_type: ed.rif_type.clone(), sw, hw });
                            }
                            Err(e) => ed.parse_err = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if let Some(err) = &ed.parse_err {
                        ui.label(RichText::new(err).color(egui::Color32::RED));
                    } else if let Some(err) = edit_err {
                        ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
                    }
                });
            });
        if cancel {
            *editor = None;
        }
        action
    }

    /// Render the parameters/generics modal
    pub fn show_rif_params_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<RifParamsEditor>,
        edit_err: &Option<String>,
    ) -> Option<EditAction> {
        let Some(ed) = editor else { return None; };
        let mut action = None;
        let mut cancel = false;
        egui::Window::new("Edit parameters / generics")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                let mut remove_param_idx = None;
                let mut remove_generic_idx = None;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.strong("Parameters");
                        TableBuilder::new(ui)
                            .striped(true)
                            .column(Column::auto().at_least(110.0))
                            .column(Column::auto().at_least(90.0))
                            .column(Column::auto().at_least(20.0))
                            .header(20.0, |mut header| {
                                header.col(|ui| { ui.strong("Name"); });
                                header.col(|ui| { ui.strong("Value"); });
                                header.col(|_| {});
                            })
                            .body(|mut body| {
                                for (i, row) in ed.param_rows.iter_mut().enumerate() {
                                    body.row(18.0, |mut r| {
                                        r.col(|ui| { ui.text_edit_singleline(&mut row.name); });
                                        r.col(|ui| { ui.text_edit_singleline(&mut row.value); });
                                        r.col(|ui| {
                                            if ui.small_button("✖").clicked() {
                                                remove_param_idx = Some(i);
                                            }
                                        });
                                    });
                                }
                                body.row(18.0, |mut r| {
                                    r.col(|ui| {
                                        let lbl = RichText::new("➕ add parameter").weak();
                                        if ui.selectable_label(false, lbl).clicked() {
                                            ed.param_rows.push(ParamRow::new_default());
                                        }
                                    });
                                    r.col(|_| {});
                                    r.col(|_| {});
                                });
                            });
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.strong("Generics");
                        TableBuilder::new(ui)
                            .striped(true)
                            .column(Column::auto().at_least(90.0))
                            .column(Column::auto().at_least(50.0))
                            .column(Column::auto().at_least(50.0))
                            .column(Column::auto().at_least(50.0))
                            .column(Column::auto().at_least(120.0))
                            .column(Column::auto().at_least(20.0))
                            .header(20.0, |mut header| {
                                header.col(|ui| { ui.strong("Name"); });
                                header.col(|ui| { ui.strong("Min"); });
                                header.col(|ui| { ui.strong("Default"); });
                                header.col(|ui| { ui.strong("Max"); });
                                header.col(|ui| { ui.strong("Description"); });
                                header.col(|_| {});
                            })
                            .body(|mut body| {
                                for (i, row) in ed.generic_rows.iter_mut().enumerate() {
                                    body.row(18.0, |mut r| {
                                        r.col(|ui| { ui.text_edit_singleline(&mut row.name); });
                                        r.col(|ui| { ui.add(egui::TextEdit::singleline(&mut row.min).desired_width(40.0)); });
                                        r.col(|ui| { ui.add(egui::TextEdit::singleline(&mut row.default).desired_width(40.0)); });
                                        r.col(|ui| { ui.add(egui::TextEdit::singleline(&mut row.max).desired_width(40.0)); });
                                        r.col(|ui| { ui.text_edit_singleline(&mut row.desc); });
                                        r.col(|ui| {
                                            if ui.small_button("✖").clicked() {
                                                remove_generic_idx = Some(i);
                                            }
                                        });
                                    });
                                }
                                body.row(18.0, |mut r| {
                                    r.col(|ui| {
                                        let lbl = RichText::new("➕ add generic").weak();
                                        if ui.selectable_label(false, lbl).clicked() {
                                            ed.generic_rows.push(GenericRow::new_default());
                                        }
                                    });
                                    r.col(|_| {});
                                    r.col(|_| {});
                                    r.col(|_| {});
                                    r.col(|_| {});
                                    r.col(|_| {});
                                });
                            });
                    });
                });
                if let Some(i) = remove_param_idx {
                    ed.param_rows.remove(i);
                }
                if let Some(i) = remove_generic_idx {
                    ed.generic_rows.remove(i);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match ed.build_values() {
                            Ok((params, generics)) => {
                                ed.parse_err = None;
                                action = Some(EditAction::UpdateRifParamsAndGenerics { rif_type: ed.rif_type.clone(), params, generics });
                            }
                            Err(e) => ed.parse_err = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if let Some(err) = &ed.parse_err {
                        ui.label(RichText::new(err).color(egui::Color32::RED));
                    } else if let Some(err) = edit_err {
                        ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
                    }
                });
            });
        if cancel {
            *editor = None;
        }
        action
    }

    /// `src_pages` is the source RIF's own page list, used only to look up whether each
    /// compiled page is currently automatic (`RifPageInst` itself carries no such flag).
    pub fn display_content_rif(ui: &mut egui::Ui, selected: &mut Selection, rif: &RifInst, edit_mode: bool, src_pages: &[RifPage]) -> Option<PageClick> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Address width: {}", rif.addr_width)).weak());
            ui.separator();
            ui.label(RichText::new(format!("Data width: {}", rif.data_width)).weak());
        });
        ui.label(rif.description.get(false));
        for (i,page) in rif.pages.iter().enumerate() {
            ui.push_id(i, |ui| {
                let is_multipage = rif.pages.len() > 1;
                if  is_multipage {
                    ui.heading(page.name.to_owned());
                    ui.label(page.description.get(false));
                }
                let src_page = src_pages.iter().find(|p| p.name == page.name);
                if let Some(a) = Self::display_reg_summary(ui, selected, page, is_multipage, edit_mode, src_page) {
                    action = Some(a);
                }
            });
        }
        action
    }

    pub fn display_content_page(ui: &mut egui::Ui, selected: &mut Selection, page: &RifPageInst, edit_mode: bool, src_pages: &[RifPage]) -> Option<PageClick> {
        ui.label(page.description.get(false));
        let src_page = src_pages.iter().find(|p| p.name == page.name);
        Self::display_reg_summary(ui, selected, page, false, edit_mode, src_page)
    }

    /// Next free absolute address on a compiled page
    pub fn next_free_addr(page: &RifPageInst, data_width: u8) -> u64 {
        page.regs.iter().map(|r| r.addr).max().map_or(0, |m| m + data_width as u64)
    }

    /// Handle Swap and deletion of register
    pub fn handle_page_click(
        rif_src: &Option<RifGenSrc>,
        confirm_delete_reg: &mut Option<PendingDeleteReg>,
        pending: &mut Option<EditAction>,
        rif_type: &str,
        click: Option<PageClick>,
    ) {
        match click {
            Some(PageClick::SwapRegisters { a_inst_name, b_inst_name }) => {
                *pending = Some(EditAction::SwapRegAddr { rif_type: rif_type.to_owned(), a_inst_name, b_inst_name });
            }
            Some(PageClick::DeleteRegister { page_name, reg_type, inst_name }) => {
                // An automatic page always removes the definition (1:1 def:instance); on a
                // manual page, only when this is the last (or only) instance of its type.
                let removes_definition = rif_src.as_ref()
                    .and_then(|src| get_rif(&src.rifs, rif_type))
                    .and_then(|rif| rif.pages.iter().find(|p| p.name == page_name))
                    .is_none_or(|page| {
                        page.is_auto() || page.instances.iter().filter(|i| i.type_name == reg_type).count() <= 1
                    });
                if removes_definition {
                    *confirm_delete_reg = Some(PendingDeleteReg { rif_type: rif_type.to_owned(), page_name, reg_type, inst_name });
                } else {
                    *pending = Some(EditAction::DeleteRegister { rif_type: rif_type.to_owned(), page_name, reg_type, inst_name });
                }
            }
            Some(PageClick::ConvertToManual { .. }) | Some(PageClick::OpenAddRegister { .. }) | None => {}
        }
    }

    /// Check if a register can be re-order
    /// Currently does not support register with relative address
    pub fn is_reg_reorderable(reg: &RifRegInst, src_page: Option<&RifPage>) -> bool {
        Self::is_reg_editable(reg)
            && reg.array.opt_idx().is_none()
            && src_page.is_some_and(|p| {
                !p.is_auto() && p.instances.iter()
                    .find(|i| i.inst_name == reg.reg_name)
                    .is_some_and(|i| i.addr.kind == AddressKind::Absolute)
            })
    }

    /// Check is a new register address collides with another register instance
    fn find_addr_conflict<'a>(page_regs: &'a [RifRegInst], mover: &RifRegInst, new_addr: u64) -> Option<&'a RifRegInst> {
        page_regs.iter().find(|r| {
            r.reg_name != mover.reg_name && r.addr == new_addr && !mover.sw_access.exclusive(r.sw_access)
        })
    }

    /// Cascade address change when a register address is increased
    fn compute_shift_cascade(
        page_regs: &[RifRegInst], inst_name: &str, new_addr: u64, step: u64, src_page: Option<&RifPage>,
    ) -> Option<Vec<(String, u64)>> {
        let mut moves: Vec<(String, u64)> = Vec::new();
        let mut probe = new_addr;
        // Stops as soon as `probe` lands on genuinely free space — the `find` returning `None`.
        while let Some(occ) = page_regs.iter().find(|r| {
            r.reg_name != inst_name && r.addr == probe && !moves.iter().any(|(n, _)| n == &r.reg_name)
        }) {
            if !Self::is_reg_reorderable(occ, src_page) {
                return None;
            }
            let next = probe + step;
            moves.push((occ.reg_name.clone(), next));
            probe = next;
        }
        Some(moves)
    }

    /// Cascade address change when a register address is decreased
    fn compute_insert_rotation(
        page_regs: &[RifRegInst], inst_name: &str, old_addr: u64, new_addr: u64, src_page: Option<&RifPage>,
    ) -> Option<Vec<(String, u64)>> {
        let mut chain: Vec<&RifRegInst> = page_regs.iter()
            .filter(|r| r.reg_name != inst_name && r.addr >= new_addr && r.addr < old_addr)
            .collect();
        chain.sort_by_key(|r| r.addr);
        if chain.is_empty() || chain.iter().any(|r| !Self::is_reg_reorderable(r, src_page)) {
            return None;
        }
        Some(chain.iter().enumerate()
            .map(|(i, r)| (r.reg_name.clone(), chain.get(i + 1).map_or(old_addr, |n| n.addr)))
            .collect())
    }

    /// Handle case of register array growing
    fn compute_growth_cascade(
        page_regs: &[RifRegInst], inst_name: &str, base_addr: u64, old_footprint: u64, new_footprint: u64, src_page: Option<&RifPage>,
    ) -> Option<Vec<(String, u64)>> {
        let delta = new_footprint - old_footprint;
        let growth_start = base_addr + old_footprint;
        let mut affected: Vec<&RifRegInst> = page_regs.iter()
            .filter(|r| r.reg_name != inst_name && r.addr >= growth_start)
            .collect();
        if affected.iter().any(|r| !Self::is_reg_reorderable(r, src_page)) {
            return None;
        }
        affected.sort_by_key(|r| r.addr);
        Some(affected.into_iter().map(|r| (r.reg_name.clone(), r.addr + delta)).collect())
    }

    /// Check if a register array increase impact other registers
    fn compute_array_growth(
        page_regs: &[RifRegInst], src_page: Option<&RifPage>, inst_name: &str, step: u64, new_dim: u8,
    ) -> Result<Option<Vec<(String, u64)>>, ()> {
        let Some(mover) = page_regs.iter().filter(|r| r.reg_name == inst_name).min_by_key(|r| r.addr) else {
            return Ok(None); // defensive: no compiled row found
        };
        let old_footprint = (mover.array.dim().max(1) as u64) * step;
        let new_footprint = (new_dim.max(1) as u64) * step;
        if new_footprint <= old_footprint {
            return Ok(None);
        }
        match Self::compute_growth_cascade(page_regs, inst_name, mover.addr, old_footprint, new_footprint, src_page) {
            Some(cascade) => Ok(Some(cascade)),
            None => Err(()),
        }
    }

    /// Handle field definition changes after a register definition is changed to array
    #[allow(clippy::too_many_arguments)]
    fn resolve_def_array_commit(
        ed: &mut RegEditor, def_reg: &RegDef, page_regs: &[RifRegInst], src_page: Option<&RifPage>,
        array_change: &mut Option<PendingRegArrayChange>,
        rif_type: String, orig_name: String, inst_name: String, vals: RegDefVals, new_dim: u8,
    ) -> Option<EditAction> {
        let fields_to_convert = if new_dim > 0 {
            def_reg.fields.iter().filter(|f| matches!(f.array, Width::Value(0))).map(|f| f.name.clone()).collect()
        } else { Vec::new() };
        let step = ed.data_width.nb_byte() as u64;
        let companions = match Self::compute_array_growth(page_regs, src_page, &inst_name, step, new_dim) {
            Ok(c) => c,
            Err(()) => {
                ed.parse_err = Some(
                    "Growing this register collides with a following register that can't be moved \
                     automatically (array element or non-absolute address).".to_owned()
                );
                return None;
            }
        };
        if fields_to_convert.is_empty() && companions.as_ref().is_none_or(Vec::is_empty) {
            return Some(EditAction::UpdateRegDef { rif_type, orig_name, inst_name, vals });
        }
        *array_change = Some(PendingRegArrayChange {
            rif_type, orig_name, inst_name,
            kind: RegArrayChangeKind::Definition(Box::new(vals)),
            fields_to_convert,
            companions,
        });
        None
    }

    /// `ed.inst_array` changed: parse it (empty means "clear the array") and, on a manual page
    /// with a growing footprint, compute the shift cascade — apply directly when there's nothing
    /// to confirm, otherwise populate `array_change` for the modal. Never touches any field —
    /// instance-level arrays don't carry the "every field must also be an array" requirement.
    fn resolve_inst_array_commit(
        ed: &mut RegEditor, page_regs: &[RifRegInst], src_page: Option<&RifPage>,
        array_change: &mut Option<PendingRegArrayChange>,
        rif_type: String, orig_name: String, inst_name: String,
    ) -> Option<EditAction> {
        let raw = ed.inst_array.trim();
        let array = if raw.is_empty() {
            None
        } else {
            match parse_expr(raw) {
                Ok(tokens) => Some(tokens),
                Err(e) => { ed.parse_err = Some(format!("Invalid array size: {e}")); return None; }
            }
        };
        // A plain literal's dimension drives the growth check; a `$param`/generic-driven size
        // can't be evaluated without the RIF's parameter table (not available here) so the
        // growth-cascade check is simply skipped for that shape — recompile's own overlap
        // detection still catches a real collision, just without the Shift confirmation modal.
        let new_dim = array.as_ref().and_then(|a| a.eval(&ParamValues::new()).ok()).map(|v| v.max(0) as u8).unwrap_or(0);
        let step = ed.data_width.nb_byte() as u64;
        let companions = if new_dim > 0 {
            match Self::compute_array_growth(page_regs, src_page, &inst_name, step, new_dim) {
                Ok(c) => c,
                Err(()) => {
                    ed.parse_err = Some(
                        "Growing this register collides with a following register that can't be \
                         moved automatically (array element or non-absolute address).".to_owned()
                    );
                    return None;
                }
            }
        } else {
            None
        };
        if companions.as_ref().is_none_or(Vec::is_empty) {
            return Some(EditAction::UpdateRegInstArray { rif_type, inst_name, array, companions: Vec::new() });
        }
        *array_change = Some(PendingRegArrayChange {
            rif_type, orig_name, inst_name,
            kind: RegArrayChangeKind::Instance(array),
            fields_to_convert: Vec::new(),
            companions,
        });
        None
    }

    /// After `RegEditor::build_action()` parses successfully, decide how to commit whichever of
    /// address / definition-level array / instance-level array actually changed — shared by every
    /// commit point that can flush a `RegEditor` (the reload-flush and auto-apply points inside
    /// `show_reg_editor`, and `save_file`'s pre-save flush) so this logic lives exactly once.
    /// Address and array changes can't both be resolved in the same commit — each has its own
    /// independent collision model, and array growth would need to know the address *after* an
    /// address move, which isn't implemented — refused with a clear message rather than silently
    /// applying one and dropping the other.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_reg_commit(
        ed: &mut RegEditor,
        def_reg: &RegDef,
        page_regs: &[RifRegInst],
        src_page: Option<&RifPage>,
        conflict: &mut Option<PendingRegAddrConflict>,
        array_change: &mut Option<PendingRegArrayChange>,
    ) -> Option<EditAction> {
        let inst_array_changed = ed.inst_array_editable && ed.inst_array != ed.inst_array_orig;
        let action = ed.build_action()?; // parse/validation error already left in ed.parse_err
        let EditAction::UpdateRegDef { rif_type, orig_name, inst_name, vals } = action
            else { unreachable!("RegEditor::build_action always returns UpdateRegDef") };

        if vals.addr.is_some() && (vals.array.is_some() || inst_array_changed) {
            ed.parse_err = Some("Apply the address change and the array-size change separately.".to_owned());
            return None;
        }
        if let Some(new_dim) = vals.array {
            return Self::resolve_def_array_commit(ed, def_reg, page_regs, src_page, array_change, rif_type, orig_name, inst_name, vals, new_dim);
        }
        if inst_array_changed {
            return Self::resolve_inst_array_commit(ed, page_regs, src_page, array_change, rif_type, orig_name, inst_name);
        }

        let Some(new_addr) = vals.addr else {
            return Some(EditAction::UpdateRegDef { rif_type, orig_name, inst_name, vals });
        };
        let Some(mover) = page_regs.iter().find(|r| r.reg_name == inst_name) else {
            return Some(EditAction::UpdateRegDef { rif_type, orig_name, inst_name, vals }); // defensive
        };
        let old_addr = mover.addr;
        let Some(target) = Self::find_addr_conflict(page_regs, mover, new_addr) else {
            return Some(EditAction::UpdateRegDef { rif_type, orig_name, inst_name, vals }); // unambiguous
        };
        let target_name = target.reg_name.clone();
        let step = ed.data_width.nb_byte() as u64;
        let swap = Self::is_reg_reorderable(target, src_page).then(|| vec![(target_name.clone(), old_addr)]);
        let shift = (new_addr > old_addr)
            .then(|| Self::compute_shift_cascade(page_regs, &inst_name, new_addr, step, src_page)).flatten();
        let insert = (new_addr < old_addr)
            .then(|| Self::compute_insert_rotation(page_regs, &inst_name, old_addr, new_addr, src_page)).flatten();
        if swap.is_none() && shift.is_none() && insert.is_none() {
            ed.parse_err = Some(format!(
                "Address 0x{new_addr:x} is already used by '{target_name}', which can't be moved \
                 automatically (array element or non-absolute address)."
            ));
            return None;
        }
        *conflict = Some(PendingRegAddrConflict { rif_type, orig_name, inst_name, new_addr, target_name, vals, swap, shift, insert });
        None
    }

    /// A register that can be removed from the summary table: editable, and not part of an
    /// array — deleting a definition-level array as a unit is untested (`test.rif` has none),
    /// so it's excluded rather than risked, same caution `is_reg_reorderable` already applies.
    fn is_reg_deletable(reg: &RifRegInst) -> bool {
        Self::is_reg_editable(reg) && reg.array.opt_idx().is_none()
    }

    fn display_reg_summary(ui: &mut egui::Ui, selected: &mut Selection, page: &RifPageInst, is_multipage: bool, edit_mode: bool, src_page: Option<&RifPage>) -> Option<PageClick> {
        let mut action = None;
        ui.separator();
        let page_is_auto = src_page.is_some_and(|p| p.is_auto());
        if edit_mode && page_is_auto && ui.button("🔓 Convert to manual addressing").clicked() {
            action = Some(PageClick::ConvertToManual { page_name: page.name.clone() });
        }

        // Create a table to display register summary
        let mut table = TableBuilder::new(ui).striped(true)
            .column(Column::auto().at_least(70.0))  // Address column
            .column(Column::auto().at_least(150.0)) // Name column
            .column(Column::auto().at_least(100.0)) // Reset value column
            .column(Column::remainder());           // Description column
        if edit_mode {
            table = table.column(Column::auto().at_least(68.0)); // compact up/down/delete icons
        }

        let n = page.regs.len();
        table
            .header(20.0, |mut header| {
                header.col(|ui| { ui.strong("Address"); });
                header.col(|ui| { ui.strong("Name"); });
                header.col(|ui| { ui.strong("Reset"); });
                header.col(|ui| { ui.strong("Description"); });
                if edit_mode {
                    header.col(|_| {});
                }
            })
            .body(|mut body| {
                for (idx, reg) in page.regs.iter().enumerate() {
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

                        // Compact reorder icons: swap address with the address-adjacent neighbour
                        if edit_mode {
                            row.col(|ui| {
                                let can_up = idx > 0
                                    && Self::is_reg_reorderable(reg, src_page)
                                    && Self::is_reg_reorderable(&page.regs[idx - 1], src_page);
                                let can_down = idx + 1 < n
                                    && Self::is_reg_reorderable(reg, src_page)
                                    && Self::is_reg_reorderable(&page.regs[idx + 1], src_page);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 2.0;
                                    if ui.add_enabled(can_up, egui::Button::new("⬆").small())
                                        .on_hover_text("Swap address with the register above").clicked()
                                    {
                                        action = Some(PageClick::SwapRegisters {
                                            a_inst_name: page.regs[idx - 1].reg_name.clone(),
                                            b_inst_name: reg.reg_name.clone(),
                                        });
                                    }
                                    if ui.add_enabled(can_down, egui::Button::new("⬇").small())
                                        .on_hover_text("Swap address with the register below").clicked()
                                    {
                                        action = Some(PageClick::SwapRegisters {
                                            a_inst_name: reg.reg_name.clone(),
                                            b_inst_name: page.regs[idx + 1].reg_name.clone(),
                                        });
                                    }
                                    if Self::is_reg_deletable(reg)
                                        && ui.button(RichText::new("🗑").small())
                                            .on_hover_text("Delete this register").clicked()
                                    {
                                        let inst_name = if page_is_auto { None } else { Some(reg.reg_name.clone()) };
                                        action = Some(PageClick::DeleteRegister {
                                            page_name: page.name.clone(),
                                            reg_type: reg.reg_type.clone(),
                                            inst_name,
                                        });
                                    }
                                });
                            });
                        }
                    });
                }
                // Discoverable "add register" affordance, mirroring the field table's ghost row
                if edit_mode {
                    body.row(18.0, |mut row| {
                        row.col(|ui| {
                            let lbl = RichText::new("➕ add register").weak();
                            if ui.selectable_label(false, lbl).clicked() {
                                action = Some(PageClick::OpenAddRegister { page_name: page.name.clone() });
                            }
                        });
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                    });
                }
            });
        action
    }

    pub fn display_content_reg(ui: &mut egui::Ui, selected: &Selection, reg: &RifRegInst, edit_mode: bool) -> Option<RegClick> {
        let mut action = None;
        // Hidden whenever some editor above already shows its own (editable) description field —
        // i.e. whenever one of the three editor branches at this register's call site actually
        // renders (`show_reg_intr_desc_editor`/`show_reg_override_editor`/`show_reg_editor`).
        // Mirrors that call site's own branch conditions so the read-only label doesn't linger
        // redundantly beside an editable one, without needing to thread the exact branch taken
        // through as an extra parameter.
        let shows_own_description = edit_mode
            && (reg.is_intr_derived() || Self::is_reg_editable(reg) || Self::is_reg_override_editable(reg));
        if !shows_own_description {
            ui.label(reg.description.get(false));
        }
        ui.separator();
        // Only definition-backed registers can gain fields / be reordered via the editor
        let editable_reg = edit_mode && Self::is_reg_editable(reg);
        let n = reg.fields.len();

        // TODO: estimate max length of name
        let width_name = reg.fields.iter().map(|f| f.name().len()).max().unwrap_or(0) as f32 * 7.9;
        let width_reset = ((reg.fields.iter().map(|f| f.width()).max().unwrap_or(0) >> 2) + 2) as f32 * 7.9;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(18.0*16.0+20.0) // Limit to 16 rows (include the header)
            .column(Column::auto().at_least(35.0))
            .column(Column::auto().at_least(width_name))
            .column(Column::auto().at_least(45.0))
            .column(Column::auto().at_least(width_reset))
            .column(Column::remainder());
        if editable_reg {
            table = table.column(Column::auto().at_least(46.0)); // compact up/down reorder icons
        }
        table
            .header(20.0, |mut header| {
                header.col(|ui| {ui.strong("Bits"  );});
                header.col(|ui| {ui.strong("Name"  );});
                header.col(|ui| {ui.strong("Access");});
                header.col(|ui| {ui.strong("Reset" );});
                header.col(|ui| {ui.strong("Desc." );});
                if editable_reg {
                    header.col(|_| {});
                }
            })
            .body(|mut body| {
                // Auto select first field if only one present
                if reg.fields.len() == 1 {
                    action = reg.fields.first().map(|f| RegClick::Select(f.name()));
                }
                for (idx, f) in reg.fields.iter().enumerate() {
                    body.row(18.0, |mut row| {
                        let pos = if f.width()==1 {format!("{}",f.lsb)} else {format!("{}:{}",f.msb(), f.lsb)};
                        row.col(|ui| {ui.label(pos);});
                        row.col(|ui| {
                            let name = f.name();
                            let selected = Some(name.as_str()) == selected.item.name() && selected.item.is_field();
                            if ui.selectable_label(selected, name).clicked() {
                                action = Some(RegClick::Select(f.name()));
                            }
                        });
                        row.col(|ui| {ui.label(f.sw_kind.access_str());});
                        row.col(|ui| {ui.label(get_field_rst_str(f));});
                        row.col(|ui| {ui.label(f.description.get_short(false));});
                        // Compact reorder icons: swap bit position with the editable neighbour
                        if editable_reg {
                            row.col(|ui| {
                                let can_up = idx > 0 && Self::is_field_reorderable(f) && Self::is_field_reorderable(&reg.fields[idx - 1]);
                                let can_down = idx + 1 < n && Self::is_field_reorderable(f) && Self::is_field_reorderable(&reg.fields[idx + 1]);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 2.0;
                                    if ui.add_enabled(can_up, egui::Button::new("⬆").small())
                                        .on_hover_text("Swap bit position with the field below").clicked()
                                    {
                                        let (lower_pos, higher_pos) = swap_positions(&reg.fields[idx - 1], f);
                                        action = Some(RegClick::MoveField { moves: vec![
                                            (reg.fields[idx - 1].name.clone(), lower_pos),
                                            (f.name.clone(), higher_pos),
                                        ]});
                                    }
                                    if ui.add_enabled(can_down, egui::Button::new("⬇").small())
                                        .on_hover_text("Swap bit position with the field above").clicked()
                                    {
                                        let (lower_pos, higher_pos) = swap_positions(f, &reg.fields[idx + 1]);
                                        action = Some(RegClick::MoveField { moves: vec![
                                            (f.name.clone(), lower_pos),
                                            (reg.fields[idx + 1].name.clone(), higher_pos),
                                        ]});
                                    }
                                });
                            });
                        }
                    });
                }
                // Discoverable "add field" affordance: a ghost row at the bottom of the table
                if editable_reg {
                    body.row(18.0, |mut row| {
                        row.col(|_| {});
                        row.col(|ui| {
                            let lbl = RichText::new("➕ add field").weak();
                            if ui.selectable_label(false, lbl).clicked() {
                                action = Some(RegClick::AddField);
                            }
                        });
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                    });
                }
            });
        action
    }

    /// A register whose fields map 1:1 to a local `RegDef` (not interrupt-derived, not included).
    /// A base interrupt register (`is_intr()`) *is* included — it goes through this same editor,
    /// gaining the extra "Interrupt" section (see `show_reg_editor`). A derived (enable/mask/
    /// pending) register is excluded here and instead goes through `show_reg_intr_desc_editor`.
    pub fn is_reg_editable(reg: &RifRegInst) -> bool {
        !reg.is_intr_derived() && reg.incl.is_none()
    }

    /// A register instance whose *own override* (as opposed to its shared type) can be edited —
    /// deliberately independent of `reg.incl`: an included register's definition lives in
    /// another RIF and stays out of scope for editing, but its instance override always lives in
    /// the *current* RIF's page, so it's editable regardless. Excludes interrupt-derived and
    /// array-element registers, same reasoning `is_reg_editable`/`is_reg_reorderable` already use
    /// (register arrays are deferred throughout, and a derived register has no override of its
    /// own to speak of).
    pub fn is_reg_override_editable(reg: &RifRegInst) -> bool {
        !reg.is_intr_derived() && reg.array.opt_idx().is_none()
    }

    /// A field whose *instance override* (as opposed to its shared definition) can be edited —
    /// only array elements are excluded (no independent slot to target, same reasoning
    /// `is_field_reorderable` uses), since description/reset/disable overrides make sense
    /// regardless of the field's own kind (generic width, partial, enum, password, pulse, ...).
    pub fn is_field_override_editable(field: &RifFieldInst) -> bool {
        field.array.dim() == 0
    }

    /// Shared header for the register/field editing panels: the Definition/Instance toggle,
    /// shown only when it's actually meaningful (`show_toggle`). Rendered once per register view
    /// (above `show_reg_editor`/`show_reg_override_editor`), governing both it and, once a field
    /// is drilled into, `show_field_editor`/`show_field_override_editor` below it.
    ///
    /// `definition_disabled` (the register's definition lives in another RIF) disables the
    /// "Definition" button and forces `edit_target` to `Instance`; hiding the toggle altogether
    /// forces it back to `Definition` — both corrections happen here, every frame, so neither
    /// caller has to duplicate the invariant.
    pub fn show_edit_target_header(ui: &mut egui::Ui, edit_target: &mut EditTarget, _reg: &RifRegInst, show_toggle: bool, definition_disabled: bool) {
        if !show_toggle {
            *edit_target = EditTarget::Definition;
        } else if definition_disabled && *edit_target == EditTarget::Definition {
            *edit_target = EditTarget::Instance;
        }
        if !show_toggle {
            return;
        }
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!definition_disabled, |ui| {
                ui.selectable_value(edit_target, EditTarget::Definition, "Definition");
            }).response.on_hover_text(if definition_disabled {
                "This register's definition lives in another RIF — only the instance override can be edited here."
            } else {
                "Edit the shared register type — affects every instance."
            });
            ui.selectable_value(edit_target, EditTarget::Instance, "Instance")
                .on_hover_text("Edit only this instance, via a per-instance override — leaves the shared type untouched.");
        });
        ui.add_space(6.0);
    }

    /// Render the editable register panel, shown above a register's field table in edit mode.
    /// Returns an `EditAction` when "Apply" is pressed and every buffer parses. Mirrors
    /// `show_field_editor`'s load/Apply shape (including its "Advanced properties" fold —
    /// interrupt, access pulse, visibility, clock, reset, external), minus Delete (add/remove/
    /// reorder registers are a separate, later increment). `addr_editable` is `true` only when
    /// the owning page is already manual; the address field is always shown (so the current
    /// value stays visible) but only accepted on Apply when editable.
    #[allow(clippy::too_many_arguments)]
    pub fn show_reg_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<RegEditor>,
        edit_err: &Option<String>,
        rif_type: &str,
        reg: &RifRegInst,
        def_reg: &RegDef,
        default_clk: &str,
        addr_editable: bool,
        // Distinct group names in use across the current page's register definitions (own
        // group included), for the "Group" row's dropdown — see the call site.
        group_choices: &[String],
        data_width: DataWidth,
        page_regs: &[RifRegInst],
        src_page: Option<&RifPage>,
        src_inst: Option<&RegInst>,
        array_scope_ok: bool,
        conflict: &mut Option<PendingRegAddrConflict>,
        array_change: &mut Option<PendingRegArrayChange>,
    ) -> Option<EditAction> {
        // (Re)load the buffers when they no longer belong to the selected instance — keyed on
        // the instance, not the type, since address differs per instance of the same type — or
        // when `addr_editable` itself changed. The latter matters because this panel isn't even
        // shown while viewing the page-level table (where "Convert to manual addressing" lives):
        // if this register's editor was already open *before* converting the page, navigating
        // straight back into it keeps the same `inst_name`, so without this check it would keep
        // showing a stale disabled address field (and silently discard any address the user
        // still managed to type, since Apply gates on `ed.addr_editable` too) even though the
        // page is manual now.
        if editor.as_ref().map(|e| (e.inst_name.as_str(), e.addr_editable)) != Some((reg.reg_name.as_str(), addr_editable)) {
            // Give the outgoing editor (if any) one last chance to commit — see the identical
            // flush in `show_field_editor` for why this can't just rely on per-widget blur.
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
                && let Some(action) = Self::resolve_reg_commit(old, def_reg, page_regs, src_page, conflict, array_change)
            {
                return Some(action);
            }
            *editor = Some(RegEditor::from_regdef(
                reg.reg_name.clone(), reg.reg_type.clone(), def_reg, reg.addr, addr_editable, rif_type, default_clk, data_width,
                src_inst, array_scope_ok,
            ));
        }
        let ed = editor.as_mut().expect("editor just set");

        // Set whenever a buffer is blurred (typed text: `lost_focus()`) or picked (checkbox/combo:
        // `changed()`) — edits commit as soon as the user moves on to something else, no explicit
        // "Apply" needed (mirrors `show_field_editor`).
        let mut auto_apply = false;
        egui::Grid::new("reg_editor_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            // Name shares its line with Group — half the field width Name would get on its own
            // row, to leave room for the group box + combo (mirrors `show_rif_editor`'s
            // Name/Address-width/Data-width row).
            ui.label("Name");
            ui.horizontal(|ui| {
                if ui.add(egui::TextEdit::singleline(&mut ed.name).desired_width(140.0)).lost_focus() { auto_apply = true; }
                ui.add_space(10.0);
                ui.label("Group");
                if ui.add(egui::TextEdit::singleline(&mut ed.group).desired_width(120.0)).lost_focus() { auto_apply = true; }
                // Picking an entry here just overwrites the text buffer above with it — the
                // combo box itself carries no independent selection state.
                egui::ComboBox::from_id_salt("reg_group_combo").width(18.0).selected_text("").show_ui(ui, |ui| {
                    for g in group_choices {
                        if ui.selectable_label(ed.group == *g, g).clicked() {
                            ed.group = g.clone();
                            auto_apply = true;
                        }
                    }
                });
            });
            ui.end_row();

            // Array shares its line with Instance array — the definition-level (shared by every
            // instance of this type) and instance-level (this instance only) array controls,
            // mirrored from `field_view.rs`'s "Array" button: a plain button while not an array,
            // an inline size box once it is.
            ui.label("Array");
            ui.horizontal(|ui| {
                ui.add_enabled_ui(ed.array_editable, |ui| {
                    let is_array = ed.array.trim().parse::<u8>().map(|n| n > 0).unwrap_or(false);
                    if is_array {
                        if ui.add(egui::TextEdit::singleline(&mut ed.array).desired_width(40.0))
                            .on_hover_text("Array size; clear or set to 0 to remove the array. Every field on this register must also be an array — converting non-array fields to [1] will be offered on Apply.")
                            .lost_focus() { auto_apply = true; }
                    } else if ui.button("Array").on_hover_text("Turn this register into an array").clicked() {
                        ed.array = "2".to_owned();
                        auto_apply = true;
                    }
                }).response.on_hover_text(if ed.array_editable {
                    ""
                } else {
                    "Not available: this type has more than one manual instance on the page, this \
                     instance already has its own instance-level array, or the type is `$param`-sized."
                });
                ui.add_space(10.0);
                ui.label("Instance");
                ui.add_enabled_ui(ed.inst_array_editable, |ui| {
                    let is_array = !ed.inst_array.trim().is_empty();
                    if is_array {
                        if ui.add(egui::TextEdit::singleline(&mut ed.inst_array).desired_width(60.0))
                            .on_hover_text("Array size for this instance only (independent of the shared type); clear to remove.")
                            .lost_focus() { auto_apply = true; }
                    } else if ui.button("Array").on_hover_text("Turn only this instance into an array, without affecting the shared type").clicked() {
                        ed.inst_array = "2".to_owned();
                        auto_apply = true;
                    }
                }).response.on_hover_text(if ed.inst_array_editable {
                    ""
                } else {
                    "Not available: the page must be manual, the shared type must not already be an \
                     array, and this type must have at most one manual instance on the page."
                });
            });
            ui.end_row();

            ui.label("Description");
            if ui.add(egui::TextEdit::multiline(&mut ed.desc).desired_width(320.0).desired_rows(3))
                .on_hover_text("First line is the inline (short) description; further lines become a `description:` block.")
                .lost_focus() { auto_apply = true; }
            ui.end_row();

            ui.label("Address");
            let addr_resp = ui.add_enabled_ui(ed.addr_editable, |ui| {
                ui.add(egui::TextEdit::singleline(&mut ed.addr).desired_width(100.0))
            });
            if addr_resp.inner.lost_focus() { auto_apply = true; }
            addr_resp.response.on_hover_text(if ed.addr_editable {
                "Absolute address for this instance. Apply this separately from an array-size change."
            } else {
                "This page assigns addresses automatically. Use \"Convert to manual addressing\" \
                 (above the register list) to edit addresses individually."
            });
            ui.end_row();
        });

        // Advanced sub-properties (each lives on its own line under the declaration), mirroring
        // `field_view.rs`'s "Advanced properties" fold exactly.
        egui::CollapsingHeader::new("Advanced properties")
            .id_salt(("reg_adv_properties", &ed.inst_name))
            .default_open(false).show(ui, |ui| {
            egui::Grid::new("reg_editor_adv_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                adv_label(ui, "Interrupt register", ed.has_intr);
                if ui.checkbox(&mut ed.has_intr, "").changed() { auto_apply = true; }
                ui.end_row();

                if ed.has_intr {
                    ui.label("Trigger");
                    egui::ComboBox::from_id_salt("reg_intr_trigger")
                        .selected_text(intr_trigger_label(ed.intr_trigger))
                        .show_ui(ui, |ui| {
                            for t in [InterruptTrigger::High, InterruptTrigger::Low, InterruptTrigger::Rising, InterruptTrigger::Falling, InterruptTrigger::Edge] {
                                if ui.selectable_value(&mut ed.intr_trigger, t, intr_trigger_label(t)).changed() { auto_apply = true; }
                            }
                        });
                    ui.end_row();

                    ui.label("Clear method");
                    egui::ComboBox::from_id_salt("reg_intr_clear")
                        .selected_text(intr_clear_label(ed.intr_clear))
                        .show_ui(ui, |ui| {
                            for c in [InterruptClr::Read, InterruptClr::Write0, InterruptClr::Write1, InterruptClr::Hw] {
                                if ui.selectable_value(&mut ed.intr_clear, c, intr_clear_label(c)).changed() { auto_apply = true; }
                            }
                        });
                    ui.end_row();

                    ui.label("Enable register");
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut ed.intr_enable_on, "").changed() {
                            // Default a freshly-enabled register to reset 0 rather than surfacing a
                            // "Reset value is empty" validation error on the very next auto-apply.
                            if ed.intr_enable_on && ed.intr_enable_rst.trim().is_empty() {
                                ed.intr_enable_rst = "0".to_owned();
                            }
                            auto_apply = true;
                        }
                        ui.add_enabled_ui(ed.intr_enable_on, |ui| {
                            ui.label("reset");
                            if ui.add(egui::TextEdit::singleline(&mut ed.intr_enable_rst).desired_width(80.0)).lost_focus() { auto_apply = true; }
                        });
                    });
                    ui.end_row();

                    ui.label("Mask register");
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut ed.intr_mask_on, "").changed() {
                            if ed.intr_mask_on && ed.intr_mask_rst.trim().is_empty() {
                                ed.intr_mask_rst = "0".to_owned();
                            }
                            auto_apply = true;
                        }
                        ui.add_enabled_ui(ed.intr_mask_on, |ui| {
                            ui.label("reset");
                            if ui.add(egui::TextEdit::singleline(&mut ed.intr_mask_rst).desired_width(80.0)).lost_focus() { auto_apply = true; }
                        });
                    });
                    ui.end_row();

                    ui.label("Pending register");
                    if ui.checkbox(&mut ed.intr_pending_on, "").changed() { auto_apply = true; }
                    ui.end_row();

                    // Named secondary interrupt block (`RegDef.interrupt[1]`) — same fields as
                    // the primary section above, except the Name box itself is the toggle (blank
                    // = no alt block) instead of a separate checkbox.
                    adv_label(ui, "Alt interrupt name", !ed.alt_name.trim().is_empty());
                    if ui.add(egui::TextEdit::singleline(&mut ed.alt_name).desired_width(120.0))
                        .on_hover_text(
                            "Adds a second, named interrupt output for this register with its own \
                             enable/mask/pending settings (same event bits as the primary interrupt). \
                             Clear to remove."
                        )
                        .lost_focus() { auto_apply = true; }
                    ui.end_row();

                    if !ed.alt_name.trim().is_empty() {
                        ui.label("Alt trigger");
                        egui::ComboBox::from_id_salt("reg_alt_intr_trigger")
                            .selected_text(intr_trigger_label(ed.alt_trigger))
                            .show_ui(ui, |ui| {
                                for t in [InterruptTrigger::High, InterruptTrigger::Low, InterruptTrigger::Rising, InterruptTrigger::Falling, InterruptTrigger::Edge] {
                                    if ui.selectable_value(&mut ed.alt_trigger, t, intr_trigger_label(t)).changed() { auto_apply = true; }
                                }
                            });
                        ui.end_row();

                        ui.label("Alt clear method");
                        egui::ComboBox::from_id_salt("reg_alt_intr_clear")
                            .selected_text(intr_clear_label(ed.alt_clear))
                            .show_ui(ui, |ui| {
                                for c in [InterruptClr::Read, InterruptClr::Write0, InterruptClr::Write1, InterruptClr::Hw] {
                                    if ui.selectable_value(&mut ed.alt_clear, c, intr_clear_label(c)).changed() { auto_apply = true; }
                                }
                            });
                        ui.end_row();

                        ui.label("Alt enable register");
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut ed.alt_enable_on, "").changed() {
                                if ed.alt_enable_on && ed.alt_enable_rst.trim().is_empty() {
                                    ed.alt_enable_rst = "0".to_owned();
                                }
                                auto_apply = true;
                            }
                            ui.add_enabled_ui(ed.alt_enable_on, |ui| {
                                ui.label("reset");
                                if ui.add(egui::TextEdit::singleline(&mut ed.alt_enable_rst).desired_width(80.0)).lost_focus() { auto_apply = true; }
                            });
                        });
                        ui.end_row();

                        ui.label("Alt mask register");
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut ed.alt_mask_on, "").changed() {
                                if ed.alt_mask_on && ed.alt_mask_rst.trim().is_empty() {
                                    ed.alt_mask_rst = "0".to_owned();
                                }
                                auto_apply = true;
                            }
                            ui.add_enabled_ui(ed.alt_mask_on, |ui| {
                                ui.label("reset");
                                if ui.add(egui::TextEdit::singleline(&mut ed.alt_mask_rst).desired_width(80.0)).lost_focus() { auto_apply = true; }
                            });
                        });
                        ui.end_row();

                        ui.label("Alt pending register");
                        if ui.checkbox(&mut ed.alt_pending_on, "").changed() { auto_apply = true; }
                        ui.end_row();
                    }
                }

                ui.label("Access pulse");
                ui.horizontal(|ui| {
                    for (label, on, is_reg, salt) in [
                        ("wr", &mut ed.wr_on, &mut ed.wr_reg, "reg_wr_pulse_kind"),
                        ("rd", &mut ed.rd_on, &mut ed.rd_reg, "reg_rd_pulse_kind"),
                        ("acc", &mut ed.acc_on, &mut ed.acc_reg, "reg_acc_pulse_kind"),
                    ] {
                        if ui.checkbox(on, label).changed() { auto_apply = true; }
                        ui.add_enabled_ui(*on, |ui| {
                            egui::ComboBox::from_id_salt(salt)
                                .selected_text(if *is_reg { "Reg" } else { "Comb" })
                                .show_ui(ui, |ui| {
                                    if ui.selectable_value(is_reg, true, "Reg").changed() { auto_apply = true; }
                                    if ui.selectable_value(is_reg, false, "Comb").changed() { auto_apply = true; }
                                });
                        });
                    }
                });
                ui.end_row();

                adv_label(ui, "Visibility", ed.visibility != Visibility::Full);
                egui::ComboBox::from_id_salt("reg_visibility")
                    .selected_text(match ed.visibility {
                        Visibility::Hidden => "Hidden",
                        Visibility::Reserved => "Reserved",
                        _ => "Full",
                    })
                    .show_ui(ui, |ui| {
                        for v in [Visibility::Full, Visibility::Hidden, Visibility::Reserved] {
                            let label = match v { Visibility::Hidden => "Hidden", Visibility::Reserved => "Reserved", _ => "Full" };
                            if ui.selectable_value(&mut ed.visibility, v, label).changed() { auto_apply = true; }
                        }
                    });
                ui.end_row();

                adv_label(ui, "Clock", !ed.clk.trim().is_empty());
                if ui.add(egui::TextEdit::singleline(&mut ed.clk).desired_width(140.0))
                    .on_hover_text("Optional clock override for this register; blank follows the RIF/page default.")
                    .lost_focus() { auto_apply = true; }
                ui.end_row();

                adv_label(ui, "Reset", !ed.rst.trim().is_empty());
                if ui.add(egui::TextEdit::singleline(&mut ed.rst).desired_width(140.0))
                    .on_hover_text("Optional reset override for this register (`hwReset`); blank follows the RIF/page default.")
                    .lost_focus() { auto_apply = true; }
                ui.end_row();

                adv_label(ui, "External", ed.external != ExternalKind::None);
                egui::ComboBox::from_id_salt("reg_external")
                    .selected_text(match ed.external {
                        ExternalKind::ReadWrite => "Read+Write",
                        ExternalKind::Done => "Done",
                        _ => "None",
                    })
                    .show_ui(ui, |ui| {
                        for k in [ExternalKind::None, ExternalKind::ReadWrite, ExternalKind::Done] {
                            let label = match k { ExternalKind::ReadWrite => "Read+Write", ExternalKind::Done => "Done", _ => "None" };
                            if ui.selectable_value(&mut ed.external, k, label).changed() { auto_apply = true; }
                        }
                    });
                ui.end_row();
            });
        });

        ui.add_space(6.0);
        if let Some(err) = &ed.parse_err {
            ui.label(RichText::new(err).color(egui::Color32::RED));
        } else if let Some(err) = edit_err {
            ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
        }

        if !auto_apply {
            return None;
        }
        Self::resolve_reg_commit(ed, def_reg, page_regs, src_page, conflict, array_change)
    }

    /// Render the register instance's whole-register override panel, shown instead of
    /// `show_reg_editor` when `EditTarget::Instance` is selected (see `show_edit_target_header`).
    /// Needs no `RegDef` at all — unlike `show_reg_editor`, so this reaches an included
    /// register's instance too, whose definition lives in another RIF. `src_inst` is the source
    /// `RegInst` (`None` only if it genuinely can't be found, e.g. a not-yet-compiled edge case);
    /// its own `reg_override` entry (if any) is what gets loaded.
    pub fn show_reg_override_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<RegOverrideEditor>,
        edit_err: &Option<String>,
        rif_type: &str,
        reg: &RifRegInst,
        src_inst: Option<&RegInst>,
    ) -> Option<EditAction> {
        if editor.as_ref().map(|e| e.inst_name.as_str()) != Some(reg.reg_name.as_str()) {
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
                && let Some(action) = old.build_action()
            {
                return Some(action);
            }
            let ovr = src_inst.and_then(|i| i.reg_override.get(&None));
            *editor = Some(RegOverrideEditor::from_reg_inst(rif_type, &reg.reg_name, ovr));
        }
        let ed = editor.as_mut().expect("editor just set");

        let mut auto_apply = false;
        egui::Grid::new("reg_override_editor_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Description");
            if ui.add(egui::TextEdit::multiline(&mut ed.desc).desired_width(320.0).desired_rows(3))
                .on_hover_text("Override this register's description for this instance only.")
                .lost_focus() { auto_apply = true; }
            ui.end_row();

            ui.label("Hw access");
            ui.horizontal(|ui| {
                if ui.checkbox(&mut ed.hw_on, "").changed() { auto_apply = true; }
                ui.add_enabled_ui(ed.hw_on, |ui| {
                    egui::ComboBox::from_id_salt("reg_ovr_hw").selected_text(format!("{}", ed.hw)).show_ui(ui, |ui| {
                        for acc in [Access::NA, Access::RO, Access::WO, Access::RW] {
                            if ui.selectable_value(&mut ed.hw, acc, format!("{acc}")).changed() { auto_apply = true; }
                        }
                    });
                });
            });
            ui.end_row();

            ui.label("Optional condition");
            if ui.add(egui::TextEdit::singleline(&mut ed.optional).desired_width(220.0))
                .on_hover_text("Instantiate this register only while this expression is true/non-zero; blank = always instantiated.")
                .lost_focus() { auto_apply = true; }
            ui.end_row();

            ui.label("Optional access");
            ui.horizontal(|ui| {
                if ui.checkbox(&mut ed.optional_acc_on, "").changed() { auto_apply = true; }
                ui.add_enabled_ui(ed.optional_acc_on, |ui| {
                    egui::ComboBox::from_id_salt("reg_ovr_optional_acc").selected_text(format!("{}", ed.optional_acc)).show_ui(ui, |ui| {
                        for acc in [Access::NA, Access::RO, Access::RW] {
                            if ui.selectable_value(&mut ed.optional_acc, acc, format!("{acc}")).changed() { auto_apply = true; }
                        }
                    });
                });
            }).response.on_hover_text("Software access while this register is optional and currently disabled.");
            ui.end_row();
        });

        ui.add_space(6.0);
        if let Some(err) = &ed.parse_err {
            ui.label(RichText::new(err).color(egui::Color32::RED));
        } else if let Some(err) = edit_err {
            ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
        }

        if !auto_apply {
            return None;
        }
        ed.build_action()
    }

    /// Render the minimal editor for a derived (enable/mask/pending) interrupt register: just its
    /// own `{enable,mask,pending}.description:`, on the *base* register's primary interrupt. No
    /// name/group/pulse/address/trigger — none of that applies to a derived view (see the plan's
    /// scope notes). `def_reg` is the *base* `RegDef` (a derived register has none of its own).
    pub fn show_reg_intr_desc_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<RegIntrDescEditor>,
        edit_err: &Option<String>,
        rif_type: &str,
        reg: &RifRegInst,
        def_reg: &RegDef,
        kind: InterruptRegKind,
    ) -> Option<EditAction> {
        if editor.as_ref().map(|e| (e.reg_type.as_str(), e.kind)) != Some((reg.reg_type.as_str(), kind)) {
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
            {
                return Some(old.build_action());
            }
            *editor = Some(RegIntrDescEditor::from_regdef(rif_type.to_owned(), reg.reg_type.clone(), def_reg, kind));
        }
        let ed = editor.as_mut().expect("editor just set");

        ui.label(RichText::new(format!("Editing '{}' description for interrupt register '{}'", intr_kind_label(ed.kind), reg.reg_type)).italics().weak());
        ui.add_space(6.0);

        let mut auto_apply = false;
        if ui.add(egui::TextEdit::multiline(&mut ed.desc).desired_width(320.0).desired_rows(3))
            .on_hover_text("Description shown for this derived register specifically — independent of the base register's own description.")
            .lost_focus() { auto_apply = true; }

        ui.add_space(6.0);
        if let Some(err) = edit_err {
            ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
        }

        if !auto_apply || ed.is_unchanged() {
            return None;
        }
        Some(ed.build_action())
    }

    /// Editor for field interrupt
    #[allow(clippy::too_many_arguments)]
    pub fn show_field_intr_desc_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<FieldIntrDescEditor>,
        edit_err: &Option<String>,
        rif_type: &str,
        reg_type: &str,
        field: &RifFieldInst,
        def_field: &Field,
        kind: InterruptRegKind,
    ) -> Option<EditAction> {
        if editor.as_ref().map(|e| (e.reg_type.as_str(), e.field_name.as_str(), e.kind)) != Some((reg_type, field.name.as_str(), kind)) {
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
            {
                return Some(old.build_action());
            }
            *editor = Some(FieldIntrDescEditor::from_field(rif_type.to_owned(), reg_type.to_owned(), field.name.clone(), def_field, kind));
        }
        let ed = editor.as_mut().expect("editor just set");

        ui.label(RichText::new(format!("Editing '{}' description for field '{}'", intr_kind_label(ed.kind), field.name)).italics().weak());
        ui.add_space(6.0);

        let mut auto_apply = false;
        if ui.add(egui::TextEdit::multiline(&mut ed.desc).desired_width(320.0).desired_rows(3))
            .on_hover_text("Description shown for this field under this derived register specifically — falls back to the field's own description when empty.")
            .lost_focus() { auto_apply = true; }

        ui.add_space(6.0);
        if let Some(err) = edit_err {
            ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
        }

        if !auto_apply || ed.is_unchanged() {
            return None;
        }
        Some(ed.build_action())
    }

    /// Modal windos to add a register
    /// Return an EditAction only when actually adding a valid register
    pub fn show_add_register_modal(ui: &mut egui::Ui, editor: &mut Option<RegAddEditor>, edit_err: &Option<String>) -> Option<EditAction> {
        let Some(ed) = editor else { return None; };
        let mut action = None;
        let mut cancel = false;
        egui::Window::new("Add register")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.label("Kind");
                    let selected_text = match &ed.kind {
                        AddRegKind::New => "New register".to_owned(),
                        AddRegKind::Instance { type_name } => format!("New instance of '{type_name}'"),
                    };
                    egui::ComboBox::from_id_salt("add_reg_kind")
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(matches!(ed.kind, AddRegKind::New), "New register").clicked()
                                && !matches!(ed.kind, AddRegKind::New)
                            {
                                ed.kind = AddRegKind::New;
                                ed.name = (0..).map(|i| if i == 0 { "new_reg".to_owned() } else { format!("new_reg_{i}") })
                                    .find(|n| !ed.page_types.contains(n)).expect("infinite iterator");
                            }
                            // A new instance of an existing type only makes sense once the page
                            // is manual — automatic mode is strictly one instance per definition
                            // (see the design review); offering it while still automatic would
                            // just fail to resolve on recompile with a confusing error.
                            if !ed.page_is_auto {
                                for t in ed.page_types.clone() {
                                    let picked = matches!(&ed.kind, AddRegKind::Instance { type_name } if *type_name == t);
                                    if ui.selectable_label(picked, &t).clicked() && !picked {
                                        ed.name = format!("{t}_0");
                                        ed.kind = AddRegKind::Instance { type_name: t };
                                    }
                                }
                            }
                            ui.add_enabled_ui(false, |ui| {
                                let _ = ui.selectable_label(false, "From RIF... (not implemented yet)");
                            });
                        });
                    if ed.page_is_auto && ui.button("🔓 Convert to manual addressing").clicked() {
                        action = Some(EditAction::ConvertPageToManual {
                            rif_type: ed.rif_type.clone(),
                            page_name: ed.page_name.clone(),
                            compiled: Box::new(ed.page_compiled.clone()),
                        });
                    };
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(if matches!(ed.kind, AddRegKind::New) { "Name" } else { "Instance name" });
                    ui.text_edit_singleline(&mut ed.name);
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        ed.parse_err = None;
                        let name = ed.name.trim().to_owned();
                        if name.is_empty() {
                            ed.parse_err = Some("Name cannot be empty".to_owned());
                        } else if ed.page_types.iter().any(|t| t == &name) {
                            ed.parse_err = Some(format!("A register named '{name}' already exists on this page"));
                        } else {
                            action = Some(EditAction::AddRegister {
                                rif_type: ed.rif_type.clone(),
                                page_name: ed.page_name.clone(),
                                kind: ed.kind.clone(),
                                name,
                                addr: ed.addr,
                            });
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if let Some(err) = &ed.parse_err {
                        ui.label(RichText::new(err).color(egui::Color32::RED));
                    } else if let Some(err) = edit_err {
                        ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
                    }
                });
            });
        if cancel {
            *editor = None;
        }
        action
    }

    pub fn flush_reg_intr_desc_editor(&mut self) {
        if let Some(ed) = self.reg_intr_desc_editor.as_ref()
            && !ed.is_unchanged()
        {
            self.pending = Some(ed.build_action());
            self.apply_pending();
        }
    }

    pub fn flush_field_intr_desc_editor(&mut self) {
        if let Some(ed) = self.field_intr_desc_editor.as_ref()
            && !ed.is_unchanged()
        {
            self.pending = Some(ed.build_action());
            self.apply_pending();
        }
    }

    /// Same idea for `reg_editor`, routed through `resolve_reg_commit` instead of a bare
    /// `ed.build_action()` — a pending address or array change that collides must pop the same
    /// confirmation the live per-frame auto-apply shows, not silently save stale data or let a
    /// doomed plain apply fail recompilation underneath the Save button. Unlike the live UI path,
    /// there's no selection-path traversal here to derive the compiled page/def from, so they're
    /// looked up by name instead (`find_compiled_rif`/`find_compiled_reg_page`/`find_regdef`).
    pub fn flush_reg_editor(&mut self) {
        let Some(ed) = self.reg_editor.as_ref() else { return; };
        if ed.is_unchanged() {
            return;
        }
        // Cloned up front: `get_rif`/`find_compiled_rif` tie their result's lifetime to the key
        // string's borrow, which would otherwise keep `ed` borrowed immutably right up until
        // `resolve_reg_commit` needs it mutably below.
        let (rif_type, reg_type, inst_name) = (ed.rif_type.clone(), ed.reg_type.clone(), ed.inst_name.clone());
        let src_page = self.rif_src.as_ref()
            .and_then(|src| get_rif(&src.rifs, &rif_type))
            .and_then(|rif| find_reg_page(rif, &reg_type));
        let Some(def_reg) = self.rif_src.as_ref()
            .and_then(|src| get_rif(&src.rifs, &rif_type))
            .and_then(|rif| find_regdef(rif, &reg_type))
        else { return; };
        let page_regs: &[RifRegInst] = self.rif_comp.as_ref()
            .and_then(|comp| Self::find_compiled_rif(comp, &rif_type))
            .and_then(|rif| Self::find_compiled_reg_page(rif, &inst_name))
            .map(|p| p.regs.as_slice())
            .unwrap_or(&[]);
        let mut conflict = None;
        let mut array_change = None;
        let ed = self.reg_editor.as_mut().expect("checked Some above");
        if let Some(action) = Self::resolve_reg_commit(ed, def_reg, page_regs, src_page, &mut conflict, &mut array_change) {
            self.pending = Some(action);
            self.apply_pending();
        } else {
            self.confirm_reg_addr_conflict = conflict;
            self.confirm_reg_array_change = array_change;
        }
    }

    /// Mirrors `flush_field_editor`/`flush_reg_intr_desc_editor`: give a still-focused
    /// `reg_override_editor` buffer one last chance to commit before `save_file` proceeds.
    pub fn flush_reg_override_editor(&mut self) {
        if let Some(ed) = self.reg_override_editor.as_mut()
            && !ed.is_unchanged()
            && let Some(action) = ed.build_action()
        {
            self.pending = Some(action);
            self.apply_pending();
        }
    }

    /// Mirrors `flush_reg_override_editor`: give a still-focused `rif_editor` buffer one last
    /// chance to commit before `save_file` proceeds.
    pub fn flush_rif_editor(&mut self) {
        if let Some(ed) = self.rif_editor.as_mut()
            && !ed.is_unchanged()
            && let Some(action) = ed.build_action()
        {
            self.pending = Some(action);
            self.apply_pending();
        }
    }
}
