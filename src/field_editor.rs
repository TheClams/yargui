use yarig::comp::comp_inst::RifFieldInst;
use yarig::parser::{counter_def, limit_def, password_info, signal_or_expr};
use yarig::rifgen::{
    Access, CounterInfo, EnumDef, EnumKind, Field, FieldHwKind, FieldOverride, FieldPos, FieldProp,
    FieldSwKind, InterruptClr, InterruptInfoField, InterruptTrigger, LimitP, LimitValueP, Lock,
    PasswordInfo, ResetVal, ResetValOverride, ResetValP, Visibility, VisibilityRaw, Width,
};

use crate::apply_pending::EditAction;

/// Live text buffers for the field currently being edited.
pub struct FieldEditor {
    /// (register type, original field name) identifying which field these buffers belong to
    pub key: (String, String),
    /// RIF and register-group type the field belongs to
    pub rif_type: String,
    pub group_type: String,
    // Declaration-line buffers
    pub name: String,
    pub lsb: String,
    pub width: String,
    /// Array size edit box; blank/"0" means "not an array"
    pub array: String,
    /// Bit-position increment between array elements; blank/"0" follows the field width.
    pub array_pos_incr: String,
    pub reset: String,
    pub desc: String,
    pub sw_kind: FieldSwKind,
    pub signed: bool,
    // Advanced (sub-property) buffers, loaded from the definition `Field`
    pub hw_acc: Access,
    pub visibility: VisMode,
    pub nb_frac: String,
    pub lock_on: bool,
    pub lock: String,
    pub limit_kind: LimitKind,
    pub limit_min: String,
    pub limit_max: String,
    pub limit_list: String,
    pub limit_bypass: String,
    pub counter_on: bool,
    pub counter_up: String,
    pub counter_down: String,
    pub counter_sat: bool,
    pub counter_event: bool,
    pub counter_clr: bool,
    pub password_on: bool,
    pub password_once: String,
    pub password_hold: String,
    pub password_protect: bool,
    pub enum_mode: EnumMode,
    pub enum_name: String,
    /// Whether `enum_name` is a new enum or a re-use of an enum defined in another field
    pub enum_type_src: EnumTypeSrc,
    /// The owning register's current primary-interrupt trigger/clear
    pub reg_intr_default: Option<InterruptInfoField>,
    pub intr_trigger_ovr_on: bool,
    pub intr_trigger_ovr: InterruptTrigger,
    pub intr_clear_ovr_on: bool,
    pub intr_clear_ovr: InterruptClr,
    /// Snapshot of the buffers as first loaded, to compute the minimal changed set on Apply
    orig: FieldOrig,
    pub parse_err: Option<String>,
}

/// Visibility choices offered in the editor.
/// `Disabled` carries an expression whose editing is deferred, so it is shown but not selectable
#[derive(Clone, Copy, PartialEq)]
pub enum VisMode { Full, Hidden, Reserved, Disabled }

impl VisMode {
    pub fn from_raw(v: &VisibilityRaw) -> Self {
        match v {
            VisibilityRaw::Full => VisMode::Full,
            VisibilityRaw::Hidden => VisMode::Hidden,
            VisibilityRaw::Reserved => VisMode::Reserved,
            VisibilityRaw::Disabled(_) => VisMode::Disabled,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            VisMode::Full => "Full",
            VisMode::Hidden => "Hidden",
            VisMode::Reserved => "Reserved",
            VisMode::Disabled => "Disabled (expr)",
        }
    }
}

/// Enum-kind choices.
#[derive(Clone, Copy, PartialEq)]
pub enum EnumMode { None, Doc, Type }

impl EnumMode {
    pub fn from_kind(k: &EnumKind) -> Self {
        match k {
            EnumKind::None => EnumMode::None,
            EnumKind::Doc(_) => EnumMode::Doc,
            EnumKind::Type(_) => EnumMode::Type,
        }
    }
    pub fn label(self) -> &'static str {
        match self { EnumMode::None => "None", EnumMode::Doc => "Doc", EnumMode::Type => "Type" }
    }
}

