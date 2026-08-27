use yarig::comp::comp_inst::RifPageInst;
use yarig::parser::parser_expr::{parse_expr, ExprTokens};
use yarig::rifgen::{Access, DataWidth, ExternalKind, InterruptClr, InterruptTrigger, RegDef, RegInst, RegOverride, RegPulseKind, ResetValP, RifPage, Visibility, Width};

use crate::field_editor::parse_reset;
use crate::apply_pending::EditAction;

/// Parse a register address. Addresses are conventionally written in hex throughout `.rif`
/// files (`AddressOffset::to_rif` always emits `0x...`), so — unlike `parse_u8`/`parse_reset`,
/// which default to decimal — a bare numeral here is still read as hex (`100` means 0x100, not
/// 100 decimal); an explicit `0x`/`0X` prefix is accepted but not required.
fn parse_addr(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(hex, 16).map_err(|_| format!("Invalid address '{s}'"))
}

/// Live buffers for the register currently being edited: name/description/access-pulse (shared
/// by every instance of this type), plus the address of the *specific instance* being viewed.
/// Reloaded whenever `inst_name` no longer matches the selection — keyed on the instance, not
/// the type (`FieldEditor`-style), because two instances of the same type share every buffer
/// here except address: switching from one to another must reload just that one field, and
/// keying on `reg_type` alone would miss the switch entirely. Only `RegDef`-level (type) editing
/// is wired for name/description/pulse — an instance's own display name is still deferred, same
/// as `FieldEditor`'s "Definition only, Instance inert" scoping; address is the one genuinely
/// instance-level property exposed here, because it has nowhere else to live.
pub struct RegEditor {
    /// RIF this register belongs to — stashed at load time so a pending edit can still be
    /// flushed after the selection has already moved on to a different register/rif (see
    /// `FieldEditor`'s identical `rif_type` field for why the caller's own argument isn't enough).
    pub rif_type: String,
    /// Register type name (used to find/mutate the `RegDef` on Apply).
    pub reg_type: String,
    /// Instance name these buffers were loaded from (the reload key — see above).
    pub inst_name: String,
    pub name: String,
    pub name_orig: String,
    /// Register group name (`RegDef::get_group_name()`) — see `show_reg_editor`'s "Group" row
    /// for how it's edited (free-text buffer plus a dropdown of the current page's other group
    /// names).
    pub group: String,
    pub group_orig: String,
    pub desc: String,
    pub desc_orig: String,
    pub wr_on: bool,
    pub wr_on_orig: bool,
    pub wr_reg: bool,
    pub wr_reg_orig: bool,
    pub rd_on: bool,
    pub rd_on_orig: bool,
    pub rd_reg: bool,
    pub rd_reg_orig: bool,
    pub acc_on: bool,
    pub acc_on_orig: bool,
    pub acc_reg: bool,
    pub acc_reg_orig: bool,
    /// The RIF's own default software clock (see `build_pulse`) — stashed alongside `rif_type`
    /// for the same reason: a deferred flush must use *this* register's clock, not whatever the
    /// caller's argument happens to be for the newly-selected one.
    pub default_clk: String,
    /// Hex address buffer (e.g. `0x100`), always populated from the compiled instance's current
    /// address regardless of automatic/manual mode — see `addr_editable`.
    pub addr: String,
    pub addr_orig: String,
    /// Whether the address field is actually editable: the owning page must already be manual
    /// (checked at the call site via `find_reg_page`/`RifPage::is_auto`) — shown either way so
    /// the current address is visible, but only accepted on Apply when true.
    pub addr_editable: bool,
    /// The RIF's data width, threaded in once at load time (mirrors `default_clk`) — gives
    /// `.addr_mask()` for `build_action`'s alignment check and `.nb_byte()` for the Shift
    /// cascade's step size (see `resolve_reg_addr_commit`).
    pub data_width: DataWidth,
    /// Definition-level array size (`RegDef.array`, shared by every instance of the type);
    /// blank/"0" means "not an array" (shows the "Array" button instead of the label+box —
    /// mirrors `FieldEditor::array`'s exact UX). A `$param`-sized array (`Width::Param`) also
    /// reads as blank here (out of scope for this control, same exclusion `FieldEditor` already
    /// applies to param-sized field arrays) — `array_editable` is `false` in that case so the
    /// button can't silently clobber it.
    pub array: String,
    pub array_orig: String,
    /// Whether the "Array" control is enabled: the type isn't `$param`-sized, this instance
    /// doesn't already carry its own instance-level array (`RegInst.array` — the library
    /// hard-errors on array-in-both), and the type has at most one manual instance on the page
    /// (register-array editing is out of scope for multi-instance types for now).
    pub array_editable: bool,
    /// Instance-level array size (`RegInst.array`, formatted via `ExprTokens::to_rif()`) — one
    /// specific manual instance's own array, independent of the shared type.
    pub inst_array: String,
    pub inst_array_orig: String,
    /// Whether the "Instance array" control is enabled: the page is already manual (same
    /// condition `addr_editable` uses), the shared type isn't itself an array (mutual exclusion),
    /// and the type has at most one manual instance on the page (same scope bound as
    /// `array_editable`).
    pub inst_array_editable: bool,
    /// Live "Interrupt register" checkbox state — mutable.
    pub has_intr: bool,
    pub has_intr_orig: bool,
    pub intr_trigger: InterruptTrigger,
    pub intr_trigger_orig: InterruptTrigger,
    pub intr_clear: InterruptClr,
    pub intr_clear_orig: InterruptClr,
    pub intr_enable_on: bool,
    pub intr_enable_on_orig: bool,
    pub intr_enable_rst: String,
    pub intr_enable_rst_orig: String,
    pub intr_mask_on: bool,
    pub intr_mask_on_orig: bool,
    pub intr_mask_rst: String,
    pub intr_mask_rst_orig: String,
    pub intr_pending_on: bool,
    pub intr_pending_on_orig: bool,
    /// Named secondary interrupt block. Blank `alt_name` means no alt interrupt
    pub alt_name: String,
    pub alt_name_orig: String,
    pub alt_trigger: InterruptTrigger,
    pub alt_trigger_orig: InterruptTrigger,
    pub alt_clear: InterruptClr,
    pub alt_clear_orig: InterruptClr,
    pub alt_enable_on: bool,
    pub alt_enable_on_orig: bool,
    pub alt_enable_rst: String,
    pub alt_enable_rst_orig: String,
    pub alt_mask_on: bool,
    pub alt_mask_on_orig: bool,
    pub alt_mask_rst: String,
    pub alt_mask_rst_orig: String,
    pub alt_pending_on: bool,
    pub alt_pending_on_orig: bool,
    // Advanced properties (shown in the "Advanced properties" fold, alongside Interrupt/Access
    // pulse above): visibility, clock/reset override, and external kind. Unlike `group`'s
    // "only emit when explicitly touched" `Option<String>`, these are always applied unconditionally
    // on Apply — the buffer always mirrors the definition's actual current value, and
    // `apply_regdef_vals`'s generic `RegProp::ALL` diff (comparing before/after `fmt_prop`) is
    // what decides whether any source line actually needs touching, exactly like `desc`/`pulse`.
    pub visibility: Visibility,
    pub visibility_orig: Visibility,
    /// Optional clock override (`RegDef.clk`); blank means "not set" (`None`).
    pub clk: String,
    pub clk_orig: String,
    /// Optional reset override (`RegDef.rst`, the register's own `hwReset` line); blank means
    /// "not set" (`None`).
    pub rst: String,
    pub rst_orig: String,
    /// Restricted to the two keyword-reachable states (`None`/`ReadWrite`/`Done`) — `Read`/
    /// `Write` have no grammar of their own yet (see `RegProp::External`'s doc comment).
    pub external: ExternalKind,
    pub external_orig: ExternalKind,
    pub parse_err: Option<String>,
}

