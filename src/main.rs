mod search;
mod widget;
mod select;
mod field_editor;
mod enum_editor;
mod reg_editor;
mod rif_editor;
mod intr_desc_editor;
mod file_io;
mod tree_view;
mod reg_view;
mod field_view;
mod apply_pending;

const VERSION: &str = env!("CARGO_PKG_VERSION");

use std::{collections::{HashMap, HashSet}, path::{Path, PathBuf}};

use apply_pending::{EditAction, RifClockLineKind};
use eframe::egui;
use egui_file_dialog::{FileDialog, Filter};
use egui::{RichText, ViewportBuilder};
use enum_editor::EnumEditor;
use field_editor::{FieldEditor, FieldOverrideEditor};
use intr_desc_editor::{FieldIntrDescEditor, RegIntrDescEditor};
use reg_editor::{
    PendingDeleteReg, PendingRegAddrConflict, PendingRegArrayChange, RegAddEditor, RegAddrResolution,
    RegArrayChangeKind, RegEditor, RegOverrideEditor,
};
use reg_view::{PageClick, RegClick};
use rif_editor::{RifClockingEditor, RifEditor, RifParamsEditor};
use search::SearchMatches;
use select::{PendingRifSwitch, SelectedItem, Selection};
use tree_view::CompRef;
use yarig::{
    comp::comp_inst::{Comp, FieldWidth, RifFieldInst, RifRegInst},
    parser::{RifGenSrc, get_rif, parser_expr::ParamValues, remove_rif},
    rifgen::{
        EnumDef, Field, FieldOverrideProp, FieldPos, FieldProp, FieldSwKind, InterruptClr,
        InterruptInfoField, InterruptRegKind, InterruptTrigger, RegDef, RegOverrideProp, RegProp,
        Rif, RifPage, RifProp, SuffixInfo, Width,
    },
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
        // let enum_defs: &[EnumDef] =
        //     rif_viewer.rif_src.as_ref().and_then(|src| get_rif(&src.rifs, "test_rif"))
        //     .map(|rif| rif.enum_defs.as_slice())
        //     .unwrap_or(&[]);
        // println!("Enum defs = {:?}", enum_defs);
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
    /// File picker for the Rif-level "Custom" interface's `.sv` implementation
    rif_intf_file_dialog: FileDialog,
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
    // Compile settings retained so edits can recompile identically to the initial load
    params: ParamValues,
    suffixes: HashMap<String, SuffixInfo>,
    // Edition feature
    edit_mode: bool,
    edit_target: EditTarget,
    field_editor: Option<FieldEditor>,
    pending: Option<EditAction>,
    /// Error raised by recompilation after an edit
    edit_err: Option<String>,
    /// The RIF type these pending-edit fields (below) currently belong to — at most one RIF type
    /// can have edits pending at a time; see the "switch RIF" guard in `ui()`. `None` when nothing
    /// is pending. Maintained by `apply_pending` (set/cleared based on `has_unsaved`'s before/
    /// after) and by `clear_pending_edits`.
    editing_rif: Option<String>,
    /// Source line numbers of field declarations edited since the last save
    dirty: HashSet<usize>,
    /// Source line numbers of field declarations deleted since the last save
    deleted: HashSet<usize>,
    /// Per-field sub-property line edits pending save, keyed by field's declaration source line, then by property
    prop_edits: HashMap<usize, HashMap<FieldProp, Option<String>>>,
    /// Pending edits to a field's `description:` block, keyed by field's declaration source line.
    desc_block_edits: HashMap<usize, Option<Vec<String>>>,
    /// True while the "discard unsaved changes?" reload confirmation is shown
    confirm_reload: bool,
    /// Last selection confirmed safe to display — i.e. not mid-navigation away from a RIF type
    /// that still has edits pending. Restored onto `selected` when a navigation gets blocked; see
    /// the "switch RIF" guard in `ui()` and `PendingRifSwitch`.
    validated_selection: Selection,
    /// Set while the "save or discard pending edits before switching RIF?" confirmation is shown.
    confirm_switch_rif: Option<PendingRifSwitch>,
    /// Source line numbers of enum entry declarations edited since the last save
    enum_dirty: HashSet<usize>,
    /// Source line numbers of enum entry declarations deleted since the last save
    enum_deleted: HashSet<usize>,
    /// Live buffers for the enum entry-table modal, when open
    enum_editor: Option<EnumEditor>,
    /// Live buffers for the register (definition) editor, when a register is selected in edit mode
    reg_editor: Option<RegEditor>,
    /// Source line numbers of register declarations edited since the last save
    reg_dirty: HashSet<usize>,
    /// Per-register sub-property (pulse) line edits pending save, keyed by register's declaration source line, then by property
    reg_prop_edits: HashMap<usize, HashMap<RegProp, Option<String>>>,
    /// Pending edits to a register's own `description, keyed by the `RegDef`'s declaration source line
    reg_desc_block_edits: HashMap<usize, Option<Vec<String>>>,
    /// Source line numbers of *already on-disk* `RegInst` declarations (address edits) edited since the last save
    reg_inst_dirty: HashSet<usize>,
    /// Source line numbers of deleted register declarations
    reg_deleted: HashSet<usize>,
    /// Source line numbers of deleted `RegInst` declarations pending save
    reg_inst_deleted: HashSet<usize>,
    /// Live buffers for the "Add register" modal, when open.
    reg_add_editor: Option<RegAddEditor>,
    /// Set while the "delete this register?" confirmation is shown
    confirm_delete_reg: Option<PendingDeleteReg>,
    /// Set while the address-conflict resolution modal (Swap/Shift/Insert/Cancel) is shown
    confirm_reg_addr_conflict: Option<PendingRegAddrConflict>,
    /// Set while the register-array confirmation modal.
    confirm_reg_array_change: Option<PendingRegArrayChange>,
    /// Live buffer for a derived (enable/mask/pending) interrupt register's own description
    reg_intr_desc_editor: Option<RegIntrDescEditor>,
    /// Same as `reg_intr_desc_editor`, one level down: a field's own description override.
    field_intr_desc_editor: Option<FieldIntrDescEditor>,
    /// Pending edits to a register's primary interrupt's description, keyed by the *base*
    /// register's declaration source line, then by which derived kind — mirrors
    /// `reg_desc_block_edits`, one map per kind instead of one map total (there are three
    /// independent block ranges to track per register).
    reg_intr_desc_block_edits: HashMap<usize, HashMap<InterruptRegKind, Option<Vec<String>>>>,
    /// Same as `reg_intr_desc_block_edits`, one level down: keyed by the field's own declaration
    /// source line instead of the register's.
    field_intr_desc_block_edits: HashMap<usize, HashMap<InterruptRegKind, Option<Vec<String>>>>,
    /// Live buffer for a register instance's whole-register override editor (`EditTarget::
    /// Instance`), when a register is selected in edit mode — see `show_reg_override_editor`.
    reg_override_editor: Option<RegOverrideEditor>,
    /// Per-instance-override sub-property line edits pending save, keyed by the owning
    /// `RegInst`'s declaration source line, then by property — mirrors `reg_prop_edits`, one layer
    /// further down (instance override instead of the shared type).
    reg_override_edits: HashMap<usize, HashMap<RegOverrideProp, Option<String>>>,
    /// Pending edits to a register-instance override's own `description:` block — mirrors
    /// `reg_desc_block_edits`, for the override instead of the shared type.
    reg_override_desc_block_edits: HashMap<usize, Option<Vec<String>>>,
    /// Live buffer for a field's whole-field override editor (`EditTarget::Instance`), when a
    /// field is selected in edit mode — see `show_field_override_editor`.
    field_override_editor: Option<FieldOverrideEditor>,
    /// Per-field-override line edits pending save, keyed by the owning `RegInst`'s declaration
    /// source line, then by (field name, property) — every field override under one instance
    /// attaches to that same instance line, mirroring how multiple `FieldProp`s all key off one
    /// `Field`'s own declaration line.
    field_override_edits: HashMap<usize, HashMap<(String, FieldOverrideProp), Option<String>>>,
    /// Live buffers for the Rif-level property editor (name/address width/data width/
    /// description), when a Rif node is selected in edit mode — see `show_rif_editor`.
    rif_editor: Option<RifEditor>,
    /// Source line numbers of `rif: <name>` declarations edited since the last save (a rename) —
    /// mirrors `reg_dirty`, but there is exactly one `Rif` per type, so the set holds at most one
    /// line.
    rif_dirty: HashSet<usize>,
    /// Rif-level scalar sub-property edits pending save, keyed by property. Simpler than
    /// `reg_prop_edits` (no declaration-line layer): unlike registers, there is only ever one
    /// `Rif` per type, so the property key alone disambiguates.
    rif_prop_edits: HashMap<RifProp, Option<String>>,
    /// Pending edit to the Rif's own `description:` block — mirrors `reg_desc_block_edits` minus
    /// the declaration-line layer, for the same reason as `rif_prop_edits`.
    rif_desc_block_edits: Option<Vec<String>>,
    /// Live buffers for the clocking-table modal, when open — see `show_rif_clocking_editor`.
    rif_clocking_editor: Option<RifClockingEditor>,
    /// Edits to one of the six shared, one-line-per-hw/sw-group clocking properties
    /// (`swClock:`/`hwClock:`/`swClkEn:`/`hwClkEn:`/`swClear:`/`hwClear:`), keyed by which one.
    /// `Some(body)` replaces/inserts the line, `None` deletes an existing one — mirrors
    /// `rif_prop_edits`, one map per line-sharing group instead of per scalar prop.
    rif_clock_line_edits: HashMap<RifClockLineKind, Option<String>>,
    /// Existing `swReset:`/`hwReset:` lines belonging to a hw/sw group whose resets changed at
    /// all (any clock added, removed, or whose own reset content changed) — every one of them
    /// gets deleted and the whole group rewritten fresh via `rif_reset_new_lines`, never patched
    /// individually. Resets are matched to clocks *positionally* by the parser (`rst_idx` in
    /// `parser_file.rs`), so replacing just the one changed line (or inserting a lone new one)
    /// among untouched siblings would silently shift that mapping for every later clock in the
    /// group.
    rif_reset_deleted: HashSet<usize>,
    /// The full, freshly-rendered `swReset:`/`hwReset:` block for a hw/sw group whose resets
    /// changed, keyed by `is_hw` — already rendered (not deferred to save time) since a
    /// whole-collection clocking replace discards each rebuilt `ResetDef`'s source line, so only
    /// the pre-edit backup (available while applying `UpdateRifClocking`, gone by the time
    /// `save_rif_file` runs) can still tell which lines the group used to occupy. Inserted at save
    /// time right after that group's own `swClock:`/`hwClock:` line (falling back to the Rif's own
    /// declaration line if that doesn't exist either) — anchoring here specifically, not at the
    /// decl line generally, is required: a reset line parsed *before* its group's clock list is
    /// established gets matched to the wrong (or a phantom) clock.
    rif_reset_new_lines: HashMap<bool, Vec<String>>,
    /// Live buffers for the parameters/generics modal
    rif_params_editor: Option<RifParamsEditor>,
    /// Per-parameter line edits pending save, keyed by the existing declaration line. `Some(body)`
    /// replaces (a content change or a rename, reusing the old entry's line) — `None` deletes it.
    /// Populated only for rows that already had a name at load time (`ParamRow::orig_name: Some`);
    /// a genuinely new row is *not* inferred from "which names now have no `param_lines` entry"
    /// (a rename also produces such a name — the *old* one — which would otherwise be
    /// double-counted as a second, spurious insert) but instead recorded explicitly in
    /// `rif_param_inserts`.
    rif_param_edits: HashMap<usize, Option<String>>,
    /// Same as `rif_param_edits`, one property over, for the generics table.
    rif_generic_edits: HashMap<usize, Option<String>>,
    /// Brand-new parameter lines.
    rif_param_inserts: Vec<String>,
    /// Same as `rif_param_inserts`, for the generics table.
    rif_generic_inserts: Vec<String>,
}

