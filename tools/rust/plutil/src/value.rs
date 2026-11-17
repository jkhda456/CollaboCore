//! A property list value, with the types CoreFoundation gives them: what the parsers make and
//! the writers take. Containers are reference-counted, so a binary plist's shared objects stay
//! shared (as CF's are) and copies are cheap; changes go through `Rc::make_mut`.

use indexmap::IndexMap;
use std::cmp::Ordering;
use std::rc::Rc;

/// A dictionary: its keys in the order the input had them (CF's own order is its hash table's,
/// which no file says; XML, `-p` and the literal formats sort, as CF and plutil do).
pub type Dict = IndexMap<String, Value>;

#[derive(Clone, Debug)]
pub enum Value {
    String(String),
    /// A CFNumber of an integer type up to SInt64 (NSNumber "q").
    Int(i64),
    /// CF's SInt128 with high = 0 (NSNumber "Q"): an integer above INT64_MAX, or any a binary
    /// plist stores in 16 bytes.
    UInt(u64),
    /// Float64 (NSNumber "d").
    Real(f64),
    /// Float32, from a binary plist's 4-byte real (NSNumber "f").
    Float(f32),
    Bool(bool),
    /// Seconds since 2001-01-01 00:00:00 UTC (CFAbsoluteTime).
    Date(f64),
    Data(Rc<Vec<u8>>),
    Array(Rc<Vec<Value>>),
    Dict(Rc<Dict>),
    /// NSKeyedArchiver's reference (CF$UID).
    Uid(u64),
    /// What only a binary plist (0x00) or JSON (null) has: not a property list type.
    Null,
    /// A binary plist's set (0xC_): not a property list type.
    Set(Rc<Vec<Value>>),
    /// A binary plist's dictionary whose keys are not all strings: not a property list.
    KeyedDict(Rc<Vec<(Value, Value)>>),
}

impl Value {
    pub fn dict(d: Dict) -> Value {
        Value::Dict(Rc::new(d))
    }

    pub fn array(a: Vec<Value>) -> Value {
        Value::Array(Rc::new(a))
    }

    pub fn data(d: Vec<u8>) -> Value {
        Value::Data(Rc::new(d))
    }

    #[cfg(test)]
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Only property list types, all the way down (plutil's validatePropertyListKeyType: what it
    /// says when this fails is "Dictionaries are required to have string keys in property lists").
    /// UIDs count: they are what keyed archives are made of, and CF writes them.
    pub fn is_plist(&self) -> bool {
        match self {
            Value::Array(a) => a.iter().all(Value::is_plist),
            Value::Dict(d) => d.values().all(Value::is_plist),
            Value::Null | Value::Set(_) | Value::KeyedDict(_) => false,
            _ => true,
        }
    }

}

/// CFStringCompare(a, b, 0): by UTF-16 code units. The order XML dictionaries are written in.
pub fn cfstring_compare(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// plutil's sortDictionaryKeys: `compare(options: [.numeric, .caseInsensitive, .forcedOrdering])`
/// — runs of digits by their value, letters without case, and strings that are equal that way
/// in their literal order. The order of `-p`, `-extract … raw` of a dictionary, the literal
/// formats and `-r` JSON.
pub fn natural_compare(a: &str, b: &str) -> Ordering {
    let (x, y): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < x.len() && x[i].is_ascii_digit() {
                i += 1;
            }
            while j < y.len() && y[j].is_ascii_digit() {
                j += 1;
            }
            let trim = |s: &[char]| -> String {
                let t: String = s.iter().collect();
                let t = t.trim_start_matches('0').to_string();
                t
            };
            let (dx, dy) = (trim(&x[si..i]), trim(&y[sj..j]));
            let by_value = dx.len().cmp(&dy.len()).then_with(|| dx.cmp(&dy));
            if by_value != Ordering::Equal {
                return by_value;
            }
            continue;
        }
        let (cx, cy) = (fold(x[i]), fold(y[j]));
        if cx != cy {
            return cx.cmp(&cy);
        }
        i += 1;
        j += 1;
    }
    let by_length = (x.len() - i).cmp(&(y.len() - j));
    if by_length != Ordering::Equal {
        return by_length;
    }
    cfstring_compare(a, b)
}

fn fold(c: char) -> String {
    c.to_lowercase().collect()
}

/// plutil's PlutilExpectType: the names `-type` prints and `-expect` takes.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "bool",
        Value::Int(_) | Value::UInt(_) => "integer",
        Value::Real(_) | Value::Float(_) => "float",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Dict(_) => "dictionary",
        Value::Date(_) => "date",
        Value::Data(_) => "data",
        _ => "(any)",
    }
}

/// Every key, sorted as plutil sorts them.
pub fn sorted_keys(d: &Dict, cmp: fn(&str, &str) -> Ordering) -> Vec<&String> {
    let mut keys: Vec<&String> = d.keys().collect();
    keys.sort_by(|a, b| cmp(a, b));
    keys
}
