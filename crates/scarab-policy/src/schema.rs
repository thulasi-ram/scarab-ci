//! The typed input schema of a PolicyCheckpoint (ADR-0075 §2), generated from
//! the Rust types that build the input.
//!
//! A checkpoint's input is a Rust struct declared with [`policy_struct!`]. The
//! macro writes the struct and a [`PolicyType`] impl from the same field list,
//! so the schema the checker reads and the value the evaluator is given cannot
//! drift apart. Optional fields come from `Option<T>`, list caps from
//! [`Capped<T, N>`], and closed string sets from [`policy_enum!`].

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
}

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
    fn policy_type(reg: &mut Registry) -> Type {
        Type::List {
            elem: Box::new(T::policy_type(reg)),
            cap: N as u64,
        }
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
                    } ),*
                ])
            }
        }
    };
}

/// Declares a closed set of strings: a Rust enum that serialises to the given
/// names, and the [`Type::Enum`] the checker validates literals against.
#[macro_export]
macro_rules! policy_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident { $( $variant:ident => $wire:literal ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
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
