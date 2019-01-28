use std::ops::AddAssign;
use std::iter;
use std::collections::HashMap;
use std::sync::Arc;
use std::any::Any;
use either::Either;

use alga::linear::FiniteDimInnerSpace;
use na::{self, Real, Point3, Point4, Vector3, Vector6, Matrix3, Matrix3x4, DMatrix, Isometry3,
         DVector, DVectorSlice, DVectorSliceMut, Cholesky, Dynamic, U3, Rotation3, Unit, Translation3};
use ncollide::utils::{self, DeterministicState};
use ncollide::shape::{TriMesh, DeformationsType, ShapeHandle};

use crate::object::{Body, BodyPart, BodyHandle, BodyPartHandle, BodyStatus, BodyUpdateStatus,
                    BodyDesc, ActivationStatus, FiniteElementIndices, DeformableColliderDesc};
use crate::solver::{IntegrationParameters, ForceDirection};
use crate::math::{Force, ForceType, Inertia, Velocity, DIM};
use crate::world::{World, ColliderWorld};
use crate::object::fem_helper;

/// One element of a deformable volume.
#[derive(Clone)]
pub struct TetrahedralElement<N: Real> {
    handle: BodyPartHandle,
    indices: Point4<usize>,
    com: Point3<N>,
    rot: Rotation3<N>,
    inv_rot: Rotation3<N>,
    j: Matrix3<N>,
    local_j_inv: Matrix3x4<N>,
    total_strain: Vector6<N>,
    plastic_strain: Vector6<N>,
    volume: N,
    density: N,
}

/// A deformable volume using FEM to simulate linear elasticity.
///
/// The volume is described by a set of tetrahedral elements. This
/// implements an isoparametric approach where the interpolations are linear.
pub struct FEMVolume<N: Real> {
    handle: BodyHandle,
    elements: Vec<TetrahedralElement<N>>,
    kinematic_nodes: DVector<bool>,
    positions: DVector<N>,
    velocities: DVector<N>,
    accelerations: DVector<N>,
    forces: DVector<N>,
    augmented_mass: DMatrix<N>,
    inv_augmented_mass: Cholesky<N, Dynamic>,

    // Cache.
    workspace: DVector<N>,

    // Parameters
    gravity_enabled: bool,
    rest_positions: DVector<N>,
    damping_coeffs: (N, N),
    young_modulus: N,
    poisson_ratio: N,
    plasticity_threshold: N,
    plasticity_creep: N,
    plasticity_max_force: N,
    // Elasticity coefficients computed from the young modulus
    // and poisson ratio.
    d0: N,
    d1: N,
    d2: N,

    companion_id: usize,
    activation: ActivationStatus<N>,
    status: BodyStatus,
    update_status: BodyUpdateStatus,

    user_data: Option<Box<Any + Send + Sync>>,
}

impl<N: Real> FEMVolume<N> {
    /// Initializes a new deformable volume from its tetrahedral elements.
    pub fn new(handle: BodyHandle, vertices: &[Point3<N>], tetrahedrons: &[Point4<usize>], pos: &Isometry3<N>,
               scale: &Vector3<N>, density: N, young_modulus: N, poisson_ratio: N, damping_coeffs: (N, N)) -> Self {
        let elements = tetrahedrons.iter().enumerate().map(|(i, idx)|
            TetrahedralElement {
                handle: BodyPartHandle(handle, i),
                indices: idx * 3,
                com: Point3::origin(),
                rot: Rotation3::identity(),
                inv_rot: Rotation3::identity(),
                j: na::zero(),
                local_j_inv: na::zero(),
                total_strain: Vector6::zeros(),
                plastic_strain: Vector6::zeros(),
                volume: na::zero(),
                density,
            }).collect();

        let ndofs = vertices.len() * 3;
        let mut rest_positions = DVector::zeros(ndofs);

        for (i, pt)  in vertices.iter().enumerate() {
            let pt = pos * Point3::from_coordinates(pt.coords.component_mul(&scale));
            rest_positions.fixed_rows_mut::<U3>(i * 3).copy_from(&pt.coords);
        }

        let (d0, d1, d2) = fem_helper::elasticity_coefficients(young_modulus, poisson_ratio);

        FEMVolume {
            handle,
            elements,
            kinematic_nodes: DVector::repeat(vertices.len(), false),
            positions: rest_positions.clone(),
            velocities: DVector::zeros(ndofs),
            accelerations: DVector::zeros(ndofs),
            forces: DVector::zeros(ndofs),
            augmented_mass: DMatrix::zeros(ndofs, ndofs),
            inv_augmented_mass: Cholesky::new(DMatrix::zeros(0, 0)).unwrap(),
            workspace: DVector::zeros(ndofs),
            rest_positions,
            damping_coeffs,
            young_modulus,
            poisson_ratio,
            d0, d1, d2,
            companion_id: 0,
            plasticity_threshold: N::zero(),
            plasticity_max_force: N::zero(),
            plasticity_creep: N::zero(),
            activation: ActivationStatus::new_active(),
            status: BodyStatus::Dynamic,
            update_status: BodyUpdateStatus::all(),
            gravity_enabled: true,
            user_data: None
        }
    }

    user_data_accessors!();


    /// The position of this body in generalized coordinates.
    #[inline]
    pub fn positions(&self) -> &DVector<N> {
        &self.positions
    }

    /// The velocity of this body in generalized coordinates.
    #[inline]
    pub fn velocities(&self) -> &DVector<N> {
        &self.velocities
    }

    /// The mutable velocity of this body in generalized coordinates.
    #[inline]
    pub fn velocities_mut(&mut self) -> &mut DVector<N> {
        self.update_status.set_velocity_changed(true);
        &mut self.velocities
    }