impl RegEditor {
    #[allow(clippy::too_many_arguments)]
    pub fn from_regdef(
        inst_name: String, reg_type: String, def: &RegDef, addr: u64, addr_editable: bool,
        rif_type: &str, default_clk: &str, data_width: DataWidth,
        // Source `RegInst` for the instance currently being viewed, if any (only manual pages
        // have one) — feeds `inst_array` and the array-mutual-exclusion check.
        src_inst: Option<&RegInst>,
        // Whether this type has at most one manual instance on its page — register-array editing
        // (both directions) is out of scope for multi-instance types for now (see design notes).
        array_scope_ok: bool,
    ) -> Self {
        let wr = def.pulse.iter().find_map(|p| if let RegPulseKind::Write(c) = p { Some(c.as_str()) } else { None });
        let rd = def.pulse.iter().find_map(|p| if let RegPulseKind::Read(c) = p { Some(c.as_str()) } else { None });
        let acc = def.pulse.iter().find_map(|p| if let RegPulseKind::Access(c) = p { Some(c.as_str()) } else { None });
        let addr_str = format!("0x{addr:x}");
        let name = def.name.clone();
        let group = def.group.name.clone();
        let desc = def.description.get(true);
        let (wr_on, wr_reg) = (wr.is_some(), wr.map(|c| !c.is_empty()).unwrap_or(true));
        let (rd_on, rd_reg) = (rd.is_some(), rd.map(|c| !c.is_empty()).unwrap_or(true));
        let (acc_on, acc_reg) = (acc.is_some(), acc.map(|c| !c.is_empty()).unwrap_or(true));
        // In scope for the "Interrupt register" toggle: either no interrupt at all (plain,
        // convertible), exactly one unnamed primary interrupt block (already one, removable), or
        // a primary block plus exactly one named `alt` secondary block. Two or more `alt` blocks
        // stay out of scope entirely, same as "no editor at all" before this toggle existed.
        let has_intr = !def.interrupt.is_empty();
        let has_intr_alt = def.interrupt.len() > 1;
        let intr = has_intr.then(|| &def.interrupt[0]);
        let has_intr = intr.is_some();
        let intr_trigger = intr.map(|i| i.trigger).unwrap_or_default();
        let intr_clear = intr.map(|i| i.clear).unwrap_or_default();
        let intr_enable_on = intr.is_some_and(|i| i.enable.is_some());
        let intr_enable_rst = intr.and_then(|i| i.enable.as_ref()).map(|v| v.to_rif()).unwrap_or_default();
        let intr_mask_on = intr.is_some_and(|i| i.mask.is_some());
        let intr_mask_rst = intr.and_then(|i| i.mask.as_ref()).map(|v| v.to_rif()).unwrap_or_default();
        let intr_pending_on = intr.is_some_and(|i| i.pending);
        // Alt secondary block: a freshly-added one defaults Trigger/Clear to the primary's
        // current values (mirrors the parser's own "alt inherits from primary when unset"
        // default — `Context::InterruptAlt` in `parser_file.rs`), rather than the bare enum
        // default, so it starts out sensible instead of silently reverting to High/rclr.
        let alt = has_intr_alt.then(|| &def.interrupt[1]);
        let alt_name = alt.map(|a| a.name.clone()).unwrap_or_default();
        let alt_trigger = alt.map(|a| a.trigger).unwrap_or(intr_trigger);
        let alt_clear = alt.map(|a| a.clear).unwrap_or(intr_clear);
        let alt_enable_on = alt.is_some_and(|a| a.enable.is_some());
        let alt_enable_rst = alt.and_then(|a| a.enable.as_ref()).map(|v| v.to_rif()).unwrap_or_default();
        let alt_mask_on = alt.is_some_and(|a| a.mask.is_some());
        let alt_mask_rst = alt.and_then(|a| a.mask.as_ref()).map(|v| v.to_rif()).unwrap_or_default();
        let alt_pending_on = alt.is_some_and(|a| a.pending);
        let array = match def.array {
            Width::Value(0) => String::new(),
            Width::Value(n) => n.to_string(),
            Width::Param(_) => String::new(),
        };
        let array_editable = array_scope_ok
            && matches!(def.array, Width::Value(_))
            && src_inst.is_none_or(|i| i.array.is_empty());
        let inst_array = src_inst.map(|i| i.array.to_rif()).unwrap_or_default();
        let inst_array_editable = array_scope_ok && addr_editable && matches!(def.array, Width::Value(0));
        let visibility = def.visibility;
        let clk = def.clk.clone().unwrap_or_default();
        let rst = def.rst.clone().unwrap_or_default();
        let external = def.external;
        RegEditor {
            rif_type: rif_type.to_owned(),
            reg_type,
            inst_name,
            name: name.clone(), name_orig: name,
            group: group.clone(), group_orig: group,
            desc: desc.clone(), desc_orig: desc,
            wr_on, wr_on_orig: wr_on, wr_reg, wr_reg_orig: wr_reg,
            rd_on, rd_on_orig: rd_on, rd_reg, rd_reg_orig: rd_reg,
            acc_on, acc_on_orig: acc_on, acc_reg, acc_reg_orig: acc_reg,
            default_clk: default_clk.to_owned(),
            addr: addr_str.clone(),
            addr_orig: addr_str,
            addr_editable,
            data_width,
            array: array.clone(), array_orig: array,
            array_editable,
            inst_array: inst_array.clone(), inst_array_orig: inst_array,
            inst_array_editable,
            has_intr, has_intr_orig: has_intr,
            intr_trigger, intr_trigger_orig: intr_trigger,
            intr_clear, intr_clear_orig: intr_clear,
            intr_enable_on, intr_enable_on_orig: intr_enable_on,
            intr_enable_rst: intr_enable_rst.clone(), intr_enable_rst_orig: intr_enable_rst,
            intr_mask_on, intr_mask_on_orig: intr_mask_on,
            intr_mask_rst: intr_mask_rst.clone(), intr_mask_rst_orig: intr_mask_rst,
            intr_pending_on, intr_pending_on_orig: intr_pending_on,
            alt_name: alt_name.clone(), alt_name_orig: alt_name,
            alt_trigger, alt_trigger_orig: alt_trigger,
            alt_clear, alt_clear_orig: alt_clear,
            alt_enable_on, alt_enable_on_orig: alt_enable_on,
            alt_enable_rst: alt_enable_rst.clone(), alt_enable_rst_orig: alt_enable_rst,
            alt_mask_on, alt_mask_on_orig: alt_mask_on,
            alt_mask_rst: alt_mask_rst.clone(), alt_mask_rst_orig: alt_mask_rst,
            alt_pending_on, alt_pending_on_orig: alt_pending_on,
            visibility, visibility_orig: visibility,
            clk: clk.clone(), clk_orig: clk,
            rst: rst.clone(), rst_orig: rst,
            external, external_orig: external,
            parse_err: None,
        }
    }

