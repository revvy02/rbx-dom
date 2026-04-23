use std::{borrow::Cow, collections::BTreeMap, io::Write};

use ahash::{HashMap, HashMapExt};
use rbx_dom_weak::{
    types::{Attributes, Ref, SharedString, SharedStringHash, Variant},
    WeakDom,
};
use rbx_reflection::{ClassDescriptor, PropertyKind, PropertySerialization, ReflectionDatabase};

static ATTRIBUTES_PROP_NAME: &str = "Attributes";

/// Walks `class_descriptor` and its superclasses to collect entries that must
/// be merged into the `Attributes` property of every serialized instance of
/// this class. See `TypeInfo::attribute_merge_entries` in rbx_binary for full
/// context.
fn collect_attribute_merge_entries<'db>(
    database: &'db ReflectionDatabase<'db>,
    class_name: &str,
) -> Vec<(&'static str, Variant)> {
    let mut entries: Vec<(&'static str, Variant)> = Vec::new();
    let Some(mut class) = database.classes.get(class_name) else {
        return entries;
    };

    loop {
        for property in class.properties.values() {
            if let PropertyKind::Canonical {
                serialization: PropertySerialization::Migrate(prop_migration),
            } = &property.kind
            {
                if let Some(merge_entries) = prop_migration.attribute_merge_entries() {
                    entries.extend(merge_entries);
                }
            }
        }

        match class.superclass.as_ref() {
            Some(superclass_name) => match database.classes.get(superclass_name.as_ref()) {
                Some(superclass) => class = superclass as &ClassDescriptor<'db>,
                None => break,
            },
            None => break,
        }
    }

    entries
}

use crate::{
    conversion::ConvertVariant,
    core::find_serialized_property_descriptor,
    error::{EncodeError as NewEncodeError, EncodeErrorKind},
    types::write_value_xml,
};

use crate::serializer_core::{XmlEventWriter, XmlWriteEvent};

pub fn encode_internal<W: Write>(
    output: W,
    tree: &WeakDom,
    ids: &[Ref],
    options: EncodeOptions,
) -> Result<(), NewEncodeError> {
    let mut writer = XmlEventWriter::from_output(output);
    let mut state = EmitState::new(options);

    writer.write(XmlWriteEvent::start_element("roblox").attr("version", "4"))?;

    let mut property_buffer = Vec::new();
    for id in ids {
        serialize_instance(&mut writer, &mut state, tree, *id, &mut property_buffer)?;
    }

    serialize_shared_strings(&mut writer, &mut state)?;

    writer.write(XmlWriteEvent::end_element())?;

    Ok(())
}

/// Describes the strategy that rbx_xml should use when serializing properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EncodePropertyBehavior {
    /// Ignores properties that aren't known by rbx_xml.
    ///
    /// This is the default.
    IgnoreUnknown,

    /// Write unrecognized properties.
    ///
    /// With this option set, properties that are newer than rbx_xml's
    /// reflection database will show up. It may be problematic to depend on
    /// these properties, since rbx_xml may start supporting them with
    /// non-reflection specific names at a future date.
    WriteUnknown,

    /// Returns an error if any properties are found that aren't known by
    /// rbx_xml.
    ErrorOnUnknown,

    /// Completely turns off rbx_xml's reflection database. Property names and
    /// types will appear exactly as they are in the tree.
    ///
    /// This setting is useful for debugging the model format. It leaves the
    /// user to deal with oddities like how `Part.FormFactor` is actually
    /// serialized as `Part.formFactorRaw`.
    NoReflection,
}

/// Options available for serializing an XML-format model or place.
#[derive(Debug, Clone)]
pub struct EncodeOptions<'db> {
    property_behavior: EncodePropertyBehavior,
    database: &'db ReflectionDatabase<'db>,
}

impl<'db> EncodeOptions<'db> {
    /// Constructs a `EncodeOptions` with all values set to their defaults.
    #[inline]
    pub fn new() -> Self {
        EncodeOptions {
            property_behavior: EncodePropertyBehavior::IgnoreUnknown,
            database: rbx_reflection_database::get().unwrap(),
        }
    }