    /// Sets the plastic properties of this deformable volume.
    ///
    /// Note that large plasticity creep coefficient can yield to significant instability.
    pub fn set_plasticity(&mut self, strain_threshold: N, creep: N, max_force: N) {
        self.plasticity_threshold = strain_threshold;
        self.plasticity_creep = creep;
        self.plasticity_max_force = max_force;
    }

    /// Sets the young modulus of this deformable surface.
    pub fn set_young_modulus(&mut self, young_modulus: N) {
        self.update_status.set_local_inertia_changed(true);
        self.young_modulus = young_modulus;

        let (d0, d1, d2) = fem_helper::elasticity_coefficients(self.young_modulus, self.poisson_ratio);
        self.d0 = d0;
        self.d1 = d1;
        self.d2 = d2;
    }

    /// Sets the poisson ratio of this deformable surface.
    pub fn set_poisson_ratio(&mut self, poisson_ratio: N) {
        self.update_status.set_local_inertia_changed(true);
        self.poisson_ratio = poisson_ratio;

        let (d0, d1, d2) = fem_helper::elasticity_coefficients(self.young_modulus, self.poisson_ratio);
        self.d0 = d0;
        self.d1 = d1;
        self.d2 = d2;
    }

    #[inline]
    pub fn handle(&self) -> BodyHandle {
        self.handle
    }

    fn assemble_mass_with_damping(&mut self, dt: N) {
        let mass_damping = dt * self.damping_coeffs.0;

        for elt in self.elements.iter().filter(|e| e.volume > N::zero()) {
            let coeff_mass = elt.density * elt.volume / na::convert::<_, N>(20.0f64) * (N::one() + mass_damping);

            for a in 0..4 {
                let ia = elt.indices[a];

                if !self.kinematic_nodes[ia / DIM] {
                    for b in 0..4 {
                        let ib = elt.indices[b];

                        if !self.kinematic_nodes[ib / DIM] {
                            let mass_contribution = if a == b {
                                coeff_mass * na::convert(2.0)
                            } else {
                                coeff_mass
                            };

                            let mut node_mass = self.augmented_mass.fixed_slice_mut::<U3, U3>(ia, ib);
                            node_mass[(0, 0)] += mass_contribution;
                            node_mass[(1, 1)] += mass_contribution;
                            node_mass[(2, 2)] += mass_contribution;
                        }
                    }
                }
            }
        }

        // Set the identity for kinematic nodes.
        for i in 0..self.kinematic_nodes.len() {
            if self.kinematic_nodes[i] {
                self.augmented_mass.fixed_slice_mut::<U3, U3>(i * DIM, i * DIM).fill_diagonal(N::one());
            }
        }
    }

    fn assemble_stiffness(&mut self, dt: N) {
        let _1: N = na::one();
        let _2: N = na::convert(2.0);
        let _6: N = na::convert(6.0);
        let stiffness_coeff = dt * (dt + self.damping_coeffs.1);

        for elt in self.elements.iter_mut().filter(|e| e.volume > N::zero()) {
            let d0_vol = self.d0 * elt.volume;
            let d1_vol = self.d1 * elt.volume;
            let d2_vol = self.d2 * elt.volume;

            for a in 0..4 {
                let ia = elt.indices[a];

                if !self.kinematic_nodes[ia / DIM] {
                    let bn = elt.local_j_inv[(0, a)];
                    let cn = elt.local_j_inv[(1, a)];
                    let dn = elt.local_j_inv[(2, a)];

                    // Fields of P_n * elt.volume:
                    //
                    // let P_n = Matrix3x6::new(
                    //     bn * d0, bn * d1, bn * d1, cn * d2, dn * d2, _0,
                    //     cn * d1, cn * d0, cn * d1, bn * d2, _0, dn * d2,
                    //     dn * d1, dn * d1, dn * d0, _0, bn * d2, cn * d2
                    // ) * elt.volume;
                    let bn0 = bn * d0_vol;
                    let bn1 = bn * d1_vol;
                    let bn2 = bn * d2_vol;
                    let cn0 = cn * d0_vol;
                    let cn1 = cn * d1_vol;
                    let cn2 = cn * d2_vol;
                    let dn0 = dn * d0_vol;
                    let dn1 = dn * d1_vol;
                    let dn2 = dn * d2_vol;

                    /*
                     * Add elastic strain.
                     */
                    for b in 0..4 {
                        let ib = elt.indices[b];
                        if !self.kinematic_nodes[ib / DIM] {
                            let bm = elt.local_j_inv[(0, b)];
                            let cm = elt.local_j_inv[(1, b)];
                            let dm = elt.local_j_inv[(2, b)];

                            let node_stiffness = Matrix3::new(
                                bn0 * bm + cn2 * cm + dn2 * dm, bn1 * cm + cn2 * bm, bn1 * dm + dn2 * bm,
                                cn1 * bm + bn2 * cm, cn0 * cm + bn2 * bm + dn2 * dm, cn1 * dm + dn2 * cm,
                                dn1 * bm + bn2 * dm, dn1 * cm + cn2 * dm, dn0 * dm + bn2 * bm + cn2 * cm,
                            );

                            let rot_stiffness = elt.rot * node_stiffness;
                            let mut mass_part = self.augmented_mass.fixed_slice_mut::<U3, U3>(ia, ib);
                            mass_part.gemm(stiffness_coeff, &rot_stiffness, elt.inv_rot.matrix(), N::one());
                        }
                    }
                }
            }
        }
    }