    /// Build the new `pulse` vector from the checkbox/kind buffers. `default_clk` (the RIF's own
    /// default software clock — see the call site) is used for every enabled "Reg" pulse, not
    /// just a freshly-enabled one: `fmt_prop` only ever emits a bare `reg` keyword (an explicit
    /// *different* clock name isn't round-tripped yet — see `RegDef::fmt_prop`'s doc comment), so
    /// using a single consistent clock for all of a register's pulses is both simplest and
    /// exactly matches what re-parsing the saved file would resolve to anyway. It also sidesteps
    /// the RTL generator's "only one clock per register's pulses" check (`trait_hw.rs`), which a
    /// stale or placeholder per-pulse clock string could otherwise trip.
    fn build_pulse(&self) -> Vec<RegPulseKind> {
        let clk = |on: bool, reg: bool| on.then(|| if reg { self.default_clk.clone() } else { String::new() });
        let mut v = Vec::new();
        if let Some(c) = clk(self.wr_on, self.wr_reg) { v.push(RegPulseKind::Write(c)); }
        if let Some(c) = clk(self.rd_on, self.rd_reg) { v.push(RegPulseKind::Read(c)); }
        if let Some(c) = clk(self.acc_on, self.acc_reg) { v.push(RegPulseKind::Access(c)); }
        v
    }

