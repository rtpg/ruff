use std::str::FromStr;

use ruff_macros::{define_violation, derive_message_formats};
use rustc_hash::FxHashMap;
use rustpython_common::cformat::{CFormatPart, CFormatSpec, CFormatStrOrBytes, CFormatString};
use rustpython_parser::ast::{Constant, Expr, ExprKind, Location};
use rustpython_parser::lexer;
use rustpython_parser::lexer::Tok;

use crate::ast::types::Range;
use crate::checkers::ast::Checker;
use crate::registry::Diagnostic;
use crate::rules::pydocstyle::helpers::{leading_quote, trailing_quote};
use crate::violation::Violation;

define_violation!(
    /// ### What it does
    /// Checks for mismatched argument types in "old-style" format strings.
    ///
    /// ### Why is this bad?
    /// The format string is not checked at compile time, so it is easy to
    /// introduce bugs by mistyping the format string.
    ///
    /// ### Example
    /// ```python
    /// print("%d" % "1")
    /// ```
    ///
    /// Use instead:
    /// ```python
    /// print("%d" % 1)
    /// ```
    pub struct BadStringFormatType;
);
impl Violation for BadStringFormatType {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("Format type does not match argument type")
    }
}

#[derive(Debug)]
enum DataType {
    String,
    Integer,
    Float,
    Number,
    Other,
}

impl DataType {
    fn is_compatible_with(&self, other: &Self) -> bool {
        match self {
            DataType::String => matches!(other, DataType::String),
            DataType::Integer => matches!(other, DataType::Integer | DataType::Number),
            DataType::Float => matches!(other, DataType::Float | DataType::Number),
            DataType::Number => matches!(
                other,
                DataType::Number | DataType::Integer | DataType::Float
            ),
            DataType::Other => false,
        }
    }
}

impl From<&Constant> for DataType {
    fn from(value: &Constant) -> Self {
        match value {
            Constant::Str(_) => DataType::String,
            // All float codes also work for integers.
            Constant::Int(_) => DataType::Number,
            Constant::Float(_) => DataType::Float,
            _ => DataType::Other,
        }
    }
}

impl From<char> for DataType {
    fn from(format: char) -> Self {
        match format {
            's' => DataType::String,
            // The python documentation says "d" only works for integers, but it works for floats as
            // well: https://docs.python.org/3/library/string.html#formatstrings
            // I checked the rest of the integer codes, and none of them work with floats
            'n' | 'd' => DataType::Number,
            'b' | 'c' | 'o' | 'x' | 'X' => DataType::Integer,
            'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%' => DataType::Float,
            _ => DataType::Other,
        }
    }
}

fn collect_specs(formats: &[CFormatStrOrBytes<String>]) -> Vec<&CFormatSpec> {
    let mut specs = vec![];
    for format in formats {
        for (_, item) in format.iter() {
            if let CFormatPart::Spec(spec) = item {
                specs.push(spec);
            }
        }
    }
    specs
}

/// Return `true` if the format string is equivalent to the constant type
fn equivalent(format: &CFormatSpec, value: &Constant) -> bool {
    let constant: DataType = value.into();
    let format: DataType = format.format_char.into();
    if matches!(format, DataType::String) {
        // We can always format as type `String`.
        return true;
    }

    if let DataType::Other = constant {
        // If the format is not string, we cannot format as type `Other`.
        false
    } else {
        constant.is_compatible_with(&format)
    }
}

/// Return `true` if the [`Constnat`] aligns with the format type.
fn is_valid_constant(formats: &[CFormatStrOrBytes<String>], value: &Constant) -> bool {
    let formats = collect_specs(formats);
    // If there is more than one format, this is not valid python and we should
    // return true so that no error is reported
    if formats.len() != 1 {
        return true;
    }
    let format = formats.get(0).unwrap();
    equivalent(format, value)
}