    fn assemble_forces(&mut self, gravity: &Vector3<N>, params: &IntegrationParameters<N>) {
        let _1: N = na::one();
        let _2: N = na::convert(2.0);
        let _6: N = na::convert(6.0);
        let dt = params.dt;

        // Gravity
        if self.gravity_enabled {
            for elt in self.elements.iter().filter(|e| e.volume > N::zero()) {
                let contribution = gravity * (elt.density * elt.volume * na::convert::<_, N>(1.0 / 4.0));

                for k in 0..4 {
                    let ie = elt.indices[k];

                    if !self.kinematic_nodes[ie / DIM] {
                        let mut forces_part = self.accelerations.fixed_rows_mut::<U3>(ie);
                        forces_part += contribution;
                    }
                }
            }
        }

        for elt in self.elements.iter_mut().filter(|e| e.volume > N::zero()) {
            let d0_vol = self.d0 * elt.volume;
            let d1_vol = self.d1 * elt.volume;
            let d2_vol = self.d2 * elt.volume;

            /*
             *
             * Plastic strain.
             *
             */
            elt.total_strain = Vector6::zeros();

            // Compute plastic strain.
            for a in 0..4 {
                let bn = elt.local_j_inv[(0, a)];
                let cn = elt.local_j_inv[(1, a)];
                let dn = elt.local_j_inv[(2, a)];

                let ia = elt.indices[a];
                let vel_part = self.velocities.fixed_rows::<U3>(ia);
                let pos_part = self.positions.fixed_rows::<U3>(ia);
                let ref_pos_part = self.rest_positions.fixed_rows::<U3>(ia);
                let dpos = elt.inv_rot * (vel_part * dt + pos_part) - ref_pos_part;
                // total_strain += B_n * dpos
                elt.total_strain += Vector6::new(
                    bn * dpos.x,
                    cn * dpos.y,
                    dn * dpos.z,
                    cn * dpos.x + bn * dpos.y,
                    dn * dpos.x + bn * dpos.z,
                    dn * dpos.y + cn * dpos.z
                );
            }

            let strain = elt.total_strain - elt.plastic_strain;
            if strain.norm() > self.plasticity_threshold {
                let coeff = params.dt * (N::one() / params.dt).min(self.plasticity_creep);
                elt.plastic_strain += strain * coeff;
            }

            if let Some((dir, magnitude)) = Unit::try_new_and_get(elt.plastic_strain, N::zero()) {
                if magnitude > self.plasticity_max_force {
                    elt.plastic_strain = *dir * self.plasticity_max_force;
                }
            }

            for a in 0..4 {
                let ia = elt.indices[a];

                if !self.kinematic_nodes[ia / DIM] {
                    let bn = elt.local_j_inv[(0, a)];
                    let cn = elt.local_j_inv[(1, a)];
                    let dn = elt.local_j_inv[(2, a)];

                    // Fields of P_n * elt.volume:
                    //
                    // let P_n = Matrix3x6::new(
                    //     bn * d0, bn * d1, bn * d1, cn * d2, dn * d2, _0,
                    //     cn * d1, cn * d0, cn * d1, bn * d2, _0, dn * d2,
                    //     dn * d1, dn * d1, dn * d0, _0, bn * d2, cn * d2
                    // ) * elt.volume;
                    let bn0 = bn * d0_vol;
                    let bn1 = bn * d1_vol;
                    let bn2 = bn * d2_vol;
                    let cn0 = cn * d0_vol;
                    let cn1 = cn * d1_vol;
                    let cn2 = cn * d2_vol;
                    let dn0 = dn * d0_vol;
                    let dn1 = dn * d1_vol;
                    let dn2 = dn * d2_vol;

                    // P_n * strain
                    let strain = elt.total_strain - elt.plastic_strain;
                    #[cfg_attr(rustfmt, rustfmt_skip)]
                    let projected_strain = Vector3::new(
                        bn0 * strain.x + bn1 * strain.y + bn1 * strain.z + cn2 * strain.w + dn2 * strain.a,
                        cn1 * strain.x + cn0 * strain.y + cn1 * strain.z + bn2 * strain.w +              dn2 * strain.b,
                        dn1 * strain.x + dn1 * strain.y + dn0 * strain.z +              bn2 * strain.a + cn2 * strain.b,
                    );

                    let mut force_part = self.accelerations.fixed_rows_mut::<U3>(ia);
                    force_part -= elt.rot * projected_strain;
                }
            }
        }
    }

    /// Returns the triangles at the boundary of this volume.
    ///
    /// Each element of the returned `Vec` is a tuple containing the 3 indices of the triangle
    /// vertices, and the index of the corresponding tetrahedral element.
    pub fn boundary(&self) -> Vec<(Point3<usize>, usize)> {
        fn key(a: usize, b: usize, c: usize) -> (usize, usize, usize) {
            let (sa, sb, sc) = utils::sort3(&a, &b, &c);
            (*sa, *sb, *sc)
        }

        let mut faces = HashMap::with_hasher(DeterministicState::new());

        for (i, elt) in self.elements.iter().enumerate() {
            let k1 = key(elt.indices.x, elt.indices.y, elt.indices.z);
            let k2 = key(elt.indices.y, elt.indices.z, elt.indices.w);
            let k3 = key(elt.indices.z, elt.indices.w, elt.indices.x);
            let k4 = key(elt.indices.w, elt.indices.x, elt.indices.y);

            faces.entry(k1).or_insert((0, elt.indices.w, i)).0.add_assign(1);
            faces.entry(k2).or_insert((0, elt.indices.x, i)).0.add_assign(1);
            faces.entry(k3).or_insert((0, elt.indices.y, i)).0.add_assign(1);
            faces.entry(k4).or_insert((0, elt.indices.z, i)).0.add_assign(1);
        }

        let boundary = faces.iter().filter_map(|(k, n)| {
            if n.0 == 1 {
                // Ensure the triangle has an outward normal.
                // FIXME: there is a much more efficient way of doing this, given the
                // tetrahedrons orientations and the face.
                let a = self.positions.fixed_rows::<U3>(k.0);
                let b = self.positions.fixed_rows::<U3>(k.1);
                let c = self.positions.fixed_rows::<U3>(k.2);
                let d = self.positions.fixed_rows::<U3>(n.1);

                let ab = b - a;
                let ac = c - a;
                let ad = d - a;

                if ab.cross(&ac).dot(&ad) < N::zero() {
                    Some((Point3::new(k.0, k.1, k.2), n.2))
                } else {
                    Some((Point3::new(k.0, k.2, k.1), n.2))
                }
            } else {
                None
            }
        }).collect();

        boundary
    }