/// Whether `enum_name` is a new enum or a re-use of an enum defined in another field
#[derive(Clone, Copy, PartialEq)]
pub enum EnumTypeSrc { New, Existing }

/// Limit kind choices offered in the editor combo.
#[derive(Clone, Copy, PartialEq)]
pub enum LimitKind { None, Range, List, Enum, External }

impl LimitKind {
    pub fn label(self) -> &'static str {
        match self {
            LimitKind::None => "None",
            LimitKind::Range => "Range",
            LimitKind::List => "List",
            LimitKind::Enum => "Enum",
            LimitKind::External => "External",
        }
    }
}

/// Buffers snapshot used to detect exactly which properties the user changed.
struct FieldOrig {
    // Declaration-line baseline
    name: String,
    lsb: String,
    width: String,
    array: String,
    array_pos_incr: String,
    desc: String,
    reset: String,
    signed: bool,
    sw_kind: FieldSwKind,
    hw_acc: Access,
    visibility: VisMode,
    nb_frac: String,
    lock_on: bool,
    lock: String,
    limit_kind: LimitKind,
    limit_min: String,
    limit_max: String,
    limit_list: String,
    limit_bypass: String,
    counter_on: bool,
    counter_up: String,
    counter_down: String,
    counter_sat: bool,
    counter_event: bool,
    counter_clr: bool,
    password_on: bool,
    password_once: String,
    password_hold: String,
    password_protect: bool,
    enum_mode: EnumMode,
    enum_name: String,
    intr_trigger_ovr_on: bool,
    intr_trigger_ovr: InterruptTrigger,
    intr_clear_ovr_on: bool,
    intr_clear_ovr: InterruptClr,
}

