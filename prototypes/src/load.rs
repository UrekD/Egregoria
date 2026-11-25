use crate::validation::ValidationError;
use crate::{validation, Prototypes, PROTOTYPES};
use common::error::MultiError;
use mlua::{Lua, Table, Value};
use std::io;
use std::sync::atomic::Ordering;
use thiserror::Error;

pub fn test_prototypes(lua: &str) {
    let l = Lua::new();

    unsafe { load_prototypes_str(l, lua).unwrap() };
}

/// Loads the prototypes from the data.lua file
/// # Safety
/// This function is not thread safe, and should only be called once at the start of the program.
pub unsafe fn load_prototypes(base: &str) -> Result<(), PrototypeLoadError> {
    log::info!("loading prototypes from {}", base);
    let l = Lua::new();

    let base = base.to_string();

    l.globals()
        .get::<_, Table>("package")?
        .set("path", base.clone() + "base_mod/?.lua")?;

    load_prototypes_str(
        l,
        &common::saveload::load_string(base + "base_mod/data.lua")?,
    )
}

unsafe fn load_prototypes_str(l: Lua, main: &str) -> Result<(), PrototypeLoadError> {
    l.load(include_str!("prototype_init.lua")).exec()?;

    l.load(main).exec()?;

    let mut p = Box::<Prototypes>::default();

    let mut errors = Vec::new();

    let data_table = l.globals().get::<_, Table>("data")?;

    let _ = data_table.for_each(|_: String, t: Table| {
        let r = p.parse_prototype(t);
        if let Err(e) = r {
            errors.push(e);
        }
        Ok(())
    });

    if !errors.is_empty() {
        return Err(PrototypeLoadError::MultiError(MultiError(errors)));
    }

    validation::validate(&p)?;

    p.fingerprint = prototype_value_hash(Value::Table(data_table), 0)?;
    p.compute_orderings();
    p.print_stats();

    PROTOTYPES.store(Box::into_raw(p), Ordering::Release);

    Ok(())
}

impl Prototypes {
    /// Identifies the Lua declarations used to build this registry.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

fn prototype_value_hash(value: Value<'_>, depth: u32) -> mlua::Result<u64> {
    use common::hash_u64;
    Ok(match value {
        Value::Boolean(value) => hash_u64((1u8, value)),
        Value::Integer(value) => hash_u64((2u8, value)),
        Value::Number(value) => hash_u64((3u8, value.to_bits())),
        Value::String(value) => hash_u64((4u8, value.as_bytes())),
        Value::Table(table) if depth < 32 => {
            let mut entries = Vec::with_capacity(table.raw_len());
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                entries.push((
                    prototype_value_hash(key, depth + 1)?,
                    prototype_value_hash(value, depth + 1)?,
                ));
            }
            entries.sort_unstable();
            hash_u64((5u8, entries))
        }
        // Functions (such as data:extend) and other opaque Lua values are not
        // prototype fields. Keep addresses and table traversal order out of the fingerprint.
        other => hash_u64((6u8, other.type_name())),
    })
}

#[derive(Error, Debug)]
pub enum PrototypeLoadError {
    #[error("loading data.lua: {0}")]
    LoadingDataLua(#[from] io::Error),
    #[error("lua error: {0}")]
    LuaError(#[from] mlua::Error),
    #[error("lua error for {0} {1}: {2}")]
    PrototypeLuaError(String, String, mlua::Error),
    #[error("service {0} shares its name with a {1} prototype")]
    ConflictingServiceName(String, &'static str),
    #[error("multiple errors: {0}")]
    MultiError(MultiError<PrototypeLoadError>),
    #[error("validation errors: {0}")]
    ValidationErrors(#[from] MultiError<ValidationError>),
}