    /// Returns a triangle mesh at the boundary of this volume as well as a mapping between the mesh
    /// vertices and this volume degrees of freedom and the mapping between the mesh triangles and
    /// this volume body parts (the tetrahedral elements).
    ///
    /// The output is (triangle mesh, deformation indices, element to body part map).
    pub fn boundary_mesh(&self) -> (TriMesh<N>, Vec<usize>, Vec<usize>) {
        const INVALID: usize = usize::max_value();
        let mut deformation_indices = Vec::new();
        let mut indices = self.boundary();
        let mut idx_remap: Vec<usize> = iter::repeat(INVALID).take(self.positions.len() / 3).collect();
        let mut vertices = Vec::new();

        for (idx, _) in &mut indices {
            for i in 0..3 {
                let idx_i = &mut idx[i];
                if idx_remap[*idx_i / 3] == INVALID {
                    let new_id = vertices.len();
                    vertices.push(Point3::new(
                        self.positions[*idx_i + 0],
                        self.positions[*idx_i + 1],
                        self.positions[*idx_i + 2])
                    );
                    deformation_indices.push(*idx_i);
                    idx_remap[*idx_i / 3] = new_id;
                    *idx_i = new_id;
                } else {
                    *idx_i = idx_remap[*idx_i / 3];
                }
            }
        }

        let body_parts = indices.iter().map(|i| i.1).collect();
//        println!("Number of trimesh triangles: {}", indices.len());
        let indices = indices.into_iter().map(|i| i.0).collect();

        (TriMesh::new(vertices, indices, None), deformation_indices, body_parts)
    }

    /// Renumber degrees of freedom so that the `deformation_indices[i]`-th DOF becomes the `i`-th one.
    pub fn renumber_dofs(&mut self, deformation_indices: &[usize]) {
        let mut dof_map: Vec<_> = (0..).take(self.positions.len()).collect();
        let mut remapped: Vec<_> = iter::repeat(false).take(self.positions.len()).collect();
        let mut new_positions = DVector::zeros(self.positions.len());
        let mut new_rest_positions = DVector::zeros(self.positions.len());

        for (target_i, orig_i) in deformation_indices.iter().cloned().enumerate() {
            assert!(!remapped[orig_i], "Duplicate DOF remapping found.");
            let target_i = target_i * 3;
            new_positions.fixed_rows_mut::<U3>(target_i).copy_from(&self.positions.fixed_rows::<U3>(orig_i));
            new_rest_positions.fixed_rows_mut::<U3>(target_i).copy_from(&self.rest_positions.fixed_rows::<U3>(orig_i));
            dof_map[orig_i] = target_i;
            remapped[orig_i] = true;
        }

        let mut curr_target = deformation_indices.len() * 3;

        for orig_i in (0..self.positions.len()).step_by(3) {
            if !remapped[orig_i] {
                new_positions.fixed_rows_mut::<U3>(curr_target).copy_from(&self.positions.fixed_rows::<U3>(orig_i));
                new_rest_positions.fixed_rows_mut::<U3>(curr_target).copy_from(&self.rest_positions.fixed_rows::<U3>(orig_i));
                dof_map[orig_i] = curr_target;
                curr_target += 3;
            }
        }

        for elt in &mut self.elements {
            elt.indices.coords.apply(|i| dof_map[i]);
        }

        self.positions = new_positions;
        self.rest_positions = new_rest_positions;
    }

// FIXME: add a method to apply a transformation to the whole volume.