impl FieldEditor {
    /// Load the editor buffers
    pub fn from_field(
        key: (String, String), f: &RifFieldInst, def: &Field, enum_defs: &[EnumDef],
        rif_type: &str, group_type: &str, reg_intr_default: Option<InterruptInfoField>,
    ) -> Self {
        let (reset, signed) = match &f.reset {
            ResetVal::Signed(v) => (format!("{v}"), true),
            ResetVal::Unsigned(v) => (format!("{v}"), false),
        };
        let hw_acc = def.hw_acc;
        let visibility = VisMode::from_raw(&def.visibility);
        let nb_frac = if def.nb_frac != 0 { def.nb_frac.to_string() } else { String::new() };
        let lock_on = def.lock.is_some();
        let lock = def.lock.expr().as_ref().and_then(|e| e.to_rif()).unwrap_or_default();
        let (limit_kind, limit_min, limit_max, limit_list) = match &def.limit.value {
            LimitValueP::None => (LimitKind::None, String::new(), String::new(), String::new()),
            LimitValueP::Min(v) => (LimitKind::Range, v.to_rif(), String::new(), String::new()),
            LimitValueP::Max(v) => (LimitKind::Range, String::new(), v.to_rif(), String::new()),
            LimitValueP::MinMax(a, b) => (LimitKind::Range, a.to_rif(), b.to_rif(), String::new()),
            LimitValueP::List(vs) => (LimitKind::List, String::new(), String::new(),
                vs.iter().map(|v| v.to_rif()).collect::<Vec<_>>().join(",")),
            LimitValueP::Enum => (LimitKind::Enum, String::new(), String::new(), String::new()),
            LimitValueP::External => (LimitKind::External, String::new(), String::new(), String::new()),
        };
        let limit_bypass = def.limit.bypass.clone();
        let counter_info = def.hw_kind.iter().find_map(|k| match k { FieldHwKind::Counter(c) => Some(c), _ => None });
        let counter_on = counter_info.is_some();
        let (counter_up, counter_down, counter_sat, counter_event, counter_clr) = match counter_info {
            Some(c) => (
                if c.is_up() { c.incr_val.to_string() } else { String::new() },
                if c.is_down() { c.decr_val.to_string() } else { String::new() },
                c.sat, c.event, c.clr,
            ),
            None => (String::new(), String::new(), false, false, false),
        };
        let (password_on, password_once, password_hold, password_protect) = match &def.sw_kind {
            FieldSwKind::Password(info) => (
                true,
                info.once.as_ref().map(|v| v.to_rif()).unwrap_or_default(),
                info.hold.as_ref().map(|v| v.to_rif()).unwrap_or_default(),
                info.protect,
            ),
            _ => (false, String::new(), String::new(), false),
        };
        let enum_mode = EnumMode::from_kind(&def.enum_kind);
        let enum_name = match &def.enum_kind { EnumKind::Type(n) => n.clone(), _ => String::new() };
        let enum_type_src = if enum_defs.iter().any(|d| d.name == enum_name && d.is_local_type()) {
            EnumTypeSrc::Existing
        } else {
            EnumTypeSrc::New
        };
        // The sw-kind combo holds the "basic" kind; a password field parks the combo on RW so
        // that un-checking Password reverts to a sensible kind.
        let sw_kind = if password_on { FieldSwKind::ReadWrite } else { f.sw_kind.clone() };
        let name = f.name.clone();
        let lsb = f.lsb.to_string();
        let width = f.width().to_string();
        let array = match def.array {
            Width::Value(0) => String::new(),
            Width::Value(n) => n.to_string(),
            // Param-sized arrays never reach this editor (`is_field_editable` excludes them).
            Width::Param(_) => String::new(),
        };
        let array_pos_incr = if def.array_pos_incr != 0 { def.array_pos_incr.to_string() } else { String::new() };
        let desc = f.description.get(true);
        // Display value falls back to the register's own default when there's no override, so
        // the (disabled) combo shows the value that's actually effective for this field.
        let intr_trigger_ovr_on = def.intr_ovr.trigger.is_some();
        let intr_trigger_ovr = def.intr_ovr.trigger
            .or_else(|| reg_intr_default.as_ref().and_then(|d| d.trigger))
            .unwrap_or_default();
        let intr_clear_ovr_on = def.intr_ovr.clear.is_some();
        let intr_clear_ovr = def.intr_ovr.clear
            .or_else(|| reg_intr_default.as_ref().and_then(|d| d.clear))
            .unwrap_or_default();
        let orig = FieldOrig {
            name: name.clone(), lsb: lsb.clone(), width: width.clone(),
            array: array.clone(), array_pos_incr: array_pos_incr.clone(), desc: desc.clone(),
            reset: reset.clone(), signed, sw_kind: def.sw_kind.clone(), hw_acc, visibility, nb_frac: nb_frac.clone(),
            lock_on, lock: lock.clone(),
            limit_kind, limit_min: limit_min.clone(), limit_max: limit_max.clone(),
            limit_list: limit_list.clone(), limit_bypass: limit_bypass.clone(),
            counter_on, counter_up: counter_up.clone(), counter_down: counter_down.clone(),
            counter_sat, counter_event, counter_clr,
            password_on, password_once: password_once.clone(), password_hold: password_hold.clone(),
            password_protect,
            enum_mode, enum_name: enum_name.clone(),
            intr_trigger_ovr_on, intr_trigger_ovr, intr_clear_ovr_on, intr_clear_ovr,
        };
        FieldEditor {
            key,
            rif_type: rif_type.to_owned(),
            group_type: group_type.to_owned(),
            name,
            lsb,
            width,
            array,
            array_pos_incr,
            reset,
            desc,
            sw_kind,
            signed,
            hw_acc, visibility, nb_frac, lock_on, lock,
            limit_kind, limit_min, limit_max, limit_list, limit_bypass,
            counter_on, counter_up, counter_down, counter_sat, counter_event, counter_clr,
            password_on, password_once, password_hold, password_protect,
            enum_mode, enum_name, enum_type_src,
            reg_intr_default,
            intr_trigger_ovr_on, intr_trigger_ovr, intr_clear_ovr_on, intr_clear_ovr,
            orig,
            parse_err: None,
        }
    }

