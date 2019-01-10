extern crate nalgebra as na;
extern crate ncollide2d;
extern crate nphysics2d;
extern crate nphysics_testbed2d;

use std::sync::Arc;
use std::f32;
use std::path::Path;
use na::{Isometry2, Point2, Point3, Vector2, Translation3};
use ncollide2d::shape::{Cuboid, ShapeHandle, Ball, Polyline};
use ncollide2d::procedural;
use ncollide2d::bounding_volume::{self, AABB, BoundingVolume};
use nphysics2d::object::{BodyPartHandle, Material, MassConstraintSystemDesc, ColliderDesc, RigidBodyDesc};
use nphysics2d::volumetric::Volumetric;
use nphysics2d::world::World;
use nphysics2d::math::Inertia;
use nphysics_testbed2d::Testbed;


const COLLIDER_MARGIN: f32 = 0.01;

fn main() {
    /*
     * World
     */
    let mut world = World::new();
    world.set_gravity(Vector2::new(0.0, -9.81));

    /*
     * Ground.
     */
    let obstacle = ShapeHandle::new(Cuboid::new(Vector2::repeat(0.2)));

    let mut obstacle_desc = ColliderDesc::new(obstacle);

    let _ = obstacle_desc
        .set_translation(Vector2::x() * 4.0)
        .build(&mut world);

    let _ = obstacle_desc
        .set_translation(Vector2::x() * -4.0)
        .build(&mut world);

    /*
     * Create the deformable body and a collider for its boundary.
     */
    let deformable = MassConstraintSystemDesc::quad(50, 1)
        .with_scale(Vector2::new(10.0, 1.0))
        .with_translation(Vector2::y() * 1.0)
        .with_stiffness(Some(1.0e4))
        .with_collider_enabled(true)
        .build(&mut world);
    let deformable_handle = deformable.handle();

    // Add other constraints for volume stiffness.
    deformable.generate_neighbor_constraints(Some(1.0e4));
    deformable.generate_neighbor_constraints(Some(1.0e4));

    let nnodes = deformable.num_nodes();
    let extra_constraints1 = (0..).map(|i| Point2::new(i, nnodes - i - 2)).take(nnodes / 2);
    let extra_constraints2 = (1..).map(|i| Point2::new(i, nnodes - i)).take(nnodes / 2);

    for constraint in extra_constraints1.chain(extra_constraints2) {
        deformable.add_constraint(constraint.x, constraint.y, Some(1.0e4));
    }

    /*
     * Create a pyramid on top of the deformable body.
     */
    let num = 20;
    let rad = 0.1;
    let shift = 2.0 * rad;
    let centerx = shift * (num as f32) / 2.0;

    let cuboid = ShapeHandle::new(Cuboid::new(Vector2::repeat(rad)));
    let mut collider_desc = ColliderDesc::new(cuboid)
        .with_density(Some(0.1));

    let mut rb_desc = RigidBodyDesc::default()
        .with_collider(&collider_desc);

    for i in 0usize..num {
        for j in i..num {
            let fj = j as f32;
            let fi = i as f32;
            let x = (fi * shift / 2.0) + (fj - fi) * 2.0 * (rad + ColliderDesc::<f32>::default_margin()) - centerx;
            let y = fi * 2.0 * (rad + collider_desc.margin()) + rad + 2.0;

            // Build the rigid body and its collider.
            let _ = rb_desc
                .set_translation(Vector2::new(x, y))
                .build(&mut world);
        }
    }

    /*
     * Set up the testbed.
     */
    let mut testbed = Testbed::new(world);
    testbed.set_body_color(deformable_handle, Point3::new(0.0, 0.0, 1.0));
    testbed.look_at(Point2::new(0.0, -3.0), 100.0);
    testbed.run();
}
