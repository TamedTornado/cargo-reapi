use std::path::Path;

use bevy::prelude::{Component, World};

#[derive(Component)]
struct FixtureValue(u32);

#[test]
fn bevy_world_and_relocated_manifest_are_correct() {
    let mut world = World::new();
    let entity = world.spawn(FixtureValue(42)).id();
    assert_eq!(world.get::<FixtureValue>(entity).unwrap().0, 42);
    assert!(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml").is_file());
    println!("manifest={}", env!("CARGO_MANIFEST_DIR"));
}