    /// Constructs an axis-aligned cube with regular subdivisions along each axis.
    ///
    /// The cube is subdivided `nx` (resp. `ny` and `nz`) times along
    /// the `x` (resp. `y` and `z`) axis.
    pub fn cube(handle: BodyHandle, pos: &Isometry3<N>, extents: &Vector3<N>, nx: usize, ny: usize, nz: usize, density: N, young_modulus: N, poisson_ratio: N, damping_coeffs: (N, N)) -> Self {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();

        // First, generate the vertices.
        let x_step: N = na::convert(1.0 / nx as f64);
        let y_step: N = na::convert(1.0 / ny as f64);
        let z_step: N = na::convert(1.0 / nz as f64);

        for i in 0..=nx {
            let x = x_step * na::convert(i as f64) - na::convert(0.5);

            for j in 0..=ny {
                let y = y_step * na::convert(j as f64) - na::convert(0.5);

                for k in 0..=nz {
                    let z = z_step * na::convert(k as f64) - na::convert(0.5);
                    vertices.push(Point3::new(x, y, z))
                }
            }
        }

        // Second, generate indices.
        for i in 0..nx {
            for j in 0..ny {
                for k in 0..nz {
                    // See https://www.ics.uci.edu/~eppstein/projects/tetra/
                    // for the 5-elements tetrahedral decomposition where local
                    // cube indices are as follows:
                    //
                    //     4 o----------o 7
                    //       | 5 o----------o 6
                    //       |   |      ·   |                y
                    //       |   |      ·   |                ^
                    //     0 o---|· · · · 3 |                |
                    //           o----------o                 --> x
                    //           1          2
                    /* Local cubic indices:
                    let a = Point4::new(0, 1, 2, 5);
                    let b = Point4::new(2, 5, 6, 7);
                    let c = Point4::new(2, 7, 3, 0);
                    let d = Point4::new(7, 4, 0, 5);
                    let e = Point4::new(0, 2, 7, 5);
                    */
                    fn shift(ny: usize, nz: usize, di: usize, dj: usize, dk: usize) -> usize {
                        ((di * (ny + 1) + dj) * (nz + 1)) + dk
                    }
                    // _0 = node at (i, j, k)
                    let _0 = (i * (ny + 1) + j) * (nz + 1) + k;
                    let _1 = _0 + shift(ny, nz, 0, 0, 1);
                    let _2 = _0 + shift(ny, nz, 1, 0, 1);
                    let _3 = _0 + shift(ny, nz, 1, 0, 0);
                    let _4 = _0 + shift(ny, nz, 0, 1, 0);
                    let _5 = _0 + shift(ny, nz, 0, 1, 1);
                    let _6 = _0 + shift(ny, nz, 1, 1, 1);
                    let _7 = _0 + shift(ny, nz, 1, 1, 0);

                    if (i % 2) == 0 && ((j % 2) == (k % 2)) ||
                        (i % 2) == 1 && ((j % 2) != (k % 2)) {
                        indices.push(Point4::new(_0, _1, _2, _5));
                        indices.push(Point4::new(_2, _5, _6, _7));
                        indices.push(Point4::new(_2, _7, _3, _0));
                        indices.push(Point4::new(_7, _4, _0, _5));
                        indices.push(Point4::new(_0, _2, _7, _5));
                    } else {
                        indices.push(Point4::new(_4, _6, _5, _1));
                        indices.push(Point4::new(_6, _2, _1, _3));
                        indices.push(Point4::new(_6, _7, _3, _4));
                        indices.push(Point4::new(_3, _4, _0, _1));
                        indices.push(Point4::new(_4, _3, _6, _1));
                    }
                }
            }
        }

        Self::new(handle, &vertices, &indices, pos, extents, density, young_modulus, poisson_ratio, damping_coeffs)
    }

    /// Restrict the specified node acceleration to always be zero so
    /// it can be controlled manually by the user at the velocity level.
    pub fn set_node_kinematic(&mut self, i: usize, is_kinematic: bool) {
        assert!(i < self.positions.len() / DIM, "Node index out of bounds.");
        self.update_status.set_status_changed(true);
        self.update_status.set_local_inertia_changed(true);
        self.kinematic_nodes[i] = is_kinematic;
    }

    /// Mark all nodes as non-kinematic.
    pub fn clear_kinematic_nodes(&mut self) {
        self.update_status.set_status_changed(true);
        self.update_status.set_local_inertia_changed(true);
        self.kinematic_nodes.fill(false)
    }
}

impl<N: Real> Body<N> for FEMVolume<N> {
    #[inline]
    fn gravity_enabled(&self) -> bool {
        self.gravity_enabled
    }

    #[inline]
    fn enable_gravity(&mut self, enabled: bool) {
        self.gravity_enabled = enabled
    }

    #[inline]
    fn deformed_positions(&self) -> Option<(DeformationsType, &[N])> {
        Some((DeformationsType::Vectors, self.positions.as_slice()))
    }

    #[inline]
    fn deformed_positions_mut(&mut self) -> Option<(DeformationsType, &mut [N])> {
        self.update_status.set_position_changed(true);
        Some((DeformationsType::Vectors, self.positions.as_mut_slice()))
    }

    fn update_kinematics(&mut self) {
        if !self.update_status.position_changed() {
            return;
        }

        for elt in &mut self.elements {
            let a = self.positions.fixed_rows::<U3>(elt.indices.x);
            let b = self.positions.fixed_rows::<U3>(elt.indices.y);
            let c = self.positions.fixed_rows::<U3>(elt.indices.z);
            let d = self.positions.fixed_rows::<U3>(elt.indices.w);

            let ab = b - a;
            let ac = c - a;
            let ad = d - a;

            elt.j = Matrix3::new(
                ab.x, ab.y, ab.z,
                ac.x, ac.y, ac.z,
                ad.x, ad.y, ad.z,
            );
            let j_det =  elt.j.determinant();
            elt.volume = j_det * na::convert(1.0 / 6.0);
            elt.com = Point3::from_coordinates(a + b + c + d) * na::convert::<_, N>(1.0 / 4.0);

            /*
             * Compute the orientation.
             */
            // Undeformed points.
            let rest_a = self.rest_positions.fixed_rows::<U3>(elt.indices.x);
            let rest_b = self.rest_positions.fixed_rows::<U3>(elt.indices.y);
            let rest_c = self.rest_positions.fixed_rows::<U3>(elt.indices.z);
            let rest_d = self.rest_positions.fixed_rows::<U3>(elt.indices.w);

            let rest_ab = rest_b - rest_a;
            let rest_ac = rest_c - rest_a;
            let rest_ad = rest_d - rest_a;

            let coeff = N::one() / j_det; // 1 / (6V)
            let n1 = rest_ac.cross(&rest_ad) * coeff;
            let n2 = rest_ad.cross(&rest_ab) * coeff;
            let n3 = rest_ab.cross(&rest_ac) * coeff;

            // The transformation matrix from which we can get the rotation component.
            let g = (Matrix3::from_columns(&[n1, n2, n3]) * elt.j).transpose();
            // FIXME: optimize this.
            let mut cols = [
                g.column(0).into_owned(),
                g.column(1).into_owned(),
                g.column(2).into_owned(),
            ];
            let _ = Vector3::orthonormalize(&mut cols);
            elt.rot = Rotation3::from_matrix_unchecked(Matrix3::from_columns(&cols));
            elt.inv_rot = elt.rot.inverse();


            let j_inv = elt.j.try_inverse().unwrap_or(Matrix3::identity());
            let local_j_inv = elt.inv_rot.matrix() * j_inv;
            elt.local_j_inv = Matrix3x4::new(
                -local_j_inv.m11 - local_j_inv.m12 - local_j_inv.m13, local_j_inv.m11, local_j_inv.m12, local_j_inv.m13,
                -local_j_inv.m21 - local_j_inv.m22 - local_j_inv.m23, local_j_inv.m21, local_j_inv.m22, local_j_inv.m23,
                -local_j_inv.m31 - local_j_inv.m32 - local_j_inv.m33, local_j_inv.m31, local_j_inv.m32, local_j_inv.m33,
            );
        }
    }

