//! Regression test for an instance carrying both a legacy property name and
//! its canonical replacement (e.g. both `BrickColor` and `Color` on a `Part`).
//!
//! `InstanceBuilder::add_property` is a plain `Vec::push` with no de-dup, so
//! the public API permits constructing an `Instance` whose `properties` map
//! contains both keys. Both serialize to the same logical `Color` property,
//! and the serializer must not panic when that happens.

use rbx_dom_weak::types::{BrickColor, Color3uint8};
use rbx_dom_weak::{InstanceBuilder, WeakDom};

#[test]
fn serialize_part_with_brick_color_and_color_does_not_panic() {
    let mut builder = InstanceBuilder::new("Part");
    builder.add_property("BrickColor", BrickColor::BrightBlue);
    builder.add_property("Color", Color3uint8::new(255, 0, 0));
    let dom = WeakDom::new(builder);

    let mut buf = Vec::new();
    rbx_binary::to_writer(&mut buf, &dom, &[dom.root_ref()]).expect(
        "serialization should succeed even when an instance carries both a legacy property \
         name and its canonical replacement",
    );
}