impl Default for RifViewer {
    fn default() -> Self {
        let file_dialog = FileDialog::new()
            .add_file_filter("RIFs", Filter::new(|p: &Path| p.extension().unwrap_or_default() == "rif"))
            .default_file_filter("RIFs")
            .show_new_folder_button(false);
        let rif_intf_file_dialog = FileDialog::new()
            .add_file_filter("SystemVerilog", Filter::new(|p: &Path| p.extension().unwrap_or_default() == "sv"))
            .default_file_filter("SystemVerilog")
            .show_new_folder_button(false);
        Self {
            file_dialog,
            file_path : PathBuf::from("."),
            rif_intf_file_dialog,
            rif_src: None,
            rif_comp: None,
            last_err: None,
            selected: Selection::default(),
            search_query: String::new(),
            search_results: SearchMatches::new(),
            current_result: 0,
            search_by_name: true,
            search_by_desc: false,
            params: ParamValues::new(),
            suffixes: HashMap::new(),
            edit_mode: false,
            edit_target: EditTarget::default(),
            field_editor: None,
            pending: None,
            edit_err: None,
            editing_rif: None,
            dirty: HashSet::new(),
            deleted: HashSet::new(),
            prop_edits: HashMap::new(),
            desc_block_edits: HashMap::new(),
            confirm_reload: false,
            validated_selection: Selection::default(),
            confirm_switch_rif: None,
            enum_dirty: HashSet::new(),
            enum_deleted: HashSet::new(),
            enum_editor: None,
            reg_editor: None,
            reg_dirty: HashSet::new(),
            reg_prop_edits: HashMap::new(),
            reg_desc_block_edits: HashMap::new(),
            reg_inst_dirty: HashSet::new(),
            reg_deleted: HashSet::new(),
            reg_inst_deleted: HashSet::new(),
            reg_add_editor: None,
            confirm_delete_reg: None,
            confirm_reg_addr_conflict: None,
            confirm_reg_array_change: None,
            reg_intr_desc_editor: None,
            field_intr_desc_editor: None,
            reg_intr_desc_block_edits: HashMap::new(),
            field_intr_desc_block_edits: HashMap::new(),
            reg_override_editor: None,
            reg_override_edits: HashMap::new(),
            reg_override_desc_block_edits: HashMap::new(),
            field_override_editor: None,
            field_override_edits: HashMap::new(),
            rif_editor: None,
            rif_dirty: HashSet::new(),
            rif_prop_edits: HashMap::new(),
            rif_desc_block_edits: None,
            rif_clocking_editor: None,
            rif_clock_line_edits: HashMap::new(),
            rif_reset_deleted: HashSet::new(),
            rif_reset_new_lines: HashMap::new(),
            rif_params_editor: None,
            rif_param_edits: HashMap::new(),
            rif_generic_edits: HashMap::new(),
            rif_param_inserts: Vec::new(),
            rif_generic_inserts: Vec::new(),
        }
    }
}