    /// Mirrors `FieldEditor::is_unchanged` — see its doc comment for the erring-direction rule.
    pub fn is_unchanged(&self) -> bool {
        self.name == self.name_orig
            && self.group == self.group_orig
            && self.desc == self.desc_orig
            && self.wr_on == self.wr_on_orig && self.wr_reg == self.wr_reg_orig
            && self.rd_on == self.rd_on_orig && self.rd_reg == self.rd_reg_orig
            && self.acc_on == self.acc_on_orig && self.acc_reg == self.acc_reg_orig
            && self.addr == self.addr_orig
            && self.array == self.array_orig
            && self.inst_array == self.inst_array_orig
            && self.has_intr == self.has_intr_orig
            && (!self.has_intr || (
                self.intr_trigger == self.intr_trigger_orig && self.intr_clear == self.intr_clear_orig
                && self.intr_enable_on == self.intr_enable_on_orig && self.intr_enable_rst == self.intr_enable_rst_orig
                && self.intr_mask_on == self.intr_mask_on_orig && self.intr_mask_rst == self.intr_mask_rst_orig
                && self.intr_pending_on == self.intr_pending_on_orig
            ))
            && self.alt_name == self.alt_name_orig
            && (self.alt_name.trim().is_empty() || (
                self.alt_trigger == self.alt_trigger_orig && self.alt_clear == self.alt_clear_orig
                && self.alt_enable_on == self.alt_enable_on_orig && self.alt_enable_rst == self.alt_enable_rst_orig
                && self.alt_mask_on == self.alt_mask_on_orig && self.alt_mask_rst == self.alt_mask_rst_orig
                && self.alt_pending_on == self.alt_pending_on_orig
            ))
            && self.visibility == self.visibility_orig
            && self.clk == self.clk_orig
            && self.rst == self.rst_orig
            && self.external == self.external_orig
    }

