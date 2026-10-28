//! The typed input schema of a PolicyCheckpoint (ADR-0075 §2), generated from
//! the Rust types that build the input.
//!
//! A checkpoint's input is a Rust struct declared with [`policy_struct!`]. The
//! macro writes the struct and a [`PolicyType`] impl from the same field list,
//! so the schema the checker reads and the value the evaluator is given cannot
//! drift apart. Optional fields come from `Option<T>`, list caps from
//! [`Capped<T, N>`], and closed string sets from [`policy_enum!`].
//!
//! **String caps.** Every string in an input has a length cap: a field's
//! [`Field::max_len`], [`MAX_STRING_LEN`] for names, ids, refs and image
//! references, and [`MAX_FREE_TEXT_LEN`] for a [`FreeText`] field. The
//! checker costs a string function as one node; the caps are what make that
//! sound. [`check_string_lengths`] and binding refuse a longer string.

use std::collections::BTreeMap;
use std::fmt;

/// A CEL type, as far as the policy subset needs one.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Bool,
    Int,
    Uint,
    Double,
    String,
    Bytes,
    Null,
    Timestamp,
    Duration,
    /// A string drawn from a closed set, named by its [`EnumDecl`].
    Enum(String),
    /// A list with at most `cap` elements. The cap feeds the static cost bound.
    List {
        elem: Box<Type>,
        cap: u64,
    },
    /// A map literal's type. No checkpoint schema declares a map today.
    Map {
        key: Box<Type>,
        value: Box<Type>,
        cap: u64,
    },
    /// A struct, named by its [`StructDecl`].
    Struct(String),
    /// The element type of an empty list literal: compatible with anything.
    Dyn,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Bool => f.write_str("bool"),
            Type::Int => f.write_str("int"),
            Type::Uint => f.write_str("uint"),
            Type::Double => f.write_str("double"),
            Type::String => f.write_str("string"),
            Type::Bytes => f.write_str("bytes"),
            Type::Null => f.write_str("null"),
            Type::Timestamp => f.write_str("timestamp"),
            Type::Duration => f.write_str("duration"),
            Type::Enum(name) => write!(f, "{name} (a string)"),
            Type::List { elem, .. } => write!(f, "list<{elem}>"),
            Type::Map { key, value, .. } => write!(f, "map<{key}, {value}>"),
            Type::Struct(name) => f.write_str(name),
            Type::Dyn => f.write_str("dyn"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    /// The field may be absent. Reading it without `has()` (or, for a
    /// top-level variable, without `!= null`) draws a warning.
    pub optional: bool,
    /// The longest string, in bytes, the field holds: its own value, or each
    /// element of a list of strings. A struct's strings use their own
    /// fields' caps.
    pub max_len: usize,
}

/// The longest string, in bytes, in a checkpoint input: names, ids, refs,
/// image references and hashes.
pub const MAX_STRING_LEN: usize = 1024;
/// The longest [`FreeText`] string, in bytes: a dispatch reason.
pub const MAX_FREE_TEXT_LEN: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<Field>,
}

impl StructDecl {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: String,
    pub variants: Vec<String>,
}

/// The struct and enum declarations a schema is built from.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    structs: BTreeMap<String, Option<StructDecl>>,
    enums: BTreeMap<String, EnumDecl>,
}

impl Registry {
    /// Declares struct `name` once, building its fields with `fields` (which may
    /// recurse into this registry).
    pub fn struct_type(
        &mut self,
        name: &str,
        fields: impl FnOnce(&mut Registry) -> Vec<Field>,
    ) -> Type {
        if !self.structs.contains_key(name) {
            self.structs.insert(name.to_string(), None);
            let fields = fields(self);
            self.structs.insert(
                name.to_string(),
                Some(StructDecl {
                    name: name.to_string(),
                    fields,
                }),
            );
        }
        Type::Struct(name.to_string())
    }

    pub fn enum_type(&mut self, name: &str, variants: &[&str]) -> Type {
        self.enums
            .entry(name.to_string())
            .or_insert_with(|| EnumDecl {
                name: name.to_string(),
                variants: variants.iter().map(|v| v.to_string()).collect(),
            });
        Type::Enum(name.to_string())
    }
}

/// A Rust type that has a CEL type in a checkpoint schema.
pub trait PolicyType {
    /// `Option<T>`: the field may be absent.
    const OPTIONAL: bool = false;
    /// The longest string a field of this type holds; see [`Field::max_len`].
    const MAX_LEN: usize = MAX_STRING_LEN;
    fn policy_type(reg: &mut Registry) -> Type;
}

