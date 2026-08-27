use eframe::egui;
use egui::RichText;
use egui_extras::{Column, TableBuilder};
use yarig::comp::comp_inst::{FieldWidth, RifFieldInst, RifRegInst};
use yarig::rifgen::{Access, EnumDef, Field, InterruptClr, InterruptInfoField, InterruptTrigger, RegDef, RegOverride, ResetVal, ResetValP, Width};

use crate::enum_editor::{EnumEditor, EnumEntryRow};
use crate::field_editor::{EnumMode, EnumTypeSrc, FieldEditor, FieldOverrideEditor, LimitKind, VisMode};
use crate::apply_pending::EditAction;
use crate::{RifViewer, adv_label, intr_clear_label, intr_trigger_label, sw_kind_choices, sw_kind_label};

impl RifViewer {
    /// A field whose position/width the declaration-line editor can handle
    /// Currently not supported: partial field and field array
    pub fn is_field_editable(reg: &RifRegInst, field: &RifFieldInst, def_field: &Field) -> bool {
        !matches!(field.width, FieldWidth::Generic(_))
            && field.partial.0.is_none()
            && matches!(def_field.array, Width::Value(_))
            && (field.array.dim() == 0 || reg.array.opt_idx().is_none())
    }

    /// Ensure there is always at least one field in a register
    pub fn field_can_delete(regdef: &RegDef) -> bool {
        regdef.fields.len() > 1
    }

    /// A field whose bit position can be swapped with a sibling's.
    /// Field arrays already spanned multiple position and needs special treatement not done yet
    pub fn is_field_reorderable(field: &RifFieldInst) -> bool {
        field.array.dim() == 0
    }