    /// Parse+Validate the buffers into the `EditAction`
    /// Return `None` when validation failed
    pub fn build_action(&mut self) -> Option<EditAction> {
        self.parse_err = None;
        if self.name.trim().is_empty() {
            self.parse_err = Some("Name is empty".to_owned());
            return None;
        }
        if self.group.trim().is_empty() {
            self.parse_err = Some("Group is empty".to_owned());
            return None;
        }
        // Only carry a new group forward when it was actually edited — leaving it `None`
        // lets the implicit-group-follows-rename behavior in `UpdateRegDef`'s handler keep
        // working exactly as before whenever the user didn't touch this field.
        let group = (self.group.trim() != self.group_orig).then(|| self.group.trim().to_owned());
        // Only ever produce a new address when it's actually editable and actually changed —
        // never silently normalize an existing (possibly relative) address just because some
        // other field on the same register was edited.
        let addr = if self.addr_editable && self.addr != self.addr_orig {
            match parse_addr(&self.addr) {
                Ok(v) if v & self.data_width.addr_mask() != 0 => {
                    self.parse_err = Some(format!(
                        "Address must be aligned to {} bytes (the RIF's data width)", self.data_width.nb_byte()
                    ));
                    return None;
                }
                Ok(v) => Some(v),
                Err(e) => { self.parse_err = Some(e); return None; }
            }
        } else {
            None
        };
        // Only ever produce a new dimension when the "Array" control is actually editable and
        // actually changed — `resolve_reg_commit` decides what to do with it (direct apply,
        // confirmation modal for field conversion/address-shift, or nothing further).
        let array = if self.array_editable && self.array != self.array_orig {
            match self.array.trim() {
                "" => Some(0u8),
                s => match s.parse::<u8>() {
                    Ok(v) => Some(v),
                    Err(_) => { self.parse_err = Some("Invalid array size".to_owned()); return None; }
                }
            }
        } else {
            None
        };
        let intr = if !self.has_intr {
            if self.has_intr_orig {Some(None)} else {None}
        } else {
            let enable = if self.intr_enable_on {
                match parse_reset(&self.intr_enable_rst, false) {
                    Ok(v) => Some(v),
                    Err(e) => { self.parse_err = Some(format!("Enable reset: {e}")); return None; }
                }
            } else {
                None
            };
            let mask = if self.intr_mask_on {
                match parse_reset(&self.intr_mask_rst, false) {
                    Ok(v) => Some(v),
                    Err(e) => { self.parse_err = Some(format!("Mask reset: {e}")); return None; }
                }
            } else {
                None
            };
            Some(Some(RegIntrVals { trigger: self.intr_trigger, clear: self.intr_clear, enable, mask, pending: self.intr_pending_on }))
        };
        // Alt secondary block: only meaningful when the primary itself is present (an alt can't
        // exist without one — if `has_intr` was just unchecked, the primary-removal handler wipes
        // `RegDef.interrupt` entirely on its own, so there's nothing left for this to attach to).
        let alt = if !self.has_intr {
            None
        } else if self.alt_name.trim().is_empty() {
            Some(None)
        } else {
            let enable = if self.alt_enable_on {
                match parse_reset(&self.alt_enable_rst, false) {
                    Ok(v) => Some(v),
                    Err(e) => { self.parse_err = Some(format!("Alt enable reset: {e}")); return None; }
                }
            } else {
                None
            };
            let mask = if self.alt_mask_on {
                match parse_reset(&self.alt_mask_rst, false) {
                    Ok(v) => Some(v),
                    Err(e) => { self.parse_err = Some(format!("Alt mask reset: {e}")); return None; }
                }
            } else {
                None
            };
            Some(Some(RegAltVals {
                name: self.alt_name.trim().to_owned(),
                trigger: self.alt_trigger,
                clear: self.alt_clear,
                enable, mask,
                pending: self.alt_pending_on,
            }))
        };
        let clk = (!self.clk.trim().is_empty()).then(|| self.clk.trim().to_owned());
        let rst = (!self.rst.trim().is_empty()).then(|| self.rst.trim().to_owned());
        Some(EditAction::UpdateRegDef {
            rif_type: self.rif_type.clone(),
            orig_name: self.reg_type.clone(),
            inst_name: self.inst_name.clone(),
            vals: RegDefVals {
                name: self.name.trim().to_owned(),
                group,
                desc: self.desc.clone(),
                pulse: self.build_pulse(),
                addr,
                intr,
                alt,
                array,
                visibility: self.visibility,
                clk,
                rst,
                external: self.external,
            },
        })
    }
}