    /// Update the dynamics property of this deformable volume.
    fn update_dynamics(&mut self, dt: N) {
        if self.update_status.inertia_needs_update() {
            self.augmented_mass.fill(N::zero());
            self.assemble_mass_with_damping(dt);
            self.assemble_stiffness(dt);

            // FIXME: avoid allocation inside Cholesky at each timestep.
            // FIXME: if Cholesky fails fallback to some sort of mass-spring formulation?
            //        If we do so we should add a bool to let give the user the ability to check which
            //        model has been used during the last timestep.
            self.inv_augmented_mass = Cholesky::new(self.augmented_mass.clone()).expect("Singular system found.");
        }

    }

    fn update_acceleration(&mut self, gravity: &Vector3<N>, params: &IntegrationParameters<N>) {
        self.accelerations.fill(N::zero());
        self.assemble_forces(gravity, params);
        self.inv_augmented_mass.solve_mut(&mut self.accelerations);
    }

    fn clear_forces(&mut self) {
        self.forces.fill(N::zero())
    }

    fn clear_update_flags(&mut self) {
        self.update_status.clear()
    }

    fn update_status(&self) -> BodyUpdateStatus {
        self.update_status
    }

    fn apply_displacement(&mut self, disp: &[N]) {
        self.update_status.set_position_changed(true);
        let disp = DVectorSlice::from_slice(disp, self.positions.len());
        self.positions += disp;
    }

    fn handle(&self) -> BodyHandle {
        self.handle
    }

    fn status(&self) -> BodyStatus {
        self.status
    }

    fn set_status(&mut self, status: BodyStatus) {
        self.update_status.set_status_changed(true);
        self.status = status
    }

    fn activation_status(&self) -> &ActivationStatus<N> {
        &self.activation
    }

    fn ndofs(&self) -> usize {
        self.positions.len()
    }

    fn generalized_acceleration(&self) -> DVectorSlice<N> {
        DVectorSlice::from_slice(self.accelerations.as_slice(), self.accelerations.len())
    }

    fn generalized_velocity(&self) -> DVectorSlice<N> {
        DVectorSlice::from_slice(self.velocities.as_slice(), self.velocities.len())
    }

    fn companion_id(&self) -> usize {
        self.companion_id
    }

    fn set_companion_id(&mut self, id: usize) {
        self.companion_id = id
    }

    fn generalized_velocity_mut(&mut self) -> DVectorSliceMut<N> {
        self.update_status.set_velocity_changed(true);
        let ndofs = self.velocities.len();
        DVectorSliceMut::from_slice(self.velocities.as_mut_slice(), ndofs)
    }

    fn integrate(&mut self, params: &IntegrationParameters<N>) {
        self.update_status.set_position_changed(true);
        self.positions.axpy(params.dt, &self.velocities, N::one())
    }

    fn activate_with_energy(&mut self, energy: N) {
        self.activation.set_energy(energy)
    }

    fn deactivate(&mut self) {
        self.update_status.set_velocity_changed(true);
        self.activation.set_energy(N::zero());
        self.velocities.fill(N::zero());
    }

    fn set_deactivation_threshold(&mut self, threshold: Option<N>) {
        self.activation.set_deactivation_threshold(threshold)
    }

    fn part(&self, id: usize) -> Option<&BodyPart<N>> {
        self.elements.get(id).map(|e| e as &BodyPart<N>)
    }

    fn world_point_at_material_point(&self, part: &BodyPart<N>, point: &Point3<N>) -> Point3<N> {
        let elt = part.downcast_ref::<TetrahedralElement<N>>().expect("The provided body part must be tetrahedral element");
        fem_helper::world_point_at_material_point(FiniteElementIndices::Tetrahedron(elt.indices), &self.positions, point)
    }

    fn position_at_material_point(&self, part: &BodyPart<N>, point: &Point3<N>) -> Isometry3<N> {
        let elt = part.downcast_ref::<TetrahedralElement<N>>().expect("The provided body part must be a tetrahedral element");
        let pt = fem_helper::world_point_at_material_point(FiniteElementIndices::Tetrahedron(elt.indices), &self.positions, point);
        Isometry3::from_parts(Translation3::from_vector(pt.coords), na::one())
    }

    fn material_point_at_world_point(&self, part: &BodyPart<N>, point: &Point3<N>) -> Point3<N> {
        let elt = part.downcast_ref::<TetrahedralElement<N>>().expect("The provided body part must be tetrahedral element");
        fem_helper::material_point_at_world_point(FiniteElementIndices::Tetrahedron(elt.indices), &self.positions, point)
    }