/// Return `true` if the tuple elements align with the format types.
fn is_valid_tuple(formats: &[CFormatStrOrBytes<String>], elts: &[Expr]) -> bool {
    let formats = collect_specs(formats);

    // If there are more formats that values, the statement is invalid. Avoid
    // checking the values.
    if formats.len() > elts.len() {
        return true;
    }

    for (format, elt) in formats.iter().zip(elts) {
        if let ExprKind::Constant { value, .. } = &elt.node {
            if !equivalent(format, value) {
                return false;
            }
        } else if let ExprKind::Name { .. } = &elt.node {
            continue;
        } else if format.format_char != 's' {
            // Non-`ExprKind::Constant` values can only be formatted as strings.
            return false;
        }
    }
    true
}

/// Return `true` if the dictionary values align with the format types.
fn is_valid_dict(
    formats: &[CFormatStrOrBytes<String>],
    keys: &[Option<Expr>],
    values: &[Expr],
) -> bool {
    let formats = collect_specs(formats);

    // If there are more formats that values, the statement is invalid. Avoid
    // checking the values.
    if formats.len() > values.len() {
        return true;
    }

    let formats_hash: FxHashMap<&str, &&CFormatSpec> = formats
        .iter()
        .filter_map(|format| {
            format
                .mapping_key
                .as_ref()
                .map(|mapping_key| (mapping_key.as_str(), format))
        })
        .collect();
    for (key, value) in keys.iter().zip(values) {
        let Some(key) = key else {
            return true;
        };
        if let ExprKind::Constant {
            value: Constant::Str(mapping_key),
            ..
        } = &key.node
        {
            let Some(format) = formats_hash.get(mapping_key.as_str()) else {
                return true;
            };
            if let ExprKind::Constant { value, .. } = &value.node {
                if !equivalent(format, value) {
                    return false;
                }
            } else if let ExprKind::Name { .. } = &value.node {
                continue;
            } else if format.format_char != 's' {
                // Non-`ExprKind::Constant` values can only be formatted as strings.
                return false;
            }
        } else {
            // We can't check non-string keys.
            return true;
        }
    }
    true
}

/// Return `true` if the format string is valid for "other" types.
fn is_valid_other(formats: &[CFormatStrOrBytes<String>]) -> bool {
    let formats = collect_specs(formats);

    // If there's more than one format, abort.
    if formats.len() != 1 {
        return true;
    }

    formats.get(0).unwrap().format_char == 's'
}

/// PLE1307
pub fn bad_string_format_type(checker: &mut Checker, expr: &Expr, right: &Expr) {
    // Grab each string segment (in case there's an implicit concatenation).
    let content = checker
        .locator
        .slice_source_code_range(&Range::from_located(expr));
    let mut strings: Vec<(Location, Location)> = vec![];
    for (start, tok, end) in lexer::make_tokenizer_located(content, expr.location).flatten() {
        if matches!(tok, Tok::String { .. }) {
            strings.push((start, end));
        } else if matches!(tok, Tok::Percent) {
            // Break as soon as we find the modulo symbol.
            break;
        }
    }

    // If there are no string segments, abort.
    if strings.is_empty() {
        return;
    }

    // Parse each string segment.
    let mut format_strings = vec![];
    for (start, end) in &strings {
        let string = checker
            .locator
            .slice_source_code_range(&Range::new(*start, *end));
        let (Some(leader), Some(trailer)) = (leading_quote(string), trailing_quote(string)) else {
            return;
        };
        let string = &string[leader.len()..string.len() - trailer.len()];

        // Parse the format string (e.g. `"%s"`) into a list of `PercentFormat`.
        if let Ok(format_string) = CFormatString::from_str(string) {
            format_strings.push(format_string);
        };
    }

    // Parse the parameters.
    let is_valid = match &right.node {
        ExprKind::Tuple { elts, .. } => is_valid_tuple(&format_strings, elts),
        ExprKind::Dict { keys, values } => is_valid_dict(&format_strings, keys, values),
        ExprKind::Constant { value, .. } => is_valid_constant(&format_strings, value),
        ExprKind::Name { .. } => true,
        _ => is_valid_other(&format_strings),
    };
    if !is_valid {
        checker.diagnostics.push(Diagnostic::new(
            BadStringFormatType,
            Range::from_located(expr),
        ));
    }
}
