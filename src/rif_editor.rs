use yarig::parser::parser_expr::{parse_expr, ExprTokens};
use yarig::rifgen::{ClockingInfo, DataWidth, DeclLine, GenericRange, Interface, ResetDef, Rif};

use crate::apply_pending::EditAction;

/// Live buffers for the Rif-level property editor: name, address width, data width, and description
pub struct RifEditor {
    pub rif_type: String,
    pub name: String,
    pub name_orig: String,
    pub addr_width: String,
    pub addr_width_orig: String,
    pub data_width: DataWidth,
    pub data_width_orig: DataWidth,
    pub interface: Interface,
    pub interface_orig: Interface,
    pub desc: String,
    pub desc_orig: String,
    pub parse_err: Option<String>,
}

impl RifEditor {
    pub fn from_rif(rif_type: &str, rif: &Rif) -> Self {
        let name = rif.name.clone();
        let addr_width = rif.addr_width.to_string();
        let desc = rif.description.get(true);
        RifEditor {
            rif_type: rif_type.to_owned(),
            name: name.clone(), name_orig: name,
            addr_width: addr_width.clone(), addr_width_orig: addr_width,
            data_width: rif.data_width, data_width_orig: rif.data_width,
            interface: rif.interface.clone(), interface_orig: rif.interface.clone(),
            desc: desc.clone(), desc_orig: desc,
            parse_err: None,
        }
    }

    /// Mirrors `RegEditor::is_unchanged` — see its doc comment for the erring-direction rule.
    pub fn is_unchanged(&self) -> bool {
        self.name == self.name_orig
            && self.addr_width == self.addr_width_orig
            && self.data_width == self.data_width_orig
            && self.interface == self.interface_orig
            && self.desc == self.desc_orig
    }

    /// Parse+validate the buffers into the `EditAction` for "Apply"
    pub fn build_action(&mut self) -> Option<EditAction> {
        self.parse_err = None;
        if self.name.trim().is_empty() {
            self.parse_err = Some("Name is empty".to_owned());
            return None;
        }
        let addr_width = match self.addr_width.trim().parse::<u8>() {
            Ok(v) if v > 0 => v,
            _ => {
                self.parse_err = Some("Invalid address width".to_owned());
                return None;
            }
        };
        // Check "Custom" interface has an implementatin file definition
        if let Interface::Custom(name, path) = &self.interface
            && (name.trim().is_empty() || path.trim().is_empty())
        {
            self.parse_err = Some("Select a SystemVerilog file for the Custom interface".to_owned());
            return None;
        }
        Some(EditAction::UpdateRifDef {
            rif_type: self.rif_type.clone(),
            vals: RifDefVals {
                name: self.name.trim().to_owned(),
                addr_width,
                data_width: self.data_width,
                interface: self.interface.clone(),
                desc: self.desc.clone(),
            },
        })
    }
}

/// New values for a Rif definition, produced when the user edits `RifEditor`'s buffers.
pub struct RifDefVals {
    pub name: String,
    pub addr_width: u8,
    pub data_width: DataWidth,
    pub interface: Interface,
    pub desc: String,
}

/// One row of the clocking-table modal — one clock and its own reset, hardware or software.
#[derive(Clone)]
pub struct ClockingRow {
    pub is_hw: bool,
    pub clk: String,
    pub rst_name: String,
    pub active_high: bool,
    pub sync: bool,
    pub clk_en: String,
    pub clear: String,
}

impl ClockingRow {
    pub fn from_info(is_hw: bool, info: &ClockingInfo) -> Self {
        ClockingRow {
            is_hw,
            clk: info.clk.clone(),
            rst_name: info.rst.name.clone(),
            active_high: info.rst.active_high,
            sync: info.rst.sync,
            clk_en: info.en.clone(),
            clear: info.clear.clone(),
        }
    }

    /// A fresh row for the "add clock" affordance — software by default, an unremarkable
    /// `rst_n`/async/active-low reset (mirrors `ResetDef::default`).
    pub fn new_default(is_hw: bool) -> Self {
        ClockingRow {
            is_hw,
            clk: "clk".to_owned(),
            rst_name: "rst_n".to_owned(),
            active_high: false,
            sync: false,
            clk_en: String::new(),
            clear: String::new(),
        }
    }
}

/// Live buffers for the clocking-table modal.
pub struct RifClockingEditor {
    pub rif_type: String,
    pub rows: Vec<ClockingRow>,
    pub parse_err: Option<String>,
}

impl RifClockingEditor {
    pub fn open(rif_type: &str, rif: &Rif) -> Self {
        let mut rows: Vec<ClockingRow> = rif.sw_clocking.iter().map(|c| ClockingRow::from_info(false, c)).collect();
        rows.extend(rif.hw_clocking.iter().map(|c| ClockingRow::from_info(true, c)));
        if rows.is_empty() {
            rows.push(ClockingRow::new_default(false));
        }
        RifClockingEditor { rif_type: rif_type.to_owned(), rows, parse_err: None }
    }

