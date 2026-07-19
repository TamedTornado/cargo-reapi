use std::path::Path;

use bevy::prelude::{Component, Entity, World};

#[derive(Component)]
struct Answer(u32);

fn main() {
    let mut world = World::new();
    let entity: Entity = world.spawn(Answer(41)).id();
    let value = world.get::<Answer>(entity).expect("fixture component").0;
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    println!("{}:{}", manifest.display(), value + 1);
}