    /// Determines how rbx_xml will serialize properties, especially unknown
    /// ones.
    #[inline]
    pub fn property_behavior(self, property_behavior: EncodePropertyBehavior) -> Self {
        EncodeOptions {
            property_behavior,
            ..self
        }
    }

    /// Determines what reflection database rbx_xml will use to serialize
    /// properties.
    #[inline]
    pub fn reflection_database(self, database: &'db ReflectionDatabase<'db>) -> Self {
        EncodeOptions { database, ..self }
    }

    pub(crate) fn use_reflection(&self) -> bool {
        self.property_behavior != EncodePropertyBehavior::NoReflection
    }
}

impl<'db> Default for EncodeOptions<'db> {
    fn default() -> EncodeOptions<'db> {
        EncodeOptions::new()
    }
}

pub struct EmitState<'db> {
    options: EncodeOptions<'db>,

    /// A map of IDs written so far to the generated referent that they use.
    /// This map is used to correctly emit Ref properties.
    referent_map: HashMap<Ref, u32>,

    /// The referent value that will be used for emitting the next instance.
    next_referent: u32,

    /// A map of all shared strings referenced so far while generating XML. This
    /// map will be written as the file's SharedString dictionary.
    shared_strings_to_emit: BTreeMap<SharedStringHash, SharedString>,
}

