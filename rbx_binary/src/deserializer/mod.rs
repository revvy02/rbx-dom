mod error;
mod header;
mod state;

use std::{io::Read, str};

use rbx_dom_weak::{
    types::{Ref, Variant},
    InstanceBuilder, Ustr, WeakDom,
};
use rbx_reflection::ReflectionDatabase;

use self::state::DeserializerState;

#[cfg(any(test, feature = "unstable_text_format"))]
pub(crate) use self::header::FileHeader;

pub use self::error::Error;

/// An in-progress instance populated by the binary decoder.
///
/// Binary files store instances and their properties in separate columnar
/// chunks. Implementing this trait lets a [`DecodeTarget`] choose the compact
/// representation used while those chunks are assembled.
pub trait DecodeInstance: Sized {
    /// Create an instance with the given class and expected property count.
    fn new(class: Ustr, property_capacity: usize) -> Self;

    /// Return the stable referent assigned to this instance.
    fn referent(&self) -> Ref;

    /// Set the decoded `Name` property.
    fn set_name(&mut self, name: String);

    /// Return whether a property has already been decoded.
    ///
    /// Property migrations use this to avoid replacing an explicitly decoded
    /// value with a migrated legacy value.
    fn has_property(&self, name: Ustr) -> bool;

    /// Add a decoded canonical property.
    ///
    /// Properties are provided in file order. If a name occurs more than
    /// once, the final value has the same precedence as it does in a
    /// [`WeakDom`].
    fn add_property(&mut self, name: Ustr, value: Variant);
}

/// Destination for instances produced by the binary decoder.
///
/// The decoder retains responsibility for parsing chunks, resolving
/// referents, canonicalizing properties, and applying migrations. A target
/// controls only the in-progress instance representation and the final tree
/// storage, allowing read-only tools to avoid materializing a [`WeakDom`].
pub trait DecodeTarget: Sized {
    /// In-progress instance representation used while decoding property
    /// columns.
    type Instance: DecodeInstance;

    /// Final value returned by [`Deserializer::deserialize_into`].
    type Output;

    /// Reserve storage for the number of instances declared by the file.
    fn reserve(&mut self, _additional: usize) {}

    /// Return the referent of the target's synthetic `DataModel` root.
    fn root_ref(&self) -> Ref;

    /// Insert a fully decoded instance under `parent`.
    ///
    /// Implementations must preserve [`DecodeInstance::referent`] as the
    /// inserted instance's referent. Calls are top-down and preserve the child
    /// order encoded by the file.
    fn insert(&mut self, parent: Ref, instance: Self::Instance);

    /// Finish constructing and return the target value.
    fn finish(self) -> Self::Output;
}

impl DecodeInstance for InstanceBuilder {
    fn new(class: Ustr, property_capacity: usize) -> Self {
        InstanceBuilder::with_property_capacity(class, property_capacity)
    }

    fn referent(&self) -> Ref {
        InstanceBuilder::referent(self)
    }

    fn set_name(&mut self, name: String) {
        InstanceBuilder::set_name(self, name);
    }

    fn has_property(&self, name: Ustr) -> bool {
        InstanceBuilder::has_property(self, name)
    }

    fn add_property(&mut self, name: Ustr, value: Variant) {
        InstanceBuilder::add_property(self, name, value);
    }
}

struct WeakDomTarget {
    tree: WeakDom,
}

impl WeakDomTarget {
    fn new() -> Self {
        Self {
            tree: WeakDom::new(InstanceBuilder::new("DataModel")),
        }
    }
}

impl DecodeTarget for WeakDomTarget {
    type Instance = InstanceBuilder;
    type Output = WeakDom;

    fn reserve(&mut self, additional: usize) {
        self.tree.reserve(additional);
    }

    fn root_ref(&self) -> Ref {
        self.tree.root_ref()
    }

    fn insert(&mut self, parent: Ref, instance: Self::Instance) {
        let expected_ref = instance.referent();
        let inserted_ref = self.tree.insert(parent, instance);
        debug_assert_eq!(inserted_ref, expected_ref);
    }

    fn finish(self) -> Self::Output {
        self.tree
    }
}

/// A configurable deserializer for Roblox binary models and places.
///
/// ## Example
/// ```no_run
/// use std::fs::File;
/// use std::io::BufReader;
///
/// use rbx_binary::Deserializer;
///
/// let input = BufReader::new(File::open("File.rbxm")?);
///
/// let deserializer = Deserializer::new();
/// let dom = deserializer.deserialize(input)?;
///
/// // rbx_binary always returns a DOM with a DataModel at the top level.
/// // To get to the instances from our file, we need to go one level deeper.
///
/// println!("Root instances in file:");
/// for &referent in dom.root().children() {
///     let instance = dom.get_by_ref(referent).unwrap();
///     println!("- {}", instance.name);
/// }
///
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// ## Configuration
///
/// A custom [`ReflectionDatabase`][ReflectionDatabase] can be specified via
/// [`reflection_database`][reflection_database].
///
/// [ReflectionDatabase]: rbx_reflection::ReflectionDatabase
/// [reflection_database]: Deserializer#method.reflection_database
pub struct Deserializer<'db> {
    database: &'db ReflectionDatabase<'db>,
}

impl<'db> Deserializer<'db> {
    /// Create a new `Deserializer` with the default settings.
    pub fn new() -> Self {
        Self {
            database: rbx_reflection_database::get().unwrap(),
        }
    }

    /// Sets what reflection database for the deserializer to use.
    #[inline]
    pub fn reflection_database(self, database: &'db ReflectionDatabase<'db>) -> Self {
        Self { database }
    }

    /// Deserialize a Roblox binary model or place from the given stream using
    /// this deserializer.
    pub fn deserialize<R: Read>(&self, reader: R) -> Result<WeakDom, Error> {
        self.deserialize_into(reader, WeakDomTarget::new())
    }

    /// Deserialize a Roblox binary model or place directly into `target`.
    ///
    /// This uses the same parsing, property migration, and reference
    /// resolution as [`deserialize`](Self::deserialize), but does not require
    /// an intermediate [`WeakDom`].
    pub fn deserialize_into<R: Read, T: DecodeTarget>(
        &self,
        reader: R,
        target: T,
    ) -> Result<T::Output, Error> {
        profiling::scope!("rbx_binary::deserialize");

        let mut deserializer = DeserializerState::new(self, reader, target)?;

        loop {
            let chunk = deserializer.next_chunk()?;

            match &chunk.name {
                b"META" => deserializer.decode_meta_chunk(&chunk.data)?,
                b"SSTR" => deserializer.decode_sstr_chunk(&chunk.data)?,
                b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
                b"PROP" => deserializer.decode_prop_chunk(&chunk.data)?,
                b"PRNT" => deserializer.decode_prnt_chunk(&chunk.data)?,
                b"END\0" => {
                    deserializer.decode_end_chunk(&chunk.data)?;
                    break;
                }
                _ => match str::from_utf8(&chunk.name) {
                    Ok(name) => log::info!("Unknown binary chunk name {name}"),
                    Err(_) => log::info!("Unknown binary chunk name {:?}", chunk.name),
                },
            }
        }

        Ok(deserializer.finish())
    }
}

impl Default for Deserializer<'_> {
    fn default() -> Self {
        Self::new()
    }
}
