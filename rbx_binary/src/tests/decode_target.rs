use rbx_dom_weak::{
    types::{Ref, Variant},
    InstanceBuilder, Ustr, WeakDom,
};

use crate::{to_writer, DecodeInstance, DecodeTarget, Deserializer};

struct RecordedInstance {
    referent: Ref,
    name: String,
    class: Ustr,
    properties: Vec<(Ustr, Variant)>,
}

impl DecodeInstance for RecordedInstance {
    fn new(class: Ustr, property_capacity: usize) -> Self {
        Self {
            referent: Ref::new(),
            name: class.to_string(),
            class,
            properties: Vec::with_capacity(property_capacity),
        }
    }

    fn referent(&self) -> Ref {
        self.referent
    }

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn has_property(&self, name: Ustr) -> bool {
        self.properties
            .iter()
            .any(|(property_name, _)| *property_name == name)
    }

    fn add_property(&mut self, name: Ustr, value: Variant) {
        self.properties.push((name, value));
    }
}

struct InsertedInstance {
    parent: Ref,
    instance: RecordedInstance,
}

struct RecordingTarget {
    root_ref: Ref,
    reserved: usize,
    instances: Vec<InsertedInstance>,
}

impl RecordingTarget {
    fn new() -> Self {
        Self {
            root_ref: Ref::new(),
            reserved: 0,
            instances: Vec::new(),
        }
    }
}

impl DecodeTarget for RecordingTarget {
    type Instance = RecordedInstance;
    type Output = Self;

    fn reserve(&mut self, additional: usize) {
        self.reserved = additional;
        self.instances.reserve(additional);
    }

    fn root_ref(&self) -> Ref {
        self.root_ref
    }

    fn insert(&mut self, parent: Ref, instance: Self::Instance) {
        self.instances.push(InsertedInstance { parent, instance });
    }

    fn finish(self) -> Self::Output {
        self
    }
}

#[test]
fn custom_target_receives_resolved_tree_in_parent_first_order() {
    let part = InstanceBuilder::new("Part").with_name("Target");
    let part_ref = part.referent();
    let object = InstanceBuilder::new("ObjectValue")
        .with_name("Pointer")
        .with_property("Value", Variant::Ref(part_ref));
    let dom = WeakDom::new(
        InstanceBuilder::new("DataModel").with_child(
            InstanceBuilder::new("Folder")
                .with_name("Container")
                .with_child(part)
                .with_child(object),
        ),
    );
    let mut encoded = Vec::new();
    to_writer(&mut encoded, &dom, dom.root().children()).unwrap();

    let output = Deserializer::new()
        .deserialize_into(encoded.as_slice(), RecordingTarget::new())
        .unwrap();

    assert_eq!(output.reserved, 3);
    assert_eq!(output.instances.len(), 3);

    let folder = &output.instances[0];
    assert_eq!(folder.parent, output.root_ref);
    assert_eq!(folder.instance.class.as_str(), "Folder");
    assert_eq!(folder.instance.name, "Container");

    let target = output
        .instances
        .iter()
        .find(|inserted| inserted.instance.name == "Target")
        .unwrap();
    let pointer = output
        .instances
        .iter()
        .find(|inserted| inserted.instance.name == "Pointer")
        .unwrap();
    assert_eq!(target.parent, folder.instance.referent);
    assert_eq!(pointer.parent, folder.instance.referent);
    assert_eq!(
        pointer
            .instance
            .properties
            .iter()
            .find(|(name, _)| name.as_str() == "Value")
            .map(|(_, value)| value),
        Some(&Variant::Ref(target.instance.referent))
    );
}
