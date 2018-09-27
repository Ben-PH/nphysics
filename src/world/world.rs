use slab::Slab;
use std::f64;
use std::sync::Arc;
use either::Either;

use na::{self, Real};
use ncollide;
use ncollide::broad_phase::BroadPhasePairFilter;
use ncollide::events::{ContactEvents, ProximityEvents};
use ncollide::shape::{ShapeHandle, Shape, DeformableShape};
use ncollide::world::{CollisionGroups, CollisionObjectHandle, GeometricQueryType};

use counters::Counters;
use detection::{ActivationManager, ColliderContactManifold};
use force_generator::{ForceGenerator, ForceGeneratorHandle};
use joint::{ConstraintHandle, Joint, JointConstraint};
use math::{Inertia, Isometry, Point, Vector};
use object::{
    Body, BodyPartHandle, BodyPart, BodySet, BodyStatus, Collider, ColliderData, ColliderAnchor,
    ColliderHandle, Colliders, Material, Multibody, MultibodyLink,
    MultibodyWorkspace, RigidBody, SensorHandle, BodyHandle, Bodies, BodiesMut,
};
use solver::{ContactModel, IntegrationParameters, MoreauJeanSolver, SignoriniCoulombPyramidModel};

/// Type of the collision world used by nphysics.
pub type CollisionWorld<N> = ncollide::world::CollisionWorld<N, ColliderData<N>>;

/// The physics world.
pub struct World<N: Real> {
    counters: Counters,
    bodies: BodySet<N>,
    active_bodies: Vec<BodyHandle>,
    colliders_w_parent: Vec<ColliderHandle>,
    // The set of colliders that have a parent.
    cworld: CollisionWorld<N>,
    solver: MoreauJeanSolver<N>,
    activation_manager: ActivationManager<N>,
    // FIXME: set those two parameters per-collider?
    prediction: N,
    angular_prediction: N,
    gravity: Vector<N>,
    constraints: Slab<Box<JointConstraint<N>>>,
    forces: Slab<Box<ForceGenerator<N>>>,
    params: IntegrationParameters<N>,
    workspace: MultibodyWorkspace<N>,
}

impl<N: Real> World<N> {
    /// Creates a new physics world with default parameters.
    ///
    /// The ground body is automatically created and added to the world without any colliders attached.
    pub fn new() -> Self {
        let counters = Counters::new(false);
        let bv_margin = na::convert(0.01f64);
        let prediction = na::convert(0.002);
        let angular_prediction = na::convert(f64::consts::PI / 180.0 * 5.0);
        let bodies = BodySet::new();
        let active_bodies = Vec::new();
        let colliders_w_parent = Vec::new();
        let constraints = Slab::new();
        let forces = Slab::new();
        let mut cworld = CollisionWorld::new(bv_margin);
        let contact_model = Box::new(SignoriniCoulombPyramidModel::new());
        let solver = MoreauJeanSolver::new(contact_model);
        let activation_manager = ActivationManager::new(na::convert(0.01f64));
        let gravity = Vector::zeros();
        let params = IntegrationParameters::default();
        let workspace = MultibodyWorkspace::new();
        cworld.register_broad_phase_pair_filter(
            "__nphysics_internal_body_status_collision_filter",
            BodyStatusCollisionFilter,
        );

        World {
            counters,
            bodies,
            active_bodies,
            colliders_w_parent,
            cworld,
            solver,
            activation_manager,
            prediction,
            angular_prediction,
            gravity,
            constraints,
            forces,
            params,
            workspace,
        }
    }

    /// Prediction distance used internally for collision detection.
    pub fn prediction(&self) -> N {
        self.prediction
    }

    /// Disable the perfomance counters that measure various times and statistics during a timestep.
    pub fn disable_performance_counters(&mut self) {
        self.counters.disable();
    }

    /// Enable the perfomance counters that measure various times and statistics during a timestep.
    pub fn enable_performance_counters(&mut self) {
        self.counters.enable();
    }

    /// Retrieve the perfomance counters that measure various times and statistics during a timestep.
    pub fn performance_counters(&self) -> &Counters {
        &self.counters
    }