macro_rules! primitive {
    ($($rust:ty => $cel:expr),* $(,)?) => {
        $(impl PolicyType for $rust {
            fn policy_type(_: &mut Registry) -> Type { $cel }
        })*
    };
}
primitive!(String => Type::String, bool => Type::Bool, i64 => Type::Int, u64 => Type::Uint, f64 => Type::Double);

impl<T: PolicyType> PolicyType for Option<T> {
    const OPTIONAL: bool = true;
    const MAX_LEN: usize = T::MAX_LEN;
    fn policy_type(reg: &mut Registry) -> Type {
        T::policy_type(reg)
    }
}

/// A list with at most `N` elements. The cap is part of the type, so the cost
/// bound the checker computes from it is the cap the input is built with.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct Capped<T, const N: usize>(Vec<T>);

impl<T, const N: usize> Capped<T, N> {
    /// `None` when `items` exceeds the cap.
    pub fn new(items: Vec<T>) -> Option<Self> {
        (items.len() <= N).then_some(Capped(items))
    }
}

impl<T: PolicyType, const N: usize> PolicyType for Capped<T, N> {
    const MAX_LEN: usize = T::MAX_LEN;
    fn policy_type(reg: &mut Registry) -> Type {
        Type::List {
            elem: Box::new(T::policy_type(reg)),
            cap: N as u64,
        }
    }
}

/// A free-form string, such as a dispatch reason: a CEL `string` of at most
/// [`MAX_FREE_TEXT_LEN`] bytes, where other strings hold [`MAX_STRING_LEN`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct FreeText(pub String);

impl From<String> for FreeText {
    fn from(s: String) -> Self {
        FreeText(s)
    }
}

impl From<&str> for FreeText {
    fn from(s: &str) -> Self {
        FreeText(s.to_string())
    }
}

impl PolicyType for FreeText {
    const MAX_LEN: usize = MAX_FREE_TEXT_LEN;
    fn policy_type(_: &mut Registry) -> Type {
        Type::String
    }
}

/// A point in time, bound as a CEL `timestamp`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct Timestamp(pub chrono::DateTime<chrono::Utc>);

impl PolicyType for Timestamp {
    fn policy_type(_: &mut Registry) -> Type {
        Type::Timestamp
    }
}