/// New values for a register definition, produced when the user hits "Apply" in the register
/// editor.
pub struct RegDefVals {
    pub name: String,
    /// New group name, when explicitly edited (`Some`). `None` means the "Group" buffer was
    /// left untouched, so the handler falls back to its existing implicit-group-follows-rename
    /// behavior instead of overwriting the group with a stale, un-edited copy of it.
    pub group: Option<String>,
    pub desc: String,
    pub pulse: Vec<RegPulseKind>,
    /// New absolute address for the specific instance currently being viewed
    pub addr: Option<u64>,
    /// Primary-interrupt state: Outer None means interrupt is untouched, inner means it is removed.
    pub intr: Option<Option<RegIntrVals>>,
    /// Alternate-interrupt state: Outer None means interrupt is untouched, inner means it is removed.
    pub alt: Option<Option<RegAltVals>>,
    /// New definition-level array dimension (`RegDef.array`), when the "Array" control was
    /// actually edited (`RegEditor::array_editable`). `None` leaves `RegDef.array` untouched;
    /// `Some(0)` removes the array; `Some(n>0)` creates/resizes it, converting every field
    /// currently at `Width::Value(0)` to `Width::Value(1)` (see `apply_regdef_vals`) — decided
    /// and confirmed by the user before this action is ever dispatched, not here.
    pub array: Option<u8>,
    /// New visibility (`RegDef.visibility`) — always applied unconditionally, like `desc`/
    /// `pulse`: the buffer always mirrors the definition's current value, and `apply_regdef_vals`'s
    /// generic `RegProp::ALL` diff decides whether any source line actually needs touching.
    pub visibility: Visibility,
    /// New clock override (`RegDef.clk`); `None` clears it back to "not set".
    pub clk: Option<String>,
    /// New reset override (`RegDef.rst`, the register's own `hwReset` line); `None` clears it
    /// back to "not set".
    pub rst: Option<String>,
    /// New external kind (`RegDef.external`) — restricted in the UI to the two keyword-reachable
    /// states (`None`/`ReadWrite`/`Done`), same scope as `RegProp::External`.
    pub external: ExternalKind,
}

/// New values for a register's primary interrupt, produced when the user hits "Apply" in the
/// register editor's "Interrupt" section.
pub struct RegIntrVals {
    pub trigger: InterruptTrigger,
    pub clear: InterruptClr,
    /// `Some` when the "Enable" derived register is toggled on (its reset value); `None` removes
    /// (or keeps absent) the derived register entirely.
    pub enable: Option<ResetValP>,
    /// Same as `enable`, for the "Mask" derived register.
    pub mask: Option<ResetValP>,
    pub pending: bool,
}

/// New values for a register's named secondary interrupt block, produced when the user hits
/// "Apply" in the register editor's "Alt interrupt" section. Mirrors `RegIntrVals`, plus the
/// name that identifies the block.
pub struct RegAltVals {
    pub name: String,
    pub trigger: InterruptTrigger,
    pub clear: InterruptClr,
    /// Same meaning as `RegIntrVals::enable`.
    pub enable: Option<ResetValP>,
    /// Same meaning as `RegIntrVals::mask`.
    pub mask: Option<ResetValP>,
    pub pending: bool,
}

/// What kind of register the "Add register" modal is about to create.
#[derive(Clone, PartialEq)]
pub enum AddRegKind {
    /// A brand new register type (definition + one starter field), plus — on an already-manual
    /// page — its own matching instance.
    New,
    /// A new instance of an existing, page-local register type. Only offered once the page is
    /// manual (automatic mode is strictly one instance per definition — see the design review).
    Instance { type_name: String },
}

/// Live buffers for the "Add register" modal. `page_types` is a snapshot of the page's own
/// register type names at the moment the modal was opened (dropdown contents don't need to
/// track further edits happening elsewhere while the modal is up).
pub struct RegAddEditor {
    pub rif_type: String,
    pub page_name: String,
    pub page_types: Vec<String>,
    pub page_is_auto: bool,
    /// Snapshot of the page's compiled instance, taken at modal-open time — the same freshness
    /// guarantee `addr` already relies on. Only used to drive the modal's own "Convert to manual
    /// addressing" button (`RifPageInst::convert_to_manual` needs the compiled page, not the
    /// source `RifPage`); kept even though the button disappears once `page_is_auto` flips to
    /// `false`, since nothing else in the modal needs to mutate or re-derive it.
    pub page_compiled: RifPageInst,
    pub kind: AddRegKind,
    pub name: String,
    /// Absolute address the new instance will get (only meaningful once the page is manual —
    /// see `AddRegKind::New`'s doc comment). Computed once at open time (next free slot after
    /// the page's current highest address) rather than re-derived every frame; not user-editable
    /// here — refine it afterward via the already-built per-register address field instead of
    /// growing this modal further.
    pub addr: u64,
    pub parse_err: Option<String>,
}