    /// Render one field's whole-field override panel
    pub fn show_field_override_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<FieldOverrideEditor>,
        edit_err: &Option<String>,
        rif_type: &str,
        inst_name: &str,
        field: &RifFieldInst,
        reg_override: Option<&RegOverride>,
    ) -> Option<EditAction> {
        let key = (inst_name.to_owned(), field.name.clone());
        if editor.as_ref().map(|e| (e.inst_name.as_str(), e.field_name.as_str())) != Some((key.0.as_str(), key.1.as_str())) {
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
                && let Some(action) = old.build_action()
            {
                return Some(action);
            }
            let field_ovr = reg_override.and_then(|o| o.fields.get(&field.name));
            let field_reset = match &field.reset {
                ResetVal::Unsigned(v) => ResetValP::Unsigned(*v),
                ResetVal::Signed(v) => ResetValP::Signed(*v),
            };
            *editor = Some(FieldOverrideEditor::from_field_override(rif_type, inst_name, &field.name, field_ovr, field_reset));
        }
        let ed = editor.as_mut().expect("editor just set");

        let mut auto_apply = false;
        egui::Grid::new("field_override_editor_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Description");
            ui.add_enabled_ui(!ed.desc_locked, |ui| {
                if ui.add(egui::TextEdit::singleline(&mut ed.desc).desired_width(280.0)).lost_focus() { auto_apply = true; }
            }).response.on_hover_text(if ed.desc_locked {
                "This override already has a multi-line description block, which isn't editable here yet."
            } else {
                "Override this field's description for this instance only."
            });
            ui.end_row();

            ui.label("Reset / Disable");
            ui.horizontal(|ui| {
                if ui.checkbox(&mut ed.disable_on, "disable").changed() { auto_apply = true; }
                if ui.add(egui::TextEdit::singleline(&mut ed.reset_val).desired_width(80.0))
                    .on_hover_text(if ed.disable_on {
                        "Value to force this field to; blank uses the field's own reset value."
                    } else {
                        "Override this field's reset value for this instance only; blank = no override."
                    })
                    .lost_focus() { auto_apply = true; }
            });
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

    /// Render the editable field panel.
    #[allow(clippy::too_many_arguments)]
    pub fn show_field_editor(
        ui: &mut egui::Ui,
        editor: &mut Option<FieldEditor>,
        edit_err: &Option<String>,
        reg_type: &str,
        group_type: &str,
        rif_type: &str,
        field: &RifFieldInst,
        def_field: &Field,
        enum_defs: &[EnumDef],
        enum_editor: &mut Option<EnumEditor>,
        can_delete: bool,
        reg_intr_default: Option<InterruptInfoField>,
        // True when this field's array is split across sibling registers
        array_partial_locked: bool,
    ) -> Option<EditAction> {
        // (Re)load the buffers when they no longer belong to the selected field.
        let key = (reg_type.to_owned(), field.name.clone());
        if editor.as_ref().map(|e| &e.key) != Some(&key) {
            if let Some(old) = editor.as_mut()
                && !old.is_unchanged()
                && let Some(action) = old.build_action()
            {
                return Some(action);
            }
            *editor = Some(FieldEditor::from_field(key, field, def_field, enum_defs, rif_type, group_type, reg_intr_default));
        }
        let ed = editor.as_mut().expect("editor just set");

        if array_partial_locked {
            ui.label(RichText::new(
                "⚠ This field's array continues in another register (arrayPartial) — name, \
                 array size and position are locked here; edit the group's other half to match, \
                 or coordinate the change manually."
            ).weak().italics());
        }
        ui.add_space(6.0);

        // Set whenever a buffer is blurred (typed text: `lost_focus()`) or picked (checkbox/combo:
        // `changed()`) — edits commit as soon as the user moves on to something else, no explicit
        // "Apply" needed.
        let mut auto_apply = false;
        egui::Grid::new("field_editor_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Name");
            ui.add_enabled_ui(!array_partial_locked, |ui| {
                ui.horizontal(|ui| {
                    if ui.text_edit_singleline(&mut ed.name).lost_focus() { auto_apply = true; }
                    let is_array = ed.array.trim().parse::<u8>().map(|n| n > 0).unwrap_or(false);
                    if is_array {
                        ui.label("array");
                        if ui.add(egui::TextEdit::singleline(&mut ed.array).desired_width(40.0))
                            .on_hover_text("Array size; clear or set to 0 to remove the array")
                            .lost_focus() { auto_apply = true; }
                    } else if ui.button("Array").on_hover_text("Turn this field into an array").clicked() {
                        ed.array = "2".to_owned();
                        auto_apply = true;
                    }
                });
            });
            ui.end_row();

            ui.label("Position");
            ui.add_enabled_ui(!array_partial_locked, |ui| {
                ui.horizontal(|ui| {
                    ui.label("lsb");
                    if ui.add(egui::TextEdit::singleline(&mut ed.lsb).desired_width(40.0)).lost_focus() { auto_apply = true; }
                    ui.label("width");
                    if ui.add(egui::TextEdit::singleline(&mut ed.width).desired_width(40.0)).lost_focus() { auto_apply = true; }
                    ui.label("posIncr");
                    if ui.add(egui::TextEdit::singleline(&mut ed.array_pos_incr).desired_width(40.0))
                        .on_hover_text("Bit-position increment between array elements; blank/0 follows the field width")
                        .lost_focus() { auto_apply = true; }
                });
            });
            ui.end_row();

            ui.label("Access");
            ui.horizontal(|ui| {
                ui.label("SW");
                egui::ComboBox::from_id_salt("field_access")
                    .selected_text(sw_kind_label(&ed.sw_kind))
                    .show_ui(ui, |ui| {
                        for (kind, label) in sw_kind_choices() {
                            if ui.selectable_value(&mut ed.sw_kind, kind, label).changed() { auto_apply = true; }
                        }
                    });
                ui.label("HW");
                egui::ComboBox::from_id_salt("field_hw_acc")
                    .selected_text(format!("{}", ed.hw_acc))
                    .show_ui(ui, |ui| {
                        for acc in [Access::NA, Access::RO, Access::WO, Access::RW] {
                            if ui.selectable_value(&mut ed.hw_acc, acc, format!("{acc}")).changed() { auto_apply = true; }
                        }
                    });
            });
            ui.end_row();

            ui.label(if ed.signed {"Reset (signed)"} else {"Reset"});
            ui.horizontal(|ui| {
                let is_array = ed.array.trim().parse::<u8>().map(|n| n > 0).unwrap_or(false);
                let mut reset_box = ui.add(egui::TextEdit::singleline(&mut ed.reset).desired_width(120.0));
                if is_array {
                    reset_box = reset_box.on_hover_text(
                        "For an array field, this value applies to every element, replacing any \
                         existing per-element list. Editing individual elements isn't supported here."
                    );
                }
                if reset_box.lost_focus() { auto_apply = true; }
                if ui.checkbox(&mut ed.signed, "signed")
                    .on_hover_text("Interpret the value as signed (adds/removes the `signed` property)")
                    .changed() { auto_apply = true; }
            });
            ui.end_row();

            ui.label("Description");
            if ui.add(egui::TextEdit::multiline(&mut ed.desc).desired_width(320.0).desired_rows(3))
                .on_hover_text("First line is the inline (short) description; further lines become a `description:` block.")
                .lost_focus() { auto_apply = true; }
            ui.end_row();
        });

        // Advanced sub-properties (each lives on its own line under the declaration)
        egui::CollapsingHeader::new("Advanced properties")
            .id_salt(("field_adv_properties", &ed.key))
            .default_open(false).show(ui, |ui| {
            egui::Grid::new("field_editor_adv_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Visibility");
                egui::ComboBox::from_id_salt("field_visibility")
                    .selected_text(ed.visibility.label())
                    .show_ui(ui, |ui| {
                        for v in [VisMode::Full, VisMode::Hidden, VisMode::Reserved] {
                            if ui.selectable_value(&mut ed.visibility, v, v.label()).changed() { auto_apply = true; }
                        }
                    });
                ui.end_row();

                ui.label("Fractional bits");
                if ui.add(egui::TextEdit::singleline(&mut ed.nb_frac).desired_width(60.0))
                    .on_hover_text("Number of fractional bits (nb_frac); empty or 0 to omit")
                    .lost_focus() { auto_apply = true; }
                ui.end_row();

                adv_label(ui, "Enum", ed.enum_mode != EnumMode::None);
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("field_enum_mode")
                        .selected_text(ed.enum_mode.label())
                        .show_ui(ui, |ui| {
                            for m in [EnumMode::None, EnumMode::Doc, EnumMode::Type] {
                                if ui.selectable_value(&mut ed.enum_mode, m, m.label()).changed() { auto_apply = true; }
                            }
                        });
                    let width = ed.width.trim().parse::<u8>().unwrap_or(1);
                    let has_frac = ed.nb_frac.trim().parse::<isize>().map(|v| v > 0).unwrap_or(false);
                    match ed.enum_mode {
                        EnumMode::None => {}
                        EnumMode::Doc => {
                            let name = format!("doc:{group_type}_{}", ed.name.trim());
                            let exists = enum_defs.iter().any(|d| d.name == name);
                            if ui.button(if exists { "Edit" } else { "New" }).clicked() {
                                *enum_editor = Some(EnumEditor::open(rif_type, name, enum_defs, has_frac, width));
                            }
                        }
                        EnumMode::Type => {
                            if enum_defs.iter().any(|d| d.is_local_type()) {
                                egui::ComboBox::from_id_salt("field_enum_type_picker")
                                    .selected_text(if ed.enum_type_src == EnumTypeSrc::New { "New".to_owned() } else { ed.enum_name.clone() })
                                    .show_ui(ui, |ui| {
                                        for d in enum_defs.iter().filter(|d| d.is_local_type()) {
                                            let picked = ed.enum_type_src == EnumTypeSrc::Existing && ed.enum_name == d.name;
                                            if ui.selectable_label(picked, &d.name).clicked() {
                                                ed.enum_type_src = EnumTypeSrc::Existing;
                                                ed.enum_name = d.name.clone();
                                                auto_apply = true;
                                            }
                                        }
                                    }).response
                                    .on_hover_text("Picking a type here applies immediately (then \"💾 Save\" writes it to disk).");
                            }
                            let candidate = if ed.enum_name.trim().is_empty() {
                                format!("e_{group_type}_{}", ed.name.trim())
                            } else {
                                ed.enum_name.trim().to_owned()
                            };
                            let exists = enum_defs.iter().any(|d| d.name == candidate);
                            if ui.button(if exists { "Edit" } else { "New" }).clicked() {
                                *enum_editor = Some(EnumEditor::open(rif_type, candidate, enum_defs, has_frac, width));
                            }
                        }
                    }
                });
                ui.end_row();

                adv_label(ui, "Lock", ed.lock_on);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut ed.lock_on, "")
                        .on_hover_text("Check to lock this field's writes")
                        .changed() { auto_apply = true; }
                    if ui.add_enabled(ed.lock_on, egui::TextEdit::singleline(&mut ed.lock).desired_width(220.0))
                        .on_hover_text("Signal/expression that locks writes (e.g. `other_field`)")
                        .lost_focus() { auto_apply = true; }
                });
                ui.end_row();

                adv_label(ui, "Limit", ed.limit_kind != LimitKind::None);
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("field_limit_kind")
                        .selected_text(ed.limit_kind.label())
                        .show_ui(ui, |ui| {
                            for k in [LimitKind::None, LimitKind::Range, LimitKind::List, LimitKind::Enum, LimitKind::External] {
                                if ui.selectable_value(&mut ed.limit_kind, k, k.label()).changed() { auto_apply = true; }
                            }
                        });
                    match ed.limit_kind {
                        LimitKind::Range => {
                            ui.label("min");
                            if ui.add(egui::TextEdit::singleline(&mut ed.limit_min).desired_width(60.0)).lost_focus() { auto_apply = true; }
                            ui.label("max");
                            if ui.add(egui::TextEdit::singleline(&mut ed.limit_max).desired_width(60.0)).lost_focus() { auto_apply = true; }
                        }
                        LimitKind::List => {
                            if ui.add(egui::TextEdit::singleline(&mut ed.limit_list).desired_width(160.0))
                                .on_hover_text("Comma-separated values, e.g. 1,2,4,8")
                                .lost_focus() { auto_apply = true; }
                        }
                        LimitKind::None | LimitKind::Enum | LimitKind::External => {}
                    }
                    ui.label("Bypass");
                    if ui.add(egui::TextEdit::singleline(&mut ed.limit_bypass).desired_width(120.0))
                        .on_hover_text("Signal name that bypasses the limit check; blank for none")
                        .lost_focus() { auto_apply = true; }
                });
                ui.end_row();

                adv_label(ui, "Counter", ed.counter_on);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut ed.counter_on, "")
                        .on_hover_text("Check to make this a counter field")
                        .changed() { auto_apply = true; }
                    ui.label("up");
                    if ui.add_enabled(ed.counter_on, egui::TextEdit::singleline(&mut ed.counter_up).desired_width(50.0))
                        .on_hover_text("Increment value; blank disables up-counting")
                        .lost_focus() { auto_apply = true; }
                    ui.label("down");
                    if ui.add_enabled(ed.counter_on, egui::TextEdit::singleline(&mut ed.counter_down).desired_width(50.0))
                        .on_hover_text("Decrement value; blank disables down-counting")
                        .lost_focus() { auto_apply = true; }
                    if ui.add_enabled(ed.counter_on, egui::Checkbox::new(&mut ed.counter_sat, "sat")).changed() { auto_apply = true; }
                    if ui.add_enabled(ed.counter_on, egui::Checkbox::new(&mut ed.counter_event, "event")).changed() { auto_apply = true; }
                    if ui.add_enabled(ed.counter_on, egui::Checkbox::new(&mut ed.counter_clr, "clr")).changed() { auto_apply = true; }
                });
                ui.end_row();

                adv_label(ui, "Password", ed.password_on);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut ed.password_on, "")
                        .on_hover_text("Check to make this a password field")
                        .changed() { auto_apply = true; }
                    ui.label("once");
                    if ui.add_enabled(ed.password_on, egui::TextEdit::singleline(&mut ed.password_once).desired_width(60.0)).lost_focus() { auto_apply = true; }
                    ui.label("hold");
                    if ui.add_enabled(ed.password_on, egui::TextEdit::singleline(&mut ed.password_hold).desired_width(60.0)).lost_focus() { auto_apply = true; }
                    if ui.add_enabled(ed.password_on, egui::Checkbox::new(&mut ed.password_protect, "protect")).changed() { auto_apply = true; }
                });
                ui.end_row();

                // Only shown for a field of the base/primary interrupt register — not for a
                // plain register (no interrupt at all) or a derived one (routed through
                // `show_field_intr_desc_editor` instead, which has no trigger control at all).
                if ed.reg_intr_default.is_some() {
                    adv_label(ui, "Interrupt", ed.intr_trigger_ovr_on || ed.intr_clear_ovr_on);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut ed.intr_trigger_ovr_on, "trigger")
                            .on_hover_text("Override the register's default trigger for this field")
                            .changed() { auto_apply = true; }
                        ui.add_enabled_ui(ed.intr_trigger_ovr_on, |ui| {
                            egui::ComboBox::from_id_salt("field_intr_trigger_ovr")
                                .selected_text(intr_trigger_label(ed.intr_trigger_ovr))
                                .show_ui(ui, |ui| {
                                    for t in [InterruptTrigger::High, InterruptTrigger::Low, InterruptTrigger::Rising, InterruptTrigger::Falling, InterruptTrigger::Edge] {
                                        if ui.selectable_value(&mut ed.intr_trigger_ovr, t, intr_trigger_label(t)).changed() { auto_apply = true; }
                                    }
                                });
                        });
                        if ui.checkbox(&mut ed.intr_clear_ovr_on, "clear")
                            .on_hover_text("Override the register's default clear method for this field")
                            .changed() { auto_apply = true; }
                        ui.add_enabled_ui(ed.intr_clear_ovr_on, |ui| {
                            egui::ComboBox::from_id_salt("field_intr_clear_ovr")
                                .selected_text(intr_clear_label(ed.intr_clear_ovr))
                                .show_ui(ui, |ui| {
                                    for c in [InterruptClr::Read, InterruptClr::Write0, InterruptClr::Write1, InterruptClr::Hw] {
                                        if ui.selectable_value(&mut ed.intr_clear_ovr, c, intr_clear_label(c)).changed() { auto_apply = true; }
                                    }
                                });
                        });
                    });
                    ui.end_row();
                }
            });
        });

        ui.add_space(6.0);
        let mut delete = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(can_delete, |ui| {
                if ui.button(RichText::new("🗑 Delete")).clicked() {
                    delete = true;
                }
            }).response.on_hover_text(if can_delete {
                "Delete this field"
            } else {
                "A register must keep at least one field"
            });
            if let Some(err) = &ed.parse_err {
                ui.label(RichText::new(err).color(egui::Color32::RED));
            } else if let Some(err) = edit_err {
                ui.label(RichText::new(format!("Compile error: {err}")).color(egui::Color32::RED));
            }
        });

        if delete {
            return Some(EditAction::DeleteField {
                rif_type: rif_type.to_owned(),
                reg_type: reg_type.to_owned(),
                field_name: ed.key.1.clone(),
            });
        }
        if !auto_apply {
            return None;
        }
        ed.build_action()
    }

    /// Render the enum entry-table modal
    pub fn show_enum_editor(ui: &mut egui::Ui, editor: &mut Option<EnumEditor>, edit_err: &Option<String>, enum_defs: &[EnumDef]) -> Option<EditAction> {
        let Some(ed) = editor else { return None; };
        let mut action = None;
        let mut cancel = false;
        egui::Window::new("Edit enum definition")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                // Doc enums are auto-named from the field and are never referenced by a stored
                // name, so renaming one here would just orphan it — lock the box in that case.
                let doc_locked = ed.name.starts_with("doc:");
                ui.horizontal(|ui| {
                    ui.label("Type name");
                    ui.add_enabled(!doc_locked, egui::TextEdit::singleline(&mut ed.name).desired_width(200.0))
                        .on_hover_text(if doc_locked {
                            "Doc enums are named automatically from the field and can't be renamed"
                        } else {
                            "Name of this enum definition; renaming targets a different (or new) definition on Save"
                        });
                });
                ui.add_space(6.0);
                let mut remove_idx = None;
                let mut table = TableBuilder::new(ui)
                    .striped(true)
                    .column(Column::auto().at_least(90.0))
                    .column(Column::auto().at_least(50.0));
                if ed.has_frac {
                    table = table.column(Column::auto().at_least(70.0));
                }
                table = table.column(Column::auto().at_least(160.0)).column(Column::auto().at_least(20.0));
                table
                    .header(20.0, |mut header| {
                        header.col(|ui| { ui.strong("Name"); });
                        header.col(|ui| { ui.strong("Value"); });
                        if ed.has_frac {
                            header.col(|ui| { ui.strong("Repr"); });
                        }
                        header.col(|ui| { ui.strong("Description"); });
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for (i, row) in ed.rows.iter_mut().enumerate() {
                            body.row(18.0, |mut r| {
                                r.col(|ui| { ui.text_edit_singleline(&mut row.name); });
                                r.col(|ui| { ui.add(egui::TextEdit::singleline(&mut row.value).desired_width(50.0)); });
                                if ed.has_frac {
                                    r.col(|ui| { ui.add(egui::TextEdit::singleline(&mut row.repr).desired_width(70.0)); });
                                }
                                r.col(|ui| { ui.text_edit_singleline(&mut row.desc); });
                                r.col(|ui| {
                                    if ui.small_button("✖").clicked() {
                                        remove_idx = Some(i);
                                    }
                                });
                            });
                        }
                        // Discoverable "add entry" affordance, mirroring the field table's ghost row
                        body.row(18.0, |mut r| {
                            r.col(|ui| {
                                let lbl = RichText::new("➕ add entry").weak();
                                if ui.selectable_label(false, lbl).clicked() {
                                    ed.rows.push(EnumEntryRow::next_default(&ed.rows));
                                }
                            });
                            r.col(|_| {});
                            if ed.has_frac {
                                r.col(|_| {});
                            }
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
                        let target_name = ed.name.trim().to_owned();
                        if target_name.is_empty() {
                            ed.parse_err = Some("Enum name cannot be empty".to_owned());
                        } else if target_name != ed.orig_name && enum_defs.iter().any(|d| d.name == target_name) {
                            ed.parse_err = Some(format!("An enum named '{target_name}' already exists"));
                        } else {
                            match ed.build_values() {
                                Ok(values) => {
                                    ed.parse_err = None;
                                    action = Some(EditAction::UpdateEnum {
                                        rif_type: ed.rif_type.clone(),
                                        name: target_name,
                                        values,
                                    });
                                }
                                Err(e) => ed.parse_err = Some(e),
                            }
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

    pub fn flush_field_editor(&mut self) {
        if let Some(ed) = self.field_editor.as_mut()
            && !ed.is_unchanged()
            && let Some(action) = ed.build_action()
        {
            self.pending = Some(action);
            self.apply_pending();
        }
    }

    pub fn flush_field_override_editor(&mut self) {
        if let Some(ed) = self.field_override_editor.as_mut()
            && !ed.is_unchanged()
            && let Some(action) = ed.build_action()
        {
            self.pending = Some(action);
            self.apply_pending();
        }
    }
}