    fn fill_constraint_geometry(
        &self,
        part: &BodyPart<N>,
        _: usize, // FIXME: keep this parameter?
        center: &Point3<N>,
        force_dir: &ForceDirection<N>,
        j_id: usize,
        wj_id: usize,
        jacobians: &mut [N],
        inv_r: &mut N,
        ext_vels: Option<&DVectorSlice<N>>,
        out_vel: Option<&mut N>
    ) {
        let elt = part.downcast_ref::<TetrahedralElement<N>>().expect("The provided body part must be a tetrahedral element");
        fem_helper::fill_contact_geometry_fem(
            self.ndofs(),
            self.status,
            FiniteElementIndices::Tetrahedron(elt.indices),
            &self.positions,
            &self.velocities,
            &self.kinematic_nodes,
            Either::Right(&self.inv_augmented_mass),
            center,
            force_dir,
            j_id,
            wj_id,
            jacobians,
            inv_r,
            ext_vels,
            out_vel
        );
    }


    #[inline]
    fn has_active_internal_constraints(&mut self) -> bool {
        false
    }

    #[inline]
    fn setup_internal_velocity_constraints(&mut self, _: &mut DVectorSliceMut<N>, _: &IntegrationParameters<N>) {}

    #[inline]
    fn step_solve_internal_velocity_constraints(&mut self, _: &mut DVectorSliceMut<N>) {}

    #[inline]
    fn step_solve_internal_position_constraints(&mut self, _: &IntegrationParameters<N>) {}


    fn apply_force_at_local_point(&mut self, part_id: usize, force: &Vector3<N>, point: &Point3<N>, force_type: ForceType, auto_wake_up: bool) {
        if self.status != BodyStatus::Dynamic {
            return;
        }

        if auto_wake_up {
            self.activate()
        }


        let element = &self.elements[part_id];
        let forces = [
            force * (N::one() - point.x - point.y - point.z),
            force * point.x,
            force * point.y,
            force * point.z,
        ];

        match force_type {
            ForceType::Force => {
                for i in 0..4 {
                    if !self.kinematic_nodes[element.indices[i] / DIM] {
                        self.forces.fixed_rows_mut::<U3>(element.indices[i]).add_assign(forces[i]);
                    }
                }
            }
            ForceType::Impulse => {
                let dvel = &mut self.workspace;
                dvel.fill(N::zero());
                for i in 0..4 {
                    if !self.kinematic_nodes[element.indices[i] / DIM] {
                        dvel.fixed_rows_mut::<U3>(element.indices[i]).copy_from(&forces[i]);
                    }
                }
                self.inv_augmented_mass.solve_mut(dvel);
                self.velocities += &*dvel;
            }
            ForceType::AccelerationChange => {
                let acceleration = &mut self.workspace;
                acceleration.fill(N::zero());
                for i in 0..4 {
                    if !self.kinematic_nodes[element.indices[i] / DIM] {
                        acceleration.fixed_rows_mut::<U3>(element.indices[i]).copy_from(&forces[i]);
                    }
                }
                self.forces.gemv(N::one(), &self.augmented_mass, acceleration, N::one())
            }
            ForceType::VelocityChange => {
                for i in 0..4 {
                    if !self.kinematic_nodes[element.indices[i] / DIM] {
                        self.velocities.fixed_rows_mut::<U3>(element.indices[i]).add_assign(forces[i]);
                    }
                }
            }
        }
    }

    fn apply_force(&mut self, part_id: usize, force: &Force<N>, force_type: ForceType, auto_wake_up: bool) {
        let _1_4: N = na::convert(1.0 / 4.0);
        let barycenter = Point3::new(_1_4, _1_4, _1_4);
        self.apply_force_at_local_point(part_id, &force.linear, &barycenter, force_type, auto_wake_up)
    }

    fn apply_local_force(&mut self, part_id: usize, force: &Force<N>, force_type: ForceType, auto_wake_up: bool) {
        let world_force = Force::new(
            self.elements[part_id].rot * force.linear,
            self.elements[part_id].rot * force.angular);
        self.apply_force(part_id, &world_force, force_type, auto_wake_up);
    }

    fn apply_force_at_point(&mut self, part_id: usize, force: &Vector3<N>, point: &Point3<N>, force_type: ForceType, auto_wake_up: bool) {
        let local_point = self.material_point_at_world_point(&self.elements[part_id], point);
        self.apply_force_at_local_point(part_id, &force, &local_point, force_type, auto_wake_up)
    }

    fn apply_local_force_at_point(&mut self, part_id: usize, force: &Vector3<N>, point: &Point3<N>, force_type: ForceType, auto_wake_up: bool) {
        let world_force = self.elements[part_id].rot * force;
        let local_point = self.material_point_at_world_point(&self.elements[part_id], point);
        self.apply_force_at_local_point(part_id, &world_force, &local_point, force_type, auto_wake_up);
    }


    fn apply_local_force_at_local_point(&mut self, part_id: usize, force: &Vector3<N>, point: &Point3<N>, force_type: ForceType, auto_wake_up: bool) {
        let world_force = self.elements[part_id].rot * force;
        self.apply_force_at_local_point(part_id, &world_force, &point, force_type, auto_wake_up);
    }
}


impl<N: Real> BodyPart<N> for TetrahedralElement<N> {
    fn part_handle(&self) -> BodyPartHandle {
        self.handle
    }

    fn center_of_mass(&self) -> Point3<N> {
        self.com
    }

    fn position(&self) -> Isometry3<N> {
        Isometry3::new(self.com.coords, na::zero())
    }

    fn velocity(&self) -> Velocity<N> {
        unimplemented!()
    }

    fn inertia(&self) -> Inertia<N> {
        Inertia::new(self.volume * self.density, Matrix3::identity())
    }

    fn local_inertia(&self) -> Inertia<N> {
        Inertia::new(self.volume * self.density, Matrix3::identity())
    }
}

enum FEMVolumeDescGeometry<'a, N: Real> {
    Cube(usize, usize, usize),
    Tetrahedrons(&'a [Point3<N>], &'a [Point4<usize>])
}

