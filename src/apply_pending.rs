use std::collections::HashSet;
use std::path::PathBuf;

use yarig::comp::comp_inst::RifPageInst;
use yarig::parser::{get_rif, get_rif_mut, parser_expr::ExprTokens, remove_rif, RifGenTop};
use yarig::rifgen::{
    order_dict::OrderDict,
    Address, AddressKind, AddressOffset, ClockingInfo, Description, EnumDef, EnumEntry,
    Field, FieldHwKind, FieldOverride, FieldOverrideProp, FieldPos, FieldProp, FieldSwKind,
    InstMode, InterruptInfo, InterruptInfoField, InterruptRegKind, RegDef, RegDefOrIncl,
    RegInst, RegOverride, RegOverrideProp, RegProp, ResetValOverride, ResetValP, Rif, RifPage,
    RifProp, RifType, Visibility, Width, fmt_generic_line, fmt_param_line,
};

use crate::field_editor::{EnumMode, EnumTypeSrc, FieldOverrideVals, FieldVals};
use crate::reg_editor::{AddRegKind, RegDefVals, RegOverrideVals};
use crate::rif_editor::{GenericEntry, ParamEntry, RifDefVals};
use crate::select::SelectedItem;
use crate::RifViewer;

/// Location of a register instance within a `Rif`, together with a snapshot of its address —
/// used to both find an instance by name and to back up/restore its address around a recompile.
#[derive(Clone)]
struct RegInstLoc {
    page_idx: usize,
    inst_idx: usize,
    addr: Address,
}

impl RegInstLoc {
    /// Find the instance named `inst_name`
    fn find(rif: &Rif, inst_name: &str) -> Option<Self> {
        rif.pages.iter().enumerate().find_map(|(page_idx, page)| {
            page.instances.iter().position(|i| i.inst_name == inst_name).map(|inst_idx| {
                RegInstLoc { page_idx, inst_idx, addr: page.instances[inst_idx].addr.clone() }
            })
        })
    }
}

/// A `Rifmux` item whose `rif_type` referenced a just-renamed RIF, with its previous value so a
/// failed recompile can restore it.
struct RifmuxItemBackup {
    mux_name: String,
    idx: usize,
    old_type: RifType,
}

/// Everything a RIF rename touches besides the `Rif` itself (`RifGenTop`, `Rifmux` items,
/// `paths`), backed up so a failed recompile can undo the rename verbatim.
struct RifRenameBackup {
    top_was_this: bool,
    item_backups: Vec<RifmuxItemBackup>,
    path_backup: Option<(String, PathBuf)>,
}

/// Everything `apply_regdef_vals` computes from a `RegDefVals` before recompiling with a backup for rollback
struct UpdateRegDefOutcome {
    backup: RegDef,
    src_line: Option<usize>,
    decl_changed: bool,
    prop_bodies: Vec<(RegProp, Option<String>)>,
    desc_block_edit: Option<Option<Vec<String>>>,
    /// `(page index, instance index, previous value)` for the target instance.
    inst_backups: Vec<(usize, usize, RegInst)>,
    inst_line_changed: Option<usize>,
    new_name: String,
    /// Register-level `{enable,mask,pending}.description:` blocks to delete, when the primary interrupt was removed
    intr_desc_removed: Vec<InterruptRegKind>,
    /// Same, per field that carried its own override — `(field's decl line, kind)`.
    field_intr_desc_removed: Vec<(usize, InterruptRegKind)>,
    /// Declaration lines of fields whose own `interrupt ...` trigger/clear override line must be deleted
    field_intr_ovr_removed: Vec<usize>,
    /// Declaration lines of fields property modified
    field_lines_changed: Vec<usize>,
}

/// Everything `apply_rif_vals` computes from a `RifDefVals` before recompiling with a backup for rollback
struct UpdateRifDefOutcome {
    backup: Rif,
    src_line: Option<usize>,
    decl_changed: bool,
    prop_bodies: Vec<(RifProp, Option<String>)>,
    desc_block_edit: Option<Option<Vec<String>>>,
    new_name: String,
}

/// Mutate `rif` in place from `vals` and report what changed.
fn apply_rif_vals(rif: &mut Rif, vals: &RifDefVals) -> UpdateRifDefOutcome {
    let before = rif.clone();
    let src_line = rif.src.decl_line;
    let orig_decl = rif.fmt_decl();
    rif.name = vals.name.clone();
    rif.addr_width = vals.addr_width;
    rif.data_width = vals.data_width;
    rif.interface = vals.interface.clone();
    let orig_desc_block = rif.fmt_desc_block("");
    rif.description.set_public(&vals.desc);
    let new_desc_block = rif.fmt_desc_block("");
    let desc_block_edit = (new_desc_block != orig_desc_block).then_some(new_desc_block);
    let decl_changed = rif.fmt_decl() != orig_decl;
    let mut prop_bodies: Vec<(RifProp, Option<String>)> = Vec::new();
    for prop in RifProp::ALL {
        let before_body = before.fmt_prop(prop, "");
        let after_body = rif.fmt_prop(prop, "");
        if before_body != after_body {
            prop_bodies.push((prop, after_body));
        }
    }
    UpdateRifDefOutcome { backup: before, src_line, decl_changed, prop_bodies, desc_block_edit, new_name: vals.name.clone() }
}

/// A pending edit to apply to `RifGenSrc` after the UI closure returns.
pub enum EditAction {
    UpdateField { rif_type: String, reg_type: String, orig_name: String, vals: Box<FieldVals> },
    AddField { rif_type: String, reg_type: String },
    DeleteField { rif_type: String, reg_type: String, field_name: String },
    /// Swap two adjacent fields' bit positions: each `(field name, new position)`.
    MoveField { rif_type: String, reg_type: String, moves: Vec<(String, FieldPos)> },
    /// Replace an enum definition's entries wholesale (creating the definition if it doesn't
    /// exist yet), from the entry-table modal.
    UpdateEnum { rif_type: String, name: String, values: Vec<EnumEntry> },
    /// Update a register type's name/description/access-pulse, and/or the specific instance name
    UpdateRegDef { rif_type: String, orig_name: String, inst_name: String, vals: RegDefVals },
    /// Convert a page from automatic to explicit manual addressing
    ConvertPageToManual { rif_type: String, page_name: String, compiled: Box<RifPageInst>},
    /// Swap two instances' addresses outright
    SwapRegAddr { rif_type: String, a_inst_name: String, b_inst_name: String },
    /// Resolve a register-address collision by moving one or more *other* instances alongside the primary edit
    UpdateRegDefWithMoves { rif_type: String, orig_name: String, inst_name: String, vals: RegDefVals, companions: Vec<(String, u64)> },
    /// Set (or clear) one specific manual instance's own array size
    UpdateRegInstArray { rif_type: String, inst_name: String, array: Option<ExprTokens>, companions: Vec<(String, u64)> },
    /// Create a register: a brand-new definition or a new instance of an existing type
    AddRegister { rif_type: String, page_name: String, kind: AddRegKind, name: String, addr: u64 },
    /// Remove a register instance or the whole definition (`inst_name: None).
    DeleteRegister { rif_type: String, page_name: String, reg_type: String, inst_name: Option<String> },
    /// Replace a derived (enable/mask/pending) interrupt register's own description
    UpdateRegIntrDesc { rif_type: String, reg_type: String, kind: InterruptRegKind, desc: String },
    /// Same as `UpdateRegIntrDesc` at field level
    UpdateFieldIntrDesc { rif_type: String, reg_type: String, field_name: String, kind: InterruptRegKind, desc: String },
    /// Update a register instance's whole-register override
    UpdateRegOverride { rif_type: String, inst_name: String, vals: RegOverrideVals },
    /// Same as `UpdateRegOverride`, at field level
    UpdateFieldOverride { rif_type: String, inst_name: String, field_name: String, vals: FieldOverrideVals },
    /// Update the Rif's own name/address width/data width/description
    UpdateRifDef { rif_type: String, vals: RifDefVals },
    /// Change the Rif's `sw_clocking`/`hw_clocking`
    UpdateRifClocking { rif_type: String, sw: Vec<ClockingInfo>, hw: Vec<ClockingInfo> },
    /// Update parameters/generics definition
    UpdateRifParamsAndGenerics {
        rif_type: String,
        params: Vec<ParamEntry>,
        generics: Vec<GenericEntry>,
    },
}

impl EditAction {
    /// The RIF type this action edits
    fn rif_type(&self) -> &str {
        match self {
            EditAction::UpdateField { rif_type, .. }
            | EditAction::AddField { rif_type, .. }
            | EditAction::DeleteField { rif_type, .. }
            | EditAction::MoveField { rif_type, .. }
            | EditAction::UpdateEnum { rif_type, .. }
            | EditAction::UpdateRegDef { rif_type, .. }
            | EditAction::ConvertPageToManual { rif_type, .. }
            | EditAction::SwapRegAddr { rif_type, .. }
            | EditAction::UpdateRegDefWithMoves { rif_type, .. }
            | EditAction::UpdateRegInstArray { rif_type, .. }
            | EditAction::AddRegister { rif_type, .. }
            | EditAction::DeleteRegister { rif_type, .. }
            | EditAction::UpdateRegIntrDesc { rif_type, .. }
            | EditAction::UpdateFieldIntrDesc { rif_type, .. }
            | EditAction::UpdateRegOverride { rif_type, .. }
            | EditAction::UpdateFieldOverride { rif_type, .. }
            | EditAction::UpdateRifDef { rif_type, .. }
            | EditAction::UpdateRifClocking { rif_type, .. }
            | EditAction::UpdateRifParamsAndGenerics { rif_type, .. } => rif_type,
        }
    }
}

/// Clock/ClockEnable/Clear property to edit in a register
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RifClockLineKind { SwClock, HwClock, SwClkEn, HwClkEn, SwClear, HwClear }

/// Find a register definition by type name across all pages of a RIF.
fn find_regdef_mut<'a>(rif: &'a mut Rif, reg_type: &str) -> Option<&'a mut RegDef> {
    rif.pages.iter_mut()
        .flat_map(|p| p.registers.iter_mut())
        .filter_map(|r| r.get_regdef_mut())
        .find(|d| d.name == reg_type)
}

/// Find a field definition by name inside a register type.
fn find_field_mut<'a>(rif: &'a mut Rif, reg_type: &str, field: &str) -> Option<&'a mut Field> {
    find_regdef_mut(rif, reg_type)?
        .fields.iter_mut()
        .find(|f| f.name == field)
}

/// Generate a field name not already present in the register definition.
fn unique_field_name(regdef: &RegDef) -> String {
    let taken = |n: &str| regdef.fields.iter().any(|f| f.name == n);
    if !taken("new_field") {
        return "new_field".to_owned();
    }
    (1..).map(|i| format!("new_field_{i}")).find(|n| !taken(n)).unwrap()
}

impl RifViewer {
    /// Everything `UpdateRegDef`/`UpdateRegDefWithMoves` compute from a `RegDefVals` before
    /// recompiling — the one piece of bookkeeping both `EditAction`s share verbatim.
    fn apply_regdef_vals(rif: &mut Rif, orig_name: &str, inst_name: &str, vals: &RegDefVals) -> Option<UpdateRegDefOutcome> {
        let new_name = vals.name.clone();
        let renaming = new_name != orig_name;
        // Keep a backup so a failed recompile leaves the source untouched
        let mut backup: Option<RegDef> = None;
        let mut src_line = None;
        let mut decl_changed = false;
        let mut prop_bodies: Vec<(RegProp, Option<String>)> = Vec::new();
        let mut desc_block_edit: Option<Option<Vec<String>>> = None;
        // Instance backup for rollback in case of issue
        let mut inst_backups: Vec<(usize, usize, RegInst)> = Vec::new();
        // Flags when some updated are applied
        let mut inst_line_changed: Option<usize> = None;
        let mut intr_desc_removed: Vec<InterruptRegKind> = Vec::new();
        let mut field_intr_desc_removed: Vec<(usize, InterruptRegKind)> = Vec::new();
        let mut field_intr_ovr_removed: Vec<usize> = Vec::new();
        let mut field_lines_changed: Vec<usize> = Vec::new();
        if let Some(regdef) = find_regdef_mut(rif, orig_name) {
            let before = regdef.clone();
            src_line = regdef.src.decl_line;
            let orig_decl = regdef.fmt_decl("");
            match &vals.group {
                // Explicit edit from the "Group" field always wins.
                Some(new_group) => regdef.group.name = new_group.clone(),
                // Otherwise, an implicit group must be renamed
                None if regdef.group.pkg.is_none() && regdef.group.name == orig_name => {
                    regdef.group.name = vals.name.clone();
                }
                None => {}
            }
            regdef.name = vals.name.clone();
            let orig_short = regdef.description.get_short(false);
            let orig_desc_block = regdef.fmt_desc_block("");
            // Replace the whole public description (short line + block).
            regdef.description.set_public(&vals.desc);
            if regdef.description.get_short(false) != orig_short {
                regdef.src.has_inline_desc = true;
            }
            let new_desc_block = regdef.fmt_desc_block("");
            if new_desc_block != orig_desc_block {
                desc_block_edit = Some(new_desc_block);
            }
            regdef.pulse = vals.pulse.clone();
            regdef.visibility = vals.visibility;
            regdef.clk = vals.clk.clone();
            regdef.rst = vals.rst.clone();
            regdef.external = vals.external;
            // Primary interrupt settings: `
            //  - Some(Some(..))` update/creates the interrupt if
            //  - Some(None)` remove the interrupt kind from the register
            //  - None: leaves it untouched
            match &vals.intr {
                Some(Some(intr_vals)) => {
                    if regdef.interrupt.is_empty() {
                        regdef.interrupt.push(InterruptInfo::new("", (None, None, None, None, None)));
                    }
                    let intr = regdef.interrupt.first_mut().expect("just ensured present");
                    intr.trigger = intr_vals.trigger;
                    intr.clear = intr_vals.clear;
                    intr.enable = intr_vals.enable.clone();
                    intr.mask = intr_vals.mask.clone();
                    intr.pending = intr_vals.pending;
                    regdef.refresh_intr_defaults();
                }
                Some(None) if !regdef.interrupt.is_empty() => {
                    for kind in [InterruptRegKind::Enable, InterruptRegKind::Mask, InterruptRegKind::Pending] {
                        if regdef.fmt_intr_desc_block(kind, "").is_some() {
                            intr_desc_removed.push(kind);
                        }
                    }
                    for f in regdef.fields.iter_mut() {
                        if let Some(line) = f.src.decl_line {
                            for kind in [InterruptRegKind::Enable, InterruptRegKind::Mask, InterruptRegKind::Pending] {
                                if f.fmt_intr_desc_block(kind, "").is_some() {
                                    field_intr_desc_removed.push((line, kind));
                                }
                            }
                            if f.fmt_prop(FieldProp::Interrupt, "").is_some() {
                                field_intr_ovr_removed.push(line);
                            }
                        }
                        f.hw_kind.retain(|k| matches!(k, FieldHwKind::Counter(_)));
                        f.sw_kind = FieldSwKind::default();
                        f.intr_ovr = InterruptInfoField::default();
                        f.intr_desc = None;
                    }
                    regdef.interrupt.clear();
                }
                Some(None) | None => {}
            }
            // Same for secondary interrupt block
            if !regdef.interrupt.is_empty() {
                match &vals.alt {
                    Some(Some(alt_vals)) => {
                        if regdef.interrupt.len() < 2 {
                            regdef.interrupt.push(InterruptInfo::new(&alt_vals.name, (None, None, None, None, None)));
                        }
                        let alt = &mut regdef.interrupt[1];
                        alt.name = alt_vals.name.clone();
                        alt.trigger = alt_vals.trigger;
                        alt.clear = alt_vals.clear;
                        alt.enable = alt_vals.enable.clone();
                        alt.mask = alt_vals.mask.clone();
                        alt.pending = alt_vals.pending;
                    }
                    Some(None) => regdef.interrupt.truncate(1),
                    None => {}
                }
            }
            // Definition-level array dimension
            if let Some(new_dim) = vals.array {
                regdef.array = Width::Value(new_dim);
                if new_dim > 0 {
                    for f in regdef.fields.iter_mut() {
                        if matches!(f.array, Width::Value(0)) {
                            let orig_field_decl = f.fmt_decl("");
                            f.array = Width::Value(1);
                            if f.fmt_decl("") != orig_field_decl
                                && let Some(line) = f.src.decl_line
                            {
                                field_lines_changed.push(line);
                            }
                        }
                    }
                }
            }
            decl_changed = regdef.fmt_decl("") != orig_decl;
            for prop in RegProp::ALL {
                let after_body = regdef.fmt_prop(prop, "");
                if before.fmt_prop(prop, "") != after_body {
                    prop_bodies.push((prop, after_body));
                }
            }
            backup = Some(before);
        }
        if backup.is_some() {
            // Locate the specific instance currently being viewed
            let target = rif.pages.iter().enumerate()
                .find_map(|(pi, page)| page.instances.iter().position(|i| i.inst_name == inst_name).map(|ii| (pi, ii)));
            if let Some((tpi, tii)) = target {
                let inst = &mut rif.pages[tpi].instances[tii];
                inst_backups.push((tpi, tii, inst.clone()));
                let orig_inst_decl = inst.fmt_decl("");
                if renaming && inst.type_name == orig_name {
                    if inst.inst_name == orig_name {
                        inst.inst_name = new_name.clone();
                    }
                    inst.type_name = new_name.clone();
                }
                if let Some(new_addr) = vals.addr {
                    inst.addr = Address::new(AddressKind::Absolute, AddressOffset::Value(new_addr));
                }
                if let Some(line) = inst.src.decl_line
                    && inst.fmt_decl("") != orig_inst_decl
                {
                    inst_line_changed = Some(line);
                }
            }
            // Cascade the rename into every OTHER instance referencing the old type
            if renaming {
                for (pi, page) in rif.pages.iter_mut().enumerate() {
                    for (ii, inst) in page.instances.iter_mut().enumerate() {
                        if target == Some((pi, ii)) || inst.type_name != orig_name {
                            continue;
                        }
                        inst_backups.push((pi, ii, inst.clone()));
                        // An instance using mirroring its type name is renamed along with it.
                        if inst.inst_name == orig_name {
                            inst.inst_name = new_name.clone();
                        }
                        inst.type_name = new_name.clone();
                    }
                }
            }
        }
        backup.map(|backup| UpdateRegDefOutcome {
            backup, src_line, decl_changed, prop_bodies, desc_block_edit, inst_backups, inst_line_changed, new_name,
            intr_desc_removed, field_intr_desc_removed, field_intr_ovr_removed, field_lines_changed,
        })
    }