impl RegAddEditor {
    pub fn open(rif_type: String, page_name: String, page: &RifPage, page_compiled: &RifPageInst, addr: u64) -> Self {
        let page_types: Vec<String> = page.registers.iter().filter_map(|r| r.get_regdef()).map(|d| d.name.clone()).collect();
        let name = unique_reg_name(page);
        RegAddEditor {
            rif_type, page_name, page_types, page_is_auto: page.is_auto(), page_compiled: page_compiled.clone(),
            kind: AddRegKind::New, name, addr, parse_err: None,
        }
    }
}

/// A register delete requested from the summary table, awaiting confirmation because it will
/// also remove the definition (see `confirm_delete_reg`'s doc comment).
pub struct PendingDeleteReg {
    pub rif_type: String,
    pub page_name: String,
    pub reg_type: String,
    pub inst_name: Option<String>,
}

/// A register-address commit that collided with another register on the page, awaiting the
/// user's Swap/Shift/Insert/Cancel choice. Everything needed to render the modal AND to act on
/// whichever button is clicked is precomputed here (by `resolve_reg_addr_commit`) so the modal
/// itself is pure rendering — no eligibility/cascade logic runs inside the `ui()` closure.
pub struct PendingRegAddrConflict {
    pub rif_type: String,
    /// The `RegDef` type name being edited (== `RegEditor::reg_type`, i.e. `UpdateRegDef`'s
    /// `orig_name`).
    pub orig_name: String,
    /// The instance being moved (the "mover").
    pub inst_name: String,
    /// Mover's requested new address.
    pub new_addr: u64,
    /// Colliding register's instance name, for the confirmation message.
    pub target_name: String,
    /// Every other bundled field change (name/group/desc/pulse/intr), with `addr` already
    /// `Some(new_addr)` — exactly what a plain `UpdateRegDef` would have carried.
    pub vals: RegDefVals,
    /// `Some([(target_name, old_addr)])` when the target is eligible (`is_reg_reorderable`);
    /// `None` when it isn't (e.g. array element or non-`Absolute` source address) — Swap is
    /// simply not offered in that case.
    pub swap: Option<Vec<(String, u64)>>,
    /// `Some(cascade)` only when `new_addr > old_addr` AND every register the cascade would
    /// touch is eligible; see `compute_shift_cascade`.
    pub shift: Option<Vec<(String, u64)>>,
    /// `Some(rotation)` only when `new_addr < old_addr` AND every register in the bounded range
    /// is eligible; see `compute_insert_rotation`.
    pub insert: Option<Vec<(String, u64)>>,
}

/// Which button the user picked in the address-conflict modal.
pub enum RegAddrResolution { Swap, Shift, Insert }

/// What a pending register-array change would apply on confirm — the definition-level dimension
/// change (bundled with the rest of `RegDefVals`) or an instance-level array-size expression.
/// Kept as separate variants rather than a shared shape, mirroring how `UpdateRegOverride`/
/// `UpdateField` are always distinct `EditAction` variants from their definition counterparts.
pub enum RegArrayChangeKind {
    Definition(Box<RegDefVals>),
    /// `None` clears the instance-level array back to "not an array".
    Instance(Option<ExprTokens>),
}

/// A register-array size change (definition- or instance-level) awaiting confirmation because it
/// needs to convert non-array fields to `[1]` and/or shift following registers on a manual page —
/// see `resolve_reg_commit`. Like `PendingRegAddrConflict`, everything needed to render the modal
/// and act on Confirm is precomputed here; the modal itself is pure rendering.
pub struct PendingRegArrayChange {
    pub rif_type: String,
    pub orig_name: String,
    /// The instance being resized (the "mover").
    pub inst_name: String,
    pub kind: RegArrayChangeKind,
    /// Fields that will be converted from `Width::Value(0)` to `Width::Value(1)` on confirm.
    /// Always empty for `RegArrayChangeKind::Instance` (instance-level arrays never touch fields).
    pub fields_to_convert: Vec<String>,
    /// Registers that will shift forward to make room for the grown footprint, if any. `None`
    /// means no shift was needed/computed (either the footprint didn't grow, or nothing occupies
    /// the newly-needed space) — a hard "not automatically resolvable" refusal is signaled via
    /// `RegEditor::parse_err` instead, before a `PendingRegArrayChange` is ever created.
    pub companions: Option<Vec<(String, u64)>>,
}

/// True when `name` is already used by either a register type or an instance on this page —
/// the two namespaces that matter for `RifPage::find_regdef`/`find_reg_inst` name resolution.
fn reg_name_taken(page: &RifPage, name: &str) -> bool {
    page.registers.iter().any(|r| r.get_regdef().is_some_and(|d| d.name == name))
        || page.instances.iter().any(|i| i.inst_name == name || i.type_name == name)
}