    /// Whether every buffer still matches the baseline loaded in `from_field`
    pub fn is_unchanged(&self) -> bool {
        self.name == self.orig.name
            && self.lsb == self.orig.lsb
            && self.width == self.orig.width
            && self.array == self.orig.array
            && self.array_pos_incr == self.orig.array_pos_incr
            && self.desc == self.orig.desc
            && self.reset == self.orig.reset
            && self.signed == self.orig.signed
            && self.sw_kind == self.orig.sw_kind
            && self.hw_acc == self.orig.hw_acc
            && self.visibility == self.orig.visibility
            && self.nb_frac == self.orig.nb_frac
            && self.lock_on == self.orig.lock_on
            && self.lock == self.orig.lock
            && self.limit_kind == self.orig.limit_kind
            && self.limit_min == self.orig.limit_min
            && self.limit_max == self.orig.limit_max
            && self.limit_list == self.orig.limit_list
            && self.limit_bypass == self.orig.limit_bypass
            && self.counter_on == self.orig.counter_on
            && self.counter_up == self.orig.counter_up
            && self.counter_down == self.orig.counter_down
            && self.counter_sat == self.orig.counter_sat
            && self.counter_event == self.orig.counter_event
            && self.counter_clr == self.orig.counter_clr
            && self.password_on == self.orig.password_on
            && self.password_once == self.orig.password_once
            && self.password_hold == self.orig.password_hold
            && self.password_protect == self.orig.password_protect
            && self.enum_mode == self.orig.enum_mode
            && self.enum_name == self.orig.enum_name
            && self.intr_trigger_ovr_on == self.orig.intr_trigger_ovr_on
            && self.intr_trigger_ovr == self.orig.intr_trigger_ovr
            && self.intr_clear_ovr_on == self.orig.intr_clear_ovr_on
            && self.intr_clear_ovr == self.orig.intr_clear_ovr
    }

    /// Compose the `.rif` limit spec text (without the `limit` keyword)
    fn limit_string(&self) -> String {
        let spec = match self.limit_kind {
            LimitKind::None => return String::new(),
            LimitKind::Range => format!("[{}:{}]", self.limit_min.trim(), self.limit_max.trim()),
            LimitKind::List => format!("{{{}}}", self.limit_list.trim()),
            LimitKind::Enum => "enum".to_owned(),
            LimitKind::External => "external".to_owned(),
        };
        if self.limit_bypass.trim().is_empty() { spec } else { format!("{spec} {}", self.limit_bypass.trim()) }
    }

    /// Compose the `.rif` counter spec text from the up/down value buffers + checkboxes.
    fn counter_string(&self) -> String {
        let up = self.counter_up.trim();
        let down = self.counter_down.trim();
        if up.is_empty() && down.is_empty() {
            return String::new();
        }
        let mut s = match (!up.is_empty(), !down.is_empty()) {
            (true, true) => "updown",
            (true, false) => "up",
            (false, true) => "down",
            (false, false) => unreachable!(),
        }.to_owned();
        // "0" means the direction is enabled with the default step (matches `CounterInfo::to_rif`,
        // which only emits `incrVal`/`decrVal` when non-zero).
        if !up.is_empty() && up != "0" { s.push_str(&format!(" incrVal={up}")); }
        if !down.is_empty() && down != "0" { s.push_str(&format!(" decrVal={down}")); }
        if self.counter_sat { s.push_str(" sat"); }
        if self.counter_event { s.push_str(" event"); }
        if self.counter_clr { s.push_str(" clr"); }
        s
    }

    /// Compose the `.rif` password spec text from the once/hold value buffers + protect checkbox.
    fn password_string(&self) -> String {
        let mut parts = Vec::new();
        if !self.password_once.trim().is_empty() { parts.push(format!("once={}", self.password_once.trim())); }
        if !self.password_hold.trim().is_empty() { parts.push(format!("hold={}", self.password_hold.trim())); }
        if self.password_protect { parts.push("protect".to_owned()); }
        parts.join(" ")
    }