    /// Apply a pending edit to the source, recompile, and refresh the selection.
    pub fn apply_pending(&mut self) {
        let Some(action) = self.pending.take() else { return; };
        let rif_type_edited = action.rif_type().to_owned();
        let was_unsaved = self.has_unsaved();
        match action {
            EditAction::UpdateField { rif_type, reg_type, orig_name, vals } => {
                let new_name = vals.name.clone();
                // Keep a backup so a failed recompile leaves the source untouched
                let mut backup: Option<Field> = None;
                let mut src_line = None;
                let mut decl_changed = false;
                let mut prop_bodies: Vec<(FieldProp, Option<String>)> = Vec::new();
                let mut desc_block_edit: Option<Option<Vec<String>>> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(field) = find_field_mut(rif, &reg_type, &orig_name)
                {
                    backup = Some(field.clone());
                    src_line = field.src.decl_line;
                    let orig_decl = field.fmt_decl("");
                    let orig_short = field.description.get_short(false);
                    let orig_desc_block = field.fmt_desc_block("");
                    // Declaration-line properties
                    field.name = vals.name.clone();
                    field.pos = vals.pos.clone();
                    // Only overwrite the reset when it actually changed (preserves enum/param resets)
                    if let Some(r) = &vals.reset { field.reset = vec![r.clone()]; }
                    field.signed = vals.signed;
                    // Collapse a stale multi-element reset when array dimensio was changed
                    if field.array != vals.array && vals.reset.is_none() && field.reset.len() > 1 {
                        field.reset.truncate(1);
                    }
                    field.array = vals.array.clone();
                    field.array_pos_incr = vals.array_pos_incr;
                    // Replace the whole public description (short line + block)
                    field.description.set_public(&vals.desc);
                    if field.description.get_short(false) != orig_short {
                        field.src.has_inline_desc = true;
                    }
                    let new_desc_block = field.fmt_desc_block("");
                    if new_desc_block != orig_desc_block {
                        desc_block_edit = Some(new_desc_block);
                    }
                    // Advanced sub-properties
                    field.set_hw_acc(vals.hw_acc);
                    if let Some(v) = &vals.visibility { field.visibility = v.clone(); }
                    field.nb_frac = vals.nb_frac;
                    field.lock = vals.lock.clone();
                    field.limit = vals.limit.clone();
                    field.enum_kind = vals.enum_kind.clone();
                    // Counter lives in hw_kind: replace any existing counter entry
                    field.hw_kind.retain(|k| !matches!(k, FieldHwKind::Counter(_)));
                    if let Some(c) = &vals.counter { field.hw_kind.push(FieldHwKind::Counter(c.clone())); }
                    // Software kind / password (a password field overrides the basic combo kind)
                    if let Some(info) = &vals.password {
                        field.sw_kind = FieldSwKind::Password(info.clone());
                    } else {
                        field.sw_kind = vals.sw_kind.clone();
                    }
                    // Interrupt Trigger/clear override
                    if let Some(ovr) = &vals.intr_ovr {
                        field.set_intr_ovr(ovr.clone(), vals.reg_intr_default.clone());
                    }
                    decl_changed = field.fmt_decl("") != orig_decl;
                    // Canonical line bodies for the properties the user changed (from the mutated field)
                    for prop in &vals.changed {
                        prop_bodies.push((*prop, field.fmt_prop(*prop, "")));
                    }
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    // Record persistence edits for a parsed field.
                    if let Some(line) = src_line {
                        if decl_changed {
                            self.dirty.insert(line);
                        }
                        if !prop_bodies.is_empty() {
                            let entry = self.prop_edits                                .entry(line).or_default();
                            for (prop, body) in prop_bodies { entry.insert(prop, body); }
                        }
                        if let Some(body) = desc_block_edit {
                            self.desc_block_edits.insert(line, body);
                        }
                    }
                    // Follow the (possibly renamed) field and reload the buffers from it
                    if let SelectedItem::Field(n) = &self.selected.item {
                        let base = n.split('[').next().unwrap_or(n);
                        if base == orig_name {
                            self.selected.item = SelectedItem::Field(new_name);
                        }
                    }
                    self.field_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(field) = find_field_mut(rif, &reg_type, &new_name)
                {
                    // Roll back the mutation (field is currently named `new_name`)
                    *field = backup;
                }
            }
            EditAction::AddField { rif_type, reg_type } => {
                let mut new_name = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(regdef) = find_regdef_mut(rif, &reg_type)
                {
                    let name = unique_field_name(regdef);
                    let mut f = Field::new(
                        name.clone(),
                        vec![ResetValP::Unsigned(0)],
                        FieldPos::Size(Width::Value(1)),
                        Some(FieldSwKind::ReadWrite),
                        None,
                        "",
                    );
                    // Allow a later description edit to round-trip onto the declaration line
                    f.src.has_inline_desc = true;
                    regdef.add_field(f);
                    new_name = Some(name);
                }
                if let Some(name) = new_name {
                    self.recompile();
                    if self.edit_err.is_none() {
                        self.selected.item = SelectedItem::Field(name);
                        self.field_editor = None;
                    } else if let Some(src) = self.rif_src.as_mut() {
                        // Roll back the appended field so the source stays compilable
                        if let Some(regdef) = get_rif_mut(&mut src.rifs, &rif_type)
                            .and_then(|rif| find_regdef_mut(rif, &reg_type))
                        {
                            regdef.fields.retain(|f| f.name != name);
                        }
                    }
                }
            }
            EditAction::DeleteField { rif_type, reg_type, field_name } => {
                // Remove the field, keeping a backup + position to roll back on failure
                let mut removed: Option<(Field, usize)> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(regdef) = find_regdef_mut(rif, &reg_type)
                    && let Some(idx) = regdef.fields.iter().position(|f| f.name == field_name)
                {
                    let f = regdef.fields.remove(idx);
                    removed = Some((f, idx));
                }
                let Some((field, idx)) = removed else { return; };
                let src_line = field.src.decl_line;
                self.recompile();
                if self.edit_err.is_none() {
                    // A parsed field must have its source block removed on the next save
                    if let Some(line) = src_line {
                        self.deleted.insert(line);
                    }
                    self.selected.item = SelectedItem::None;
                    self.field_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(regdef) = find_regdef_mut(rif, &reg_type)
                {
                    // Deletion broke compilation => restore it.
                    let at = idx.min(regdef.fields.len());
                    regdef.fields.insert(at, field);
                }
            }
            EditAction::MoveField { rif_type, reg_type, moves } => {
                // Apply the new position to each affected field, backing them up for rollback
                let mut backups: Vec<(String, Field)> = Vec::new();
                let mut lines: Vec<usize> = Vec::new();
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    for (name, pos) in &moves {
                        if let Some(field) = find_field_mut(rif, &reg_type, name) {
                            backups.push((name.clone(), field.clone()));
                            if let Some(l) = field.src.decl_line {
                                lines.push(l);
                            }
                            field.pos = pos.clone();
                        }
                    }
                }
                if backups.is_empty() {
                    return;
                }
                self.recompile();
                if self.edit_err.is_none() {
                    for l in lines {
                        self.dirty.insert(l);
                    }
                    // Positions changed but names/selection are unchanged; reload the buffers
                    self.field_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    for (name, bak) in backups {
                        if let Some(field) = find_field_mut(rif, &reg_type, &name) {
                            *field = bak;
                        }
                    }
                }
            }
            EditAction::UpdateEnum { rif_type, name, values } => {
                // Find-or-create the definition, backing up its previous state for rollback
                let mut backup: Option<(bool, EnumDef)> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    if let Some(def) = rif.enum_defs.iter_mut().find(|d| d.name == name) {
                        backup = Some((true, def.clone()));
                        def.values = values;
                    } else {
                        backup = Some((false, EnumDef::new(name.clone(), String::new())));
                        let mut new_def = EnumDef::new(name.clone(), String::new());
                        new_def.values = values;
                        rif.enum_defs.push(new_def);
                    }
                }
                let Some((existed, prev)) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    // Record persistence edits
                    if let Some(src) = self.rif_src.as_ref()
                        && let Some(rif) = get_rif(&src.rifs, &rif_type)
                        && let Some(def) = rif.enum_defs.iter().find(|d| d.name == name)
                    {
                        let new_lines: HashSet<usize> = def.values.iter().filter_map(|e| e.src.decl_line).collect();
                        for old in &prev.values {
                            let Some(line) = old.src.decl_line else { continue };
                            if !new_lines.contains(&line) {
                                self.enum_deleted.insert(line);
                            } else if def.values.iter().any(|e| e.src.decl_line == Some(line) && e != old) {
                                self.enum_dirty.insert(line);
                            }
                        }
                    }
                    self.enum_editor = None;
                    // The modal may have renamed a "Type" enum; keep the open field editor's own
                    // buffer in sync so its own Apply targets the (possibly renamed) definition.
                    if let Some(fe) = self.field_editor.as_mut()
                        && fe.enum_mode == EnumMode::Type
                    {
                        fe.enum_name = name.clone();
                        fe.enum_type_src = EnumTypeSrc::Existing;
                    }
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    if existed {
                        if let Some(def) = rif.enum_defs.iter_mut().find(|d| d.name == name) {
                            *def = prev;
                        }
                    } else {
                        rif.enum_defs.retain(|d| d.name != name);
                    }
                }
            }
            EditAction::UpdateRegDef { rif_type, orig_name, inst_name, vals } => {
                let mut outcome = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    outcome = Self::apply_regdef_vals(rif, &orig_name, &inst_name, &vals);
                }
                let Some(UpdateRegDefOutcome {
                    backup, src_line, decl_changed, prop_bodies, desc_block_edit, inst_backups, inst_line_changed, new_name,
                    intr_desc_removed, field_intr_desc_removed, field_intr_ovr_removed, field_lines_changed,
                }) = outcome else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line {
                        if decl_changed {
                            self.reg_dirty.insert(line);
                        }
                        if !prop_bodies.is_empty() {
                            let entry = self.reg_prop_edits.entry(line).or_default();
                            for (prop, body) in prop_bodies { entry.insert(prop, body); }
                        }
                        if let Some(body) = desc_block_edit {
                            self.reg_desc_block_edits.insert(line, body);
                        }
                        for kind in intr_desc_removed {
                            self.reg_intr_desc_block_edits.entry(line).or_default().insert(kind, None);
                        }
                    }
                    for (line, kind) in field_intr_desc_removed {
                        self.field_intr_desc_block_edits.entry(line).or_default().insert(kind, None);
                    }
                    for line in field_intr_ovr_removed {
                        self.prop_edits.entry(line).or_default().insert(FieldProp::Interrupt, None);
                    }
                    for line in field_lines_changed {
                        self.dirty.insert(line);
                    }
                    if let Some(line) = inst_line_changed {
                        self.reg_inst_dirty.insert(line);
                    }
                    // Follow a rename in the tree selection (the register is always the last
                    // path segment when its own view is showing)
                    if self.selected.path.last() == Some(&orig_name) {
                        *self.selected.path.last_mut().expect("checked Some above") = new_name;
                    }
                    self.reg_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    if let Some(regdef) = find_regdef_mut(rif, &new_name) {
                        *regdef = backup;
                    }
                    for (pi, ii, old) in inst_backups {
                        if let Some(page) = rif.pages.get_mut(pi)
                            && let Some(inst) = page.instances.get_mut(ii)
                        {
                            *inst = old;
                        }
                    }
                }
            }
            EditAction::UpdateRegDefWithMoves { rif_type, orig_name, inst_name, vals, companions } => {
                let mut outcome = None;
                let mut companion_lines: Vec<usize> = Vec::new();
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    outcome = Self::apply_regdef_vals(rif, &orig_name, &inst_name, &vals);
                    if let Some(outcome) = outcome.as_mut() {
                        // Companion instances (the other register(s) a Swap/Shift/Insert
                        // resolution moves alongside the primary edit) are backed up into the
                        // *same* `inst_backups` the helper already populated, and mutated within
                        // this same `rif_src` borrow, so a failed recompile below rolls back the
                        // RegDef, any rename cascade, AND every companion move together.
                        for (comp_name, comp_addr) in &companions {
                            if let Some((pi, ii)) = rif.pages.iter().enumerate()
                                .find_map(|(pi, page)| page.instances.iter().position(|i| &i.inst_name == comp_name).map(|ii| (pi, ii)))
                            {
                                let inst = &mut rif.pages[pi].instances[ii];
                                outcome.inst_backups.push((pi, ii, inst.clone()));
                                inst.addr = Address::new(AddressKind::Absolute, AddressOffset::Value(*comp_addr));
                                if let Some(line) = inst.src.decl_line {
                                    companion_lines.push(line);
                                }
                            }
                        }
                    }
                }
                let Some(UpdateRegDefOutcome {
                    backup, src_line, decl_changed, prop_bodies, desc_block_edit, inst_backups, inst_line_changed, new_name,
                    intr_desc_removed, field_intr_desc_removed, field_intr_ovr_removed, field_lines_changed,
                }) = outcome else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line {
                        if decl_changed {
                            self.reg_dirty.insert(line);
                        }
                        if !prop_bodies.is_empty() {
                            let entry = self.reg_prop_edits.entry(line).or_default();
                            for (prop, body) in prop_bodies { entry.insert(prop, body); }
                        }
                        if let Some(body) = desc_block_edit {
                            self.reg_desc_block_edits.insert(line, body);
                        }
                        for kind in intr_desc_removed {
                            self.reg_intr_desc_block_edits.entry(line).or_default().insert(kind, None);
                        }
                    }
                    for (line, kind) in field_intr_desc_removed {
                        self.field_intr_desc_block_edits.entry(line).or_default().insert(kind, None);
                    }
                    for line in field_intr_ovr_removed {
                        self.prop_edits.entry(line).or_default().insert(FieldProp::Interrupt, None);
                    }
                    for line in field_lines_changed {
                        self.dirty.insert(line);
                    }
                    // Target + every companion that already had a source line — mirrors
                    // `SwapRegAddr`'s dirty-tracking, generalized to N instances instead of 2.
                    for line in inst_line_changed.into_iter().chain(companion_lines) {
                        self.reg_inst_dirty.insert(line);
                    }
                    if self.selected.path.last() == Some(&orig_name) {
                        *self.selected.path.last_mut().expect("checked Some above") = new_name;
                    }
                    // More than one instance's displayed address may now be stale (mirrors
                    // `SwapRegAddr`, which forces the same reload).
                    self.reg_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    if let Some(regdef) = find_regdef_mut(rif, &new_name) {
                        *regdef = backup;
                    }
                    for (pi, ii, old) in inst_backups {
                        if let Some(page) = rif.pages.get_mut(pi)
                            && let Some(inst) = page.instances.get_mut(ii)
                        {
                            *inst = old;
                        }
                    }
                }
            }
            EditAction::UpdateRegInstArray { rif_type, inst_name, array, companions } => {
                let mut backup: Option<RegInst> = None;
                let mut src_line = None;
                let mut decl_changed = false;
                let mut inst_backups: Vec<(usize, usize, RegInst)> = Vec::new();
                let mut companion_lines: Vec<usize> = Vec::new();
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    if let Some((pi, ii)) = rif.pages.iter().enumerate()
                        .find_map(|(pi, page)| page.instances.iter().position(|i| i.inst_name == inst_name).map(|ii| (pi, ii)))
                    {
                        let inst = &mut rif.pages[pi].instances[ii];
                        backup = Some(inst.clone());
                        src_line = inst.src.decl_line;
                        let orig_decl = inst.fmt_decl("");
                        inst.array = array.clone().unwrap_or_default();
                        decl_changed = inst.fmt_decl("") != orig_decl;
                    }
                    // Companion instances shifted forward to make room — backed up into their own
                    // vec (the primary instance's own backup lives separately, unlike
                    // `UpdateRegDefWithMoves` where everything shares one `RegDef`-shaped backup)
                    // so a failed recompile below rolls back the primary change and every
                    // companion move together.
                    for (comp_name, comp_addr) in &companions {
                        if let Some((pi, ii)) = rif.pages.iter().enumerate()
                            .find_map(|(pi, page)| page.instances.iter().position(|i| &i.inst_name == comp_name).map(|ii| (pi, ii)))
                        {
                            let inst = &mut rif.pages[pi].instances[ii];
                            inst_backups.push((pi, ii, inst.clone()));
                            inst.addr = Address::new(AddressKind::Absolute, AddressOffset::Value(*comp_addr));
                            if let Some(line) = inst.src.decl_line {
                                companion_lines.push(line);
                            }
                        }
                    }
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    for line in src_line.filter(|_| decl_changed).into_iter().chain(companion_lines) {
                        self.reg_inst_dirty.insert(line);
                    }
                    self.reg_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    let target = rif.pages.iter().enumerate()
                        .find_map(|(pi, page)| page.instances.iter().position(|i| i.inst_name == inst_name).map(|ii| (pi, ii)));
                    if let Some((tpi, tii)) = target {
                        rif.pages[tpi].instances[tii] = backup;
                    }
                    for (pi, ii, old) in inst_backups {
                        if let Some(page) = rif.pages.get_mut(pi)
                            && let Some(inst) = page.instances.get_mut(ii)
                        {
                            *inst = old;
                        }
                    }
                }
            }
            EditAction::UpdateRegIntrDesc { rif_type, reg_type, kind, desc } => {
                let mut backup: Option<RegDef> = None;
                let mut src_line = None;
                let mut desc_block_edit: Option<Option<Vec<String>>> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(regdef) = find_regdef_mut(rif, &reg_type)
                {
                    backup = Some(regdef.clone());
                    src_line = regdef.src.decl_line;
                    let orig_block = regdef.fmt_intr_desc_block(kind, "");
                    if let Some(d) = regdef.intr_desc_mut(kind) {
                        d.set_public(&desc);
                    }
                    let new_block = regdef.fmt_intr_desc_block(kind, "");
                    if new_block != orig_block {
                        desc_block_edit = Some(new_block);
                    }
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line
                        && let Some(body) = desc_block_edit
                    {
                        self.reg_intr_desc_block_edits.entry(line).or_default().insert(kind, body);
                    }
                    self.reg_intr_desc_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(regdef) = find_regdef_mut(rif, &reg_type)
                {
                    *regdef = backup;
                }
            }
            EditAction::UpdateFieldIntrDesc { rif_type, reg_type, field_name, kind, desc } => {
                let mut backup: Option<Field> = None;
                let mut src_line = None;
                let mut desc_block_edit: Option<Option<Vec<String>>> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(field) = find_field_mut(rif, &reg_type, &field_name)
                {
                    backup = Some(field.clone());
                    src_line = field.src.decl_line;
                    let orig_block = field.fmt_intr_desc_block(kind, "");
                    field.intr_desc_mut(kind).set_public(&desc);
                    let new_block = field.fmt_intr_desc_block(kind, "");
                    if new_block != orig_block {
                        desc_block_edit = Some(new_block);
                    }
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line
                        && let Some(body) = desc_block_edit
                    {
                        self.field_intr_desc_block_edits.entry(line).or_default().insert(kind, body);
                    }
                    self.field_intr_desc_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(field) = find_field_mut(rif, &reg_type, &field_name)
                {
                    *field = backup;
                }
            }
            EditAction::UpdateRegOverride { rif_type, inst_name, vals } => {
                // Backup for possible roll back on a failed recompile.
                let mut backup: Option<Option<RegOverride>> = None;
                let mut src_line = None;
                let mut prop_bodies: Vec<(RegOverrideProp, Option<String>)> = Vec::new();
                let mut desc_block_edit: Option<Option<Vec<String>>> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(inst) = rif.pages.iter_mut().find_map(|p| p.instances.iter_mut().find(|i| i.inst_name == inst_name))
                {
                    src_line = inst.src.decl_line;
                    let before = inst.reg_override.get(&None).cloned();
                    backup = Some(before.clone());
                    let before_ovr = before.unwrap_or_default();
                    let ovr = inst.reg_override.entry(None).or_default();
                    if vals.desc.trim().is_empty() {
                        ovr.description = None;
                    } else {
                        let mut d = Description::default();
                        d.set_public(&vals.desc);
                        ovr.description = Some(d);
                    }
                    ovr.hw_acc = vals.hw;
                    ovr.optional = vals.optional.unwrap_or_default();
                    ovr.optional_acc = vals.optional_acc;
                    for prop in RegOverrideProp::ALL {
                        let after_body = ovr.fmt_prop(prop, "");
                        if before_ovr.fmt_prop(prop, "") != after_body {
                            prop_bodies.push((prop, after_body));
                        }
                    }
                    let before_block = before_ovr.fmt_desc_block("");
                    let after_block = ovr.fmt_desc_block("");
                    if before_block != after_block {
                        desc_block_edit = Some(after_block);
                    }
                    // Drop the entry entirely once it's back to fully default
                    if *ovr == RegOverride::default() && ovr.fields.is_empty() {
                        inst.reg_override.remove(&None);
                    }
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line {
                        if !prop_bodies.is_empty() {
                            let entry = self.reg_override_edits.entry(line).or_default();
                            for (prop, body) in prop_bodies { entry.insert(prop, body); }
                        }
                        if let Some(body) = desc_block_edit {
                            self.reg_override_desc_block_edits.insert(line, body);
                        }
                    }
                    self.reg_override_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(inst) = rif.pages.iter_mut().find_map(|p| p.instances.iter_mut().find(|i| i.inst_name == inst_name))
                {
                    match backup {
                        Some(prev) => { inst.reg_override.insert(None, prev); }
                        None => { inst.reg_override.remove(&None); }
                    }
                }
            }
            EditAction::UpdateFieldOverride { rif_type, inst_name, field_name, vals } => {
                let mut backup: Option<Option<FieldOverride>> = None;
                let mut src_line = None;
                let mut desc_body: Option<Option<String>> = None;
                let mut reset_body: Option<Option<String>> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(inst) = rif.pages.iter_mut().find_map(|p| p.instances.iter_mut().find(|i| i.inst_name == inst_name))
                {
                    src_line = inst.src.decl_line;
                    let reg_ovr = inst.reg_override.entry(None).or_default();
                    let before = reg_ovr.fields.get(&field_name).cloned();
                    backup = Some(before.clone());
                    let before_field = before.unwrap_or_default();
                    let field_ovr = reg_ovr.fields.entry(field_name.clone()).or_default();
                    if vals.desc.trim().is_empty() {
                        field_ovr.description = None;
                    } else {
                        let mut d = Description::default();
                        d.set_public(&vals.desc);
                        field_ovr.description = Some(d);
                    }
                    field_ovr.visibility = vals.disabled.then_some(Visibility::Disabled);
                    field_ovr.reset = match &vals.reset {
                        Some(v) => ResetValOverride::Reset(v.clone()),
                        None => ResetValOverride::None,
                    };
                    let before_desc = before_field.fmt_prop(&field_name, FieldOverrideProp::Description, "", &vals.field_reset);
                    let after_desc = field_ovr.fmt_prop(&field_name, FieldOverrideProp::Description, "", &vals.field_reset);
                    if before_desc != after_desc { desc_body = Some(after_desc); }
                    let before_reset = before_field.fmt_prop(&field_name, FieldOverrideProp::Reset, "", &vals.field_reset);
                    let after_reset = field_ovr.fmt_prop(&field_name, FieldOverrideProp::Reset, "", &vals.field_reset);
                    if before_reset != after_reset { reset_body = Some(after_reset); }
                    if *field_ovr == FieldOverride::default() {
                        reg_ovr.fields.remove(&field_name);
                    }
                    if *reg_ovr == RegOverride::default() && reg_ovr.fields.is_empty() {
                        inst.reg_override.remove(&None);
                    }
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line {
                        let entry = self.field_override_edits.entry(line).or_default();
                        if let Some(body) = desc_body { entry.insert((field_name.clone(), FieldOverrideProp::Description), body); }
                        if let Some(body) = reset_body { entry.insert((field_name.clone(), FieldOverrideProp::Reset), body); }
                    }
                    self.field_override_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(inst) = rif.pages.iter_mut().find_map(|p| p.instances.iter_mut().find(|i| i.inst_name == inst_name))
                {
                    let reg_ovr = inst.reg_override.entry(None).or_default();
                    match backup {
                        Some(prev) => { reg_ovr.fields.insert(field_name.clone(), prev); }
                        None => { reg_ovr.fields.remove(&field_name); }
                    }
                }
            }
            EditAction::ConvertPageToManual { rif_type, page_name, compiled } => {
                let mut result: Option<Result<Vec<RegInst>, String>> = None;
                if let Some(src) = self.rif_src.as_ref()
                    && let Some(rif) = get_rif(&src.rifs, &rif_type)
                    && let Some(page) = rif.pages.iter().find(|p| p.name == page_name)
                {
                    result = Some(compiled.convert_to_manual(page, &src.rifs));
                }
                let Some(result) = result else { return; };
                let instances = match result {
                    Ok(instances) => instances,
                    Err(e) => { self.edit_err = Some(e); return; }
                };
                let mut backup: Option<RifPage> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(page) = rif.pages.iter_mut().find(|p| p.name == page_name)
                {
                    backup = Some(page.clone());
                    page.instances = instances;
                    page.inst_auto = InstMode::Manual;
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_some() {
                    if let Some(src) = self.rif_src.as_mut()
                        && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                        && let Some(page) = rif.pages.iter_mut().find(|p| p.name == page_name)
                    {
                        *page = backup;
                    }
                } else if let Some(ed) = self.reg_add_editor.as_mut()
                    && ed.rif_type == rif_type && ed.page_name == page_name
                {
                    ed.page_is_auto = false;
                }
            }
            EditAction::SwapRegAddr { rif_type, a_inst_name, b_inst_name } => {
                // Locate both instances by name
                let mut backup: Option<(RegInstLoc, RegInstLoc)> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(a_loc) = RegInstLoc::find(rif, &a_inst_name)
                    && let Some(b_loc) = RegInstLoc::find(rif, &b_inst_name)
                {
                    rif.pages[a_loc.page_idx].instances[a_loc.inst_idx].addr = b_loc.addr.clone();
                    rif.pages[b_loc.page_idx].instances[b_loc.inst_idx].addr = a_loc.addr.clone();
                    backup = Some((a_loc, b_loc));
                }
                let Some((a_loc, b_loc)) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    // Record persistence edits for whichever side already has a source line
                    if let Some(src) = self.rif_src.as_ref()
                        && let Some(rif) = get_rif(&src.rifs, &rif_type)
                    {
                        for line in [
                            rif.pages[a_loc.page_idx].instances[a_loc.inst_idx].src.decl_line,
                            rif.pages[b_loc.page_idx].instances[b_loc.inst_idx].src.decl_line,
                        ].into_iter().flatten() {
                            self.reg_inst_dirty.insert(line);
                        }
                    }
                    // Force a reload
                    self.reg_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    rif.pages[a_loc.page_idx].instances[a_loc.inst_idx].addr = a_loc.addr;
                    rif.pages[b_loc.page_idx].instances[b_loc.inst_idx].addr = b_loc.addr;
                }
            }
            EditAction::AddRegister { rif_type, page_name, kind, name, addr } => {
                let name = name.trim().to_owned();
                let mut added_reg_type = false;
                let mut added_inst_name: Option<String> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(page) = rif.pages.iter_mut().find(|p| p.name == page_name)
                {
                    let page_is_auto = page.is_auto();
                    match &kind {
                        AddRegKind::New => {
                            let mut def = RegDef::new(&name, None, None, "");
                            def.src.has_inline_desc = true;
                            // Default field to ensure the newly created register is valid
                            let mut f = Field::new(
                                "field0",
                                vec![ResetValP::Unsigned(0)],
                                FieldPos::Size(Width::Value(1)),
                                Some(FieldSwKind::ReadWrite),
                                None,
                                "",
                            );
                            f.src.has_inline_desc = true;
                            def.add_field(f);
                            page.registers.push(RegDefOrIncl::Def(Box::new(def)));
                            added_reg_type = true;
                            if !page_is_auto {
                                let inst = RegInst::from((
                                    name.as_str(), ExprTokens::new(0), None, None,
                                    Some(Address::new(AddressKind::Absolute, AddressOffset::Value(addr))),
                                ));
                                page.instances.push(inst);
                                added_inst_name = Some(name.clone());
                            }
                        }
                        AddRegKind::Instance { type_name } => {
                            let inst = RegInst::from((
                                name.as_str(), ExprTokens::new(0), Some(type_name.as_str()), None,
                                Some(Address::new(AddressKind::Absolute, AddressOffset::Value(addr))),
                            ));
                            page.instances.push(inst);
                            added_inst_name = Some(name.clone());
                        }
                    }
                }
                if !added_reg_type && added_inst_name.is_none() {
                    return;
                }
                self.recompile();
                if self.edit_err.is_none() {
                    // Follow the tree to the newly created register
                    if let Some(src) = self.rif_src.as_ref()
                        && let Some(rif) = get_rif(&src.rifs, &rif_type)
                        && rif.pages.len() > 1
                        && self.selected.path.last() != Some(&page_name)
                    {
                        self.selected.path.push(page_name);
                    }
                    self.selected.path.push(added_inst_name.unwrap_or(name));
                    self.selected.item = SelectedItem::None;
                    self.selected.updt = 1;
                    self.reg_add_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(page) = rif.pages.iter_mut().find(|p| p.name == page_name)
                {
                    // Roll back whatever was appended so the source stays compilable.
                    if added_reg_type {
                        page.registers.retain(|r| r.get_regdef().map(|d| d.name != name).unwrap_or(true));
                    }
                    if let Some(inst_name) = &added_inst_name {
                        page.instances.retain(|i| &i.inst_name != inst_name);
                    }
                }
            }
            EditAction::DeleteRegister { rif_type, page_name, reg_type, inst_name } => {
                // Keep backups (with original position) so a failed recompile leaves the source
                // untouched.
                let mut removed_def: Option<(usize, RegDefOrIncl)> = None;
                let mut removed_inst: Option<(usize, RegInst)> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(page) = rif.pages.iter_mut().find(|p| p.name == page_name)
                {
                    if let Some(name) = &inst_name
                        && let Some(idx) = page.instances.iter().position(|i| &i.inst_name == name)
                    {
                        removed_inst = Some((idx, page.instances.remove(idx)));
                    }
                    // Remove the definition once nothing on the page still instantiates it
                    let still_referenced = page.instances.iter().any(|i| i.type_name == reg_type);
                    if !still_referenced
                        && let Some(idx) = page.registers.iter().position(|r| r.get_regdef().is_some_and(|d| d.name == reg_type))
                    {
                        removed_def = Some((idx, page.registers.remove(idx)));
                    }
                }
                if removed_def.is_none() && removed_inst.is_none() {
                    return;
                }
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some((_, RegDefOrIncl::Def(d))) = &removed_def
                        && let Some(line) = d.src.decl_line
                    {
                        self.reg_deleted.insert(line);
                    }
                    if let Some((_, inst)) = &removed_inst
                        && let Some(line) = inst.src.decl_line
                    {
                        self.reg_inst_deleted.insert(line);
                    }
                    // The register just removed is always the current selection's last segment,
                    let removed_display_name = inst_name.clone().unwrap_or_else(|| reg_type.clone());
                    if self.selected.path.last() == Some(&removed_display_name) {
                        self.selected.path.pop();
                        self.selected.item = SelectedItem::None;
                    }
                    self.reg_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    && let Some(page) = rif.pages.iter_mut().find(|p| p.name == page_name)
                {
                    if let Some((idx, def)) = removed_def {
                        let at = idx.min(page.registers.len());
                        page.registers.insert(at, def);
                    }
                    if let Some((idx, inst)) = removed_inst {
                        let at = idx.min(page.instances.len());
                        page.instances.insert(at, inst);
                    }
                }
            }
            EditAction::UpdateRifDef { rif_type, vals } => {
                let mut outcome = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    outcome = Some(apply_rif_vals(rif, &vals));
                }
                let Some(UpdateRifDefOutcome { backup, src_line, decl_changed, prop_bodies, desc_block_edit, new_name }) = outcome else { return; };
                let renaming = new_name != rif_type;
                // A rename onto a name already used by another RIF must be rejected outright,
                if renaming
                    && let Some(src) = self.rif_src.as_ref()
                    && src.rifs.contains_key(&new_name)
                {
                    if let Some(src) = self.rif_src.as_mut()
                        && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                    {
                        *rif = backup;
                    }
                    self.edit_err = Some(format!("A RIF named '{new_name}' already exists"));
                    return;
                }
                let mut rename_backup: Option<RifRenameBackup> = None;
                if renaming
                    && let Some(src) = self.rif_src.as_mut()
                {
                    let mut item_backups: Vec<RifmuxItemBackup> = Vec::new();
                    for (mux_name, mux) in src.rifmux.iter_mut() {
                        for (idx, item) in mux.items.iter_mut().enumerate() {
                            if item.rif_type == RifType::Rif(rif_type.clone()) {
                                item_backups.push(RifmuxItemBackup { mux_name: mux_name.clone(), idx, old_type: item.rif_type.clone() });
                                item.rif_type = RifType::Rif(new_name.clone());
                            }
                        }
                    }
                    let top_was_this = src.top == RifGenTop::Rif(rif_type.clone());
                    if top_was_this {
                        src.top = RifGenTop::Rif(new_name.clone());
                    }
                    let old_path_key = remove_rif(&rif_type).to_owned();
                    let new_path_key = remove_rif(&new_name).to_owned();
                    let path_backup = if let Some(dir) = src.paths.remove(&old_path_key) {
                        src.paths.insert(new_path_key, dir.clone());
                        Some((old_path_key, dir))
                    } else {
                        None
                    };
                    if let Some(existing) = src.rifs.remove(&rif_type) {
                        src.rifs.insert(new_name.clone(), existing);
                    }
                    rename_backup = Some(RifRenameBackup { top_was_this, item_backups, path_backup });
                }
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(line) = src_line {
                        if decl_changed { self.rif_dirty.insert(line); }
                        if !prop_bodies.is_empty() {
                            let entry = &mut self.rif_prop_edits;
                            for (prop, body) in prop_bodies { entry.insert(prop, body); }
                        }
                        if let Some(body) = desc_block_edit {
                            self.rif_desc_block_edits = body;
                        }
                    }
                    // A rename retargets which RIF type `editing_rif`
                    if renaming { self.editing_rif = Some(new_name.clone()); }
                    self.rif_editor = None;
                } else {
                    if let Some(src) = self.rif_src.as_mut() {
                        if let Some(RifRenameBackup { top_was_this, item_backups, path_backup }) = rename_backup {
                            if let Some(existing) = src.rifs.remove(&new_name) {
                                src.rifs.insert(rif_type.clone(), existing);
                            }
                            if top_was_this {
                                src.top = RifGenTop::Rif(rif_type.clone());
                            }
                            for RifmuxItemBackup { mux_name, idx, old_type } in item_backups {
                                if let Some(mux) = src.rifmux.get_mut(&mux_name)
                                    && let Some(item) = mux.items.get_mut(idx)
                                {
                                    item.rif_type = old_type;
                                }
                            }
                            if let Some((old_path_key, dir)) = path_backup {
                                src.paths.remove(remove_rif(&new_name));
                                src.paths.insert(old_path_key, dir);
                            }
                        }
                        if let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type) {
                            *rif = backup;
                        }
                    }
                }
            }
            EditAction::UpdateRifClocking { rif_type, sw, hw } => {
                let mut backup: Option<Rif> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    backup = Some(rif.clone());
                    rif.sw_clocking = sw;
                    rif.hw_clocking = hw;
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    if let Some(src) = self.rif_src.as_ref()
                        && let Some(rif) = get_rif(&src.rifs, &rif_type)
                    {
                        let groups = [
                            (RifClockLineKind::SwClock, backup.fmt_clock_line(false, ""), rif.fmt_clock_line(false, "")),
                            (RifClockLineKind::HwClock, backup.fmt_clock_line(true, ""), rif.fmt_clock_line(true, "")),
                            (RifClockLineKind::SwClkEn, backup.fmt_clken_line(false, ""), rif.fmt_clken_line(false, "")),
                            (RifClockLineKind::HwClkEn, backup.fmt_clken_line(true, ""), rif.fmt_clken_line(true, "")),
                            (RifClockLineKind::SwClear, backup.fmt_clear_line(false, ""), rif.fmt_clear_line(false, "")),
                            (RifClockLineKind::HwClear, backup.fmt_clear_line(true, ""), rif.fmt_clear_line(true, "")),
                        ];
                        for (kind, before_text, after_text) in groups {
                            if before_text != after_text {
                                self.rif_clock_line_edits.insert(kind, after_text);
                            }
                        }
                        // Per-clock reset lines
                        for (before_vec, after_vec, hw_flag) in
                            [(&backup.sw_clocking, &rif.sw_clocking, false), (&backup.hw_clocking, &rif.hw_clocking, true)]
                        {
                            let unchanged = before_vec.len() == after_vec.len()
                                && before_vec.iter().zip(after_vec.iter()).all(|(b, a)| b.rst == a.rst);
                            if unchanged {
                                continue;
                            }
                            let keyword = if hw_flag { "hwReset" } else { "swReset" };
                            for before in before_vec {
                                if let Some(line) = before.rst.src.decl_line {
                                    self.rif_reset_deleted.insert(line);
                                }
                            }
                            let new_lines: Vec<String> = after_vec.iter().map(|c| c.rst.fmt_decl(keyword)).collect();
                            self.rif_reset_new_lines.insert(hw_flag, new_lines);
                        }
                    }
                    self.rif_clocking_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    *rif = backup;
                }
            }
            EditAction::UpdateRifParamsAndGenerics { rif_type, params, generics } => {
                let mut backup: Option<Rif> = None;
                if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    backup = Some(rif.clone());
                    let mut new_params = OrderDict::new();
                    for entry in &params { new_params.insert(entry.name.clone(), entry.value.clone()); }
                    rif.parameters = new_params;
                    let mut new_generics = OrderDict::new();
                    for entry in &generics { new_generics.insert(entry.name.clone(), entry.range.clone()); }
                    rif.generics = new_generics;
                }
                let Some(backup) = backup else { return; };
                self.recompile();
                if self.edit_err.is_none() {
                    let mut seen_params: HashSet<String> = HashSet::new();
                    for ParamEntry { orig_name, name, value } in &params {
                        let Some(old) = orig_name else {
                            self.rif_param_inserts.push(fmt_param_line(name, value, ""));
                            continue;
                        };
                        seen_params.insert(old.clone());
                        let changed = old != name || backup.parameters.get(old) != Some(value);
                        if changed && let Some(&line) = backup.src.param_lines.get(old.as_str()) {
                            self.rif_param_edits.insert(line, Some(fmt_param_line(name, value, "")));
                        }
                    }
                    for (old_name, _) in backup.parameters.items() {
                        if !seen_params.contains(old_name)
                            && let Some(&line) = backup.src.param_lines.get(old_name.as_str())
                        {
                            self.rif_param_edits.insert(line, None);
                        }
                    }
                    let mut seen_generics: HashSet<String> = HashSet::new();
                    for GenericEntry { orig_name, name, range } in &generics {
                        let Some(old) = orig_name else {
                            self.rif_generic_inserts.push(fmt_generic_line(name, range, ""));
                            continue;
                        };
                        seen_generics.insert(old.clone());
                        let changed = old != name || backup.generics.get(old) != Some(range);
                        if changed && let Some(&line) = backup.src.generic_lines.get(old.as_str()) {
                            self.rif_generic_edits.insert(line, Some(fmt_generic_line(name, range, "")));
                        }
                    }
                    for (old_name, _) in backup.generics.items() {
                        if !seen_generics.contains(old_name)
                            && let Some(&line) = backup.src.generic_lines.get(old_name.as_str())
                        {
                            self.rif_generic_edits.insert(line, None);
                        }
                    }
                    self.rif_params_editor = None;
                } else if let Some(src) = self.rif_src.as_mut()
                    && let Some(rif) = get_rif_mut(&mut src.rifs, &rif_type)
                {
                    *rif = backup;
                }
            }
        }
        if !was_unsaved && self.has_unsaved() {
            self.editing_rif.get_or_insert(rif_type_edited);
        } else if was_unsaved && !self.has_unsaved() {
            self.editing_rif = None;
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    use yarig::rifgen::{Access, ClockingInfo, DataWidth, DeclLine, DescBlockKind, EnumKind, ExternalKind, GenericRange, Interface, LimitP, Lock, RegPulseKind, ResetDef};
    use yarig::parser::parser_expr::parse_expr;
    use crate::enum_editor::EnumEntryRow;
    use crate::reg_editor::{RegAltVals, RegArrayChangeKind, RegIntrVals};

    fn basic_rw_field_lsb(v: &RifViewer, name: &str) -> u8 {
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let reg = rif.pages.iter().flat_map(|p| p.regs.iter())
            .find(|r| r.reg_name == "basic_rw").expect("basic_rw exists");
        reg.fields.iter().find(|f| f.name == name).expect("field exists").lsb
    }

    /// Load test.rif and select `basic_rw` the way the tree does: [top, page, register].
    fn load_with_basic_rw_selected() -> RifViewer {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        assert!(v.rif_comp.is_some(), "test.rif should compile: {:?}", v.last_err);
        // path[0] is the compiled top name; test_rif is multi-page, so "Main" then the register
        v.selected.path.push("Main".to_owned());
        v.selected.path.push("basic_rw".to_owned());
        v.selected.item = SelectedItem::Field("field1".to_owned());
        v
    }

    #[test]
    fn selected_rif_type_resolves_top_level_rif() {
        // Regression: a plain top-level RIF (name at path[0]) must still resolve, else every
        // edit action gets an empty rif_type and silently does nothing.
        let v = load_with_basic_rw_selected();
        assert_eq!(v.selected_rif_type(), "test_rif");
    }

    fn def_field(v: &RifViewer, reg: &str, name: &str) -> Field {
        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, "test_rif").unwrap();
        find_field(rif, reg, name).unwrap().clone()
    }

    /// Editing advanced properties mutates the definition and records the minimal per-line
    /// persistence edits (add an `hw`/`nbfrac` line, remove the `signed` line).
    #[test]
    fn apply_update_field_applies_advanced_props_and_records_prop_edits() {
        let mut v = load_with_basic_rw_selected();
        let f = def_field(&v, "basic_rw", "field1");
        assert!(f.signed, "field1 starts signed");
        let decl_line = f.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "basic_rw".to_owned(),
            orig_name: "field1".to_owned(),
            vals: Box::new(FieldVals {
                name: "field1".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: false,                 // remove `signed`
                sw_kind: FieldSwKind::ReadWrite,
                desc: f.description.get(true),
                hw_acc: Access::RW,            // add `hw rw`
                visibility: None,
                nb_frac: 2,                    // add `nbfrac 2`
                lock: Lock::default(),
                limit: LimitP::default(),
                counter: None,
                password: None,
                enum_kind: EnumKind::None,
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![FieldProp::Signed, FieldProp::HwAcc, FieldProp::NbFrac],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        // Definition mutated
        let f2 = def_field(&v, "basic_rw", "field1");
        assert!(!f2.signed);
        assert_eq!(f2.hw_acc, Access::RW);
        assert_eq!(f2.nb_frac, 2);
        // Minimal per-line persistence edits recorded against the declaration line
        let edits = v.prop_edits.get(&decl_line).expect("prop_edits recorded");
        assert_eq!(edits.get(&FieldProp::Signed), Some(&None), "signed line removed");
        assert_eq!(edits.get(&FieldProp::HwAcc), Some(&Some("hw rw".to_owned())));
        assert_eq!(edits.get(&FieldProp::NbFrac), Some(&Some("nbfrac 2".to_owned())));
        assert!(v.has_unsaved());
    }

    /// Editing the multi-line description box: growing field0's existing block records the new
    /// block body against its declaration line, and — since the first line is unchanged — leaves
    /// the declaration line itself untouched (not marked dirty).
    #[test]
    fn apply_update_field_description_block_edit_is_tracked() {
        let mut v = load_with_basic_rw_selected();
        let f = def_field(&v, "basic_rw", "field0");
        assert_eq!(f.description.get(true),
            "Field 8b\nMore detailled information on current field\nCan span multiple lines.");
        let decl_line = f.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "basic_rw".to_owned(),
            orig_name: "field0".to_owned(),
            vals: Box::new(FieldVals {
                name: "field0".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: "Field 8b\nMore detailled information on current field.\nNow with a third line.".to_owned(),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let f2 = def_field(&v, "basic_rw", "field0");
        assert_eq!(f2.description.get(true),
            "Field 8b\nMore detailled information on current field.\nNow with a third line.");
        assert!(!v.dirty.contains(&decl_line),
            "first line unchanged -> declaration line must not be marked dirty");
        let body = v.desc_block_edits.get(&decl_line)
            .expect("desc_block_edits recorded").clone().expect("block was not deleted");
        assert_eq!(body, vec![
            "description:".to_owned(),
            "  More detailled information on current field.".to_owned(),
            "  Now with a third line.".to_owned(),
        ]);
        assert!(v.has_unsaved());
    }

    /// End-to-end: applying advanced-property edits then `save_file` rewrites the sub-property
    /// lines in place (add `hw`/`nbfrac`, remove `signed`), keeps comments/siblings, and re-parses.
    #[test]
    fn save_file_persists_advanced_props_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_adv_props.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        v.selected.path.push("Main".to_owned());
        v.selected.path.push("basic_rw".to_owned());
        v.selected.item = SelectedItem::Field("field1".to_owned());

        let f = def_field(&v, "basic_rw", "field1");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type,
            reg_type: "basic_rw".to_owned(),
            orig_name: "field1".to_owned(),
            vals: Box::new(FieldVals {
                name: "field1".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: false,
                sw_kind: FieldSwKind::ReadWrite,
                desc: f.description.get(true),
                hw_acc: Access::RW,
                visibility: None,
                nb_frac: 2,
                lock: Lock::default(),
                limit: LimitP::default(),
                counter: None,
                password: None,
                enum_kind: EnumKind::None,
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![FieldProp::Signed, FieldProp::HwAcc, FieldProp::NbFrac],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "apply should recompile: {:?}", v.edit_err);
        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        // Sub-property lines reconciled in place
        assert!(after.lines().any(|l| l.trim() == "hw rw"), "hw line missing:\n{after}");
        assert!(after.lines().any(|l| l.trim() == "nbfrac 2"), "nbfrac line missing:\n{after}");
        assert!(!after.lines().any(|l| l.trim() == "signed"), "signed line not removed:\n{after}");
        // Surrounding content untouched
        assert!(after.contains("// Main parameters"));
        assert!(after.contains("- field2 = 0x45  31:16 \"Field with hexa init\""));
        // Re-parse assigned the new sub-properties to field1
        let f2 = def_field(&v, "basic_rw", "field1");
        assert_eq!(f2.hw_acc, Access::RW);
        assert_eq!(f2.nb_frac, 2);
        assert!(!f2.signed);

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: editing an array field's `arrayPosIncr` persists as a replaced sub-property
    /// line, and the file re-parses with the new value — the last leg of the field-array editing
    /// feature (name/position/array-size were already covered by the plain decl-line path;
    /// `arrayPosIncr` needed its own `FieldProp`/`prop_lines`/`fmt_prop` wiring in rifgen).
    #[test]
    fn save_file_persists_array_pos_incr_edit_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_pos_incr.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        v.selected.path.push("Main".to_owned());
        v.selected.path.push("reg_fields_pos".to_owned());
        v.selected.item = SelectedItem::Field("coeffs".to_owned());

        let f = def_field(&v, "reg_fields_pos", "coeffs");
        assert_eq!(f.array_pos_incr, 4);
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type,
            reg_type: "reg_fields_pos".to_owned(),
            orig_name: "coeffs".to_owned(),
            vals: Box::new(FieldVals {
                name: "coeffs".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: 2,
                reset: None,
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![FieldProp::ArrayPosIncr],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "apply should recompile: {:?}", v.edit_err);
        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.lines().any(|l| l.trim() == "arrayPosIncr 2"), "arrayPosIncr line not updated:\n{after}");
        assert!(!after.contains("arrayPosIncr 4"), "old arrayPosIncr value should be gone:\n{after}");
        // Sibling array fields untouched
        assert!(after.contains("- coeffs[4] = {0,1,2,3} 7:0 \"Coefficient 0 to 3\""));
        assert!(after.contains("- coeffs[4] = {4,5,6,7} 7:0 \"Coefficient 4 to 7\""));

        let f2 = def_field(&v, "reg_fields_pos", "coeffs");
        assert_eq!(f2.array_pos_incr, 2);

        fs::remove_file(&tmp).ok();
    }

    /// Pulse fields declared via the separate `pulse comb`/`pulse reg` sub-property line (as
    /// `f_pulse_c`/`f_pulse_r` are in test.rif, see `reg_group1`) must round-trip through
    /// Apply+Save without leaving a stale, now-redundant property line behind: `fmt_decl` always
    /// re-renders `sw_kind.inline_token()` onto the declaration line regardless of how the source
    /// originally expressed it, so an unrelated edit (here: just the description) would otherwise
    /// silently duplicate the pulse spec (inline on the decl line AND on the old sub-property
    /// line) unless the old line is removed. Also covers switching sw_kind away from pulse.
    #[test]
    fn save_file_pulse_field_round_trips_without_duplicating_property_line() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_pulse_field.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let f = def_field(&v, "reg_group1", "f_pulse_c");
        assert!(f.sw_kind.is_pulse_comb(), "f_pulse_c starts as a combinatorial pulse");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type,
            reg_type: "reg_group1".to_owned(),
            orig_name: "f_pulse_c".to_owned(),
            vals: Box::new(FieldVals {
                name: "f_pulse_c".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: false,
                sw_kind: f.sw_kind.clone(), // unchanged: still comb pulse
                desc: "Combinatorial Pulse (edited)".to_owned(),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                // Mirrors what `show_field_editor`'s own `changed` computation would produce for
                // an edit to a field that's (still) a pulse — see its `FieldProp::Pulse` push.
                changed: vec![FieldProp::Pulse],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "apply should recompile: {:?}", v.edit_err);
        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        let pulse_lines: Vec<&str> = after.lines().filter(|l| l.trim() == "pulse comb" || l.trim().starts_with("pulse")).collect();
        assert_eq!(pulse_lines.len(), 1, "expected exactly one pulse spec, got: {pulse_lines:?}\nfull file:\n{after}");
        let f2 = def_field(&v, "reg_group1", "f_pulse_c");
        assert!(f2.sw_kind.is_pulse_comb(), "still a combinatorial pulse after round-trip");

        fs::remove_file(&tmp).ok();
    }

    /// Switching a field OFF pulse must delete its old `pulse comb`/`pulse reg` sub-property
    /// line, not just stop inlining the token — otherwise the stale line is silently left behind
    /// describing a pulse that no longer exists.
    #[test]
    fn save_file_switching_off_pulse_removes_property_line() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_pulse_field_off.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let f = def_field(&v, "reg_group1", "f_pulse_r");
        assert!(f.sw_kind.is_pulse(), "f_pulse_r starts as a pulse");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type,
            reg_type: "reg_group1".to_owned(),
            orig_name: "f_pulse_r".to_owned(),
            vals: Box::new(FieldVals {
                name: "f_pulse_r".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: false,
                sw_kind: FieldSwKind::ReadWrite,
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![FieldProp::Pulse],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "apply should recompile: {:?}", v.edit_err);
        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        // Scope the check to f_pulse_r's own block: f_pulse_c (untouched, a sibling field) keeps
        // its own legitimate "pulse comb" line, so a whole-file search would false-positive on it.
        let block: Vec<&str> = after.lines().skip_while(|l| !l.contains("f_pulse_r"))
            .skip(1).take_while(|l| !l.trim_start().starts_with("- ")).collect();
        assert!(!block.iter().any(|l| l.trim().starts_with("pulse")), "stale pulse line left behind:\n{block:?}");
        let f2 = def_field(&v, "reg_group1", "f_pulse_r");
        assert!(!f2.sw_kind.is_pulse(), "no longer a pulse after switching to RW");
        // Regression: an unrelated edit must not wipe f_pulse_r's own untouched description
        // block just because `vals.desc` round-trips through `Description::set` (whole-string
        // replace) now — the editor always carries the full text forward, so passing the full
        // current description (not just its short line) must leave the block untouched.
        assert!(after.contains("Field stays high one cycle after write"),
            "unrelated edit must not drop the untouched description block:\n{after}");
        assert_eq!(f2.description.get(true), "Registered Pulse\nField stays high one cycle after write");

        fs::remove_file(&tmp).ok();
    }

    /// Regression for a reported "Save doesn't do anything" symptom on a brand new enum: a row
    /// added via the "add entry" ghost affordance must carry a value that already parses (0 for
    /// the first row, previous + 1 after), otherwise `build_values` rejects the row with a blank
    /// value and Save silently keeps failing — every added row must succeed with only a name typed.
    #[test]
    fn enum_editor_add_entry_prefills_a_valid_incrementing_value() {
        let mut ed = EnumEditor::open("test_rif", "doc:basic_rw_field0".to_owned(), &[], false, 8);
        // A brand new enum already carries one prefilled entry, so Save never requires clicking
        // "add entry" first.
        assert_eq!(ed.rows.len(), 1);
        assert_eq!(ed.rows[0].value, "0", "first entry defaults to 0");
        ed.rows[0].name = "IDLE".to_owned();

        ed.rows.push(EnumEntryRow::next_default(&ed.rows));
        assert_eq!(ed.rows[1].value, "1", "second entry defaults to previous + 1");
        ed.rows[1].name = "RUN".to_owned();

        // Exactly what a user gets after typing only the two names: build_values must succeed.
        let values = ed.build_values().expect("rows with auto-filled values should parse");
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].name, "IDLE");
        assert_eq!(values[0].value, 0);
        assert_eq!(values[1].name, "RUN");
        assert_eq!(values[1].value, 1);
    }

    /// Regression for the actual root cause behind "doc enum always appears undefined" on a
    /// grouped register: `RifRegInst.reg_type` is the register's own type name (`reg_group1`),
    /// but the parser names auto-generated enums after the register's *group* (`RegDef::
    /// get_group_name()`, exposed on the instance as `group_type`) — e.g. `doc:reg_group_field0`
    /// for `reg_group0`, shared across every register in the `(reg_group)` group. Computing the
    /// button's candidate name from `reg_type` (as `show_field_editor` did before this fix) built
    /// `doc:reg_group1_field4`, which never matches an existing group-scoped definition, so the
    /// button — and any name typed for a new one — could never agree with what actually gets
    /// parsed back out of the file.
    #[test]
    fn grouped_register_group_type_differs_from_reg_type_and_drives_enum_naming() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let reg1 = rif.pages.iter().flat_map(|p| p.regs.iter())
            .find(|r| r.reg_name == "reg_group1").expect("reg_group1 exists");
        assert_eq!(reg1.reg_type, "reg_group1");
        assert_eq!(reg1.group_type, "reg_group",
            "reg_group1's group type is the shared '(reg_group)' name, not its own type name");

        // Same formula `show_field_editor` now uses for the Doc button's candidate name.
        let kind = EnumKind::new("", &reg1.group_type, "field4");
        assert_eq!(kind, EnumKind::Doc("doc:reg_group_field4".to_owned()));
    }

    /// Regression for a reported "New button never flips to Edit entries" symptom: create a
    /// brand-new doc enum via the exact sequence the UI drives (open editor for the not-yet-
    /// existing name, add one entry through the ghost row, Save), then check the button's own
    /// existence-check formula (`enum_defs.iter().any(|d| d.name == doc_name)`) now finds it.
    #[test]
    fn doc_enum_button_flips_to_edit_entries_after_creating_it() {
        let mut v = load_with_basic_rw_selected(); // selects basic_rw.field1, which starts with no enum
        assert_eq!(def_field(&v, "basic_rw", "field1").enum_kind, EnumKind::None);
        let rif_type = v.selected_rif_type();
        let doc_name = "doc:basic_rw_field1".to_owned();

        let def_exists = |v: &RifViewer| {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            rif.enum_defs.iter().any(|d| d.name == doc_name)
        };
        assert!(!def_exists(&v), "doc enum shouldn't exist yet");

        // Exactly what the UI does: open the editor (already carries one prefilled entry), type
        // only a name (value is auto-filled), then Save.
        let mut ed = EnumEditor::open(&rif_type, doc_name.clone(), &[], false, 8);
        assert_eq!(ed.rows.len(), 1, "brand new enum starts with one prefilled entry");
        ed.rows[0].name = "IDLE".to_owned();
        let values = ed.build_values().expect("auto-filled value should parse");
        v.pending = Some(EditAction::UpdateEnum { rif_type: rif_type.clone(), name: doc_name.clone(), values });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        assert!(def_exists(&v), "definition should exist after Save, flipping the button to 'Edit'");
    }

    /// End-to-end: giving a field with no enum a brand new named "Type" enum, then adding
    /// entries to it (both before ever saving), persists the header line AND the entries block
    /// piggybacked right after it in a single save — the trickiest new path, since neither the
    /// header nor the definition exist in the file yet.
    #[test]
    fn save_file_persists_new_enum_with_entries_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_new_enum.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        v.selected.path.push("Main".to_owned());
        v.selected.path.push("basic_rw".to_owned());
        v.selected.item = SelectedItem::Field("field0".to_owned());

        let f = def_field(&v, "basic_rw", "field0");
        assert_eq!(f.enum_kind, EnumKind::None, "field0 starts with no enum");
        let rif_type = v.selected_rif_type();
        // Give field0 a brand new named enum type (nothing else changes)
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "basic_rw".to_owned(),
            orig_name: "field0".to_owned(),
            vals: Box::new(FieldVals {
                name: f.name.clone(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: EnumKind::Type("e_basic_rw_field0".to_owned()),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![FieldProp::EnumKind],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert_eq!(def_field(&v, "basic_rw", "field0").enum_kind, EnumKind::Type("e_basic_rw_field0".to_owned()));

        // Add two brand new entries to that (not-yet-saved) definition
        v.pending = Some(EditAction::UpdateEnum {
            rif_type: rif_type.clone(),
            name: "e_basic_rw_field0".to_owned(),
            values: vec![
                EnumEntry { name: "IDLE".to_owned(), value: 0, repr: None, description: "Idle".into(), src: DeclLine::default() },
                EnumEntry { name: "RUN".to_owned(), value: 1, repr: None, description: "Running".into(), src: DeclLine::default() },
            ],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.lines().any(|l| l.trim() == "enum e_basic_rw_field0"), "enum header missing:\n{after}");
        assert!(after.lines().any(|l| l.trim() == "- IDLE = 0 \"Idle\""), "IDLE entry missing:\n{after}");
        assert!(after.lines().any(|l| l.trim() == "- RUN = 1 \"Running\""), "RUN entry missing:\n{after}");
        // Surrounding content untouched
        assert!(after.contains("- field2 = 0x45  31:16 \"Field with hexa init\""));
        assert!(after.lines().any(|l| l.trim() == "signed"));

        // Re-parse assigned real source lines to the definition and its entries
        let f2 = def_field(&v, "basic_rw", "field0");
        assert_eq!(f2.enum_kind, EnumKind::Type("e_basic_rw_field0".to_owned()));
        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let def = rif.enum_defs.iter().find(|d| d.name == "e_basic_rw_field0").expect("def persisted");
        assert!(def.src.decl_line.is_some());
        assert_eq!(def.values.len(), 2);
        assert!(def.values.iter().all(|e| e.src.decl_line.is_some()));

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: adding an entry to an ALREADY-DECLARED enum (its header and other entries
    /// already exist in the file) inserts just the new entry line at the end of the existing
    /// entries block, leaving everything else — including the other entry lines — untouched.
    #[test]
    fn save_file_persists_new_entry_on_existing_enum_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_existing_enum.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        v.selected.path.push("Main".to_owned());
        v.selected.path.push("reg_group0".to_owned());
        v.selected.item = SelectedItem::Field("field1".to_owned());

        let f = def_field(&v, "reg_group0", "field1");
        let name = f.enum_kind.name().expect("field1 has a named enum").to_owned();
        let rif_type = v.selected_rif_type();
        let mut values = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            let def = rif.enum_defs.iter().find(|d| d.name == name).expect("enum already declared");
            assert!(def.src.decl_line.is_some(), "enum header already exists in the file");
            assert_eq!(def.values.len(), 2, "starts with its 2 declared entries");
            def.values.clone()
        };
        values.push(EnumEntry { name: "F1_VAL2".to_owned(), value: 2, repr: None, description: "F1 Value 2".into(), src: DeclLine::default() });

        v.pending = Some(EditAction::UpdateEnum { rif_type: rif_type.clone(), name: name.clone(), values });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.enum_dirty.is_empty(), "existing entries were untouched");
        assert!(v.enum_deleted.is_empty(), "nothing was removed");

        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.lines().any(|l| l.trim() == "- F1_VAL2 = 2 \"F1 Value 2\""), "new entry missing:\n{after}");
        assert!(after.lines().any(|l| l.trim() == "- F1_VAL0 = 0 \"F1 Value 0\""), "original entry 0 missing:\n{after}");
        assert!(after.lines().any(|l| l.trim() == "- F1_VAL1 = 1 \"F1 Value 1\""), "original entry 1 missing:\n{after}");
        // Surrounding content untouched (sibling field in the same register)
        assert!(after.contains("partial 0"));

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let def = rif.enum_defs.iter().find(|d| d.name == name).expect("def still there");
        assert_eq!(def.values.len(), 3);
        assert!(def.values.iter().all(|e| e.src.decl_line.is_some()), "all entries got real source lines");

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: opening a Rifmux and editing a field inside one of its sub-RIFs (resolved
    /// purely by name, exactly as a Rifmux references it — no path is ever stored on `Rif`)
    /// writes the change to the sub-RIF's OWN file, not the Rifmux file that was actually opened,
    /// and leaves the Rifmux file byte-for-byte untouched. This is the scenario `save_file` used
    /// to reject outright ("Saving is only supported for single-RIF files for now").
    #[test]
    fn save_file_persists_edit_to_rifmux_sub_rif_in_its_own_file() {
        use std::fs;
        let dir = std::env::temp_dir().join("yargui_rifmux_save_test");
        fs::create_dir_all(&dir).unwrap();
        let mux_path = dir.join("top_mux.rif");
        let leaf_path = dir.join("leaf.rif");
        fs::write(&mux_path, concat!(
            "rifmux: top_mux\n",
            "  addrWidth: 16\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  map:\n",
            "    - leaf_inst = leaf @ 0x0000\n",
        )).unwrap();
        fs::write(&leaf_path, concat!(
            "rif: leaf\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main Page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - basic_rw: \"Simple register\"\n",
            "        - field0 = 0  7:0  \"Field A\"\n",
        )).unwrap();

        let mut v = RifViewer::default();
        v.file_path = mux_path.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "rifmux should compile: {:?}", v.last_err);
        assert!(matches!(v.rif_comp, Some(Comp::Rifmux(_))), "top should be a Rifmux");

        // Select the sub-RIF's field the way the tree does: [top, sub-inst, register].
        v.selected.path.push("leaf_inst".to_owned());
        v.selected.path.push("basic_rw".to_owned());
        v.selected.item = SelectedItem::Field("field0".to_owned());
        let rif_type = v.selected_rif_type();
        assert_eq!(rif_type, "leaf", "selection should resolve to the sub-RIF, not the Rifmux");

        let f = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            find_field(rif, "basic_rw", "field0").unwrap().clone()
        };
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "basic_rw".to_owned(),
            orig_name: "field0".to_owned(),
            vals: Box::new(FieldVals {
                name: "field0".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: "Field A (edited)".to_owned(),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: Vec::new(),
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        // Dirty state is attributed to the sub-RIF's own type name, not the Rifmux's
        assert_eq!(v.editing_rif.as_deref(), Some("leaf"), "edit should be recorded under the sub-RIF's name, not the Rifmux's");

        v.save_file();
        assert!(v.edit_err.is_none(), "save+reparse should succeed: {:?}", v.edit_err);

        // The edit landed in leaf.rif, not top_mux.rif
        let leaf_after = fs::read_to_string(&leaf_path).unwrap();
        assert!(leaf_after.contains("Field A (edited)"), "edit missing from leaf.rif:\n{leaf_after}");
        let mux_after = fs::read_to_string(&mux_path).unwrap();
        assert!(mux_after.contains("- leaf_inst = leaf @ 0x0000"), "top_mux.rif should be untouched");

        // Re-parse assigned the edit to the right RIF
        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, "leaf").unwrap();
        let f2 = find_field(rif, "basic_rw", "field0").unwrap();
        assert_eq!(f2.description.get_short(false), "Field A (edited)");

        fs::remove_file(&mux_path).ok();
        fs::remove_file(&leaf_path).ok();
        fs::remove_dir(&dir).ok();
    }

    /// Assigning a field from `None` to an already-declared enum type (picked from the combo,
    /// as opposed to auto-naming a new one) records a prop edit and flips `has_unsaved()`, same
    /// as any other advanced-property change — there is no special case here.
    #[test]
    fn apply_update_field_to_existing_enum_type_marks_unsaved() {
        let mut v = load_with_basic_rw_selected();
        let existing_name = def_field(&v, "reg_group0", "field1").enum_kind.name()
            .expect("field1 has a type enum").to_owned();
        let f = def_field(&v, "basic_rw", "field0");
        assert_eq!(f.enum_kind, EnumKind::None);
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type,
            reg_type: "basic_rw".to_owned(),
            orig_name: "field0".to_owned(),
            vals: Box::new(FieldVals {
                name: f.name.clone(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: EnumKind::Type(existing_name.clone()),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![FieldProp::EnumKind],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert_eq!(def_field(&v, "basic_rw", "field0").enum_kind, EnumKind::Type(existing_name));
        assert!(v.has_unsaved(), "picking an existing enum type is a prop edit like any other");
    }

    #[test]
    fn apply_move_field_swaps_positions() {
        let mut v = load_with_basic_rw_selected();
        // Baseline
        assert_eq!(basic_rw_field_lsb(&v, "field0"), 0);
        assert_eq!(basic_rw_field_lsb(&v, "field1"), 8);
        // Build the move the way the UI does, using the *resolved* rif type
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::MoveField {
            rif_type,
            reg_type: "basic_rw".to_owned(),
            moves: vec![
                ("field0".to_owned(), FieldPos::MsbLsb((Width::Value(15), Width::Value(8)))),
                ("field1".to_owned(), FieldPos::MsbLsb((Width::Value(7), Width::Value(0)))),
            ],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert_eq!(basic_rw_field_lsb(&v, "field0"), 8, "field0 should have moved up to 15:8");
        assert_eq!(basic_rw_field_lsb(&v, "field1"), 0, "field1 should have moved down to 7:0");
    }

    /// End-to-end: moving a partial field changes only its register-local `pos`/`lsb`; the
    /// `partial N` logical-offset attribute (which `PartialFieldInfo` reads from `partial.0`, not
    /// `lsb`) is untouched, so the recompile succeeds and the merged cross-register field stays
    /// intact.
    #[test]
    fn apply_move_field_preserves_partial_offset() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        assert!(v.rif_comp.is_some(), "test.rif should compile: {:?}", v.last_err);
        v.selected.path.push("Main".to_owned());
        v.selected.path.push("reg_group0".to_owned());
        v.selected.item = SelectedItem::Field("field2".to_owned());

        let before = def_field(&v, "reg_group0", "field2");
        assert_eq!(before.partial.0, Some(0), "field2 starts as partial 0");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::MoveField {
            rif_type,
            reg_type: "reg_group0".to_owned(),
            moves: vec![
                ("field1".to_owned(), FieldPos::MsbLsb((Width::Value(31), Width::Value(24)))),
                ("field2".to_owned(), FieldPos::MsbLsb((Width::Value(23), Width::Value(8)))),
            ],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let after = def_field(&v, "reg_group0", "field2");
        assert_eq!(after.partial.0, Some(0), "partial offset is untouched by the position swap");
        match after.pos {
            FieldPos::MsbLsb((m, l)) => {
                assert_eq!(l.value(&ParamValues::default()).unwrap(), 8);
                assert_eq!(m.value(&ParamValues::default()).unwrap(), 23);
            }
            other => panic!("expected MsbLsb after a move, got {other:?}"),
        }
    }

    /// Pulse AND partial fields ARE reorderable (a position swap only touches `pos`/`lsb`, which
    /// for a partial field is entirely independent of its `partial N` logical-offset attribute
    /// — see `PartialFieldInfo`, which reads `partial.0`, never `lsb`). Only array elements stay
    /// blocked — they share one definition-level `Field` (no independent position). Regression
    /// test for a report that reordering looked "increasingly broken" deeper into test.rif:
    /// `reg_group1`'s `field4` sits between a pulse field and a partial field, so it was wrongly
    /// blocked in both directions before `is_field_reorderable` stopped treating pulse/partial the
    /// same as array.
    #[test]
    fn is_field_reorderable_allows_pulse_and_partial_blocks_array() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let get_reg = |name: &str| rif.pages.iter().flat_map(|p| p.regs.iter())
            .find(|r| r.reg_name == name).unwrap_or_else(|| panic!("{name} exists"));
        let get_field = |reg: &RifRegInst, name: &str| reg.fields.iter().find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} exists")).clone();

        let reg_group1 = get_reg("reg_group1");
        let f_pulse_c = get_field(reg_group1, "f_pulse_c");
        let f_pulse_r = get_field(reg_group1, "f_pulse_r");
        let field4 = get_field(reg_group1, "field4");
        let field2 = get_field(reg_group1, "field2");
        assert!(RifViewer::is_field_reorderable(&f_pulse_c), "pulse fields are reorderable");
        assert!(RifViewer::is_field_reorderable(&f_pulse_r), "pulse fields are reorderable");
        assert!(RifViewer::is_field_reorderable(&field4), "field4 itself has a concrete position");
        assert!(RifViewer::is_field_reorderable(&field2), "partial fields are reorderable too");

        let reg_arr = get_reg("reg_fields_a0");
        assert!(!RifViewer::is_field_reorderable(&reg_arr.fields[0]), "array elements stay blocked");
    }

    fn write_mini_register_array_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: mini_arr\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - reg_arr[4] : \"Array register\"\n",
            "        - coeffs[2] = 0 3:0 \"Coeffs\"\n",
            "    instances: auto\n",
        )).unwrap();
    }

    /// Field arrays are editable in a plain register (case 1/2 from the feature request), but
    /// NOT when the owning register is itself a register-array (case 3, explicitly deferred —
    /// register arrays have no general editing support yet). Mirrors
    /// `is_field_reorderable_allows_pulse_and_partial_blocks_array`'s style, one gate up.
    #[test]
    fn is_field_editable_allows_array_unless_register_is_array() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let reg_pos = rif.pages.iter().flat_map(|p| p.regs.iter())
            .find(|r| r.reg_name == "reg_fields_pos").expect("reg_fields_pos exists");
        let field_pos = reg_pos.fields.iter().find(|f| f.name == "coeffs").expect("coeffs exists");
        let def_pos = def_field(&v, "reg_fields_pos", "coeffs");
        assert!(RifViewer::is_field_editable(reg_pos, field_pos, &def_pos),
            "a plain array field (fits in one register) is editable");

        let tmp = std::env::temp_dir().join("yargui_reg_array_editable.rif");
        write_mini_register_array_fixture(&tmp);
        let mut v_arr = RifViewer::default();
        v_arr.file_path = tmp.clone();
        v_arr.open_file();
        assert!(v_arr.rif_comp.is_some(), "mini_arr should compile: {:?}", v_arr.last_err);
        let Some(Comp::Rif(rif_arr)) = &v_arr.rif_comp else { panic!("expected a Rif comp") };
        let reg_arr = rif_arr.pages.iter().flat_map(|p| p.regs.iter())
            .find(|r| r.reg_type == "reg_arr").expect("reg_arr exists");
        assert!(reg_arr.array.opt_idx().is_some(), "reg_arr is itself a register-array");
        let field_arr = reg_arr.fields.iter().find(|f| f.name == "coeffs").expect("coeffs exists");
        let def_arr = def_regdef_in(&v_arr, "mini_arr", "reg_arr").fields.into_iter()
            .find(|f| f.name == "coeffs").expect("coeffs def exists");
        assert!(!RifViewer::is_field_editable(reg_arr, field_arr, &def_arr),
            "a field array inside a register-array stays blocked (case 3, deferred)");

        std::fs::remove_file(&tmp).ok();
    }

    /// A register's Delete button must stay disabled when its only field definition is an array
    /// — `RifRegInst.fields` holds one `RifFieldInst` per array *element*, so counting instances
    /// (the pre-fix behavior) would wrongly report `reg_fields_pos` (one field, `coeffs[8]`) as
    /// having 8 fields and enable Delete, which would empty the register.
    #[test]
    fn can_delete_counts_field_definitions_not_array_elements() {
        let v = load_with_basic_rw_selected();
        let regdef = def_regdef(&v, "reg_fields_pos");
        assert_eq!(regdef.fields.len(), 1, "reg_fields_pos has exactly one field definition");
        assert!(!RifViewer::field_can_delete(&regdef), "sole array field must not be deletable");
        // Sanity check against the pre-fix instance count, to document why the naive count was wrong
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let reg = rif.pages.iter().flat_map(|p| p.regs.iter())
            .find(|r| r.reg_name == "reg_fields_pos").expect("reg_fields_pos exists");
        assert_eq!(reg.fields.len(), 8, "8 RifFieldInst rows, one per array element");
    }

    /// `field_has_array_partial` must flag BOTH halves of an `arrayPartial` split by name across
    /// the whole group — the continuation half (`reg_fields_a1`) carries `partial.1 > 0` on its
    /// own `Field`, but the base half (`reg_fields_a0`) does not, and still needs the same lock.
    #[test]
    fn field_has_array_partial_flags_both_halves_of_the_split() {
        let v = load_with_basic_rw_selected();
        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, "test_rif").unwrap();
        assert!(field_has_array_partial(rif, "reg_fields_a", "coeffs"),
            "the base half (reg_fields_a0) must be locked too, even though its own partial.1 is 0");
        assert!(!field_has_array_partial(rif, "reg_fields_pos", "coeffs"),
            "a plain (non-arrayPartial) array field must not be locked");
    }

    /// Renaming/re-describing an array field with a multi-value reset list must not touch the
    /// list itself — this is exactly the scenario the `fmt_decl` reset-list fix (rifgen) guards
    /// against: any decl-line rewrite used to silently collapse `{4,5,6,7}` to its first value.
    #[test]
    fn update_field_preserves_array_reset_list_on_unrelated_edit() {
        let mut v = load_with_basic_rw_selected();
        let f = def_field(&v, "reg_fields_a1", "coeffs");
        assert_eq!(f.reset, vec![
            ResetValP::Unsigned(4), ResetValP::Unsigned(5), ResetValP::Unsigned(6), ResetValP::Unsigned(7),
        ]);
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "reg_fields_a1".to_owned(),
            orig_name: "coeffs".to_owned(),
            vals: Box::new(FieldVals {
                name: "coeffs_renamed".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None, // Reset untouched this Apply
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let f2 = def_field(&v, "reg_fields_a1", "coeffs_renamed");
        assert_eq!(f2.reset, vec![
            ResetValP::Unsigned(4), ResetValP::Unsigned(5), ResetValP::Unsigned(6), ResetValP::Unsigned(7),
        ], "reset list must survive an unrelated rename");
    }

    /// Resizing an array field without also touching Reset this Apply collapses any stale
    /// multi-element reset list down to a single value — otherwise the (now-correct) `fmt_decl`
    /// would persist a `{...}` list whose length no longer matches the new `[n]`.
    #[test]
    fn update_field_shrinking_array_collapses_stale_reset() {
        let mut v = load_with_basic_rw_selected();
        let f = def_field(&v, "reg_fields_a1", "coeffs");
        assert_eq!(f.array, Width::Value(4));
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "reg_fields_a1".to_owned(),
            orig_name: "coeffs".to_owned(),
            vals: Box::new(FieldVals {
                name: "coeffs".to_owned(),
                pos: f.pos.clone(),
                array: Width::Value(2), // shrink 4 -> 2
                array_pos_incr: f.array_pos_incr,
                reset: None, // Reset untouched this Apply
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let f2 = def_field(&v, "reg_fields_a1", "coeffs");
        assert_eq!(f2.array, Width::Value(2));
        assert_eq!(f2.reset.len(), 1, "stale 4-element reset list must collapse on resize");
        assert_eq!(f2.reset[0], ResetValP::Unsigned(4), "collapses to the first element's value");
    }

    /// A generic-width field keeps its `Width::Param` reference through a reorder (`lsb+:$name`)
    /// instead of collapsing to a hardcoded literal msb, which would silently strip the
    /// parameterization on save. A concrete-width field still gets a plain `msb:lsb`.
    #[test]
    fn reordered_pos_preserves_generic_width_param() {
        let generic = FieldWidth::Generic(("MY_GEN".to_owned(), GenericRange { min: 1, max: 8, default: 4, desc: None }));
        match reordered_pos(&generic, 4) {
            FieldPos::LsbSize((Width::Value(lsb), Width::Param(name))) => {
                assert_eq!(lsb, 4);
                assert_eq!(name, "MY_GEN");
            }
            other => panic!("expected LsbSize(Value, Param) for a generic-width field, got {other:?}"),
        }

        let concrete = FieldWidth::Value(8);
        match reordered_pos(&concrete, 4) {
            FieldPos::MsbLsb((Width::Value(msb), Width::Value(lsb))) => {
                assert_eq!(lsb, 4);
                assert_eq!(msb, 11);
            }
            other => panic!("expected MsbLsb(Value, Value) for a concrete-width field, got {other:?}"),
        }
    }

    // ---- Register (type) editor: name/description/pulse/address ----

    /// Small manually-instanced fixture (mirrors `rifgen`'s own `parse_mini_manual_rif`, since
    /// `test.rif`'s only page is fully automatic): two named instances sharing one type
    /// (`ctrl_a`/`ctrl_b` -> `ctrl`) and one bare instance (`status`), all with real source
    /// lines from parsing — unlike a freshly-converted page's instances, which start with none.
    fn write_mini_manual_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: mini_rif\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - ctrl: \"Control register\"\n",
            "        - en = 0 0:0 \"Enable\"\n",
            "      - status: \"Status register\"\n",
            "        - busy = 0 0:0 ro \"Busy\"\n",
            "    instances:\n",
            "      - ctrl_a = ctrl @ 0x0\n",
            "      - ctrl_b = ctrl @ 0x4\n",
            "      - status @ 0x8\n",
        )).unwrap();
    }

    fn def_regdef(v: &RifViewer, reg: &str) -> RegDef {
        def_regdef_in(v, "test_rif", reg)
    }

    fn def_regdef_in(v: &RifViewer, rif_type: &str, reg: &str) -> RegDef {
        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, rif_type).unwrap();
        find_regdef(rif, reg).unwrap().clone()
    }

    /// Renaming a register and flipping its pulse mutates the definition and records the
    /// minimal per-line persistence edits: the declaration line only when the rename actually
    /// changes it, and only the pulse property that actually changed.
    #[test]
    fn apply_update_regdef_renames_and_toggles_pulse() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None; // viewing the register itself, not a field
        let d = def_regdef(&v, "basic_rw");
        assert!(d.pulse.iter().any(|p| matches!(p, RegPulseKind::Write(_))), "basic_rw starts with wrPulse");
        assert!(!d.pulse.iter().any(|p| matches!(p, RegPulseKind::Read(_))), "basic_rw starts without rdPulse");
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw2".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: vec![RegPulseKind::Write("clk_rif".to_owned()), RegPulseKind::Read(String::new())],
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "basic_rw2");
        assert_eq!(d2.name, "basic_rw2");
        // Regression: basic_rw's implicit (ungrouped) group must follow the rename, or
        // `fmt_decl` leaks the old name back as a spurious explicit `(basic_rw)` group.
        assert_eq!(d2.group.name, "basic_rw2", "implicit group must be renamed in lockstep");
        assert!(d2.pulse.iter().any(|p| matches!(p, RegPulseKind::Read(c) if c.is_empty())), "rdPulse comb added");

        // Declaration line dirty (name changed); only the newly-added rdPulse property recorded
        // (wrPulse's canonical form is unchanged, so it must NOT show up as an edit)
        assert!(v.reg_dirty.contains(&decl_line));
        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::RdPulse), Some(&Some("rdPulse comb".to_owned())));
        assert!(!edits.contains_key(&RegProp::WrPulse), "unchanged wrPulse must not be recorded");
        assert!(v.has_unsaved());

        // The tree selection followed the rename
        assert_eq!(v.selected.path.last(), Some(&"basic_rw2".to_owned()));
    }

    /// Editing visibility/clock/reset/external mutates the definition and records each newly-set
    /// sub-property's line for insertion (basic_rw starts with none of these) — mirrors
    /// `apply_update_regdef_renames_and_toggles_pulse`, one property group over.
    #[test]
    fn apply_update_regdef_edits_visibility_clock_reset_external() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let d = def_regdef(&v, "basic_rw");
        assert_eq!(d.visibility, Visibility::Full);
        assert_eq!(d.clk, None);
        assert_eq!(d.rst, None);
        assert_eq!(d.external, ExternalKind::None);
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Hidden,
                clk: Some("myclk".to_owned()),
                rst: Some("myrst".to_owned()),
                external: ExternalKind::ReadWrite,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "basic_rw");
        assert_eq!(d2.visibility, Visibility::Hidden);
        assert_eq!(d2.clk, Some("myclk".to_owned()));
        assert_eq!(d2.rst, Some("myrst".to_owned()));
        assert_eq!(d2.external, ExternalKind::ReadWrite);

        // Decl line unchanged (no rename); all four new sub-properties recorded as inserts
        assert!(!v.reg_dirty.contains(&decl_line), "decl line unchanged: name wasn't touched");
        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edits recorded");
        assert_eq!(edits.get(&RegProp::Visibility), Some(&Some("hidden".to_owned())));
        assert_eq!(edits.get(&RegProp::Clock), Some(&Some("clock myclk".to_owned())));
        assert_eq!(edits.get(&RegProp::Reset), Some(&Some("hwReset myrst".to_owned())));
        assert_eq!(edits.get(&RegProp::External), Some(&Some("external".to_owned())));
    }

    /// Editing the primary interrupt's trigger/clear/enable/mask/pending mutates
    /// `RegDef.interrupt[0]`, records the single `interrupt:` line as a prop edit (via the
    /// generic `RegProp::ALL` diff — no interrupt-specific plumbing needed there), and refreshes
    /// every non-overriding field's baked trigger (`event_0`) while leaving an explicitly
    /// overriding field (`event_1`) alone.
    #[test]
    fn apply_update_regdef_edits_interrupt_settings() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let d = def_regdef(&v, "interrupt");
        let intr = d.interrupt.first().expect("interrupt register starts with a primary interrupt");
        assert_eq!(intr.trigger, InterruptTrigger::Rising);
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "interrupt".to_owned(),
            inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "interrupt".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr:Some(Some(RegIntrVals {
                    trigger: InterruptTrigger::Falling,
                    clear: InterruptClr::Read,
                    enable: Some(ResetValP::Unsigned(5)),
                    mask: None,
                    pending: false,
                })),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "interrupt");
        let intr2 = d2.interrupt.first().unwrap();
        assert_eq!(intr2.trigger, InterruptTrigger::Falling);
        assert_eq!(intr2.clear, InterruptClr::Read);
        assert_eq!(intr2.enable, Some(ResetValP::Unsigned(5)));
        assert_eq!(intr2.mask, None);
        assert!(!intr2.pending);

        // event_0 had no override: refreshed to the new default
        let event_0 = d2.fields.iter().find(|f| f.name == "event_0").unwrap();
        assert_eq!(event_0.hw_kind.first(), Some(&FieldHwKind::Interrupt(InterruptTrigger::Falling)));
        // event_1 overrides its own trigger: untouched by the register-level change
        let event_1 = d2.fields.iter().find(|f| f.name == "event_1").unwrap();
        assert_eq!(event_1.hw_kind.first(), Some(&FieldHwKind::Interrupt(InterruptTrigger::High)));

        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::Interrupt), Some(&Some("interrupt falling rclr en=5".to_owned())));
        assert!(v.has_unsaved());
    }

    /// Checking "Interrupt register" on a plain register (`RegDefVals.intr: Some(Some(..))` with
    /// `RegDef.interrupt` starting empty) creates the primary interrupt from scratch and, via the
    /// same `refresh_intr_defaults` call as the settings-change case above, bakes the new
    /// trigger/clear into every one of its fields (none of which carry an override yet).
    #[test]
    fn apply_update_regdef_edits_converts_plain_register_to_interrupt() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let d = def_regdef(&v, "basic_rw");
        assert!(d.interrupt.is_empty(), "basic_rw starts as a plain register");
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: Some( Some(RegIntrVals {
                    trigger: InterruptTrigger::Edge,
                    clear: InterruptClr::Write1,
                    enable: Some(ResetValP::Unsigned(0)),
                    mask: None,
                    pending: true,
                })),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "basic_rw");
        let intr2 = d2.interrupt.first().expect("interrupt just created");
        assert_eq!(intr2.trigger, InterruptTrigger::Edge);
        assert_eq!(intr2.clear, InterruptClr::Write1);
        assert_eq!(intr2.enable, Some(ResetValP::Unsigned(0)));
        assert!(intr2.pending);

        // Every pre-existing field had no override, so all three are baked to the new default.
        for name in ["field0", "field1", "field2"] {
            let f = d2.fields.iter().find(|f| f.name == name).unwrap();
            assert_eq!(f.hw_kind.first(), Some(&FieldHwKind::Interrupt(InterruptTrigger::Edge)), "{name}");
            assert_eq!(f.sw_kind, FieldSwKind::W1Clr, "{name}");
        }

        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::Interrupt), Some(&Some("interrupt edge w1clr en=0 pending".to_owned())));
        assert!(v.has_unsaved());
    }

    /// Unchecking "Interrupt register" on the `interrupt` register removes `RegDef.interrupt`
    /// entirely and, since nothing else would otherwise notice these are now orphaned, explicitly
    /// schedules removal of: the register's own `interrupt:` line (via the generic `RegProp::ALL`
    /// diff, same as the settings-change case), its `{enable,mask,pending}.description:` blocks,
    /// each field's own `interrupt ...` trigger/clear override line (`event_1`), and any field-
    /// level description override for a derived kind (set up here on `event_1` first, to exercise
    /// that path too). Every field is also reverted to a plain field: no `Interrupt` `hw_kind`,
    /// default `sw_kind`, no override, no derived-kind description.
    #[test]
    fn apply_update_regdef_edits_removes_interrupt() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateFieldIntrDesc {
            rif_type: rif_type.clone(),
            reg_type: "interrupt".to_owned(),
            field_name: "event_1".to_owned(),
            kind: InterruptRegKind::Pending,
            desc: "event_1's own pending description".to_owned(),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d = def_regdef(&v, "interrupt");
        assert!(!d.interrupt.is_empty(), "interrupt register starts with a primary interrupt");
        let decl_line = d.src.decl_line.unwrap();
        let event_0_line = def_field(&v, "interrupt", "event_0").src.decl_line.unwrap();
        let event_1_line = def_field(&v, "interrupt", "event_1").src.decl_line.unwrap();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "interrupt".to_owned(),
            inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "interrupt".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: Some(None),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "interrupt");
        assert!(d2.interrupt.is_empty(), "interrupt removed");
        for name in ["event_0", "event_1"] {
            let f = d2.fields.iter().find(|f| f.name == name).unwrap();
            assert!(f.hw_kind.is_empty(), "{name} reverted to a plain field");
            assert_eq!(f.sw_kind, FieldSwKind::default(), "{name}");
            assert_eq!(f.intr_ovr, InterruptInfoField::default(), "{name}");
            assert!(f.intr_desc.is_none(), "{name}");
        }

        let reg_edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(reg_edits.get(&RegProp::Interrupt), Some(&None), "interrupt line deleted");

        let reg_desc_edits = v.reg_intr_desc_block_edits.get(&decl_line)
            .expect("register-level description removals recorded");
        for kind in [InterruptRegKind::Enable, InterruptRegKind::Mask, InterruptRegKind::Pending] {
            assert_eq!(reg_desc_edits.get(&kind), Some(&None), "{kind:?} description block deleted");
        }

        let field_edits = v.prop_edits.get(&event_1_line).expect("field prop edit recorded");
        assert_eq!(field_edits.get(&FieldProp::Interrupt), Some(&None), "event_1's own override line deleted");
        assert!(!v.prop_edits.contains_key(&event_0_line),
            "event_0 never had its own override line, so nothing to delete");

        let field_desc_edits = v.field_intr_desc_block_edits.get(&event_1_line)
            .expect("event_1's own description-block removal recorded");
        assert_eq!(field_desc_edits.get(&InterruptRegKind::Pending), Some(&None));

        assert!(v.has_unsaved());
    }

    /// Filling in the "Alt interrupt name" box on the `interrupt` register (which starts with a
    /// primary interrupt and no alt, `RegDef.interrupt.len() == 1`) creates a second, named block
    /// at `RegDef.interrupt[1]` and records its own `alt ...` line via the same generic
    /// `RegProp::ALL` diff the primary interrupt line already uses (no interrupt-alt-specific
    /// plumbing needed in `file_io.rs`). Unlike creating/removing the *primary*, this never
    /// touches any field — alt shares the primary's event bits, it's a register-level-only
    /// concern (no `refresh_intr_defaults`, no field cascade).
    #[test]
    fn apply_update_regdef_edits_creates_alt_interrupt() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let d = def_regdef(&v, "interrupt");
        assert_eq!(d.interrupt.len(), 1, "interrupt register starts with a primary interrupt and no alt");
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "interrupt".to_owned(),
            inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                alt: Some(Some(RegAltVals {
                    name: "aux".to_owned(),
                    trigger: InterruptTrigger::Falling,
                    clear: InterruptClr::Read,
                    enable: Some(ResetValP::Unsigned(5)),
                    mask: None,
                    pending: false,
                })),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "interrupt");
        assert_eq!(d2.interrupt.len(), 2, "alt block created");
        let alt = &d2.interrupt[1];
        assert_eq!(alt.name, "aux");
        assert_eq!(alt.trigger, InterruptTrigger::Falling);
        assert_eq!(alt.clear, InterruptClr::Read);
        assert_eq!(alt.enable, Some(ResetValP::Unsigned(5)));
        assert_eq!(alt.mask, None);
        assert!(!alt.pending);
        // The primary interrupt and every field are untouched: alt never affects hw_kind/sw_kind
        assert_eq!(d2.interrupt[0].trigger, InterruptTrigger::Rising);
        let event_0 = d2.fields.iter().find(|f| f.name == "event_0").unwrap();
        assert_eq!(event_0.hw_kind.first(), Some(&FieldHwKind::Interrupt(InterruptTrigger::Rising)));

        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::InterruptAlt), Some(&Some("alt aux falling rclr en=5".to_owned())));
        assert!(v.has_unsaved());
    }

    /// Editing an already-existing alt block's settings updates `RegDef.interrupt[1]` in place and
    /// re-emits its `alt ...` line — mirrors `apply_update_regdef_edits_interrupt_settings`, one
    /// Vec slot over.
    #[test]
    fn apply_update_regdef_edits_updates_alt_interrupt_settings() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let rif_type = v.selected_rif_type();
        let d = def_regdef(&v, "interrupt");
        let decl_line = d.src.decl_line.unwrap();

        // First create the alt block.
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(), orig_name: "interrupt".to_owned(), inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: None, intr: None,
                alt: Some(Some(RegAltVals {
                    name: "aux".to_owned(), trigger: InterruptTrigger::High, clear: InterruptClr::Hw,
                    enable: None, mask: None, pending: false,
                })),
                array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        // Then change its settings (mask + pending), keeping the same name/trigger/clear.
        let d2 = def_regdef(&v, "interrupt");
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(), orig_name: "interrupt".to_owned(), inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(), group: None, desc: d2.description.get(true), pulse: d2.pulse.clone(),
                addr: None, intr: None,
                alt: Some(Some(RegAltVals {
                    name: "aux".to_owned(), trigger: InterruptTrigger::High, clear: InterruptClr::Hw,
                    enable: None, mask: Some(ResetValP::Unsigned(0xF)), pending: true,
                })),
                array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d3 = def_regdef(&v, "interrupt");
        assert_eq!(d3.interrupt.len(), 2, "still exactly one alt block");
        let alt = &d3.interrupt[1];
        assert_eq!(alt.mask, Some(ResetValP::Unsigned(0xF)));
        assert!(alt.pending);

        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::InterruptAlt), Some(&Some("alt aux high hwclr mask=0xf pending".to_owned())));
        assert!(v.has_unsaved());
    }

    /// Clearing the "Alt interrupt name" box (blank name, `vals.alt: Some(None)`) removes
    /// `RegDef.interrupt[1]` and its `alt ...` line, leaving the primary interrupt
    /// (`interrupt[0]`) and every field completely untouched.
    #[test]
    fn apply_update_regdef_edits_removes_alt_interrupt() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let rif_type = v.selected_rif_type();
        let d = def_regdef(&v, "interrupt");
        let decl_line = d.src.decl_line.unwrap();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(), orig_name: "interrupt".to_owned(), inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: None, intr: None,
                alt: Some(Some(RegAltVals {
                    name: "aux".to_owned(), trigger: InterruptTrigger::High, clear: InterruptClr::Hw,
                    enable: Some(ResetValP::Unsigned(1)), mask: None, pending: false,
                })),
                array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert_eq!(def_regdef(&v, "interrupt").interrupt.len(), 2);

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(), orig_name: "interrupt".to_owned(), inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: None, intr: None, alt: Some(None),
                array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "interrupt");
        assert_eq!(d2.interrupt.len(), 1, "alt removed, primary untouched");
        assert_eq!(d2.interrupt[0].trigger, InterruptTrigger::Rising, "primary interrupt unaffected");

        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::InterruptAlt), Some(&None), "alt line deleted");
        assert!(v.has_unsaved());
    }

    /// Unchecking "Interrupt register" while an alt block exists removes the primary AND the alt
    /// together (`regdef.interrupt.clear()` wipes the whole Vec) — exercises the
    /// `apply_regdef_vals` guard that skips alt handling once the primary-removal branch has
    /// already emptied `regdef.interrupt`, mirroring what `RegEditor::build_action` actually sends
    /// in this situation (`vals.alt: None`, since alt is gated on `has_intr`).
    #[test]
    fn apply_update_regdef_edits_removes_interrupt_also_clears_alt() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let rif_type = v.selected_rif_type();
        let d = def_regdef(&v, "interrupt");
        let decl_line = d.src.decl_line.unwrap();

        // First create an alt block.
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(), orig_name: "interrupt".to_owned(), inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: None, intr: None,
                alt: Some(Some(RegAltVals {
                    name: "aux".to_owned(), trigger: InterruptTrigger::High, clear: InterruptClr::Hw,
                    enable: None, mask: None, pending: false,
                })),
                array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert_eq!(def_regdef(&v, "interrupt").interrupt.len(), 2);

        // Then remove the primary, as unchecking "Interrupt register" would.
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(), orig_name: "interrupt".to_owned(), inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: None, intr: Some(None), alt: None,
                array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "interrupt");
        assert!(d2.interrupt.is_empty(), "both primary and alt removed together");

        let edits = v.reg_prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&RegProp::Interrupt), Some(&None), "interrupt line deleted");
        assert_eq!(edits.get(&RegProp::InterruptAlt), Some(&None), "alt line deleted too");
    }

    /// A derived (enable/mask/pending) register's own description lives on the primary
    /// interrupt's `InterruptInfo`, edited via the dedicated `UpdateRegIntrDesc` action — separate
    /// from `UpdateRegDef` since none of a derived register's other properties are editable.
    #[test]
    fn apply_update_reg_intr_desc_edits_enable_description() {
        let mut v = load_with_basic_rw_selected();
        v.selected.item = SelectedItem::None;
        let d = def_regdef(&v, "interrupt");
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegIntrDesc {
            rif_type: rif_type.clone(),
            reg_type: "interrupt".to_owned(),
            kind: InterruptRegKind::Enable,
            desc: "New enable description\nWith a second line.".to_owned(),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "interrupt");
        assert_eq!(d2.interrupt.first().unwrap().description.enable.get(true),
            "New enable description\nWith a second line.");
        assert!(v.reg_intr_desc_block_edits.get(&decl_line)
            .is_some_and(|k| k.contains_key(&InterruptRegKind::Enable)));
        assert!(v.has_unsaved());
    }

    /// A field's own trigger override (`Field::intr_ovr`) is applied via `set_intr_ovr`, which
    /// falls back to the register's current default for whichever half has no override — and,
    /// unlike the trigger itself, correctly *resets* `hw_kind` back to that default once the
    /// override is cleared, rather than leaving the last override value stuck in place.
    #[test]
    fn apply_update_field_sets_and_clears_intr_override() {
        let mut v = load_with_basic_rw_selected();
        let f = def_field(&v, "interrupt", "event_0");
        assert_eq!(f.intr_ovr, InterruptInfoField::default(), "event_0 starts without an override");
        let decl_line = f.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();
        let reg_default = InterruptInfoField { trigger: Some(InterruptTrigger::Rising), clear: Some(InterruptClr::Write1) };
        let base_vals = |intr_ovr, changed| FieldVals {
            name: "event_0".to_owned(),
            pos: f.pos.clone(),
            array: f.array.clone(),
            array_pos_incr: f.array_pos_incr,
            reset: None,
            signed: f.signed,
            sw_kind: f.sw_kind.clone(),
            desc: f.description.get(true),
            hw_acc: f.hw_acc,
            visibility: None,
            nb_frac: f.nb_frac,
            lock: f.lock.clone(),
            limit: f.limit.clone(),
            counter: None,
            password: None,
            enum_kind: f.enum_kind.clone(),
            intr_ovr,
            reg_intr_default: reg_default.clone(),
            changed,
        };

        // Override just the trigger
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "interrupt".to_owned(),
            orig_name: "event_0".to_owned(),
            vals: Box::new(base_vals(
                Some(InterruptInfoField { trigger: Some(InterruptTrigger::Low), clear: None }),
                vec![FieldProp::Interrupt],
            )),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let f2 = def_field(&v, "interrupt", "event_0");
        assert_eq!(f2.intr_ovr, InterruptInfoField { trigger: Some(InterruptTrigger::Low), clear: None });
        assert_eq!(f2.hw_kind.first(), Some(&FieldHwKind::Interrupt(InterruptTrigger::Low)));
        let edits = v.prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits.get(&FieldProp::Interrupt), Some(&Some("interrupt low".to_owned())));

        // Clear the override: resets to the register's default, not a stale value
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "interrupt".to_owned(),
            orig_name: "event_0".to_owned(),
            vals: Box::new(base_vals(Some(InterruptInfoField::default()), vec![FieldProp::Interrupt])),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let f3 = def_field(&v, "interrupt", "event_0");
        assert_eq!(f3.intr_ovr, InterruptInfoField::default());
        assert_eq!(f3.hw_kind.first(), Some(&FieldHwKind::Interrupt(InterruptTrigger::Rising)));
        let edits2 = v.prop_edits.get(&decl_line).expect("prop edit recorded");
        assert_eq!(edits2.get(&FieldProp::Interrupt), Some(&None), "cleared override deletes the line");
    }

    /// Renaming a register cascades into every `instances:` entry that references it by type
    /// name: a distinctly-named instance (`ctrl_a`/`ctrl_b`, both typed `ctrl`) only has its
    /// `type_name` updated, while a bare instance whose display name only ever mirrored the type
    /// (`status`) has both `inst_name` and `type_name` follow the rename — otherwise it would
    /// leak the old name back as a spurious explicit `(group)`/`=type` clause (the same class of
    /// bug `RegInst::fmt_decl`'s rename-gotcha note warns about at the library level).
    #[test]
    fn apply_update_regdef_rename_cascades_into_manual_instances() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_regdef_rename_cascade.rif");
        write_mini_manual_fixture(&tmp);

        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        // Rename the shared type: both named instances follow, by type only.
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "ctrl".to_owned(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegDefVals { name: "ctrl2".to_owned(), group: None, desc: "Control register".to_owned(), pulse: vec![], addr: None, intr: None, alt: None, array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            let page = &rif.pages[0];
            let ctrl_a = page.instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
            assert_eq!(ctrl_a.type_name, "ctrl2");
            let ctrl_b = page.instances.iter().find(|i| i.inst_name == "ctrl_b").unwrap();
            assert_eq!(ctrl_b.type_name, "ctrl2");
            // The unrelated bare instance is untouched
            assert!(page.instances.iter().any(|i| i.inst_name == "status" && i.type_name == "status"));
        }

        // Rename the bare (unnamed) instance's type: its display name follows too.
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "status".to_owned(),
            inst_name: "status".to_owned(),
            vals: RegDefVals { name: "status2".to_owned(), group: None, desc: "Status register".to_owned(), pulse: vec![], addr: None, intr: None, alt: None, array: None, visibility: Visibility::Full, clk: None, rst: None, external: ExternalKind::None },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            let page = &rif.pages[0];
            let status = page.instances.iter().find(|i| i.type_name == "status2").expect("cascaded");
            assert_eq!(status.inst_name, "status2", "bare instance's display name must follow the rename too");
        }

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: renaming a register and toggling its pulse, then `save_file`, rewrites the
    /// declaration line and inserts the new pulse sub-property line, keeps sibling registers /
    /// fields / comments byte-identical, and re-parses.
    #[test]
    fn save_file_persists_regdef_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_regdef_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let d = def_regdef(&v, "basic_rw");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type,
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw_renamed".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: vec![RegPulseKind::Write("clk_rif".to_owned()), RegPulseKind::Access(String::new())],
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- basic_rw_renamed : \"Simple register with r/w fields\""), "got:\n{after}");
        assert!(after.contains("accPulse comb"), "got:\n{after}");
        // Sibling register, its fields, and an unrelated comment are untouched
        assert!(after.contains("- reg_group0 : (reg_group) \"Group multiple registers in one struct (first half)\""));
        assert!(after.contains("- field0 = 0      7:0  \"Field 8b\""));
        assert!(after.contains("// Main parameters"));
        assert!(!v.has_unsaved(), "reg_dirty/reg_prop_edits must be cleared after a successful save");

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: setting visibility/clock/reset/external on a register that starts with none
    /// of them (basic_rw) inserts four new sub-property lines right after the declaration,
    /// leaves sibling content untouched, and the saved file still re-parses with the new values —
    /// mirrors `save_file_persists_regdef_edits_to_disk`, one property group over.
    #[test]
    fn save_file_persists_regdef_advanced_props_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_regdef_advanced_props_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let d = def_regdef(&v, "basic_rw");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Hidden,
                clk: Some("myclk".to_owned()),
                rst: Some("myrst".to_owned()),
                external: ExternalKind::ReadWrite,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("hidden"), "got:\n{after}");
        assert!(after.contains("clock myclk"), "got:\n{after}");
        assert!(after.contains("hwReset myrst"), "got:\n{after}");
        assert!(after.contains("external"), "got:\n{after}");
        // Sibling register, its fields, and an unrelated comment are untouched
        assert!(after.contains("- reg_group0 : (reg_group) \"Group multiple registers in one struct (first half)\""));
        assert!(after.contains("- field0 = 0      7:0  \"Field 8b\""));
        assert!(after.contains("// Main parameters"));
        assert!(!v.has_unsaved(), "reg_dirty/reg_prop_edits must be cleared after a successful save");

        // Re-parsing from scratch confirms the new lines are valid, correctly-anchored grammar,
        // not just text that happens to satisfy the `contains` checks above.
        let mut v2 = RifViewer::default();
        v2.file_path = tmp.clone();
        v2.open_file();
        assert!(v2.rif_comp.is_some(), "edited file should re-parse: {:?}", v2.last_err);
        let d2 = def_regdef(&v2, "basic_rw");
        assert_eq!(d2.visibility, Visibility::Hidden);
        assert_eq!(d2.clk, Some("myclk".to_owned()));
        assert_eq!(d2.rst, Some("myrst".to_owned()));
        assert_eq!(d2.external, ExternalKind::ReadWrite);

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: editing the primary interrupt's trigger, a derived register's own
    /// `enable.description:` block, and a field's own trigger override, all through the real
    /// `apply_pending`/`save_file` pipeline (not hand-rolled reconciliation) — proves the new
    /// `reg_intr_desc_block_edits`/`field_intr_desc_block_edits` maps are actually wired into
    /// `save_rif_file`, not just populated and never read.
    #[test]
    fn save_file_persists_interrupt_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_interrupt_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let d = def_regdef(&v, "interrupt");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "interrupt".to_owned(),
            inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "interrupt".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: Some(Some(RegIntrVals {
                    trigger: InterruptTrigger::Falling,
                    clear: InterruptClr::Read,
                    enable: Some(ResetValP::Unsigned(0x1337)),
                    mask: Some(ResetValP::Unsigned(0xCAFE)),
                    pending: true,
                })),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        v.pending = Some(EditAction::UpdateRegIntrDesc {
            rif_type: rif_type.clone(),
            reg_type: "interrupt".to_owned(),
            kind: InterruptRegKind::Enable,
            desc: "Updated enable description\nWith a second line.".to_owned(),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let f = def_field(&v, "interrupt", "event_0");
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "interrupt".to_owned(),
            orig_name: "event_0".to_owned(),
            vals: Box::new(FieldVals {
                name: "event_0".to_owned(),
                pos: f.pos.clone(),
                array: f.array.clone(),
                array_pos_incr: f.array_pos_incr,
                reset: None,
                signed: f.signed,
                sw_kind: f.sw_kind.clone(),
                desc: f.description.get(true),
                hw_acc: f.hw_acc,
                visibility: None,
                nb_frac: f.nb_frac,
                lock: f.lock.clone(),
                limit: f.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f.enum_kind.clone(),
                intr_ovr: Some(InterruptInfoField { trigger: Some(InterruptTrigger::Low), clear: None }),
                reg_intr_default: InterruptInfoField { trigger: Some(InterruptTrigger::Falling), clear: Some(InterruptClr::Read) },
                changed: vec![FieldProp::Interrupt],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("interrupt falling rclr en=0x1337 mask=0xcafe pending"), "got:\n{after}");
        assert!(after.contains("Updated enable description"), "got:\n{after}");
        assert!(after.contains("With a second line."), "got:\n{after}");
        assert!(!after.contains("Enable interrupt. When set to 0"), "old enable description lingering:\n{after}");
        assert!(after.contains("interrupt low"), "event_0's trigger override not persisted:\n{after}");
        // Sibling content untouched: the other two descriptions, event_1's own override, the next register
        assert!(after.contains("pending.description : Pending interrupt (AND between status and mask)"));
        assert!(after.contains("mask.description    : Mask interrupt from the global IRQ output"));
        assert!(after.contains("interrupt high"));
        assert!(after.contains("- reg_fields_a0 : (reg_fields_a)"));
        assert!(!v.has_unsaved(), "every pending map must be cleared after a successful save");

        let d2 = def_regdef(&v, "interrupt");
        let intr2 = d2.interrupt.first().unwrap();
        assert_eq!(intr2.trigger, InterruptTrigger::Falling);
        assert_eq!(intr2.description.enable.get(true), "Updated enable description\nWith a second line.");
        let event_0 = def_field(&v, "interrupt", "event_0");
        assert_eq!(event_0.intr_ovr, InterruptInfoField { trigger: Some(InterruptTrigger::Low), clear: None });

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end mirror of `save_file_persists_interrupt_edits_to_disk`, for the removal
    /// direction: unchecking "Interrupt register" on `interrupt` through the real
    /// `apply_pending`/`save_file` pipeline must delete the register's `interrupt:` line, all
    /// three `{enable,mask,pending}.description:` lines, and `event_1`'s own `interrupt high`
    /// override line — and, on reopening the saved file, reparse both fields as plain (no
    /// `Interrupt` `hw_kind`), proving nothing was left inconsistent between session state and
    /// the file on disk.
    #[test]
    fn save_file_persists_interrupt_removal_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_interrupt_removal.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let d = def_regdef(&v, "interrupt");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "interrupt".to_owned(),
            inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "interrupt".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: Some(None),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(!after.contains("interrupt rising w1clr"), "interrupt line lingering:\n{after}");
        assert!(!after.contains("pending.description"), "pending description lingering:\n{after}");
        assert!(!after.contains("enable.description"), "enable description lingering:\n{after}");
        assert!(!after.contains("mask.description"), "mask description lingering:\n{after}");
        assert!(!after.contains("interrupt high"), "event_1's own override lingering:\n{after}");
        // Sibling content untouched
        assert!(after.contains("- event_0 = 0   0:0 \"Event 0\""));
        assert!(after.contains("- event_1 = 0   1:1 \"Event 1 using non-default setting\""));
        assert!(after.contains("- reg_fields_a0 : (reg_fields_a)"));
        assert!(!v.has_unsaved(), "every pending map must be cleared after a successful save");

        v.open_file();
        assert!(v.rif_comp.is_some(), "reopened file should still compile: {:?}", v.last_err);
        let d2 = def_regdef(&v, "interrupt");
        assert!(d2.interrupt.is_empty(), "reparsed register is plain");
        for name in ["event_0", "event_1"] {
            let f = d2.fields.iter().find(|f| f.name == name).unwrap();
            assert!(f.hw_kind.is_empty(), "{name} reparsed with no Interrupt hw_kind");
        }

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: adding a brand-new alt block through the real `apply_pending`/`save_file`
    /// pipeline inserts a fresh `alt ...` line (there's no existing line to replace, unlike the
    /// primary-interrupt edit tests above) right after the register's own `interrupt:` line, and
    /// leaves every sibling line (the primary's own descriptions, the fields, the next register)
    /// untouched. Proves the generic `RegProp::ALL`/`reg_prop_edits` insert path — already relied
    /// on by every other `RegProp` variant — also handles `InterruptAlt` for free.
    #[test]
    fn save_file_persists_alt_interrupt_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_alt_interrupt_add.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let d = def_regdef(&v, "interrupt");
        assert_eq!(d.interrupt.len(), 1, "sanity: interrupt starts with a primary interrupt and no alt");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "interrupt".to_owned(),
            inst_name: "interrupt".to_owned(),
            vals: RegDefVals {
                name: "interrupt".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                alt: Some(Some(RegAltVals {
                    name: "aux".to_owned(),
                    trigger: InterruptTrigger::Falling,
                    clear: InterruptClr::Write0,
                    enable: Some(ResetValP::Unsigned(1)),
                    mask: None,
                    pending: false,
                })),
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("alt aux falling w0clr en=1"), "got:\n{after}");
        // Sibling content untouched: the primary interrupt line itself, its descriptions, both
        // fields, and the next register.
        assert!(after.contains("interrupt rising w1clr enable=0x1337 mask=0xCAFE pending"));
        assert!(after.contains("pending.description : Pending interrupt (AND between status and mask)"));
        assert!(after.contains("- event_0 = 0   0:0 \"Event 0\""));
        assert!(after.contains("- event_1 = 0   1:1 \"Event 1 using non-default setting\""));
        assert!(after.contains("- reg_fields_a0 : (reg_fields_a)"));
        assert!(!v.has_unsaved(), "every pending map must be cleared after a successful save");

        v.open_file();
        assert!(v.rif_comp.is_some(), "reopened file should still compile: {:?}", v.last_err);
        let d2 = def_regdef(&v, "interrupt");
        assert_eq!(d2.interrupt.len(), 2, "alt block reparsed");
        assert_eq!(d2.interrupt[1].name, "aux");

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: explicitly editing a register's group name (the "Group" row's text buffer,
    /// as opposed to a group merely following along with a register rename — see
    /// `grouped_register_group_type_differs_from_reg_type_and_drives_enum_naming`) rewrites just
    /// that register's declaration line, leaves its former group-mate untouched, and re-parses
    /// with the new group.
    #[test]
    fn save_file_persists_explicit_group_edit_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_group_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let d = def_regdef(&v, "reg_group0");
        assert_eq!(d.group.name, "reg_group", "sanity: reg_group0 starts in the shared 'reg_group' group");
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type,
            orig_name: "reg_group0".to_owned(),
            inst_name: "reg_group0".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "reg_group0".to_owned(),
                group: Some("reg_group_new".to_owned()),
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        let d2 = def_regdef(&v, "reg_group0");
        assert_eq!(d2.group.name, "reg_group_new");

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- reg_group0 : (reg_group_new) \"Group multiple registers in one struct (first half)\""), "got:\n{after}");
        // Former group-mate is untouched
        assert!(after.contains("- reg_group1 : (reg_group) \"Group multiple registers in one struct (second half)\""));
        assert!(!v.has_unsaved(), "reg_dirty/reg_prop_edits must be cleared after a successful save");

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end through the real UI wiring: growing field0's existing description block,
    /// giving field1 (which has none) a brand new one, and clearing basic_rw's own block all
    /// persist correctly in one save — siblings/comments untouched, and the file re-parses with
    /// the new content.
    #[test]
    fn save_file_persists_description_block_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_desc_block_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        let d = def_regdef(&v, "basic_rw");
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: "Simple register with r/w fields".to_owned(), // clears the block
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let f0 = def_field(&v, "basic_rw", "field0");
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "basic_rw".to_owned(),
            orig_name: "field0".to_owned(),
            vals: Box::new(FieldVals {
                name: "field0".to_owned(),
                pos: f0.pos.clone(),
                array: f0.array.clone(),
                array_pos_incr: f0.array_pos_incr,
                reset: None,
                signed: f0.signed,
                sw_kind: f0.sw_kind.clone(),
                desc: "Field 8b\nMore detailled information on current field.\nNow with a third line.".to_owned(),
                hw_acc: f0.hw_acc,
                visibility: None,
                nb_frac: f0.nb_frac,
                lock: f0.lock.clone(),
                limit: f0.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f0.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let f1 = def_field(&v, "basic_rw", "field1");
        assert!(f1.src.desc_blocks.get(&DescBlockKind::Public).is_none(), "field1 starts with no description block");
        v.pending = Some(EditAction::UpdateField {
            rif_type: rif_type.clone(),
            reg_type: "basic_rw".to_owned(),
            orig_name: "field1".to_owned(),
            vals: Box::new(FieldVals {
                name: "field1".to_owned(),
                pos: f1.pos.clone(),
                array: f1.array.clone(),
                array_pos_incr: f1.array_pos_incr,
                reset: None,
                signed: f1.signed,
                sw_kind: f1.sw_kind.clone(),
                desc: "Signed Field\nBrand new block for a field that had none.".to_owned(),
                hw_acc: f1.hw_acc,
                visibility: None,
                nb_frac: f1.nb_frac,
                lock: f1.lock.clone(),
                limit: f1.limit.clone(),
                counter: None,
                password: None,
                enum_kind: f1.enum_kind.clone(),
                intr_ovr: None,
                reg_intr_default: InterruptInfoField::default(),
                changed: vec![],
            }),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(!after.contains("More detailled information on current register"),
            "register block not cleared:\n{after}");
        assert!(after.contains("Now with a third line."), "field0 block not grown:\n{after}");
        assert!(after.contains("Brand new block for a field that had none."), "field1 block not inserted:\n{after}");
        // Declaration lines, sibling register/fields and an unrelated comment are untouched
        assert!(after.contains("- basic_rw: \"Simple register with r/w fields\""));
        assert!(after.contains("- field0 = 0      7:0  \"Field 8b\""));
        assert!(after.contains("- field1 = 1     15:8  \"Signed Field\""));
        assert!(after.contains("- reg_group0 : (reg_group) \"Group multiple registers in one struct (first half)\""));
        assert!(after.contains("// Main parameters"));
        assert!(!v.has_unsaved(), "desc_block_edits/reg_desc_block_edits must be cleared after a successful save");

        // Re-parses with the new content
        let d2 = def_regdef(&v, "basic_rw");
        assert_eq!(d2.description.get(true), "Simple register with r/w fields");
        let f0_2 = def_field(&v, "basic_rw", "field0");
        assert_eq!(f0_2.description.get(true),
            "Field 8b\nMore detailled information on current field.\nNow with a third line.");
        let f1_2 = def_field(&v, "basic_rw", "field1");
        assert_eq!(f1_2.description.get(true),
            "Signed Field\nBrand new block for a field that had none.");

        fs::remove_file(&tmp).ok();
    }

    // ---- Page-level auto -> manual conversion (wiring around `RifPageInst::convert_to_manual`) ----

    fn compiled_page(v: &RifViewer, page_name: &str) -> RifPageInst {
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        rif.pages.iter().find(|p| p.name == page_name)
            .unwrap_or_else(|| panic!("{page_name} exists")).clone()
    }

    /// Converting `test.rif`'s (genuinely automatic) `Main` page mutates the source page to
    /// manual and, after recompiling from scratch, every register's name/address is unchanged —
    /// exercising the same property `rifgen`'s own `convert_to_manual_preserves_every_compiled_
    /// register` test already locks in at the library level, but through the actual UI wiring
    /// (`EditAction`/`apply_pending`) rather than calling the library function directly.
    #[test]
    fn apply_convert_page_to_manual_preserves_addresses() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        assert!(v.rif_comp.is_some(), "test.rif should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let before = compiled_page(&v, "Main");
        assert!(before.regs.len() >= 11, "sanity: expected at least 11 compiled registers");

        v.pending = Some(EditAction::ConvertPageToManual {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            compiled: Box::new(before.clone()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "conversion should succeed: {:?}", v.edit_err);

        {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            let page = rif.pages.iter().find(|p| p.name == "Main").unwrap();
            assert!(!page.is_auto());
            assert_eq!(page.instances.len(), before.regs.len());
        }

        let after = compiled_page(&v, "Main");
        assert_eq!(after.regs.len(), before.regs.len());
        for (b, a) in before.regs.iter().zip(after.regs.iter()) {
            assert_eq!(b.reg_name, a.reg_name);
            assert_eq!(b.addr, a.addr, "address changed for {}", b.reg_name);
        }
        assert!(v.has_unsaved(), "the new (unsaved) instance declarations should count as unsaved");
    }

    /// Converting an already-manual page is refused by the library, and the wiring surfaces that
    /// refusal as `edit_err` rather than panicking or silently doing nothing.
    #[test]
    fn apply_convert_page_to_manual_surfaces_refusal_as_edit_err() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        let rif_type = v.selected_rif_type();
        let compiled = compiled_page(&v, "Main");

        v.pending = Some(EditAction::ConvertPageToManual {
            rif_type: rif_type.clone(), page_name: "Main".to_owned(), compiled: Box::new(compiled.clone()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "first conversion should succeed: {:?}", v.edit_err);

        v.pending = Some(EditAction::ConvertPageToManual {
            rif_type, page_name: "Main".to_owned(), compiled: Box::new(compiled)
        });
        v.apply_pending();
        assert!(v.edit_err.as_ref().is_some_and(|e| e.contains("already using manual")), "got: {:?}", v.edit_err);
    }

    /// End-to-end: converting `Main` to manual then `save_file` drops the `auto` keyword from
    /// the `instances:` header and writes one declaration line per register (in address order),
    /// keeps register/field declarations untouched, and re-parses still manual (not silently
    /// reverted) with the same instance count.
    #[test]
    fn save_file_persists_converted_page_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_convert_page.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let before = compiled_page(&v, "Main");

        v.pending = Some(EditAction::ConvertPageToManual {
            rif_type,
            page_name: "Main".to_owned(),
            compiled: Box::new(before.clone()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "conversion should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        assert!(!v.has_unsaved(), "the converted instances must no longer count as new after reparse");

        let after_text = fs::read_to_string(&tmp).unwrap();
        assert!(!after_text.contains("instances: auto"), "got:\n{after_text}");
        assert!(after_text.contains("- basic_rw @ 0x0"), "got:\n{after_text}");
        // Register/field declarations are untouched
        assert!(after_text.contains("- reg_group0 : (reg_group) \"Group multiple registers in one struct (first half)\""));
        assert!(after_text.contains("- field0 = 0      7:0  \"Field 8b\""));

        // Re-parsed source is still manual, not silently reverted, with the same instance count
        let rif2 = get_rif(&v.rif_src.as_ref().unwrap().rifs, "test_rif").unwrap();
        let page2 = rif2.pages.iter().find(|p| p.name == "Main").unwrap();
        assert!(!page2.is_auto());
        assert_eq!(page2.instances.len(), before.regs.len());

        fs::remove_file(&tmp).ok();
    }

    // ---- Address editing (RegDefVals::addr / EditAction::UpdateRegDef's `inst_name` target) ----

    /// Editing an address right after converting a page (before ever saving) just mutates the
    /// in-memory `RegInst` — there's no source line to mark dirty yet (`rif_has_new_instances`'s
    /// path serializes the whole, already-current instance fresh on save regardless).
    #[test]
    fn apply_update_regdef_edits_address_right_after_conversion() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        let rif_type = v.selected_rif_type();
        let compiled = compiled_page(&v, "Main");
        v.pending = Some(EditAction::ConvertPageToManual {
            rif_type: rif_type.clone(), page_name: "Main".to_owned(), compiled: Box::new(compiled),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "conversion should succeed: {:?}", v.edit_err);

        let d = def_regdef(&v, "basic_rw");
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: Some(0x100),
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = rif.pages.iter().find(|p| p.name == "Main").unwrap();
        let inst = page.instances.iter().find(|i| i.inst_name == "basic_rw").unwrap();
        assert_eq!(inst.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x100)));
        assert!(inst.src.decl_line.is_none(), "not saved yet");
        assert!(v.reg_inst_dirty.is_empty(), "nothing to track for an instance with no source line yet");

        let after = compiled_page(&v, "Main");
        let basic_rw = after.regs.iter().find(|r| r.reg_name == "basic_rw").unwrap();
        assert_eq!(basic_rw.addr, 0x100);
    }

    /// On a page that's *already* manual with real source lines (not fresh from a conversion),
    /// editing one instance's address records that line in `reg_inst_dirty` and leaves every
    /// other instance (including the sibling sharing its type) untouched.
    #[test]
    fn apply_update_regdef_edits_address_on_already_manual_page() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_edit_addr_manual.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let d = def_regdef_in(&v, &rif_type, "ctrl");
        let ctrl_a_line = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap().src.decl_line.unwrap()
        };

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "ctrl".to_owned(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "ctrl".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: Some(0x20),
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        let ctrl_a = page.instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
        assert_eq!(ctrl_a.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x20)));
        // The sibling sharing the same type is untouched
        let ctrl_b = page.instances.iter().find(|i| i.inst_name == "ctrl_b").unwrap();
        assert_eq!(ctrl_b.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x4)));

        assert!(v.reg_inst_dirty.contains(&ctrl_a_line));
        assert!(v.has_unsaved());

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: editing an address on an already-manual, already-on-disk instance and saving
    /// rewrites just that instance's declaration line, leaves its sibling and the register
    /// definitions untouched, and re-parses with the new address.
    #[test]
    fn save_file_persists_address_edit_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_save_addr_edit.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let d = def_regdef_in(&v, &rif_type, "ctrl");

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type,
            orig_name: "ctrl".to_owned(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "ctrl".to_owned(),
                group: None,
                desc: d.description.get(true),
                pulse: d.pulse.clone(),
                addr: Some(0x20),
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        assert!(!v.has_unsaved());

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- ctrl_a = ctrl @ 0x20"), "got:\n{after}");
        // Sibling instance and the bare instance are untouched
        assert!(after.contains("- ctrl_b = ctrl @ 0x4"));
        assert!(after.contains("- status @ 0x8"));
        // Register definitions are untouched
        assert!(after.contains("- ctrl: \"Control register\""));
        assert!(after.contains("- en = 0 0:0 \"Enable\""));

        let rif2 = get_rif(&v.rif_src.as_ref().unwrap().rifs, "mini_rif").unwrap();
        let ctrl_a2 = rif2.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
        assert_eq!(ctrl_a2.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x20)));

        fs::remove_file(&tmp).ok();
    }

    // ---- Register reorder (address swap): is_reg_reorderable / EditAction::SwapRegAddr ----

    /// Reorderability requires: editable, not an array element, the owning page already manual,
    /// and the specific instance's address currently `Absolute` — mirrors the same reasoning
    /// `RifPageInst::convert_to_manual` and `RegInst::fmt_decl` already apply elsewhere.
    #[test]
    fn is_reg_reorderable_requires_manual_page_and_absolute_address() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_reorderable_check.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let manual_page = compiled_page(&v, "Main");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let ctrl_a = manual_page.regs.iter().find(|r| r.reg_name == "ctrl_a").unwrap();
        assert!(RifViewer::is_reg_reorderable(ctrl_a, Some(&src_page)), "manual + absolute should be reorderable");
        assert!(!RifViewer::is_reg_reorderable(ctrl_a, None), "no source page at all should not be reorderable");

        // The same register on a page still automatic (no source page found, or `is_auto()`)
        // is not reorderable — exercised directly against `test.rif`'s genuinely automatic page.
        let mut va = RifViewer::default();
        va.file_path = "../rifgen/test/test.rif".into();
        va.open_file();
        let rif_type_a = va.selected_rif_type();
        let auto_src_page = {
            let src = va.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type_a).unwrap().pages.iter().find(|p| p.name == "Main").unwrap().clone()
        };
        let auto_page = compiled_page(&va, "Main");
        let basic_rw = auto_page.regs.iter().find(|r| r.reg_name == "basic_rw").unwrap();
        assert!(!RifViewer::is_reg_reorderable(basic_rw, Some(&auto_src_page)), "automatic page should not be reorderable");

        fs::remove_file(&tmp).ok();
    }

    /// Swapping two instances' addresses exchanges the values outright (no gap-preserving math
    /// needed, unlike fields), records both source lines as dirty (both already on disk), and
    /// leaves the unrelated bare instance untouched.
    #[test]
    fn apply_swap_reg_addr_exchanges_addresses() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_swap_addr.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::SwapRegAddr {
            rif_type: rif_type.clone(),
            a_inst_name: "ctrl_a".to_owned(),
            b_inst_name: "ctrl_b".to_owned(),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        let ctrl_a = page.instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
        let ctrl_b = page.instances.iter().find(|i| i.inst_name == "ctrl_b").unwrap();
        assert_eq!(ctrl_a.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x4)));
        assert_eq!(ctrl_b.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x0)));
        // The unrelated bare instance is untouched
        let status = page.instances.iter().find(|i| i.inst_name == "status").unwrap();
        assert_eq!(status.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x8)));

        assert!(v.reg_inst_dirty.len() == 2, "both sides already had a source line");
        assert!(v.has_unsaved());

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: swapping two addresses and saving rewrites just those two declaration lines,
    /// leaves the unrelated instance and the register definitions untouched, and re-parses with
    /// the addresses exchanged.
    #[test]
    fn save_file_persists_swapped_addresses_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_swap_addr_save.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::SwapRegAddr {
            rif_type,
            a_inst_name: "ctrl_a".to_owned(),
            b_inst_name: "ctrl_b".to_owned(),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        assert!(!v.has_unsaved());

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- ctrl_a = ctrl @ 0x4"), "got:\n{after}");
        assert!(after.contains("- ctrl_b = ctrl @ 0x0"), "got:\n{after}");
        assert!(after.contains("- status @ 0x8"));
        assert!(after.contains("- ctrl: \"Control register\""));
        assert!(after.contains("- en = 0 0:0 \"Enable\""));

        let rif2 = get_rif(&v.rif_src.as_ref().unwrap().rifs, "mini_rif").unwrap();
        let page2 = &rif2.pages[0];
        assert_eq!(page2.instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap().addr,
            Address::new(AddressKind::Absolute, AddressOffset::Value(0x4)));
        assert_eq!(page2.instances.iter().find(|i| i.inst_name == "ctrl_b").unwrap().addr,
            Address::new(AddressKind::Absolute, AddressOffset::Value(0x0)));

        fs::remove_file(&tmp).ok();
    }

    // ---- Address-conflict resolution: alignment, Swap/Shift/Insert, and the save-flush fix ----

    /// Same layout as `write_mini_manual_fixture` but with a deliberate gap after `ctrl_c`
    /// (registers at 0x0, 0x4, 0x8, then a gap up to 0x28) — needed to distinguish "cascade
    /// pushes only as far as necessary" from "cascade pushes everything after the target".
    fn write_reorder_gap_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: mini_rif\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - ctrl: \"Control register\"\n",
            "        - en = 0 0:0 \"Enable\"\n",
            "      - status: \"Status register\"\n",
            "        - busy = 0 0:0 ro \"Busy\"\n",
            "    instances:\n",
            "      - ctrl_a = ctrl @ 0x0\n",
            "      - ctrl_b = ctrl @ 0x4\n",
            "      - ctrl_c = ctrl @ 0x8\n",
            "      - status @ 0x28\n",
        )).unwrap();
    }

    /// Same registers as `write_mini_manual_fixture`, but `ctrl_b` is `Relative` (`@+`) rather
    /// than `Absolute` — excluded from Swap/Shift/Insert for the same reason the existing
    /// address-swap arrows exclude it (`is_reg_reorderable`): moving it independently would
    /// silently move whatever follows it in its offset chain.
    fn write_relative_addr_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: mini_rif\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - ctrl: \"Control register\"\n",
            "        - en = 0 0:0 \"Enable\"\n",
            "      - status: \"Status register\"\n",
            "        - busy = 0 0:0 ro \"Busy\"\n",
            "    instances:\n",
            "      - ctrl_a = ctrl @ 0x0\n",
            "      - ctrl_b = ctrl @+0x4\n",
            "      - status @ 0x8\n",
        )).unwrap();
    }

    /// A misaligned address (not a multiple of the RIF's data width in bytes) is rejected before
    /// any collision/move logic even runs — mirrors the existing empty-name validation.
    #[test]
    fn regeditor_build_action_rejects_misaligned_address() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_alignment.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "ctrl");

        let mut ed = RegEditor::from_regdef(
            "ctrl_a".to_owned(), "ctrl".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x21".to_owned(); // not a multiple of 4
        assert!(ed.build_action().is_none(), "misaligned address must be rejected");
        assert!(ed.parse_err.as_deref().is_some_and(|e| e.contains("aligned")), "got: {:?}", ed.parse_err);

        fs::remove_file(&tmp).ok();
    }

    /// When the requested address isn't occupied by anything else, the change applies directly as
    /// a plain `UpdateRegDef` — no conflict, no modal (the "unambiguous" case).
    #[test]
    fn resolve_reg_addr_commit_applies_unambiguous_move_directly() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_unambiguous.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "ctrl");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef(
            "ctrl_a".to_owned(), "ctrl".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x20".to_owned(); // free slot, nothing collides
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(conflict.is_none(), "no collision expected");
        assert!(matches!(&action, Some(EditAction::UpdateRegDef { vals, .. }) if vals.addr == Some(0x20)),
            "expected a plain address update");

        fs::remove_file(&tmp).ok();
    }

    /// `UpdateRegDefWithMoves` applies the primary register's own field/address change AND its
    /// companion Swap atomically — bundling an unrelated description edit alongside proves the
    /// two don't interfere.
    #[test]
    fn apply_update_regdef_with_moves_swap_bundles_unrelated_field_edit() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_swap_bundle.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let d = def_regdef_in(&v, &rif_type, "ctrl");

        v.pending = Some(EditAction::UpdateRegDefWithMoves {
            rif_type: rif_type.clone(),
            orig_name: "ctrl".to_owned(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "ctrl".to_owned(),
                group: None,
                desc: "Updated description".to_owned(),
                pulse: d.pulse.clone(),
                addr: Some(0x4), // ctrl_b's current address
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
            companions: vec![("ctrl_b".to_owned(), 0x0)],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef_in(&v, &rif_type, "ctrl");
        assert_eq!(d2.description.get(true), "Updated description");

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        let ctrl_a = page.instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
        let ctrl_b = page.instances.iter().find(|i| i.inst_name == "ctrl_b").unwrap();
        assert_eq!(ctrl_a.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x4)));
        assert_eq!(ctrl_b.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x0)));
        let status = page.instances.iter().find(|i| i.inst_name == "status").unwrap();
        assert_eq!(status.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x8)));

        assert!(v.reg_inst_dirty.len() == 2, "both moved instances already had a source line");
        assert!(v.has_unsaved());

        fs::remove_file(&tmp).ok();
    }

    /// The Shift cascade only pushes as many registers as needed to resolve the collision — a
    /// register beyond a pre-existing gap is left untouched. Mirrors the confirmed worked example
    /// (registers at 0,1,2,10 with step 1; here rescaled to a 32-bit RIF's 4-byte step).
    #[test]
    fn resolve_reg_addr_commit_shift_stops_at_free_space() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_shift.rif");
        write_reorder_gap_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "ctrl");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef(
            "ctrl_b".to_owned(), "ctrl".to_owned(), &def, 0x4, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x8".to_owned(); // collides with ctrl_c
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none(), "a collision must pop the modal, not apply directly");
        let pc = conflict.expect("collision expected");
        assert_eq!(pc.shift, Some(vec![("ctrl_c".to_owned(), 0xc)]), "only ctrl_c should move, into the free slot right after it");
        assert!(pc.insert.is_none(), "moving to a higher address is never an Insert");

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: applying a Shift-resolved `UpdateRegDefWithMoves` moves the mover and cascades
    /// exactly the companion computed above, leaving the untouched register alone and the mover's
    /// own vacated slot empty (a hole — nothing back-fills it).
    #[test]
    fn apply_update_regdef_with_moves_shift_leaves_hole_at_old_address() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_shift_apply.rif");
        write_reorder_gap_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let d = def_regdef_in(&v, &rif_type, "ctrl");

        v.pending = Some(EditAction::UpdateRegDefWithMoves {
            rif_type: rif_type.clone(),
            orig_name: "ctrl".to_owned(),
            inst_name: "ctrl_b".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "ctrl".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: Some(0x8), intr: None, array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
            companions: vec![("ctrl_c".to_owned(), 0xc)],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let page = compiled_page(&v, "Main");
        assert_eq!(page.regs.iter().find(|r| r.reg_name == "ctrl_a").unwrap().addr, 0x0);
        assert_eq!(page.regs.iter().find(|r| r.reg_name == "ctrl_b").unwrap().addr, 0x8);
        assert_eq!(page.regs.iter().find(|r| r.reg_name == "ctrl_c").unwrap().addr, 0xc);
        assert_eq!(page.regs.iter().find(|r| r.reg_name == "status").unwrap().addr, 0x28);
        assert!(page.regs.iter().all(|r| r.addr != 0x4), "ctrl_b's old address must be left empty");

        fs::remove_file(&tmp).ok();
    }

    /// The Insert rotation moves every register strictly between the target and the mover's old
    /// address forward by one slot each, landing the last one exactly on the vacated old address
    /// — no hole, unlike Shift. Mirrors the confirmed 10/20/30/40 worked example.
    #[test]
    fn resolve_reg_addr_commit_insert_rotates_between_target_and_old() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_insert.rif");
        write_reorder_gap_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "status");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef(
            "status".to_owned(), "status".to_owned(), &def, 0x28, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x4".to_owned(); // ctrl_b's current address
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none(), "a collision must pop the modal, not apply directly");
        let pc = conflict.expect("collision expected");
        assert_eq!(pc.insert, Some(vec![("ctrl_b".to_owned(), 0x8), ("ctrl_c".to_owned(), 0x28)]));
        assert!(pc.shift.is_none(), "moving to a lower address is never a Shift");

        fs::remove_file(&tmp).ok();
    }

    /// When the colliding register can't be safely moved — here `ctrl_b` uses `Relative`
    /// addressing, excluded for the same reason the existing address-swap arrows exclude it — no
    /// modal is offered at all; the commit is rejected with a plain validation error, same as
    /// today's collision-via-recompile path.
    #[test]
    fn resolve_reg_addr_commit_falls_back_to_error_when_target_ineligible() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_addr_ineligible.rif");
        write_relative_addr_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "status");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef(
            "status".to_owned(), "status".to_owned(), &def, 0x8, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x4".to_owned(); // ctrl_b's resolved address, but ctrl_b is Relative
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none());
        assert!(conflict.is_none(), "nothing can be moved automatically, so no modal is shown");
        assert!(ed.parse_err.as_deref().is_some_and(|e| e.contains("ctrl_b")), "got: {:?}", ed.parse_err);

        fs::remove_file(&tmp).ok();
    }

    /// The core regression: a not-yet-blurred address edit sitting in `reg_editor`'s live buffer
    /// must be applied before `save_file` proceeds — reproducing the reported bug where clicking
    /// Save immediately after typing (without tabbing away first) silently discarded the edit,
    /// because `save_file` used to clear every editor unconditionally before anything downstream
    /// had a chance to flush it. Unlike every other test, `apply_pending` is deliberately never
    /// called here — this simulates the edit still being live in the UI at the moment Save fires.
    #[test]
    fn save_file_flushes_unblurred_reg_editor_edit_before_saving() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_save_flush_addr.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "ctrl");

        let mut ed = RegEditor::from_regdef(
            "ctrl_a".to_owned(), "ctrl".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x20".to_owned();
        v.reg_editor = Some(ed);

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- ctrl_a = ctrl @ 0x20"), "unblurred edit must have been flushed before saving:\n{after}");

        fs::remove_file(&tmp).ok();
    }

    /// Companion case: if the unflushed edit collides, `save_file` must not proceed at all — the
    /// user has to resolve the Swap/Shift/Insert modal first, rather than the save either
    /// discarding the edit or writing something inconsistent underneath it.
    #[test]
    fn save_file_blocks_when_unblurred_reg_editor_edit_collides() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_save_flush_addr_conflict.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "ctrl");

        let mut ed = RegEditor::from_regdef(
            "ctrl_a".to_owned(), "ctrl".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true,
        );
        ed.addr = "0x4".to_owned(); // collides with ctrl_b
        v.reg_editor = Some(ed);

        let before = fs::read_to_string(&tmp).unwrap();
        v.save_file();

        assert!(v.confirm_reg_addr_conflict.is_some(), "the collision must pop the resolution modal");
        assert!(v.reg_editor.is_some(), "the edit must not have been silently discarded");
        let after = fs::read_to_string(&tmp).unwrap();
        assert_eq!(before, after, "save must not have written anything while the conflict is unresolved");

        fs::remove_file(&tmp).ok();
    }

    // ---- Add / delete registers: EditAction::AddRegister / EditAction::DeleteRegister ----

    /// A brand-new register on an automatic page needs no instance at all — the definition
    /// auto-instantiates on its own — and starts with one starter field (mirroring `AddField`'s
    /// default) rather than an untested zero-field register.
    #[test]
    fn apply_add_register_new_on_automatic_page() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        assert!(v.rif_comp.is_some(), "test.rif should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::AddRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            kind: AddRegKind::New,
            name: "brand_new".to_owned(),
            addr: 0, // ignored: the page stays automatic, so no instance is created
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d = def_regdef(&v, "brand_new");
        assert_eq!(d.fields.len(), 1, "starter field");
        {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            assert!(rif.pages[0].instances.iter().all(|i| i.inst_name != "brand_new"),
                "automatic page needs no instance for a new definition");
        }
        let page = compiled_page(&v, "Main");
        assert!(page.regs.iter().any(|r| r.reg_name == "brand_new"), "auto-instantiated");
        assert!(v.has_unsaved());
    }

    /// A brand-new register on an *already-manual* page needs both a definition and its own
    /// matching instance (manual mode only instantiates from `instances:`), at the computed
    /// default address.
    #[test]
    fn apply_add_register_new_on_manual_page() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_add_reg_manual.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::AddRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            kind: AddRegKind::New,
            name: "extra".to_owned(),
            addr: 0x100,
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        assert!(find_regdef(rif, "extra").is_some());
        let inst = rif.pages[0].instances.iter().find(|i| i.inst_name == "extra").expect("instance created too");
        assert_eq!(inst.type_name, "extra");
        assert_eq!(inst.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0x100)));
        assert!(inst.src.decl_line.is_none(), "not saved yet");

        fs::remove_file(&tmp).ok();
    }

    /// A new instance of an existing type adds only a `RegInst` — the definition (and its
    /// existing instances) are untouched.
    #[test]
    fn apply_add_register_instance_of_existing_type() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_add_reg_instance.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::AddRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            kind: AddRegKind::Instance { type_name: "ctrl".to_owned() },
            name: "ctrl_c".to_owned(),
            addr: 0xC,
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        assert_eq!(page.instances.iter().filter(|i| i.type_name == "ctrl").count(), 3, "ctrl_a, ctrl_b, and the new one");
        let ctrl_c = page.instances.iter().find(|i| i.inst_name == "ctrl_c").expect("new instance");
        assert_eq!(ctrl_c.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(0xC)));
        // Exactly one "ctrl" definition still — this action never touches `page.registers`
        assert_eq!(page.registers.iter().filter(|r| r.get_regdef().is_some_and(|d| d.name == "ctrl")).count(), 1);

        fs::remove_file(&tmp).ok();
    }

    /// Deleting a register on an automatic page always removes the definition — there is no
    /// separate instance to speak of there.
    #[test]
    fn apply_delete_register_removes_definition_on_automatic_page() {
        let mut v = RifViewer::default();
        v.file_path = "../rifgen/test/test.rif".into();
        v.open_file();
        assert!(v.rif_comp.is_some(), "test.rif should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let decl_line = def_regdef(&v, "basic_rw").src.decl_line.unwrap();

        v.pending = Some(EditAction::DeleteRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            reg_type: "basic_rw".to_owned(),
            inst_name: None,
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        assert!(find_regdef(rif, "basic_rw").is_none());
        assert!(v.reg_deleted.contains(&decl_line));
        assert!(v.has_unsaved());
    }

    /// Deleting one of several instances removes only that instance; the definition (still
    /// referenced by the sibling) stays.
    #[test]
    fn apply_delete_register_removes_only_instance_when_others_remain() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_delete_reg_instance.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let inst_line = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap().src.decl_line.unwrap()
        };

        v.pending = Some(EditAction::DeleteRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            reg_type: "ctrl".to_owned(),
            inst_name: Some("ctrl_a".to_owned()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        assert!(page.instances.iter().all(|i| i.inst_name != "ctrl_a"));
        assert!(page.instances.iter().any(|i| i.inst_name == "ctrl_b"), "sibling untouched");
        assert!(find_regdef(rif, "ctrl").is_some(), "definition stays: ctrl_b still references it");
        assert!(v.reg_inst_deleted.contains(&inst_line));
        assert!(v.reg_deleted.is_empty(), "no definition was removed");

        fs::remove_file(&tmp).ok();
    }

    /// Deleting the last (or only) instance of a type removes the instance *and* the definition.
    #[test]
    fn apply_delete_register_removes_definition_when_last_instance() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_delete_reg_last.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::DeleteRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            reg_type: "status".to_owned(),
            inst_name: Some("status".to_owned()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        assert!(page.instances.iter().all(|i| i.inst_name != "status"));
        assert!(find_regdef(rif, "status").is_none(), "last instance gone: definition removed too");
        assert!(!v.reg_deleted.is_empty());
        assert!(!v.reg_inst_deleted.is_empty());

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: adding a register to an automatic page and saving writes its declaration and
    /// starter field after the page's last existing register, leaves everything else untouched,
    /// and re-parses with the field intact.
    #[test]
    fn save_file_persists_added_register_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_save_add_reg.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::AddRegister {
            rif_type,
            page_name: "Main".to_owned(),
            kind: AddRegKind::New,
            name: "brand_new".to_owned(),
            addr: 0,
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        assert!(!v.has_unsaved());

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- brand_new :"), "got:\n{after}");
        assert!(after.contains("- field0 = 0"), "starter field, got:\n{after}");
        // Sibling registers/fields and the page's trailing `instances: auto` are untouched
        assert!(after.contains("- reg_fields_pos :"));
        assert!(after.contains("instances: auto"));

        let rif2 = get_rif(&v.rif_src.as_ref().unwrap().rifs, "test_rif").unwrap();
        assert!(find_regdef(rif2, "brand_new").is_some(), "re-parses");

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: deleting one instance (sibling remains) and the last instance of another type
    /// (removes its definition too) in the same save, leaving the untouched register intact.
    #[test]
    fn save_file_persists_deleted_registers_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_save_delete_reg.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::DeleteRegister {
            rif_type: rif_type.clone(),
            page_name: "Main".to_owned(),
            reg_type: "ctrl".to_owned(),
            inst_name: Some("ctrl_a".to_owned()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        v.pending = Some(EditAction::DeleteRegister {
            rif_type,
            page_name: "Main".to_owned(),
            reg_type: "status".to_owned(),
            inst_name: Some("status".to_owned()),
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        assert!(!v.has_unsaved());

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(!after.contains("ctrl_a"), "got:\n{after}");
        assert!(!after.contains("status"), "both the instance and the definition, got:\n{after}");
        // The untouched sibling instance and its definition survive
        assert!(after.contains("- ctrl_b = ctrl @ 0x4"), "got:\n{after}");
        assert!(after.contains("- ctrl: \"Control register\""));

        let rif2 = get_rif(&v.rif_src.as_ref().unwrap().rifs, "mini_rif").unwrap();
        assert!(find_regdef(rif2, "status").is_none(), "re-parses without it");
        assert_eq!(rif2.pages[0].instances.len(), 1, "only ctrl_b remains");

        fs::remove_file(&tmp).ok();
    }

    /// Regression for a real bug: a register with no *inline* description (`has_inline_desc:
    /// false` — either genuinely no description, like `reg_fields_pos`, or a block-only one)
    /// never marked itself dirty when the user typed a new one, because `RegDef::fmt_decl` only
    /// ever emits the inline slot when that flag is set — `set_short` alone updates the
    /// `Description`'s content but never flips the flag, so `decl_changed` compared two
    /// declaration lines that were identical (both missing the description) no matter what was
    /// typed. Fixed by flipping `has_inline_desc` to `true` whenever the description buffer
    /// actually changed from what was loaded — mirroring how a brand-new field/register already
    /// sets it on creation.
    #[test]
    fn apply_update_regdef_description_without_inline_desc_gets_tracked() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        let d = def_regdef(&v, "reg_fields_pos");
        assert!(!d.src.has_inline_desc, "reg_fields_pos starts with no inline description");
        let decl_line = d.src.decl_line.unwrap();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "reg_fields_pos".to_owned(),
            inst_name: "reg_fields_pos".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "reg_fields_pos".to_owned(),
                group: None,
                desc: "brand new description".to_owned(),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "reg_fields_pos");
        assert!(d2.src.has_inline_desc, "typing a description promotes it to inline");
        assert_eq!(d2.description.get_short(false), "brand new description");
        assert!(v.reg_dirty.contains(&decl_line),
            "the declaration line must be marked dirty so Save actually writes the new description");
        assert!(v.has_unsaved());
    }

    /// Editing an already-existing inline description (the common case, `basic_rw`) still marks
    /// the line dirty — the fix above must not regress the already-working path.
    #[test]
    fn apply_update_regdef_description_change_on_existing_inline_desc_still_tracked() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        let d = def_regdef(&v, "basic_rw");
        assert!(d.src.has_inline_desc);
        let decl_line = d.src.decl_line.unwrap();

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: "a different description".to_owned(),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.reg_dirty.contains(&decl_line));
    }

    /// Leaving the description buffer untouched (only the pulse changed, say) must not
    /// spuriously flip `has_inline_desc` on a register that never had one — that would promote
    /// a block-only or absent description to a stray empty inline slot the user never asked for.
    #[test]
    fn apply_update_regdef_untouched_description_does_not_gain_inline_flag() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        let d = def_regdef(&v, "reg_fields_pos");
        assert!(!d.src.has_inline_desc);

        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "reg_fields_pos".to_owned(),
            inst_name: "reg_fields_pos".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "reg_fields_pos".to_owned(),
                group: None,
                desc: d.description.get(true), // unchanged
                pulse: vec![RegPulseKind::Write("clk_rif".to_owned())], // something else changes
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "reg_fields_pos");
        assert!(!d2.src.has_inline_desc, "untouched description must not gain an inline flag");
    }

    /// Clearing a register's existing `description:` block down to just the short line records
    /// a deletion (`None`) against its declaration line.
    #[test]
    fn apply_update_regdef_description_block_clear_is_tracked() {
        let mut v = load_with_basic_rw_selected();
        let d = def_regdef(&v, "basic_rw");
        assert_eq!(d.description.get(true), "Simple register with r/w fields\nMore detailled information on current register");
        let decl_line = d.src.decl_line.unwrap();
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRegDef {
            rif_type: rif_type.clone(),
            orig_name: "basic_rw".to_owned(),
            inst_name: "basic_rw".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "basic_rw".to_owned(),
                group: None,
                desc: "Simple register with r/w fields".to_owned(),
                pulse: d.pulse.clone(),
                addr: None,
                intr: None,
                array: None,
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef(&v, "basic_rw");
        assert_eq!(d2.description.get(true), "Simple register with r/w fields");
        assert_eq!(v.reg_desc_block_edits.get(&decl_line), Some(&None));
        assert!(v.has_unsaved());
    }

    // ---- Instance overrides (`EditTarget::Instance`) ----

    /// A plain register's instance override is editable regardless of `incl` (there is none
    /// here, but the predicate is deliberately independent of it — see its doc comment); a
    /// derived (enable/mask/pending) interrupt register has no override of its own to speak of.
    #[test]
    fn is_reg_override_editable_excludes_intr_derived() {
        let v = load_with_basic_rw_selected();
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let basic_rw = rif.pages.iter().flat_map(|p| p.regs.iter()).find(|r| r.reg_name == "basic_rw")
            .unwrap_or_else(|| panic!("basic_rw exists"));
        assert!(RifViewer::is_reg_override_editable(basic_rw), "a plain register's instance override is editable");
        let interrupt = rif.pages.iter().flat_map(|p| p.regs.iter()).find(|r| r.reg_name == "interrupt")
            .unwrap_or_else(|| panic!("interrupt exists"));
        assert!(RifViewer::is_reg_override_editable(interrupt), "the base interrupt register is still override-editable");
        let derived = rif.pages.iter().flat_map(|p| p.regs.iter()).find(|r| r.is_intr_derived())
            .expect("a derived interrupt row exists");
        assert!(!RifViewer::is_reg_override_editable(derived), "a derived interrupt register has no override of its own");
    }

    /// An array field element shares one definition-level `Field` with no independent slot to
    /// override (same reasoning `is_field_reorderable` already uses); a plain field is fine.
    #[test]
    fn is_field_override_editable_excludes_array_elements() {
        let v = load_with_basic_rw_selected();
        let Some(Comp::Rif(rif)) = &v.rif_comp else { panic!("expected a Rif comp") };
        let reg_fields_a1 = rif.pages.iter().flat_map(|p| p.regs.iter()).find(|r| r.reg_name == "reg_fields_a1")
            .unwrap_or_else(|| panic!("reg_fields_a1 exists"));
        let coeffs = reg_fields_a1.fields.iter().find(|f| f.name == "coeffs").unwrap_or_else(|| panic!("coeffs exists"));
        assert!(!RifViewer::is_field_override_editable(coeffs), "an array field element has no independent override slot");
        let basic_rw = rif.pages.iter().flat_map(|p| p.regs.iter()).find(|r| r.reg_name == "basic_rw")
            .unwrap_or_else(|| panic!("basic_rw exists"));
        let field1 = basic_rw.fields.iter().find(|f| f.name == "field1").unwrap_or_else(|| panic!("field1 exists"));
        assert!(RifViewer::is_field_override_editable(field1), "a plain field is override-editable");
    }

    /// Setting every whole-register override property records the minimal per-line edit for
    /// each (via the generic `RegOverrideProp::ALL` diff, mirroring `UpdateRegDef`), leaves the
    /// sibling instance (`ctrl_b`, same type) completely untouched, and clearing every property
    /// back out drops the now-fully-default override entry entirely.
    #[test]
    fn apply_update_reg_override_sets_and_clears_properties() {
        let tmp = std::env::temp_dir().join("yargui_reg_override_apply.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let ctrl_a_line = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap().src.decl_line.unwrap()
        };

        v.pending = Some(EditAction::UpdateRegOverride {
            rif_type: rif_type.clone(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegOverrideVals {
                desc: "Override desc".to_owned(),
                hw: Some(Access::RW),
                optional: Some(yarig::parser::parser_expr::parse_expr("1").unwrap()),
                optional_acc: Some(Access::RO),
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        {
            let rif = get_rif(&v.rif_src.as_ref().unwrap().rifs, &rif_type).unwrap();
            let ctrl_a = rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
            let ovr = ctrl_a.reg_override.get(&None).expect("override was created");
            assert_eq!(ovr.description.as_ref().map(|d| d.get_short(true)), Some("Override desc".to_owned()));
            assert_eq!(ovr.hw_acc, Some(Access::RW));
            assert_eq!(ovr.optional_acc, Some(Access::RO));
            assert!(!ovr.optional.is_empty());
            let ctrl_b = rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_b").unwrap();
            assert!(ctrl_b.reg_override.get(&None).is_none(), "sibling instance must be untouched");
        }
        let edits = v.reg_override_edits.get(&ctrl_a_line).expect("prop edits recorded");
        assert_eq!(edits.get(&RegOverrideProp::Description), Some(&Some("description : Override desc".to_owned())));
        assert_eq!(edits.get(&RegOverrideProp::Hw), Some(&Some("hw rw".to_owned())));
        assert_eq!(edits.get(&RegOverrideProp::Optional), Some(&Some("optional : 1".to_owned())));
        assert_eq!(edits.get(&RegOverrideProp::OptionalAcc), Some(&Some("optional_acc: ro".to_owned())));
        assert!(v.has_unsaved());

        v.pending = Some(EditAction::UpdateRegOverride {
            rif_type: rif_type.clone(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegOverrideVals { desc: String::new(), hw: None, optional: None, optional_acc: None },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let rif = get_rif(&v.rif_src.as_ref().unwrap().rifs, &rif_type).unwrap();
        let ctrl_a = rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
        assert!(ctrl_a.reg_override.get(&None).is_none(), "fully-default override should be dropped");

        std::fs::remove_file(&tmp).ok();
    }

    /// Setting a field override's description + reset records both sub-property lines; switching
    /// to "disable" with a value equal to the field's own default reset (0) round-trips as a bare
    /// `.disable`, per `FieldOverride::fmt_prop`'s omission rule.
    #[test]
    fn apply_update_field_override_reset_and_disable() {
        let tmp = std::env::temp_dir().join("yargui_field_override_apply.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let ctrl_a_line = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap().src.decl_line.unwrap()
        };

        v.pending = Some(EditAction::UpdateFieldOverride {
            rif_type: rif_type.clone(),
            inst_name: "ctrl_a".to_owned(),
            field_name: "en".to_owned(),
            vals: FieldOverrideVals {
                desc: "Enable override".to_owned(),
                disabled: false,
                reset: Some(ResetValP::Unsigned(1)),
                field_reset: ResetValP::Unsigned(0),
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        {
            let rif = get_rif(&v.rif_src.as_ref().unwrap().rifs, &rif_type).unwrap();
            let ctrl_a = rif.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
            let field_ovr = ctrl_a.reg_override.get(&None).and_then(|o| o.fields.get("en")).expect("field override created");
            assert_eq!(field_ovr.reset, ResetValOverride::Reset(ResetValP::Unsigned(1)));
            assert_eq!(field_ovr.visibility, None);
        }
        let edits = v.field_override_edits.get(&ctrl_a_line).expect("field edits recorded");
        assert_eq!(edits.get(&("en".to_owned(), FieldOverrideProp::Description)), Some(&Some("en.description Enable override".to_owned())));
        assert_eq!(edits.get(&("en".to_owned(), FieldOverrideProp::Reset)), Some(&Some("en.reset = 1".to_owned())));

        v.pending = Some(EditAction::UpdateFieldOverride {
            rif_type: rif_type.clone(),
            inst_name: "ctrl_a".to_owned(),
            field_name: "en".to_owned(),
            vals: FieldOverrideVals {
                desc: "Enable override".to_owned(),
                disabled: true,
                reset: Some(ResetValP::Unsigned(0)),
                field_reset: ResetValP::Unsigned(0),
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let edits = v.field_override_edits.get(&ctrl_a_line).expect("field edits recorded");
        assert_eq!(edits.get(&("en".to_owned(), FieldOverrideProp::Reset)), Some(&Some("en.disable".to_owned())));

        std::fs::remove_file(&tmp).ok();
    }

    /// End-to-end: a register-level override (description + hw) and a field-level override
    /// (disable) on `ctrl_a`, saved together, land as the expected lines, leave the sibling
    /// instance and both register definitions byte-for-byte untouched, and survive a re-parse.
    #[test]
    fn save_file_persists_reg_and_field_override_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_save_override_edit.rif");
        write_mini_manual_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegOverride {
            rif_type: rif_type.clone(),
            inst_name: "ctrl_a".to_owned(),
            vals: RegOverrideVals { desc: "Override desc".to_owned(), hw: Some(Access::RW), optional: None, optional_acc: None },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        v.pending = Some(EditAction::UpdateFieldOverride {
            rif_type: rif_type.clone(),
            inst_name: "ctrl_a".to_owned(),
            field_name: "en".to_owned(),
            vals: FieldOverrideVals { desc: String::new(), disabled: true, reset: None, field_reset: ResetValP::Unsigned(0) },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        assert!(!v.has_unsaved());

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("description : Override desc"), "got:\n{after}");
        assert!(after.contains("hw rw"), "got:\n{after}");
        assert!(after.contains("en.disable"), "got:\n{after}");
        // Sibling instance and both register definitions are untouched
        assert!(after.contains("- ctrl_b = ctrl @ 0x4"));
        assert!(after.contains("- status @ 0x8"));
        assert!(after.contains("- ctrl: \"Control register\""));
        assert!(after.contains("- en = 0 0:0 \"Enable\""));

        let rif2 = get_rif(&v.rif_src.as_ref().unwrap().rifs, "mini_rif").unwrap();
        let ctrl_a2 = rif2.pages[0].instances.iter().find(|i| i.inst_name == "ctrl_a").unwrap();
        let ovr2 = ctrl_a2.reg_override.get(&None).expect("override survives reparse");
        assert_eq!(ovr2.description.as_ref().map(|d| d.get_short(true)), Some("Override desc".to_owned()));
        assert_eq!(ovr2.hw_acc, Some(Access::RW));
        let en_ovr2 = ovr2.fields.get("en").expect("field override survives reparse");
        assert_eq!(en_ovr2.visibility, Some(Visibility::Disabled));

        fs::remove_file(&tmp).ok();
    }

    // ---- Register arrays: EditAction::UpdateRegDef{,WithMoves}.vals.array / UpdateRegInstArray ----

    /// Four single-field, single-manual-instance registers in a row (`a`,`b`,`c`,`d`, plain
    /// non-array fields) plus a fifth (`e`, already an array field) with no follower — the exact
    /// worked example from the design notes (32-bit data width, 4-byte step): `a` growing from a
    /// plain register to a dim-3 array must shift `b`,`c`,`d` forward by 2 slots (8 bytes) each.
    fn write_array_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: arr_rif\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - a: \"Register A\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "      - b: \"Register B\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "      - c: \"Register C\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "      - d: \"Register D\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "      - e: \"Register E\"\n",
            "        - val[2] = 0 7:0 \"Value\"\n",
            "    instances:\n",
            "      - a @ 0x0\n",
            "      - b @ 0x4\n",
            "      - c @ 0x8\n",
            "      - d @ 0xc\n",
            "      - e @ 0x10\n",
        )).unwrap();
    }

    /// Same shape as `write_array_fixture`'s `a`/`b`, but `b` is `Relative` (`@+`) — not
    /// `is_reg_reorderable`, so growing `a`'s footprint into `b`'s address can't cascade
    /// automatically.
    fn write_array_relative_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: arr_rif\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - a: \"Register A\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "      - b: \"Register B\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "    instances:\n",
            "      - a @ 0x0\n",
            "      - b @+0x4\n",
        )).unwrap();
    }

    /// `a` already carries its own instance-level array (`a[3]`) while `RegDef.array` stays 0 —
    /// used to test the mutual-exclusion gate and `RegEditor::from_regdef` loading an existing
    /// instance-level array back into `inst_array`.
    fn write_inst_array_fixture(path: &std::path::Path) {
        std::fs::write(path, concat!(
            "rif: arr_rif2\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - a: \"Register A\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "      - b: \"Register B\"\n",
            "        - val = 0 7:0 \"Value\"\n",
            "    instances:\n",
            "      - a[3] @ 0x0\n",
            "      - b @ 0xc\n",
        )).unwrap();
    }

    /// Growing a plain register into an array pops the confirmation modal when its (only) field
    /// isn't an array yet AND the footprint grows into following registers — the exact worked
    /// example from the design notes: `a` (dim 1 -> 3, +2 slots = +8 bytes) shifts `b`,`c`,`d`.
    #[test]
    fn resolve_def_array_commit_needs_confirmation_for_field_conversion_and_shift() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_confirm.rif");
        write_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "a");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef("a".to_owned(), "a".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true);
        assert!(ed.array_editable, "single-instance, non-param type should allow the Array control");
        ed.array = "3".to_owned();
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none(), "field conversion + shift must pop the modal, not apply directly");
        assert!(conflict.is_none());
        let pc = array_change.expect("array change expected");
        assert_eq!(pc.fields_to_convert, vec!["val".to_owned()]);
        assert_eq!(pc.companions, Some(vec![("b".to_owned(), 0xc), ("c".to_owned(), 0x10), ("d".to_owned(), 0x14), ("e".to_owned(), 0x18)]));
        assert!(matches!(pc.kind, RegArrayChangeKind::Definition(ref vals) if vals.array == Some(3)));

        fs::remove_file(&tmp).ok();
    }

    /// When the array's only field is already an array and there's no follower to shift, the
    /// change applies directly — no modal, mirroring the address editor's "unambiguous" fast path.
    #[test]
    fn resolve_def_array_commit_applies_directly_when_nothing_to_confirm() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_direct.rif");
        write_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "e");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef("e".to_owned(), "e".to_owned(), &def, 0x10, true, &rif_type, "clk", DataWidth::W32(32), None, true);
        ed.array = "2".to_owned();
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(array_change.is_none(), "nothing to confirm: field is already an array and nothing follows");
        assert!(matches!(&action, Some(EditAction::UpdateRegDef { vals, .. }) if vals.array == Some(2)));

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: applying the confirmed `UpdateRegDefWithMoves` converts the field to `[1]`,
    /// sets the register's own array dimension, and shifts every companion atomically — and all
    /// of it is tracked dirty for save.
    #[test]
    fn apply_update_regdef_with_moves_array_converts_fields_and_shifts_companions() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_apply.rif");
        write_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let d = def_regdef_in(&v, &rif_type, "a");

        v.pending = Some(EditAction::UpdateRegDefWithMoves {
            rif_type: rif_type.clone(),
            orig_name: "a".to_owned(),
            inst_name: "a".to_owned(),
            vals: RegDefVals {
                alt: None,
                name: "a".to_owned(), group: None, desc: d.description.get(true), pulse: d.pulse.clone(),
                addr: None, intr: None, array: Some(3),
                visibility: Visibility::Full,
                clk: None,
                rst: None,
                external: ExternalKind::None,
            },
            companions: vec![("b".to_owned(), 0xc), ("c".to_owned(), 0x10), ("d".to_owned(), 0x14), ("e".to_owned(), 0x18)],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef_in(&v, &rif_type, "a");
        assert_eq!(d2.array, Width::Value(3));
        let val2 = d2.fields.iter().find(|f| f.name == "val").expect("field exists");
        assert_eq!(val2.array, Width::Value(1), "the plain field must be converted to a dim-1 array");

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        for (name, addr) in [("b", 0xcu64), ("c", 0x10), ("d", 0x14), ("e", 0x18)] {
            let inst = page.instances.iter().find(|i| i.inst_name == name).unwrap();
            assert_eq!(inst.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(addr)), "{name} should have shifted");
        }

        // Recompile reproduces exactly 3 rows for `a`, at the array's new addresses.
        let Some(Comp::Rif(rif_inst)) = &v.rif_comp else { panic!("expected a plain Rif") };
        let a_rows: Vec<&RifRegInst> = rif_inst.pages[0].regs.iter().filter(|r| r.reg_name == "a").collect();
        assert_eq!(a_rows.len(), 3);
        assert_eq!(a_rows.iter().map(|r| r.addr).collect::<Vec<_>>(), vec![0x0, 0x4, 0x8]);

        assert!(v.reg_dirty.contains(&d.src.decl_line.unwrap()), "register's own decl line must be dirtied");
        let val_line = d.fields.iter().find(|f| f.name == "val").unwrap().src.decl_line.unwrap();
        assert!(v.dirty.contains(&val_line), "the converted field's decl line must be dirtied");
        assert!(v.reg_inst_dirty.len() == 4, "all four shifted companions must be dirtied");

        fs::remove_file(&tmp).ok();
    }

    /// Setting an instance-level array on a single-instance manual register pops the same
    /// shift-cascade confirmation as the definition-level case, but never touches any field.
    #[test]
    fn resolve_inst_array_commit_needs_confirmation_for_shift() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_inst_array_confirm.rif");
        write_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "a");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef("a".to_owned(), "a".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true);
        assert!(ed.inst_array_editable, "manual, non-array, single-instance type should allow the Instance array control");
        ed.inst_array = "3".to_owned();
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none());
        let pc = array_change.expect("array change expected");
        assert!(pc.fields_to_convert.is_empty(), "instance-level arrays never touch fields");
        assert_eq!(pc.companions, Some(vec![("b".to_owned(), 0xc), ("c".to_owned(), 0x10), ("d".to_owned(), 0x14), ("e".to_owned(), 0x18)]));
        assert!(matches!(&pc.kind, RegArrayChangeKind::Instance(Some(expr)) if expr.to_rif() == "3"));

        fs::remove_file(&tmp).ok();
    }

    /// End-to-end: applying the confirmed `UpdateRegInstArray` sets only the instance's own array
    /// (via the library fix wiring `ExprTokens::to_rif()` into `RegInst::fmt_decl`), shifts the
    /// companions, and leaves `RegDef.array`/every field completely untouched.
    #[test]
    fn apply_update_reg_inst_array_sets_array_and_shifts_companions() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_inst_array_apply.rif");
        write_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();

        v.pending = Some(EditAction::UpdateRegInstArray {
            rif_type: rif_type.clone(),
            inst_name: "a".to_owned(),
            array: Some(parse_expr("3").expect("parse 3 cannot fail")),
            companions: vec![("b".to_owned(), 0xc), ("c".to_owned(), 0x10), ("d".to_owned(), 0x14), ("e".to_owned(), 0x18)],
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let d2 = def_regdef_in(&v, &rif_type, "a");
        assert_eq!(d2.array, Width::Value(0), "RegDef.array must stay untouched for an instance-level array");
        let val2 = d2.fields.iter().find(|f| f.name == "val").unwrap();
        assert_eq!(val2.array, Width::Value(0), "fields must stay untouched for an instance-level array");

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        let page = &rif.pages[0];
        let a_inst = page.instances.iter().find(|i| i.inst_name == "a").unwrap();
        assert_eq!(a_inst.array.to_rif(), "3");
        for (name, addr) in [("b", 0xcu64), ("c", 0x10), ("d", 0x14), ("e", 0x18)] {
            let inst = page.instances.iter().find(|i| i.inst_name == name).unwrap();
            assert_eq!(inst.addr, Address::new(AddressKind::Absolute, AddressOffset::Value(addr)), "{name} should have shifted");
        }

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);
        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("- a[3] @ 0x0"), "got:\n{after}");

        fs::remove_file(&tmp).ok();
    }

    /// A collision that can't be resolved automatically (here `b` is `Relative`) refuses with a
    /// plain validation error instead of popping an unusable modal — mirrors
    /// `resolve_reg_addr_commit_falls_back_to_error_when_target_ineligible`.
    #[test]
    fn resolve_def_array_commit_blocked_by_non_reorderable_follower() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_blocked.rif");
        write_array_relative_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "a");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef("a".to_owned(), "a".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true);
        ed.array = "2".to_owned();
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none());
        assert!(array_change.is_none(), "nothing can be moved automatically, so no modal is shown");
        assert!(ed.parse_err.as_deref().is_some_and(|e| e.contains("collides")), "got: {:?}", ed.parse_err);

        fs::remove_file(&tmp).ok();
    }

    /// Mutual exclusion: once an instance already carries its own instance-level array, the
    /// definition-level "Array" control is disabled — but the instance-level control itself stays
    /// editable (to change or clear it), and `inst_array` loads the existing size back correctly.
    #[test]
    fn regeditor_from_regdef_disables_def_array_when_instance_already_has_inst_array() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_mutex.rif");
        write_inst_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "a");
        let src_inst = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            rif.pages[0].instances.iter().find(|i| i.inst_name == "a").unwrap().clone()
        };

        let ed = RegEditor::from_regdef("a".to_owned(), "a".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), Some(&src_inst), true);
        assert!(!ed.array_editable, "definition-level array must be disabled while the instance already has its own");
        assert_eq!(ed.inst_array, "3");
        assert!(ed.inst_array_editable, "the instance's own array control must stay editable");

        fs::remove_file(&tmp).ok();
    }

    /// The scope bound for multi-instance types (`array_scope_ok = false`, as computed by the
    /// call site for a type with more than one manual instance on the page) disables both array
    /// controls outright, regardless of the definition's own array state.
    #[test]
    fn regeditor_from_regdef_disables_both_array_controls_outside_scope() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_scope_bound.rif");
        write_mini_manual_fixture(&tmp); // `ctrl` has two manual instances: ctrl_a, ctrl_b
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "ctrl");

        let ed = RegEditor::from_regdef("ctrl_a".to_owned(), "ctrl".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, false);
        assert!(!ed.array_editable);
        assert!(!ed.inst_array_editable);

        fs::remove_file(&tmp).ok();
    }

    /// Address and array-size changes can't be resolved in the same commit — refused with a clear
    /// message rather than silently applying one and dropping the other.
    #[test]
    fn resolve_reg_commit_rejects_simultaneous_address_and_array_edit() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_array_and_addr.rif");
        write_array_fixture(&tmp);
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "fixture should compile: {:?}", v.last_err);
        let rif_type = v.selected_rif_type();
        let def = def_regdef_in(&v, &rif_type, "a");
        let src_page = {
            let src = v.rif_src.as_ref().unwrap();
            get_rif(&src.rifs, &rif_type).unwrap().pages[0].clone()
        };
        let page = compiled_page(&v, "Main");

        let mut ed = RegEditor::from_regdef("a".to_owned(), "a".to_owned(), &def, 0x0, true, &rif_type, "clk", DataWidth::W32(32), None, true);
        ed.addr = "0x20".to_owned();
        ed.array = "2".to_owned();
        let mut conflict = None;
        let mut array_change = None;
        let action = RifViewer::resolve_reg_commit(&mut ed, &def, &page.regs, Some(&src_page), &mut conflict, &mut array_change);
        assert!(action.is_none());
        assert!(conflict.is_none());
        assert!(array_change.is_none());
        assert!(ed.parse_err.as_deref().is_some_and(|e| e.contains("separately")), "got: {:?}", ed.parse_err);

        fs::remove_file(&tmp).ok();
    }

    /// Editing name/address width/data width/description recompiles and records the minimal
    /// dirty state — no rename here, so `rifs` keeps its existing key.
    #[test]
    fn apply_update_rif_def_edits_widths_and_desc_without_renaming() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        v.pending = Some(EditAction::UpdateRifDef {
            rif_type: rif_type.clone(),
            vals: RifDefVals {
                name: rif_type.clone(),
                addr_width: 12,
                data_width: DataWidth::W16(16),
                interface: Interface::Apb,
                desc: "Edited top description".to_owned(),
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).expect("rif still there under the same name");
        assert_eq!(rif.addr_width, 12);
        assert_eq!(rif.data_width, DataWidth::W16(16));
        assert_eq!(rif.description.get(true), "Edited top description");
        assert!(v.rif_dirty.is_empty(), "decl line unchanged: name wasn't touched");
        let prop_edits = &v.rif_prop_edits;
        assert!(prop_edits.contains_key(&RifProp::AddrWidth));
        assert!(prop_edits.contains_key(&RifProp::DataWidth));
        assert!(v.rif_desc_block_edits.is_some(), "description block change recorded");
    }

    /// Renaming the top-level (non-Rifmux) Rif moves the `rifs` map entry: the old name no
    /// longer resolves, the new one does, and the decl line is marked dirty.
    #[test]
    fn apply_update_rif_def_renames_top_level_rif() {
        let mut v = load_with_basic_rw_selected();
        let orig_type = v.selected_rif_type();
        let (addr_width, data_width, interface, desc) = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &orig_type).unwrap();
            (rif.addr_width, rif.data_width, rif.interface.clone(), rif.description.get(true))
        };
        v.pending = Some(EditAction::UpdateRifDef {
            rif_type: orig_type.clone(),
            vals: RifDefVals { name: "test_rif_renamed".to_owned(), addr_width, data_width, interface, desc },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        let src = v.rif_src.as_ref().unwrap();
        assert!(get_rif(&src.rifs, &orig_type).is_none(), "old name should no longer resolve");
        let rif = get_rif(&src.rifs, "test_rif_renamed").expect("renamed rif resolves under its new name");
        assert_eq!(rif.name, "test_rif_renamed");
        assert!(!v.rif_dirty.is_empty(), "decl line marked dirty");
        assert_eq!(v.editing_rif.as_deref(), Some("test_rif_renamed"), "dirty state attributed to the RIF's new name");
    }

    /// Renaming a RIF onto the name of another RIF already declared in the same file tree is
    /// rejected outright (surfaced via `edit_err`) rather than silently clobbering the other
    /// RIF's definition via the `rifs` map's plain `insert` — leaves both definitions, `top`, and
    /// every Rifmux item completely untouched.
    #[test]
    fn apply_update_rif_def_rename_onto_existing_name_is_rejected() {
        use std::fs;
        let dir = std::env::temp_dir().join("yargui_rif_rename_collision");
        fs::create_dir_all(&dir).unwrap();
        let mux_path = dir.join("top_mux.rif");
        let leaf_a_path = dir.join("leaf_a.rif");
        let leaf_b_path = dir.join("leaf_b.rif");
        fs::write(&mux_path, concat!(
            "rifmux: top_mux\n",
            "  addrWidth: 16\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  map:\n",
            "    - inst_a = leaf_a @ 0x0000\n",
            "    - inst_b = leaf_b @ 0x1000\n",
        )).unwrap();
        fs::write(&leaf_a_path, concat!(
            "rif: leaf_a\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  - Main : \"Main Page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - basic_rw: \"Simple register\"\n",
            "        - field0 = 0  7:0  \"Field A\"\n",
        )).unwrap();
        fs::write(&leaf_b_path, concat!(
            "rif: leaf_b\n",
            "  addrWidth: 8\n",
            "  dataWidth: 32\n",
            "  interface: apb\n",
            "  description:\n",
            "    Leaf B's own description\n",
            "  - Main : \"Main Page\"\n",
            "    baseAddress: 0x0\n",
            "    registers:\n",
            "      - basic_rw: \"Simple register\"\n",
            "        - field0 = 0  7:0  \"Field B\"\n",
        )).unwrap();

        let mut v = RifViewer::default();
        v.file_path = mux_path.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "rifmux should compile: {:?}", v.last_err);
        let (addr_width, data_width, interface, desc) = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, "leaf_a").unwrap();
            (rif.addr_width, rif.data_width, rif.interface.clone(), rif.description.get(true))
        };
        v.pending = Some(EditAction::UpdateRifDef {
            rif_type: "leaf_a".to_owned(),
            vals: RifDefVals { name: "leaf_b".to_owned(), addr_width, data_width, interface, desc },
        });
        v.apply_pending();

        assert!(v.edit_err.is_some(), "renaming onto an existing RIF name must be rejected");
        let src = v.rif_src.as_ref().unwrap();
        assert!(get_rif(&src.rifs, "leaf_a").is_some(), "leaf_a must still exist under its own name");
        let leaf_b = get_rif(&src.rifs, "leaf_b").expect("leaf_b must still exist, untouched");
        assert_eq!(leaf_b.description.get(true), "Leaf B's own description", "leaf_b's own content must be unclobbered");
        let mux = src.rifmux.get("top_mux").expect("rifmux still there");
        assert!(mux.items.iter().any(|i| i.rif_type == RifType::Rif("leaf_a".to_owned())), "leaf_a's map entry untouched");
        assert!(mux.items.iter().any(|i| i.rif_type == RifType::Rif("leaf_b".to_owned())), "leaf_b's map entry untouched");

        fs::remove_file(&mux_path).ok();
        fs::remove_file(&leaf_a_path).ok();
        fs::remove_file(&leaf_b_path).ok();
    }

    /// End-to-end: renaming the top-level RIF and editing its address/data width and description,
    /// then `save_file`, rewrites the `rif:`/`addrWidth:`/`dataWidth:` lines and the description
    /// block in place, keeps sibling content (comments, `swClock:`, `parameters:`, pages,
    /// registers) byte-identical, and the file re-parses under its new name.
    #[test]
    fn save_file_persists_rif_def_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_rif_def_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let rif_type = v.selected_rif_type();
        assert_eq!(rif_type, "test_rif");
        v.pending = Some(EditAction::UpdateRifDef {
            rif_type: rif_type.clone(),
            vals: RifDefVals {
                name: "test_rif_renamed".to_owned(),
                addr_width: 10,
                data_width: DataWidth::W16(16),
                interface: Interface::Apb,
                desc: "New top description".to_owned(),
            },
        });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("rif: test_rif_renamed"), "got:\n{after}");
        assert!(after.contains("addrWidth: 10"), "got:\n{after}");
        assert!(after.contains("dataWidth: 16"), "got:\n{after}");
        assert!(after.contains("  description:"), "got:\n{after}");
        assert!(after.contains("    New top description"), "got:\n{after}");
        // Sibling content untouched
        assert!(after.contains("// Main parameters"));
        assert!(after.contains("swClock    : clk_rif"));
        assert!(after.contains("- OPTION_A = True"));
        assert!(after.contains("- basic_rw: \"Simple register with r/w fields\""));
        assert!(!v.has_unsaved(), "rif_dirty/rif_prop_edits/rif_desc_block_edits must be cleared after a successful save");

        let src = v.rif_src.as_ref().unwrap();
        assert!(get_rif(&src.rifs, "test_rif_renamed").is_some(), "re-parsed file resolves under the new name");

        fs::remove_file(&tmp).ok();
    }

    /// Editing an existing clock's reset (polarity) recompiles and records the change against
    /// its own already-tracked `swReset:` line for deletion, and queues the whole (one-entry)
    /// group as a fresh rewrite — a lone in-place edit isn't safe here since resets are matched
    /// to clocks positionally; the shared `swClock:`/`swClkEn:`/`swClear:` lines are untouched
    /// since only the reset's content changed.
    #[test]
    fn apply_update_rif_clocking_edits_existing_reset_and_marks_line_dirty() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        let (sw, hw) = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            (rif.sw_clocking.clone(), rif.hw_clocking.clone())
        };
        assert_eq!(sw.len(), 1, "test_rif declares exactly one software clock");
        let orig_reset_line = sw[0].rst.src.decl_line.expect("swReset has a tracked line in test.rif");
        let mut new_sw = sw.clone();
        new_sw[0].rst.active_high = true;
        v.pending = Some(EditAction::UpdateRifClocking { rif_type: rif_type.clone(), sw: new_sw, hw: hw.clone() });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        assert!(rif.sw_clocking[0].rst.active_high);
        let deleted = &v.rif_reset_deleted;
        assert!(deleted.contains(&orig_reset_line));
        let new_lines = v.rif_reset_new_lines.get(&false).expect("sw group");
        assert_eq!(new_lines.len(), 1);
        assert!(new_lines[0].contains("activeHigh"), "got: {new_lines:?}");
        assert!(v.rif_clock_line_edits.is_empty(), "clock/en/clear lines are unrelated to this edit");
    }

    /// Adding a second software clock records the shared `swClock:` line as changed (now listing
    /// both clocks) and queues the whole sw group's reset block for a fresh rewrite (both the
    /// unchanged original clock's reset and the new clock's) — never a lone inserted line, since
    /// that would corrupt the parser's positional clock↔reset matching for whichever clock comes
    /// after it.
    #[test]
    fn apply_update_rif_clocking_adds_new_clock_records_line_and_insert() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        let (mut sw, hw) = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            (rif.sw_clocking.clone(), rif.hw_clocking.clone())
        };
        let orig_reset_line = sw[0].rst.src.decl_line.expect("swReset has a tracked line in test.rif");
        sw.push(ClockingInfo { clk: "clk2".to_owned(), rst: ResetDef::new("rst2_n".to_owned()), en: String::new(), clear: String::new() });
        v.pending = Some(EditAction::UpdateRifClocking { rif_type: rif_type.clone(), sw, hw });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let line_edits = &v.rif_clock_line_edits;
        let body = line_edits.get(&RifClockLineKind::SwClock).expect("SwClock entry present")
            .as_ref().expect("a replace, not a deletion");
        assert!(body.contains("clk_rif") && body.contains("clk2"), "got: {body}");
        assert!(v.rif_reset_deleted.contains(&orig_reset_line), "original reset line queued for deletion too");
        let new_lines = v.rif_reset_new_lines.get(&false).expect("sw group");
        assert_eq!(new_lines.len(), 2, "whole group rewritten, not just the new clock");
        assert!(new_lines.iter().any(|l| l.contains("rst_rif_n")));
        assert!(new_lines.iter().any(|l| l.contains("rst2_n")));
    }

    /// End-to-end: adding a second software clock through `save_file` regenerates the shared
    /// `swClock:` line in place and rewrites the whole reset block (both the original clock's
    /// reset and the new one) as fresh, canonical lines right after it — the original clock's
    /// reset line is *not* preserved verbatim, since leaving it in place while inserting only the
    /// new clock's reset would corrupt the parser's positional clock↔reset matching on re-parse.
    #[test]
    fn save_file_persists_rif_clocking_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_rif_clocking_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let rif_type = v.selected_rif_type();
        let (mut sw, hw) = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            (rif.sw_clocking.clone(), rif.hw_clocking.clone())
        };
        sw.push(ClockingInfo { clk: "clk2".to_owned(), rst: ResetDef::new("rst2_n".to_owned()), en: String::new(), clear: String::new() });
        v.pending = Some(EditAction::UpdateRifClocking { rif_type: rif_type.clone(), sw, hw });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(after.contains("swClock: clk_rif clk2"), "got:\n{after}");
        assert!(!after.contains("swReset    : rst_rif_n activeLow async"), "original terse-formatted line must be gone, got:\n{after}");
        assert!(after.contains("swReset: rst_rif_n activeLow async"), "original clock's reset rewritten in canonical form, got:\n{after}");
        assert!(after.contains("swReset: rst2_n activeLow async"), "new clock's reset inserted, got:\n{after}");
        // Reset lines land right after the (regenerated) swClock line, not scattered elsewhere.
        let clock_pos = after.find("swClock: clk_rif clk2").unwrap();
        let rst1_pos = after.find("swReset: rst_rif_n").unwrap();
        let rst2_pos = after.find("swReset: rst2_n").unwrap();
        assert!(clock_pos < rst1_pos && rst1_pos < rst2_pos, "got:\n{after}");
        assert!(!v.has_unsaved(), "clocking edits must be cleared after a successful save");

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        assert_eq!(rif.sw_clocking.len(), 2);
        assert!(rif.sw_clocking[1].rst.src.decl_line.is_some(), "new reset line got a real line number after re-parse");

        fs::remove_file(&tmp).ok();
    }

    /// Renaming a parameter reuses its existing line (recorded as a replace, not a delete-and-
    /// insert), while an untouched sibling parameter's line is left alone entirely — parameters
    /// are matched by name, so unlike clocking there's no shared-line/positional concern.
    #[test]
    fn apply_update_rif_params_renames_param_and_reuses_its_line() {
        let mut v = load_with_basic_rw_selected();
        let rif_type = v.selected_rif_type();
        let (option_a_line, nb_reg_line, params, generics) = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            assert_eq!(rif.parameters.len(), 2);
            let params: Vec<ParamEntry> = rif.parameters.items()
                .map(|(k, v)| ParamEntry { orig_name: Some(k.clone()), name: k.clone(), value: v.clone() }).collect();
            let generics: Vec<GenericEntry> = rif.generics.items()
                .map(|(k, v)| GenericEntry { orig_name: Some(k.clone()), name: k.clone(), range: v.clone() }).collect();
            (*rif.src.param_lines.get("OPTION_A").unwrap(), *rif.src.param_lines.get("NB_REG").unwrap(), params, generics)
        };
        let mut renamed_params = params;
        renamed_params[0].name = "OPTION_A_RENAMED".to_owned();
        v.pending = Some(EditAction::UpdateRifParamsAndGenerics { rif_type: rif_type.clone(), params: renamed_params, generics });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        assert!(rif.parameters.get("OPTION_A").is_none());
        assert!(rif.parameters.get("OPTION_A_RENAMED").is_some());
        let edits = &v.rif_param_edits;
        assert!(edits.get(&option_a_line).is_some_and(|b| b.as_deref().is_some_and(|t| t.contains("OPTION_A_RENAMED"))), "got: {edits:?}");
        assert!(!edits.contains_key(&nb_reg_line), "untouched sibling parameter's line must be left alone");
    }

    /// End-to-end: renaming a parameter, adding a brand-new one, and adding a brand-new generic
    /// (test.rif declares no `generics:` section at all) — `save_file` rewrites the renamed
    /// parameter's line in place, appends the new parameter after the last existing one, and
    /// emits a fresh `generics :` section header alongside the new generic entry.
    #[test]
    fn save_file_persists_rif_params_and_generics_edits_to_disk() {
        use std::fs;
        let tmp = std::env::temp_dir().join("yargui_rif_params_edit.rif");
        fs::copy("../rifgen/test/test.rif", &tmp).unwrap();
        let mut v = RifViewer::default();
        v.file_path = tmp.clone();
        v.open_file();
        assert!(v.rif_comp.is_some(), "temp test.rif should compile: {:?}", v.last_err);

        let rif_type = v.selected_rif_type();
        let mut params: Vec<ParamEntry> = {
            let src = v.rif_src.as_ref().unwrap();
            let rif = get_rif(&src.rifs, &rif_type).unwrap();
            assert!(rif.generics.is_empty(), "test.rif declares no generics: section");
            rif.parameters.items().map(|(k, v)| ParamEntry { orig_name: Some(k.clone()), name: k.clone(), value: v.clone() }).collect()
        };
        params[0].name = "OPTION_A_RENAMED".to_owned();
        params.push(ParamEntry { orig_name: None, name: "NEW_PARAM".to_owned(), value: parse_expr("7").unwrap() });
        let generics = vec![GenericEntry { orig_name: None, name: "WIDTH".to_owned(), range: GenericRange { min: 1, default: 4, max: 8, desc: Some("Bit width".to_owned()) } }];
        v.pending = Some(EditAction::UpdateRifParamsAndGenerics { rif_type: rif_type.clone(), params, generics });
        v.apply_pending();
        assert!(v.edit_err.is_none(), "recompile should succeed: {:?}", v.edit_err);
        assert!(v.has_unsaved());

        v.save_file();
        assert!(v.edit_err.is_none(), "save should succeed: {:?}", v.edit_err);

        let after = fs::read_to_string(&tmp).unwrap();
        // `ExprTokens::to_rif()` normalizes a boolean literal to its numeric form (a pre-existing
        // library behavior, not introduced here) — round-tripping any parameter through the
        // table, even unedited, loses the original `True`/`False` spelling.
        assert!(after.contains("- OPTION_A_RENAMED = 1"), "got:\n{after}");
        assert!(after.contains("- NB_REG = 4"), "sibling parameter untouched, got:\n{after}");
        assert!(after.contains("- NEW_PARAM = 7"), "new parameter appended, got:\n{after}");
        assert!(after.contains("generics :"), "new generics: section header emitted, got:\n{after}");
        assert!(after.contains("- WIDTH : 1:4:8 \"Bit width\""), "got:\n{after}");
        assert!(!v.has_unsaved(), "param/generic edits must be cleared after a successful save");

        let src = v.rif_src.as_ref().unwrap();
        let rif = get_rif(&src.rifs, &rif_type).unwrap();
        assert_eq!(rif.parameters.len(), 3);
        assert_eq!(rif.generics.len(), 1);
        assert!(rif.src.param_lines.contains_key("NEW_PARAM"), "new param got a real line number after re-parse");
        assert!(rif.src.generic_lines.contains_key("WIDTH"), "new generic got a real line number after re-parse");

        fs::remove_file(&tmp).ok();
    }
}