/// Generate a brand-new register type name not already present on the page.
fn unique_reg_name(page: &RifPage) -> String {
    if !reg_name_taken(page, "new_reg") {
        return "new_reg".to_owned();
    }
    (1..).map(|i| format!("new_reg_{i}")).find(|n| !reg_name_taken(page, n)).unwrap()
}

/// Live buffers for a register instance's whole-register override (`RegOverride`, the
/// `OptArrayIndex::None` entry) — the four properties `.rif` allows overriding per instance:
/// description, hardware access, optional condition, and optional access. Shown instead of
/// `RegEditor` when `EditTarget::Instance` is selected — see `show_reg_override_editor`. Mirrors
/// `RegIntrDescEditor`'s "small sibling editor" shape, but fallible (`optional` needs parsing).
pub struct RegOverrideEditor {
    pub rif_type: String,
    /// Instance name these buffers were loaded from (the reload key).
    pub inst_name: String,
    pub desc: String,
    pub desc_orig: String,
    pub hw_on: bool,
    pub hw_on_orig: bool,
    pub hw: Access,
    pub hw_orig: Access,
    pub optional: String,
    pub optional_orig: String,
    pub optional_acc_on: bool,
    pub optional_acc_on_orig: bool,
    pub optional_acc: Access,
    pub optional_acc_orig: Access,
    pub parse_err: Option<String>,
}

impl RegOverrideEditor {
    pub fn from_reg_inst(rif_type: &str, inst_name: &str, ovr: Option<&RegOverride>) -> Self {
        let desc = ovr.and_then(|o| o.description.as_ref()).map(|d| d.get(true)).unwrap_or_default();
        let hw_on = ovr.is_some_and(|o| o.hw_acc.is_some());
        let hw = ovr.and_then(|o| o.hw_acc).unwrap_or_default();
        // `to_rif()` on an empty `ExprTokens` (no override set) is the empty string.
        let optional = ovr.map(|o| o.optional.to_rif()).unwrap_or_default();
        let optional_acc_on = ovr.is_some_and(|o| o.optional_acc.is_some());
        let optional_acc = ovr.and_then(|o| o.optional_acc).unwrap_or_default();
        RegOverrideEditor {
            rif_type: rif_type.to_owned(),
            inst_name: inst_name.to_owned(),
            desc: desc.clone(), desc_orig: desc,
            hw_on, hw_on_orig: hw_on, hw, hw_orig: hw,
            optional: optional.clone(), optional_orig: optional,
            optional_acc_on, optional_acc_on_orig: optional_acc_on,
            optional_acc, optional_acc_orig: optional_acc,
            parse_err: None,
        }
    }

    /// Mirrors `RegEditor::is_unchanged` — see its doc comment for the erring-direction rule.
    pub fn is_unchanged(&self) -> bool {
        self.desc == self.desc_orig
            && self.hw_on == self.hw_on_orig && self.hw == self.hw_orig
            && self.optional == self.optional_orig
            && self.optional_acc_on == self.optional_acc_on_orig && self.optional_acc == self.optional_acc_orig
    }

    /// Parse+validate the buffers into the `EditAction` for "Apply" (auto-applied on blur, like
    /// every other editor here). `None` means a validation error (`self.parse_err` is set).
    pub fn build_action(&mut self) -> Option<EditAction> {
        self.parse_err = None;
        let optional = if self.optional.trim().is_empty() {
            None
        } else {
            match parse_expr(self.optional.trim()) {
                Ok(tokens) => Some(tokens),
                Err(e) => { self.parse_err = Some(format!("Invalid optional condition: {e}")); return None; }
            }
        };
        Some(EditAction::UpdateRegOverride {
            rif_type: self.rif_type.clone(),
            inst_name: self.inst_name.clone(),
            vals: RegOverrideVals {
                desc: self.desc.clone(),
                hw: self.hw_on.then_some(self.hw),
                optional,
                optional_acc: self.optional_acc_on.then_some(self.optional_acc),
            },
        })
    }
}

/// New values for a register instance's whole-register override, produced when the user edits
/// the `RegOverrideEditor` panel. `None` for `hw`/`optional_acc` means "no override" (the
/// checkbox is off); `optional: None` means "no condition" (the box is blank) — both are
/// distinguished from `ExprTokens::new(0)`'s "empty" only by the caller (`apply_pending`), which
/// clears the override field outright rather than storing a degenerate empty value.
pub struct RegOverrideVals {
    pub desc: String,
    pub hw: Option<Access>,
    pub optional: Option<ExprTokens>,
    pub optional_acc: Option<Access>,
}