    /// Parse and validate every buffer
    /// `None` means either a validation error
    pub fn build_action(&mut self) -> Option<EditAction> {
        self.parse_err = None;
        let lsb = match self.lsb.trim().parse::<u8>() {
            Ok(v) => v,
            Err(_) => { self.parse_err = Some("Invalid lsb".to_owned()); return None; }
        };
        let width = match self.width.trim().parse::<u8>() {
            Ok(v) if v >= 1 => v,
            _ => { self.parse_err = Some("Width must be >= 1".to_owned()); return None; }
        };
        if self.name.trim().is_empty() {
            self.parse_err = Some("Name is empty".to_owned());
            return None;
        }
        let array = if self.array.trim().is_empty() { 0 } else {
            match self.array.trim().parse::<u8>() {
                Ok(v) => v,
                Err(_) => { self.parse_err = Some("Invalid array size".to_owned()); return None; }
            }
        };
        let array_pos_incr = if self.array_pos_incr.trim().is_empty() { 0 } else {
            match self.array_pos_incr.trim().parse::<u8>() {
                Ok(v) => v,
                Err(_) => { self.parse_err = Some("Invalid array position increment".to_owned()); return None; }
            }
        };
        // Reset: only re-parse (and thus overwrite the definition) when the value or its
        // signedness actually changed, so enum-label / parameter resets are preserved otherwise.
        let reset = if self.reset != self.orig.reset || self.signed != self.orig.signed {
            match parse_reset(&self.reset, self.signed) {
                Ok(v) => Some(v),
                Err(e) => { self.parse_err = Some(e); return None; }
            }
        } else {
            None
        };
        // nb_frac
        let nb_frac = if self.nb_frac.trim().is_empty() { 0 } else {
            match self.nb_frac.trim().parse::<isize>() {
                Ok(v) => v,
                Err(_) => { self.parse_err = Some("Invalid fractional bits".to_owned()); return None; }
            }
        };
        // lock
        let lock = if !self.lock_on || self.lock.trim().is_empty() { Lock::default() } else {
            match signal_or_expr(self.lock.trim()) {
                Ok(e) => {
                    if e.to_rif().is_none() {
                        self.parse_err = Some("Lock expression too complex to persist yet".to_owned());
                        return None;
                    }
                    Lock::new(e)
                }
                Err(e) => { self.parse_err = Some(format!("Invalid lock: {e}")); return None; }
            }
        };
        // limit
        let limit_str = self.limit_string();
        let limit = if limit_str.is_empty() { LimitP::default() } else {
            match limit_def(&limit_str) {
                Ok(l) => l,
                Err(e) => { self.parse_err = Some(format!("Invalid limit: {e}")); return None; }
            }
        };
        // counter
        let counter_str = self.counter_string();
        let counter = if !self.counter_on || counter_str.is_empty() { None } else {
            match counter_def(&counter_str) {
                Ok(c) => Some(c),
                Err(e) => { self.parse_err = Some(format!("Invalid counter: {e}")); return None; }
            }
        };
        // password
        let password = if !self.password_on { None } else {
            let password_str = self.password_string();
            if password_str.is_empty() {
                Some(PasswordInfo { once: None, hold: None, protect: false })
            } else {
                match password_info(&password_str) {
                    Ok(p) => Some(p),
                    Err(e) => { self.parse_err = Some(format!("Invalid password: {e}")); return None; }
                }
            }
        };
        // enum kind (built against the possibly-renamed field name)
        let new_name = self.name.trim().to_owned();
        let enum_kind = match self.enum_mode {
            EnumMode::None => EnumKind::None,
            EnumMode::Doc => EnumKind::new("", &self.group_type, &new_name),
            EnumMode::Type => if self.enum_name.trim().is_empty() {
                EnumKind::new("type", &self.group_type, &new_name)
            } else {
                EnumKind::new(self.enum_name.trim(), &self.group_type, &new_name)
            },
        };
        // visibility: None keeps a preserved `disabled` expression untouched
        let visibility = match self.visibility {
            VisMode::Full => Some(VisibilityRaw::Full),
            VisMode::Hidden => Some(VisibilityRaw::Hidden),
            VisMode::Reserved => Some(VisibilityRaw::Reserved),
            VisMode::Disabled => None,
        };
        // Which advanced properties did the user actually change?
        let mut changed = Vec::new();
        if self.signed != self.orig.signed { changed.push(FieldProp::Signed); }
        if self.hw_acc != self.orig.hw_acc { changed.push(FieldProp::HwAcc); }
        if self.nb_frac.trim() != self.orig.nb_frac { changed.push(FieldProp::NbFrac); }
        if self.array_pos_incr.trim() != self.orig.array_pos_incr { changed.push(FieldProp::ArrayPosIncr); }
        if self.visibility != self.orig.visibility { changed.push(FieldProp::Visibility); }
        if self.enum_mode != self.orig.enum_mode || self.enum_name.trim() != self.orig.enum_name {
            changed.push(FieldProp::EnumKind);
        }
        if self.lock_on != self.orig.lock_on || self.lock.trim() != self.orig.lock { changed.push(FieldProp::Lock); }
        // A pulse field may carry a `pulse [comb|reg]` sub-property line rather than an inline decl-line token        if self.sw_kind.is_pulse() || self.orig.sw_kind.is_pulse() { changed.push(FieldProp::Pulse); }
        let limit_changed = self.limit_kind != self.orig.limit_kind
            || self.limit_min.trim() != self.orig.limit_min
            || self.limit_max.trim() != self.orig.limit_max
            || self.limit_list.trim() != self.orig.limit_list
            || self.limit_bypass.trim() != self.orig.limit_bypass;
        if limit_changed { changed.push(FieldProp::Limit); }
        let counter_changed = self.counter_on != self.orig.counter_on
            || self.counter_up.trim() != self.orig.counter_up
            || self.counter_down.trim() != self.orig.counter_down
            || self.counter_sat != self.orig.counter_sat
            || self.counter_event != self.orig.counter_event
            || self.counter_clr != self.orig.counter_clr;
        if counter_changed { changed.push(FieldProp::Counter); }
        let password_changed = self.password_on != self.orig.password_on
            || self.password_once.trim() != self.orig.password_once
            || self.password_hold.trim() != self.orig.password_hold
            || self.password_protect != self.orig.password_protect;
        if password_changed { changed.push(FieldProp::Password); }
        let intr_ovr = self.reg_intr_default.clone().map(|_| {
            if self.intr_trigger_ovr_on != self.orig.intr_trigger_ovr_on || self.intr_trigger_ovr != self.orig.intr_trigger_ovr
                || self.intr_clear_ovr_on != self.orig.intr_clear_ovr_on || self.intr_clear_ovr != self.orig.intr_clear_ovr
            {
                changed.push(FieldProp::Interrupt);
            }
            InterruptInfoField {
                trigger: self.intr_trigger_ovr_on.then_some(self.intr_trigger_ovr),
                clear: self.intr_clear_ovr_on.then_some(self.intr_clear_ovr),
            }
        });

        let pos = FieldPos::MsbLsb((Width::Value(lsb + width - 1), Width::Value(lsb)));
        Some(EditAction::UpdateField {
            rif_type: self.rif_type.clone(),
            reg_type: self.key.0.clone(),
            orig_name: self.key.1.clone(),
            vals: Box::new(FieldVals {
                name: new_name,
                pos,
                array: Width::Value(array),
                array_pos_incr,
                reset,
                signed: self.signed,
                sw_kind: self.sw_kind.clone(),
                desc: self.desc.clone(),
                hw_acc: self.hw_acc,
                visibility,
                nb_frac,
                lock,
                limit,
                counter,
                password,
                enum_kind,
                intr_ovr,
                reg_intr_default: self.reg_intr_default.clone().unwrap_or_default(),
                changed,
            }),
        })
    }
}

