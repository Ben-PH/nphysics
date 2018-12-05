use na::{DVector, Real};
use std::ops::Range;

use crate::joint::JointConstraint;
use crate::math::{AngularVector, Isometry, Point, ANGULAR_DIM};
use crate::object::{BodyPartHandle, BodySet};
use crate::solver::helper;
use crate::solver::{ConstraintSet, GenericNonlinearConstraint, IntegrationParameters,
             NonlinearConstraintGenerator};

/// A constraint that removes all relative angular motion between two body parts.
pub struct CartesianConstraint<N: Real> {
    b1: BodyPartHandle,
    b2: BodyPartHandle,
    joint_to_b1: Isometry<N>,
    joint_to_b2: Isometry<N>,
    ang_impulses: AngularVector<N>,
    bilateral_ground_rng: Range<usize>,
    bilateral_rng: Range<usize>,
}

impl<N: Real> CartesianConstraint<N> {
    /// Creates a cartesian constraint between two body parts.
    /// 
    /// This will ensure the rotational parts of the frames given identified by `joint_to_b1` and
    /// `joint_to_b2` and attached to the corresponding bodies will coincide.
    pub fn new(
        b1: BodyPartHandle,
        b2: BodyPartHandle,
        joint_to_b1: Isometry<N>,
        joint_to_b2: Isometry<N>,
    ) -> Self {
        CartesianConstraint {
            b1,
            b2,
            joint_to_b1,
            joint_to_b2,
            ang_impulses: AngularVector::zeros(),
            bilateral_ground_rng: 0..0,
            bilateral_rng: 0..0,
        }
    }

    /// Changes the reference frame for the first body part.
    pub fn set_anchor_1(&mut self, local1: Isometry<N>) {
        self.joint_to_b1 = local1
    }

    /// Changes the reference frame for the second body part.
    pub fn set_anchor_2(&mut self, local2: Isometry<N>) {
        self.joint_to_b2 = local2
    }
}

impl<N: Real> JointConstraint<N> for CartesianConstraint<N> {
    fn num_velocity_constraints(&self) -> usize {
        ANGULAR_DIM
    }

    fn anchors(&self) -> (BodyPartHandle, BodyPartHandle) {
        (self.b1, self.b2)
    }

    fn velocity_constraints(
        &mut self,
        _: &IntegrationParameters<N>,
        bodies: &BodySet<N>,
        ext_vels: &DVector<N>,
        ground_j_id: &mut usize,
        j_id: &mut usize,
        jacobians: &mut [N],
        constraints: &mut ConstraintSet<N>,
    ) {
        let body1 = bodies.body(self.b1.body_handle);
        let body2 = bodies.body(self.b2.body_handle);

        let part1 = body1.part(self.b1);
        let part2 = body2.part(self.b2);

        let pos1 = part1.position() * self.joint_to_b1;
        let pos2 = part2.position() * self.joint_to_b2;

        let anchor1 = Point::from_coordinates(pos1.translation.vector);
        let anchor2 = Point::from_coordinates(pos2.translation.vector);

        let assembly_id1 = body1.companion_id();
        let assembly_id2 = body2.companion_id();

        let first_bilateral_ground = constraints.velocity.bilateral_ground.len();
        let first_bilateral = constraints.velocity.bilateral.len();

        helper::cancel_relative_angular_velocity(
            body1,
            part1,
            body2,
            part2,
            assembly_id1,
            assembly_id2,
            &anchor1,
            &anchor2,
            ext_vels,
            &self.ang_impulses,
            0,
            ground_j_id,
            j_id,
            jacobians,
            constraints,
        );

        self.bilateral_ground_rng =
            first_bilateral_ground..constraints.velocity.bilateral_ground.len();
        self.bilateral_rng = first_bilateral..constraints.velocity.bilateral.len();
    }

    fn cache_impulses(&mut self, constraints: &ConstraintSet<N>) {
        for c in &constraints.velocity.bilateral_ground[self.bilateral_ground_rng.clone()] {
            self.ang_impulses[c.impulse_id] = c.impulse;
        }

        for c in &constraints.velocity.bilateral[self.bilateral_rng.clone()] {
            self.ang_impulses[c.impulse_id] = c.impulse;
        }
    }
}

impl<N: Real> NonlinearConstraintGenerator<N> for CartesianConstraint<N> {
    fn num_position_constraints(&self, bodies: &BodySet<N>) -> usize {
        // FIXME: calling this at each iteration of the non-linear resolution is costly.
        if self.is_active(bodies) {
            1
        } else {
            0
        }
    }

    fn position_constraint(
        &self,
        params: &IntegrationParameters<N>,
        _: usize,
        bodies: &mut BodySet<N>,
        jacobians: &mut [N],
    ) -> Option<GenericNonlinearConstraint<N>> {
        let body1 = bodies.body(self.b1.body_handle);
        let body2 = bodies.body(self.b2.body_handle);
        let part1 = body1.part(self.b1);
        let part2 = body2.part(self.b2);

        let pos1 = part1.position() * self.joint_to_b1;
        let pos2 = part2.position() * self.joint_to_b2;

        let anchor1 = Point::from_coordinates(pos1.translation.vector);
        let anchor2 = Point::from_coordinates(pos2.translation.vector);

        let rotation1 = pos1.rotation;
        let rotation2 = pos2.rotation;

        helper::cancel_relative_rotation(
            params,
            body1,
            part1,
            body2,
            part2,
            &anchor1,
            &anchor2,
            &rotation1,
            &rotation2,
            jacobians,
        )
    }
}
