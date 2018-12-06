use either::Either;

use na::{Real, Cholesky, Dynamic, DVectorSliceMut, VectorSliceMutN, Point2, Point3, Point4, DVector, DVectorSlice};
#[cfg(feature = "dim3")]
use na::Matrix3;
use ncollide::shape::{Segment, Triangle};
use ncollide::query::PointQueryWithLocation;
#[cfg(feature = "dim3")]
use ncollide::shape::Tetrahedron;

use crate::object::BodyStatus;
use crate::solver::ForceDirection;
use crate::math::{Point, Isometry, Dim, DIM};


/// Indices of the nodes of on element of a body decomposed in finite elements.
#[derive(Copy, Clone, Debug)]
pub enum FiniteElementIndices {
    #[cfg(feature = "dim3")]
    /// A tetrahedral element.
    Tetrahedron(Point4<usize>),
    /// A triangular element.
    Triangle(Point3<usize>),
    /// A segment element.
    Segment(Point2<usize>)
}


#[inline]
pub fn world_point_at_material_point<N: Real>(indices: FiniteElementIndices, positions: &DVector<N>, point: &Point<N>) -> Point<N> {
    match indices {
        FiniteElementIndices::Segment(indices) => {
            let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
            let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
            Point::from_coordinates(a * (N::one() - point.x) + b * point.x)
        }
        FiniteElementIndices::Triangle(indices) => {
            let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
            let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
            let c = positions.fixed_rows::<Dim>(indices.z).into_owned();
            Point::from_coordinates(a * (N::one() - point.x - point.y) + b * point.x + c * point.y)
        }
        #[cfg(feature = "dim3")]
        FiniteElementIndices::Tetrahedron(indices) => {
            let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
            let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
            let c = positions.fixed_rows::<Dim>(indices.z).into_owned();
            let d = positions.fixed_rows::<Dim>(indices.w).into_owned();
            Point::from_coordinates(a * (N::one() - point.x - point.y - point.z) + b * point.x + c * point.y + d * point.z)
        }
    }
}


#[inline]
pub fn material_point_at_world_point<N: Real>(indices: FiniteElementIndices, positions: &DVector<N>, point: &Point<N>) -> Point<N> {
    match indices {
        FiniteElementIndices::Segment(indices) => {
            let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
            let b = positions.fixed_rows::<Dim>(indices.y).into_owned();

            let seg = Segment::new(
                Point::from_coordinates(a),
                Point::from_coordinates(b),
            );

            // FIXME: do we really want to project here? Even in 2D?
            let proj = seg.project_point_with_location(&Isometry::identity(), point, false).1;
            let bcoords = proj.barycentric_coordinates();

            let mut res = Point::origin();
            res.x = bcoords[1];
            res
        }
        FiniteElementIndices::Triangle(indices) => {
            let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
            let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
            let c = positions.fixed_rows::<Dim>(indices.z).into_owned();

            let tri = Triangle::new(
                Point::from_coordinates(a),
                Point::from_coordinates(b),
                Point::from_coordinates(c),
            );

            // FIXME: do we really want to project here? Even in 2D?
            let proj = tri.project_point_with_location(&Isometry::identity(), point, false).1;
            let bcoords = proj.barycentric_coordinates().unwrap();

            let mut res = Point::origin();
            res.x = bcoords[1];
            res.y = bcoords[2];
            res
        }
        #[cfg(feature = "dim3")]
        FiniteElementIndices::Tetrahedron(indices) => {
            let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
            let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
            let c = positions.fixed_rows::<Dim>(indices.z).into_owned();
            let d = positions.fixed_rows::<Dim>(indices.w).into_owned();

            let tetra = Tetrahedron::new(
                Point3::from_coordinates(a),
                Point3::from_coordinates(b),
                Point3::from_coordinates(c),
                Point3::from_coordinates(d),
            );

            // FIXME: what to do if this returns `None`?
            let bcoords = tetra.barycentric_coordinates(point).unwrap_or([N::zero(); 4]);
            Point3::new(bcoords[1], bcoords[2], bcoords[3])
        }
    }
}