/// Which layer an edit targets: the shared register/field type, or one instance's own override.
/// The toggle that picks between them (`show_edit_target_header`) is only shown when it's
/// actually meaningful — more than one instance of the type, or the type comes from another RIF
/// via `include` (in which case `Definition` itself is disabled, since editing another file's
/// definition is out of scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum EditTarget {
    #[default]
    Definition,
    Instance,
}



/// Software access kinds offered in the edit combo: simple, payload-free variants, plus the two
/// inline-expressible pulse kinds (`W1Pulse`'s write-only flag isn't picked here — see
/// `sw_kind_label`/`is_field_editable`'s doc comment — so an existing write-only pulse field keeps
/// that flag unless the user actively reselects one of these two entries).
fn sw_kind_choices() -> [(FieldSwKind, &'static str); 10] {
    [
        (FieldSwKind::ReadWrite, "RW"),
        (FieldSwKind::ReadOnly, "RO"),
        (FieldSwKind::WriteOnly, "WO"),
        (FieldSwKind::ReadClr, "RCLR"),
        (FieldSwKind::W1Clr, "W1CLR"),
        (FieldSwKind::W0Clr, "W0CLR"),
        (FieldSwKind::W1Set, "W1SET"),
        (FieldSwKind::W1Tgl, "W1TGL"),
        (FieldSwKind::W1Pulse(false, false), "PULSE"),
        (FieldSwKind::W1Pulse(true, false), "PULSEREG"),
    ]
}

/// Label for the SW-access combo's collapsed button. `FieldSwKind::access_str` collapses both
/// pulse variants to "Pulse", which would hide whether a loaded field is comb or reg pulse;
/// this mirrors the distinct "PULSE"/"PULSEREG" labels used in `sw_kind_choices`.
fn sw_kind_label(kind: &FieldSwKind) -> &str {
    match kind {
        FieldSwKind::W1Pulse(true, _) => "PULSEREG",
        FieldSwKind::W1Pulse(false, _) => "PULSE",
        k => k.access_str(),
    }
}

/// Label for the interrupt trigger combo.
fn intr_trigger_label(t: InterruptTrigger) -> &'static str {
    match t {
        InterruptTrigger::High => "High",
        InterruptTrigger::Low => "Low",
        InterruptTrigger::Rising => "Rising",
        InterruptTrigger::Falling => "Falling",
        InterruptTrigger::Edge => "Edge",
    }
}

/// Label for the interrupt clear-method combo.
fn intr_clear_label(c: InterruptClr) -> &'static str {
    match c {
        InterruptClr::Read => "Read (rclr)",
        InterruptClr::Write0 => "Write 0 (w0clr)",
        InterruptClr::Write1 => "Write 1 (w1clr)",
        InterruptClr::Hw => "Hardware (hwclr)",
    }
}

/// Label for a derived interrupt kind (enable/mask/pending), e.g. in the derived-register and
/// per-field description editors.
fn intr_kind_label(kind: InterruptRegKind) -> &'static str {
    match kind {
        InterruptRegKind::Enable => "enable",
        InterruptRegKind::Mask => "mask",
        InterruptRegKind::Pending => "pending",
        _ => "",
    }
}

/// Read-only lookup of a register definition by type name (used to load the register editor's
/// buffers, mirroring `find_field`'s role for fields).
fn find_regdef<'a>(rif: &'a Rif, reg_type: &str) -> Option<&'a RegDef> {
    rif.pages.iter()
        .flat_map(|p| p.registers.iter())
        .filter_map(|r| r.get_regdef())
        .find(|d| d.name == reg_type)
}

