use std::marker::PhantomData;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use na::{self, RealField};

use crate::counters::Counters;
use crate::geometry::{ContactManager, ParticlesContacts};
use crate::kernel::{CubicSplineKernel, Kernel};
use crate::math::{Vector, DIM};
use crate::object::{Boundary, Fluid};
use crate::solver::{helper, PressureSolver};

/// A Position Based Fluid solver.
pub struct DFSPHSolver<
    N: RealField,
    KernelDensity: Kernel = CubicSplineKernel,
    KernelGradient: Kernel = CubicSplineKernel,
> {
    min_pressure_iter: usize,
    max_pressure_iter: usize,
    max_density_error: N,
    min_divergence_iter: usize,
    max_divergence_iter: usize,
    max_divergence_error: N,
    min_neighbors_for_divergence_solve: usize,
    alphas: Vec<Vec<N>>,
    densities: Vec<Vec<N>>,
    predicted_densities: Vec<Vec<N>>,
    divergences: Vec<Vec<N>>,
    boundaries_volumes: Vec<Vec<N>>,
    velocity_changes: Vec<Vec<Vector<N>>>,
    phantoms: PhantomData<(KernelDensity, KernelGradient)>,
}

impl<N, KernelDensity, KernelGradient> DFSPHSolver<N, KernelDensity, KernelGradient>
where
    N: RealField,
    KernelDensity: Kernel,
    KernelGradient: Kernel,
{
    /// Initialize a new Position Based Fluid solver.
    pub fn new() -> Self {
        Self {
            min_pressure_iter: 1,
            max_pressure_iter: 50,
            max_density_error: na::convert(0.05),
            min_divergence_iter: 1,
            max_divergence_iter: 50,
            max_divergence_error: na::convert(0.1),
            min_neighbors_for_divergence_solve: if DIM == 2 { 6 } else { 20 },
            alphas: Vec::new(),
            densities: Vec::new(),
            predicted_densities: Vec::new(),
            divergences: Vec::new(),
            boundaries_volumes: Vec::new(),
            velocity_changes: Vec::new(),
            phantoms: PhantomData,
        }
    }

    fn compute_boundary_volumes(
        &mut self,
        boundary_boundary_contacts: &[ParticlesContacts<N>],
        boundaries: &[Boundary<N>],
    ) {
        for boundary_id in 0..boundaries.len() {
            par_iter_mut!(self.boundaries_volumes[boundary_id])
                .enumerate()
                .for_each(|(i, volume)| {
                    let mut denominator = N::zero();

                    for c in boundary_boundary_contacts[boundary_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        denominator += c.weight;
                    }

                    assert!(!denominator.is_zero());
                    *volume = N::one() / denominator;
                })
        }
    }

    fn compute_densities(
        &mut self,
        fluid_fluid_contacts: &[ParticlesContacts<N>],
        fluid_boundary_contacts: &[ParticlesContacts<N>],
        fluids: &[Fluid<N>],
    ) {
        let boundaries_volumes = &self.boundaries_volumes;

        for fluid_id in 0..fluids.len() {
            par_iter_mut!(self.densities[fluid_id])
                .enumerate()
                .for_each(|(i, density)| {
                    *density = N::zero();

                    for c in fluid_fluid_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        *density += fluids[c.j_model].particle_mass(c.j) * c.weight;
                    }

                    for c in fluid_boundary_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        *density += boundaries_volumes[c.j_model][c.j]
                            * fluids[c.i_model].density0
                            * c.weight;
                    }

                    assert!(!density.is_zero());
                })
        }
    }

    fn compute_predicted_densities(
        &mut self,
        dt: N,
        fluid_fluid_contacts: &[ParticlesContacts<N>],
        fluid_boundary_contacts: &[ParticlesContacts<N>],
        fluids: &[Fluid<N>],
    ) -> N {
        let boundaries_volumes = &self.boundaries_volumes;
        let velocity_changes = &self.velocity_changes;
        let densities = &self.densities;
        let mut max_error = N::zero();

        for fluid_id in 0..fluids.len() {
            let it = par_iter_mut!(self.predicted_densities[fluid_id])
                .enumerate()
                .map(|(i, predicted_density)| {
                    let fluid_i = &fluids[fluid_id];
                    let mut delta = N::zero();

                    for c in fluid_fluid_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let fluid_j = &fluids[c.j_model];
                        let vi = fluid_i.velocities[c.i] + velocity_changes[c.i_model][c.i];
                        let vj = fluid_j.velocities[c.j] + velocity_changes[c.j_model][c.j];

                        delta += fluids[c.j_model].particle_mass(c.j) * (vi - vj).dot(&c.gradient);
                    }

                    for c in fluid_boundary_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let vi = fluid_i.velocities[c.i] + velocity_changes[c.i_model][c.i];
                        // FIXME: take the velocity of j too?

                        delta += boundaries_volumes[c.j_model][c.j]
                            * fluid_i.density0
                            * vi.dot(&c.gradient);
                    }

                    *predicted_density = densities[fluid_id][i] + delta * dt;
                    *predicted_density = predicted_density.max(fluid_i.density0);
                    assert!(!predicted_density.is_zero());
                    *predicted_density / fluid_i.density0 - N::one()
                });
            let err = par_reduce_sum!(N::zero(), it);

            let nparts = fluids[fluid_id].num_particles();
            if nparts != 0 {
                max_error = max_error.max(err / na::convert(nparts as f64));
            }
        }

        max_error
    }

    // NOTE: this actually computes alpha_i / density_i
    fn compute_alphas(
        &mut self,
        _inv_dt: N,
        fluid_fluid_contacts: &[ParticlesContacts<N>],
        fluid_boundary_contacts: &[ParticlesContacts<N>],
        fluids: &[Fluid<N>],
    ) {
        let boundaries_volumes = &self.boundaries_volumes;

        for fluid_id in 0..fluids.len() {
            let fluid_fluid_contacts = &fluid_fluid_contacts[fluid_id];
            let fluid_boundary_contacts = &fluid_boundary_contacts[fluid_id];
            let alphas_i = &mut self.alphas[fluid_id];
            let fluid_i = &fluids[fluid_id];

            par_iter_mut!(alphas_i)
                .enumerate()
                .for_each(|(i, alpha_i)| {
                    let mut grad_sum = Vector::zeros();
                    let mut squared_grad_sum = N::zero();

                    for c in fluid_fluid_contacts
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let grad_i = c.gradient * fluids[c.j_model].particle_mass(c.j);
                        squared_grad_sum += grad_i.norm_squared();
                        grad_sum += grad_i;
                    }

                    for c in fluid_boundary_contacts
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let grad_i =
                            c.gradient * boundaries_volumes[c.j_model][c.j] * fluid_i.density0;
                        squared_grad_sum += grad_i.norm_squared();
                        grad_sum += grad_i;
                    }

                    let denominator = squared_grad_sum + grad_sum.norm_squared();
                    *alpha_i = N::one() / denominator.max(na::convert(1.0e-6));
                })
        }
    }

    fn compute_velocity_changes(
        &mut self,
        _dt: N,
        inv_dt: N,
        fluid_fluid_contacts: &[ParticlesContacts<N>],
        fluid_boundary_contacts: &[ParticlesContacts<N>],
        fluids: &[Fluid<N>],
        boundaries: &[Boundary<N>],
    ) {
        let alphas = &self.alphas;
        let boundaries_volumes = &self.boundaries_volumes;
        let predicted_densities = &self.predicted_densities;

        for (fluid_id, _fluid1) in fluids.iter().enumerate() {
            par_iter_mut!(self.velocity_changes[fluid_id])
                .enumerate()
                .for_each(|(i, velocity_change)| {
                    let fluid1 = &fluids[fluid_id];
                    let ki =
                        (predicted_densities[fluid_id][i] - fluid1.density0) * alphas[fluid_id][i];

                    for c in fluid_fluid_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let fluid2 = &fluids[c.j_model];

                        let kj = (predicted_densities[c.j_model][c.j] - fluid2.density0)
                            * alphas[c.j_model][c.j];

                        let kij = ki + kj;

                        // Compute velocity change.
                        if kij > N::default_epsilon() {
                            let coeff = kij * fluid2.particle_mass(c.j);
                            *velocity_change -= c.gradient * (coeff * inv_dt);
                        }
                    }

                    if ki > N::default_epsilon() {
                        for c in fluid_boundary_contacts[fluid_id]
                            .particle_contacts(i)
                            .read()
                            .unwrap()
                            .iter()
                        {
                            let coeff = ki * boundaries_volumes[c.j_model][c.j] * fluid1.density0;
                            let delta = c.gradient * (coeff * inv_dt);

                            *velocity_change -= delta;

                            // Apply the force to the boundary too.
                            let particle_mass = fluid1.particle_mass(c.i);
                            boundaries[c.j_model]
                                .apply_force(c.j, delta * (inv_dt * particle_mass));
                        }
                    }
                })
        }
    }

    fn compute_divergences(
        &mut self,
        _dt: N,
        fluid_fluid_contacts: &[ParticlesContacts<N>],
        fluid_boundary_contacts: &[ParticlesContacts<N>],
        fluids: &[Fluid<N>],
    ) -> N {
        let boundaries_volumes = &self.boundaries_volumes;
        let velocity_changes = &self.velocity_changes;
        let min_neighbors_for_divergence_solve = self.min_neighbors_for_divergence_solve;
        let mut max_error = N::zero();

        for fluid_id in 0..fluids.len() {
            let fluid_fluid_contacts = &fluid_fluid_contacts[fluid_id];
            let fluid_boundary_contacts = &fluid_boundary_contacts[fluid_id];
            let divergences_i = &mut self.divergences[fluid_id];
            let fluid_i = &fluids[fluid_id];

            let it = par_iter_mut!(divergences_i)
                .enumerate()
                .map(|(i, divergence_i)| {
                    *divergence_i = N::zero();

                    if fluid_fluid_contacts
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .len()
                        + fluid_boundary_contacts
                            .particle_contacts(i)
                            .read()
                            .unwrap()
                            .len()
                        < min_neighbors_for_divergence_solve
                    {
                        return N::zero();
                    }

                    for c in fluid_fluid_contacts
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let fluid_j = &fluids[c.j_model];
                        let v_i = fluid_i.velocities[c.i] + velocity_changes[c.i_model][c.i];
                        let v_j = fluid_j.velocities[c.j] + velocity_changes[c.j_model][c.j];
                        let dvel = v_i - v_j;
                        *divergence_i += dvel.dot(&c.gradient) * fluid_j.particle_mass(c.j);
                    }

                    for c in fluid_boundary_contacts
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let v_i = fluid_i.velocities[c.i] + velocity_changes[c.i_model][c.i];
                        // FIXME: take the velocity of j too?

                        let dvel = v_i;
                        *divergence_i += dvel.dot(&c.gradient)
                            * boundaries_volumes[c.j_model][c.j]
                            * fluid_i.density0;
                    }

                    *divergence_i = divergence_i.max(N::zero());
                    *divergence_i / fluid_i.density0
                });
            let err = par_reduce_sum!(N::zero(), it);

            let nparts = fluids[fluid_id].num_particles();
            if nparts != 0 {
                max_error = max_error.max(err / na::convert(nparts as f64));
            }
        }

        max_error
    }

    fn compute_velocity_changes_for_divergence(
        &mut self,
        _dt: N,
        inv_dt: N,
        fluid_fluid_contacts: &[ParticlesContacts<N>],
        fluid_boundary_contacts: &[ParticlesContacts<N>],
        fluids: &[Fluid<N>],
        boundaries: &[Boundary<N>],
    ) {
        let alphas = &self.alphas;
        let boundaries_volumes = &self.boundaries_volumes;
        let divergences = &self.divergences;

        for (fluid_id, _fluid1) in fluids.iter().enumerate() {
            par_iter_mut!(self.velocity_changes[fluid_id])
                .enumerate()
                .for_each(|(i, velocity_change)| {
                    let fluid1 = &fluids[fluid_id];
                    let ki = divergences[fluid_id][i] * alphas[fluid_id][i];

                    for c in fluid_fluid_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let fluid2 = &fluids[c.j_model];
                        let kj = divergences[c.j_model][c.j] * alphas[c.j_model][c.j];

                        // Compute velocity change.
                        let coeff = -(ki + kj) * fluid2.particle_mass(c.j);
                        *velocity_change += c.gradient * coeff;
                    }

                    for c in fluid_boundary_contacts[fluid_id]
                        .particle_contacts(i)
                        .read()
                        .unwrap()
                        .iter()
                    {
                        let boundary2 = &boundaries[c.j_model];

                        // Compute velocity change.
                        let coeff = -ki * boundaries_volumes[c.j_model][c.j] * fluid1.density0;
                        let delta = c.gradient * coeff;
                        *velocity_change += delta;

                        // Apply the force to the boundary too.
                        let particle_mass = fluid1.particle_mass(c.i);
                        boundary2.apply_force(c.j, delta * (-inv_dt * particle_mass));
                    }
                })
        }
    }

    fn update_positions(&mut self, dt: N, fluids: &mut [Fluid<N>]) {
        for (fluid, delta) in fluids.iter_mut().zip(self.velocity_changes.iter()) {
            par_iter_mut!(fluid.positions)
                .zip(par_iter!(fluid.velocities))
                .zip(par_iter!(delta))
                .for_each(|((pos, vel), delta)| {
                    *pos += (*vel + delta) * dt;
                })
        }
    }

    fn update_velocities(&mut self, _dt: N, fluids: &mut [Fluid<N>]) {
        for (fluid, delta) in fluids.iter_mut().zip(self.velocity_changes.iter()) {
            par_iter_mut!(fluid.velocities)
                .zip(par_iter!(delta))
                .for_each(|(vel, delta)| {
                    *vel += delta;
                })
        }
    }

    fn pressure_solve(
        &mut self,
        dt: N,
        inv_dt: N,
        _kernel_radius: N,
        contact_manager: &mut ContactManager<N>,
        fluids: &mut [Fluid<N>],
        boundaries: &[Boundary<N>],
    ) {
        for i in 0..self.max_pressure_iter {
            let avg_err = self.compute_predicted_densities(
                dt,
                &contact_manager.fluid_fluid_contacts,
                &contact_manager.fluid_boundary_contacts,
                fluids,
            );

            if avg_err <= self.max_density_error && i >= self.min_pressure_iter {
                //                println!(
                //                    "Average density error: {}, break after niters: {}",
                //                    avg_err, i
                //                );
                break;
            }

            self.compute_velocity_changes(
                dt,
                inv_dt,
                &contact_manager.fluid_fluid_contacts,
                &contact_manager.fluid_boundary_contacts,
                fluids,
                boundaries,
            );
        }
    }

    fn divergence_solve(
        &mut self,
        counters: &mut Counters,
        dt: N,
        inv_dt: N,
        _kernel_radius: N,
        contact_manager: &mut ContactManager<N>,
        fluids: &mut [Fluid<N>],
        boundaries: &[Boundary<N>],
    ) {
        for i in 0..self.max_divergence_iter {
            let avg_err = self.compute_divergences(
                dt,
                &contact_manager.fluid_fluid_contacts,
                &contact_manager.fluid_boundary_contacts,
                fluids,
            );

            let max_err = self.max_divergence_error * inv_dt * na::convert(0.01);
            if avg_err <= max_err && i >= self.min_divergence_iter {
                //                println!(
                //                    "Average divergence error: {} <= {}, break after niters: {}",
                //                    avg_err, max_err, i
                //                );
                break;
            }

            counters.custom.resume();

            self.compute_velocity_changes_for_divergence(
                dt,
                inv_dt,
                &contact_manager.fluid_fluid_contacts,
                &contact_manager.fluid_boundary_contacts,
                fluids,
                boundaries,
            );
            counters.custom.pause();
        }
    }
}