#[inline]
pub fn fill_contact_geometry_fem<N: Real>(
    ndofs: usize,
    status: BodyStatus,
    indices: FiniteElementIndices,
    positions: &DVector<N>,
    velocities: &DVector<N>,
    kinematic_nodes: &DVector<bool>,
    inv_augmented_mass: Either<N, &Cholesky<N, Dynamic>>,
    // Original parameters of fill_contact_geometry.
    center: &Point<N>,
    force_dir: &ForceDirection<N>,
    j_id: usize,
    wj_id: usize,
    jacobians: &mut [N],
    inv_r: &mut N,
    ext_vels: Option<&DVectorSlice<N>>,
    out_vel: Option<&mut N>
) {
    if status == BodyStatus::Static || status == BodyStatus::Disabled {
        return;
    }

    // Needed by the non-linear SOR-prox.
    // FIXME: should this `fill` be done by the non-linear SOR-prox itself?
    DVectorSliceMut::from_slice(&mut jacobians[j_id..], ndofs).fill(N::zero());

    if let ForceDirection::Linear(dir) = force_dir {
        match indices {
            FiniteElementIndices::Segment(indices) => {
                let kinematic1 = kinematic_nodes[indices.x / DIM];
                let kinematic2 = kinematic_nodes[indices.y / DIM];

                let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
                let b = positions.fixed_rows::<Dim>(indices.y).into_owned();

                let seg = Segment::new(
                    Point::from_coordinates(a),
                    Point::from_coordinates(b),
                );

                // FIXME: This is costly!
                let proj = seg.project_point_with_location(&Isometry::identity(), center, false).1;
                let bcoords = proj.barycentric_coordinates();

                let dir1 = **dir * bcoords[0];
                let dir2 = **dir * bcoords[1];

                if status == BodyStatus::Dynamic {
                    if !kinematic1 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.x..]).copy_from(&dir1);
                    }
                    if !kinematic2 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.y..]).copy_from(&dir2);
                    }
                }

                if let Some(out_vel) = out_vel {
                    let va = velocities.fixed_rows::<Dim>(indices.x);
                    let vb = velocities.fixed_rows::<Dim>(indices.y);

                    *out_vel += va.dot(&dir1) + vb.dot(&dir2);

                    if let Some(ext_vels) = ext_vels {
                        if !kinematic1 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.x).dot(&dir1);
                        }
                        if !kinematic2 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.y).dot(&dir2);
                        }
                    }
                }
            }
            FiniteElementIndices::Triangle(indices) => {
                let kinematic1 = kinematic_nodes[indices.x / DIM];
                let kinematic2 = kinematic_nodes[indices.y / DIM];
                let kinematic3 = kinematic_nodes[indices.z / DIM];

                let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
                let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
                let c = positions.fixed_rows::<Dim>(indices.z).into_owned();

                let tri = Triangle::new(
                    Point::from_coordinates(a),
                    Point::from_coordinates(b),
                    Point::from_coordinates(c),
                );

                // FIXME: This is costly!
                let proj = tri.project_point_with_location(&Isometry::identity(), center, false).1;
                let bcoords = proj.barycentric_coordinates().unwrap();

                let dir1 = **dir * bcoords[0];
                let dir2 = **dir * bcoords[1];
                let dir3 = **dir * bcoords[2];

                if status == BodyStatus::Dynamic {
                    if !kinematic1 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.x..]).copy_from(&dir1);
                    }
                    if !kinematic2 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.y..]).copy_from(&dir2);
                    }
                    if !kinematic3 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.z..]).copy_from(&dir3);
                    }
                }

                if let Some(out_vel) = out_vel {
                    let va = velocities.fixed_rows::<Dim>(indices.x);
                    let vb = velocities.fixed_rows::<Dim>(indices.y);
                    let vc = velocities.fixed_rows::<Dim>(indices.z);

                    *out_vel += va.dot(&dir1) + vb.dot(&dir2) + vc.dot(&dir3);

                    if let Some(ext_vels) = ext_vels {
                        if !kinematic1 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.x).dot(&dir1);
                        }
                        if !kinematic2 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.y).dot(&dir2);
                        }
                        if !kinematic3 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.z).dot(&dir3);
                        }
                    }
                }
            }
            #[cfg(feature = "dim3")]
            FiniteElementIndices::Tetrahedron(indices) => {
                let kinematic1 = kinematic_nodes[indices.x / DIM];
                let kinematic2 = kinematic_nodes[indices.y / DIM];
                let kinematic3 = kinematic_nodes[indices.z / DIM];
                let kinematic4 = kinematic_nodes[indices.w / DIM];

                let a = positions.fixed_rows::<Dim>(indices.x).into_owned();
                let b = positions.fixed_rows::<Dim>(indices.y).into_owned();
                let c = positions.fixed_rows::<Dim>(indices.z).into_owned();
                let d = positions.fixed_rows::<Dim>(indices.w).into_owned();

                let tetra = Tetrahedron::new(
                    Point3::from_coordinates(a),
                    Point3::from_coordinates(b),
                    Point3::from_coordinates(c),
                    Point3::from_coordinates(d),
                );

                // FIXME: what to do if this returns `None`?
                let bcoords = tetra.barycentric_coordinates(center).unwrap_or([N::zero(); 4]);

                let dir1 = **dir * bcoords[0];
                let dir2 = **dir * bcoords[1];
                let dir3 = **dir * bcoords[2];
                let dir4 = **dir * bcoords[3];

                if status == BodyStatus::Dynamic {
                    if !kinematic1 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.x..]).copy_from(&dir1);
                    }
                    if !kinematic2 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.y..]).copy_from(&dir2);
                    }
                    if !kinematic3 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.z..]).copy_from(&dir3);
                    }
                    if !kinematic4 {
                        VectorSliceMutN::<N, Dim>::from_slice(&mut jacobians[j_id + indices.w..]).copy_from(&dir4);
                    }
                }

                if let Some(out_vel) = out_vel {
                    let va = velocities.fixed_rows::<Dim>(indices.x);
                    let vb = velocities.fixed_rows::<Dim>(indices.y);
                    let vc = velocities.fixed_rows::<Dim>(indices.z);
                    let vd = velocities.fixed_rows::<Dim>(indices.w);

                    *out_vel += va.dot(&dir1) + vb.dot(&dir2) + vc.dot(&dir3) + vd.dot(&dir4);

                    if let Some(ext_vels) = ext_vels {
                        if !kinematic1 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.x).dot(&dir1);
                        }
                        if !kinematic2 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.y).dot(&dir2);
                        }
                        if !kinematic3 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.z).dot(&dir3);
                        }
                        if !kinematic4 {
                            *out_vel += ext_vels.fixed_rows::<Dim>(indices.w).dot(&dir4);
                        }
                    }
                }
            }
        }

        if status == BodyStatus::Dynamic {
            match inv_augmented_mass {
                Either::Right(inv_augmented_mass) => {
                    // FIXME: use a mem::copy_nonoverlapping?
                    for i in 0..ndofs {
                        jacobians[wj_id + i] = jacobians[j_id + i];
                    }

                    inv_augmented_mass.solve_mut(&mut DVectorSliceMut::from_slice(&mut jacobians[wj_id..], ndofs));
                },
                Either::Left(inv_augmented_mass) => {
                    for i in 0..ndofs {
                        jacobians[wj_id + i] = jacobians[j_id + i] * inv_augmented_mass;
                    }
                }
            }

            // FIXME: optimize this because j is sparse.
            *inv_r += DVectorSlice::from_slice(&jacobians[j_id..], ndofs).dot(&DVectorSlice::from_slice(&jacobians[wj_id..], ndofs));
        }
    }
}