pub struct FEMVolumeDesc<'a, N: Real> {
    gravity_enabled: bool,
    geom: FEMVolumeDescGeometry<'a, N>,
    scale: Vector3<N>,
    position: Isometry3<N>,
    young_modulus: N,
    poisson_ratio: N,
    sleep_threshold: Option<N>,
    boundary_trimesh_collider_enabled: bool,
    mass_damping: N,
    stiffness_damping: N,
    density: N,
    plasticity: (N, N, N),
    kinematic_nodes: Vec<usize>,
    status: BodyStatus
}

impl<'a, N: Real> FEMVolumeDesc<'a, N> {
    fn with_geometry(geom: FEMVolumeDescGeometry<'a, N>) -> Self {
        FEMVolumeDesc {
            gravity_enabled: true,
            geom,
            scale: Vector3::repeat(N::one()),
            position: Isometry3::identity(),
            young_modulus: na::convert(0.3),
            poisson_ratio: N::zero(),
            sleep_threshold: Some(ActivationStatus::default_threshold()),
            boundary_trimesh_collider_enabled: false,
            mass_damping: na::convert(0.2),
            stiffness_damping: N::zero(),
            density: N::one(),
            plasticity: (N::zero(), N::zero(), N::zero()),
            kinematic_nodes: Vec::new(),
            status: BodyStatus::Dynamic
        }
    }

    pub fn new(vertices: &'a [Point3<N>], tetrahedrons: &'a [Point4<usize>]) -> Self {
        Self::with_geometry(FEMVolumeDescGeometry::Tetrahedrons(vertices, tetrahedrons))
    }

    pub fn cube(subdiv_x: usize, subdiv_y: usize, subdiv_z: usize) -> Self {
        Self::with_geometry(FEMVolumeDescGeometry::Cube(subdiv_x, subdiv_y, subdiv_z))
    }

    pub fn clear_kinematic_nodes(&mut self) -> &mut Self {
        self.kinematic_nodes.clear();
        self
    }

    desc_custom_setters!(
        self.with_boundary_trimesh_collider, set_boundary_trimesh_collider_enabled, enable: bool | { self.boundary_trimesh_collider_enabled = enable }
        self.with_plasticity, set_plasticity, strain_threshold: N, creep: N, max_force: N | { self.plasticity = (strain_threshold, creep, max_force) }
        self.with_kinematic_nodes, set_kinematic_nodes, nodes: &[usize] | { self.kinematic_nodes.extend_from_slice(nodes) }
        self.with_translation, set_translation, vector: Vector3<N> | { self.position.translation.vector = vector }
    );

    desc_setters!(
        with_gravity_enabled, enable_gravity, gravity_enabled: bool
        with_scale, set_scale, scale: Vector3<N>
        with_young_modulus, set_young_modulus, young_modulus: N
        with_poisson_ratio, set_poisson_ratio, poisson_ratio: N
        with_sleep_threshold, set_sleep_threshold, sleep_threshold: Option<N>
        with_mass_damping, set_mass_damping, mass_damping: N
        with_stiffness_damping, set_stiffness_damping, stiffness_damping: N
        with_density, set_density, density: N
        with_status, set_status, status: BodyStatus
        with_position, set_position, position: Isometry3<N>
    );

    desc_custom_getters!(
        self.plasticity_strain_threshold: N | { self.plasticity.0 }
        self.plasticity_creep: N | { self.plasticity.1 }
        self.plasticity_max_force: N | { self.plasticity.2 }
        self.kinematic_nodes: &[usize] | { &self.kinematic_nodes[..] }
        self.translation: &Vector3<N> | { &self.position.translation.vector }
    );

    desc_getters!(
        [val] gravity_enabled: bool
        [val] young_modulus: N
        [val] poisson_ratio: N
        [val] sleep_threshold: Option<N>
        [val] mass_damping: N
        [val] stiffness_damping: N
        [val] density: N
        [val] status: BodyStatus
        [val] boundary_trimesh_collider_enabled: bool
        [ref] position: Isometry3<N>
        [ref] scale: Vector3<N>
    );

    pub fn build<'w>(&self, world: &'w mut World<N>) -> &'w FEMVolume<N> {
        world.add_body(self)
    }
}

impl<'a, N: Real> BodyDesc<N> for FEMVolumeDesc<'a, N> {
    type Body = FEMVolume<N>;

    fn build_with_handle(&self, cworld: &mut ColliderWorld<N>, handle: BodyHandle) -> FEMVolume<N> {
        let mut vol = match self.geom {
            FEMVolumeDescGeometry::Cube(nx, ny, nz) =>
                FEMVolume::cube(handle, &self.position, &self.scale,
                                       nx, ny, nz, self.density, self.young_modulus,
                                       self.poisson_ratio,
                                       (self.mass_damping, self.stiffness_damping)),
            FEMVolumeDescGeometry::Tetrahedrons(pts, idx) =>
                FEMVolume::new(handle, pts, idx, &self.position, &self.scale,
                                      self.density, self.young_modulus, self.poisson_ratio,
                                      (self.mass_damping, self.stiffness_damping))
        };

        vol.set_deactivation_threshold(self.sleep_threshold);
        vol.set_plasticity(self.plasticity.0, self.plasticity.1, self.plasticity.2);
        vol.enable_gravity(self.gravity_enabled);

        for i in &self.kinematic_nodes {
            vol.set_node_kinematic(*i, true)
        }

        if self.boundary_trimesh_collider_enabled {
            let (mesh, ids_map, parts_map) = vol.boundary_mesh();
            vol.renumber_dofs(&ids_map);
            let _ = DeformableColliderDesc::new(ShapeHandle::new(mesh))
                .with_body_parts_mapping(Some(Arc::new(parts_map)))
                .build_with_infos(&vol, cworld);
        }

        vol
    }
}