/// Declares a checkpoint input struct and its schema from one field list.
#[macro_export]
macro_rules! policy_struct {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $( $(#[$fmeta:meta])* $fvis:vis $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, serde::Serialize)]
        $vis struct $name {
            $( $(#[$fmeta])* $fvis $field : $ty ),*
        }

        impl $crate::schema::PolicyType for $name {
            fn policy_type(reg: &mut $crate::schema::Registry) -> $crate::schema::Type {
                reg.struct_type(stringify!($name), |reg| vec![
                    $( $crate::schema::Field {
                        // `r#ref` is the field `ref`, as serde names it.
                        name: stringify!($field).trim_start_matches("r#").to_string(),
                        ty: <$ty as $crate::schema::PolicyType>::policy_type(reg),
                        optional: <$ty as $crate::schema::PolicyType>::OPTIONAL,
                        max_len: <$ty as $crate::schema::PolicyType>::MAX_LEN,
                    } ),*
                ])
            }
        }
    };
}

/// Declares a closed set of strings: a Rust enum that serialises to (and
/// deserialises from) the given names, and the [`Type::Enum`] the checker
/// validates literals against.
#[macro_export]
macro_rules! policy_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident { $( $variant:ident => $wire:literal ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
        )]
        $vis enum $name {
            $( #[serde(rename = $wire)] $variant ),*
        }

        impl $crate::schema::PolicyType for $name {
            fn policy_type(reg: &mut $crate::schema::Registry) -> $crate::schema::Type {
                reg.enum_type(stringify!($name), &[$($wire),*])
            }
        }
    };
}

/// The input schema of one PolicyCheckpoint: its root struct's fields are the
/// CEL variables a rule can read.
#[derive(Clone, Debug)]
pub struct Schema {
    pub checkpoint: String,
    pub version: u32,
    pub variables: Vec<Field>,
    structs: BTreeMap<String, StructDecl>,
    enums: BTreeMap<String, EnumDecl>,
}

impl Schema {
    /// The schema whose variables are the fields of `Root`.
    pub fn for_root<Root: PolicyType>(checkpoint: &str, version: u32) -> Schema {
        let mut reg = Registry::default();
        let Type::Struct(root) = Root::policy_type(&mut reg) else {
            panic!("a checkpoint's root input must be a struct");
        };
        let structs: BTreeMap<String, StructDecl> = reg
            .structs
            .into_iter()
            .map(|(name, decl)| (name, decl.expect("every declared struct is defined")))
            .collect();
        let variables = structs[&root].fields.clone();
        Schema {
            checkpoint: checkpoint.to_string(),
            version,
            variables,
            structs,
            enums: reg.enums,
        }
    }

    pub fn variable(&self, name: &str) -> Option<&Field> {
        self.variables.iter().find(|f| f.name == name)
    }

    pub fn struct_decl(&self, name: &str) -> &StructDecl {
        &self.structs[name]
    }

    pub fn enum_decl(&self, name: &str) -> &EnumDecl {
        &self.enums[name]
    }
}

/// A string of an input longer than its field's [`Field::max_len`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{path}` is {len} bytes, more than the {max} a Policy input string may hold")]
pub struct StringTooLong {
    /// Where the string is, e.g. `pipeline.name` or `steps[3].image`.
    pub path: String,
    pub len: usize,
    pub max: usize,
}

/// A list of an input with more items than its type's cap.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{path}` has {len} items, more than the {cap} a Policy input list may hold")]
pub struct ListTooLong {
    /// Where the list is, e.g. `gates` or `steps[3].origin`.
    pub path: String,
    pub len: usize,
    pub cap: u64,
}

/// A string or a list of an input over its cap.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OverCap {
    #[error(transparent)]
    String(#[from] StringTooLong),
    #[error(transparent)]
    List(#[from] ListTooLong),
}

/// The first string of `input` longer than its field allows, walking the
/// input as `schema` declares it. A value of the wrong type is left to
/// binding to report; only lengths are checked here, in time linear in the
/// input.
pub fn check_string_lengths(
    schema: &Schema,
    input: &serde_json::Value,
) -> Result<(), StringTooLong> {
    match walk_input(schema, input, false) {
        Ok(()) => Ok(()),
        Err(OverCap::String(e)) => Err(e),
        Err(OverCap::List(_)) => unreachable!("list caps are not checked here"),
    }
}

/// The first string or list of `input` over its cap: what
/// [`check_string_lengths`] checks, and every list against its type's cap
/// (checked before its items are walked). Binding refuses both, but an input
/// that is read without binding it (the built-in plugin default reads the
/// gates directly) is held to the caps by this.
pub fn check_caps(schema: &Schema, input: &serde_json::Value) -> Result<(), OverCap> {
    walk_input(schema, input, true)
}

fn walk_input(schema: &Schema, input: &serde_json::Value, lists: bool) -> Result<(), OverCap> {
    for var in &schema.variables {
        if let Some(v) = input.get(&var.name) {
            walk_strings(
                schema,
                &var.ty,
                var.max_len,
                v,
                &mut var.name.clone(),
                lists,
            )?;
        }
    }
    Ok(())
}

fn walk_strings(
    schema: &Schema,
    ty: &Type,
    max_len: usize,
    v: &serde_json::Value,
    path: &mut String,
    lists: bool,
) -> Result<(), OverCap> {
    use serde_json::Value as J;
    match (ty, v) {
        (_, J::String(s)) if s.len() > max_len => Err(StringTooLong {
            path: path.clone(),
            len: s.len(),
            max: max_len,
        }
        .into()),
        (Type::List { cap, .. }, J::Array(items)) if lists && items.len() as u64 > *cap => {
            Err(ListTooLong {
                path: path.clone(),
                len: items.len(),
                cap: *cap,
            }
            .into())
        }
        (Type::List { elem, .. }, J::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let at = path.len();
                path.push_str(&format!("[{i}]"));
                walk_strings(schema, elem, max_len, item, path, lists)?;
                path.truncate(at);
            }
            Ok(())
        }
        (Type::Struct(name), J::Object(obj)) => {
            for field in &schema.struct_decl(name).fields {
                if let Some(fv) = obj.get(&field.name) {
                    let at = path.len();
                    path.push('.');
                    path.push_str(&field.name);
                    walk_strings(schema, &field.ty, field.max_len, fv, path, lists)?;
                    path.truncate(at);
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
