use yarig::rifgen::{DeclLine, EnumDef, EnumEntry};

/// One row of the enum entry-table modal. `src_line` is preserved from the parsed entry
#[derive(Clone, Default)]
pub struct EnumEntryRow {
    pub src_line: Option<usize>,
    pub name: String,
    pub value: String,
    pub repr: String,
    pub desc: String,
}

impl EnumEntryRow {
    pub fn from_entry(e: &EnumEntry) -> Self {
        EnumEntryRow {
            src_line: e.src.decl_line,
            name: e.name.clone(),
            value: e.value.to_string(),
            repr: e.repr.map(|r| r.to_string()).unwrap_or_default(),
            desc: e.description.get_short(false),
        }
    }

    /// A fresh row for the "add entry"
    pub fn next_default(existing: &[EnumEntryRow]) -> Self {
        let value = existing.iter()
            .filter_map(|r| parse_u8(&r.value).ok())
            .max()
            .map_or(0, |v| v.saturating_add(1));
        EnumEntryRow { value: value.to_string(), ..Default::default() }
    }
}

/// Live buffers for the enum entry-table modal.
pub struct EnumEditor {
    pub rif_type: String,
    /// Resolved enum definition name being edited
    pub name: String,
    /// `name` as it was when the modal was opened, to detect a rename on Save (collision check)
    pub orig_name: String,
    pub rows: Vec<EnumEntryRow>,
    /// Whether to show the optional fractional-representation column (field has nb_frac > 0)
    pub has_frac: bool,
    /// Current field width, to flag entries that don't fit
    pub width: u8,
    pub parse_err: Option<String>,
}

impl EnumEditor {
    pub fn open(rif_type: &str, name: String, enum_defs: &[EnumDef], has_frac: bool, width: u8) -> Self {
        // A brand new enum starts with one entry already prefilled (value 0) so Save doesn't
        // require clicking "add entry" first.
        let rows = enum_defs.iter()
            .find(|d| d.name == name)
            .map(|d| d.values.iter().map(EnumEntryRow::from_entry).collect())
            .unwrap_or_else(|| vec![EnumEntryRow::next_default(&[])]);
        EnumEditor { rif_type: rif_type.to_owned(), orig_name: name.clone(), name, rows, has_frac, width, parse_err: None }
    }

    /// Parse and validate the row buffers into `EnumEntry` values, ready for `EditAction::UpdateEnum`.
    pub fn build_values(&self) -> Result<Vec<EnumEntry>, String> {
        let mut out = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let name = row.name.trim();
            if name.is_empty() {
                return Err("Entry name cannot be empty".to_owned());
            }
            let value = parse_u8(&row.value).map_err(|e| format!("{name}: {e}"))?;
            if (value as u16) >= (1u16 << self.width) {
                return Err(format!("{name}: value {value} does not fit the field width ({} bits)", self.width));
            }
            let repr = if row.repr.trim().is_empty() {
                None
            } else {
                match row.repr.trim().parse::<f64>() {
                    Ok(v) => Some(v),
                    Err(_) => return Err(format!("{name}: invalid representation '{}'", row.repr)),
                }
            };
            out.push(EnumEntry {
                name: name.to_owned(),
                value,
                repr,
                description: row.desc.trim().into(),
                src: DeclLine { decl_line: row.src_line },
            });
        }
        for i in 0..out.len() {
            for j in (i + 1)..out.len() {
                if out[i].name == out[j].name {
                    return Err(format!("Duplicate entry name '{}'", out[i].name));
                }
                if out[i].value == out[j].value {
                    return Err(format!("Duplicate value {} ('{}' and '{}')", out[i].value, out[i].name, out[j].name));
                }
            }
        }
        Ok(out)
    }
}

/// Parse an integer allowing a `0x` hex prefix (mirrors `parse_reset`, unsigned byte range).
pub fn parse_u8(s: &str) -> Result<u8, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).map_err(|_| format!("Invalid value '{s}'"))
    } else {
        s.parse::<u8>().map_err(|_| format!("Invalid value '{s}'"))
    }
}