    /// Parse and validate the row buffers into the two `ClockingInfo` vectors
    pub fn build_values(&self) -> Result<(Vec<ClockingInfo>, Vec<ClockingInfo>), String> {
        let mut sw = Vec::new();
        let mut hw = Vec::new();
        for row in &self.rows {
            let clk = row.clk.trim();
            if clk.is_empty() {
                return Err("Clock name cannot be empty".to_owned());
            }
            let rst_name = row.rst_name.trim();
            if rst_name.is_empty() {
                return Err(format!("{clk}: reset name cannot be empty"));
            }
            let info = ClockingInfo {
                clk: clk.to_owned(),
                rst: ResetDef { name: rst_name.to_owned(), sync: row.sync, active_high: row.active_high, src: DeclLine::default() },
                en: row.clk_en.trim().to_owned(),
                clear: row.clear.trim().to_owned(),
            };
            if row.is_hw { hw.push(info); } else { sw.push(info); }
        }
        for group in [&sw, &hw] {
            for i in 0..group.len() {
                for j in (i + 1)..group.len() {
                    if group[i].clk == group[j].clk {
                        return Err(format!("Duplicate clock name '{}'", group[i].clk));
                    }
                }
            }
        }
        Ok((sw, hw))
    }
}

/// One row of the parameters table (name + value expression)
#[derive(Clone)]
pub struct ParamRow {
    pub orig_name: Option<String>,
    pub name: String,
    pub value: String,
}

impl ParamRow {
    pub fn from_entry(name: &str, value: &ExprTokens) -> Self {
        ParamRow { orig_name: Some(name.to_owned()), name: name.to_owned(), value: value.to_rif() }
    }

    pub fn new_default() -> Self {
        ParamRow { orig_name: None, name: String::new(), value: "0".to_owned() }
    }
}

/// One row of the generics table (name + min/default/max + optional description).
#[derive(Clone)]
pub struct GenericRow {
    pub orig_name: Option<String>,
    pub name: String,
    pub min: String,
    pub default: String,
    pub max: String,
    pub desc: String,
}

impl GenericRow {
    pub fn from_entry(name: &str, range: &GenericRange) -> Self {
        GenericRow {
            orig_name: Some(name.to_owned()),
            name: name.to_owned(),
            min: range.min.to_string(),
            default: range.default.to_string(),
            max: range.max.to_string(),
            desc: range.desc.clone().unwrap_or_default(),
        }
    }

    pub fn new_default() -> Self {
        GenericRow { orig_name: None, name: String::new(), min: "0".to_owned(), default: "0".to_owned(), max: "1".to_owned(), desc: String::new() }
    }
}

/// A parsed, validated parameter
pub struct ParamEntry {
    pub orig_name: Option<String>,
    pub name: String,
    pub value: ExprTokens,
}

/// A parsed, validated generic
pub struct GenericEntry {
    pub orig_name: Option<String>,
    pub name: String,
    pub range: GenericRange,
}

/// Live buffers for the parameters/generics modal
pub struct RifParamsEditor {
    pub rif_type: String,
    pub param_rows: Vec<ParamRow>,
    pub generic_rows: Vec<GenericRow>,
    pub parse_err: Option<String>,
}

impl RifParamsEditor {
    pub fn open(rif_type: &str, rif: &Rif) -> Self {
        let param_rows: Vec<ParamRow> = rif.parameters.items().map(|(k, v)| ParamRow::from_entry(k, v)).collect();
        let generic_rows: Vec<GenericRow> = rif.generics.items().map(|(k, v)| GenericRow::from_entry(k, v)).collect();
        RifParamsEditor { rif_type: rif_type.to_owned(), param_rows, generic_rows, parse_err: None }
    }

    /// Parse and validate both tables into entries ready for
    /// `EditAction::UpdateRifParamsAndGenerics`.
    pub fn build_values(&self) -> Result<(Vec<ParamEntry>, Vec<GenericEntry>), String> {
        let mut params = Vec::with_capacity(self.param_rows.len());
        for row in &self.param_rows {
            let name = row.name.trim();
            if name.is_empty() {
                return Err("Parameter name cannot be empty".to_owned());
            }
            let value = parse_expr(row.value.trim()).map_err(|e| format!("{name}: {e}"))?;
            params.push(ParamEntry { orig_name: row.orig_name.clone(), name: name.to_owned(), value });
        }
        for i in 0..params.len() {
            for j in (i + 1)..params.len() {
                if params[i].name == params[j].name {
                    return Err(format!("Duplicate parameter name '{}'", params[i].name));
                }
            }
        }
        let mut generics = Vec::with_capacity(self.generic_rows.len());
        for row in &self.generic_rows {
            let name = row.name.trim();
            if name.is_empty() {
                return Err("Generic name cannot be empty".to_owned());
            }
            let min: u16 = row.min.trim().parse().map_err(|_| format!("{name}: invalid min"))?;
            let default: u16 = row.default.trim().parse().map_err(|_| format!("{name}: invalid default"))?;
            let max: u16 = row.max.trim().parse().map_err(|_| format!("{name}: invalid max"))?;
            let desc = (!row.desc.trim().is_empty()).then(|| row.desc.trim().to_owned());
            generics.push(GenericEntry { orig_name: row.orig_name.clone(), name: name.to_owned(), range: GenericRange { min, default, max, desc } });
        }
        for i in 0..generics.len() {
            for j in (i + 1)..generics.len() {
                if generics[i].name == generics[j].name {
                    return Err(format!("Duplicate generic name '{}'", generics[i].name));
                }
            }
        }
        Ok((params, generics))
    }
}