    /// Set the contact model for all contacts.
    pub fn set_contact_model<C: ContactModel<N>>(&mut self, model: C) {
        self.solver.set_contact_model(Box::new(model))
    }

    /// Retrieve a reference to the parameters for the integration.
    pub fn integration_parameters(&self) -> &IntegrationParameters<N> {
        &self.params
    }

    /// Retrieve a mutable reference to the parameters for the integration.
    pub fn integration_parameters_mut(&mut self) -> &mut IntegrationParameters<N> {
        &mut self.params
    }

    /// Retrieve the timestep used for the integration.
    pub fn timestep(&self) -> N {
        self.params.dt
    }

    /// Sets the timestep used for the integration.
    pub fn set_timestep(&mut self, dt: N) {
        self.params.dt = dt;
    }

    /// Activate the given body.
    pub fn activate_body(&mut self, handle: BodyHandle) {
        Self::activate_body_at(&mut self.bodies, handle)
    }

    // NOTE: static method used to avoid borrowing issues.
    fn activate_body_at(bodies: &mut BodySet<N>, handle: BodyHandle) {
        let body = bodies.body_mut(handle);
        if body.status_dependent_ndofs() != 0 {
            body.activate();
        }
    }

    /// Add a constraints to the physics world and retrieves its handle.
    pub fn add_constraint<C: JointConstraint<N>>(&mut self, constraint: C) -> ConstraintHandle {
        let (anchor1, anchor2) = constraint.anchors();
        self.activate_body(anchor1.body_handle);
        self.activate_body(anchor2.body_handle);
        self.constraints.insert(Box::new(constraint))
    }

    /// Get a reference to the specified constraint.
    pub fn constraint(&self, handle: ConstraintHandle) -> &JointConstraint<N> {
        &*self.constraints[handle]
    }

    /// Get a mutable reference to the specified constraint.
    pub fn constraint_mut(&mut self, handle: ConstraintHandle) -> &mut JointConstraint<N> {
        let (anchor1, anchor2) = self.constraints[handle].anchors();
        self.activate_body(anchor1.body_handle);
        self.activate_body(anchor2.body_handle);
        &mut *self.constraints[handle]
    }

    /// Remove the specified constraint from the world.
    pub fn remove_constraint(&mut self, handle: ConstraintHandle) -> Box<JointConstraint<N>> {
        let constraint = self.constraints.remove(handle);
        let (anchor1, anchor2) = constraint.anchors();
        self.activate_body(anchor1.body_handle);
        self.activate_body(anchor2.body_handle);

        constraint
    }

    /// Remove the specified collider from the world.
    pub fn remove_colliders(&mut self, handles: &[ColliderHandle]) {
        self.cworld.remove(handles);
        self.colliders_w_parent
            .retain(|handle| !handles.contains(handle));
    }

    /// Add a force generator to the world.
    pub fn add_force_generator<G: ForceGenerator<N>>(
        &mut self,
        force_generator: G,
    ) -> ForceGeneratorHandle {
        self.forces.insert(Box::new(force_generator))
    }

    /// Retrieve a reference to the specified force generator.
    pub fn force_generator(&self, handle: ForceGeneratorHandle) -> &ForceGenerator<N> {
        &*self.forces[handle]
    }

    /// Retrieve a mutable reference to the specified force generator.
    pub fn force_generator_mut(&mut self, handle: ForceGeneratorHandle) -> &mut ForceGenerator<N> {
        &mut *self.forces[handle]
    }

    /// Remove the specified force generator from the world.
    pub fn remove_force_generator(
        &mut self,
        handle: ForceGeneratorHandle,
    ) -> Box<ForceGenerator<N>> {
        self.forces.remove(handle)
    }

    /// Set the gravity.
    pub fn set_gravity(&mut self, gravity: Vector<N>) {
        self.gravity = gravity
    }

    /// The gravity applied to all dynamic bodies.
    pub fn gravity(&self) -> &Vector<N> {
        &self.gravity
    }