/// Find the page that declares a register type's definition. Only meaningful for a register
/// passing `is_reg_editable` (not reached through an include, so its `instances:` — if any —
/// live on this same page): used to decide whether address editing is available (the page must
/// already be manual) and, if so, which page's `instances` to look the specific instance up in.
fn find_reg_page<'a>(rif: &'a Rif, reg_type: &str) -> Option<&'a RifPage> {
    rif.pages.iter().find(|p| p.registers.iter().any(|r| r.get_regdef().is_some_and(|d| d.name == reg_type)))
}


/// Read-only lookup of a field definition (used to load advanced-property buffers, which the
/// compiled instance does not all carry — e.g. `lock`).
fn find_field<'a>(rif: &'a Rif, reg_type: &str, field: &str) -> Option<&'a Field> {
    rif.pages.iter()
        .flat_map(|p| p.registers.iter())
        .filter_map(|r| r.get_regdef())
        .find(|d| d.name == reg_type)?
        .fields.iter()
        .find(|f| f.name == field)
}

/// True when `field_name` in `group_type` is split across sibling registers via `arrayPartial`.
/// Checked by name across the whole `(group)`, not just the currently-selected register's own
/// field, because `arrayPartial` is only stamped on the *continuation* half(s) (`partial.1 > 0`)
/// — the base half's own `Field` carries no marker of its own, even though it needs the same
/// editing lock (see `field_view::show_field_editor`'s `array_partial_locked` parameter).
fn field_has_array_partial(rif: &Rif, group_type: &str, field_name: &str) -> bool {
    rif.pages.iter()
        .flat_map(|p| p.registers.iter())
        .filter_map(|r| r.get_regdef())
        .filter(|d| d.get_group_name() == group_type)
        .flat_map(|d| d.fields.iter())
        .any(|f| f.name == field_name && f.partial.1 > 0)
}

/// Render an advanced-property row label, dimmed when the property is currently unused —
/// makes it obvious at a glance which of lock/limit/counter/password/enum are actually set.
fn adv_label(ui: &mut egui::Ui, text: &str, active: bool) {
    ui.label(if active { RichText::new(text) } else { RichText::new(text).weak() });
}


/// Build the new position for a field after a reorder swap. A concrete-width field gets a plain
/// `msb:lsb`; a generic-width field (its real width is a hardware generic, statically allocated
/// at the generic's max per the RTL — see `is_field_reorderable`) keeps its `Width::Param`
/// reference via `lsb+:$name` instead of collapsing to a hardcoded literal msb, which would
/// silently strip the parameterization on save.
fn reordered_pos(width: &FieldWidth, new_lsb: u16) -> FieldPos {
    match width {
        FieldWidth::Value(w) => FieldPos::MsbLsb((Width::Value((new_lsb + w - 1) as u8), Width::Value(new_lsb as u8))),
        FieldWidth::Generic((name, _)) => FieldPos::LsbSize((Width::Value(new_lsb as u8), Width::Param(name.clone()))),
    }
}

/// Compute the two new positions when swapping adjacent fields `lower` (smaller lsb) and
/// `higher`. The higher field takes the bottom slot; the lower one moves just above it, so the
/// occupied region and any gap between them are preserved and only these two fields move.
/// Returns `(new position for lower, new position for higher)`.
fn swap_positions(lower: &RifFieldInst, higher: &RifFieldInst) -> (FieldPos, FieldPos) {
    let la = lower.lsb as u16;
    let wa = lower.width();
    let wb = higher.width();
    let gap = (higher.lsb as u16).saturating_sub(la + wa);
    let higher_lsb = la;
    let lower_lsb = la + wb + gap;
    (reordered_pos(&lower.width, lower_lsb), reordered_pos(&higher.width, higher_lsb))
}