/// New values for a field, produced when the user hits "Apply".
pub struct FieldVals {
    // Declaration line
    pub name: String,
    pub pos: FieldPos,
    /// New array size (`Width::Value(0)` means "not an array").
    pub array: Width,
    /// New `arrayPosIncr` (0 = default, no line emitted).
    pub array_pos_incr: u8,
    /// `None` leaves the definition reset untouched (so enum-label / param resets are preserved
    /// when the user did not change the reset).
    pub reset: Option<ResetValP>,
    pub signed: bool,
    pub sw_kind: FieldSwKind,
    pub desc: String,
    // Advanced
    pub hw_acc: Access,
    /// `None` leaves visibility as-is (e.g. a preserved `disabled` expression).
    pub visibility: Option<VisibilityRaw>,
    pub nb_frac: isize,
    pub lock: Lock,
    pub limit: LimitP,
    pub counter: Option<CounterInfo>,
    pub password: Option<PasswordInfo>,
    pub enum_kind: EnumKind,
    /// New trigger/clear override
    pub intr_ovr: Option<InterruptInfoField>,
    pub reg_intr_default: InterruptInfoField,
    /// Advanced properties the user actually changed (drives per-line save reconciliation).
    pub changed: Vec<FieldProp>,
}

/// Live buffers for one field's whole-field override
pub struct FieldOverrideEditor {
    pub rif_type: String,
    pub inst_name: String,
    pub field_name: String,
    pub desc: String,
    pub desc_orig: String,
    /// Read-only guard: an override description that already carries a multi-line block
    pub desc_locked: bool,
    pub disable_on: bool,
    pub disable_on_orig: bool,
    pub reset_val: String,
    pub reset_val_orig: String,
    /// The field's ownreset value
    pub field_reset: ResetValP,
    pub parse_err: Option<String>,
}