    /// Execute one time step of the physics simulation.
    pub fn step(&mut self) {
        self.counters.step_started();
        self.counters.update_started();
        // FIXME: objects involeved in a non-linear position stabilization elready
        // updated their kinematics.
        self.bodies.clear_dynamics();
        self.bodies.update_kinematics();

        for gen in &mut self.forces {
            let _ = gen.1.apply(&self.params, &mut self.bodies);
        }

        self.bodies
            .update_dynamics(&self.gravity, &self.params, &mut self.workspace);
        self.counters.update_completed();

        self.counters.collision_detection_started();
        for collider_id in &self.colliders_w_parent {
            // FIXME: the new_pos trick will probably no longer be
            // needed once NLL land.
            let new_pos;
            let body;
            {
                // FIXME: update only if the position changed (especially for static bodies).
                let collider = self
                    .cworld
                    .collision_object_mut(*collider_id)
                    .expect("Internal error: collider not found.");
                body = self.bodies.body(collider.data().body());

                collider
                    .data_mut()
                    .set_body_status_dependent_ndofs(body.status_dependent_ndofs());

                if !body.is_active() {
                    continue;
                }

                match collider.data().anchor() {
                    ColliderAnchor::OnBodyPart { body_part, position_wrt_body_part } => {
                        let part = body.part(*body_part);
                        let part_pos = part.position();
                        new_pos = Either::Left(part_pos * position_wrt_body_part)
                    }
                    ColliderAnchor::OnDeformableBody { indices, .. } => {
                        // FIXME: too bad we have to clone the indices here
                        // (that's why this is an arc) to avoid borrowing issue.
                        new_pos = Either::Right(indices.clone());
                    }
                }
            }

            match new_pos {
                Either::Left(pos) => self.cworld.set_position(*collider_id, pos),
                Either::Right(indices) => self.cworld.set_deformations(*collider_id, body.deformed_positions().unwrap().1, indices.as_ref().map(|idx| &idx[..]))
            }
        }

        self.cworld.clear_events();
        self.counters.broad_phase_started();
        self.cworld.perform_broad_phase();
        self.counters.broad_phase_completed();
        self.counters.narrow_phase_started();
        self.cworld.perform_narrow_phase();
        self.counters.narrow_phase_completed();
        self.counters.collision_detection_completed();

        if self.counters.enabled() {
            let npairs = self.cworld.contact_pairs().count();
            self.counters.set_ncontact_pairs(npairs);
        }

        // FIXME: for now, no island is built.
        self.counters.island_construction_started();
        self.active_bodies.clear();
        self.activation_manager.update(
            &mut self.bodies,
            &self.cworld,
            &self.constraints,
            &mut self.active_bodies,
        );
        self.counters.island_construction_completed();

        let mut contact_manifolds = Vec::new(); // FIXME: avoid allocations.
        for (coll1, coll2, c) in self.cworld.contact_manifolds() {
            // assert!(coll1.data().body_part() != coll2.data().body());

            let b1 = self.bodies.body(coll1.data().body());
            let b2 = self.bodies.body(coll2.data().body());

            if b1.status() != BodyStatus::Disabled && b2.status() != BodyStatus::Disabled
                && ((b1.status_dependent_ndofs() != 0 && b1.is_active())
                || (b2.status_dependent_ndofs() != 0 && b2.is_active()))
                {
                    contact_manifolds.push(ColliderContactManifold::new(coll1, coll2, c));
                }
        }

        self.counters.solver_started();
        self.solver.step(
            &mut self.counters,
            &mut self.bodies,
            &mut self.constraints,
            &contact_manifolds[..],
            &self.active_bodies[..],
            &self.params,
            &self.cworld,
        );

        // FIXME: not sure what is the most pretty/efficient way of doing this.
        for mb in self.bodies.bodies_mut() {
            if mb.status() == BodyStatus::Kinematic {
                mb.integrate(&self.params)
            }
        }

        self.counters.solver_completed();
        self.counters.step_completed();
    }

    /// Remove the specified bodies.
    pub fn remove_bodies(&mut self, handles: &[BodyHandle]) {
        for handle in handles {
            self.bodies.remove_body(*handle);
        }

        self.cleanup_after_body_removal();
    }

    /// Remove several links of a single multibody.
    ///
    /// Panics if not all links belong to the same multibody.
    pub fn remove_multibody_links(&mut self, links: &[BodyPartHandle]) {
        self.bodies.remove_multibody_links(links);
        self.cleanup_after_body_removal();
    }