impl<N, KernelDensity, KernelGradient> PressureSolver<N>
    for DFSPHSolver<N, KernelDensity, KernelGradient>
where
    N: RealField,
    KernelDensity: Kernel,
    KernelGradient: Kernel,
{
    fn velocity_changes(&self) -> &[Vec<Vector<N>>] {
        &self.velocity_changes
    }

    fn velocity_changes_mut(&mut self) -> &mut [Vec<Vector<N>>] {
        &mut self.velocity_changes
    }

    fn init_with_fluids(&mut self, fluids: &[Fluid<N>]) {
        // Resize every buffer.
        self.alphas.resize(fluids.len(), Vec::new());
        self.densities.resize(fluids.len(), Vec::new());
        self.predicted_densities.resize(fluids.len(), Vec::new());
        self.divergences.resize(fluids.len(), Vec::new());
        self.velocity_changes.resize(fluids.len(), Vec::new());

        for (fluid, alphas, densities, predicted_densities, divergences, velocity_changes) in
            itertools::multizip((
                fluids.iter(),
                self.alphas.iter_mut(),
                self.densities.iter_mut(),
                self.predicted_densities.iter_mut(),
                self.divergences.iter_mut(),
                self.velocity_changes.iter_mut(),
            ))
        {
            alphas.resize(fluid.num_particles(), N::zero());
            densities.resize(fluid.num_particles(), N::zero());
            predicted_densities.resize(fluid.num_particles(), N::zero());
            divergences.resize(fluid.num_particles(), N::zero());
            velocity_changes.resize(fluid.num_particles(), Vector::zeros());
        }
    }

    fn init_with_boundaries(&mut self, boundaries: &[Boundary<N>]) {
        self.boundaries_volumes.resize(boundaries.len(), Vec::new());

        for (boundary, volumes) in boundaries.iter().zip(self.boundaries_volumes.iter_mut()) {
            volumes.resize(boundary.num_particles(), N::zero())
        }
    }

    fn predict_advection(&mut self, dt: N, gravity: &Vector<N>, fluids: &[Fluid<N>]) {
        for (_fluid, velocity_changes) in fluids.iter().zip(self.velocity_changes.iter_mut()) {
            par_iter_mut!(velocity_changes).for_each(|velocity_change| {
                *velocity_change += gravity * dt;
            })
        }
    }

    fn step(
        &mut self,
        counters: &mut Counters,
        dt: N,
        contact_manager: &mut ContactManager<N>,
        kernel_radius: N,
        fluids: &mut [Fluid<N>],
        boundaries: &[Boundary<N>],
    ) {
        let inv_dt = N::one() / dt;

        counters.solver.pressure_resolution_time.resume();
        // Init boundary-related data.
        helper::update_boundary_contacts::<_, KernelDensity, KernelGradient>(
            kernel_radius,
            &mut contact_manager.boundary_boundary_contacts,
            boundaries,
        );

        self.compute_boundary_volumes(&contact_manager.boundary_boundary_contacts, boundaries);

        helper::update_fluid_contacts::<_, KernelDensity, KernelGradient>(
            kernel_radius,
            &mut contact_manager.fluid_fluid_contacts,
            &mut contact_manager.fluid_boundary_contacts,
            fluids,
            boundaries,
        );

        self.compute_densities(
            &contact_manager.fluid_fluid_contacts,
            &contact_manager.fluid_boundary_contacts,
            fluids,
        );

        self.compute_alphas(
            inv_dt,
            &contact_manager.fluid_fluid_contacts,
            &contact_manager.fluid_boundary_contacts,
            fluids,
        );

        self.divergence_solve(
            counters,
            dt,
            inv_dt,
            kernel_radius,
            contact_manager,
            fluids,
            boundaries,
        );

        self.update_velocities(dt, fluids);
        self.velocity_changes
            .iter_mut()
            .for_each(|vs| vs.iter_mut().for_each(|v| v.fill(N::zero())));
        counters.solver.pressure_resolution_time.pause();

        counters.solver.non_pressure_resolution_time.resume();
        for (fluid, fluid_fluid_contacts, densities, velocity_changes) in itertools::multizip((
            &mut *fluids,
            &contact_manager.fluid_fluid_contacts,
            &self.densities,
            &mut self.velocity_changes,
        )) {
            let mut forces = std::mem::replace(&mut fluid.nonpressure_forces, Vec::new());

            for np_force in &mut forces {
                np_force.solve(
                    dt,
                    kernel_radius,
                    fluid_fluid_contacts,
                    fluid,
                    densities,
                    velocity_changes,
                );
            }

            fluid.nonpressure_forces = forces;
        }
        counters.solver.non_pressure_resolution_time.pause();

        counters.solver.pressure_resolution_time.resume();
        self.pressure_solve(
            dt,
            inv_dt,
            kernel_radius,
            contact_manager,
            fluids,
            boundaries,
        );

        self.update_positions(dt, fluids);
        counters.solver.pressure_resolution_time.pause();
    }
}