impl FieldOverrideEditor {
    pub fn from_field_override(rif_type: &str, inst_name: &str, field_name: &str, ovr: Option<&FieldOverride>, field_reset: ResetValP) -> Self {
        let disable_on = ovr.is_some_and(|o| o.visibility == Some(Visibility::Disabled));
        let reset_val = match ovr.map(|o| &o.reset) {
            Some(ResetValOverride::Reset(v)) => v.to_rif(),
            _ => String::new(),
        };
        let desc_locked = ovr.and_then(|o| o.description.as_ref()).is_some_and(|d| d.get_split(true).1.is_some());
        let desc = ovr.and_then(|o| o.description.as_ref()).map(|d| d.get_short(true)).unwrap_or_default();
        FieldOverrideEditor {
            rif_type: rif_type.to_owned(),
            inst_name: inst_name.to_owned(),
            field_name: field_name.to_owned(),
            desc: desc.clone(), desc_orig: desc, desc_locked,
            disable_on, disable_on_orig: disable_on,
            reset_val: reset_val.clone(), reset_val_orig: reset_val,
            field_reset,
            parse_err: None,
        }
    }

    /// Mirrors `FieldEditor::is_unchanged` — see its doc comment for the erring-direction rule.
    pub fn is_unchanged(&self) -> bool {
        self.desc == self.desc_orig && self.disable_on == self.disable_on_orig && self.reset_val == self.reset_val_orig
    }

    /// Parse+validate the buffers into the `EditAction` for "Apply".
    pub fn build_action(&mut self) -> Option<EditAction> {
        self.parse_err = None;
        let reset = if self.reset_val.trim().is_empty() {
            None
        } else {
            match parse_reset(&self.reset_val, matches!(self.field_reset, ResetValP::Signed(_))) {
                Ok(v) => Some(v),
                Err(e) => { self.parse_err = Some(e); return None; }
            }
        };
        Some(EditAction::UpdateFieldOverride {
            rif_type: self.rif_type.clone(),
            inst_name: self.inst_name.clone(),
            field_name: self.field_name.clone(),
            vals: FieldOverrideVals {
                desc: self.desc.clone(),
                disabled: self.disable_on,
                reset,
                field_reset: self.field_reset.clone(),
            },
        })
    }
}

/// New values for a field's whole-field override
pub struct FieldOverrideVals {
    pub desc: String,
    pub disabled: bool,
    pub reset: Option<ResetValP>,
    pub field_reset: ResetValP,
}

/// Parse an integer allowing a `0x` hex prefix. Signedness follows the field.
pub fn parse_reset(s: &str, signed: bool) -> Result<ResetValP, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Reset value is empty".to_owned());
    }
    if signed {
        let v = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i128::from_str_radix(hex, 16)
        } else {
            s.parse::<i128>()
        }
        .map_err(|_| format!("Invalid signed reset '{s}'"))?;
        Ok(ResetValP::Signed(v))
    } else {
        let v = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u128::from_str_radix(hex, 16)
        } else {
            s.parse::<u128>()
        }
        .map_err(|_| format!("Invalid unsigned reset '{s}'"))?;
        Ok(ResetValP::Unsigned(v))
    }
}