    fn cleanup_after_body_removal(&mut self) {
        self.activate_bodies_touching_deleted_bodies();
        self.cleanup_colliders_with_deleted_parents();
        self.cleanup_constraints_with_deleted_anchors();
    }

    fn activate_bodies_touching_deleted_bodies(&mut self) {
        let bodies = &mut self.bodies;

        for (co1, co2, detector) in self.cworld.contact_pairs() {
            if detector.num_contacts() != 0 {
                let b1_exists = bodies.contains_body(co1.data().body());
                let b2_exists = bodies.contains_body(co2.data().body());

                if !b1_exists {
                    if b2_exists {
                        Self::activate_body_at(bodies, co2.data().body());
                    }
                } else if !b2_exists {
                    Self::activate_body_at(bodies, co1.data().body());
                }
            }
        }
    }

    fn cleanup_colliders_with_deleted_parents(&mut self) {
        let mut i = 0;

        while i < self.colliders_w_parent.len() {
            let cid = self.colliders_w_parent[i];
            let do_remove;

            match self
                .collider(cid)
                .expect("Internal error: collider not present")
                .data()
                .anchor() {
                ColliderAnchor::OnBodyPart { body_part, .. } => {
                    do_remove = !self.bodies.contains_body_part(*body_part)
                }
                ColliderAnchor::OnDeformableBody { body, .. } => {
                    do_remove = !self.bodies.contains_body(*body)
                }
            };

            if do_remove {
                self.cworld.remove(&[cid]);
                let _ = self.colliders_w_parent.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn cleanup_constraints_with_deleted_anchors(&mut self) {
        let bodies = &mut self.bodies;

        self.constraints.retain(|_, constraint| {
            let (b1, b2) = constraint.anchors();
            let b1_exists = bodies.contains_body_part(b1);
            let b2_exists = bodies.contains_body_part(b2);

            if !b1_exists {
                if b2_exists {
                    Self::activate_body_at(bodies, b2.body_handle);
                }
            } else if !b2_exists {
                Self::activate_body_at(bodies, b1.body_handle);
            }

            b1_exists && b2_exists
        })
    }

    /// Add an abstract body to the world.
    pub fn add_body(&mut self, body: Box<Body<N>>) -> BodyHandle {
        self.bodies.add_body(body)
    }

    /// Add a rigid body to the world and retrieve its handle.
    pub fn add_rigid_body(
        &mut self,
        position: Isometry<N>,
        local_inertia: Inertia<N>,
        local_center_of_mass: Point<N>,
    ) -> BodyPartHandle {
        self.bodies
            .add_rigid_body(position, local_inertia, local_center_of_mass)
    }

    /// Add a multibody link to the world and retrieve its handle.
    pub fn add_multibody_link<J: Joint<N>>(
        &mut self,
        parent: BodyPartHandle,
        joint: J,
        parent_shift: Vector<N>,
        body_shift: Vector<N>,
        local_inertia: Inertia<N>,
        local_center_of_mass: Point<N>,
    ) -> BodyPartHandle {
        self.bodies.add_multibody_link(
            parent,
            joint,
            parent_shift,
            body_shift,
            local_inertia,
            local_center_of_mass,
        )
    }

    /// Add a deformable collider to the world and retrieve its handle.
    pub fn add_deformable_collider<S: Shape<N> + DeformableShape<N> + Clone>(
        &mut self,
        margin: N,
        shape: S,
        parent: BodyHandle,
        dof_map: Option<Arc<Vec<usize>>>,
        parts_map: Option<Arc<Vec<usize>>>,
        material: Material<N>,
    ) -> ColliderHandle {
        let query = GeometricQueryType::Contacts(
            margin + self.prediction * na::convert(0.5f64),
            self.angular_prediction,
        );
        self.add_deformable_collision_object(query, margin, shape, parent, dof_map, parts_map, material)
    }

    /// Add a collider to the world and retrieve its handle.
    pub fn add_collider(
        &mut self,
        margin: N,
        shape: ShapeHandle<N>,
        parent: BodyPartHandle,
        to_parent: Isometry<N>,
        material: Material<N>,
    ) -> ColliderHandle {
        let query = GeometricQueryType::Contacts(
            margin + self.prediction * na::convert(0.5f64),
            self.angular_prediction,
        );
        self.add_collision_object(query, margin, shape, parent, to_parent, material)
    }

    /// Add a sensor to the world and retrieve its handle.
    pub fn add_sensor(
        &mut self,
        shape: ShapeHandle<N>,
        parent: BodyPartHandle,
        to_parent: Isometry<N>,
    ) -> SensorHandle {
        let query = GeometricQueryType::Proximity(self.prediction * na::convert(0.5f64));
        self.add_collision_object(
            query,
            N::zero(),
            shape,
            parent,
            to_parent,
            Material::default(),
        )
    }

    fn add_collision_object(
        &mut self,
        query: GeometricQueryType<N>,
        margin: N,
        shape: ShapeHandle<N>,
        parent: BodyPartHandle,
        to_parent: Isometry<N>,
        material: Material<N>,
    ) -> CollisionObjectHandle {
        let (pos, ndofs) = if parent.is_ground() {
            (to_parent, 0)
        } else {
            let parent_body = self.bodies.body(parent.body_handle);
            let parent_part = parent_body.part(parent);
            (
                parent_part.position() * to_parent,
                parent_body.status_dependent_ndofs(),
            )
        };

        let anchor = ColliderAnchor::OnBodyPart { body_part: parent, position_wrt_body_part: to_parent };
        let data = ColliderData::new(margin, anchor, ndofs, material);
        let groups = CollisionGroups::new();
        let handle = self.cworld.add(pos, shape, groups, query, data);

        if !parent.is_ground() {
            self.colliders_w_parent.push(handle);
        }

        handle
    }

    fn add_deformable_collision_object<S: Shape<N> + DeformableShape<N> + Clone>(
        &mut self,
        query: GeometricQueryType<N>,
        margin: N,
        shape: S,
        parent: BodyHandle,
        dof_map: Option<Arc<Vec<usize>>>,
        parts_map: Option<Arc<Vec<usize>>>,
        material: Material<N>,
    ) -> CollisionObjectHandle {
        let parent_body = self.bodies.body(parent);
        let parent_deformation_type = parent_body
            .deformed_positions()
            .expect("A deformable collider must be attached to a deformable body.")
            .0;

        assert_eq!(
            parent_deformation_type,
            shape.deformations_type(),
            "Both the deformable shape and deformable body must support the same deformation types."
        );

        let anchor = ColliderAnchor::OnDeformableBody { body: parent, indices: dof_map, body_parts: parts_map };
        let ndofs = parent_body.status_dependent_ndofs();
        let data = ColliderData::new(margin, anchor, ndofs, material);
        let groups = CollisionGroups::new();
        let handle = self.cworld.add(Isometry::identity(), ShapeHandle::new(shape), groups, query, data);

        self.colliders_w_parent.push(handle);

        handle
    }

    /// Get a reference to the specified body part.
    ///
    /// Panics if a body part with the given handle was not found.
    pub fn body_part(&self, handle: BodyPartHandle) -> &BodyPart<N> {
        self.bodies.body_part(handle)
    }

    /// Get a mutable reference to the specified body part.
    ///
    /// Panics if a body part with the given handle was not found.
    pub fn body_part_mut(&mut self, handle: BodyPartHandle) -> &mut BodyPart<N> {
        self.bodies.body_part_mut(handle)
    }

    /// Get a reference to the specified body.
    pub fn body(&self, handle: BodyHandle) -> &Body<N> {
        self.bodies.body(handle)
    }

    /// Get a mutable reference to the specified body.
    pub fn body_mut(&mut self, handle: BodyHandle) -> &mut Body<N> {
        self.bodies.body_mut(handle)
    }

    /// Get a reference to the multibody containing the specified multibody link.
    ///
    /// Returns `None` if the handle does not correspond to a multibody link in this world.
    pub fn multibody(&self, handle: BodyHandle) -> Option<&Multibody<N>> {
        self.bodies.multibody(handle)
    }

    /// Get a mutable reference to the multibody containing the specified multibody link.
    ///
    /// Returns `None` if the handle does not correspond to a multibody link in this world.
    pub fn multibody_mut(&mut self, handle: BodyHandle) -> Option<&mut Multibody<N>> {
        self.bodies.multibody_mut(handle)
    }

    /// Get a reference to the specified multibody link.
    ///
    /// Returns `None` if the handle does not correspond to a multibody link in this world.
    pub fn multibody_link(&self, handle: BodyPartHandle) -> Option<&MultibodyLink<N>> {
        self.bodies.multibody_link(handle)
    }

    /// Get a mutable reference to the specified multibody link.
    ///
    /// Returns `None` if the handle does not correspond to a multibody link in this world.
    pub fn multibody_link_mut(&mut self, handle: BodyPartHandle) -> Option<&mut MultibodyLink<N>> {
        self.bodies.multibody_link_mut(handle)
    }

    /// Get a reference to the specified rigid body.
    ///
    /// Returns `None` if the handle does not correspond to a rigid body in this world.
    pub fn rigid_body(&self, handle: BodyHandle) -> Option<&RigidBody<N>> {
        self.bodies.rigid_body(handle)
    }

    /// Get a mutable reference to the specified rigid body.
    ///
    /// Returns `None` if the handle does not correspond to a rigid body in this world.
    pub fn rigid_body_mut(&mut self, handle: BodyHandle) -> Option<&mut RigidBody<N>> {
        self.bodies.rigid_body_mut(handle)
    }

    /// Reference to the underlying collision world.
    pub fn collision_world(&self) -> &CollisionWorld<N> {
        &self.cworld
    }
    /// Mutable reference to the underlying collision world.
    pub fn collision_world_mut(&mut self) -> &mut CollisionWorld<N> {
        &mut self.cworld
    }

    /// Get a mutable reference to the specified collider.
    ///
    /// Returns `None` if the handle does not correspond to a collider in this world.
    pub fn collider(&self, handle: ColliderHandle) -> Option<&Collider<N>> {
        self.cworld.collision_object(handle)
    }

    /// Gets the handle of the body the specified collider is attached to.
    pub fn collider_body_handle(&self, handle: ColliderHandle) -> Option<BodyHandle> {
        self.collider_anchor(handle).map(|anchor| anchor.body())
    }

    /// Gets the anchor attaching this collider to a body or body part.
    pub fn collider_anchor(&self, handle: ColliderHandle) -> Option<&ColliderAnchor<N>> {
        self.cworld
            .collision_object(handle)
            .map(|co| co.data().anchor())
    }

    /// An iterator through all the colliders on this collision world.
    pub fn colliders(&self) -> Colliders<N> {
        self.cworld.collision_objects()
    }

    /// An iterator through all the bodies on this world.
    pub fn bodies(&self) -> Bodies<N> { self.bodies.bodies() }

    /// A mutable iterator through all the bodies on this world.
    pub fn bodies_mut(&mut self) -> BodiesMut<N> { self.bodies.bodies_mut() }

    /// An iterator through all the contact events generated during the last execution of `self.step()`.
    pub fn contact_events(&self) -> &ContactEvents {
        self.cworld.contact_events()
    }

    /// An iterator through all the proximity events generated during the last execution of `self.step()`.
    pub fn proximity_events(&self) -> &ProximityEvents {
        self.cworld.proximity_events()
    }
}

impl<N: Real> Default for World<N> {
    fn default() -> Self {
        Self::new()
    }
}

struct BodyStatusCollisionFilter;

impl<N: Real> BroadPhasePairFilter<N, ColliderData<N>> for BodyStatusCollisionFilter {
    /// Activate an action for when two objects start or stop to be close to each other.
    fn is_pair_valid(&self, b1: &Collider<N>, b2: &Collider<N>) -> bool {
        b1.data().body_status_dependent_ndofs() != 0 || b2.data().body_status_dependent_ndofs() != 0
    }
}

#[cfg(test)]
mod test {
    use world::World;

    #[test]
    fn world_is_send_sync() {
        let _ = Box::new(World::<f32>::new()) as Box<Send + Sync>;
    }
}