impl eframe::App for RifViewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Re-open last file When hitting F5 (guard against discarding unsaved edits)
        if ui.input(|i|  i.key_pressed(egui::Key::F5)) {
            if self.has_unsaved() {
                self.confirm_reload = true;
            } else {
                self.open_file();
            }
        }
        // Confirmation dialog before discarding unsaved edits on reload
        if self.confirm_reload {
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("You have unsaved edits. Reload from disk and discard them?");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Discard & Reload").clicked() {
                            self.confirm_reload = false;
                            self.open_file();
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_reload = false;
                        }
                    });
                });
        }
        // "Save or discard pending edits before switching RIF?" confirmation, when the "switch
        // RIF" guard above just blocked a navigation.
        if let Some(pending_switch) = self.confirm_switch_rif.clone() {
            let mut save = false;
            let mut discard = false;
            let mut cancel = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "Changes on '{}' are pending: save or discard them before switching RIF?",
                        pending_switch.from_rif
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() { save = true; }
                        if ui.button("Discard").clicked() { discard = true; }
                        if ui.button("Cancel").clicked() { cancel = true; }
                    });
                });
            if save {
                self.save_file();
                if !self.has_unsaved() {
                    self.selected = pending_switch.target.clone();
                    self.validated_selection = pending_switch.target;
                    self.confirm_switch_rif = None;
                }
            } else if discard {
                self.clear_pending_edits();
                self.selected = pending_switch.target.clone();
                self.validated_selection = pending_switch.target;
                self.confirm_switch_rif = None;
            } else if cancel {
                self.confirm_switch_rif = None;
            }
        }
        // Enum entry-table modal, when open
        let enum_rif_type = self.enum_editor.as_ref().map(|ed| ed.rif_type.clone());
        let enum_defs: &[EnumDef] = enum_rif_type.as_ref()
            .and_then(|rt| self.rif_src.as_ref().and_then(|src| get_rif(&src.rifs, rt)))
            .map(|rif| rif.enum_defs.as_slice())
            .unwrap_or(&[]);
        if let Some(action) = Self::show_enum_editor(ui, &mut self.enum_editor, &self.edit_err, enum_defs) {
            self.pending = Some(action);
        }
        // "Add register" modal, when open
        if let Some(action) = Self::show_add_register_modal(ui, &mut self.reg_add_editor, &self.edit_err) {
            self.pending = Some(action);
        }
        // Clocking-table modal, when open
        if let Some(action) = Self::show_rif_clocking_editor(ui, &mut self.rif_clocking_editor, &self.edit_err) {
            self.pending = Some(action);
        }
        // Parameters/generics modal, when open
        if let Some(action) = Self::show_rif_params_editor(ui, &mut self.rif_params_editor, &self.edit_err) {
            self.pending = Some(action);
        }
        // "Delete register?" confirmation, when a delete would also remove the definition
        if let Some(pending_delete) = &self.confirm_delete_reg {
            let mut confirmed = false;
            let mut cancel = false;
            let msg = match &pending_delete.inst_name {
                Some(name) => format!(
                    "Delete instance '{name}'? It's the last instance of register type '{}', \
                     so its definition (and all its fields) will be removed too.",
                    pending_delete.reg_type
                ),
                None => format!(
                    "Delete register '{}'? Its definition (and all its fields) will be removed.",
                    pending_delete.reg_type
                ),
            };
            egui::Window::new("Delete register?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(msg);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if confirmed {
                self.pending = Some(EditAction::DeleteRegister {
                    rif_type: pending_delete.rif_type.clone(),
                    page_name: pending_delete.page_name.clone(),
                    reg_type: pending_delete.reg_type.clone(),
                    inst_name: pending_delete.inst_name.clone(),
                });
                self.confirm_delete_reg = None;
            } else if cancel {
                self.confirm_delete_reg = None;
            }
        }
        // Address-conflict resolution (Swap/Shift/Insert/Cancel), when a committed address
        // change collided with another register — see `resolve_reg_addr_commit`.
        if self.confirm_reg_addr_conflict.is_some() {
            let mut clicked: Option<RegAddrResolution> = None;
            let mut cancel = false;
            {
                let pc = self.confirm_reg_addr_conflict.as_ref().expect("checked Some above");
                egui::Window::new("Address conflict")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.label(format!(
                            "Moving '{}' to 0x{:x} collides with '{}'. How should this be resolved?",
                            pc.inst_name, pc.new_addr, pc.target_name
                        ));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if pc.swap.is_some() && ui.button("Swap").clicked() { clicked = Some(RegAddrResolution::Swap); }
                            if pc.shift.is_some() && ui.button("Shift").clicked() { clicked = Some(RegAddrResolution::Shift); }
                            if pc.insert.is_some() && ui.button("Insert").clicked() { clicked = Some(RegAddrResolution::Insert); }
                            if ui.button("Cancel").clicked() { cancel = true; }
                        });
                    });
            }
            if let Some(resolution) = clicked {
                let pc = self.confirm_reg_addr_conflict.take().expect("checked Some above");
                let companions = match resolution {
                    RegAddrResolution::Swap => pc.swap.expect("button only enabled when Some"),
                    RegAddrResolution::Shift => pc.shift.expect("button only enabled when Some"),
                    RegAddrResolution::Insert => pc.insert.expect("button only enabled when Some"),
                };
                self.pending = Some(EditAction::UpdateRegDefWithMoves {
                    rif_type: pc.rif_type, orig_name: pc.orig_name, inst_name: pc.inst_name, vals: pc.vals, companions,
                });
            } else if cancel {
                if let Some(ed) = self.reg_editor.as_mut() {
                    ed.addr = ed.addr_orig.clone();
                }
                self.confirm_reg_addr_conflict = None;
            }
        }
        // Register-array change confirmation (field conversion / shift cascade / Cancel), when a
        // committed array-size change needs converting non-array fields and/or shifting following
        // registers on a manual page — see `resolve_reg_commit`.
        if self.confirm_reg_array_change.is_some() {
            let mut confirmed = false;
            let mut cancel = false;
            {
                let pc = self.confirm_reg_array_change.as_ref().expect("checked Some above");
                egui::Window::new("Register array change")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        if !pc.fields_to_convert.is_empty() {
                            ui.label(format!(
                                "These fields aren't arrays yet and will be converted to [1]: {}. \
                                 Once converted, they become read-only in the field panel (same as \
                                 any array field on an array register) — set reset/description/access \
                                 first if you still need to change them.",
                                pc.fields_to_convert.join(", ")
                            ));
                            ui.add_space(6.0);
                        }
                        if let Some(companions) = &pc.companions
                            && !companions.is_empty()
                        {
                            let names = companions.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
                            ui.label(format!("These following registers will shift forward to make room: {names}."));
                            ui.add_space(6.0);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Confirm").clicked() { confirmed = true; }
                            if ui.button("Cancel").clicked() { cancel = true; }
                        });
                    });
            }
            if confirmed {
                let pc = self.confirm_reg_array_change.take().expect("checked Some above");
                let companions = pc.companions.unwrap_or_default();
                self.pending = Some(match pc.kind {
                    RegArrayChangeKind::Definition(vals) => if companions.is_empty() {
                        EditAction::UpdateRegDef { rif_type: pc.rif_type, orig_name: pc.orig_name, inst_name: pc.inst_name, vals: *vals }
                    } else {
                        EditAction::UpdateRegDefWithMoves { rif_type: pc.rif_type, orig_name: pc.orig_name, inst_name: pc.inst_name, vals: *vals, companions }
                    },
                    RegArrayChangeKind::Instance(array) => {
                        EditAction::UpdateRegInstArray { rif_type: pc.rif_type, inst_name: pc.inst_name, array, companions }
                    }
                });
            } else if cancel {
                if let Some(ed) = self.reg_editor.as_mut() {
                    ed.array = ed.array_orig.clone();
                    ed.inst_array = ed.inst_array_orig.clone();
                }
                self.confirm_reg_array_change = None;
            }
        }
        let edit_mode = self.edit_mode;
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
        // "Switch RIF" guard: block navigating to a different RIF type than
        // `validated_selection`'s while that one still has an edit pending (committed or still
        // sitting unflushed in an open editor) — revert the just-made selection change and ask
        // for Save/Discard/Cancel instead. Placed here so it sees tree/breadcrumb clicks (which
        // land on `self.selected` earlier this same frame) before the content panel below acts
        // on them.
        let attempted_rif = self.rif_type_of(&self.selected.path);
        if attempted_rif != self.rif_type_of(&self.validated_selection.path) {
            match self.pending_edit_owner() {
                Some(owner) if owner != attempted_rif => {
                    self.confirm_switch_rif = Some(PendingRifSwitch { target: self.selected.clone(), from_rif: owner });
                    self.selected = self.validated_selection.clone();
                }
                _ => self.validated_selection = self.selected.clone(),
            }
        }
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
                    // Capture the top component too: a plain top-level RIF is at path[0],
                    // which the skip(1) loop below would otherwise never see.
                    let mut rif_ref : Option<CompRef> = if matches!(comp_ref, CompRef::Rif(_)) {
                        Some(comp_ref.clone())
                    } else {
                        None
                    };
                    for name in self.selected.path.iter().skip(1) {
                        (comp_ref, offset) = Self::get_comp_ref(comp_ref, name);
                        if matches!(comp_ref, CompRef::Rif(_)) {
                            rif_ref = Some(comp_ref.clone());
                        }
                        addr += offset;
                    }
                    // Type name of the enclosing RIF, needed to locate the definition to edit
                    let rif_type = self.selected_rif_type();
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
                    let src_pages: &[RifPage] = self.rif_src.as_ref()
                        .and_then(|src| get_rif(&src.rifs, &rif_type))
                        .map(|rif| rif.pages.as_slice())
                        .unwrap_or(&[]);
                    match comp_ref {
                        CompRef::External(ext)  => Self::display_content_ext(ui, ext),
                        CompRef::Rifmux(rifmux) => Self::display_content_rifmux(ui, &mut self.selected, rifmux),
                        CompRef::Rif(rif)       => {
                            if edit_mode
                                && let Some(src_rif) = self.rif_src.as_ref().and_then(|src| get_rif(&src.rifs, &rif_type))
                            {
                                let action = Self::show_rif_editor(ui, &mut self.rif_editor, &mut self.rif_clocking_editor, &mut self.rif_params_editor, &mut self.rif_intf_file_dialog, &self.edit_err, &rif_type, src_rif);
                                if action.is_some() {
                                    self.pending = action;
                                }
                                ui.separator();
                                ui.add_space(6.0);
                            }
                            let click = Self::display_content_rif(ui, &mut self.selected, rif, edit_mode, src_pages);
                            match &click {
                                Some(PageClick::ConvertToManual { page_name }) => {
                                    if let Some(compiled) = rif.pages.iter().find(|p| &p.name == page_name) {
                                        self.pending = Some(EditAction::ConvertPageToManual {
                                            rif_type: rif_type.clone(),
                                            page_name: page_name.clone(),
                                            compiled: Box::new(compiled.clone()),
                                        });
                                    }
                                }
                                Some(PageClick::OpenAddRegister { page_name }) => {
                                    if let Some(compiled_page) = rif.pages.iter().find(|p| &p.name == page_name) {
                                        let addr = Self::next_free_addr(compiled_page, rif.data_width);
                                        if let Some(src) = self.rif_src.as_ref()
                                            && let Some(src_rif) = get_rif(&src.rifs, &rif_type)
                                            && let Some(src_page) = src_rif.pages.iter().find(|p| &p.name == page_name)
                                        {
                                            self.reg_add_editor = Some(RegAddEditor::open(rif_type.clone(), page_name.clone(), src_page, compiled_page, addr));
                                        }
                                    }
                                }
                                _ => Self::handle_page_click(&self.rif_src, &mut self.confirm_delete_reg, &mut self.pending, &rif_type, click),
                            }
                        }
                        CompRef::Page(page)     => {
                            let click = Self::display_content_page(ui, &mut self.selected, page, edit_mode, src_pages);
                            match &click {
                                Some(PageClick::ConvertToManual { page_name }) => {
                                    self.pending = Some(EditAction::ConvertPageToManual {
                                        rif_type: rif_type.clone(),
                                        page_name: page_name.clone(),
                                        compiled: Box::new(page.clone()),
                                    });
                                }
                                Some(PageClick::OpenAddRegister { page_name }) => {
                                    let data_width = match &rif_ref {
                                        Some(CompRef::Rif(rif)) => rif.data_width,
                                        _ => 4,
                                    };
                                    let addr = Self::next_free_addr(page, data_width);
                                    if let Some(src) = self.rif_src.as_ref()
                                        && let Some(src_rif) = get_rif(&src.rifs, &rif_type)
                                        && let Some(src_page) = src_rif.pages.iter().find(|p| &p.name == page_name)
                                    {
                                        self.reg_add_editor = Some(RegAddEditor::open(rif_type.clone(), page_name.clone(), src_page, page, addr));
                                    }
                                }
                                _ => Self::handle_page_click(&self.rif_src, &mut self.confirm_delete_reg, &mut self.pending, &rif_type, click),
                            }
                        }
                        CompRef::Reg(reg)       => {
                            // Hoisted once, shared by the header below and both editors it
                            // governs (this register's own, and — once a field is drilled
                            // into — the field-selected block further down).
                            let src_reg_page = self.rif_src.as_ref()
                                .and_then(|src| get_rif(&src.rifs, &rif_type))
                                .and_then(|rif| find_reg_page(rif, &reg.reg_type));
                            let src_inst = src_reg_page.and_then(|p| p.instances.iter().find(|i| i.inst_name == reg.reg_name));
                            let can_edit_def = edit_mode && Self::is_reg_editable(reg);
                            let can_edit_ovr = edit_mode && Self::is_reg_override_editable(reg);
                            if can_edit_def || can_edit_ovr {
                                // Accurate for both auto (always 1) and manual (possibly several
                                // named instances sharing one type) pages — a source-level count
                                // via `page.instances` would instead reflect override entries on
                                // an automatic page, not true instance multiplicity.
                                let compiled_count = match &rif_ref {
                                    Some(CompRef::Rif(rif)) => Self::find_compiled_reg_page(rif, &reg.reg_name)
                                        .map_or(1, |p| p.inst_by_type(&reg.reg_type).count()),
                                    _ => 1,
                                };
                                let show_toggle = can_edit_ovr && (compiled_count > 1 || reg.incl.is_some());
                                Self::show_edit_target_header(ui, &mut self.edit_target, reg, show_toggle, !can_edit_def);
                            }
                            if edit_mode && reg.is_intr_derived()
                                && let Some(def_reg) = self.rif_src.as_ref()
                                    .and_then(|src| get_rif(&src.rifs, &rif_type))
                                    .and_then(|rif| find_regdef(rif, &reg.reg_type))
                            {
                                let action = Self::show_reg_intr_desc_editor(
                                    ui, &mut self.reg_intr_desc_editor, &self.edit_err, &rif_type, reg, def_reg, reg.intr_info.0,
                                );
                                if action.is_some() {
                                    self.pending = action;
                                }
                                ui.separator();
                                ui.add_space(6.0);
                            } else if can_edit_ovr && self.edit_target == EditTarget::Instance {
                                let action = Self::show_reg_override_editor(
                                    ui, &mut self.reg_override_editor, &self.edit_err, &rif_type, reg, src_inst,
                                );
                                if action.is_some() {
                                    self.pending = action;
                                }
                                ui.separator();
                                ui.add_space(6.0);
                            } else if can_edit_def
                                && let Some(def_reg) = self.rif_src.as_ref()
                                    .and_then(|src| get_rif(&src.rifs, &rif_type))
                                    .and_then(|rif| find_regdef(rif, &reg.reg_type))
                            {
                                // Bare `wrPulse`/`rdPulse`/`accPulse` resolve to the RIF's own
                                // default software clock at parse time (`reg_pulse_info` in
                                // `parser_reg.rs` uses `sw_clocking.last()`) — mirrored here so a
                                // newly-enabled "Reg" pulse recompiles with a real clock instead
                                // of a placeholder.
                                let default_clk = match &rif_ref {
                                    Some(CompRef::Rif(rif)) => rif.sw_clocking.last().map(|c| c.clk.as_str()).unwrap_or("clk"),
                                    _ => "clk",
                                };
                                // Address editing needs the owning page to already be manual
                                // (`RifPage::is_auto` — not `RifPageInst`, which carries no such
                                // flag) AND an actual `RegInst` entry for this specific instance.
                                let addr_editable = src_reg_page
                                    .is_some_and(|page| !page.is_auto() && page.instances.iter().any(|i| i.inst_name == reg.reg_name));
                                // Distinct group names already in use on the page, for the
                                // "Group" row's dropdown (own group included).
                                let mut group_choices: Vec<String> = Vec::new();
                                for g in src_reg_page.map(|p| p.registers.as_slice()).unwrap_or(&[])
                                    .iter().filter_map(|r| r.get_regdef()).map(|d| d.group.name.clone())
                                {
                                    if !group_choices.contains(&g) { group_choices.push(g); }
                                }
                                // Needed for the address-conflict check (`resolve_reg_addr_commit`):
                                // the page's other compiled registers (for collision/cascade lookup)
                                // and the RIF's data width (for alignment + the Shift step size).
                                let page_regs: &[RifRegInst] = match &rif_ref {
                                    Some(CompRef::Rif(rif)) => Self::find_compiled_reg_page(rif, &reg.reg_name).map(|p| p.regs.as_slice()).unwrap_or(&[]),
                                    _ => &[],
                                };
                                let data_width = self.rif_src.as_ref()
                                    .and_then(|src| get_rif(&src.rifs, &rif_type))
                                    .map(|r| r.data_width)
                                    .unwrap_or_default();
                                // Register-array editing (both directions) is out of scope for a
                                // type with more than one manual instance on the page — an
                                // automatic page never has this ambiguity (see design notes).
                                let array_scope_ok = src_reg_page.is_some_and(|p| {
                                    p.is_auto() || p.instances.iter().filter(|i| i.type_name == reg.reg_type).count() <= 1
                                });
                                let action = Self::show_reg_editor(
                                    ui, &mut self.reg_editor, &self.edit_err, &rif_type, reg, def_reg, default_clk, addr_editable,
                                    &group_choices, data_width, page_regs, src_reg_page, src_inst, array_scope_ok,
                                    &mut self.confirm_reg_addr_conflict, &mut self.confirm_reg_array_change,
                                );
                                if action.is_some() {
                                    self.pending = action;
                                }
                                ui.separator();
                                ui.add_space(6.0);
                            }
                            match Self::display_content_reg(ui, &self.selected, reg, edit_mode) {
                                Some(RegClick::Select(name)) => self.selected.item = SelectedItem::Field(name),
                                Some(RegClick::AddField) => {
                                    self.pending = Some(EditAction::AddField {
                                        rif_type: rif_type.clone(),
                                        reg_type: reg.reg_type.clone(),
                                    });
                                }
                                Some(RegClick::MoveField { moves }) => {
                                    self.pending = Some(EditAction::MoveField {
                                        rif_type: rif_type.clone(),
                                        reg_type: reg.reg_type.clone(),
                                        moves,
                                    });
                                }
                                None => {}
                            }
                        }
                        CompRef::None => {},
                    }
                    // Add field description panel when a field is selected
                    if let (SelectedItem::Field(field_name), CompRef::Reg(reg)) = (&self.selected.item, &comp_ref) {
                        let basename = field_name.split('[').next().unwrap_or(field_name);
                        if let Some(field) = reg.fields.iter().find(|f| f.name == basename) {
                            ui.separator();
                            ui.add_space(10.0);
                            // The definition carries advanced properties the instance doesn't (e.g. lock)
                            let def_field = self.rif_src.as_ref()
                                .and_then(|src| get_rif(&src.rifs, &rif_type))
                                .and_then(|rif| find_field(rif, &reg.reg_type, basename));
                            let enum_defs: &[EnumDef] = self.rif_src.as_ref()
                                .and_then(|src| get_rif(&src.rifs, &rif_type))
                                .map(|rif| rif.enum_defs.as_slice())
                                .unwrap_or(&[]);
                            let can_edit_def = edit_mode && Self::is_reg_editable(reg);
                            let can_edit_ovr = edit_mode && Self::is_reg_override_editable(reg) && Self::is_field_override_editable(field);
                            if edit_mode && reg.is_intr_derived()
                                && let Some(def_field) = def_field
                            {
                                // Derived register: only the field's own description override for
                                // this kind is editable — no position/access/other controls.
                                let action = Self::show_field_intr_desc_editor(
                                    ui, &mut self.field_intr_desc_editor, &self.edit_err,
                                    &rif_type, &reg.reg_type, field, def_field, reg.intr_info.0,
                                );
                                if action.is_some() {
                                    self.pending = action;
                                }
                            } else if can_edit_ovr && self.edit_target == EditTarget::Instance {
                                let reg_override = self.rif_src.as_ref()
                                    .and_then(|src| get_rif(&src.rifs, &rif_type))
                                    .and_then(|rif| find_reg_page(rif, &reg.reg_type))
                                    .and_then(|p| p.instances.iter().find(|i| i.inst_name == reg.reg_name))
                                    .and_then(|i| i.reg_override.get(&None));
                                let action = Self::show_field_override_editor(
                                    ui, &mut self.field_override_editor, &self.edit_err, &rif_type, &reg.reg_name, field, reg_override,
                                );
                                if action.is_some() {
                                    self.pending = action;
                                }
                            } else if can_edit_def && let Some(def_field) = def_field
                                && Self::is_field_editable(reg, field, def_field)
                            {
                                // Only a field of the base/primary interrupt register gets the
                                // trigger/clear override row — see `FieldEditor::reg_intr_default`.
                                let reg_intr_default = reg.is_intr().then(|| {
                                    self.rif_src.as_ref()
                                        .and_then(|src| get_rif(&src.rifs, &rif_type))
                                        .and_then(|rif| find_regdef(rif, &reg.reg_type))
                                        .and_then(|d| d.interrupt.first())
                                        .map(|intr| InterruptInfoField { trigger: Some(intr.trigger), clear: Some(intr.clear) })
                                }).flatten();
                                // Delete must stay disabled once a register's only field is an
                                // array — `reg.fields` holds one instance per array element, so
                                // its length overcounts; count field *definitions* instead.
                                let can_delete = self.rif_src.as_ref()
                                    .and_then(|src| get_rif(&src.rifs, &rif_type))
                                    .and_then(|rif| find_regdef(rif, &reg.reg_type))
                                    .map(Self::field_can_delete)
                                    .unwrap_or(reg.fields.len() > 1);
                                // `arrayPartial` is only stamped on the continuation half(s), so
                                // the base half is checked the same way (by name, across the group).
                                let array_partial_locked = self.rif_src.as_ref()
                                    .and_then(|src| get_rif(&src.rifs, &rif_type))
                                    .is_some_and(|rif| field_has_array_partial(rif, &reg.group_type, basename));
                                // Editable view of the field definition
                                let action = Self::show_field_editor(
                                    ui, &mut self.field_editor,
                                    &self.edit_err, &reg.reg_type, &reg.group_type, &rif_type, field, def_field,
                                    enum_defs, &mut self.enum_editor,
                                    can_delete,
                                    reg_intr_default,
                                    array_partial_locked,
                                );
                                if action.is_some() {
                                    self.pending = action;
                                }
                            } else {
                                // Read-only view
                                let mut enum_def = None;
                                let mut enum_rst  = None;
                                if let (Some(enum_type),Some(CompRef::Rif(rif))) = (field.enum_kind.name(), rif_ref) {
                                    enum_def = rif.enum_defs.iter().find(|e| e.name==*enum_type);
                                    if let Some(d) = enum_def {
                                        enum_rst = d.values.iter().find(|e| e.value==field.reset.to_u128(field.width() as u8) as u8).map(|e| e.name.clone());
                                    }
                                }
                                let rst_str = enum_rst.unwrap_or(get_field_rst_str(field));
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
                                if edit_mode {
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("This field or register type can't be edited yet.").italics().weak());
                                }
                            }
                       }
                    }
                }
            });
        });
        // Apply any edit requested during this frame, then recompile the view
        self.apply_pending();
    }
}