impl<'db> EmitState<'db> {
    pub fn new(options: EncodeOptions<'db>) -> EmitState<'db> {
        EmitState {
            options,
            referent_map: HashMap::new(),
            next_referent: 0,
            shared_strings_to_emit: BTreeMap::new(),
        }
    }

    pub fn map_id(&mut self, id: Ref) -> u32 {
        match self.referent_map.get(&id) {
            Some(&value) => value,
            None => {
                let referent = self.next_referent;
                self.referent_map.insert(id, referent);
                self.next_referent += 1;
                referent
            }
        }
    }

    pub fn add_shared_string(&mut self, value: SharedString) {
        self.shared_strings_to_emit.insert(value.hash(), value);
    }
}

/// Serialize a single instance.
///
/// `property_buffer` is a Vec that can be reused between calls to
/// serialize_instance to make sorting properties more efficient.
fn serialize_instance<'dom, W: Write>(
    writer: &mut XmlEventWriter<W>,
    state: &mut EmitState,
    tree: &'dom WeakDom,
    id: Ref,
    property_buffer: &mut Vec<(&'dom str, &'dom Variant)>,
) -> Result<(), NewEncodeError> {
    let instance = tree.get_by_ref(id).unwrap();
    let mapped_id = state.map_id(id);

    writer.write(
        XmlWriteEvent::start_element("Item")
            .attr("class", &instance.class)
            .attr("referent", &mapped_id.to_string()),
    )?;

    writer.write(XmlWriteEvent::start_element("Properties"))?;

    write_value_xml(
        writer,
        state,
        "Name",
        &Variant::String(instance.name.clone()),
    )?;

    // Move references to our properties into property_buffer so we can sort
    // them and iterate them in order.
    property_buffer.extend(instance.properties.iter().map(|(k, v)| (k.as_str(), v)));
    property_buffer.sort_unstable_by_key(|(key, _)| *key);

    // Collect any attribute-merge entries defined by Migrate serializations on
    // this class (e.g. LightingTechnologyUnifiedMigration). When non-empty, we
    // ensure these are present in the written Attributes property; if the
    // instance has no Attributes, we emit one containing just these entries.
    let attribute_merge_entries = if state.options.use_reflection() {
        collect_attribute_merge_entries(state.options.database, &instance.class)
    } else {
        Vec::new()
    };
    let mut attributes_emitted = false;

    for (property_name, value) in property_buffer.drain(..) {
        let maybe_serialized_descriptor = if state.options.use_reflection() {
            find_serialized_property_descriptor(
                &instance.class,
                property_name,
                state.options.database,
            )
        } else {
            None
        };

        if let Some(serialized_descriptor) = maybe_serialized_descriptor {
            let data_type = serialized_descriptor.data_type.ty();

            let mut serialized_name = serialized_descriptor.name.as_ref();

            let mut converted_value = match value.try_convert_ref(instance.class, data_type) {
                Ok(value) => value,
                Err(message) => {
                    return Err(
                        writer.error(EncodeErrorKind::UnsupportedPropertyConversion {
                            class_name: instance.class.to_string(),
                            property_name: property_name.to_string(),
                            expected_type: data_type,
                            actual_type: value.ty(),
                            message,
                        }),
                    )
                }
            };

            // Perform migrations during serialization
            if let PropertyKind::Canonical {
                serialization: PropertySerialization::Migrate(migration),
            } = &serialized_descriptor.kind
            {
                // Attribute-merge migrations don't replace the source
                // property's serialization; they only cause entries to be
                // merged into the class's Attributes property. Leave the
                // source (e.g. LightingStyle) serializing as itself.
                if migration.attribute_merge_entries().is_none() {
                    // If the migration fails, there's no harm in us doing nothing
                    // since old values will still load in Studio.
                    if let Ok(new_value) = migration.perform(&converted_value) {
                        converted_value = Cow::Owned(new_value);
                        serialized_name = &migration.new_property_name
                    }
                }
            }

            // Merge attribute-merge entries into the Attributes property if
            // this class defines any. Instance values take precedence.
            let merged_attributes_value;
            let value_to_write: &Variant = if serialized_name == ATTRIBUTES_PROP_NAME
                && !attribute_merge_entries.is_empty()
            {
                attributes_emitted = true;
                if let Variant::Attributes(attrs) = converted_value.as_ref() {
                    let mut merged = attrs.clone();
                    for (key, merge_value) in &attribute_merge_entries {
                        if merged.get(*key).is_none() {
                            merged.insert((*key).to_string(), merge_value.clone());
                        }
                    }
                    merged_attributes_value = Variant::Attributes(merged);
                    &merged_attributes_value
                } else {
                    // Unexpected type; let the writer handle it.
                    converted_value.as_ref()
                }
            } else {
                converted_value.as_ref()
            };

            write_value_xml(writer, state, serialized_name, value_to_write)?;
        } else {
            match state.options.property_behavior {
                EncodePropertyBehavior::IgnoreUnknown => {}
                EncodePropertyBehavior::WriteUnknown | EncodePropertyBehavior::NoReflection => {
                    // We'll take this value as-is with no conversions on
                    // either the name or value.

                    write_value_xml(writer, state, property_name, value)?;
                }
                EncodePropertyBehavior::ErrorOnUnknown => {
                    return Err(writer.error(EncodeErrorKind::UnknownProperty {
                        class_name: instance.class.to_string(),
                        property_name: property_name.to_string(),
                    }));
                }
            }
        }
    }

    // If this class has attribute-merge entries but no Attributes property was
    // emitted (instance had no Attributes), synthesize one so the required
    // entries still make it into the output.
    if !attribute_merge_entries.is_empty() && !attributes_emitted {
        let mut synthesized = Attributes::new();
        for (key, merge_value) in &attribute_merge_entries {
            synthesized.insert((*key).to_string(), merge_value.clone());
        }
        write_value_xml(
            writer,
            state,
            ATTRIBUTES_PROP_NAME,
            &Variant::Attributes(synthesized),
        )?;
    }

    writer.write(XmlWriteEvent::end_element())?;

    for child_id in instance.children() {
        serialize_instance(writer, state, tree, *child_id, property_buffer)?;
    }

    writer.write(XmlWriteEvent::end_element())?;

    Ok(())
}

fn serialize_shared_strings<W: Write>(
    writer: &mut XmlEventWriter<W>,
    state: &mut EmitState,
) -> Result<(), NewEncodeError> {
    if state.shared_strings_to_emit.is_empty() {
        return Ok(());
    }

    writer.write(XmlWriteEvent::start_element("SharedStrings"))?;

    for value in state.shared_strings_to_emit.values() {
        // Roblox expects SharedString hashes to be the same length as an MD5
        // hash: 16 bytes, so we truncate our larger hashes to fit.
        let full_hash = value.hash();
        let truncated_hash = &full_hash.as_bytes()[..16];

        writer.write(
            XmlWriteEvent::start_element("SharedString")
                .attr("md5", &base64::encode(truncated_hash)),
        )?;

        writer.write_string(&base64::encode(value.data()))?;
        writer.end_element()?;
    }

    writer.end_element()?;
    Ok(())
}