impl RifViewer {


    fn show_bottom_panel(&mut self, ui: &mut egui::Ui) {
        let mut do_save = false;
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
                    if !self.search_query.is_empty() && (self.search_by_name || self.search_by_desc)
                        && let Some(ref comp) = self.rif_comp {
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

                // Edit toggle and Save, pinned to the right end of the bar
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_edit = self.rif_src.is_some();
                    ui.add_enabled_ui(can_edit, |ui| {
                        ui.toggle_value(&mut self.edit_mode, RichText::new("✏ Edit"))
                            .on_hover_text("Enable editing of register field definitions");
                    });
                    if self.edit_mode {
                        let has_changes = self.has_unsaved();
                        ui.add_enabled_ui(has_changes, |ui| {
                            if ui.button(RichText::new("💾 Save"))
                                .on_hover_text("Write edited field definitions back to the .rif file")
                                .clicked()
                            {
                                do_save = true;
                            }
                        });
                        if has_changes {
                            ui.label(RichText::new("● unsaved").weak());
                        }
                    }
                });

            });
        });
        if do_save {
            self.save_file();
        }
    }

    /// Type name of the RIF that owns the current selection
    fn selected_rif_type(&self) -> String {
        self.rif_type_of(&self.selected.path)
    }

    /// Find Type name of a RIF  b ypath
    fn rif_type_of(&self, path: &[String]) -> String {
        let Some(comp) = self.rif_comp.as_ref() else { return String::new(); };
        let mut comp_ref = match comp {
            Comp::Rifmux(r) => CompRef::Rifmux(r),
            Comp::Rif(r) => CompRef::Rif(r),
            Comp::External(r) => CompRef::External(r),
        };
        let mut rif_type = if let CompRef::Rif(ri) = &comp_ref { ri.type_name.clone() } else { String::new() };
        for name in path.iter().skip(1) {
            let (next, _) = Self::get_comp_ref(comp_ref, name);
            comp_ref = next;
            if let CompRef::Rif(ri) = &comp_ref {
                rif_type = ri.type_name.clone();
            }
        }
        rif_type
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
